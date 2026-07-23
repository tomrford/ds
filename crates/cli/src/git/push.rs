use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;

use devspace_machine::{
    GitProcessEnvironment, MachineGitRepository, Oid, ProjectionGitCursor, ProjectionGitSnapshot,
    PushFailpoint, PushHead, QualifiedRef, push_with_journal,
};
use jj_cli::cli_util::CommandHelper;
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;
use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{RefTarget, RemoteRef, RemoteRefState};
use jj_lib::ref_name::{RefName, RemoteName, RemoteRefSymbol};

use crate::checkout::reject_unsupported_global_options;

use super::{
    cloud_runtime, display_error, failpoint_enabled, locked_checkout_entry, open_cloud_session,
    short_oid,
};

const AFTER_PUSH_FAILPOINT: &str = "after_git_push_before_finalize";

#[derive(Clone, Debug)]
struct RequestedBookmark {
    name: String,
    local_target: Option<CommitId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BookmarkDisposition {
    Update,
    Delete,
    UpToDate,
    Missing,
}

fn classify_bookmark(
    local_target: Option<&CommitId>,
    cursor: Option<&ProjectionGitCursor>,
) -> BookmarkDisposition {
    match (local_target, cursor) {
        (None, None) => BookmarkDisposition::Missing,
        (None, Some(_)) => BookmarkDisposition::Delete,
        (Some(target), Some(cursor)) if target.as_bytes() == cursor.canonical_oid.0 => {
            BookmarkDisposition::UpToDate
        }
        (Some(_), _) => BookmarkDisposition::Update,
    }
}

pub(super) async fn push_bookmarks(
    ui: &mut Ui,
    command: &CommandHelper,
    bookmark_names: Vec<String>,
    deleted: bool,
    remote_name: String,
) -> Result<(), CommandError> {
    reject_unsupported_global_options(command, "git push")?;
    let mut seen = BTreeSet::new();
    for bookmark in &bookmark_names {
        if !seen.insert(bookmark.clone()) {
            return Err(user_error(format!(
                "Bookmark `{bookmark}` was requested more than once."
            )));
        }
        QualifiedRef::from_bookmark(bookmark).map_err(display_error)?;
    }

    let workspace_root = command
        .workspace_helper(ui)
        .await?
        .workspace_root()
        .to_owned();
    let locked = locked_checkout_entry(ui, &workspace_root, command.settings(), "push").await?;
    let session = open_cloud_session(ui, command.settings(), &locked.store, &locked.entry).await?;
    let snapshot = cloud_runtime()?
        .block_on(session.transport.projection_snapshot_all())
        .map_err(display_error)?;
    let remote_exists = cloud_runtime()?
        .block_on(session.transport.list_remotes())
        .map_err(display_error)?
        .iter()
        .any(|remote| remote.name == remote_name);
    if !remote_exists {
        return Err(user_error(format!(
            "remote-not-found: no such Git remote `{remote_name}`"
        )));
    }

    let view = session.repository.repo().view();
    let mut requested = Vec::with_capacity(bookmark_names.len());
    for name in bookmark_names {
        let target = view.get_local_bookmark(RefName::new(name.as_str()));
        if target.has_conflict() {
            return Err(user_error(format!(
                "Bookmark `{name}` is conflicted and must resolve to exactly one commit before push."
            )));
        }
        let remote_ref = view.get_remote_bookmark(RemoteRefSymbol {
            name: RefName::new(name.as_str()),
            remote: RemoteName::new(remote_name.as_str()),
        });
        if remote_ref.is_present() && !remote_ref.is_tracked() {
            return Err(user_error(format!(
                "Non-tracking remote bookmark {}@{} exists. Run `ds bookmark track {} --remote={}` to import the remote bookmark.",
                name, remote_name, name, remote_name
            )));
        }
        requested.push(RequestedBookmark {
            name,
            local_target: target.as_normal().cloned(),
        });
    }

    if deleted {
        let locals = view
            .local_bookmarks()
            .map(|(name, _)| name.as_str().to_owned())
            .collect::<BTreeSet<_>>();
        let tracked = view
            .local_remote_bookmarks(RemoteName::new(remote_name.as_str()))
            .filter(|(_, targets)| targets.remote_ref.is_tracked())
            .map(|(name, _)| name.as_str().to_owned())
            .collect::<BTreeSet<_>>();
        let mut names = requested
            .iter()
            .map(|request| request.name.clone())
            .collect::<BTreeSet<_>>();
        for cursor in snapshot
            .cursors
            .iter()
            .filter(|cursor| cursor.remote == remote_name)
        {
            if !locals.contains(&cursor.bookmark)
                && tracked.contains(&cursor.bookmark)
                && names.insert(cursor.bookmark.clone())
            {
                requested.push(RequestedBookmark {
                    name: cursor.bookmark.clone(),
                    local_target: None,
                });
            }
        }
    }

    let outcome = cloud_runtime()?
        .block_on(push_requested(
            &session.repository,
            &session.transport,
            &remote_name,
            &requested,
            &snapshot,
            deleted,
        ))
        .map_err(user_error)?;
    for line in outcome.lines {
        writeln!(ui.status(), "{line}")?;
    }
    if let Some(warning) = outcome.warning {
        writeln!(ui.warning_default(), "{warning}")?;
    }
    Ok(())
}

struct PushOutcome {
    lines: Vec<String>,
    warning: Option<String>,
}

async fn push_requested(
    repository: &MachineGitRepository,
    transport: &devspace_machine::GitHttpTransport,
    remote: &str,
    requested: &[RequestedBookmark],
    snapshot: &ProjectionGitSnapshot,
    deleted: bool,
) -> Result<PushOutcome, String> {
    let cursors = snapshot
        .cursors
        .iter()
        .filter(|cursor| cursor.remote == remote)
        .map(|cursor| (cursor.bookmark.as_str(), cursor))
        .collect::<BTreeMap<_, _>>();
    let mut heads = Vec::with_capacity(requested.len());
    for request in requested {
        if classify_bookmark(
            request.local_target.as_ref(),
            cursors.get(request.name.as_str()).copied(),
        ) == BookmarkDisposition::Missing
        {
            return Err(format!("no such bookmark `{}`", request.name));
        }
        heads.push(PushHead {
            bookmark: request.name.clone(),
            canonical_oid: request.local_target.as_ref().map(commit_oid).transpose()?,
        });
    }
    if heads.is_empty() {
        return Ok(PushOutcome {
            lines: Vec::new(),
            warning: None,
        });
    }

    let result = match push_with_journal(
        repository,
        transport,
        remote,
        &heads,
        new_batch_id()?,
        &GitProcessEnvironment::default(),
        if failpoint_enabled(AFTER_PUSH_FAILPOINT) {
            PushFailpoint::AfterGitPush
        } else {
            PushFailpoint::None
        },
    )
    .await
    {
        Err(devspace_machine::JournalFlowError::AfterPushFailpoint { .. }) => {
            std::process::exit(86);
        }
        Err(error) => return Err(error.to_string()),
        Ok(result) => result,
    };

    let mut lines = Vec::with_capacity(requested.len());
    let mut pushed = Vec::with_capacity(requested.len());
    for request in requested {
        let cursor = cursors.get(request.name.as_str()).copied();
        let public = result.public_heads.get(&request.name).copied().flatten();
        match classify_bookmark(request.local_target.as_ref(), cursor) {
            BookmarkDisposition::Missing => unreachable!("checked before journal flow"),
            BookmarkDisposition::UpToDate => {
                let cursor = cursor.expect("up-to-date bookmark has a cursor");
                lines.push(format!(
                    "pushed {} to {remote}: up to date at {}",
                    request.name,
                    short_oid(cursor.public_oid)
                ));
            }
            BookmarkDisposition::Delete => {
                let cursor = cursor.expect("deleted bookmark has a cursor");
                lines.push(format!(
                    "pushed {} to {remote}: deleted {}",
                    request.name,
                    short_oid(cursor.public_oid)
                ));
            }
            BookmarkDisposition::Update => {
                let new = public.ok_or_else(|| {
                    format!(
                        "projection journal omitted the public head for `{}`",
                        request.name
                    )
                })?;
                lines.push(match cursor {
                    Some(cursor) => format!(
                        "pushed {} to {remote}: {} -> {}",
                        request.name,
                        short_oid(cursor.public_oid),
                        short_oid(new)
                    ),
                    None => format!(
                        "pushed {} to {remote}: created {}",
                        request.name,
                        short_oid(new)
                    ),
                });
            }
        }
        pushed.push((request.name.clone(), request.local_target.clone()));
    }

    let warning = update_view_after_push(repository, remote, &pushed, deleted)
        .await
        .err()
        .map(|error| view_repair_warning(remote, &pushed, &error));
    Ok(PushOutcome { lines, warning })
}

async fn update_view_after_push(
    repository: &MachineGitRepository,
    remote: &str,
    pushed: &[(String, Option<CommitId>)],
    deleted: bool,
) -> Result<(), String> {
    let mut updates = Vec::new();
    for (bookmark, target) in pushed {
        let symbol = RemoteRefSymbol {
            name: RefName::new(bookmark),
            remote: RemoteName::new(remote),
        };
        let new_ref = match target {
            Some(id) => RemoteRef {
                target: RefTarget::normal(id.clone()),
                state: RemoteRefState::Tracked,
            },
            None => RemoteRef::absent(),
        };
        if repository.repo().view().get_remote_bookmark(symbol) != &new_ref {
            updates.push((bookmark.clone(), new_ref));
        }
    }
    if updates.is_empty() {
        return Ok(());
    }

    let mut transaction = repository.repo().start_transaction();
    for (bookmark, remote_ref) in &updates {
        transaction.repo_mut().set_remote_bookmark(
            RemoteRefSymbol {
                name: RefName::new(bookmark),
                remote: RemoteName::new(remote),
            },
            remote_ref.clone(),
        );
    }
    let mut names = updates
        .iter()
        .map(|(bookmark, _)| bookmark.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    let description = match (deleted, names.as_slice()) {
        (true, _) => format!("push all deleted bookmarks to git remote {remote}"),
        (false, [name]) => format!("push bookmark {name} to git remote {remote}"),
        (false, names) => format!("push bookmarks {} to git remote {remote}", names.join(", ")),
    };
    transaction
        .commit(description)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn view_repair_warning(remote: &str, pushed: &[(String, Option<CommitId>)], error: &str) -> String {
    let updated = pushed
        .iter()
        .filter(|(_, target)| target.is_some())
        .map(|(bookmark, _)| format!(" -b {bookmark}"))
        .collect::<String>();
    let deleted = pushed
        .iter()
        .filter(|(_, target)| target.is_none())
        .map(|(bookmark, _)| format!(" {bookmark}"))
        .collect::<String>();
    let mut repairs = Vec::new();
    if !updated.is_empty() {
        repairs.push(format!(
            "Run `ds git fetch --remote {remote}{updated}` to repair the updated remote bookmarks."
        ));
    }
    if !deleted.is_empty() {
        repairs.push(format!(
            "Run `ds bookmark forget{deleted} --include-remotes` to remove the deleted remote bookmarks from the view."
        ));
    }
    format!(
        "The push updated {remote}, but recording it in the local view failed: {error}. {}",
        repairs.join(" ")
    )
}

fn commit_oid(id: &CommitId) -> Result<Oid, String> {
    Oid::from_bytes(id.as_bytes())
        .ok_or_else(|| format!("Git backend commit {id} is not a 20-byte object ID"))
}

fn new_batch_id() -> Result<[u8; 16], String> {
    let mut id = [0; 16];
    getrandom::fill(&mut id).map_err(|_| "failed to generate a projection batch ID".to_owned())?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(byte: u8) -> CommitId {
        CommitId::new(vec![byte; 20])
    }

    fn cursor(canonical: &CommitId) -> ProjectionGitCursor {
        ProjectionGitCursor {
            remote: "origin".to_owned(),
            bookmark: "main".to_owned(),
            canonical_oid: commit_oid(canonical).unwrap(),
            public_oid: Oid([4; 20]),
            hidden_set_id: None,
            activation_sequence: 1,
        }
    }

    #[test]
    fn classifies_local_updates_deletions_and_missing_bookmarks() {
        let target = commit(1);
        let other = commit(2);
        let cursor = cursor(&target);
        assert_eq!(
            classify_bookmark(Some(&target), Some(&cursor)),
            BookmarkDisposition::UpToDate
        );
        assert_eq!(
            classify_bookmark(Some(&other), Some(&cursor)),
            BookmarkDisposition::Update
        );
        assert_eq!(
            classify_bookmark(Some(&target), None),
            BookmarkDisposition::Update
        );
        assert_eq!(
            classify_bookmark(None, Some(&cursor)),
            BookmarkDisposition::Delete
        );
        assert_eq!(classify_bookmark(None, None), BookmarkDisposition::Missing);
    }
}
