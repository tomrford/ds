//! Seed selection and parent-first lifting for fetched Git history.

use std::collections::{BTreeMap, BTreeSet};

use jj_lib::backend::{Commit as BackendCommit, CommitId, CopyId, FileId, TreeValue};
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;
use jj_lib::rewrite::merge_commit_trees;
use thiserror::Error;

use crate::git_projection::{HiddenSetCache, resolve_hidden_set_for_tree};
use crate::{
    CommitMapping, GitProjection, HiddenSetIdentity, ImportMappings, MachineRepository,
    ProjectionError, ProjectionSnapshot,
};

pub const TOMBSTONE_A: &[u8] = b"This conflict placeholder was inserted by Devspace.\n\
A collaborator published this file at a path this repository keeps\n\
private. The other side of this conflict is their published content;\n\
no private value existed here. Keep the content this file should have\n\
privately; deleting the file publishes a deletion on the next push.\n\
devspace-tombstone-v1-a\n";

pub const TOMBSTONE_B: &[u8] = b"This conflict placeholder was inserted by Devspace.\n\
A collaborator published this file at a path this repository keeps\n\
private. The other side of this conflict is their published content;\n\
no private value existed here. Keep the content this file should have\n\
privately; deleting the file publishes a deletion on the next push.\n\
devspace-tombstone-v1-b\n";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FetchedGitRef {
    pub remote: String,
    pub bookmark: String,
    pub head: [u8; 20],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitSeed {
    pub git_oid: [u8; 20],
    pub public_commit_id: CommitId,
    pub canonical_commit_id: CommitId,
    pub hidden_set_id: HiddenSetIdentity,
}

#[derive(Clone, Debug)]
pub struct SeedSelection {
    seeds: BTreeMap<CommitId, GitSeed>,
    stop_set: ImportMappings,
    ancestry_by_ref: BTreeMap<(String, String), BTreeSet<CommitId>>,
}

impl SeedSelection {
    pub fn from_seeds(seeds: impl IntoIterator<Item = GitSeed>) -> Result<Self, GitLiftError> {
        let mut by_git = BTreeMap::new();
        let mut stops = Vec::new();
        for seed in seeds {
            let git_id = CommitId::new(seed.git_oid.to_vec());
            if let Some(existing) = by_git.insert(git_id.clone(), seed.clone())
                && existing != seed
            {
                return Err(GitLiftError::AmbiguousSeed {
                    git_oid: seed.git_oid,
                });
            }
            stops.push(CommitMapping {
                git_id,
                canonical_id: seed.public_commit_id.clone(),
            });
        }
        Ok(Self {
            seeds: by_git,
            stop_set: ImportMappings::from_rows(stops)?,
            ancestry_by_ref: BTreeMap::new(),
        })
    }

    pub fn seeds(&self) -> impl Iterator<Item = &GitSeed> {
        self.seeds.values()
    }

    pub fn stop_set(&self) -> &ImportMappings {
        &self.stop_set
    }

    pub fn reaches(&self, remote: &str, bookmark: &str, git_oid: &[u8; 20]) -> bool {
        self.ancestry_by_ref
            .get(&(remote.to_owned(), bookmark.to_owned()))
            .is_some_and(|ancestry| ancestry.contains(&CommitId::new(git_oid.to_vec())))
    }

    pub fn seed_for(&self, git_oid: &[u8; 20]) -> Option<&GitSeed> {
        self.seeds.get(&CommitId::new(git_oid.to_vec()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiftedCommitState {
    pub git_oid: [u8; 20],
    pub canonical_commit_id: CommitId,
    pub public_commit_id: CommitId,
    pub hidden_set_id: HiddenSetIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiftResult {
    pub states: Vec<LiftedCommitState>,
    pub polluted_paths: BTreeSet<jj_lib::repo_path::RepoPathBuf>,
}

/// Select every receipt-backed seed reached from the fetched heads.
///
/// `receipts` contains only durable Git-to-public rows. Snapshot mappings are
/// active journal states; cursors and pending batches are deliberately ignored
/// as lineage evidence.
pub async fn select_seeds(
    projection: &GitProjection,
    fetched: &[FetchedGitRef],
    snapshot: &ProjectionSnapshot,
    receipts: &ImportMappings,
) -> Result<SeedSelection, GitLiftError> {
    if fetched.len() > crate::git_projection::MAX_IMPORT_HEADS {
        return Err(GitLiftError::Projection(ProjectionError::ImportHeadLimit {
            actual: fetched.len(),
            limit: crate::git_projection::MAX_IMPORT_HEADS,
        }));
    }
    let receipt_rows = receipts
        .rows()
        .map(|row| (row.git_id, row.canonical_id))
        .collect::<BTreeMap<_, _>>();
    let mut seeds = BTreeMap::new();
    let mut ancestry_by_ref = BTreeMap::new();

    for fetched_ref in fetched {
        let head_id = CommitId::new(fetched_ref.head.to_vec());
        let ancestry = git_ancestry(projection, &head_id).await?;
        ancestry_by_ref.insert(
            (fetched_ref.remote.clone(), fetched_ref.bookmark.clone()),
            ancestry.clone(),
        );
        if let Some(cursor) = snapshot.cursors.iter().find(|cursor| {
            cursor.remote == fetched_ref.remote && cursor.bookmark == fetched_ref.bookmark
        }) && !ancestry.contains(&CommitId::new(cursor.git_oid.to_vec()))
        {
            return Err(GitLiftError::RefRewritten {
                remote: fetched_ref.remote.clone(),
                bookmark: fetched_ref.bookmark.clone(),
            });
        }

        let mut found_seed = false;
        for git_id in &ancestry {
            let Some(public_id) = receipt_rows.get(git_id) else {
                continue;
            };
            let git_oid: [u8; 20] = git_id
                .as_bytes()
                .try_into()
                .expect("Git backend commit IDs are SHA-1 IDs");
            let mut newest_by_bookmark = BTreeMap::new();
            // Snapshot pages are concatenated in activation order, so replacing
            // a bookmark here retains its newest active state.
            for mapping in snapshot.mappings.iter().filter(|mapping| {
                mapping.remote == fetched_ref.remote
                    && mapping.git_oid == git_oid
                    && mapping.public_commit_id.as_slice() == public_id.as_bytes()
            }) {
                newest_by_bookmark.insert(mapping.bookmark.as_str(), mapping);
            }
            let mut lineage = None;
            for mapping in newest_by_bookmark.values() {
                let candidate = (
                    CommitId::new(mapping.canonical_commit_id.to_vec()),
                    HiddenSetIdentity::from_projection_id(mapping.hidden_set_id),
                );
                if lineage
                    .as_ref()
                    .is_some_and(|existing| existing != &candidate)
                {
                    return Err(GitLiftError::AmbiguousSeed { git_oid });
                }
                lineage = Some(candidate);
            }
            if let Some((canonical_commit_id, hidden_set_id)) = lineage {
                found_seed = true;
                seeds.insert(
                    git_id.clone(),
                    GitSeed {
                        git_oid,
                        public_commit_id: public_id.clone(),
                        canonical_commit_id,
                        hidden_set_id,
                    },
                );
            }
        }

        let has_cursor = snapshot.cursors.iter().any(|cursor| {
            cursor.remote == fetched_ref.remote && cursor.bookmark == fetched_ref.bookmark
        });
        if has_cursor && !found_seed {
            return Err(GitLiftError::MissingSeed {
                remote: fetched_ref.remote.clone(),
                bookmark: fetched_ref.bookmark.clone(),
            });
        }
    }

    Ok(SeedSelection {
        seeds,
        stop_set: ImportMappings::from_rows(receipts.rows())?,
        ancestry_by_ref,
    })
}

async fn git_ancestry(
    projection: &GitProjection,
    head: &CommitId,
) -> Result<BTreeSet<CommitId>, GitLiftError> {
    let store = projection.store();
    let mut seen = BTreeSet::new();
    let mut deepest = BTreeMap::<CommitId, usize>::new();
    let mut pending = vec![(head.clone(), 1_usize)];
    while let Some((id, depth)) = pending.pop() {
        if id == *store.root_commit_id() {
            continue;
        }
        if depth > crate::git_projection::MAX_IMPORT_COMMIT_DEPTH {
            return Err(GitLiftError::Projection(
                ProjectionError::ImportCommitDepthLimit {
                    commit_id: id,
                    depth,
                    limit: crate::git_projection::MAX_IMPORT_COMMIT_DEPTH,
                },
            ));
        }
        seen.insert(id.clone());
        if deepest.get(&id).is_some_and(|seen| *seen >= depth) {
            continue;
        }
        deepest.insert(id.clone(), depth);
        let commit = store.backend().read_commit(&id).await?;
        pending.extend(commit.parents.into_iter().map(|parent| (parent, depth + 1)));
    }
    Ok(seen)
}

/// Lift newly imported public shadows onto the selected private lineage.
pub async fn lift_imported(
    repository: &MachineRepository,
    imported: &[CommitMapping],
    selection: &SeedSelection,
) -> Result<LiftResult, GitLiftError> {
    let store = repository.repo().store();
    let mut transaction = repository.repo().start_transaction();
    let mut public_to_lifted = selection
        .seeds
        .values()
        .map(|seed| {
            (
                seed.public_commit_id.clone(),
                seed.canonical_commit_id.clone(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut imported_objects = Vec::with_capacity(imported.len());
    for mapping in imported {
        imported_objects.push(store.get_commit_async(&mapping.canonical_id).await?);
    }
    transaction
        .repo_mut()
        .index_commits(&imported_objects)
        .await?;

    let mut states = Vec::with_capacity(imported.len());
    let mut polluted_paths = BTreeSet::new();
    let mut hidden_cache = HiddenSetCache::default();
    for mapping in imported {
        let public_id = &mapping.canonical_id;
        let public = store.backend().read_commit(public_id).await?;
        let lifted_parents = public
            .parents
            .iter()
            .map(|parent| {
                if parent == store.root_commit_id() {
                    Ok(parent.clone())
                } else {
                    public_to_lifted.get(parent).cloned().ok_or_else(|| {
                        GitLiftError::MissingLiftedParent {
                            public_commit_id: public_id.clone(),
                            parent_id: parent.clone(),
                        }
                    })
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let public_parents = load_commits(store, &public.parents).await?;
        let private_parents = load_commits(store, &lifted_parents).await?;
        let public_base = merge_commit_trees(transaction.repo_mut(), &public_parents).await?;
        let private_base = merge_commit_trees(transaction.repo_mut(), &private_parents).await?;
        let public_tree = MergedTree::new(
            store.clone(),
            public.root_tree.clone(),
            jj_lib::conflict_labels::ConflictLabels::from_merge(public.conflict_labels.clone()),
        );
        let rebased = MergedTree::merge(Merge::from_vec(vec![
            (private_base.clone(), "lifted parents".to_owned()),
            (public_base, "public parents".to_owned()),
            (public_tree.clone(), "public commit".to_owned()),
        ]))
        .await?;
        let hidden_set =
            resolve_hidden_set_for_tree(store, public_id, &private_base, &mut hidden_cache).await?;
        let polluted =
            apply_pollution_tombstones(store, rebased, &private_base, &public_tree, &hidden_set)
                .await?;
        polluted_paths.extend(hidden_conflict_paths(&polluted, &hidden_set).await?);
        let (root_tree, labels) = polluted.into_tree_ids_and_labels();
        let lifted = BackendCommit {
            parents: lifted_parents,
            predecessors: public.predecessors,
            root_tree,
            conflict_labels: labels.into_merge(),
            change_id: public.change_id,
            description: public.description,
            author: public.author,
            committer: public.committer,
            secure_sig: None,
        };
        let lifted_id = store.write_commit(lifted, None).await?.id().clone();
        let lifted_commit = store.get_commit_async(&lifted_id).await?;
        transaction
            .repo_mut()
            .index_commits(std::slice::from_ref(&lifted_commit))
            .await?;
        public_to_lifted.insert(public_id.clone(), lifted_id.clone());
        states.push(LiftedCommitState {
            git_oid: mapping
                .git_id
                .as_bytes()
                .try_into()
                .map_err(|_| GitLiftError::InvalidGitCommitId(mapping.git_id.clone()))?,
            canonical_commit_id: lifted_id,
            public_commit_id: public_id.clone(),
            hidden_set_id: hidden_set.identity().clone(),
        });
    }
    Ok(LiftResult {
        states,
        polluted_paths,
    })
}

async fn hidden_conflict_paths(
    tree: &MergedTree,
    hidden_set: &crate::HiddenSet,
) -> Result<BTreeSet<jj_lib::repo_path::RepoPathBuf>, GitLiftError> {
    let mut paths = BTreeSet::new();
    for (path, value) in tree.entries() {
        let value = value?;
        if hidden_set.hides_file(&path) && value.into_resolved().is_err() {
            paths.insert(path);
        }
    }
    Ok(paths)
}

async fn load_commits(
    store: &std::sync::Arc<jj_lib::store::Store>,
    ids: &[CommitId],
) -> Result<Vec<jj_lib::commit::Commit>, jj_lib::backend::BackendError> {
    let mut commits = Vec::with_capacity(ids.len());
    for id in ids {
        commits.push(store.get_commit_async(id).await?);
    }
    Ok(commits)
}

async fn apply_pollution_tombstones(
    store: &std::sync::Arc<jj_lib::store::Store>,
    rebased: MergedTree,
    private_base: &MergedTree,
    public_tree: &MergedTree,
    hidden_set: &crate::HiddenSet,
) -> Result<MergedTree, GitLiftError> {
    let mut rewrites = Vec::new();
    for (path, value) in rebased.entries() {
        let value = value?;
        let Ok(Some(clean_value)) = value.into_resolved() else {
            continue;
        };
        if !hidden_set.hides_file(&path)
            || private_base.path_value(&path).await?.into_resolved() != Ok(None)
            || public_tree.path_value(&path).await?.into_resolved() != Ok(Some(clean_value.clone()))
        {
            continue;
        }
        let tombstone = tombstone_value(store, &path, &clean_value).await?;
        let conflict = Merge::from_removes_adds([Some(tombstone)], [None, Some(clean_value)]);
        rewrites.push((path, conflict));
    }
    if rewrites.is_empty() {
        return Ok(rebased);
    }
    let mut builder = MergedTreeBuilder::new(rebased);
    for (path, conflict) in rewrites {
        builder.set_or_remove(path, conflict);
    }
    Ok(builder.write_tree().await?)
}

async fn tombstone_value(
    store: &std::sync::Arc<jj_lib::store::Store>,
    path: &jj_lib::repo_path::RepoPath,
    public: &TreeValue,
) -> Result<TreeValue, GitLiftError> {
    let bytes = match public {
        TreeValue::File { id, .. } => {
            let mut bytes = Vec::new();
            let reader = store.read_file(path, id).await?;
            jj_lib::file_util::copy_async_to_sync(reader, &mut bytes)
                .await
                .map_err(GitLiftError::ReadPublicValue)?;
            bytes
        }
        TreeValue::Symlink(_) => Vec::new(),
        TreeValue::GitSubmodule(_) => return Err(GitLiftError::GitLink(path.to_owned())),
        TreeValue::Tree(_) => unreachable!("tree entries are traversed per leaf"),
    };
    let selected = if bytes == TOMBSTONE_A {
        TOMBSTONE_B
    } else {
        TOMBSTONE_A
    };
    let mut reader = selected;
    let id: FileId = store.write_file(path, &mut reader).await?;
    Ok(TreeValue::File {
        id,
        executable: false,
        copy_id: CopyId::placeholder(),
    })
}

#[derive(Debug, Error)]
pub enum GitLiftError {
    #[error("fetched ref {remote}/{bookmark} does not descend from its projection cursor")]
    RefRewritten { remote: String, bookmark: String },
    #[error("Git object {git_oid:02x?} has ambiguous private seed lineage")]
    AmbiguousSeed { git_oid: [u8; 20] },
    #[error("fetched ref {remote}/{bookmark} has a cursor but no receipt-backed private seed")]
    MissingSeed { remote: String, bookmark: String },
    #[error("public commit {public_commit_id} has no lifted mapping for parent {parent_id}")]
    MissingLiftedParent {
        public_commit_id: CommitId,
        parent_id: CommitId,
    },
    #[error("commit ID {0} is not a SHA-1 Git object ID")]
    InvalidGitCommitId(CommitId),
    #[error("cannot lift Git link at {0:?}")]
    GitLink(jj_lib::repo_path::RepoPathBuf),
    #[error("cannot read public value while selecting a tombstone")]
    ReadPublicValue(#[source] std::io::Error),
    #[error(transparent)]
    Projection(#[from] ProjectionError),
    #[error(transparent)]
    Backend(#[from] jj_lib::backend::BackendError),
}
