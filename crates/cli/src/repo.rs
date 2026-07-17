use std::io::Write as _;

use devspace_machine::{
    ControlPlaneClient, ControlPlaneClientError, ControlPlaneRemoteErrorKind, MachineStore,
    RepositoryCreationKey, RepositoryCreationTarget, RepositoryName,
};
use jj_cli::cli_util::CommandHelper;
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;

use crate::add::{AddArgs, add_checkout};
use crate::remove::{RemoveArgs, remove_checkout};
use crate::sync::{SyncArgs, run_sync};

#[derive(clap::Subcommand)]
pub(crate) enum DevspaceCommand {
    /// Create a checkout of a repository already present on this machine.
    Add(AddArgs),
    /// Remove a disposable checkout while preserving its repository.
    Remove(RemoveArgs),
    /// Manage cloud repositories.
    Repo(RepoArgs),
    #[command(hide = true)]
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
    New { name: String },
}

pub(crate) async fn run(
    ui: &mut Ui,
    command: &CommandHelper,
    args: DevspaceCommand,
) -> Result<(), CommandError> {
    match args {
        DevspaceCommand::Add(args) => add_checkout(ui, command, args).await,
        DevspaceCommand::Remove(args) => remove_checkout(ui, command, args).await,
        DevspaceCommand::Repo(RepoArgs {
            command: RepoCommand::New { name },
        }) => create_empty_repository(ui, command, name).await,
        DevspaceCommand::Sync(args) => run_sync(ui, command, args).await,
    }
}

async fn create_empty_repository(
    ui: &mut Ui,
    command: &CommandHelper,
    name: String,
) -> Result<(), CommandError> {
    let name = RepositoryName::parse(name).map_err(|error| user_error(error.to_string()))?;
    let store = MachineStore::platform_default().map_err(|error| user_error(error.to_string()))?;
    let config = store
        .load_config()
        .map_err(|error| user_error(error.to_string()))?;
    let client = ControlPlaneClient::new(&config).map_err(|error| user_error(error.to_string()))?;
    let target = RepositoryCreationTarget::from_config(&config);
    let mut intent = store
        .begin_repository_creation(name.clone(), target.clone(), new_creation_key()?)
        .map_err(|error| user_error(error.to_string()))?;

    let identity = if let Some(identity) = intent.identity() {
        identity.clone()
    } else {
        // jj-cli drives commands on its own executor. reqwest requires a Tokio
        // reactor, so own that narrow transport runtime here rather than making
        // the embedded command runner or machine-store work Tokio-specific.
        let runtime = tokio::runtime::Runtime::new()
            .map_err(|_| user_error("failed to start the cloud transport runtime"))?;
        let mut retirement_retry_available = true;
        loop {
            if let Some(identity) = intent.identity() {
                break identity.clone();
            }
            match runtime.block_on(client.create_repository(&name, intent.key().bytes())) {
                Ok(repository) => {
                    intent = store
                        .record_repository_created(&intent, repository.identity)
                        .map_err(|error| user_error(error.to_string()))?;
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
                                .map_err(|error| user_error(error.to_string()))?;
                            if retirement_retry_available {
                                retirement_retry_available = false;
                                intent = store
                                    .begin_repository_creation(
                                        name.clone(),
                                        target.clone(),
                                        new_creation_key()?,
                                    )
                                    .map_err(|error| user_error(error.to_string()))?;
                                continue;
                            }
                        }
                        Some(
                            ControlPlaneRemoteErrorKind::RepositoryNameInUse
                            | ControlPlaneRemoteErrorKind::IdempotencyKeyReused,
                        ) => {
                            store
                                .discard_repository_creation(&intent)
                                .map_err(|error| user_error(error.to_string()))?;
                        }
                        Some(ControlPlaneRemoteErrorKind::Other) | None => {}
                    }
                    return Err(user_error(error.to_string()));
                }
            }
        }
    };

    let entry = store
        .register_repository(name.clone(), identity.clone())
        .map_err(|error| user_error(error.to_string()))?;
    let (settings, _) = command.settings_for_new_workspace(ui, &entry.native_repository_path)?;
    store
        .materialize_repository(&name, &identity, &settings)
        .await
        .map_err(|error| user_error(error.to_string()))?;
    store
        .complete_repository_creation(&intent)
        .map_err(|error| user_error(error.to_string()))?;

    writeln!(ui.status(), "Created repository `{name}`.")?;
    Ok(())
}

fn new_creation_key() -> Result<RepositoryCreationKey, CommandError> {
    let mut key = [0; 16];
    getrandom::fill(&mut key)
        .map_err(|_| user_error("failed to generate a repository creation idempotency key"))?;
    Ok(RepositoryCreationKey::new(key))
}
