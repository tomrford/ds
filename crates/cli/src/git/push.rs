use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;

use devspace_machine::{
    CatalogEntry, CommitMapping, ExportMappings, ExportMode, GitOid, GitProcessEnvironment,
    GitProjection, HttpTransport, ImportMappings, LeaseUpdate, MachineRepository, MachineStore,
    PackOptions, PendingProjectionBatch, ProjectionCursor, ProjectionError, ProjectionMapping,
    ProjectionObservation, ProjectionSnapshot, ProjectionState, ProjectionUpdate, PushErrorKind,
    PushRefStatus, QualifiedRef, RegisteredRemote, RemoteUrl, upload_object_closure,
};
use jj_cli::cli_util::CommandHelper;
use jj_cli::command_error::{CommandError, user_error};
use jj_cli::ui::Ui;
use jj_lib::backend::{BackendError, CommitId};
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::{RefTarget, RemoteRef, RemoteRefState};
use jj_lib::ref_name::{RefName, RemoteName, RemoteRefSymbol};
use jj_lib::repo::Repo as _;

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
    cursor: Option<&ProjectionCursor>,
) -> BookmarkDisposition {
    match (local_target, cursor) {
        (None, None) => BookmarkDisposition::Missing,
        (None, Some(_)) => BookmarkDisposition::Delete,
        (Some(target), Some(cursor)) if target.as_bytes() == cursor.canonical_commit_id => {
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
    let local_bookmarks = deleted.then(|| {
        view.local_bookmarks()
            .map(|(name, _)| name.as_str().to_owned())
            .collect::<BTreeSet<_>>()
    });
    let tracked_remote_bookmarks = deleted.then(|| {
        view.local_remote_bookmarks(RemoteName::new(remote_name.as_str()))
            .filter(|(_, targets)| targets.remote_ref.is_tracked())
            .map(|(name, _)| name.as_str().to_owned())
            .collect::<BTreeSet<_>>()
    });
    let runtime = cloud_runtime()?;
    let outcome = runtime
        .block_on(push_with_cloud(
            &locked.store,
            &locked.entry,
            &session.repository,
            &session.projection,
            session.transport,
            session.machine_id,
            &remote_name,
            &requested,
            local_bookmarks,
            tracked_remote_bookmarks,
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

#[allow(clippy::too_many_arguments)]
async fn push_with_cloud(
    store: &MachineStore,
    entry: &CatalogEntry,
    repository: &MachineRepository,
    projection: &GitProjection,
    mut cloud: HttpTransport,
    machine_id: [u8; 16],
    remote_name: &str,
    requested: &[RequestedBookmark],
    deleted_locals: Option<BTreeSet<String>>,
    tracked_remote_bookmarks: Option<BTreeSet<String>>,
    deleted: bool,
) -> Result<PushOutcome, String> {
    let remotes = cloud
        .list_remotes()
        .await
        .map_err(|error| error.to_string())?;
    let remote = find_remote(&remotes, remote_name)?;
    let mut snapshot = load_projection_snapshot(&cloud).await?;
    let mut requested = requested.to_vec();
    if let (Some(locals), Some(tracked)) = (&deleted_locals, &tracked_remote_bookmarks) {
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
    let requested_names = requested
        .iter()
        .map(|request| request.name.as_str())
        .collect::<BTreeSet<_>>();
    let mut completed_deletions = BTreeMap::new();

    while let Some(pending) = overlapping_pending(&snapshot, remote_name, &requested_names) {
        let recovered = pending_deletions(&pending, &requested, &snapshot);
        recover_pending_batch(
            repository, projection, &mut cloud, machine_id, remote, &snapshot, &pending,
        )
        .await?;
        completed_deletions.extend(recovered);
        snapshot = load_projection_snapshot(&cloud).await?;
    }

    loop {
        let mut prepared = prepare_updates(
            repository,
            projection,
            remote_name,
            &requested,
            &snapshot,
            &completed_deletions,
        )
        .await?;
        let mut output = prepared.output.clone();
        if prepared.updates.is_empty() {
            return Ok(finish_push(
                repository,
                remote_name,
                output,
                &prepared.view_updates,
                deleted,
            )
            .await);
        }
        let closure = repository
            .commit_closure(&prepared.durable_heads)
            .map_err(|error| error.to_string())?;
        upload_object_closure(
            &closure,
            store.repository_packs_path(&entry.identity),
            PackOptions::default(),
            &mut cloud,
        )
        .await
        .map_err(|error| error.to_string())?;

        let batch_id = new_batch_id()?;
        match cloud
            .begin_push(batch_id, machine_id, remote_name, &prepared.updates)
            .await
        {
            Ok(result) if result.pending => {
                let report = observed_push(
                    projection,
                    remote,
                    &prepared.leases,
                    Some(AFTER_PUSH_FAILPOINT),
                )?;
                let observations = observations(&prepared.updates, &report)?;
                let finalized = cloud
                    .recover_push(batch_id, machine_id, result.fence, &observations)
                    .await;
                match finalized {
                    Ok(result) if result.outcome.as_deref() == Some("accepted") => {
                        output.append(&mut prepared.success_output);
                        return Ok(finish_push(
                            repository,
                            remote_name,
                            output,
                            &prepared.view_updates,
                            deleted,
                        )
                        .await);
                    }
                    Ok(result) if result.outcome.as_deref() == Some("aborted") => {
                        return Err(format!(
                            "Git push did not update the remote; the journal aborted the batch.\n{}",
                            refusal_summary(&report)
                        ));
                    }
                    Ok(_) => return Err("projection journal left the push pending".to_owned()),
                    Err(_error) if remote_moved(&prepared.leases, &report) => {
                        return Err(
                            "remote ref moved outside devspace; fetch it before retrying the push"
                                .to_owned(),
                        );
                    }
                    Err(error) => return Err(error.to_string()),
                }
            }
            Ok(result) if result.outcome.as_deref() == Some("accepted") => {
                output.append(&mut prepared.success_output);
                return Ok(finish_push(
                    repository,
                    remote_name,
                    output,
                    &prepared.view_updates,
                    deleted,
                )
                .await);
            }
            Ok(_) => return Err("projection journal returned an invalid batch state".to_owned()),
            Err(begin_error) => {
                let refreshed = load_projection_snapshot(&cloud).await;
                let Ok(refreshed) = refreshed else {
                    return Err(begin_error.to_string());
                };
                let Some(pending) = overlapping_pending(&refreshed, remote_name, &requested_names)
                else {
                    return Err(begin_error.to_string());
                };
                let recovered = pending_deletions(&pending, &requested, &refreshed);
                recover_pending_batch(
                    repository, projection, &mut cloud, machine_id, remote, &refreshed, &pending,
                )
                .await?;
                completed_deletions.extend(recovered);
                snapshot = load_projection_snapshot(&cloud).await?;
            }
        }
    }
}

struct PreparedPush {
    updates: Vec<ProjectionUpdate>,
    leases: BTreeMap<QualifiedRef, LeaseUpdate>,
    durable_heads: Vec<CommitId>,
    output: Vec<String>,
    success_output: Vec<String>,
    view_updates: Vec<(String, Option<CommitId>)>,
}

async fn prepare_updates(
    repository: &MachineRepository,
    projection: &GitProjection,
    remote: &str,
    requested: &[RequestedBookmark],
    snapshot: &ProjectionSnapshot,
    completed_deletions: &BTreeMap<String, GitOid>,
) -> Result<PreparedPush, String> {
    let cursor_by_bookmark = snapshot
        .cursors
        .iter()
        .filter(|cursor| cursor.remote == remote)
        .map(|cursor| (cursor.bookmark.as_str(), cursor))
        .collect::<BTreeMap<_, _>>();
    let import_rows = snapshot.mappings.iter().map(|mapping| CommitMapping {
        canonical_id: CommitId::new(mapping.public_commit_id.to_vec()),
        git_id: CommitId::new(mapping.git_oid.to_vec()),
    });
    let base_import = ImportMappings::from_rows(import_rows).map_err(|error| error.to_string())?;
    let mut public_by_git = snapshot
        .mappings
        .iter()
        .map(|mapping| {
            (
                CommitId::new(mapping.git_oid.to_vec()),
                CommitId::new(mapping.public_commit_id.to_vec()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut updates = Vec::new();
    let mut leases = BTreeMap::new();
    let mut durable_heads = Vec::new();
    let mut output = Vec::new();
    let mut success_output = Vec::new();
    let mut view_updates = Vec::new();

    for request in requested {
        let cursor = cursor_by_bookmark.get(request.name.as_str()).copied();
        match classify_bookmark(request.local_target.as_ref(), cursor) {
            BookmarkDisposition::Missing => {
                let Some(old) = completed_deletions.get(&request.name).copied() else {
                    return Err(format!("no such bookmark `{}`", request.name));
                };
                output.push(format!(
                    "pushed {} to {remote}: deleted {}",
                    request.name,
                    short_oid(old)
                ));
                view_updates.push((request.name.clone(), None));
            }
            BookmarkDisposition::UpToDate => {
                let cursor = cursor.expect("up-to-date has cursor");
                output.push(format!(
                    "pushed {} to {remote}: up to date at {}",
                    request.name,
                    short_oid(GitOid(cursor.git_oid))
                ));
                view_updates.push((
                    request.name.clone(),
                    Some(CommitId::new(cursor.canonical_commit_id.to_vec())),
                ));
            }
            BookmarkDisposition::Delete => {
                let old = GitOid(cursor.expect("deletion has cursor").git_oid);
                updates.push(ProjectionUpdate {
                    bookmark: request.name.clone(),
                    expected_old_oid: Some(old.0),
                    states: Vec::new(),
                    proposed_state: None,
                });
                leases.insert(
                    QualifiedRef::from_bookmark(&request.name)
                        .map_err(|error| error.to_string())?,
                    LeaseUpdate {
                        expected_old_oid: Some(old),
                        new_oid: None,
                    },
                );
                success_output.push(format!(
                    "pushed {} to {remote}: deleted {}",
                    request.name,
                    short_oid(old)
                ));
                view_updates.push((request.name.clone(), None));
            }
            BookmarkDisposition::Update => {
                let canonical_head = request
                    .local_target
                    .as_ref()
                    .expect("update has a local target");
                let export_rows = snapshot.mappings.iter().map(export_mapping);
                let mut export_mappings =
                    ExportMappings::from_rows(export_rows).map_err(|error| error.to_string())?;
                let exported = projection
                    .export_reachable(
                        repository.repo().store(),
                        std::slice::from_ref(canonical_head),
                        &mut export_mappings,
                        ExportMode::Strict,
                    )
                    .await
                    .map_err(|error| {
                        if matches!(&error, ProjectionError::MissingMappedObject { .. }) {
                            format!(
                                "{error}. Run `ds git fetch --remote {remote} -b {}` to repair the mapped bookmark.",
                                request.name
                            )
                        } else {
                            error.to_string()
                        }
                    })?;
                let git_head = exported
                    .git_heads
                    .first()
                    .expect("one canonical head produces one Git head")
                    .clone();
                let hidden_set = projection
                    .hidden_set_for_commit(repository.repo().store(), canonical_head)
                    .await
                    .map_err(|error| error.to_string())?;
                let leaked = projection
                    .scan_hidden_paths(&git_head, &hidden_set)
                    .await
                    .map_err(|error| error.to_string())?;
                if !leaked.is_empty() {
                    return Err(format!(
                        "public Git head for bookmark `{}` contains hidden paths: {leaked:?}",
                        request.name
                    ));
                }

                let mut import_mappings = base_import.clone();
                // Accepted journal mappings are durable receipts: they bound
                // the import depth walk, keeping deep-history pushes
                // incremental.
                let imported = projection
                    .import_reachable_with_stops(
                        repository.repo().store(),
                        std::slice::from_ref(&git_head),
                        &base_import,
                        &mut import_mappings,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                for mapping in imported
                    .new_mappings
                    .iter()
                    .chain(&imported.reached_mappings)
                {
                    public_by_git.insert(mapping.git_id.clone(), mapping.canonical_id.clone());
                }

                let state_mappings = ensure_head_mapping(
                    exported.new_mappings,
                    canonical_head.clone(),
                    git_head.clone(),
                );
                let mut states = Vec::with_capacity(state_mappings.len());
                for mapping in state_mappings {
                    let public = public_by_git.get(&mapping.git_id).ok_or_else(|| {
                        format!("public shadow is missing for Git commit {}", mapping.git_id)
                    })?;
                    let state_hidden_set = projection
                        .hidden_set_for_commit(repository.repo().store(), &mapping.canonical_id)
                        .await
                        .map_err(|error| error.to_string())?;
                    states.push(ProjectionState {
                        git_oid: mapping
                            .git_id
                            .as_bytes()
                            .try_into()
                            .map_err(|_| "projected Git ID is not 20 bytes".to_owned())?,
                        canonical_commit_id: mapping
                            .canonical_id
                            .as_bytes()
                            .try_into()
                            .map_err(|_| "canonical commit ID is not 64 bytes".to_owned())?,
                        public_commit_id: public
                            .as_bytes()
                            .try_into()
                            .map_err(|_| "public commit ID is not 64 bytes".to_owned())?,
                        hidden_set_id: state_hidden_set.identity().to_projection_id(),
                    });
                }
                let new = GitOid(
                    git_head
                        .as_bytes()
                        .try_into()
                        .map_err(|_| "projected Git ID is not 20 bytes".to_owned())?,
                );
                let proposed_state = states
                    .iter()
                    .position(|state| state.git_oid == new.0)
                    .ok_or_else(|| {
                        "projection state set does not contain its Git head".to_owned()
                    })?;
                let old = cursor.map(|cursor| GitOid(cursor.git_oid));
                updates.push(ProjectionUpdate {
                    bookmark: request.name.clone(),
                    expected_old_oid: old.map(|oid| oid.0),
                    states,
                    proposed_state: Some(proposed_state),
                });
                leases.insert(
                    QualifiedRef::from_bookmark(&request.name)
                        .map_err(|error| error.to_string())?,
                    LeaseUpdate {
                        expected_old_oid: old,
                        new_oid: Some(new),
                    },
                );
                durable_heads.push(canonical_head.clone());
                durable_heads.extend(imported.canonical_heads);
                success_output.push(match old {
                    Some(old) => format!(
                        "pushed {} to {remote}: {} -> {}",
                        request.name,
                        short_oid(old),
                        short_oid(new)
                    ),
                    None => format!(
                        "pushed {} to {remote}: created {}",
                        request.name,
                        short_oid(new)
                    ),
                });
                view_updates.push((request.name.clone(), Some(canonical_head.clone())));
            }
        }
    }

    Ok(PreparedPush {
        updates,
        leases,
        durable_heads,
        output,
        success_output,
        view_updates,
    })
}

async fn finish_push(
    repository: &MachineRepository,
    remote: &str,
    lines: Vec<String>,
    pushed: &[(String, Option<CommitId>)],
    deleted: bool,
) -> PushOutcome {
    let warning = update_view_after_push(repository, remote, pushed, deleted)
        .await
        .err()
        .map(|error| view_repair_warning(remote, pushed, &error));
    PushOutcome { lines, warning }
}

async fn update_view_after_push(
    repository: &MachineRepository,
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
        let symbol = RemoteRefSymbol {
            name: RefName::new(bookmark),
            remote: RemoteName::new(remote),
        };
        transaction
            .repo_mut()
            .set_remote_bookmark(symbol, remote_ref.clone());
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

fn pending_deletions(
    pending: &PendingProjectionBatch,
    requested: &[RequestedBookmark],
    snapshot: &ProjectionSnapshot,
) -> BTreeMap<String, GitOid> {
    pending
        .refs
        .iter()
        .filter(|pending_ref| pending_ref.proposed_git_oid.is_none())
        .filter(|pending_ref| {
            requested.iter().any(|request| {
                request.name == pending_ref.bookmark && request.local_target.is_none()
            })
        })
        .filter_map(|pending_ref| {
            snapshot
                .cursors
                .iter()
                .find(|cursor| {
                    cursor.remote == pending.remote && cursor.bookmark == pending_ref.bookmark
                })
                .map(|cursor| (pending_ref.bookmark.clone(), GitOid(cursor.git_oid)))
        })
        .collect()
}

fn ensure_head_mapping(
    mut mappings: Vec<CommitMapping>,
    canonical_head: CommitId,
    git_head: CommitId,
) -> Vec<CommitMapping> {
    if !mappings
        .iter()
        .any(|mapping| mapping.canonical_id == canonical_head)
    {
        mappings.push(CommitMapping {
            canonical_id: canonical_head,
            git_id: git_head,
        });
    }
    mappings
}

fn export_mapping(mapping: &ProjectionMapping) -> CommitMapping {
    CommitMapping {
        canonical_id: CommitId::new(mapping.canonical_commit_id.to_vec()),
        git_id: CommitId::new(mapping.git_oid.to_vec()),
    }
}

pub(super) async fn load_projection_snapshot(
    journal: &HttpTransport,
) -> Result<ProjectionSnapshot, String> {
    let mut snapshot = journal
        .get(0, None)
        .await
        .map_err(|error| error.to_string())?;
    let through = snapshot.through;
    while snapshot.has_more {
        let page = journal
            .get(snapshot.next_after, Some(through))
            .await
            .map_err(|error| error.to_string())?;
        if page.through != through {
            return Err("projection snapshot high-water changed while paging".to_owned());
        }
        snapshot.mappings.extend(page.mappings);
        snapshot.next_after = page.next_after;
        snapshot.has_more = page.has_more;
    }
    Ok(snapshot)
}

pub(super) fn overlapping_pending(
    snapshot: &ProjectionSnapshot,
    remote: &str,
    bookmarks: &BTreeSet<&str>,
) -> Option<PendingProjectionBatch> {
    snapshot
        .pending
        .iter()
        .find(|batch| {
            batch.remote == remote
                && batch
                    .refs
                    .iter()
                    .any(|pending| bookmarks.contains(pending.bookmark.as_str()))
        })
        .cloned()
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn recover_pending_batch(
    repository: &MachineRepository,
    projection: &GitProjection,
    cloud: &mut HttpTransport,
    machine_id: [u8; 16],
    remote: &RegisteredRemote,
    snapshot: &ProjectionSnapshot,
    pending: &PendingProjectionBatch,
) -> Result<(), String> {
    let claim = cloud
        .claim_push(pending.batch_id, machine_id)
        .await
        .map_err(|error| error.to_string())?;
    let replay = cloud
        .get_push_replay(pending.batch_id)
        .await
        .map_err(|error| error.to_string())?;
    if replay.remote != pending.remote || replay.fence != claim.fence {
        return Err("projection replay does not match the claimed batch".to_owned());
    }
    validate_replay(&replay)?;

    match rebuild_replay_heads(repository, projection, snapshot, &replay).await {
        Err(devspace_machine::ProjectionError::Backend(BackendError::ObjectNotFound {
            ..
        })) => {
            download_all_packs(repository, cloud).await?;
            rebuild_replay_heads(repository, projection, snapshot, &replay)
                .await
                .map_err(|error| error.to_string())?;
        }
        result => result.map_err(|error| error.to_string())?,
    }

    let leases = replay_leases(&replay)?;
    let report = observed_push(projection, remote, &leases, Some(AFTER_PUSH_FAILPOINT))?;
    let observations = observations(&replay.updates, &report)?;
    match cloud
        .recover_push(pending.batch_id, machine_id, claim.fence, &observations)
        .await
    {
        Ok(result) if result.outcome.as_deref() == Some("accepted") => Ok(()),
        Ok(_) => Err("projection recovery did not accept the replayed batch".to_owned()),
        Err(_) if remote_moved(&leases, &report) => {
            Err("remote ref moved outside devspace; fetch it before retrying the push".to_owned())
        }
        Err(error) => Err(error.to_string()),
    }
}

fn validate_replay(replay: &devspace_machine::ProjectionReplay) -> Result<(), String> {
    if replay.updates.is_empty() {
        return Err("projection replay contains no refs".to_owned());
    }
    let mut bookmarks = BTreeSet::new();
    for update in &replay.updates {
        QualifiedRef::from_bookmark(&update.bookmark).map_err(|error| error.to_string())?;
        if !bookmarks.insert(update.bookmark.as_str()) {
            return Err("projection replay contains duplicate bookmarks".to_owned());
        }
        if update
            .proposed_state
            .is_some_and(|index| index >= update.states.len())
        {
            return Err(format!(
                "projection replay has an invalid proposed state for `{}`",
                update.bookmark
            ));
        }
    }
    Ok(())
}

async fn rebuild_replay_heads(
    repository: &MachineRepository,
    projection: &GitProjection,
    snapshot: &ProjectionSnapshot,
    replay: &devspace_machine::ProjectionReplay,
) -> Result<(), devspace_machine::ProjectionError> {
    for update in &replay.updates {
        let Some(proposed) = update.proposed_state else {
            continue;
        };
        let proposed = &update.states[proposed];
        let canonical_head = CommitId::new(proposed.canonical_commit_id.to_vec());
        let expected_git_head = CommitId::new(proposed.git_oid.to_vec());
        let rows = snapshot
            .mappings
            .iter()
            .map(export_mapping)
            .chain(update.states.iter().map(|state| CommitMapping {
                canonical_id: CommitId::new(state.canonical_commit_id.to_vec()),
                git_id: CommitId::new(state.git_oid.to_vec()),
            }));
        let mut mappings = ExportMappings::from_rows(rows)?;
        let exported = projection
            .export_reachable(
                repository.repo().store(),
                std::slice::from_ref(&canonical_head),
                &mut mappings,
                ExportMode::Replay,
            )
            .await?;
        if exported.git_heads.as_slice() != std::slice::from_ref(&expected_git_head) {
            return Err(devspace_machine::ProjectionError::ConflictingMapping {
                source_name: "canonical commit",
                source_id: canonical_head,
                existing: exported.git_heads[0].clone(),
                proposed: expected_git_head,
            });
        }
        let hidden_set = projection
            .hidden_set_for_commit(repository.repo().store(), &canonical_head)
            .await?;
        let leaked = projection
            .scan_hidden_paths(&exported.git_heads[0], &hidden_set)
            .await?;
        if !leaked.is_empty() {
            return Err(devspace_machine::ProjectionError::StaleMapping {
                canonical_id: canonical_head,
                git_id: exported.git_heads[0].clone(),
                leaked,
            });
        }
    }
    Ok(())
}

async fn download_all_packs(
    repository: &MachineRepository,
    cloud: &mut HttpTransport,
) -> Result<(), String> {
    use devspace_machine::SyncTransport as _;

    let mut after = 0;
    let mut through = None;
    loop {
        let page = cloud
            .list_packs(after, through)
            .await
            .map_err(|error| error.to_string())?;
        through = Some(page.through);
        for entry in page.packs {
            let pack = cloud
                .download_pack(entry.id)
                .await
                .map_err(|error| error.to_string())?;
            repository
                .install_pack(pack.id, &pack.manifest, &pack.chunks)
                .map_err(|error| error.to_string())?;
        }
        if !page.has_more {
            return Ok(());
        }
        after = page.next_after;
    }
}

fn replay_leases(
    replay: &devspace_machine::ProjectionReplay,
) -> Result<BTreeMap<QualifiedRef, LeaseUpdate>, String> {
    replay
        .updates
        .iter()
        .map(|update| {
            let qualified =
                QualifiedRef::from_bookmark(&update.bookmark).map_err(|error| error.to_string())?;
            let new_oid = update
                .proposed_state
                .map(|index| GitOid(update.states[index].git_oid));
            Ok((
                qualified,
                LeaseUpdate {
                    expected_old_oid: update.expected_old_oid.map(GitOid),
                    new_oid,
                },
            ))
        })
        .collect()
}

fn observed_push(
    projection: &GitProjection,
    remote: &RegisteredRemote,
    leases: &BTreeMap<QualifiedRef, LeaseUpdate>,
    failpoint: Option<&str>,
) -> Result<devspace_machine::PushReport, String> {
    let remote_url = RemoteUrl::new(remote.url.clone());
    match devspace_machine::push(
        projection.git_repo_path(),
        &remote_url,
        leases,
        &GitProcessEnvironment::default(),
    ) {
        Ok(report) => {
            if failpoint.is_some_and(failpoint_enabled) {
                std::process::exit(86);
            }
            Ok(report)
        }
        Err(error) if error.kind == PushErrorKind::PushFailed => Ok(error.report),
        Err(error) => Err(error.to_string()),
    }
}

fn observations(
    updates: &[ProjectionUpdate],
    report: &devspace_machine::PushReport,
) -> Result<Vec<ProjectionObservation>, String> {
    updates
        .iter()
        .map(|update| {
            let qualified =
                QualifiedRef::from_bookmark(&update.bookmark).map_err(|error| error.to_string())?;
            let observed = report
                .refs
                .get(&qualified)
                .ok_or_else(|| format!("Git did not report {}", qualified.as_str()))?;
            Ok(ProjectionObservation {
                bookmark: update.bookmark.clone(),
                live_oid: observed.observed_oid.map(|oid| oid.0),
            })
        })
        .collect()
}

/// Explains a remote-side refusal from the redacted push report; the journal
/// outcome alone would leave the user without the remote's stated reason.
fn refusal_summary(report: &devspace_machine::PushReport) -> String {
    let mut lines = report
        .refs
        .iter()
        .filter(|(_, entry)| {
            !matches!(
                entry.status,
                PushRefStatus::Updated | PushRefStatus::Deleted | PushRefStatus::UpToDate
            )
        })
        .map(|(qualified, entry)| format!("{}: {:?}", qualified.as_str(), entry.status))
        .collect::<Vec<_>>();
    lines.push(format!(
        "Git reported: {}",
        report.diagnostic.stderr_excerpt
    ));
    lines.join("\n")
}

fn remote_moved(
    leases: &BTreeMap<QualifiedRef, LeaseUpdate>,
    report: &devspace_machine::PushReport,
) -> bool {
    leases.iter().any(|(qualified, lease)| {
        let Some(observed) = report.refs.get(qualified) else {
            return false;
        };
        observed.status == PushRefStatus::LeaseRejected
            || (observed.observed_oid != lease.expected_old_oid
                && observed.observed_oid != lease.new_oid)
    })
}

pub(super) fn find_remote<'a>(
    remotes: &'a [RegisteredRemote],
    name: &str,
) -> Result<&'a RegisteredRemote, String> {
    remotes
        .iter()
        .find(|remote| remote.name == name)
        .ok_or_else(|| format!("remote-not-found: no such Git remote `{name}`"))
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
        CommitId::new(vec![byte; 64])
    }

    fn cursor(canonical: &CommitId) -> ProjectionCursor {
        ProjectionCursor {
            remote: "origin".to_owned(),
            bookmark: "main".to_owned(),
            git_oid: [3; 20],
            canonical_commit_id: canonical.as_bytes().try_into().unwrap(),
            public_commit_id: [4; 64],
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

    #[test]
    fn state_assembly_always_includes_the_proposed_head() {
        let parent = CommitMapping {
            canonical_id: commit(1),
            git_id: CommitId::new(vec![2; 20]),
        };
        let canonical_head = commit(3);
        let git_head = CommitId::new(vec![4; 20]);
        let mappings = ensure_head_mapping(
            vec![parent.clone()],
            canonical_head.clone(),
            git_head.clone(),
        );
        assert_eq!(mappings.len(), 2);
        assert_eq!(mappings[0], parent);
        assert_eq!(mappings[1].canonical_id, canonical_head);
        assert_eq!(mappings[1].git_id, git_head);
    }

    #[test]
    fn unknown_remote_uses_the_worker_error_code() {
        assert_eq!(
            find_remote(&[], "origin").unwrap_err(),
            "remote-not-found: no such Git remote `origin`"
        );
    }
}
