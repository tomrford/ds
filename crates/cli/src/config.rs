use std::ffi::OsString;
use std::io::{self, Write as _};
use std::process::ExitCode;

use clap::{CommandFactory as _, Parser as _};
use devspace_machine::MachineStore;

#[derive(clap::Parser)]
#[command(name = "ds config", about = "Manage Devspace configuration")]
struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(clap::Subcommand)]
enum ConfigCommand {
    /// Print the Devspace config file path.
    Path,
    /// Get a Devspace config value.
    Get { key: ConfigKey },
    /// Set a Devspace config value.
    Set {
        key: ConfigKey,
        #[arg(action = clap::ArgAction::Set)]
        value: bool,
    },
}

#[derive(Clone, clap::ValueEnum)]
enum ConfigKey {
    GitShim,
    #[value(name = "context.auto-sync")]
    ContextAutoSync,
}

pub(crate) fn intercept(args: &[OsString]) -> Option<ExitCode> {
    let (tail, help) = match args {
        [config, tail @ ..] if config == "config" => (tail, false),
        [help, config, tail @ ..] if help == "help" && config == "config" => (tail, true),
        _ => return None,
    };
    let parsed = ConfigArgs::try_parse_from(
        std::iter::once(OsString::from("ds config"))
            .chain(tail.iter().cloned())
            .chain(help.then(|| OsString::from("--help"))),
    );
    Some(match parsed {
        Ok(args) => run(args),
        Err(error) => {
            let code = error.exit_code();
            let _ = error.print();
            ExitCode::from(u8::try_from(code).unwrap_or(1))
        }
    })
}

fn run(args: ConfigArgs) -> ExitCode {
    match run_inner(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "Error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_inner(args: ConfigArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = MachineStore::platform_default()?;
    match args.command {
        ConfigCommand::Path => {
            println!("{}", store.config_path().display());
        }
        ConfigCommand::Get { key } => {
            let config = store.load_config()?;
            let value = match key {
                ConfigKey::GitShim => config.git_shim(),
                ConfigKey::ContextAutoSync => config.context_auto_sync(),
            };
            println!("{value}");
        }
        ConfigCommand::Set { key, value } => {
            store.update_config(|config| match key {
                ConfigKey::GitShim => config.with_git_shim(value),
                ConfigKey::ContextAutoSync => config.with_context_auto_sync(value),
            })?;
        }
    }
    Ok(())
}

pub(crate) fn help_app() -> clap::Command {
    ConfigArgs::command()
}
