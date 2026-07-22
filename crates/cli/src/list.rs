use std::io::Write as _;

use jj_cli::cli_util::{CommandHelper, short_change_hash};
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;

use crate::checkout::{read_checkout_owner, reject_unsupported_global_options};

#[derive(clap::Args)]
pub(crate) struct ListArgs {
    /// Print workspace state as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(serde::Serialize)]
struct WorkspaceListEntry {
    current: bool,
    workspace_id: String,
    change_id: String,
    commit_id: String,
    description: String,
}

pub(crate) async fn list_workspaces(
    ui: &mut Ui,
    command: &CommandHelper,
    args: ListArgs,
) -> Result<(), CommandError> {
    reject_unsupported_global_options(command, "list")?;
    crate::boundary_sync::suppress();
    let workspace = command.workspace_helper_no_snapshot(ui).await?;
    read_checkout_owner(workspace.workspace().workspace_root())
        .map_err(|_| user_error("`ds list` is available only inside a Devspace checkout"))?;
    let current = workspace.workspace().workspace_name();
    let mut entries = Vec::new();
    for (name, commit_id) in workspace.repo().view().wc_commit_ids() {
        let commit = workspace.repo().store().get_commit_async(commit_id).await?;
        let description = commit.description().lines().next().unwrap_or_default();
        if args.json {
            entries.push(WorkspaceListEntry {
                current: name == current,
                workspace_id: name.as_str().to_owned(),
                change_id: commit.change_id().reverse_hex(),
                commit_id: commit.id().hex(),
                description: description.to_owned(),
            });
        } else {
            writeln!(
                ui.stdout(),
                "{} {} {} {}",
                if name == current { "*" } else { " " },
                name.as_symbol(),
                short_change_hash(commit.change_id()),
                description
            )?;
        }
    }
    if args.json {
        serde_json::to_writer(ui.stdout(), &entries)
            .map_err(|error| user_error(format!("failed to encode workspace list: {error}")))?;
        writeln!(ui.stdout())?;
    }
    Ok(())
}
