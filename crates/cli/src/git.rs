pub(crate) mod fetch;
mod projection_sidecar;
mod push;

use std::io::Write as _;
use std::path::Path;

use clap::parser::ValueSource;
use devspace_machine::{
    CatalogEntry, GitOid, GitProjection, HttpTransport, LowerHexError, MachineRepository,
    MachineStore, RegisteredRemote, RepositorySyncGuard, decode_lower_hex,
};
use jj_cli::cli_util::CommandHelper;
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;
use jj_lib::settings::UserSettings;

use crate::checkout::{read_checkout_owner, reject_unsupported_global_options};
use crate::sync::{LockedSyncRun, run_sync_entry_foreground_locked};

use self::projection_sidecar::open_or_create_projection;

const DEFAULT_REMOTE: &str = "origin";
const FAILPOINT_ENV: &str = "DEVSPACE_FAILPOINT";

pub(super) struct LockedCheckoutEntry {
    pub(super) store: MachineStore,
    pub(super) entry: CatalogEntry,
    _guard: RepositorySyncGuard,
}

pub(super) struct CloudSession {
    pub(super) repository: MachineRepository,
    pub(super) projection: GitProjection,
    pub(super) transport: HttpTransport,
    pub(super) machine_id: [u8; 16],
}

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
        Some(("fetch", fetch_matches)) => {
            reject_command_line_values(fetch_matches, &["branches", "remotes"], "git fetch")?;
            let bookmarks = raw_values(fetch_matches, "branches");
            let remotes = raw_values(fetch_matches, "remotes");
            if remotes.len() > 1 {
                return Err(user_error(
                    "`ds git fetch` accepts exactly one `--remote <name>` value.",
                ));
            }
            let remote = remotes
                .into_iter()
                .next()
                .unwrap_or_else(|| DEFAULT_REMOTE.to_owned());
            crate::boundary_sync::suppress();
            fetch::fetch_bookmarks(ui, command, bookmarks, remote).await
        }
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
        "{command} is unavailable in a Devspace checkout; Devspace owns the Git boundary. Use `ds git remote add`, `ds git remote list`, `ds git fetch`, or `ds git push -b <bookmark>`."
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
    register_remote(&config, &entry, name, url)?;
    writeln!(ui.status(), "Added Git remote `{name}`.")?;
    Ok(())
}

pub(crate) fn register_remote(
    config: &devspace_machine::MachineConfig,
    entry: &CatalogEntry,
    name: &str,
    url: &str,
) -> Result<(), CommandError> {
    let transport = HttpTransport::new(
        config,
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
    Ok(())
}

async fn remote_list(ui: &mut Ui, command: &CommandHelper) -> Result<(), CommandError> {
    reject_unsupported_global_options(command, "git remote list")?;
    let entry = checkout_entry(ui, command).await?;
    let store = MachineStore::platform_default().map_err(display_error)?;
    let config = store.load_config().map_err(display_error)?;
    let remotes = list_registered_remotes(&config, &entry)?;
    for remote in remotes {
        writeln!(ui.stdout(), "{} {}", remote.name, remote.url)?;
    }
    Ok(())
}

pub(crate) fn list_registered_remotes(
    config: &devspace_machine::MachineConfig,
    entry: &CatalogEntry,
) -> Result<Vec<RegisteredRemote>, CommandError> {
    let transport = HttpTransport::new(
        config,
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
    Ok(remotes)
}

async fn checkout_entry(ui: &Ui, command: &CommandHelper) -> Result<CatalogEntry, CommandError> {
    let workspace = command.workspace_helper_no_snapshot(ui).await?;
    resolve_checkout_entry(workspace.workspace_root()).map(|(_, entry)| entry)
}

fn resolve_checkout_entry(
    workspace_root: &Path,
) -> Result<(MachineStore, CatalogEntry), CommandError> {
    let owner = read_checkout_owner(workspace_root).map_err(|_| {
        user_error("`ds git` product commands are available only inside a Devspace checkout.")
    })?;
    let store = MachineStore::platform_default().map_err(display_error)?;
    let entry = store
        .entries()
        .map_err(display_error)?
        .into_iter()
        .find(|entry| {
            entry.identity.repository_id.as_str() == owner.repository_id()
                && entry.identity.incarnation.as_str() == owner.incarnation()
        })
        .ok_or_else(|| {
            user_error(
                "repository-not-registered: This checkout's repository is not registered on this machine.",
            )
        })?;
    Ok((store, entry))
}

pub(super) async fn locked_checkout_entry(
    ui: &mut Ui,
    workspace_root: &Path,
    settings: &UserSettings,
    operation: &str,
) -> Result<LockedCheckoutEntry, CommandError> {
    let (store, entry) = resolve_checkout_entry(workspace_root)?;
    let guard = match run_sync_entry_foreground_locked(ui, &store, &entry, settings).await {
        Ok(LockedSyncRun::Completed(guard)) => guard,
        Ok(LockedSyncRun::AlreadyLocked) => {
            return Err(user_error(format!(
                "Repository `{}` is already being synchronized; retry the {operation} after it finishes.",
                entry.name
            )));
        }
        Err(error) => return Err(user_error(error)),
    };
    Ok(LockedCheckoutEntry {
        store,
        entry,
        _guard: guard,
    })
}

pub(super) async fn open_cloud_session(
    ui: &mut Ui,
    settings: &UserSettings,
    store: &MachineStore,
    entry: &CatalogEntry,
) -> Result<CloudSession, CommandError> {
    let repository = MachineRepository::open(&entry.native_repository_path, settings)
        .await
        .map_err(display_error)?;
    let config = store.load_config().map_err(display_error)?;
    let incarnation = parse_hex(
        entry.identity.incarnation.as_str(),
        "repository incarnation",
    )?;
    let machine_id = parse_hex(config.machine_id().as_str(), "machine ID")?;
    let transport = HttpTransport::new(&config, entry.identity.repository_id.as_str(), incarnation)
        .map_err(display_error)?;
    let (projection, rebuilt_projection) =
        open_or_create_projection(&store.repository_projection_path(&entry.identity), settings)
            .map_err(user_error)?;
    if rebuilt_projection {
        writeln!(
            ui.warning_default(),
            "Rebuilt the local Git projection sidecar after it failed validation."
        )?;
    }
    Ok(CloudSession {
        repository,
        projection,
        transport,
        machine_id,
    })
}

pub(super) fn parse_hex<const N: usize>(value: &str, label: &str) -> Result<[u8; N], CommandError> {
    decode_lower_hex(value).map_err(|error| match error {
        LowerHexError::InvalidLength { .. } => user_error(format!("{label} has an invalid length")),
        LowerHexError::InvalidDigit => user_error(format!("{label} is not lowercase hexadecimal")),
    })
}

pub(super) fn cloud_runtime() -> Result<tokio::runtime::Runtime, CommandError> {
    tokio::runtime::Runtime::new()
        .map_err(|_| user_error("failed to start the cloud transport runtime"))
}

pub(super) fn display_error(error: impl std::fmt::Display) -> CommandError {
    user_error(error.to_string())
}

pub(super) fn short_oid(oid: GitOid) -> String {
    oid.to_string()[..12].to_owned()
}

pub(super) fn failpoint_enabled(name: &str) -> bool {
    std::env::var_os(FAILPOINT_ENV).as_deref() == Some(std::ffi::OsStr::new(name))
}
