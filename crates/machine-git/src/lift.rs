//! Fetch-side replay of hidden state onto foreign Git history.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use devspace_kernel_git::{Oid, parse_commit};
use gix::objs::{Kind as GitObjectKind, Write as _};
use jj_lib::backend::{Commit as BackendCommit, CommitId, CopyId, FileId, TreeId, TreeValue};
use jj_lib::merge::Merge;
use jj_lib::merged_tree::MergedTree;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::{RepoPath, RepoPathBuf};
use jj_lib::rewrite::merge_commit_trees;
use jj_lib::store::Store;
use thiserror::Error;

use crate::projection::{
    HiddenSetCache, ProjectionError, resolve_hidden_set_for_tree, rewrite_commit,
};
use crate::{CommitMapping, HiddenSet, MachineGitRepository};

const TOMBSTONE_A: &[u8] = b"This conflict placeholder was inserted by Devspace.\n\
A collaborator published this file at a path this repository keeps\n\
private. The other side of this conflict is their published content;\n\
no private value existed here. Keep the content this file should have\n\
privately; deleting the file publishes a deletion on the next push.\n\
devspace-tombstone-v1-a\n";

const TOMBSTONE_B: &[u8] = b"This conflict placeholder was inserted by Devspace.\n\
A collaborator published this file at a path this repository keeps\n\
private. The other side of this conflict is their published content;\n\
no private value existed here. Keep the content this file should have\n\
privately; deleting the file publishes a deletion on the next push.\n\
devspace-tombstone-v1-b\n";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiftedCommit {
    pub public_commit: Oid,
    pub canonical_commit: Oid,
    pub public_parents: Vec<Oid>,
    pub canonical_parents: Vec<Oid>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Disclosure {
    pub public_commit: Oid,
    pub path: RepoPathBuf,
}

impl Disclosure {
    pub fn warning(&self) -> String {
        format!(
            "WARNING: DATA DISCLOSURE: foreign commit {} contains hidden path `{}`; \
             that foreign version is publicly visible on the remote",
            crate::hex(&self.public_commit.0),
            self.path.as_internal_file_string(),
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverlayLiftResult {
    pub canonical_heads: Vec<Oid>,
    pub new_mappings: Vec<CommitMapping>,
    pub mirrors: Vec<LiftedCommit>,
    pub disclosures: Vec<Disclosure>,
}

enum Visit {
    Enter(Oid),
    Exit(Oid),
}

/// Replays canonical-parent hidden state over every newly reached public commit.
///
/// Seed mappings are stop points. Identity commits intentionally produce no
/// mapping row.
pub async fn overlay_lift(
    repository: &MachineGitRepository,
    public_heads: &[Oid],
    seed_mappings: impl IntoIterator<Item = CommitMapping>,
) -> Result<OverlayLiftResult, LiftError> {
    let git = repository.git_repo();
    let store = repository.repo().store();
    let mut public_to_canonical = BTreeMap::new();
    for mapping in seed_mappings {
        if let Some(previous) = public_to_canonical.insert(mapping.public_id, mapping.canonical_id)
            && previous != mapping.canonical_id
        {
            return Err(LiftError::AmbiguousPublicLineage(mapping.public_id));
        }
    }

    let mut states = BTreeMap::<Oid, bool>::new();
    let mut staged = BTreeMap::<Oid, Vec<u8>>::new();
    let mut parent_first = Vec::new();
    for head in public_heads {
        let mut stack = vec![Visit::Enter(*head)];
        while let Some(visit) = stack.pop() {
            match visit {
                Visit::Enter(id) => {
                    match states.get(&id) {
                        Some(true) => continue,
                        Some(false) => return Err(LiftError::CommitCycle(id)),
                        None => {}
                    }
                    if public_to_canonical.contains_key(&id) {
                        states.insert(id, true);
                        continue;
                    }
                    let bytes = read_commit(&git, id)?;
                    let commit = parse_commit(&bytes)
                        .map_err(|source| LiftError::InvalidCommit { id, source })?;
                    let parents = commit.parents.clone();
                    states.insert(id, false);
                    staged.insert(id, bytes);
                    stack.push(Visit::Exit(id));
                    for parent in parents.into_iter().rev() {
                        stack.push(Visit::Enter(parent));
                    }
                }
                Visit::Exit(id) => {
                    if states.get(&id) == Some(&true) {
                        continue;
                    }
                    states.insert(id, true);
                    parent_first.push(id);
                }
            }
        }
    }

    let mut transaction = repository.repo().start_transaction();
    let mut indexed_ids = BTreeSet::new();
    let mut imported = Vec::with_capacity(parent_first.len() + public_to_canonical.len() * 2);
    for (public_id, canonical_id) in &public_to_canonical {
        for id in [public_id, canonical_id] {
            if indexed_ids.insert(*id) {
                imported.push(store.get_commit_async(&CommitId::from_bytes(&id.0)).await?);
            }
        }
    }
    for id in &parent_first {
        if indexed_ids.insert(*id) {
            imported.push(store.get_commit_async(&CommitId::from_bytes(&id.0)).await?);
        }
    }
    transaction.repo_mut().index_commits(&imported).await?;

    let mut hidden_cache = HiddenSetCache::default();
    let mut new_mappings = Vec::new();
    let mut mirrors = Vec::new();
    let mut disclosures = Vec::new();
    for public_id in parent_first {
        let bytes = staged
            .remove(&public_id)
            .ok_or(LiftError::MissingStagedCommit(public_id))?;
        let parsed = parse_commit(&bytes).map_err(|source| LiftError::InvalidCommit {
            id: public_id,
            source,
        })?;
        let public_parents = parsed.parents.clone();
        let canonical_parents = public_parents
            .iter()
            .map(|parent| public_to_canonical.get(parent).copied().unwrap_or(*parent))
            .collect::<Vec<_>>();

        let public_backend_id = CommitId::from_bytes(&public_id.0);
        let public = store.backend().read_commit(&public_backend_id).await?;
        let public_parent_commits = load_commits(store, &public_parents).await?;
        let canonical_parent_commits = load_commits(store, &canonical_parents).await?;
        let public_base =
            merge_commit_trees(transaction.repo_mut(), &public_parent_commits).await?;
        let canonical_base =
            merge_commit_trees(transaction.repo_mut(), &canonical_parent_commits).await?;
        let public_tree = MergedTree::new(
            store.clone(),
            public.root_tree.clone(),
            jj_lib::conflict_labels::ConflictLabels::from_merge(public.conflict_labels.clone()),
        );
        let hidden_set = resolve_hidden_set_for_tree(
            store,
            &public_backend_id,
            &canonical_base,
            &mut hidden_cache,
        )
        .await?;
        let disclosed_paths =
            collect_hidden_paths(store, &public_tree, &hidden_set, public_id).await?;

        let parents_changed = public_parents != canonical_parents;
        if !parents_changed
            && hidden_set.identity().to_projection_id().is_none()
            && disclosed_paths.is_empty()
        {
            public_to_canonical.insert(public_id, public_id);
            continue;
        }

        let overlaid = MergedTree::merge(Merge::from_vec(vec![
            (canonical_base.clone(), "canonical parents".to_owned()),
            (public_base.clone(), "public parents".to_owned()),
            (public_tree.clone(), "foreign commit".to_owned()),
        ]))
        .await?;
        let overlaid = force_disclosure_conflicts(
            store,
            overlaid,
            &canonical_base,
            &public_tree,
            &disclosed_paths,
        )
        .await?;
        let canonical_id = write_lifted_commit(
            repository,
            &bytes,
            public_id,
            &public,
            &canonical_parents,
            overlaid,
        )
        .await?;
        let canonical_commit = store
            .get_commit_async(&CommitId::from_bytes(&canonical_id.0))
            .await?;
        transaction
            .repo_mut()
            .index_commits(std::slice::from_ref(&canonical_commit))
            .await?;
        public_to_canonical.insert(public_id, canonical_id);
        new_mappings.push(CommitMapping {
            canonical_id,
            public_id,
        });
        mirrors.push(LiftedCommit {
            public_commit: public_id,
            canonical_commit: canonical_id,
            public_parents,
            canonical_parents,
        });
        disclosures.extend(disclosed_paths.into_iter().map(|path| Disclosure {
            public_commit: public_id,
            path,
        }));
    }

    Ok(OverlayLiftResult {
        canonical_heads: public_heads
            .iter()
            .map(|head| public_to_canonical.get(head).copied().unwrap_or(*head))
            .collect(),
        new_mappings,
        mirrors,
        disclosures,
    })
}

async fn load_commits(
    store: &Arc<Store>,
    ids: &[Oid],
) -> Result<Vec<jj_lib::commit::Commit>, jj_lib::backend::BackendError> {
    let mut commits = Vec::with_capacity(ids.len());
    for id in ids {
        commits.push(store.get_commit_async(&CommitId::from_bytes(&id.0)).await?);
    }
    Ok(commits)
}

async fn collect_hidden_paths(
    store: &Arc<Store>,
    tree: &MergedTree,
    hidden_set: &HiddenSet,
    commit_id: Oid,
) -> Result<Vec<RepoPathBuf>, LiftError> {
    let tree_id = tree
        .tree_ids()
        .as_resolved()
        .ok_or(LiftError::ConflictedPublicCommit(commit_id))?;
    let mut paths =
        collect_hidden_paths_in_tree(store, RepoPath::root(), tree_id, hidden_set).await?;
    paths.sort_unstable();
    Ok(paths)
}

fn collect_hidden_paths_in_tree<'a>(
    store: &'a Arc<Store>,
    path: &'a RepoPath,
    tree_id: &'a TreeId,
    hidden_set: &'a HiddenSet,
) -> Pin<Box<dyn Future<Output = Result<Vec<RepoPathBuf>, LiftError>> + 'a>> {
    Box::pin(async move {
        let tree = store.get_tree(path.to_owned(), tree_id).await?;
        let mut paths = Vec::new();
        for entry in tree.entries_non_recursive() {
            let entry_path = path.join(entry.name());
            match entry.value() {
                TreeValue::Tree(child_id) => {
                    paths.extend(
                        collect_hidden_paths_in_tree(store, &entry_path, child_id, hidden_set)
                            .await?,
                    );
                }
                TreeValue::GitSubmodule(_) => {
                    return Err(LiftError::Gitlink(entry_path));
                }
                TreeValue::File { .. } | TreeValue::Symlink(_) => {
                    if hidden_set.hides_file(&entry_path) {
                        paths.push(entry_path);
                    }
                }
            }
        }
        Ok(paths)
    })
}

async fn force_disclosure_conflicts(
    store: &Arc<Store>,
    overlaid: MergedTree,
    canonical_base: &MergedTree,
    public_tree: &MergedTree,
    disclosed_paths: &[RepoPathBuf],
) -> Result<MergedTree, LiftError> {
    let mut rewrites = Vec::new();
    for path in disclosed_paths {
        if !overlaid.path_value(path).await?.is_resolved() {
            continue;
        }
        let public = public_tree
            .path_value(path)
            .await?
            .into_resolved()
            .expect("foreign Git trees are resolved");
        let canonical = canonical_base.path_value(path).await?;
        let Ok(canonical) = canonical.into_resolved() else {
            continue;
        };
        let tombstone = tombstone_value(
            store,
            path,
            public.as_ref().expect("disclosed path is present"),
        )
        .await?;
        let conflict = Merge::from_removes_adds([Some(tombstone)], [canonical, public]);
        rewrites.push((path.clone(), conflict));
    }
    if rewrites.is_empty() {
        return Ok(overlaid);
    }
    let mut builder = MergedTreeBuilder::new(overlaid);
    for (path, conflict) in rewrites {
        builder.set_or_remove(path, conflict);
    }
    Ok(builder.write_tree().await?)
}

async fn tombstone_value(
    store: &Arc<Store>,
    path: &RepoPath,
    public: &TreeValue,
) -> Result<TreeValue, LiftError> {
    let public_bytes = match public {
        TreeValue::File { id, .. } => {
            let mut bytes = Vec::new();
            let reader = store.read_file(path, id).await?;
            jj_lib::file_util::copy_async_to_sync(reader, &mut bytes)
                .await
                .map_err(LiftError::ReadPublicValue)?;
            bytes
        }
        TreeValue::Symlink(_) => Vec::new(),
        TreeValue::GitSubmodule(_) => return Err(LiftError::Gitlink(path.to_owned())),
        TreeValue::Tree(_) => unreachable!("tree entries are traversed per leaf"),
    };
    let bytes = if public_bytes == TOMBSTONE_A {
        TOMBSTONE_B
    } else {
        TOMBSTONE_A
    };
    let mut reader = bytes;
    let id: FileId = store.write_file(path, &mut reader).await?;
    Ok(TreeValue::File {
        id,
        executable: false,
        copy_id: CopyId::placeholder(),
    })
}

async fn write_lifted_commit(
    repository: &MachineGitRepository,
    source_bytes: &[u8],
    source_id: Oid,
    source: &BackendCommit,
    parents: &[Oid],
    tree: MergedTree,
) -> Result<Oid, LiftError> {
    let store = repository.repo().store();
    let (root_tree, labels) = tree.into_tree_ids_and_labels();
    if let Some(tree_id) = root_tree.as_resolved() {
        let tree_id = Oid::from_bytes(tree_id.as_bytes())
            .ok_or_else(|| LiftError::InvalidTreeId(tree_id.as_bytes().to_vec()))?;
        let bytes = rewrite_commit(source_bytes, tree_id, parents, source_id)?;
        return write_commit(repository, &bytes);
    }

    let template = BackendCommit {
        parents: parents
            .iter()
            .map(|parent| CommitId::from_bytes(&parent.0))
            .collect(),
        predecessors: source.predecessors.clone(),
        root_tree,
        conflict_labels: labels.into_merge(),
        change_id: source.change_id.clone(),
        description: source.description.clone(),
        author: source.author.clone(),
        committer: source.committer.clone(),
        secure_sig: None,
    };
    let template_id = store.write_commit(template, None).await?.id().clone();
    let template_oid = Oid::from_bytes(template_id.as_bytes())
        .ok_or_else(|| LiftError::InvalidCommitId(template_id.as_bytes().to_vec()))?;
    let template_bytes = read_commit(&repository.git_repo(), template_oid)?;
    let bytes = rewrite_conflicted_commit(source_bytes, &template_bytes, parents, source_id)?;
    write_commit(repository, &bytes)
}

fn rewrite_conflicted_commit(
    source: &[u8],
    template: &[u8],
    parents: &[Oid],
    source_id: Oid,
) -> Result<Vec<u8>, LiftError> {
    let template_commit = parse_commit(template).map_err(|source| LiftError::InvalidCommit {
        id: source_id,
        source,
    })?;
    let mut output = rewrite_commit(source, template_commit.tree, parents, source_id)?;
    let message_start = output.len()
        - parse_commit(&output)
            .expect("rewritten commit is valid")
            .message
            .len();
    output.truncate(message_start - 1);
    for name in [b"jj:conflict-labels".as_slice(), b"jj:trees".as_slice()] {
        if let Some(header) = template_commit
            .headers
            .iter()
            .find(|header| header.name == name)
        {
            let index = template_commit
                .headers
                .iter()
                .position(|candidate| std::ptr::eq(candidate, header))
                .expect("header came from the template");
            let end = template_commit
                .headers
                .get(index + 1)
                .map_or(template.len() - template_commit.message.len() - 1, |next| {
                    next.offset
                });
            output.extend_from_slice(&template[header.offset..end]);
        }
    }
    output.push(b'\n');
    output.extend_from_slice(
        parse_commit(source)
            .expect("source was already validated")
            .message,
    );
    Ok(output)
}

fn read_commit(git: &gix::Repository, id: Oid) -> Result<Vec<u8>, LiftError> {
    let object = git
        .find_object(gix::ObjectId::from_bytes_or_panic(&id.0))
        .map_err(|source| LiftError::ReadObject {
            id,
            source: Box::new(source),
        })?;
    if object.kind != GitObjectKind::Commit {
        return Err(LiftError::WrongObjectKind {
            id,
            actual: object.kind,
        });
    }
    Ok(object.data.clone())
}

fn write_commit(repository: &MachineGitRepository, bytes: &[u8]) -> Result<Oid, LiftError> {
    let id = repository
        .git_repo()
        .objects
        .write_buf(GitObjectKind::Commit, bytes)
        .map_err(|source| LiftError::WriteObject { source })?;
    Oid::from_bytes(id.as_bytes()).ok_or_else(|| LiftError::InvalidCommitId(id.as_bytes().to_vec()))
}

#[derive(Debug, Error)]
pub enum LiftError {
    #[error("public commit {0:?} has ambiguous canonical lineage")]
    AmbiguousPublicLineage(Oid),
    #[error("cycle in fetched commit graph at {0:?}")]
    CommitCycle(Oid),
    #[error("missing staged public commit {0:?}")]
    MissingStagedCommit(Oid),
    #[error("fetched public commit {0:?} is already conflicted")]
    ConflictedPublicCommit(Oid),
    #[error("invalid fetched commit {id:?}")]
    InvalidCommit {
        id: Oid,
        #[source]
        source: devspace_kernel_git::CommitError,
    },
    #[error("cannot read Git object {id:?}")]
    ReadObject {
        id: Oid,
        #[source]
        source: Box<gix::object::find::existing::Error>,
    },
    #[error("Git object {id:?} is {actual:?}, not a commit")]
    WrongObjectKind { id: Oid, actual: GitObjectKind },
    #[error("cannot write lifted Git commit")]
    WriteObject {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("commit ID is not a SHA-1 Git object ID: {0:02x?}")]
    InvalidCommitId(Vec<u8>),
    #[error("tree ID is not a SHA-1 Git object ID: {0:02x?}")]
    InvalidTreeId(Vec<u8>),
    #[error("cannot lift Git link at {0:?}")]
    Gitlink(RepoPathBuf),
    #[error("cannot read public value while selecting a disclosure tombstone")]
    ReadPublicValue(#[source] std::io::Error),
    #[error(transparent)]
    Projection(#[from] ProjectionError),
    #[error(transparent)]
    Backend(#[from] jj_lib::backend::BackendError),
}
