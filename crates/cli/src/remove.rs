use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use devspace_machine::{CatalogEntry, MachineRepository, MachineStore};
use jj_cli::cli_util::CommandHelper;
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;
use jj_lib::op_store::OpStoreError;
use jj_lib::ref_name::{WorkspaceName, WorkspaceNameBuf};
use jj_lib::repo::Repo as _;
use jj_lib::working_copy::{WorkingCopyFreshness, create_and_check_out_recovery_commit};
use jj_lib::workspace::Workspace;
use jj_lib::workspace_store::{SimpleWorkspaceStore, WorkspaceStore as _};

use crate::bare_workspace::is_stock_bare_repository;
use crate::checkout::{
    CheckoutOwner, absolute_path, canonical_destination_path, destination_hash,
    owned_directory_matches, read_checkout_owner, reject_unsupported_global_options,
    workspace_name,
};

#[derive(clap::Args)]
pub(crate) struct RemoveArgs {
    /// Checkout directory to remove.
    #[arg(value_hint = clap::ValueHint::DirPath)]
    path: PathBuf,

    /// Print the removed checkout identity as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(serde::Serialize)]
struct RemovedCheckout<'a> {
    root: &'a Path,
    repo: &'a str,
    workspace_id: &'a str,
}

struct RemovalTarget {
    entry: CatalogEntry,
    owner: CheckoutOwner,
    path_exists: bool,
}

pub(crate) async fn remove_checkout(
    ui: &mut Ui,
    command: &CommandHelper,
    args: RemoveArgs,
) -> Result<(), CommandError> {
    reject_unsupported_global_options(command, "remove")?;
    let requested_path = absolute_path(command.cwd(), &args.path);
    if args.json && requested_path.to_str().is_none() {
        return Err(user_error(
            "`ds remove --json` requires a checkout path representable as UTF-8",
        ));
    }
    let path = canonical_destination_path(&requested_path)?;
    let store = MachineStore::platform_default().map_err(|error| user_error(error.to_string()))?;
    let machine = store
        .load_config()
        .map_err(|error| user_error(error.to_string()))?;
    let expected_workspace = workspace_name(machine.machine_id().as_str(), &path);
    let target = match fs::symlink_metadata(&path) {
        Ok(_) => target_from_marker(&store, &path, &expected_workspace)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            target_from_workspace_store(&store, &path, &expected_workspace, command.settings())
                .await?
        }
        Err(_) => return Err(not_checkout(&path)),
    };
    let _destination_guard = store
        .try_lock_checkout_destination(&destination_hash(&path))
        .map_err(|error| user_error(error.to_string()))?;
    if target.path_exists {
        if !owned_directory_matches(&path, &target.owner).map_err(|_| not_checkout(&path))? {
            return Err(not_checkout(&path));
        }
    } else if path.exists() {
        return Err(user_error(format!(
            "Checkout directory {} reappeared while removal was starting; nothing was touched",
            path.display()
        )));
    }
    require_native_repository(&target.entry)?;

    let workspace_name = WorkspaceNameBuf::from(target.owner.workspace_name().to_owned());
    let settings = command.settings().clone();
    let repository = MachineRepository::open(&target.entry.native_repository_path, &settings)
        .await
        .map_err(|error| user_error(error.to_string()))?;
    let registered = repository
        .repo()
        .view()
        .get_wc_commit_id(&workspace_name)
        .is_some();

    if registered && target.path_exists {
        snapshot_checkout(ui, command, &target.entry, &path, &workspace_name).await?;
    }
    if registered {
        forget_workspace(&target.entry, &workspace_name, &settings).await?;
    } else {
        forget_workspace_record(&target.entry.native_repository_path, &workspace_name)?;
    }

    if target.path_exists {
        if !owned_directory_matches(&path, &target.owner).map_err(|_| not_checkout(&path))? {
            return Err(not_checkout(&path));
        }
        fs::remove_dir_all(&path).map_err(|error| {
            user_error(format!(
                "Failed to delete checkout directory {}: {error}",
                path.display()
            ))
        })?;
    }

    let removed = RemovedCheckout {
        root: &path,
        repo: target.entry.name.as_str(),
        workspace_id: workspace_name.as_str(),
    };
    if args.json {
        serde_json::to_writer_pretty(ui.stdout(), &removed)
            .map_err(|error| user_error(format!("failed to encode checkout identity: {error}")))?;
        writeln!(ui.stdout())?;
    } else if target.path_exists {
        writeln!(
            ui.status(),
            "Removed workspace {} for `{}` at {}.",
            workspace_name.as_symbol(),
            target.entry.name,
            path.display()
        )?;
    } else {
        writeln!(
            ui.status(),
            "Forgot workspace {} for `{}`; checkout directory {} was already gone.",
            workspace_name.as_symbol(),
            target.entry.name,
            path.display()
        )?;
    }
    Ok(())
}

fn target_from_marker(
    store: &MachineStore,
    path: &Path,
    expected_workspace: &WorkspaceName,
) -> Result<RemovalTarget, CommandError> {
    let owner = read_checkout_owner(path).map_err(|_| not_checkout(path))?;
    if owner.workspace_name() != expected_workspace.as_str() {
        return Err(not_checkout(path));
    }
    let entry = store
        .entries()
        .map_err(|error| user_error(error.to_string()))?
        .into_iter()
        .find(|entry| {
            entry.identity.repository_id.as_str() == owner.repository_id()
                && entry.identity.incarnation.as_str() == owner.incarnation()
        })
        .ok_or_else(|| {
            user_error(format!(
                "Checkout {} belongs to a repository that is not in this machine store; nothing was touched",
                path.display()
            ))
        })?;
    Ok(RemovalTarget {
        entry,
        owner,
        path_exists: true,
    })
}

async fn target_from_workspace_store(
    store: &MachineStore,
    path: &Path,
    workspace_name: &WorkspaceName,
    settings: &jj_lib::settings::UserSettings,
) -> Result<RemovalTarget, CommandError> {
    let mut matches = Vec::new();
    for entry in store
        .entries()
        .map_err(|error| user_error(error.to_string()))?
    {
        if !entry
            .native_repository_path
            .join("workspace_store/index")
            .is_file()
        {
            continue;
        }
        let Ok(workspace_store) = SimpleWorkspaceStore::load(&entry.native_repository_path) else {
            continue;
        };
        let Ok(Some(stored_path)) = workspace_store.get_workspace_path(workspace_name) else {
            continue;
        };
        if !workspace_paths_match(&entry.native_repository_path, &stored_path, path) {
            continue;
        }
        let repository = MachineRepository::open(&entry.native_repository_path, settings)
            .await
            .map_err(|error| user_error(error.to_string()))?;
        if repository
            .repo()
            .view()
            .get_wc_commit_id(workspace_name)
            .is_some()
        {
            matches.push(entry);
        }
    }
    let [entry] = matches.as_slice() else {
        return Err(not_checkout(path));
    };
    Ok(RemovalTarget {
        owner: CheckoutOwner::new(
            entry.identity.repository_id.as_str(),
            entry.identity.incarnation.as_str(),
            workspace_name.as_str(),
        ),
        entry: entry.clone(),
        path_exists: false,
    })
}

fn workspace_paths_match(
    repository_path: &Path,
    stored_path: &Path,
    requested_path: &Path,
) -> bool {
    let stored_path = if stored_path.is_absolute() {
        stored_path.to_owned()
    } else {
        repository_path.join(stored_path)
    };
    canonical_destination_path(&stored_path).is_ok_and(|stored| stored == requested_path)
}

fn require_native_repository(entry: &CatalogEntry) -> Result<(), CommandError> {
    if !entry.native_repository_path.exists() {
        return Err(user_error(format!(
            "Repository `{}` has an incomplete clone; run `ds add` again to finish it; nothing was touched",
            entry.name
        )));
    }
    if is_stock_bare_repository(&entry.native_repository_path) {
        Ok(())
    } else {
        Err(user_error(format!(
            "Repository `{}` is registered locally, but its native repository is invalid; nothing was touched",
            entry.name
        )))
    }
}

async fn snapshot_checkout(
    ui: &Ui,
    command: &CommandHelper,
    entry: &CatalogEntry,
    path: &Path,
    expected_workspace: &WorkspaceName,
) -> Result<(), CommandError> {
    let settings = command.settings().clone();
    let workspace = command.load_workspace_at(path, &settings)?;
    validate_workspace(&workspace, entry, expected_workspace)?;
    let working_copy_operation = workspace.working_copy().operation_id().clone();

    match workspace
        .repo_loader()
        .load_operation(&working_copy_operation)
        .await
    {
        Ok(operation) => {
            let repo = workspace.repo_loader().load_at(&operation).await?;
            let mut stale_helper = command.for_workable_repo(ui, workspace, repo)?;
            stale_helper.maybe_snapshot(ui).await?;
            let stale_commit_id = stale_helper
                .get_wc_commit_id()
                .cloned()
                .ok_or_else(|| user_error("Checkout has no working-copy commit"))?;
            let stale_commit = stale_helper
                .repo()
                .store()
                .get_commit_async(&stale_commit_id)
                .await?;
            drop(stale_helper);

            let workspace = command.load_workspace_at(path, &settings)?;
            validate_workspace(&workspace, entry, expected_workspace)?;
            let current_repo = workspace.repo_loader().load_at_head().await?;
            let mut helper = command.for_workable_repo(ui, workspace, current_repo)?;
            let current_repo = helper.repo().clone();
            let current_operation = current_repo.op_id().clone();
            let (mut locked_workspace, desired_commit) =
                helper.unchecked_start_working_copy_mutation().await?;
            match WorkingCopyFreshness::check_stale(
                locked_workspace.locked_wc(),
                &desired_commit,
                &current_repo,
            )
            .await?
            {
                WorkingCopyFreshness::Fresh => drop(locked_workspace),
                WorkingCopyFreshness::Updated(_) => {
                    return Err(user_error("Concurrent working copy operation. Try again."));
                }
                WorkingCopyFreshness::WorkingCopyStale | WorkingCopyFreshness::SiblingOperation => {
                    if stale_commit.tree().tree_ids_and_labels()
                        != locked_workspace
                            .locked_wc()
                            .old_tree()
                            .tree_ids_and_labels()
                    {
                        return Err(user_error("Concurrent working copy operation. Try again."));
                    }
                    locked_workspace
                        .locked_wc()
                        .check_out(&desired_commit)
                        .await
                        .map_err(|error| {
                            user_error(format!("Failed to update stale working copy: {error}"))
                        })?;
                    locked_workspace
                        .finish(current_operation)
                        .await
                        .map_err(|error| {
                            user_error(format!("Failed to save stale working copy state: {error}"))
                        })?;
                }
            }
            helper.maybe_snapshot(ui).await?;
        }
        Err(OpStoreError::ObjectNotFound { .. }) => {
            let mut workspace = workspace;
            let current_repo = workspace.repo_loader().load_at_head().await?;
            let mut locked_workspace = workspace.start_working_copy_mutation().await?;
            let (repo, _) = create_and_check_out_recovery_commit(
                locked_workspace.locked_wc(),
                &current_repo,
                expected_workspace.to_owned(),
                "RECOVERY COMMIT FROM `ds remove`\n\nThis commit preserves files from a checkout whose working-copy operation was unavailable during removal.\n",
            )
            .await?;
            locked_workspace.finish(repo.op_id().clone()).await?;
            let mut helper = command.for_workable_repo(ui, workspace, repo)?;
            helper.maybe_snapshot(ui).await?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn validate_workspace(
    workspace: &Workspace,
    entry: &CatalogEntry,
    expected_workspace: &WorkspaceName,
) -> Result<(), CommandError> {
    if workspace.workspace_name() != expected_workspace {
        return Err(user_error(
            "Checkout ownership marker does not match its working-copy state; nothing was touched",
        ));
    }
    let actual_repository = dunce::canonicalize(workspace.repo_path())
        .map_err(|error| user_error(format!("Failed to resolve checkout repository: {error}")))?;
    let expected_repository = dunce::canonicalize(&entry.native_repository_path)
        .map_err(|error| user_error(format!("Failed to resolve machine repository: {error}")))?;
    if actual_repository != expected_repository {
        return Err(user_error(
            "Checkout ownership marker does not match its repository pointer; nothing was touched",
        ));
    }
    Ok(())
}

async fn forget_workspace(
    entry: &CatalogEntry,
    workspace_name: &WorkspaceName,
    settings: &jj_lib::settings::UserSettings,
) -> Result<(), CommandError> {
    let repository = MachineRepository::open(&entry.native_repository_path, settings)
        .await
        .map_err(|error| user_error(error.to_string()))?;
    if let Some(working_copy_commit_id) = repository
        .repo()
        .view()
        .get_wc_commit_id(workspace_name)
        .cloned()
    {
        let working_copy_commit = repository
            .repo()
            .store()
            .get_commit_async(&working_copy_commit_id)
            .await?;
        let mut transaction = repository.repo().start_transaction();
        let view = transaction.repo().view();
        let referenced_elsewhere = view
            .wc_commit_ids()
            .iter()
            .any(|(name, id)| name != workspace_name && id == working_copy_commit.id())
            || view
                .local_bookmarks()
                .any(|(_, target)| target.added_ids().any(|id| id == working_copy_commit.id()))
            || view
                .local_tags()
                .any(|(_, target)| target.added_ids().any(|id| id == working_copy_commit.id()));
        let should_abandon = working_copy_commit
            .is_discardable(transaction.repo())
            .await?
            && !referenced_elsewhere
            && view.heads().contains(working_copy_commit.id());

        transaction
            .repo_mut()
            .remove_wc_commit(workspace_name)
            .await?;
        if should_abandon {
            transaction
                .repo_mut()
                .record_abandoned_commit(&working_copy_commit);
        }
        transaction.repo_mut().rebase_descendants().await?;
        transaction
            .commit(format!(
                "forget Devspace checkout workspace {}",
                workspace_name.as_symbol()
            ))
            .await?;
    }
    forget_workspace_record(&entry.native_repository_path, workspace_name)
}

fn forget_workspace_record(
    repository_path: &Path,
    workspace_name: &WorkspaceName,
) -> Result<(), CommandError> {
    SimpleWorkspaceStore::load(repository_path)
        .and_then(|store| store.forget(&[workspace_name]))
        .map_err(CommandError::from)
}

fn not_checkout(path: &Path) -> CommandError {
    user_error(format!(
        "{} is not a Devspace checkout; nothing was touched",
        path.display()
    ))
}
