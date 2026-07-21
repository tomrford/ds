use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use devspace_machine::{
    CatalogEntry, GitProcessEnvironment, MachineRepository, MachineStore, RemoteUrl, RepositoryName,
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
use crate::repo::{
    CloudRepositoryCreationError, PendingRepositoryCreation, create_cloud_repository,
    materialize_cloud_repository,
};
use crate::sync::wait_for_repository_sync_lock;
use crate::tx::{commit_repo_transaction, materialize_checkout};
use crate::working_copy::devspace_working_copy_factories;

const ORIGIN: &str = "origin";
const AFTER_CLOUD_REGISTRATION_FAILPOINT: &str = "after_init_cloud_registration";

#[derive(clap::Args)]
pub(crate) struct InitArgs {
    /// Git remote URL to import, or a directory for a new blank repository.
    source: Option<String>,

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
    let mode = classify_init(command.cwd(), args)?;
    match mode {
        InitMode::Blank { requested } => init_blank(ui, command, requested).await,
        InitMode::Import {
            git_url,
            directory,
            name,
        } => {
            let derived = crate::repo::parse_repository_name(
                name.clone()
                    .unwrap_or_else(|| repository_name_from_url(&git_url)),
            )?;
            let requested = directory.unwrap_or_else(|| PathBuf::from(derived.as_str()));
            let checkout_path = preflight_destination(command.cwd(), &requested)?;
            let imported = import_git_repository(ui, command, git_url, name).await?;
            let incomplete = format!(
                "Repository `{}` was imported, but its first checkout at {} is incomplete.",
                imported.name,
                checkout_path.display()
            );
            add_checkout(
                ui,
                command,
                AddArgs::for_init(&imported.name, checkout_path.clone()),
            )
            .await
            .map_err(|error| post_registration_error(error, &incomplete))?;
            position_checkout(
                command,
                &imported.store,
                &imported.entry,
                &checkout_path,
                &imported.head_branch,
            )
            .await
            .map_err(|error| post_registration_error(user_error(error), &incomplete))?;
            sync_repository(
                &imported.config,
                &imported.store,
                &imported.entry,
                command.settings(),
            )
            .await
            .map_err(|error| post_registration_error(user_error(error), &incomplete))?;
            imported.write_summary(ui)?;
            writeln!(ui.status(), "Checkout: {}", checkout_path.display())?;
            Ok(())
        }
    }
}

pub(crate) struct ImportedRepository {
    name: RepositoryName,
    git_url: String,
    head_branch: String,
    fetched: Vec<String>,
    store: MachineStore,
    config: devspace_machine::MachineConfig,
    entry: CatalogEntry,
}

impl ImportedRepository {
    pub(crate) fn write_summary(&self, ui: &mut Ui) -> Result<(), CommandError> {
        writeln!(ui.status(), "Repository: {}", self.name)?;
        writeln!(ui.status(), "Remote: {ORIGIN} -> {}", self.git_url)?;
        writeln!(
            ui.status(),
            "Fetched bookmarks: {}",
            fetched_bookmarks(&self.fetched)
        )?;
        Ok(())
    }
}

pub(crate) async fn import_git_repository(
    ui: &mut Ui,
    command: &CommandHelper,
    git_url: String,
    requested_name: Option<String>,
) -> Result<ImportedRepository, CommandError> {
    reject_unsupported_global_options(command, "repo add")?;
    crate::boundary_sync::suppress();
    let name = crate::repo::parse_repository_name(
        requested_name.unwrap_or_else(|| repository_name_from_url(&git_url)),
    )?;
    let remote_url = RemoteUrl::new(git_url.clone());
    // Decide resume-vs-collision from the local catalog BEFORE any remote
    // observation: a name collision must fail fast (and a resume proceed)
    // without depending on the requested remote being reachable or sane.
    let locally_registered = MachineStore::platform_default()
        .map_err(display_error)?
        .resolve(&name)
        .map_err(display_error)?
        .is_some_and(|entry| entry.native_repository_path.is_dir());
    let observe_head = || {
        devspace_machine::ls_remote_head(&remote_url, &GitProcessEnvironment::default())
            .map_err(display_error)?
            .ok_or_else(|| {
                user_error(
                    "The Git remote has no history yet; use `ds repo new` for a repository with no history yet.",
                )
            })
    };
    let (repository, head) = if locally_registered {
        let repository =
            resume_registered_import(&name, &git_url, crate::repo::name_taken_error(&name))?;
        (repository, observe_head()?)
    } else {
        // The empty-remote preflight must precede cloud creation so a bad
        // URL never mints a repository.
        let head = observe_head()?;
        let repository = match create_cloud_repository(name.clone()) {
            Ok(pending) => ImportRepository::Pending(pending),
            Err(error @ CloudRepositoryCreationError::NameTaken(_)) => {
                resume_registered_import(&name, &git_url, error)?
            }
            Err(error) => return Err(user_error(error.to_string())),
        };
        (repository, head)
    };
    failpoint(AFTER_CLOUD_REGISTRATION_FAILPOINT);
    let incomplete = format!(
        "Repository `{name}` was created in the cloud, but its local Git import is incomplete."
    );
    let (store, config, identity) = match &repository {
        ImportRepository::Pending(pending) => (&pending.store, &pending.config, &pending.identity),
        ImportRepository::Registered {
            store,
            config,
            entry,
        } => (store, config, &entry.identity),
    };
    let provisional_entry = match &repository {
        ImportRepository::Pending(_) => CatalogEntry {
            name: name.clone(),
            identity: identity.clone(),
            native_repository_path: store.native_repository_path(identity),
        },
        ImportRepository::Registered { entry, .. } => entry.clone(),
    };
    let Some(sync_guard) = wait_for_repository_sync_lock(ui, store, &provisional_entry)
        .map_err(|error| post_registration_error(user_error(error), &incomplete))?
    else {
        return Err(post_registration_error(
            user_error(format!(
                "Repository `{name}` is already being imported or synchronized; retry after it finishes."
            )),
            &incomplete,
        ));
    };
    crate::git::register_remote(config, &provisional_entry, ORIGIN, &git_url)
        .map_err(|error| post_registration_error(error, &incomplete))?;
    let entry = match &repository {
        ImportRepository::Pending(pending) => materialize_cloud_repository(ui, command, pending)
            .await
            .map_err(|error| post_registration_error(error, &incomplete))?,
        ImportRepository::Registered { entry, .. } => entry.clone(),
    };
    let fetched = crate::git::fetch::fetch_entry(
        ui,
        command.settings(),
        store,
        &entry,
        Vec::new(),
        ORIGIN.to_owned(),
    )
    .await
    .map_err(|error| post_registration_error(error, &incomplete))?;
    sync_repository(config, store, &entry, command.settings())
        .await
        .map_err(|error| post_registration_error(user_error(error), &incomplete))?;
    drop(sync_guard);
    let (store, config) = match repository {
        ImportRepository::Pending(pending) => (pending.store, pending.config),
        ImportRepository::Registered { store, config, .. } => (store, config),
    };
    Ok(ImportedRepository {
        name,
        git_url,
        head_branch: head.branch,
        fetched,
        store,
        config,
        entry,
    })
}

enum ImportRepository {
    Pending(PendingRepositoryCreation),
    Registered {
        store: MachineStore,
        config: devspace_machine::MachineConfig,
        entry: CatalogEntry,
    },
}

fn resume_registered_import(
    name: &RepositoryName,
    git_url: &str,
    creation_error: CloudRepositoryCreationError,
) -> Result<ImportRepository, CommandError> {
    let store = MachineStore::platform_default().map_err(display_error)?;
    let Some(entry) = store.resolve(name).map_err(display_error)? else {
        return Err(user_error(creation_error.to_string()));
    };
    if !entry.native_repository_path.is_dir() {
        return Err(user_error(creation_error.to_string()));
    }
    let config = store.load_config().map_err(display_error)?;
    let remotes = crate::git::list_registered_remotes(&config, &entry)?;
    match remotes.iter().find(|remote| remote.name == ORIGIN) {
        Some(origin) if origin.url == git_url => Ok(ImportRepository::Registered {
            store,
            config,
            entry,
        }),
        Some(origin) => Err(user_error(format!(
            "{creation_error}; existing origin is {}",
            origin.url
        ))),
        None => Err(user_error(creation_error.to_string())),
    }
}

async fn sync_repository(
    config: &devspace_machine::MachineConfig,
    store: &MachineStore,
    entry: &CatalogEntry,
    settings: &jj_lib::settings::UserSettings,
) -> Result<(), String> {
    let mut repository = MachineRepository::open(&entry.native_repository_path, settings)
        .await
        .map_err(|error| error.to_string())?;
    crate::sync::run_sync_engine(
        config,
        &entry.identity,
        &mut repository,
        &store.repository_sync_path(&entry.identity),
        &store.repository_packs_path(&entry.identity),
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

enum InitMode {
    Blank {
        requested: PathBuf,
    },
    Import {
        git_url: String,
        directory: Option<PathBuf>,
        name: Option<String>,
    },
}

fn classify_init(cwd: &Path, args: InitArgs) -> Result<InitMode, CommandError> {
    match (args.source, args.directory) {
        (None, None) => {
            if args.name.is_some() {
                return Err(user_error(
                    "`ds init --name` requires a Git URL; blank repositories are named after their directory",
                ));
            }
            Ok(InitMode::Blank {
                requested: cwd.to_owned(),
            })
        }
        (Some(source), Some(directory)) => Ok(InitMode::Import {
            git_url: source,
            directory: Some(directory),
            name: args.name,
        }),
        (Some(source), None) if looks_like_git_remote(cwd, &source) => Ok(InitMode::Import {
            git_url: source,
            directory: None,
            name: args.name,
        }),
        (Some(directory), None) => {
            if args.name.is_some() {
                return Err(user_error(
                    "`ds init --name` requires a Git URL; blank repositories are named after their directory",
                ));
            }
            Ok(InitMode::Blank {
                requested: PathBuf::from(directory),
            })
        }
        (None, Some(_)) => unreachable!("a second positional cannot exist without the first"),
    }
}

fn looks_like_git_remote(cwd: &Path, value: &str) -> bool {
    let path = absolute_path(cwd, Path::new(value));
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() => fs::read_dir(&path)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(true),
        Ok(_) => true,
        Err(_) => {
            value.contains("://")
                || value.ends_with(".git")
                || value
                    .split_once(':')
                    .is_some_and(|(host, path)| host.contains('@') && !path.is_empty())
        }
    }
}

async fn init_blank(
    ui: &mut Ui,
    command: &CommandHelper,
    requested: PathBuf,
) -> Result<(), CommandError> {
    let requested = absolute_path(command.cwd(), &requested);
    let directory_name = requested
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| user_error("init directory has no UTF-8 directory name"))?;
    let name = crate::repo::parse_repository_name(directory_name.to_owned())?;
    let checkout_path = preflight_destination(command.cwd(), &requested)?;
    let pending =
        create_cloud_repository(name.clone()).map_err(|error| user_error(error.to_string()))?;
    let entry = materialize_cloud_repository(ui, command, &pending).await?;
    add_checkout(ui, command, AddArgs::for_init(&name, checkout_path.clone())).await?;
    crate::boundary_sync::record(&entry);
    writeln!(ui.status(), "Repository: {name}")?;
    writeln!(ui.status(), "Checkout: {}", checkout_path.display())?;
    Ok(())
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
