use std::io::Write as _;
use std::process::Command;

use devspace_machine::{ControlPlaneClient, MachineConfig, MachineStore};
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

fn check_cloud(config: &MachineConfig) -> Result<usize, String> {
    let client = ControlPlaneClient::new(config).map_err(|error| error.to_string())?;
    let runtime = cloud_runtime().map_err(|_| CLOUD_RUNTIME_ERROR.to_owned())?;
    runtime
        .block_on(client.list_repositories())
        .map(|repositories| repositories.len())
        .map_err(|error| error.to_string())
}
