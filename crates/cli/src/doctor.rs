use std::io::Write as _;
use std::process::Command;

use devspace_machine::{
    CatalogEntry, ControlPlaneClient, HttpTransport, MachineConfig, MachineStore,
    PendingProjectionBatch, decode_lower_hex, encode_lower_hex,
};
use jj_cli::cli_util::CommandHelper;
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;

use crate::checkout::{read_checkout_owner, reject_unsupported_global_options};
use crate::git::{CLOUD_RUNTIME_ERROR, cloud_runtime};

pub(crate) async fn run_doctor(ui: &mut Ui, command: &CommandHelper) -> Result<(), CommandError> {
    reject_unsupported_global_options(command, "doctor")?;
    crate::boundary_sync::suppress();
    let mut failed = false;

    let store = match MachineStore::platform_default() {
        Ok(store) => Some(store),
        Err(error) => {
            failed = true;
            writeln!(ui.stdout(), "FAIL machine config: {error}")?;
            None
        }
    };
    let config = match store.as_ref().map(MachineStore::load_config) {
        Some(Ok(config)) => {
            writeln!(ui.stdout(), "OK machine config: config.toml is valid")?;
            Some(config)
        }
        Some(Err(error)) => {
            failed = true;
            writeln!(ui.stdout(), "FAIL machine config: {error}")?;
            None
        }
        None => None,
    };

    if let Some(config) = config.as_ref() {
        match check_cloud(config) {
            Ok(count) => writeln!(
                ui.stdout(),
                "OK cloud: reachable and authenticated ({count} active repositories)"
            )?,
            Err(error) => {
                failed = true;
                writeln!(ui.stdout(), "FAIL cloud: {error}")?;
            }
        }
    } else {
        failed = true;
        writeln!(ui.stdout(), "FAIL cloud: machine config is unavailable")?;
    }

    if let (Some(store), Some(config)) = (store.as_ref(), config.as_ref()) {
        check_projection(ui, store, config)?;
    } else {
        writeln!(
            ui.stdout(),
            "WARN projection: machine config is unavailable"
        )?;
    }

    match store.as_ref() {
        Some(store) if crate::daemon::is_running(store) => {
            writeln!(ui.stdout(), "OK daemon: running")?
        }
        Some(_) => writeln!(ui.stdout(), "WARN daemon: not running")?,
        None => writeln!(ui.stdout(), "WARN daemon: machine store is unavailable")?,
    }

    match Command::new("git").arg("--version").output() {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout);
            writeln!(ui.stdout(), "OK git: {}", version.trim())?;
        }
        Ok(output) => {
            failed = true;
            writeln!(ui.stdout(), "FAIL git: exited with {}", output.status)?;
        }
        Err(error) => {
            failed = true;
            writeln!(ui.stdout(), "FAIL git: not found on PATH ({error})")?;
        }
    }

    let aliases = crate::shadowed_aliases();
    if aliases.is_empty() {
        writeln!(ui.stdout(), "OK aliases: no jj aliases are shadowed")?;
    } else {
        writeln!(
            ui.stdout(),
            "WARN aliases: shadowed by ds commands: {}",
            aliases.join(", ")
        )?;
    }

    if let Ok(loader) = command.workspace_loader()
        && loader.workspace_root().join(".jj").is_dir()
    {
        match read_checkout_owner(loader.workspace_root()) {
            Ok(_) => writeln!(ui.stdout(), "OK checkout: ownership marker is valid")?,
            Err(error) => {
                failed = true;
                writeln!(ui.stdout(), "FAIL checkout: {error}")?;
            }
        }
    }

    if failed {
        Err(user_error("doctor found one or more failures"))
    } else {
        Ok(())
    }
}

fn check_projection(
    ui: &mut Ui,
    store: &MachineStore,
    config: &MachineConfig,
) -> Result<(), CommandError> {
    let entries = match store.entries() {
        Ok(entries) => entries,
        Err(error) => {
            writeln!(
                ui.stdout(),
                "WARN projection: repository catalog is unavailable ({error})"
            )?;
            return Ok(());
        }
    };
    for entry in entries {
        match pending_push_batches(config, &entry) {
            Ok(pending) if pending.is_empty() => writeln!(
                ui.stdout(),
                "OK projection: {}: no pending push batches",
                entry.name
            )?,
            Ok(pending) => {
                for PendingBatchReport {
                    batch,
                    owner_machine,
                } in pending
                {
                    let bookmarks = batch
                        .refs
                        .iter()
                        .map(|pending_ref| pending_ref.bookmark.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let owner_machine = owner_machine
                        .map(short_machine_id)
                        .unwrap_or_else(|| "unknown".to_owned());
                    writeln!(
                        ui.stdout(),
                        "WARN projection: {}: pending push batch on {} (bookmarks: {bookmarks}; \
                         owner machine {owner_machine}); pushes to these bookmarks are blocked \
                         until the batch is recovered",
                        entry.name,
                        batch.remote,
                    )?;
                }
            }
            Err(error) => writeln!(
                ui.stdout(),
                "WARN projection: {}: cloud unreachable ({error})",
                entry.name
            )?,
        }
    }
    Ok(())
}

struct PendingBatchReport {
    batch: PendingProjectionBatch,
    owner_machine: Option<[u8; 16]>,
}

fn pending_push_batches(
    config: &MachineConfig,
    entry: &CatalogEntry,
) -> Result<Vec<PendingBatchReport>, String> {
    let incarnation = decode_lower_hex(entry.identity.incarnation.as_str())
        .map_err(|_| "repository incarnation is invalid".to_owned())?;
    let transport = HttpTransport::new(config, entry.identity.repository_id.as_str(), incarnation)
        .map_err(|error| error.to_string())?;
    let runtime = cloud_runtime().map_err(|_| CLOUD_RUNTIME_ERROR.to_owned())?;
    runtime.block_on(async {
        let snapshot = transport
            .get(0, None)
            .await
            .map_err(|error| error.to_string())?;
        let mut pending = Vec::with_capacity(snapshot.pending.len());
        for batch in snapshot.pending {
            let replay = transport.get_push_replay(batch.batch_id).await.ok();
            pending.push(PendingBatchReport {
                batch,
                owner_machine: replay.map(|replay| replay.owner_machine),
            });
        }
        Ok(pending)
    })
}

fn short_machine_id(machine_id: [u8; 16]) -> String {
    encode_lower_hex(&machine_id)[..12].to_owned()
}

fn check_cloud(config: &MachineConfig) -> Result<usize, String> {
    let client = ControlPlaneClient::new(config).map_err(|error| error.to_string())?;
    let runtime = cloud_runtime().map_err(|_| CLOUD_RUNTIME_ERROR.to_owned())?;
    runtime
        .block_on(client.list_repositories())
        .map(|repositories| repositories.len())
        .map_err(|error| error.to_string())
}
