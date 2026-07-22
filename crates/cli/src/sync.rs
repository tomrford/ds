use std::error::Error as _;
use std::io::Write as _;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use devspace_machine::{
    CatalogEntry, HttpTransport, MachineConfig, MachineRepository, MachineStore, MachineStoreError,
    MachineSyncStore, RepositoryIdentity, RepositoryName, RepositorySyncGuard, SyncEngine,
    SyncEngineError, decode_lower_hex,
};
use jj_cli::cli_util::CommandHelper;
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;
use jj_lib::settings::UserSettings;

use crate::checkout::reject_unsupported_global_options;
use crate::git::{CLOUD_RUNTIME_ERROR, cloud_runtime};

const FOREGROUND_SYNC_LOCK_WAIT: Duration = Duration::from_secs(60);
const SYNC_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(100);

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
        // jj owns global `--repository`; reusing it here makes CliRunner reload
        // aliases after Devspace's collision migration has self-disarmed.
        #[arg(long = "repository-name")]
        sync_repository: String,
    },
    /// Show local synchronization state for every catalog repository.
    Status(SyncStatusArgs),
}

#[derive(clap::Args)]
struct SyncStatusArgs {
    /// Print synchronization state as JSON.
    #[arg(long)]
    json: bool,
}

pub(crate) async fn run_sync(
    ui: &mut Ui,
    command: &CommandHelper,
    args: SyncArgs,
) -> Result<(), CommandError> {
    match args.command {
        SyncCommand::Run {
            sync_repository: name,
        } => sync_repository(ui, command, name).await,
        SyncCommand::Status(args) => {
            crate::sync_status::write_catalog_status(ui, command, args.json).await
        }
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

async fn run_sync_entry_locked(
    store: &MachineStore,
    entry: &CatalogEntry,
    settings: &UserSettings,
) -> Result<LockedSyncRun, String> {
    validate_sync_entry(entry)?;

    let guard = match store.try_lock_repository_sync(&entry.identity) {
        Ok(guard) => guard,
        Err(MachineStoreError::RepositorySyncAlreadyLocked { .. }) => {
            return Ok(LockedSyncRun::AlreadyLocked);
        }
        Err(error) => return Err(error.to_string()),
    };
    run_sync_entry_after_lock(store, entry, settings, guard).await
}

pub(crate) async fn run_sync_entry_foreground_locked(
    ui: &mut Ui,
    store: &MachineStore,
    entry: &CatalogEntry,
    settings: &UserSettings,
) -> Result<LockedSyncRun, String> {
    validate_sync_entry(entry)?;
    let Some(guard) = wait_for_repository_sync_lock(ui, store, entry)? else {
        return Ok(LockedSyncRun::AlreadyLocked);
    };
    run_sync_entry_after_lock(store, entry, settings, guard).await
}

pub(crate) fn wait_for_repository_sync_lock(
    ui: &mut Ui,
    store: &MachineStore,
    entry: &CatalogEntry,
) -> Result<Option<RepositorySyncGuard>, String> {
    let deadline = Instant::now() + FOREGROUND_SYNC_LOCK_WAIT;
    let mut reported_wait = false;
    loop {
        match store.try_lock_repository_sync(&entry.identity) {
            Ok(guard) => return Ok(Some(guard)),
            Err(MachineStoreError::RepositorySyncAlreadyLocked { .. }) => {
                let now = Instant::now();
                if now >= deadline {
                    return Ok(None);
                }
                if !reported_wait {
                    writeln!(
                        ui.status(),
                        "Waiting for an in-flight operation on repository `{}` to finish...",
                        entry.name
                    )
                    .map_err(|error| error.to_string())?;
                    reported_wait = true;
                }
                thread::sleep(SYNC_LOCK_POLL_INTERVAL.min(deadline - now));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn validate_sync_entry(entry: &CatalogEntry) -> Result<(), String> {
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
    Ok(())
}

async fn run_sync_entry_after_lock(
    store: &MachineStore,
    entry: &CatalogEntry,
    settings: &UserSettings,
    guard: RepositorySyncGuard,
) -> Result<LockedSyncRun, String> {
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
    let runtime = cloud_runtime().map_err(|_| CLOUD_RUNTIME_ERROR.to_owned())?;
    runtime
        .block_on(SyncEngine::new(repository, &state, packs_path, &mut transport).run())
        .map(|_| ())
        .map_err(|error| sync_error_message(&error))
}

fn parse_incarnation(value: &str) -> [u8; 16] {
    decode_lower_hex(value).expect("catalog incarnations are validated lowercase hex")
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
