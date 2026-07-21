use std::fs;
use std::io::Write as _;

use devspace_machine::{
    CatalogEntry, ControlPlaneClient, ControlPlaneClientError, ControlPlaneRemoteErrorKind,
    MachineConfig, MachineStore, RepositoryCreationIntent,
    RepositoryCreationIntentError, RepositoryCreationKey, RepositoryCreationTarget,
    RepositoryIdentity, RepositoryName,
};
use jj_cli::cli_util::CommandHelper;
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;
use jj_lib::workspace_store::{SimpleWorkspaceStore, WorkspaceStore as _};

use crate::add::{AddArgs, add_checkout};
use crate::context::{ContextArgs, run_context};
use crate::daemon::{DaemonArgs, run_daemon};
use crate::doctor::run_doctor;
use crate::init::{InitArgs, import_git_repository, init_repository};
use crate::list::list_workspaces;
use crate::remove::{RemoveArgs, remove_checkout};
use crate::skill::{SkillArgs, print_skill};
use crate::sync::{SyncArgs, run_sync};

#[derive(clap::Subcommand)]
pub(crate) enum DevspaceCommand {
    /// Create a checkout, cloning the repository on first use.
    Add(AddArgs),
    /// Initialize a Devspace repository from a Git remote.
    Init(InitArgs),
    /// List every workspace of the current repository.
    List,
    /// Check this machine's Devspace setup.
    Doctor,
    /// Pin read-only Git reference repos under .repos/.
    Context(ContextArgs),
    /// Remove a disposable checkout while preserving its repository.
    Remove(RemoveArgs),
    #[command(hide = true)]
    Daemon(DaemonArgs),
    /// Manage cloud repositories.
    Repo(RepoArgs),
    /// Print agent-facing guidance for working with Devspace.
    Skill(SkillArgs),
    /// Manage machine synchronization.
    Sync(SyncArgs),
}

#[derive(clap::Args)]
pub(crate) struct RepoArgs {
    #[command(subcommand)]
    command: RepoCommand,
}

#[derive(clap::Subcommand)]
enum RepoCommand {
    /// Create an empty repository in the cloud and on this machine.
    New { name: Option<String> },
    /// Import a Git remote into the cloud without creating a checkout.
    Add {
        git_url: String,
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
    },
    /// Rename a cloud repository and its local catalog entry.
    Rename { old: String, new: String },
    /// Delete a cloud repository and this machine's local copy.
    Remove {
        name: String,
        /// Delete without an interactive confirmation.
        #[arg(long)]
        force: bool,
    },
    /// List active cloud repositories.
    List,
}

pub(crate) async fn run(
    ui: &mut Ui,
    command: &CommandHelper,
    args: DevspaceCommand,
) -> Result<(), CommandError> {
    match args {
        DevspaceCommand::Add(args) => add_checkout(ui, command, args).await,
        DevspaceCommand::Init(args) => init_repository(ui, command, args).await,
        DevspaceCommand::List => list_workspaces(ui, command).await,
        DevspaceCommand::Doctor => run_doctor(ui, command).await,
        DevspaceCommand::Context(args) => run_context(ui, command, args).await,
        DevspaceCommand::Remove(args) => remove_checkout(ui, command, args).await,
        DevspaceCommand::Daemon(args) => {
            crate::boundary_sync::suppress();
            run_daemon(ui, command, args).await
        }
        DevspaceCommand::Repo(args) => run_repo(ui, command, args).await,
        DevspaceCommand::Skill(args) => print_skill(ui, args),
        DevspaceCommand::Sync(args) => {
            crate::boundary_sync::suppress();
            run_sync(ui, command, args).await
        }
    }
}

async fn run_repo(
    ui: &mut Ui,
    command: &CommandHelper,
    args: RepoArgs,
) -> Result<(), CommandError> {
    match args.command {
        RepoCommand::New { name } => create_empty_repository(ui, command, name).await,
        RepoCommand::Add { git_url, name } => {
            let imported = import_git_repository(ui, command, git_url, name).await?;
            imported.write_summary(ui)
        }
        RepoCommand::Rename { old, new } => rename_repository(ui, old, new),
        RepoCommand::Remove { name, force } => remove_repository(ui, command, name, force).await,
        RepoCommand::List => list_repositories(ui, command).await,
    }
}

async fn create_empty_repository(
    ui: &mut Ui,
    command: &CommandHelper,
    requested_name: Option<String>,
) -> Result<(), CommandError> {
    const DEFAULT_NAME: &str = "new-repo";
    const MAX_DEFAULT_ATTEMPTS: usize = 20;
    let explicit = requested_name.is_some();
    let base = requested_name.unwrap_or_else(|| DEFAULT_NAME.to_owned());
    let mut pending = None;
    for attempt in 1..=MAX_DEFAULT_ATTEMPTS {
        let candidate = if attempt == 1 {
            base.clone()
        } else {
            format!("{base}-{attempt}")
        };
        let name = parse_repository_name(candidate)?;
        match create_cloud_repository(name) {
            Ok(created) => {
                pending = Some(created);
                break;
            }
            Err(CloudRepositoryCreationError::NameTaken(error)) if !explicit => {
                if attempt == MAX_DEFAULT_ATTEMPTS {
                    return Err(user_error(format!(
                        "{error}; exhausted {MAX_DEFAULT_ATTEMPTS} default repository names"
                    )));
                }
            }
            Err(error) => return Err(user_error(error.to_string())),
        }
    }
    let pending = pending.expect("bounded creation loop returns or records a repository");
    let entry = materialize_cloud_repository(ui, command, &pending).await?;
    crate::boundary_sync::record(&entry);

    writeln!(ui.status(), "Created repository `{}`.", entry.name)?;
    Ok(())
}

#[derive(Debug)]
pub(crate) enum CloudRepositoryCreationError {
    NameTaken(String),
    Other(String),
}

impl std::fmt::Display for CloudRepositoryCreationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NameTaken(message) | Self::Other(message) => formatter.write_str(message),
        }
    }
}

pub(crate) struct PendingRepositoryCreation {
    pub store: MachineStore,
    pub config: MachineConfig,
    pub intent: RepositoryCreationIntent,
    pub identity: RepositoryIdentity,
}

pub(crate) fn name_taken_error(name: &RepositoryName) -> CloudRepositoryCreationError {
    CloudRepositoryCreationError::NameTaken(format!(
        "repository-name-taken: repository {name} already exists on this machine"
    ))
}

pub(crate) fn create_cloud_repository(
    name: RepositoryName,
) -> Result<PendingRepositoryCreation, CloudRepositoryCreationError> {
    let other = |message| CloudRepositoryCreationError::Other(message);
    let store = MachineStore::platform_default().map_err(|error| other(error.to_string()))?;
    let config = store
        .load_config()
        .map_err(|error| other(error.to_string()))?;
    let client = ControlPlaneClient::new(&config).map_err(|error| other(error.to_string()))?;
    let target = RepositoryCreationTarget::from_config(&config);
    let mut intent = store
        .begin_repository_creation(
            name.clone(),
            target.clone(),
            new_creation_key().map_err(other)?,
        )
        .map_err(|error| match error {
            RepositoryCreationIntentError::AlreadyRegistered(_) => name_taken_error(&name),
            error => other(error.to_string()),
        })?;

    let identity = if let Some(identity) = intent.identity() {
        identity.clone()
    } else {
        // jj-cli drives commands on its own executor. reqwest requires a Tokio
        // reactor, so own that narrow transport runtime here rather than making
        // the embedded command runner or machine-store work Tokio-specific.
        let runtime = tokio::runtime::Runtime::new()
            .map_err(|_| other("failed to start the cloud transport runtime".to_owned()))?;
        let mut retirement_retry_available = true;
        loop {
            if let Some(identity) = intent.identity() {
                break identity.clone();
            }
            match runtime.block_on(client.create_repository(&name, intent.key().bytes())) {
                Ok(repository) => {
                    intent = store
                        .record_repository_created(&intent, repository.identity)
                        .map_err(|error| other(error.to_string()))?;
                }
                Err(error) => {
                    let terminal_kind = match &error {
                        ControlPlaneClientError::Remote { kind, .. } => Some(*kind),
                        _ => None,
                    };
                    match terminal_kind {
                        Some(
                            ControlPlaneRemoteErrorKind::RepositoryCreationRetired
                            | ControlPlaneRemoteErrorKind::RepositoryCreationRetiring,
                        ) => {
                            store
                                .discard_repository_creation(&intent)
                                .map_err(|error| other(error.to_string()))?;
                            if retirement_retry_available {
                                retirement_retry_available = false;
                                intent = store
                                    .begin_repository_creation(
                                        name.clone(),
                                        target.clone(),
                                        new_creation_key().map_err(other)?,
                                    )
                                    .map_err(|error| other(error.to_string()))?;
                                continue;
                            }
                        }
                        Some(
                            ControlPlaneRemoteErrorKind::RepositoryNameInUse
                            | ControlPlaneRemoteErrorKind::IdempotencyKeyReused,
                        ) => {
                            store
                                .discard_repository_creation(&intent)
                                .map_err(|error| other(error.to_string()))?;
                        }
                        Some(
                            ControlPlaneRemoteErrorKind::RepositoryNotFound
                            | ControlPlaneRemoteErrorKind::Other,
                        )
                        | None => {}
                    }
                    if terminal_kind == Some(ControlPlaneRemoteErrorKind::RepositoryNameInUse) {
                        return Err(CloudRepositoryCreationError::NameTaken(error.to_string()));
                    }
                    return Err(other(error.to_string()));
                }
            }
        }
    };

    Ok(PendingRepositoryCreation {
        store,
        config,
        intent,
        identity,
    })
}

pub(crate) fn parse_repository_name(value: String) -> Result<RepositoryName, CommandError> {
    RepositoryName::parse(value)
        .map_err(|error| user_error(format!("invalid-repository-name: {error}")))
}

fn rename_repository(ui: &mut Ui, old: String, new: String) -> Result<(), CommandError> {
    let old = parse_repository_name(old)?;
    let new = parse_repository_name(new)?;
    let store = MachineStore::platform_default().map_err(display_error)?;
    let config = store.load_config().map_err(display_error)?;
    let client = ControlPlaneClient::new(&config).map_err(display_error)?;
    let local_old = store.resolve(&old).map_err(display_error)?;
    let local_new = store.resolve(&new).map_err(display_error)?;
    let runtime = cloud_runtime()?;
    let cloud = match runtime.block_on(client.rename_repository(&old, &new)) {
        Ok(repository) => repository,
        Err(ControlPlaneClientError::Remote {
            kind: ControlPlaneRemoteErrorKind::RepositoryNotFound,
            ..
        }) => {
            let resolved = runtime.block_on(client.resolve_repository(&new));
            match resolved {
                Ok(repository)
                    if local_old
                        .as_ref()
                        .or(local_new.as_ref())
                        .is_some_and(|entry| entry.identity == repository.identity) =>
                {
                    repository
                }
                _ => {
                    return Err(user_error(format!(
                        "repository-not-found: repository `{old}` was not found in the control plane"
                    )));
                }
            }
        }
        Err(error) => return Err(display_error(error)),
    };

    if let Some(entry) = local_old {
        store
            .rename_repository(&old, cloud.name.clone(), &entry.identity)
            .map_err(display_error)?;
    } else if let Some(entry) = local_new
        && entry.identity != cloud.identity
    {
        return Err(user_error(format!(
            "Local repository `{new}` has a different cloud identity; the cloud rename succeeded but the local catalog was not changed."
        )));
    }
    writeln!(ui.status(), "Renamed repository `{old}` to `{new}`.")?;
    Ok(())
}

async fn remove_repository(
    ui: &mut Ui,
    command: &CommandHelper,
    name: String,
    force: bool,
) -> Result<(), CommandError> {
    let name = parse_repository_name(name)?;
    let store = MachineStore::platform_default().map_err(display_error)?;
    let config = store.load_config().map_err(display_error)?;
    let client = ControlPlaneClient::new(&config).map_err(display_error)?;
    let local = store.resolve(&name).map_err(display_error)?;
    let runtime = cloud_runtime()?;
    let identity = match &local {
        Some(entry) => entry.identity.clone(),
        None => match runtime.block_on(client.resolve_repository(&name)) {
            Ok(repository) => repository.identity,
            Err(ControlPlaneClientError::Remote {
                kind: ControlPlaneRemoteErrorKind::RepositoryNotFound,
                ..
            }) => {
                return Err(user_error(format!(
                    "repository-not-found: repository `{name}` was not found in the control plane"
                )));
            }
            Err(error) => return Err(display_error(error)),
        },
    };

    if let Some(entry) = &local {
        let paths = registered_checkout_paths(entry, command).await?;
        if !paths.is_empty() {
            return Err(user_error(format!(
                "Repository `{name}` has registered checkouts on this machine:\n{}\nRun `ds remove <path>` for each checkout before deleting the repository.",
                paths
                    .iter()
                    .map(|path| format!("  {}", path.display()))
                    .collect::<Vec<_>>()
                    .join("\n")
            )));
        }
        let remote_workspaces = remote_workspace_names(entry, command).await?;
        if !remote_workspaces.is_empty() {
            writeln!(
                ui.warning_default(),
                "Other machines still have workspaces in `{name}`: {}. Deletion will proceed.",
                remote_workspaces.join(", ")
            )?;
        }
    }

    if !force {
        if !ui.can_prompt() {
            return Err(user_error(
                "Repository deletion requires `--force` when not interactive.",
            ));
        }
        let confirmation = ui
            .prompt(&format!(
                "Retype repository name `{name}` to confirm deletion"
            ))
            .map_err(display_error)?;
        if confirmation != name.as_str() {
            return Err(user_error(
                "Repository deletion confirmation did not match; nothing was deleted.",
            ));
        }
    }

    let already_deleted = match runtime.block_on(client.delete_repository(&name, &identity)) {
        Ok(_) => false,
        Err(ControlPlaneClientError::Remote {
            kind: ControlPlaneRemoteErrorKind::RepositoryNotFound,
            ..
        }) if local.is_some() => true,
        Err(error) => return Err(display_error(error)),
    };

    let cleanup = local
        .as_ref()
        .map(|entry| cleanup_local_repository(ui, &store, entry))
        .transpose()?;
    writeln!(ui.status(), "Deleted repository `{name}`.")?;
    if already_deleted {
        writeln!(ui.status(), "The cloud repository was already deleted.")?;
    }
    if cleanup.is_some() {
        writeln!(ui.status(), "Removed this machine's local repository data.")?;
    }
    Ok(())
}

async fn list_repositories(ui: &mut Ui, command: &CommandHelper) -> Result<(), CommandError> {
    let store = MachineStore::platform_default().map_err(display_error)?;
    let config = store.load_config().map_err(display_error)?;
    let client = ControlPlaneClient::new(&config).map_err(display_error)?;
    let repositories = cloud_runtime()?
        .block_on(client.list_repositories())
        .map_err(|error| match error {
            ControlPlaneClientError::Request(_) => user_error(
                "control-plane-unreachable: cannot list repositories while the control plane is offline",
            ),
            error => display_error(error),
        })?;
    let mut local = store.entries().map_err(display_error)?;
    for repository in repositories {
        let matching = local
            .iter()
            .position(|entry| entry.identity == repository.identity);
        let Some(index) = matching else {
            writeln!(ui.stdout(), "{}", repository.name)?;
            continue;
        };
        let mut entry = local.remove(index);
        if entry.name != repository.name {
            let old_name = entry.name.clone();
            entry = store
                .rename_repository(&old_name, repository.name.clone(), &repository.identity)
                .map_err(|error| {
                    user_error(format!(
                        "local-catalog-rename-failed: cloud calls this repository `{}`, but the local catalog could not self-heal: {error}",
                        repository.name
                    ))
                })?;
        }
        let paths = registered_checkout_paths(&entry, command).await?;
        if paths.is_empty() {
            writeln!(ui.stdout(), "{} (local)", repository.name)?;
        } else {
            writeln!(
                ui.stdout(),
                "{} (local: {})",
                repository.name,
                paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )?;
        }
    }
    for entry in local {
        let paths = registered_checkout_paths(&entry, command).await?;
        if paths.is_empty() {
            cleanup_local_repository(ui, &store, &entry)?;
            writeln!(
                ui.status(),
                "Removed local repository `{}` because it was deleted in the cloud.",
                entry.name
            )?;
        } else {
            writeln!(
                ui.stdout(),
                "{} (deleted in cloud; local checkouts remain: {})",
                entry.name,
                paths
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )?;
        }
    }
    Ok(())
}

async fn registered_checkout_paths(
    entry: &CatalogEntry,
    command: &CommandHelper,
) -> Result<Vec<std::path::PathBuf>, CommandError> {
    if !entry.native_repository_path.exists() {
        return Ok(Vec::new());
    }
    let repository = devspace_machine::MachineRepository::open(
        &entry.native_repository_path,
        command.settings(),
    )
    .await
    .map_err(display_error)?;
    let workspace_store = SimpleWorkspaceStore::load(&entry.native_repository_path)
        .map_err(|error| user_error(error.to_string()))?;
    let mut paths = Vec::new();
    for workspace in repository.repo().view().wc_commit_ids().keys() {
        if let Some(path) = workspace_store
            .get_workspace_path(workspace)
            .map_err(|error| user_error(error.to_string()))?
        {
            let path = if path.is_absolute() {
                path
            } else {
                entry.native_repository_path.join(path)
            };
            paths.push(dunce::canonicalize(&path).unwrap_or(path));
        }
    }
    paths.sort();
    Ok(paths)
}

async fn remote_workspace_names(
    entry: &CatalogEntry,
    command: &CommandHelper,
) -> Result<Vec<String>, CommandError> {
    if !entry.native_repository_path.exists() {
        return Ok(Vec::new());
    }
    let repository = devspace_machine::MachineRepository::open(
        &entry.native_repository_path,
        command.settings(),
    )
    .await
    .map_err(display_error)?;
    let workspace_store = SimpleWorkspaceStore::load(&entry.native_repository_path)
        .map_err(|error| user_error(error.to_string()))?;
    let mut names = Vec::new();
    for workspace in repository.repo().view().wc_commit_ids().keys() {
        if workspace_store
            .get_workspace_path(workspace)
            .map_err(|error| user_error(error.to_string()))?
            .is_none()
        {
            names.push(workspace.as_str().to_owned());
        }
    }
    names.sort();
    Ok(names)
}

fn cleanup_local_repository(
    ui: &mut Ui,
    store: &MachineStore,
    entry: &CatalogEntry,
) -> Result<(), CommandError> {
    let Some(_sync_guard) =
        crate::sync::wait_for_repository_sync_lock(ui, store, entry).map_err(user_error)?
    else {
        return Err(user_error(
            "repository-sync-busy: repository synchronization is in progress; retry repository removal",
        ));
    };
    store
        .unregister_repository(&entry.name, &entry.identity)
        .map_err(display_error)?;
    let root = store.repository_removal_root(&entry.identity);
    match fs::symlink_metadata(&root) {
        Ok(_) => fs::remove_dir_all(&root).map_err(display_error)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(display_error(error)),
    }
    if let Some(shard) = root.parent() {
        let _ = fs::remove_dir(shard);
    }
    Ok(())
}

fn cloud_runtime() -> Result<tokio::runtime::Runtime, CommandError> {
    tokio::runtime::Runtime::new()
        .map_err(|_| user_error("failed to start the cloud transport runtime"))
}

fn display_error(error: impl std::fmt::Display) -> CommandError {
    user_error(error.to_string())
}

pub(crate) async fn materialize_cloud_repository(
    ui: &mut Ui,
    command: &CommandHelper,
    pending: &PendingRepositoryCreation,
) -> Result<CatalogEntry, CommandError> {
    let name = pending.intent.name();
    let entry = pending
        .store
        .register_repository(name.clone(), pending.identity.clone())
        .map_err(|error| user_error(error.to_string()))?;
    let (settings, _) = command.settings_for_new_workspace(ui, &entry.native_repository_path)?;
    pending
        .store
        .materialize_repository(name, &pending.identity, &settings)
        .await
        .map_err(|error| user_error(error.to_string()))?;
    pending
        .store
        .complete_repository_creation(&pending.intent)
        .map_err(|error| user_error(error.to_string()))?;
    Ok(entry)
}

fn new_creation_key() -> Result<RepositoryCreationKey, String> {
    let mut key = [0; 16];
    getrandom::fill(&mut key)
        .map_err(|_| "failed to generate a repository creation idempotency key".to_owned())?;
    Ok(RepositoryCreationKey::new(key))
}
