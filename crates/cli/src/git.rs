mod projection_sidecar;
mod push;

use std::io::Write as _;

use clap::parser::ValueSource;
use devspace_machine::{CatalogEntry, MachineStore, ProjectionTransport};
use jj_cli::cli_util::CommandHelper;
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;

use crate::checkout::{read_checkout_owner, reject_unsupported_global_options};

const DEFAULT_REMOTE: &str = "origin";

pub(crate) async fn run_git(ui: &mut Ui, command: &CommandHelper) -> Result<(), CommandError> {
    let git_matches = command
        .matches()
        .subcommand_matches("git")
        .expect("Git dispatch has Git argument matches");
    match git_matches.subcommand() {
        Some(("remote", remote_matches)) => match remote_matches.subcommand() {
            Some(("add", add_matches)) => {
                reject_command_line_values(add_matches, &["remote", "url"], "git remote add")?;
                let name = required_raw(add_matches, "remote")?;
                let url = required_raw(add_matches, "url")?;
                remote_add(ui, command, &name, &url).await
            }
            Some(("list", list_matches)) => {
                reject_command_line_values(list_matches, &[], "git remote list")?;
                remote_list(ui, command).await
            }
            Some((name, _)) => Err(owned_boundary_error(&format!("remote {name}"))),
            None => Err(owned_boundary_error("remote")),
        },
        Some(("push", push_matches)) => {
            reject_command_line_values(push_matches, &["bookmark", "remote"], "git push")?;
            let bookmarks = raw_values(push_matches, "bookmark");
            if bookmarks.is_empty() {
                return Err(user_error(
                    "`ds git push` requires at least one `-b <bookmark>` argument.",
                ));
            }
            let remote =
                optional_raw(push_matches, "remote").unwrap_or_else(|| DEFAULT_REMOTE.to_owned());
            crate::boundary_sync::suppress();
            push::push_bookmarks(ui, command, bookmarks, remote).await
        }
        Some(("fetch", _)) => Err(user_error(
            "`ds git fetch` is not yet implemented; Devspace owns the Git boundary in native checkouts.",
        )),
        Some((name, _)) => Err(owned_boundary_error(name)),
        None => Err(owned_boundary_error("")),
    }
}

fn owned_boundary_error(subcommand: &str) -> CommandError {
    let command = if subcommand.is_empty() {
        "`ds git`".to_owned()
    } else {
        format!("`ds git {subcommand}`")
    };
    user_error(format!(
        "{command} is unavailable in a Devspace checkout; Devspace owns the Git boundary. Use `ds git remote add`, `ds git remote list`, or `ds git push -b <bookmark>`."
    ))
}

fn reject_command_line_values(
    matches: &clap::ArgMatches,
    allowed: &[&str],
    command: &str,
) -> Result<(), CommandError> {
    let unsupported = matches.ids().find(|id| {
        matches.value_source(id.as_str()) == Some(ValueSource::CommandLine)
            && !allowed.contains(&id.as_str())
            && !id.as_str().ends_with("Args")
            && !matches!(id.as_str(), "specific" | "what")
    });
    if let Some(id) = unsupported {
        return Err(user_error(format!(
            "`ds {command}` does not support `{}`.",
            id.as_str()
        )));
    }
    Ok(())
}

fn required_raw(matches: &clap::ArgMatches, id: &str) -> Result<String, CommandError> {
    optional_raw(matches, id).ok_or_else(|| user_error(format!("missing required `{id}` argument")))
}

fn optional_raw(matches: &clap::ArgMatches, id: &str) -> Option<String> {
    matches
        .get_raw(id)
        .and_then(|mut values| values.next())
        .and_then(|value| value.to_str())
        .map(str::to_owned)
}

fn raw_values(matches: &clap::ArgMatches, id: &str) -> Vec<String> {
    matches
        .get_raw(id)
        .into_iter()
        .flatten()
        .filter_map(|value| value.to_str().map(str::to_owned))
        .collect()
}

async fn remote_add(
    ui: &mut Ui,
    command: &CommandHelper,
    name: &str,
    url: &str,
) -> Result<(), CommandError> {
    reject_unsupported_global_options(command, "git remote add")?;
    let entry = checkout_entry(ui, command).await?;
    let store = MachineStore::platform_default().map_err(display_error)?;
    let config = store.load_config().map_err(display_error)?;
    let transport = ProjectionTransport::new(
        &config,
        entry.identity.repository_id.as_str(),
        parse_hex(
            entry.identity.incarnation.as_str(),
            "repository incarnation",
        )?,
    )
    .map_err(display_error)?;
    cloud_runtime()?
        .block_on(transport.set_remote(name, url))
        .map_err(display_error)?;
    writeln!(ui.status(), "Added Git remote `{name}`.")?;
    Ok(())
}

async fn remote_list(ui: &mut Ui, command: &CommandHelper) -> Result<(), CommandError> {
    reject_unsupported_global_options(command, "git remote list")?;
    let entry = checkout_entry(ui, command).await?;
    let store = MachineStore::platform_default().map_err(display_error)?;
    let config = store.load_config().map_err(display_error)?;
    let transport = ProjectionTransport::new(
        &config,
        entry.identity.repository_id.as_str(),
        parse_hex(
            entry.identity.incarnation.as_str(),
            "repository incarnation",
        )?,
    )
    .map_err(display_error)?;
    let remotes = cloud_runtime()?
        .block_on(transport.list_remotes())
        .map_err(display_error)?;
    for remote in remotes {
        writeln!(ui.stdout(), "{} {}", remote.name, remote.url)?;
    }
    Ok(())
}

async fn checkout_entry(ui: &Ui, command: &CommandHelper) -> Result<CatalogEntry, CommandError> {
    let workspace = command.workspace_helper_no_snapshot(ui).await?;
    let owner = read_checkout_owner(workspace.workspace_root()).map_err(|_| {
        user_error("`ds git` product commands are available only inside a Devspace checkout.")
    })?;
    let store = MachineStore::platform_default().map_err(display_error)?;
    store
        .entries()
        .map_err(display_error)?
        .into_iter()
        .find(|entry| {
            entry.identity.repository_id.as_str() == owner.repository_id()
                && entry.identity.incarnation.as_str() == owner.incarnation()
        })
        .ok_or_else(|| user_error("This checkout's repository is not registered on this machine."))
}

fn parse_hex<const N: usize>(value: &str, label: &str) -> Result<[u8; N], CommandError> {
    if value.len() != N * 2 {
        return Err(user_error(format!("{label} has an invalid length")));
    }
    let mut bytes = [0; N];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| user_error(format!("{label} is not lowercase hexadecimal")))?;
    }
    Ok(bytes)
}

fn cloud_runtime() -> Result<tokio::runtime::Runtime, CommandError> {
    tokio::runtime::Runtime::new()
        .map_err(|_| user_error("failed to start the cloud transport runtime"))
}

fn display_error(error: impl std::fmt::Display) -> CommandError {
    user_error(error.to_string())
}
