use std::io::Write as _;
use std::path::Path;

use devspace_machine::{CatalogEntry, MachineStore, MachineSyncStore};
use jj_cli::cli_util::CommandHelper;
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;
use jj_lib::object_id::ObjectId as _;

use crate::checkout::read_checkout_owner;

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
    let sync_path = machine_store.repository_sync_path(&entry.identity);
    if !sync_path.exists() {
        writeln!(ui.status(), "sync: never synchronized")?;
        return Ok(());
    }

    let sync_store =
        MachineSyncStore::open(sync_path).map_err(|error| user_error(error.to_string()))?;
    let outbox = sync_store
        .load_outbox()
        .map_err(|error| user_error(error.to_string()))?;
    let state = sync_store
        .load_state()
        .map_err(|error| user_error(error.to_string()))?;
    let workspace = command.load_workspace()?;
    let current_heads: Vec<[u8; 64]> = workspace
        .repo_loader()
        .op_heads_store()
        .get_op_heads()
        .await?
        .into_iter()
        .map(|head| {
            head.as_bytes().try_into().map_err(|_| {
                user_error(format!(
                    "Local operation head has an invalid {}-byte ID",
                    head.as_bytes().len()
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let pending = outbox.map_or_else(
        || {
            current_heads
                .iter()
                .filter(|head| !state.accepted_heads.contains(*head))
                .count()
        },
        |batch| batch.entries.len(),
    );

    if pending == 0 {
        writeln!(
            ui.status(),
            "sync: in sync with cloud as of the last successful sync"
        )?;
    } else {
        writeln!(
            ui.status(),
            "sync: {pending} operation{} pending upload",
            if pending == 1 { "" } else { "s" }
        )?;
    }
    Ok(())
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
