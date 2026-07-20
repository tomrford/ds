use std::error::Error as _;
use std::io::Write as _;
use std::path::Path;

use devspace_machine::{
    CatalogEntry, HttpTransport, MachineConfig, MachineRepository, MachineStore, MachineStoreError,
    MachineSyncStore, RepositoryIdentity, RepositoryName, RepositorySyncGuard, SyncEngine,
    SyncEngineError,
};
use jj_cli::cli_util::CommandHelper;
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;
use jj_lib::settings::UserSettings;

use crate::checkout::reject_unsupported_global_options;

#[derive(clap::Args)]
pub(crate) struct SyncArgs {
    #[command(subcommand)]
    command: SyncCommand,
}

#[derive(clap::Subcommand)]
enum SyncCommand {
    /// Synchronize one machine repository with its cloud authority.
    #[command(hide = true)]
    Run {
        /// Repository name in the local machine catalog.
        #[arg(long)]
        repository: String,
    },
    /// Show local synchronization state for every catalog repository.
    Status,
}

pub(crate) async fn run_sync(
    ui: &mut Ui,
    command: &CommandHelper,
    args: SyncArgs,
) -> Result<(), CommandError> {
    match args.command {
        SyncCommand::Run { repository } => sync_repository(ui, command, repository).await,
        SyncCommand::Status => crate::sync_status::write_catalog_status(ui, command).await,
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
    match run_sync_entry(&store, &entry, command.settings()).await {
        Ok(SyncRun::Completed) => Ok(()),
        Ok(SyncRun::AlreadyLocked) => {
            // A concurrent run owns the durable outbox. Operations recorded after
            // its upload phase remain local and are discovered by the next run.
            writeln!(
                ui.status(),
                "Repository `{name}` is already being synchronized; skipping."
            )?;
            Ok(())
        }
        Err(error) => Err(user_error(error)),
    }
}

pub(crate) enum SyncRun {
    Completed,
    AlreadyLocked,
}

pub(crate) enum LockedSyncRun {
    Completed(RepositorySyncGuard),
    AlreadyLocked,
}

pub(crate) async fn run_sync_entry(
    store: &MachineStore,
    entry: &CatalogEntry,
    settings: &UserSettings,
) -> Result<SyncRun, String> {
    match run_sync_entry_locked(store, entry, settings).await? {
        LockedSyncRun::Completed(_guard) => Ok(SyncRun::Completed),
        LockedSyncRun::AlreadyLocked => Ok(SyncRun::AlreadyLocked),
    }
}

pub(crate) async fn run_sync_entry_locked(
    store: &MachineStore,
    entry: &CatalogEntry,
    settings: &UserSettings,
) -> Result<LockedSyncRun, String> {
    let name = &entry.name;
    if !entry.native_repository_path.exists() {
        return Err(format!(
            "Repository `{name}` has an incomplete clone; run `ds add` again to finish it."
        ));
    }
    if !crate::bare_workspace::is_stock_bare_repository(&entry.native_repository_path) {
        return Err(format!(
            "Repository `{name}` is registered locally, but its native repository is invalid."
        ));
    }

    let guard = match store.try_lock_repository_sync(&entry.identity) {
        Ok(guard) => guard,
        Err(MachineStoreError::RepositorySyncAlreadyLocked { .. }) => {
            return Ok(LockedSyncRun::AlreadyLocked);
        }
        Err(error) => return Err(error.to_string()),
    };
    let config = store.load_config().map_err(|error| error.to_string())?;
    let mut repository = MachineRepository::open(&entry.native_repository_path, settings)
        .await
        .map_err(|error| error.to_string())?;
    run_sync_engine(
        &config,
        &entry.identity,
        &mut repository,
        &store.repository_sync_path(&entry.identity),
        &store.repository_packs_path(&entry.identity),
    )?;
    Ok(LockedSyncRun::Completed(guard))
}

pub(crate) fn run_sync_engine(
    config: &MachineConfig,
    identity: &RepositoryIdentity,
    repository: &mut MachineRepository,
    sync_path: &Path,
    packs_path: &Path,
) -> Result<(), String> {
    let state = MachineSyncStore::open(sync_path).map_err(|error| error.to_string())?;
    let incarnation = parse_incarnation(identity.incarnation.as_str());
    let mut transport = HttpTransport::new(config, identity.repository_id.as_str(), incarnation)
        .map_err(|error| error.to_string())?;
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|_| "failed to start the cloud transport runtime".to_owned())?;
    runtime
        .block_on(SyncEngine::new(repository, &state, packs_path, &mut transport).run())
        .map(|_| ())
        .map_err(|error| sync_error_message(&error))
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
