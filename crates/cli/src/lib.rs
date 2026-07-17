mod add;
mod bare_workspace;
mod checkout;
mod remove;
mod repo;
mod sync;

use std::process::ExitCode;
use std::sync::{Arc, OnceLock};

use bare_workspace::{
    DevspaceWorkspaceLoaderFactory, MultipleOperationHeads, ParsedRepositoryArgs,
    RepositorySelector, is_stock_bare_repository,
};
use jj_cli::cli_util::{CliRunner, CommandHelper};
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;
use jj_lib::op_heads_store::OpHeadsStoreError;

static REPOSITORY_SELECTOR: OnceLock<Arc<RepositorySelector>> = OnceLock::new();

pub fn run() -> ExitCode {
    let repository_selector = REPOSITORY_SELECTOR
        .get_or_init(|| Arc::new(RepositorySelector::from_process_cwd()))
        .clone();
    let selector_for_args = repository_selector.clone();
    CliRunner::init()
        .name("ds")
        .version(env!("CARGO_PKG_VERSION"))
        .set_workspace_loader_factory(Box::new(DevspaceWorkspaceLoaderFactory::new(
            repository_selector,
        )))
        .add_subcommand::<repo::DevspaceCommand, _>(repo::run)
        .add_global_args::<ParsedRepositoryArgs, _>(move |_ui, args| {
            selector_for_args.set_parsed_repository(args.repository);
            Ok(())
        })
        .add_dispatch_hook(restrict_bare_repository_commands)
        .run()
        .into()
}

async fn restrict_bare_repository_commands(
    ui: &mut Ui,
    command: &CommandHelper,
    stock_dispatch: jj_cli::cli_util::BoxedAsyncCliDispatch<'_>,
) -> Result<(), CommandError> {
    // `sync run --repository` deliberately shares jj's global option spelling,
    // but resolves the value itself through the machine catalog.
    if command.matches().subcommand_name() == Some("sync") {
        return stock_dispatch.call(ui, command).await;
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
        return stock_dispatch.call(ui, command).await;
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
