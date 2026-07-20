use std::io::Write as _;
use std::path::Path;

use devspace_machine::{CatalogEntry, MachineRepository, MachineStore, MachineSyncStore};
use jj_cli::cli_util::CommandHelper;
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;
use jj_lib::object_id::ObjectId as _;

use crate::checkout::{read_checkout_owner, reject_unsupported_global_options};

struct LocalSyncStatus {
    has_sync_state: bool,
    pending: usize,
}

pub(crate) async fn write_status_line(
    ui: &mut Ui,
    command: &CommandHelper,
) -> Result<(), CommandError> {
    if command.matches().subcommand_name() != Some("status") {
        return Ok(());
    }
    let loader = command.workspace_loader()?;
    if read_checkout_owner(loader.workspace_root()).is_err() {
        return Ok(());
    }
    let Some((machine_store, entry)) =
        catalog_entry_for_path(loader.repo_path()).map_err(user_error)?
    else {
        return Ok(());
    };
    let workspace = command.load_workspace()?;
    let current_heads = workspace
        .repo_loader()
        .op_heads_store()
        .get_op_heads()
        .await?
        .into_iter()
        .map(|head| operation_id(head.as_bytes()))
        .collect::<Result<Vec<_>, _>>()?;
    let status = local_sync_status(&machine_store, &entry, &current_heads).map_err(user_error)?;

    if !status.has_sync_state {
        writeln!(ui.status(), "sync: never synchronized")?;
    } else if status.pending == 0 {
        writeln!(
            ui.status(),
            "sync: in sync with cloud as of the last successful sync"
        )?;
    } else {
        writeln!(
            ui.status(),
            "sync: {} operation{} pending upload",
            status.pending,
            if status.pending == 1 { "" } else { "s" }
        )?;
    }
    Ok(())
}

pub(crate) async fn write_catalog_status(
    ui: &mut Ui,
    command: &CommandHelper,
) -> Result<(), CommandError> {
    reject_unsupported_global_options(command, "sync status")?;
    let store = MachineStore::platform_default().map_err(|error| user_error(error.to_string()))?;
    writeln!(
        ui.status(),
        "daemon: {}",
        if crate::daemon::is_running(&store) {
            "running"
        } else {
            "not running"
        }
    )?;

    for entry in store
        .entries()
        .map_err(|error| user_error(error.to_string()))?
    {
        let complete = entry.native_repository_path.is_dir();
        let current_heads = if complete {
            let repository =
                MachineRepository::open(&entry.native_repository_path, command.settings())
                    .await
                    .map_err(|error| user_error(error.to_string()))?;
            repository
                .current_operation_heads()
                .await
                .map_err(|error| user_error(error.to_string()))?
                .into_iter()
                .collect()
        } else {
            Vec::new()
        };
        let status = local_sync_status(&store, &entry, &current_heads).map_err(user_error)?;
        write!(ui.status(), "{}: ", entry.name)?;
        if !complete {
            write!(ui.status(), "incomplete clone; ")?;
        }
        write!(ui.status(), "pending: {}", status.pending)?;
        if !status.has_sync_state {
            write!(ui.status(), "; never synchronized")?;
        } else if status.pending == 0 {
            write!(
                ui.status(),
                "; in sync with cloud as of the last successful sync"
            )?;
        }
        writeln!(ui.status())?;
    }
    Ok(())
}

fn local_sync_status(
    store: &MachineStore,
    entry: &CatalogEntry,
    current_heads: &[[u8; 64]],
) -> Result<LocalSyncStatus, String> {
    let sync_path = store.repository_sync_path(&entry.identity);
    if !sync_path.exists() {
        return Ok(LocalSyncStatus {
            has_sync_state: false,
            pending: current_heads.len(),
        });
    }

    let sync_store = MachineSyncStore::open(sync_path).map_err(|error| error.to_string())?;
    let outbox = sync_store
        .load_outbox()
        .map_err(|error| error.to_string())?;
    let state = sync_store.load_state().map_err(|error| error.to_string())?;
    let pending = outbox.map_or_else(
        || {
            current_heads
                .iter()
                .filter(|head| !state.accepted_heads.contains(*head))
                .count()
        },
        |batch| batch.entries.len(),
    );
    Ok(LocalSyncStatus {
        has_sync_state: true,
        pending,
    })
}

fn operation_id(bytes: &[u8]) -> Result<[u8; 64], CommandError> {
    bytes.try_into().map_err(|_| {
        user_error(format!(
            "Local operation head has an invalid {}-byte ID",
            bytes.len()
        ))
    })
}

fn catalog_entry_for_path(path: &Path) -> Result<Option<(MachineStore, CatalogEntry)>, String> {
    let store = MachineStore::platform_default().map_err(|error| error.to_string())?;
    let path = dunce::canonicalize(path).map_err(|error| error.to_string())?;
    let entry = store
        .entries()
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|entry| {
            dunce::canonicalize(&entry.native_repository_path)
                .is_ok_and(|candidate| candidate == path)
        });
    Ok(entry.map(|entry| (store, entry)))
}
