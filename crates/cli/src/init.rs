use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use devspace_machine::{
    CatalogEntry, ControlPlaneClient, ControlPlaneClientError, ControlPlaneRemoteErrorKind,
    GitProcessEnvironment, MachineRepository, MachineStore, MachineStoreError, RemoteUrl,
    RepositoryName,
};
use jj_cli::cli_util::CommandHelper;
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;
use jj_lib::op_store::{RefTarget, RemoteRef, RemoteRefState};
use jj_lib::ref_name::{RefName, RemoteName, RemoteRefSymbol};
use jj_lib::repo::{Repo as _, StoreFactories};
use jj_lib::workspace::Workspace;

use crate::add::{AddArgs, add_checkout};
use crate::checkout::{
    absolute_path, canonical_destination_path, ensure_destination_parent,
    reject_unsupported_global_options, workspace_name,
};
use crate::git::display_error;
use crate::repo::{create_cloud_repository, materialize_cloud_repository};
use crate::tx::{commit_repo_transaction, materialize_checkout};
use crate::working_copy::devspace_working_copy_factories;

const ORIGIN: &str = "origin";
const AFTER_CLOUD_REGISTRATION_FAILPOINT: &str = "after_init_cloud_registration";

#[derive(clap::Args)]
pub(crate) struct InitArgs {
    /// Git remote URL to import.
    git_url: String,

    /// Directory to create the checkout at (defaults to ./<name>).
    #[arg(value_hint = clap::ValueHint::DirPath)]
    directory: Option<PathBuf>,

    /// Repository name (defaults to the Git URL basename).
    #[arg(long, value_name = "NAME")]
    name: Option<String>,
}

pub(crate) async fn init_repository(
    ui: &mut Ui,
    command: &CommandHelper,
    args: InitArgs,
) -> Result<(), CommandError> {
    reject_unsupported_global_options(command, "init")?;
    crate::boundary_sync::suppress();

    let name = RepositoryName::parse(
        args.name
            .unwrap_or_else(|| repository_name_from_url(&args.git_url)),
    )
    .map_err(|error| user_error(format!(
        "Cannot derive a valid repository name from the Git URL: {error}. Use `ds init --name <name>`."
    )))?;
    let requested = args
        .directory
        .unwrap_or_else(|| PathBuf::from(name.as_str()));
    let checkout_path = preflight_destination(command.cwd(), &requested)?;

    let store = MachineStore::platform_default().map_err(display_error)?;
    if store.resolve(&name).map_err(display_error)?.is_some() {
        return Err(user_error(format!(
            "Repository `{name}` is already registered in this machine's local catalog."
        )));
    }
    ensure_cloud_name_available(&store, &name)?;

    let remote_url = RemoteUrl::new(args.git_url.clone());
    let head = devspace_machine::ls_remote_head(&remote_url, &GitProcessEnvironment::default())
        .map_err(display_error)?
        .ok_or_else(|| {
            user_error(
                "The Git remote has no history yet; use `ds repo new` for a repository with no history yet.",
            )
        })?;

    let pending = create_cloud_repository(name.clone()).map_err(user_error)?;
    failpoint(AFTER_CLOUD_REGISTRATION_FAILPOINT);
    let incomplete = || {
        format!(
            "Repository `{name}` was created in the cloud. Clean it up with `ds repo`, then re-run `ds init`."
        )
    };
    let sync_guard = pending
        .store
        .try_lock_repository_sync(&pending.identity)
        .map_err(|error| match error {
            MachineStoreError::RepositorySyncAlreadyLocked { .. } => post_registration_error(
                user_error(format!(
                    "Repository `{name}` is already being initialized or synchronized; retry after it finishes."
                )),
                &incomplete(),
            ),
            error => post_registration_error(display_error(error), &incomplete()),
        })?;

    let provisional_entry = CatalogEntry {
        name: name.clone(),
        identity: pending.identity.clone(),
        native_repository_path: pending.store.native_repository_path(&pending.identity),
    };
    crate::git::register_remote(&pending.config, &provisional_entry, ORIGIN, &args.git_url)
        .map_err(|error| post_registration_error(error, &incomplete()))?;
    let entry = materialize_cloud_repository(ui, command, &pending)
        .await
        .map_err(|error| post_registration_error(error, &incomplete()))?;

    add_checkout(ui, command, AddArgs::for_init(&name, checkout_path.clone()))
        .await
        .map_err(|error| post_registration_error(error, &incomplete()))?;

    sync_repository(&pending, &entry, command.settings())
        .await
        .map_err(|error| post_registration_error(user_error(error), &incomplete()))?;
    let fetched = crate::git::fetch::fetch_entry(
        ui,
        command.settings(),
        &pending.store,
        &entry,
        Vec::new(),
        ORIGIN.to_owned(),
    )
    .await
    .map_err(|error| post_registration_error(error, &incomplete()))?;
    position_checkout(
        command,
        &pending.store,
        &entry,
        &checkout_path,
        &head.branch,
    )
    .await
    .map_err(|error| post_registration_error(user_error(error), &incomplete()))?;
    sync_repository(&pending, &entry, command.settings())
        .await
        .map_err(|error| post_registration_error(user_error(error), &incomplete()))?;
    drop(sync_guard);
    crate::git_shim::ensure(&checkout_path);

    writeln!(ui.status(), "Repository: {name}")?;
    writeln!(ui.status(), "Remote: {ORIGIN} -> {}", args.git_url)?;
    writeln!(
        ui.status(),
        "Fetched bookmarks: {}",
        fetched_bookmarks(&fetched)
    )?;
    writeln!(ui.status(), "Checkout: {}", checkout_path.display())?;
    Ok(())
}

async fn sync_repository(
    pending: &crate::repo::PendingRepositoryCreation,
    entry: &CatalogEntry,
    settings: &jj_lib::settings::UserSettings,
) -> Result<(), String> {
    let mut repository = MachineRepository::open(&entry.native_repository_path, settings)
        .await
        .map_err(|error| error.to_string())?;
    crate::sync::run_sync_engine(
        &pending.config,
        &entry.identity,
        &mut repository,
        &pending.store.repository_sync_path(&entry.identity),
        &pending.store.repository_packs_path(&entry.identity),
    )
}

fn repository_name_from_url(url: &str) -> String {
    url.trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or_default()
        .strip_suffix(".git")
        .unwrap_or_else(|| {
            url.trim_end_matches('/')
                .rsplit(['/', ':'])
                .next()
                .unwrap_or_default()
        })
        .to_owned()
}

fn preflight_destination(cwd: &Path, requested: &Path) -> Result<PathBuf, CommandError> {
    let requested = absolute_path(cwd, requested);
    match fs::symlink_metadata(&requested) {
        Ok(metadata) if metadata.is_dir() => {
            let mut entries = fs::read_dir(&requested).map_err(|error| {
                user_error(format!(
                    "Failed to inspect init directory {}: {error}",
                    requested.display()
                ))
            })?;
            if entries.next().transpose().map_err(display_error)?.is_some() {
                return Err(user_error(format!(
                    "Init directory {} already exists and is not empty; no cloud repository was created.",
                    requested.display()
                )));
            }
            fs::remove_dir(&requested).map_err(|error| {
                user_error(format!(
                    "Failed to prepare empty init directory {}: {error}",
                    requested.display()
                ))
            })?;
        }
        Ok(_) => {
            return Err(user_error(format!(
                "Init directory {} already exists and is not an empty directory; no cloud repository was created.",
                requested.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(display_error(error)),
    }
    ensure_destination_parent(&requested)?;
    canonical_destination_path(&requested)
}

fn ensure_cloud_name_available(
    store: &MachineStore,
    name: &RepositoryName,
) -> Result<(), CommandError> {
    if store
        .repository_creation_intent(name)
        .map_err(display_error)?
        .is_some()
    {
        return Ok(());
    }
    let config = store.load_config().map_err(display_error)?;
    let client = ControlPlaneClient::new(&config).map_err(display_error)?;
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|_| user_error("failed to start the cloud transport runtime"))?;
    match runtime.block_on(client.resolve_repository(name)) {
        Ok(_) => Err(user_error(format!(
            "Repository `{name}` is already registered in the cloud directory."
        ))),
        Err(ControlPlaneClientError::Remote {
            kind: ControlPlaneRemoteErrorKind::RepositoryNotFound,
            ..
        }) => Ok(()),
        Err(error) => Err(display_error(error)),
    }
}

async fn position_checkout(
    command: &CommandHelper,
    store: &MachineStore,
    entry: &CatalogEntry,
    checkout_path: &Path,
    head_branch: &str,
) -> Result<(), String> {
    let config = store.load_config().map_err(|error| error.to_string())?;
    let workspace_name = workspace_name(&config, checkout_path);
    let repository = MachineRepository::open(&entry.native_repository_path, command.settings())
        .await
        .map_err(|error| error.to_string())?;
    let name = RefName::new(head_branch);
    let symbol = RemoteRefSymbol {
        name,
        remote: RemoteName::new(ORIGIN),
    };
    let remote = repository.repo().view().get_remote_bookmark(symbol).clone();
    let target = remote.target.as_normal().cloned().ok_or_else(|| {
        format!("fetched HEAD bookmark `{head_branch}@{ORIGIN}` is absent or conflicted")
    })?;
    let head = repository
        .repo()
        .store()
        .get_commit_async(&target)
        .await
        .map_err(|error| error.to_string())?;
    let mut transaction = repository.repo().start_transaction();
    transaction
        .repo_mut()
        .set_local_bookmark_target(name, RefTarget::normal(target.clone()));
    transaction.repo_mut().set_remote_bookmark(
        symbol,
        RemoteRef {
            target: RefTarget::normal(target),
            state: RemoteRefState::Tracked,
        },
    );
    let working_copy_commit = transaction
        .repo_mut()
        .check_out(workspace_name, &head)
        .await
        .map_err(|error| error.to_string())?;
    let repo = commit_repo_transaction(
        transaction,
        format!("track {head_branch}@{ORIGIN} after init"),
    )
    .await
    .map_err(|error| error.to_string())?;

    let mut workspace = Workspace::load(
        command.settings(),
        checkout_path,
        &StoreFactories::default(),
        &devspace_working_copy_factories(),
    )
    .map_err(|error| error.to_string())?;
    materialize_checkout(&mut workspace, repo.op_id().clone(), &working_copy_commit)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn fetched_bookmarks(lines: &[String]) -> String {
    let bookmarks = lines
        .iter()
        .filter_map(|line| {
            line.strip_prefix("new bookmark ")
                .or_else(|| line.strip_prefix("fetched "))
                .or_else(|| line.strip_prefix("up to date "))
                .and_then(|rest| rest.split_whitespace().next())
        })
        .collect::<Vec<_>>();
    if bookmarks.is_empty() {
        "none".to_owned()
    } else {
        bookmarks.join(", ")
    }
}

fn post_registration_error(error: CommandError, context: &str) -> CommandError {
    user_error(format!("{}\n{context}", error.error))
}

fn failpoint(name: &str) {
    if std::env::var_os("DEVSPACE_FAILPOINT").as_deref() == Some(std::ffi::OsStr::new(name)) {
        std::process::exit(86);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_repository_names_from_common_remote_shapes() {
        assert_eq!(
            repository_name_from_url("https://example.invalid/team/project.git"),
            "project"
        );
        assert_eq!(
            repository_name_from_url("git@example.invalid:team/project.git"),
            "project"
        );
        assert_eq!(repository_name_from_url("/tmp/project.git/"), "project");
    }

    #[test]
    fn rejects_nonempty_destination_without_changing_it() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("checkout");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("keep"), b"untouched").unwrap();

        let error = preflight_destination(temp.path(), Path::new("checkout")).unwrap_err();

        assert!(error.error.to_string().contains("is not empty"));
        assert_eq!(fs::read(destination.join("keep")).unwrap(), b"untouched");
    }
}
