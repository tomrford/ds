use std::io::Write as _;

use jj_cli::cli_util::{CommandHelper, short_change_hash};
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;
use jj_lib::repo::Repo as _;

use crate::checkout::{read_checkout_owner, reject_unsupported_global_options};

pub(crate) async fn list_workspaces(
    ui: &mut Ui,
    command: &CommandHelper,
) -> Result<(), CommandError> {
    reject_unsupported_global_options(command, "list")?;
    crate::boundary_sync::suppress();
    let workspace = command.workspace_helper_no_snapshot(ui).await?;
    read_checkout_owner(workspace.workspace().workspace_root())
        .map_err(|_| user_error("`ds list` is available only inside a Devspace checkout"))?;
    let current = workspace.workspace().workspace_name();
    for (name, commit_id) in workspace.repo().view().wc_commit_ids() {
        let commit = workspace.repo().store().get_commit_async(commit_id).await?;
        let description = commit.description().lines().next().unwrap_or_default();
        writeln!(
            ui.stdout(),
            "{} {} {} {}",
            if name == current { "*" } else { " " },
            name.as_symbol(),
            short_change_hash(commit.change_id()),
            description
        )?;
    }
    Ok(())
}
