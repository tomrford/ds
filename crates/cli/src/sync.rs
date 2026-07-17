use std::error::Error as _;
use std::io::Write as _;

use devspace_machine::{
    HttpTransport, MachineRepository, MachineStore, MachineStoreError, MachineSyncStore,
    RepositoryName, SyncEngine, SyncEngineError,
};
use jj_cli::cli_util::CommandHelper;
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;

use crate::checkout::reject_unsupported_global_options;

#[derive(clap::Args)]
pub(crate) struct SyncArgs {
    #[command(subcommand)]
    command: SyncCommand,
}

#[derive(clap::Subcommand)]
enum SyncCommand {
    /// Synchronize one machine repository with its cloud authority.
    Run {
        /// Repository name in the local machine catalog.
        #[arg(long)]
        repository: String,
    },
}

pub(crate) async fn run_sync(
    ui: &mut Ui,
    command: &CommandHelper,
    args: SyncArgs,
) -> Result<(), CommandError> {
    match args.command {
        SyncCommand::Run { repository } => sync_repository(ui, command, repository).await,
    }
}

async fn sync_repository(
    ui: &mut Ui,
    command: &CommandHelper,
    repository: String,
) -> Result<(), CommandError> {
    reject_unsupported_global_options(command, "sync run")?;
    let name = RepositoryName::parse(repository).map_err(|error| user_error(error.to_string()))?;
    let store = MachineStore::platform_default().map_err(|error| user_error(error.to_string()))?;
    let entry = store
        .resolve(&name)
        .map_err(|error| user_error(error.to_string()))?
        .ok_or_else(|| {
            user_error(format!(
                "Repository `{name}` is not present in this machine store."
            ))
        })?;

    let _guard = match store.try_lock_repository_sync(&entry.identity) {
        Ok(guard) => guard,
        Err(MachineStoreError::RepositorySyncAlreadyLocked { .. }) => {
            // A concurrent run owns the durable outbox. Operations recorded after
            // its upload phase remain local and are discovered by the next run.
            writeln!(
                ui.status(),
                "Repository `{name}` is already being synchronized; skipping."
            )?;
            return Ok(());
        }
        Err(error) => return Err(user_error(error.to_string())),
    };

    let config = store
        .load_config()
        .map_err(|error| user_error(error.to_string()))?;
    let mut repository = MachineRepository::open(&entry.native_repository_path, command.settings())
        .await
        .map_err(|error| user_error(error.to_string()))?;
    let state = MachineSyncStore::open(store.repository_sync_path(&entry.identity))
        .map_err(|error| user_error(error.to_string()))?;
    let incarnation = parse_incarnation(entry.identity.incarnation.as_str());
    let mut transport =
        HttpTransport::new(&config, entry.identity.repository_id.as_str(), incarnation)
            .map_err(|error| user_error(error.to_string()))?;
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|_| user_error("failed to start the cloud transport runtime"))?;
    runtime
        .block_on(
            SyncEngine::new(
                &mut repository,
                &state,
                store.repository_packs_path(&entry.identity),
                &mut transport,
            )
            .run(),
        )
        .map_err(|error| user_error(sync_error_message(&error)))?;
    Ok(())
}

fn parse_incarnation(value: &str) -> [u8; 16] {
    debug_assert_eq!(value.len(), 32);
    std::array::from_fn(|index| {
        u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .expect("catalog incarnations are validated lowercase hex")
    })
}

fn sync_error_message(error: &SyncEngineError) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        message.push_str(": ");
        message.push_str(&error.to_string());
        source = error.source();
    }
    message
}
