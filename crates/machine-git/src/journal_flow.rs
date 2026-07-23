//! Library-level push, recovery, fetch, and canonical-parent graft proofs.

use std::collections::{BTreeMap, BTreeSet};

use devspace_kernel_git::{ObjectKind, Oid, parse_commit};
use gix::objs::{Kind as GitObjectKind, Write as _};
use thiserror::Error;

use crate::{
    CommitMapping, GitHttpTransport, GitHttpTransportError, GitProcessEnvironment, LeaseUpdate,
    MachineGitRepository, ObjectClosureError, PackBuildError, PackInstallError, PackOptions,
    ProjectionError, ProjectionGitBatchResult, ProjectionGitFetchRef, ProjectionGitFetchResult,
    ProjectionGitObservation, ProjectionGitSnapshot, ProjectionGitState, ProjectionGitUpdate,
    ProjectionMappings, PushError, PushErrorKind, QualifiedRef, RemoteUrl, build_packs, fetch, hex,
    push,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PushHead {
    pub bookmark: String,
    pub canonical_oid: Option<Oid>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PushFailpoint {
    #[default]
    None,
    AfterGitPush,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PushFlowResult {
    pub batch_id: Option<[u8; 16]>,
    pub outcome: String,
    pub recovered_batches: Vec<[u8; 16]>,
    pub public_heads: BTreeMap<String, Option<Oid>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalParentGraft {
    pub public_commit: Oid,
    pub canonical_commit: Oid,
    pub public_parents: Vec<Oid>,
    pub canonical_parents: Vec<Oid>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FetchFlowResult {
    pub receipt: ProjectionGitFetchResult,
    pub public_heads: BTreeMap<String, Oid>,
    pub canonical_heads: BTreeMap<String, Oid>,
    pub grafts: Vec<CanonicalParentGraft>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalGraftResult {
    pub canonical_head: Oid,
    pub states: Vec<ProjectionGitState>,
    pub grafts: Vec<CanonicalParentGraft>,
}

pub async fn push_with_journal(
    repository: &MachineGitRepository,
    transport: &GitHttpTransport,
    remote: &str,
    heads: &[PushHead],
    batch_id: [u8; 16],
    environment: &GitProcessEnvironment,
    failpoint: PushFailpoint,
) -> Result<PushFlowResult, JournalFlowError> {
    if heads.is_empty() {
        return Err(JournalFlowError::InvalidInput(
            "no push heads were requested".to_owned(),
        ));
    }
    let mut requested = BTreeSet::new();
    for head in heads {
        QualifiedRef::from_bookmark(&head.bookmark)?;
        if !requested.insert(head.bookmark.clone()) {
            return Err(JournalFlowError::InvalidInput(format!(
                "bookmark `{}` was requested more than once",
                head.bookmark
            )));
        }
    }

    let remote_url = registered_remote(transport, remote).await?;
    let mut snapshot = transport.projection_snapshot_all().await?;
    let mut recovered_batches = recover_overlapping(
        repository,
        transport,
        &remote_url,
        remote,
        &requested,
        &snapshot,
        environment,
    )
    .await?;
    if !recovered_batches.is_empty() {
        snapshot = transport.projection_snapshot_all().await?;
    }

    let cursor_by_bookmark = snapshot
        .cursors
        .iter()
        .filter(|cursor| cursor.remote == remote)
        .map(|cursor| (cursor.bookmark.as_str(), cursor))
        .collect::<BTreeMap<_, _>>();
    for head in heads {
        if head.canonical_oid.is_none() && !cursor_by_bookmark.contains_key(head.bookmark.as_str())
        {
            return Err(JournalFlowError::InvalidInput(format!(
                "bookmark `{}` has no projection cursor to delete",
                head.bookmark
            )));
        }
    }
    let mut active_heads = Vec::new();
    for head in heads {
        if let Some(canonical_oid) = head.canonical_oid
            && cursor_by_bookmark
                .get(head.bookmark.as_str())
                .is_none_or(|cursor| cursor.canonical_oid != canonical_oid)
        {
            active_heads.push(canonical_oid);
        }
    }
    if active_heads.is_empty()
        && heads.iter().all(|head| {
            head.canonical_oid.is_some_and(|oid| {
                cursor_by_bookmark
                    .get(head.bookmark.as_str())
                    .is_some_and(|cursor| cursor.canonical_oid == oid)
            })
        })
    {
        return Ok(PushFlowResult {
            batch_id: None,
            outcome: "up-to-date".to_owned(),
            recovered_batches,
            public_heads: heads
                .iter()
                .map(|head| (head.bookmark.clone(), head.canonical_oid))
                .collect(),
        });
    }

    let seed_rows = snapshot.mappings.iter().map(|mapping| CommitMapping {
        canonical_id: mapping.canonical_oid,
        public_id: mapping.public_oid,
    });
    let mut mappings = ProjectionMappings::from_rows(seed_rows)?;
    let projected = repository
        .project_hidden_paths(&active_heads, &mut mappings)
        .await?;
    let projected_by_canonical = active_heads
        .iter()
        .copied()
        .zip(projected.public_heads.iter().copied())
        .collect::<BTreeMap<_, _>>();

    let mut public_heads = BTreeMap::new();
    for head in heads {
        public_heads.insert(
            head.bookmark.clone(),
            head.canonical_oid.map(|canonical| {
                projected_by_canonical
                    .get(&canonical)
                    .copied()
                    .unwrap_or_else(|| {
                        cursor_by_bookmark
                            .get(head.bookmark.as_str())
                            .map_or(canonical, |cursor| cursor.public_oid)
                    })
            }),
        );
    }
    for head in heads {
        if let (Some(canonical), Some(public)) = (head.canonical_oid, public_heads[&head.bookmark])
        {
            let hidden_set = repository.hidden_set_for_commit(canonical).await?;
            repository.scan_hidden_paths(public, &hidden_set)?;
        }
    }

    let closure_heads = heads
        .iter()
        .flat_map(|head| {
            [head.canonical_oid, public_heads[&head.bookmark]]
                .into_iter()
                .flatten()
        })
        .collect::<Vec<_>>();
    upload_closure(repository, transport, &closure_heads).await?;

    let mut state_rows = Vec::new();
    let mut seen_state_rows = BTreeSet::new();
    for row in projected
        .reached_mappings
        .iter()
        .chain(projected.new_mappings.iter())
    {
        if seen_state_rows.insert(row.canonical_id) {
            state_rows.push((row.canonical_id, row.public_id));
        }
    }
    let mut states = Vec::new();
    for (canonical_id, public_id) in state_rows {
        let hidden_set_id = repository
            .hidden_set_for_commit(canonical_id)
            .await?
            .identity()
            .to_projection_id();
        states.push(ProjectionGitState {
            canonical_oid: canonical_id,
            public_oid: public_id,
            hidden_set_id,
        });
    }
    for head in heads {
        if let (Some(canonical), Some(public)) = (head.canonical_oid, public_heads[&head.bookmark])
            && !states.iter().any(|state| state.canonical_oid == canonical)
        {
            let hidden_set_id = repository
                .hidden_set_for_commit(canonical)
                .await?
                .identity()
                .to_projection_id();
            states.push(ProjectionGitState {
                canonical_oid: canonical,
                public_oid: public,
                hidden_set_id,
            });
        }
    }
    let updates = heads
        .iter()
        .map(|head| {
            let cursor = cursor_by_bookmark.get(head.bookmark.as_str());
            let proposed_state = head.canonical_oid.map(|canonical| {
                states
                    .iter()
                    .position(|state| state.canonical_oid == canonical)
                    .expect("head state was inserted")
            });
            ProjectionGitUpdate {
                bookmark: head.bookmark.clone(),
                expected_old_oid: cursor.map(|cursor| cursor.public_oid),
                states: if proposed_state.is_some() {
                    states.clone()
                } else {
                    Vec::new()
                },
                proposed_state,
            }
        })
        .collect::<Vec<_>>();

    let begun = match transport.begin_push(batch_id, remote, &updates).await {
        Ok(result) => result,
        Err(error) if error_code(&error) == Some("push-in-progress") => {
            let raced = transport.projection_snapshot_all().await?;
            recovered_batches.extend(
                recover_overlapping(
                    repository,
                    transport,
                    &remote_url,
                    remote,
                    &requested,
                    &raced,
                    environment,
                )
                .await?,
            );
            let mut retried = Box::pin(push_with_journal(
                repository,
                transport,
                remote,
                heads,
                batch_id,
                environment,
                failpoint,
            ))
            .await?;
            recovered_batches.append(&mut retried.recovered_batches);
            retried.recovered_batches = recovered_batches;
            return Ok(retried);
        }
        Err(error) => return Err(error.into()),
    };
    if !begun.pending {
        return Ok(PushFlowResult {
            batch_id: Some(batch_id),
            outcome: begun.outcome.unwrap_or_else(|| "finished".to_owned()),
            recovered_batches,
            public_heads,
        });
    }

    let leases = lease_updates(&updates)?;
    let report = match push(
        repository.git_repo_path(),
        &remote_url,
        &leases,
        environment,
    ) {
        Ok(report) => report,
        Err(error) if error.kind == PushErrorKind::PushFailed => error.report,
        Err(error) => return Err(error.into()),
    };
    if failpoint == PushFailpoint::AfterGitPush {
        return Err(JournalFlowError::AfterPushFailpoint { batch_id });
    }
    let observations = observations_from_report(&updates, &report)?;
    let recovered = transport
        .recover_push(batch_id, begun.fence, &observations)
        .await?;
    require_accepted(&recovered)?;
    Ok(PushFlowResult {
        batch_id: Some(batch_id),
        outcome: "accepted".to_owned(),
        recovered_batches,
        public_heads,
    })
}

pub async fn fetch_with_journal(
    repository: &MachineGitRepository,
    transport: &GitHttpTransport,
    remote: &str,
    bookmarks: &[String],
    fetch_id: [u8; 16],
    environment: &GitProcessEnvironment,
) -> Result<FetchFlowResult, JournalFlowError> {
    let remote_url = registered_remote(transport, remote).await?;
    let mut snapshot = transport.projection_snapshot_all().await?;
    let requested = bookmarks.iter().cloned().collect::<BTreeSet<_>>();
    let recovered = recover_overlapping(
        repository,
        transport,
        &remote_url,
        remote,
        &requested,
        &snapshot,
        environment,
    )
    .await?;
    if !recovered.is_empty() {
        snapshot = transport.projection_snapshot_all().await?;
    }

    let fetched = fetch(
        repository.git_repo_path(),
        remote,
        bookmarks,
        &remote_url,
        environment,
    )?;
    let mut public_to_canonical = BTreeMap::new();
    for mapping in &snapshot.mappings {
        if let Some(previous) =
            public_to_canonical.insert(mapping.public_oid, mapping.canonical_oid)
            && previous != mapping.canonical_oid
        {
            return Err(JournalFlowError::AmbiguousPublicLineage(mapping.public_oid));
        }
    }
    let mut all_states = Vec::<ProjectionGitState>::new();
    let mut seen_public_states = BTreeSet::new();
    let mut grafts = Vec::new();
    let mut canonical_heads = BTreeMap::new();
    for (bookmark, public_head) in &fetched.heads {
        let grafted = graft_public_head(repository, *public_head, &public_to_canonical).await?;
        canonical_heads.insert(bookmark.clone(), grafted.canonical_head);
        for state in grafted.states {
            if seen_public_states.insert(state.public_oid) {
                all_states.push(state);
            }
        }
        grafts.extend(grafted.grafts);
    }
    let closure_heads = fetched
        .heads
        .values()
        .copied()
        .chain(canonical_heads.values().copied())
        .collect::<Vec<_>>();
    upload_closure(repository, transport, &closure_heads).await?;

    let cursor_by_bookmark = snapshot
        .cursors
        .iter()
        .filter(|cursor| cursor.remote == remote)
        .map(|cursor| (cursor.bookmark.as_str(), cursor))
        .collect::<BTreeMap<_, _>>();
    let states = all_states;
    let refs = fetched
        .heads
        .iter()
        .map(|(bookmark, public_head)| {
            if let Some(cursor) = cursor_by_bookmark.get(bookmark.as_str())
                && !reaches(repository, *public_head, cursor.public_oid)
            {
                return Err(JournalFlowError::RefRewritten {
                    remote: remote.to_owned(),
                    bookmark: bookmark.clone(),
                });
            }
            let own_states = states
                .iter()
                .filter(|state| reaches(repository, *public_head, state.public_oid))
                .cloned()
                .collect::<Vec<_>>();
            let proposed_state = own_states
                .iter()
                .position(|state| state.public_oid == *public_head);
            Ok(ProjectionGitFetchRef {
                bookmark: bookmark.clone(),
                observed_public_oid: *public_head,
                expected_cursor_oid: cursor_by_bookmark
                    .get(bookmark.as_str())
                    .map(|cursor| cursor.public_oid),
                states: own_states,
                proposed_state,
            })
        })
        .collect::<Result<Vec<_>, JournalFlowError>>()?;
    let receipt = transport.record_fetch(fetch_id, remote, &refs).await?;
    Ok(FetchFlowResult {
        receipt,
        public_heads: fetched.heads,
        canonical_heads,
        grafts,
    })
}

pub async fn graft_public_lineage(
    repository: &MachineGitRepository,
    head: Oid,
    mappings: impl IntoIterator<Item = CommitMapping>,
) -> Result<CanonicalGraftResult, JournalFlowError> {
    let mut public_to_canonical = BTreeMap::new();
    for mapping in mappings {
        if let Some(previous) = public_to_canonical.insert(mapping.public_id, mapping.canonical_id)
            && previous != mapping.canonical_id
        {
            return Err(JournalFlowError::AmbiguousPublicLineage(mapping.public_id));
        }
    }
    graft_public_head(repository, head, &public_to_canonical).await
}

async fn graft_public_head(
    repository: &MachineGitRepository,
    head: Oid,
    public_to_canonical: &BTreeMap<Oid, Oid>,
) -> Result<CanonicalGraftResult, JournalFlowError> {
    enum Visit {
        Enter(Oid),
        Exit(Oid),
    }
    let git = gix::open(repository.git_repo_path())
        .map_err(|error| JournalFlowError::GitObject(error.to_string()))?;
    let mut stack = vec![Visit::Enter(head)];
    let mut staged = BTreeMap::<Oid, Vec<u8>>::new();
    let mut canonical = public_to_canonical.clone();
    let mut states = Vec::new();
    let mut grafts = Vec::new();
    while let Some(visit) = stack.pop() {
        match visit {
            Visit::Enter(id) => {
                if canonical.contains_key(&id) || staged.contains_key(&id) {
                    continue;
                }
                let object = git
                    .find_object(gix::ObjectId::from_bytes_or_panic(&id.0))
                    .map_err(|error| JournalFlowError::GitObject(error.to_string()))?;
                if object.kind != GitObjectKind::Commit {
                    return Err(JournalFlowError::GitObject(
                        "fetched head is not a commit".to_owned(),
                    ));
                }
                let parsed = parse_commit(&object.data)
                    .map_err(|error| JournalFlowError::GitObject(error.to_string()))?;
                let parents = parsed.parents.clone();
                staged.insert(id, object.data.clone());
                stack.push(Visit::Exit(id));
                for parent in parents.into_iter().rev() {
                    stack.push(Visit::Enter(parent));
                }
            }
            Visit::Exit(id) => {
                let bytes = staged.remove(&id).expect("entered commit is staged");
                let parsed = parse_commit(&bytes)
                    .map_err(|error| JournalFlowError::GitObject(error.to_string()))?;
                let public_parents = parsed.parents.clone();
                let canonical_parents = public_parents
                    .iter()
                    .map(|parent| canonical.get(parent).copied().unwrap_or(*parent))
                    .collect::<Vec<_>>();
                let canonical_id = if public_parents == canonical_parents {
                    id
                } else {
                    let rewritten = rewrite_parent_headers(&bytes, &canonical_parents, id)?;
                    let written = git
                        .objects
                        .write_buf(GitObjectKind::Commit, &rewritten)
                        .map_err(|error| JournalFlowError::GitObject(error.to_string()))?;
                    let oid = Oid(written.as_bytes().try_into().map_err(|_| {
                        JournalFlowError::GitObject("Git wrote a non-SHA-1 object ID".to_owned())
                    })?);
                    grafts.push(CanonicalParentGraft {
                        public_commit: id,
                        canonical_commit: oid,
                        public_parents,
                        canonical_parents,
                    });
                    oid
                };
                canonical.insert(id, canonical_id);
                let hidden_set_id = repository
                    .hidden_set_for_commit(canonical_id)
                    .await?
                    .identity()
                    .to_projection_id();
                states.push(ProjectionGitState {
                    canonical_oid: canonical_id,
                    public_oid: id,
                    hidden_set_id,
                });
            }
        }
    }
    Ok(CanonicalGraftResult {
        canonical_head: canonical.get(&head).copied().unwrap_or(head),
        states,
        grafts,
    })
}

fn rewrite_parent_headers(
    source: &[u8],
    parents: &[Oid],
    id: Oid,
) -> Result<Vec<u8>, JournalFlowError> {
    let commit =
        parse_commit(source).map_err(|error| JournalFlowError::GitObject(error.to_string()))?;
    let message_start = source.len() - commit.message.len();
    let headers_end = message_start.checked_sub(1).ok_or_else(|| {
        JournalFlowError::GitObject(format!("commit {} has no header separator", hex(&id.0)))
    })?;
    let mut output = Vec::with_capacity(source.len());
    let mut parent_index = 0;
    for (index, header) in commit.headers.iter().enumerate() {
        let end = commit
            .headers
            .get(index + 1)
            .map_or(headers_end, |next| next.offset);
        match header.name {
            b"parent" => {
                output.extend_from_slice(b"parent ");
                output.extend_from_slice(hex(&parents[parent_index].0).as_bytes());
                output.push(b'\n');
                parent_index += 1;
            }
            b"gpgsig" | b"gpgsig-sha256" | b"mergetag" => {}
            _ => output.extend_from_slice(&source[header.offset..end]),
        }
    }
    output.push(b'\n');
    output.extend_from_slice(commit.message);
    Ok(output)
}

async fn recover_overlapping(
    repository: &MachineGitRepository,
    transport: &GitHttpTransport,
    remote_url: &RemoteUrl,
    remote: &str,
    bookmarks: &BTreeSet<String>,
    snapshot: &ProjectionGitSnapshot,
    environment: &GitProcessEnvironment,
) -> Result<Vec<[u8; 16]>, JournalFlowError> {
    let batches = snapshot
        .pending
        .iter()
        .filter(|batch| {
            batch.remote == remote
                && batch
                    .refs
                    .iter()
                    .any(|reference| bookmarks.contains(&reference.bookmark))
        })
        .map(|batch| batch.batch_id)
        .collect::<Vec<_>>();
    let mut recovered = Vec::new();
    for batch_id in batches {
        let claimed = transport.claim_push(batch_id).await?;
        if !claimed.pending && claimed.outcome.as_deref() == Some("accepted") {
            recovered.push(batch_id);
            continue;
        }
        let replay = transport.push_replay(batch_id).await?;
        download_cloud_catalog(repository, transport).await?;
        for update in &replay.updates {
            if let Some(index) = update.proposed_state {
                let state = update.states.get(index).ok_or_else(|| {
                    JournalFlowError::Protocol(
                        "replay proposed state is outside its state array".to_owned(),
                    )
                })?;
                let hidden = repository
                    .hidden_set_for_commit(state.canonical_oid)
                    .await?;
                repository.scan_hidden_paths(state.public_oid, &hidden)?;
            }
        }
        let leases = lease_updates(&replay.updates)?;
        let report = match push(repository.git_repo_path(), remote_url, &leases, environment) {
            Ok(report) => report,
            Err(error) if error.kind == PushErrorKind::PushFailed => error.report,
            Err(error) => return Err(error.into()),
        };
        let observations = observations_from_report(&replay.updates, &report)?;
        let result = transport
            .recover_push(batch_id, replay.fence, &observations)
            .await?;
        require_accepted(&result)?;
        recovered.push(batch_id);
    }
    Ok(recovered)
}

fn lease_updates(
    updates: &[ProjectionGitUpdate],
) -> Result<BTreeMap<QualifiedRef, LeaseUpdate>, JournalFlowError> {
    updates
        .iter()
        .map(|update| {
            let new_oid = update
                .proposed_state
                .map(|index| {
                    update
                        .states
                        .get(index)
                        .map(|state| state.public_oid)
                        .ok_or_else(|| {
                            JournalFlowError::Protocol(
                                "proposed state is outside its state array".to_owned(),
                            )
                        })
                })
                .transpose()?;
            Ok((
                QualifiedRef::from_bookmark(&update.bookmark)?,
                LeaseUpdate {
                    expected_old_oid: update.expected_old_oid,
                    new_oid,
                },
            ))
        })
        .collect()
}

fn observations_from_report(
    updates: &[ProjectionGitUpdate],
    report: &crate::PushReport,
) -> Result<Vec<ProjectionGitObservation>, JournalFlowError> {
    updates
        .iter()
        .map(|update| {
            let reference = QualifiedRef::from_bookmark(&update.bookmark)?;
            let observed = report.refs.get(&reference).ok_or_else(|| {
                JournalFlowError::Protocol(format!("push report omitted {reference}"))
            })?;
            Ok(ProjectionGitObservation {
                bookmark: update.bookmark.clone(),
                live_oid: observed.observed_oid,
            })
        })
        .collect()
}

async fn registered_remote(
    transport: &GitHttpTransport,
    remote: &str,
) -> Result<RemoteUrl, JournalFlowError> {
    transport
        .list_remotes()
        .await?
        .into_iter()
        .find(|entry| entry.name == remote)
        .map(|entry| RemoteUrl::new(entry.url))
        .ok_or_else(|| JournalFlowError::RemoteNotFound(remote.to_owned()))
}

async fn upload_closure(
    repository: &MachineGitRepository,
    transport: &GitHttpTransport,
    heads: &[Oid],
) -> Result<(), JournalFlowError> {
    if heads.is_empty() {
        return Ok(());
    }
    let closure = repository.object_closure(heads.iter().copied())?;
    let packs = build_packs(
        repository,
        &closure,
        &BTreeSet::new(),
        PackOptions::default(),
    )?;
    for pack in &packs.packs {
        transport.upload_pack(pack).await?;
    }
    Ok(())
}

async fn download_cloud_catalog(
    repository: &MachineGitRepository,
    transport: &GitHttpTransport,
) -> Result<(), JournalFlowError> {
    let mut after = 0;
    let mut through = None;
    loop {
        let page = transport.list_packs(after, through).await?;
        through = Some(page.through);
        for pack in page.packs {
            let downloaded = transport.download_pack(pack.id).await?;
            repository.install_pack(downloaded.id, &downloaded.manifest, &downloaded.chunks)?;
        }
        if !page.has_more {
            return Ok(());
        }
        after = page.next_after;
    }
}

fn reaches(repository: &MachineGitRepository, head: Oid, target: Oid) -> bool {
    if head == target {
        return true;
    }
    repository.object_closure([head]).is_ok_and(|closure| {
        closure
            .objects
            .iter()
            .any(|object| object.key.kind == ObjectKind::Commit && object.key.id == target)
    })
}

fn require_accepted(result: &ProjectionGitBatchResult) -> Result<(), JournalFlowError> {
    if !result.pending && result.outcome.as_deref() == Some("accepted") {
        Ok(())
    } else {
        Err(JournalFlowError::JournalDidNotAccept(result.clone()))
    }
}

fn error_code(error: &GitHttpTransportError) -> Option<&str> {
    match error {
        GitHttpTransportError::Status { code, .. } => code.as_deref(),
        _ => None,
    }
}

#[derive(Debug, Error)]
pub enum JournalFlowError {
    #[error("invalid journal flow input: {0}")]
    InvalidInput(String),
    #[error("remote `{0}` is not registered")]
    RemoteNotFound(String),
    #[error("public OID {0:?} has ambiguous canonical lineage")]
    AmbiguousPublicLineage(Oid),
    #[error("remote ref {remote}/{bookmark} does not descend from its projection cursor")]
    RefRewritten { remote: String, bookmark: String },
    #[error("Git object operation failed: {0}")]
    GitObject(String),
    #[error("journal protocol violation: {0}")]
    Protocol(String),
    #[error("AFTER_PUSH failpoint fired for batch {batch_id:?}")]
    AfterPushFailpoint { batch_id: [u8; 16] },
    #[error("projection journal did not accept the batch: {0:?}")]
    JournalDidNotAccept(ProjectionGitBatchResult),
    #[error(transparent)]
    Http(#[from] GitHttpTransportError),
    #[error(transparent)]
    Projection(#[from] ProjectionError),
    #[error(transparent)]
    Closure(#[from] ObjectClosureError),
    #[error(transparent)]
    Pack(#[from] PackBuildError),
    #[error(transparent)]
    Install(#[from] PackInstallError),
    #[error(transparent)]
    Push(#[from] PushError),
    #[error(transparent)]
    Fetch(#[from] crate::FetchError),
    #[error(transparent)]
    Ref(#[from] crate::QualifiedRefError),
}
