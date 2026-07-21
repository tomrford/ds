mod add;
mod bare_workspace;
mod boundary_sync;
mod checkout;
mod context;
mod daemon;
mod doctor;
mod git;
mod git_shim;
mod init;
mod list;
mod remove;
mod repo;
mod skill;
mod sync;
mod sync_status;
mod tree_modes;
mod tx;
mod working_copy;

use std::cell::Cell;
use std::env;
use std::ffi::OsStr;
use std::io::{self, Write as _};
use std::mem;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};

use bare_workspace::{
    DevspaceWorkspaceLoaderFactory, MultipleOperationHeads, ParsedRepositoryArgs,
    RepositorySelector, is_stock_bare_repository,
};
use clap::Subcommand as _;
use jj_cli::cli_util::{CliRunner, CommandHelper};
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;
use jj_lib::config::{ConfigLayer, ConfigMigrationRule, ConfigSource};
use jj_lib::op_heads_store::OpHeadsStoreError;

static REPOSITORY_SELECTOR: OnceLock<Arc<RepositorySelector>> = OnceLock::new();
static SHADOWED_ALIASES: OnceLock<Mutex<std::collections::BTreeSet<String>>> = OnceLock::new();
const APP_ABOUT: &str = "Cloudflare-native development workspaces backed by Jujutsu";
const JJ_HELP_HINT: &str =
    "ds embeds Jujutsu. Run `ds help jj` for the full Jujutsu command reference.";

pub fn run() -> ExitCode {
    if let Some(exit_code) = intercept_help(env::args_os().skip(1)) {
        return exit_code;
    }
    let repository_selector = REPOSITORY_SELECTOR
        .get_or_init(|| Arc::new(RepositorySelector::from_process_cwd()))
        .clone();
    let selector_for_args = repository_selector.clone();
    let exit_code = CliRunner::init()
        .name("ds")
        .about(APP_ABOUT)
        .version(env!("CARGO_PKG_VERSION"))
        .add_extra_config(devspace_default_config())
        .set_workspace_loader_factory(Box::new(DevspaceWorkspaceLoaderFactory::new(
            repository_selector,
        )))
        .add_extra_config_migration(devspace_alias_migration())
        .add_subcommand::<repo::DevspaceCommand, _>(repo::run)
        .add_global_args::<ParsedRepositoryArgs, _>(move |_ui, args| {
            selector_for_args.set_parsed_repository(args.repository);
            Ok(())
        })
        .add_dispatch_hook(restrict_bare_repository_commands)
        .run()
        .into();
    if exit_code == ExitCode::SUCCESS {
        boundary_sync::spawn_recorded();
    }
    exit_code
}

fn devspace_default_config() -> ConfigLayer {
    ConfigLayer::parse(
        ConfigSource::Default,
        r#"
            [devspace]
            git-shim = false
        "#,
    )
    .expect("built-in Devspace config is valid")
}

fn devspace_alias_migration() -> ConfigMigrationRule {
    let command_names = devspace_command_names();
    let names_to_match = command_names.clone();
    let migration_active = Rc::new(Cell::new(true));

    ConfigMigrationRule::custom(
        move |layer| {
            if !migration_active.get() {
                return false;
            }
            let matches = names_to_match
                .iter()
                .filter(|name| {
                    layer
                        .look_up_item(["aliases", name.as_str()])
                        .is_ok_and(|item| item.is_some())
                })
                .cloned()
                .collect::<Vec<_>>();
            if !matches.is_empty() {
                SHADOWED_ALIASES
                    .get_or_init(Default::default)
                    .lock()
                    .expect("shadowed alias set lock is not poisoned")
                    .extend(matches.iter().cloned());
            }
            if layer.source == ConfigSource::EnvOverrides {
                migration_active.set(false);
            }
            !matches.is_empty()
        },
        move |layer| {
            for name in &command_names {
                layer.delete_value(["aliases", name.as_str()])?;
            }
            Ok("aliases colliding with Devspace commands are ignored".to_owned())
        },
    )
}

fn devspace_command_names() -> Vec<String> {
    repo::DevspaceCommand::augment_subcommands(clap::Command::new("ds"))
        .get_subcommands()
        .map(|command| command.get_name().to_owned())
        .collect()
}

pub(crate) fn shadowed_aliases() -> Vec<String> {
    SHADOWED_ALIASES
        .get_or_init(Default::default)
        .lock()
        .expect("shadowed alias set lock is not poisoned")
        .iter()
        .cloned()
        .collect()
}

fn intercept_help(args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Option<ExitCode> {
    let args = args
        .into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect::<Vec<_>>();
    let mut app = match args.as_slice() {
        [arg] if arg == "--help" || arg == "-h" || arg == "help" => root_help_app(),
        [help, jj] if help == "help" && jj == "jj" => devspace_app(),
        _ => return None,
    };
    let help = app.render_long_help();
    let result = write!(io::stdout().lock(), "{help}");
    Some(if result.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn devspace_app() -> clap::Command {
    repo::DevspaceCommand::augment_subcommands(jj_cli::commands::default_app())
        .name("ds")
        .about(APP_ABOUT)
        .long_about(APP_ABOUT)
        .version(env!("CARGO_PKG_VERSION"))
}

fn root_help_app() -> clap::Command {
    let mut app = jj_cli::commands::default_app();
    for slot in app.get_subcommands_mut() {
        let command = mem::take(slot).hide(true);
        *slot = match command.get_name() {
            "git" => command
                .hide(false)
                .about("Manage Devspace's Git remote boundary"),
            "status" => command
                .hide(false)
                .about("Show Jujutsu status with Devspace synchronization state"),
            _ => command,
        };
    }
    repo::DevspaceCommand::augment_subcommands(app)
        .name("ds")
        .about(APP_ABOUT)
        .long_about(APP_ABOUT)
        .version(env!("CARGO_PKG_VERSION"))
        .after_long_help(JJ_HELP_HINT)
}

async fn restrict_bare_repository_commands(
    ui: &mut Ui,
    command: &CommandHelper,
    stock_dispatch: jj_cli::cli_util::BoxedAsyncCliDispatch<'_>,
) -> Result<(), CommandError> {
    crate::boundary_sync::configure_git_shim(command.settings())?;
    // Daemon and sync plumbing are workspace-less. `sync run --repository-name`
    // resolves its value through the machine catalog.
    if matches!(command.matches().subcommand_name(), Some("daemon" | "sync")) {
        return stock_dispatch.call(ui, command).await;
    }
    if command.matches().subcommand_name() == Some("git")
        && let Ok(loader) = command.workspace_loader()
        && crate::checkout::read_checkout_owner(loader.workspace_root()).is_ok()
    {
        return git::run_git(ui, command).await;
    }
    let repository_selector = REPOSITORY_SELECTOR
        .get()
        .expect("repository selector is initialized before the CLI runner");
    let Ok(loader) = command.workspace_loader() else {
        return stock_dispatch.call(ui, command).await;
    };
    if let Some(name) = repository_selector.selected_name()
        && !loader.workspace_root().join(".jj").is_dir()
    {
        let Some(entry) = repository_selector.catalog_entry().map_err(user_error)? else {
            return Err(user_error(format!(
                "Repository `{}` is not present in this machine store; run `ds add` to clone it first.",
                name.as_str()
            )));
        };
        if !entry.native_repository_path.exists() {
            return Err(user_error(format!(
                "Repository `{}` has an incomplete clone; run `ds add` again to finish it.",
                name.as_str()
            )));
        }
        if !is_stock_bare_repository(&entry.native_repository_path) {
            return Err(user_error(format!(
                "Repository `{}` is registered locally, but its native repository is invalid.",
                name.as_str()
            )));
        }
    }
    let is_bare_repository = loader.workspace_root() == loader.repo_path();
    if !is_bare_repository {
        stock_dispatch.call(ui, command).await?;
        return sync_status::write_status_line(ui, command).await;
    }
    let is_log = command.matches().subcommand_name() == Some("log");
    if command.global_args().repository.is_none() {
        if is_log {
            return Err(user_error(
                "Bare machine repositories must be selected with `ds -R <name> log`.",
            ));
        }
        // jj-cli has no safe pre-dispatch classification for commands that do not
        // need a workspace. Workspace-less commands are deliberately unavailable
        // from a bare-root cwd instead of trusting it as a config scope.
        return Err(user_error(
            "This command requires a checkout when run from a bare machine repository.",
        ));
    }
    if !is_log {
        return Err(user_error(
            "Repository-targeted mode currently supports only read-only `log`; this command requires a checkout.",
        ));
    }
    let workspace = command.load_workspace()?;
    if let Err(error) = workspace
        .repo_loader()
        .op_heads_store()
        .get_op_heads()
        .await
    {
        if let OpHeadsStoreError::Read(source) = &error
            && let Some(error) = source.downcast_ref::<MultipleOperationHeads>()
        {
            return Err(user_error(error.to_string()));
        }
        return Err(error.into());
    }
    stock_dispatch.call(ui, command).await
}
