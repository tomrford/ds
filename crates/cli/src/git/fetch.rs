use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;

use devspace_machine::{
    FetchFlowResult, GitProcessEnvironment, MachineGitRepository, Oid, ProjectionGitCursor,
    QualifiedRef, RemoteUrl, fetch_with_journal, ls_remote_heads,
};
use jj_cli::cli_util::CommandHelper;
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;
use jj_lib::backend::CommitId;
use jj_lib::op_store::{RefTarget, RemoteRef, RemoteRefState};
use jj_lib::ref_name::{RefName, RemoteName, RemoteRefSymbol};
use jj_lib::repo::Repo as _;
use jj_lib::settings::UserSettings;

use devspace_machine::{CatalogEntry, MachineStore};

use crate::checkout::reject_unsupported_global_options;

use super::{
    cloud_runtime, failpoint_enabled, locked_checkout_entry, open_cloud_session, short_oid,
};

const AFTER_FETCH_RECORD_FAILPOINT: &str = "after_fetch_record_before_view";

pub(super) async fn fetch_bookmarks(
    ui: &mut Ui,
    command: &CommandHelper,
    bookmark_names: Vec<String>,
    remote_name: String,
) -> Result<(), CommandError> {
    reject_unsupported_global_options(command, "git fetch")?;
    validate_requested_bookmarks(&bookmark_names)?;

    let workspace = command.workspace_helper(ui).await?;
    let workspace_root = workspace.workspace_root().to_owned();
    drop(workspace);
    let locked = locked_checkout_entry(ui, &workspace_root, command.settings(), "fetch").await?;
    fetch_entry(
        ui,
        command.settings(),
        &locked.store,
        &locked.entry,
        bookmark_names,
        remote_name,
    )
    .await?;
    Ok(())
}

pub(crate) async fn fetch_entry(
    ui: &mut Ui,
    settings: &UserSettings,
    store: &MachineStore,
    entry: &CatalogEntry,
    mut bookmark_names: Vec<String>,
    remote_name: String,
) -> Result<Vec<String>, CommandError> {
    let session = open_cloud_session(ui, settings, store, entry).await?;
    let runtime = cloud_runtime()?;
    let remotes = runtime
        .block_on(session.transport.list_remotes())
        .map_err(super::display_error)?;
    let remote = remotes
        .iter()
        .find(|remote| remote.name == remote_name)
        .ok_or_else(|| {
            user_error(format!(
                "remote-not-found: no such Git remote `{remote_name}`"
            ))
        })?;
    if bookmark_names.is_empty() {
        bookmark_names = ls_remote_heads(
            &RemoteUrl::new(remote.url.clone()),
            &GitProcessEnvironment::default(),
        )
        .map_err(super::display_error)?
        .into_keys()
        .collect();
    }
    validate_requested_bookmarks(&bookmark_names)?;
    if bookmark_names.is_empty() {
        return Ok(Vec::new());
    }

    let before = runtime
        .block_on(session.transport.projection_snapshot_all())
        .map_err(super::display_error)?;
    let outcome = runtime
        .block_on(fetch_with_journal(
            &session.repository,
            &session.transport,
            &remote_name,
            &bookmark_names,
            new_fetch_id().map_err(user_error)?,
            &GitProcessEnvironment::default(),
        ))
        .map_err(|error| user_error(error.to_string()))?;
    if failpoint_enabled(AFTER_FETCH_RECORD_FAILPOINT) {
        std::process::exit(86);
    }

    let mut lines = fetch_lines(&before.cursors, &remote_name, &bookmark_names, &outcome)?;
    lines.extend(
        update_view_from_journal(
            &session.repository,
            &remote_name,
            &bookmark_names,
            &outcome.canonical_heads,
        )
        .await
        .map_err(user_error)?,
    );
    for line in &lines {
        writeln!(ui.status(), "{line}")?;
    }
    Ok(lines)
}

fn fetch_lines(
    cursors: &[ProjectionGitCursor],
    remote: &str,
    bookmarks: &[String],
    outcome: &FetchFlowResult,
) -> Result<Vec<String>, CommandError> {
    let cursors = cursors
        .iter()
        .filter(|cursor| cursor.remote == remote)
        .map(|cursor| (cursor.bookmark.as_str(), cursor))
        .collect::<BTreeMap<_, _>>();
    bookmarks
        .iter()
        .map(|bookmark| {
            let public = outcome.public_heads.get(bookmark).copied().ok_or_else(|| {
                user_error(format!("Git did not report fetched bookmark `{bookmark}`"))
            })?;
            Ok(match cursors.get(bookmark.as_str()).copied() {
                Some(cursor) if cursor.public_oid == public => {
                    format!("up to date {bookmark} from {remote}")
                }
                Some(cursor) => format!(
                    "fetched {bookmark} from {remote}: {} -> {}",
                    short_oid(cursor.public_oid),
                    short_oid(public)
                ),
                None => format!("new bookmark {bookmark} from {remote}"),
            })
        })
        .collect()
}

async fn update_view_from_journal(
    repository: &MachineGitRepository,
    remote: &str,
    bookmarks: &[String],
    canonical_heads: &BTreeMap<String, Oid>,
) -> Result<Vec<String>, String> {
    let mut updates = Vec::new();
    for bookmark in bookmarks {
        let canonical = canonical_heads.get(bookmark).copied().ok_or_else(|| {
            format!("projection journal has no cursor for {remote}/{bookmark} after fetch")
        })?;
        let target = RefTarget::normal(CommitId::new(canonical.0.to_vec()));
        let symbol = RemoteRefSymbol {
            name: RefName::new(bookmark),
            remote: RemoteName::new(remote),
        };
        let old_remote = repository.repo().view().get_remote_bookmark(symbol).clone();
        if old_remote.target != target {
            updates.push((bookmark.clone(), old_remote, target));
        }
    }
    if updates.is_empty() {
        return Ok(Vec::new());
    }

    let mut transaction = repository.repo().start_transaction();
    let mut commits = Vec::with_capacity(updates.len());
    for (_, _, target) in &updates {
        commits.push(
            repository
                .repo()
                .store()
                .get_commit_async(
                    target
                        .as_normal()
                        .expect("journal cursor selects one canonical commit"),
                )
                .await
                .map_err(|error| error.to_string())?,
        );
    }
    transaction
        .repo_mut()
        .index_commits(&commits)
        .await
        .map_err(|error| error.to_string())?;
    transaction
        .repo_mut()
        .add_heads(&commits)
        .await
        .map_err(|error| error.to_string())?;

    let mut lines = Vec::new();
    for (bookmark, old_remote, target) in updates {
        let name = RefName::new(&bookmark);
        if old_remote.is_tracked() {
            transaction
                .repo_mut()
                .merge_local_bookmark(name, &old_remote.target, &target)
                .map_err(|error| error.to_string())?;
            lines.push(format!("bookmark: {bookmark}@{remote} [updated] tracked"));
        }
        transaction.repo_mut().set_remote_bookmark(
            RemoteRefSymbol {
                name,
                remote: RemoteName::new(remote),
            },
            RemoteRef {
                target,
                state: if old_remote.is_present() {
                    old_remote.state
                } else {
                    RemoteRefState::New
                },
            },
        );
    }
    transaction
        .commit(format!("fetch from {remote}"))
        .await
        .map_err(|error| error.to_string())?;
    Ok(lines)
}

fn validate_requested_bookmarks(bookmarks: &[String]) -> Result<(), CommandError> {
    let mut seen = BTreeSet::new();
    for bookmark in bookmarks {
        QualifiedRef::from_bookmark(bookmark).map_err(super::display_error)?;
        if !seen.insert(bookmark.as_str()) {
            return Err(user_error(format!(
                "Bookmark `{bookmark}` was requested more than once."
            )));
        }
    }
    Ok(())
}

fn new_fetch_id() -> Result<[u8; 16], String> {
    let mut id = [0; 16];
    getrandom::fill(&mut id).map_err(|_| "failed to generate a projection fetch ID".to_owned())?;
    Ok(id)
}
