//! Library-level push, recovery, fetch, and overlay-lift proofs.

use std::collections::{BTreeMap, BTreeSet};

use devspace_kernel::{ObjectKind, Oid};
use thiserror::Error;

use crate::{
    CommitMapping, GitHttpTransport, GitHttpTransportError, GitProcessEnvironment, LeaseUpdate,
    LiftError, LiftedCommit, MachineGitRepository, ObjectClosureError, PackBuildError,
    PackInstallError, PackOptions, ProjectionError, ProjectionGitBatchResult,
    ProjectionGitFetchRef, ProjectionGitObservation, ProjectionGitSnapshot, ProjectionGitState,
    ProjectionGitUpdate, ProjectionMappings, PushError, PushErrorKind, QualifiedRef, RemoteUrl,
    build_packs, fetch, overlay_lift, push,
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
pub struct FetchFlowResult {
    pub public_heads: BTreeMap<String, Oid>,
    pub canonical_heads: BTreeMap<String, Oid>,
    pub mirrors: Vec<LiftedCommit>,
    pub disclosure_warnings: Vec<String>,
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
    let mut cloud_catalog = CloudCatalogInstaller::default();
    push_with_journal_attempt(
        repository,
        transport,
        remote,
        heads,
        batch_id,
        environment,
        failpoint,
        None,
        true,
        &mut cloud_catalog,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn push_with_journal_attempt(
    repository: &MachineGitRepository,
    transport: &GitHttpTransport,
    remote: &str,
    heads: &[PushHead],
    batch_id: [u8; 16],
    environment: &GitProcessEnvironment,
    failpoint: PushFailpoint,
    initial_snapshot: Option<ProjectionGitSnapshot>,
    retry_allowed: bool,
    cloud_catalog: &mut CloudCatalogInstaller,
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
    let mut snapshot = match initial_snapshot {
        Some(snapshot) => snapshot,
        None => transport.projection_snapshot_all().await?,
    };
    let recovery = recover_overlapping(
        repository,
        transport,
        &remote_url,
        remote,
        &requested,
        &snapshot,
        environment,
        cloud_catalog,
    )
    .await?;
    let mut recovered_batches = recovery.accepted;
    let recovered_deletions = snapshot
        .pending
        .iter()
        .filter(|batch| batch.remote == remote && recovered_batches.contains(&batch.batch_id))
        .flat_map(|batch| &batch.refs)
        .filter(|pending_ref| {
            requested.contains(&pending_ref.bookmark) && pending_ref.proposed_public_oid.is_none()
        })
        .map(|pending_ref| pending_ref.bookmark.clone())
        .collect::<BTreeSet<_>>();
    if recovery.settled {
        snapshot = transport.projection_snapshot_all().await?;
    }

    let cursor_by_bookmark = snapshot
        .cursors
        .iter()
        .filter(|cursor| cursor.remote == remote)
        .map(|cursor| (cursor.bookmark.as_str(), cursor))
        .collect::<BTreeMap<_, _>>();
    for head in heads {
        if head.canonical_oid.is_none()
            && !cursor_by_bookmark.contains_key(head.bookmark.as_str())
            && !recovered_deletions.contains(&head.bookmark)
        {
            return Err(JournalFlowError::InvalidInput(format!(
                "bookmark `{}` has no projection cursor to delete",
                head.bookmark
            )));
        }
    }
    let journal_heads = heads
        .iter()
        .filter(|head| {
            !(head.canonical_oid.is_none()
                && recovered_deletions.contains(&head.bookmark)
                && !cursor_by_bookmark.contains_key(head.bookmark.as_str()))
        })
        .collect::<Vec<_>>();
    let mut active_heads = Vec::new();
    for head in &journal_heads {
        if let Some(canonical_oid) = head.canonical_oid
            && cursor_by_bookmark
                .get(head.bookmark.as_str())
                .is_none_or(|cursor| cursor.canonical_oid != canonical_oid)
        {
            active_heads.push(canonical_oid);
        }
    }
    if active_heads.is_empty()
        && journal_heads.iter().all(|head| {
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
                .map(|head| {
                    let public = head.canonical_oid.map(|canonical| {
                        cursor_by_bookmark
                            .get(head.bookmark.as_str())
                            .map_or(canonical, |cursor| cursor.public_oid)
                    });
                    (head.bookmark.clone(), public)
                })
                .collect(),
        });
    }

    let seed_rows = snapshot
        .mappings
        .iter()
        .map(|mapping| CommitMapping {
            canonical_id: mapping.canonical_oid,
            public_id: mapping.public_oid,
        })
        .chain(snapshot.cursors.iter().map(|cursor| CommitMapping {
            canonical_id: cursor.canonical_oid,
            public_id: cursor.public_oid,
        }))
        .chain(snapshot.pending.iter().flat_map(|batch| {
            batch.refs.iter().filter_map(|reference| {
                reference.identity_oid.map(|identity_oid| CommitMapping {
                    canonical_id: identity_oid,
                    public_id: identity_oid,
                })
            })
        }));
    let mut mappings = ProjectionMappings::from_rows(seed_rows)?;
    let projected = project_with_cloud_seeds(
        repository,
        transport,
        &active_heads,
        &mut mappings,
        cloud_catalog,
    )
    .await?;
    let projected_by_canonical = active_heads
        .iter()
        .copied()
        .zip(projected.public_heads.iter().copied())
        .collect::<BTreeMap<_, _>>();

    let mut public_heads = recovered_deletions
        .iter()
        .map(|bookmark| (bookmark.clone(), None))
        .collect::<BTreeMap<_, _>>();
    for head in &journal_heads {
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
    let closure_heads = journal_heads
        .iter()
        .flat_map(|head| {
            [head.canonical_oid, public_heads[&head.bookmark]]
                .into_iter()
                .flatten()
        })
        .collect::<Vec<_>>();
    let required_closure = closure_heads.iter().copied().collect::<BTreeSet<_>>();
    ensure_cloud_closure(repository, transport, &required_closure, cloud_catalog).await?;
    for head in &journal_heads {
        if let (Some(canonical), Some(public)) = (head.canonical_oid, public_heads[&head.bookmark])
        {
            let hidden_set = repository.hidden_set_for_commit(canonical).await?;
            repository.scan_hidden_paths(public, &hidden_set)?;
        }
    }
    upload_closure(repository, transport, &closure_heads).await?;

    let mut state_rows = Vec::new();
    let mut seen_state_rows = BTreeSet::new();
    for row in projected
        .reached_mappings
        .iter()
        .chain(projected.new_mappings.iter())
    {
        if row.canonical_id != row.public_id && seen_state_rows.insert(row.canonical_id) {
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
    for head in &journal_heads {
        if let (Some(canonical), Some(public)) = (head.canonical_oid, public_heads[&head.bookmark])
            && canonical != public
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
    let updates = journal_heads
        .iter()
        .map(|head| {
            let cursor = cursor_by_bookmark.get(head.bookmark.as_str());
            let public_head = public_heads[&head.bookmark];
            let identity_oid = head
                .canonical_oid
                .filter(|canonical| public_head == Some(*canonical));
            let own_states = public_head.map_or_else(Vec::new, |public| {
                states
                    .iter()
                    .filter(|state| reaches(repository, public, state.public_oid))
                    .filter(|state| {
                        head.canonical_oid == Some(state.canonical_oid)
                            || !snapshot.mappings.iter().any(|mapping| {
                                mapping.remote == remote
                                    && mapping.bookmark == head.bookmark
                                    && mapping.canonical_oid == state.canonical_oid
                                    && mapping.public_oid == state.public_oid
                                    && mapping.hidden_set_id == state.hidden_set_id
                            })
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            });
            let proposed_state = head.canonical_oid.and_then(|canonical| {
                own_states
                    .iter()
                    .position(|state| state.canonical_oid == canonical)
            });
            debug_assert!(
                head.canonical_oid.is_none() || identity_oid.is_some() || proposed_state.is_some(),
                "rewritten push head state was inserted"
            );
            ProjectionGitUpdate {
                bookmark: head.bookmark.clone(),
                expected_old_oid: cursor.map(|cursor| cursor.public_oid),
                states: own_states,
                proposed_state,
                identity_oid,
            }
        })
        .collect::<Vec<_>>();

    let begun = match transport.begin_push(batch_id, remote, &updates).await {
        Ok(result) => result,
        Err(error) => {
            let Some(code @ ("push-in-progress" | "projection-cursor-stale")) = error_code(&error)
            else {
                return Err(error.into());
            };
            if !retry_allowed {
                return Err(error.into());
            }
            let raced = transport.projection_snapshot_all().await?;
            if code == "projection-cursor-stale" && raced == snapshot {
                return Err(error.into());
            }
            let mut retried = Box::pin(push_with_journal_attempt(
                repository,
                transport,
                remote,
                heads,
                batch_id,
                environment,
                failpoint,
                Some(raced),
                false,
                cloud_catalog,
            ))
            .await?;
            recovered_batches.append(&mut retried.recovered_batches);
            retried.recovered_batches = recovered_batches;
            return Ok(retried);
        }
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
    let (report, push_error) = match push(
        repository.git_repo_path(),
        &remote_url,
        &leases,
        environment,
    ) {
        Ok(report) => (report, None),
        Err(error) if error.kind == PushErrorKind::PushFailed => {
            (error.report.clone(), Some(error))
        }
        Err(error) => return Err(error.into()),
    };
    if failpoint == PushFailpoint::AfterGitPush {
        return Err(JournalFlowError::AfterPushFailpoint { batch_id });
    }
    let observations = observations_from_report(&updates, &report)?;
    let recovered = transport
        .recover_push(batch_id, begun.fence, &observations)
        .await?;
    if let Err(journal_error) = require_accepted(&recovered) {
        if recovered.outcome.as_deref() == Some("aborted")
            && let Some(push_error) = push_error
        {
            return Err(push_error.into());
        }
        return Err(journal_error);
    }
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
    let mut cloud_catalog = CloudCatalogInstaller::default();
    let mut snapshot = transport.projection_snapshot_all().await?;
    let requested = bookmarks.iter().cloned().collect::<BTreeSet<_>>();
    let recovery = recover_overlapping(
        repository,
        transport,
        &remote_url,
        remote,
        &requested,
        &snapshot,
        environment,
        &mut cloud_catalog,
    )
    .await?;
    if recovery.settled {
        snapshot = transport.projection_snapshot_all().await?;
    }

    let fetched = fetch(
        repository.git_repo_path(),
        remote,
        bookmarks,
        &remote_url,
        environment,
    )?;
    // Cursor and mapping rows can select canonical commits that are not
    // reachable from the public Git remote. Install the cloud object catalog
    // before lift so a fresh machine can resolve those private lineages.
    cloud_catalog.install(repository, transport).await?;
    let cursor_by_bookmark = snapshot
        .cursors
        .iter()
        .filter(|cursor| cursor.remote == remote)
        .map(|cursor| (cursor.bookmark.as_str(), cursor))
        .collect::<BTreeMap<_, _>>();
    let mut canonical_heads = BTreeMap::new();
    let mut states_by_bookmark = BTreeMap::<String, Vec<ProjectionGitState>>::new();
    let mut mirrors = Vec::new();
    let mut disclosures = Vec::new();
    for (bookmark, public_head) in &fetched.heads {
        let cursor = cursor_by_bookmark.get(bookmark.as_str()).copied();
        if let Some(cursor) = cursor
            && !reaches(repository, *public_head, cursor.public_oid)
        {
            return Err(JournalFlowError::RefRewritten {
                remote: remote.to_owned(),
                bookmark: bookmark.clone(),
            });
        }
        let seed_rows = snapshot
            .mappings
            .iter()
            .filter(|mapping| {
                mapping.remote == remote
                    && cursor.is_none_or(|cursor| {
                        mapping.public_oid != cursor.public_oid
                            || mapping.canonical_oid == cursor.canonical_oid
                    })
            })
            .map(|mapping| CommitMapping {
                canonical_id: mapping.canonical_oid,
                public_id: mapping.public_oid,
            })
            .chain(cursor.into_iter().map(|cursor| CommitMapping {
                canonical_id: cursor.canonical_oid,
                public_id: cursor.public_oid,
            }));
        let lifted = overlay_lift(repository, &[*public_head], seed_rows).await?;
        let canonical_head = lifted.canonical_heads[0];
        canonical_heads.insert(bookmark.clone(), canonical_head);
        let mut states = Vec::with_capacity(lifted.new_mappings.len());
        for mapping in &lifted.new_mappings {
            let hidden_set_id = repository
                .hidden_set_for_commit(mapping.canonical_id)
                .await?
                .identity()
                .to_projection_id();
            states.push(ProjectionGitState {
                canonical_oid: mapping.canonical_id,
                public_oid: mapping.public_id,
                hidden_set_id,
            });
        }
        states_by_bookmark.insert(bookmark.clone(), states);
        mirrors.extend(lifted.mirrors);
        disclosures.extend(lifted.disclosures);
    }
    let disclosure_warnings = disclosures
        .iter()
        .map(|disclosure| disclosure.warning())
        .collect::<Vec<_>>();
    for warning in &disclosure_warnings {
        eprintln!("{warning}");
    }
    let closure_heads = fetched
        .heads
        .values()
        .copied()
        .chain(canonical_heads.values().copied())
        .collect::<Vec<_>>();
    upload_closure(repository, transport, &closure_heads).await?;

    let refs = fetched
        .heads
        .iter()
        .map(|(bookmark, public_head)| {
            let own_states = states_by_bookmark
                .get(bookmark)
                .cloned()
                .expect("lift states were recorded per fetched bookmark");
            let proposed_state = own_states
                .iter()
                .position(|state| state.public_oid == *public_head);
            let canonical_head = canonical_heads[bookmark];
            Ok(ProjectionGitFetchRef {
                bookmark: bookmark.clone(),
                observed_public_oid: *public_head,
                expected_cursor_oid: cursor_by_bookmark
                    .get(bookmark.as_str())
                    .map(|cursor| cursor.public_oid),
                states: own_states,
                proposed_state,
                identity_oid: (proposed_state.is_none() && canonical_head == *public_head)
                    .then_some(*public_head),
            })
        })
        .collect::<Result<Vec<_>, JournalFlowError>>()?;
    transport.record_fetch(fetch_id, remote, &refs).await?;
    Ok(FetchFlowResult {
        public_heads: fetched.heads,
        canonical_heads,
        mirrors,
        disclosure_warnings,
    })
}

#[derive(Default)]
struct RecoverySummary {
    accepted: Vec<[u8; 16]>,
    settled: bool,
}

#[allow(clippy::too_many_arguments)]
async fn recover_overlapping(
    repository: &MachineGitRepository,
    transport: &GitHttpTransport,
    remote_url: &RemoteUrl,
    remote: &str,
    bookmarks: &BTreeSet<String>,
    snapshot: &ProjectionGitSnapshot,
    environment: &GitProcessEnvironment,
    cloud_catalog: &mut CloudCatalogInstaller,
) -> Result<RecoverySummary, JournalFlowError> {
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
    let mut recovery = RecoverySummary::default();
    for batch_id in batches {
        let claimed = transport.claim_push(batch_id).await?;
        match claimed.outcome.as_deref() {
            Some("accepted") => {
                recovery.accepted.push(batch_id);
                recovery.settled = true;
                continue;
            }
            Some("aborted") => {
                // The batch settled between the snapshot and claim. It has no
                // replay row and must simply disappear from the refreshed
                // snapshot before projection seeds are assembled.
                recovery.settled = true;
                continue;
            }
            Some(outcome) => {
                return Err(JournalFlowError::Protocol(format!(
                    "claim returned unknown projection outcome {outcome:?}"
                )));
            }
            None if claimed.pending => {}
            None => {
                return Err(JournalFlowError::Protocol(
                    "settled projection claim omitted its outcome".to_owned(),
                ));
            }
        }
        let replay = transport.push_replay(batch_id).await?;
        let replay_heads = replay
            .updates
            .iter()
            .flat_map(|update| {
                update
                    .proposed_state
                    .and_then(|index| update.states.get(index))
                    .into_iter()
                    .flat_map(|state| [state.canonical_oid, state.public_oid])
                    .chain(update.identity_oid)
            })
            .collect::<BTreeSet<_>>();
        ensure_cloud_closure(repository, transport, &replay_heads, cloud_catalog).await?;
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
        recovery.accepted.push(batch_id);
        recovery.settled = true;
    }
    Ok(recovery)
}

fn lease_updates(
    updates: &[ProjectionGitUpdate],
) -> Result<BTreeMap<QualifiedRef, LeaseUpdate>, JournalFlowError> {
    updates
        .iter()
        .map(|update| {
            let new_oid = if let Some(identity_oid) = update.identity_oid {
                Some(identity_oid)
            } else {
                update
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
                    .transpose()?
            };
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

#[derive(Default)]
struct CloudCatalogInstaller {
    installed_through: u64,
}

impl CloudCatalogInstaller {
    async fn install(
        &mut self,
        repository: &MachineGitRepository,
        transport: &GitHttpTransport,
    ) -> Result<(), JournalFlowError> {
        let mut after = self.installed_through;
        let mut through = None;
        loop {
            let page = transport.list_packs(after, through).await?;
            if through.is_some_and(|fixed| fixed != page.through) {
                return Err(JournalFlowError::Protocol(
                    "Git pack catalog page changed its high-water".to_owned(),
                ));
            }
            through = Some(page.through);
            if page.next_after < after || page.next_after > page.through {
                return Err(JournalFlowError::Protocol(
                    "Git pack catalog returned invalid cursor bounds".to_owned(),
                ));
            }
            if page.has_more && page.next_after <= after {
                return Err(JournalFlowError::Protocol(
                    "Git pack catalog did not make monotonic progress".to_owned(),
                ));
            }
            for pack in page.packs {
                let downloaded = transport.download_pack(pack.id).await?;
                repository.install_pack(downloaded.id, &downloaded.manifest, &downloaded.chunks)?;
            }
            if !page.has_more {
                self.installed_through = page.through;
                return Ok(());
            }
            after = page.next_after;
        }
    }
}

async fn project_with_cloud_seeds(
    repository: &MachineGitRepository,
    transport: &GitHttpTransport,
    active_heads: &[Oid],
    mappings: &mut ProjectionMappings,
    cloud_catalog: &mut CloudCatalogInstaller,
) -> Result<crate::ProjectionResult, JournalFlowError> {
    match repository
        .project_hidden_paths(active_heads, mappings)
        .await
    {
        Ok(projected) => Ok(projected),
        Err(ProjectionError::SeededPublicCommitUnavailable { .. }) => {
            cloud_catalog.install(repository, transport).await?;
            Ok(repository
                .project_hidden_paths(active_heads, mappings)
                .await?)
        }
        Err(error) => Err(error.into()),
    }
}

async fn ensure_cloud_closure(
    repository: &MachineGitRepository,
    transport: &GitHttpTransport,
    heads: &BTreeSet<Oid>,
    cloud_catalog: &mut CloudCatalogInstaller,
) -> Result<(), JournalFlowError> {
    if heads.is_empty() || repository.object_closure(heads.iter().copied()).is_ok() {
        return Ok(());
    }
    cloud_catalog.install(repository, transport).await?;
    repository.object_closure(heads.iter().copied())?;
    Ok(())
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
    #[error("remote ref {remote}/{bookmark} does not descend from its projection cursor")]
    RefRewritten { remote: String, bookmark: String },
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
    Lift(#[from] LiftError),
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
