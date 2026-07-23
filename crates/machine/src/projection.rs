//! Hidden-path projection inside the canonical Git object database.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use blake2::{Blake2b512, Digest as _};
use devspace_kernel::{CommitError, Oid, TreeEntryKind, TreeError, parse_commit, parse_tree};
use gix::objs::{Kind as GitObjectKind, Write as _};
use jj_lib::backend::{Commit as BackendCommit, CommitId, FileId, TreeId, TreeValue};
use jj_lib::conflict_labels::ConflictLabels;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::{RepoPath, RepoPathBuf};
use jj_lib::store::Store;
use thiserror::Error;

use crate::MachineGitRepository;

const DSPRIVATE: &str = ".dsprivate";
const HIDDEN_SET_DOMAIN: &[u8] = b"devspace-hidden-set-v1";
const HIDDEN_CHAIN_DOMAIN: &[u8] = b"devspace-hidden-chain-v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommitMapping {
    pub canonical_id: Oid,
    pub public_id: Oid,
}

#[derive(Clone, Debug, Default)]
pub struct ProjectionMappings {
    by_canonical: BTreeMap<Oid, Oid>,
}

impl ProjectionMappings {
    pub fn from_rows(
        rows: impl IntoIterator<Item = CommitMapping>,
    ) -> Result<Self, ProjectionError> {
        let mut mappings = Self::default();
        for row in rows {
            mappings.record(row.canonical_id, row.public_id)?;
        }
        Ok(mappings)
    }

    pub fn rows(&self) -> impl Iterator<Item = CommitMapping> + '_ {
        self.by_canonical
            .iter()
            .map(|(canonical_id, public_id)| CommitMapping {
                canonical_id: *canonical_id,
                public_id: *public_id,
            })
    }

    fn record(&mut self, canonical_id: Oid, public_id: Oid) -> Result<bool, ProjectionError> {
        if let Some(existing) = self.by_canonical.get(&canonical_id) {
            if *existing != public_id {
                return Err(ProjectionError::ConflictingMapping {
                    canonical_id,
                    existing: *existing,
                    proposed: public_id,
                });
            }
            return Ok(false);
        }
        self.by_canonical.insert(canonical_id, public_id);
        Ok(true)
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HiddenSetIdentity(Option<[u8; 64]>);

impl HiddenSetIdentity {
    pub fn to_projection_id(&self) -> Option<[u8; 64]> {
        self.0
    }
}

#[derive(Clone, Debug)]
pub struct HiddenSet {
    identity: HiddenSetIdentity,
    matcher: Arc<GitIgnoreFile>,
    files: BTreeMap<RepoPathBuf, FileId>,
}

impl HiddenSet {
    pub fn identity(&self) -> &HiddenSetIdentity {
        &self.identity
    }

    pub fn hides_file(&self, path: &RepoPath) -> bool {
        if is_dsprivate_path(path) || self.matcher.matches_file(path) {
            return true;
        }
        let mut ancestor = path.parent();
        while let Some(directory) = ancestor {
            if directory.is_root() {
                break;
            }
            if self.matcher.matches_dir(directory) {
                return true;
            }
            ancestor = directory.parent();
        }
        false
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionResult {
    pub public_heads: Vec<Oid>,
    pub new_mappings: Vec<CommitMapping>,
    pub reached_mappings: Vec<CommitMapping>,
}

#[derive(Default)]
pub(crate) struct HiddenSetCache {
    blobs: BTreeMap<FileId, Arc<Vec<u8>>>,
    trees: BTreeMap<TreeId, Arc<Vec<RepoPathBuf>>>,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct HiddenChainIdentity([u8; 64]);

#[derive(Clone)]
struct HiddenChain {
    identity: HiddenChainIdentity,
    hasher: Blake2b512,
    matcher: Arc<GitIgnoreFile>,
}

impl HiddenChain {
    fn empty() -> Self {
        let mut hasher = Blake2b512::new();
        hasher.update(HIDDEN_CHAIN_DOMAIN);
        let identity = HiddenChainIdentity(hasher.clone().finalize().into());
        Self {
            identity,
            hasher,
            matcher: GitIgnoreFile::empty(),
        }
    }

    fn chain(&self, prefix: &RepoPath, id: &FileId, bytes: &[u8]) -> Self {
        let mut hasher = self.hasher.clone();
        hash_hidden_file(&mut hasher, prefix, id);
        let identity = HiddenChainIdentity(hasher.clone().finalize().into());
        let matcher = self
            .matcher
            .chain(prefix, Path::new(DSPRIVATE), bytes)
            .expect("in-memory gitignore patterns have no parse errors");
        Self {
            identity,
            hasher,
            matcher,
        }
    }
}

#[derive(Default)]
struct ProjectionCache {
    hidden_sets: HiddenSetCache,
    trees: BTreeMap<(HiddenChainIdentity, RepoPathBuf, Oid), Oid>,
}

enum Visit {
    Enter(Oid),
    Exit(Oid),
}

impl MachineGitRepository {
    /// Projects canonical heads to public heads in the same Git object store.
    ///
    /// A seeded mapping is a repository-wide stop point. Commits whose tree and
    /// complete visited parent cone are unchanged remain identity mappings and
    /// deliberately produce no mapping row.
    pub async fn project_hidden_paths(
        &self,
        canonical_heads: &[Oid],
        mappings: &mut ProjectionMappings,
    ) -> Result<ProjectionResult, ProjectionError> {
        let git_repo = self.git_repo();
        let store = self.repo().store();
        let mut states = BTreeMap::<Oid, bool>::new();
        let mut commits = BTreeMap::<Oid, Vec<u8>>::new();
        let mut parent_first = Vec::new();
        let mut reached_mappings = BTreeMap::new();

        for head in canonical_heads {
            let mut stack = vec![Visit::Enter(*head)];
            while let Some(visit) = stack.pop() {
                match visit {
                    Visit::Enter(id) => {
                        match states.get(&id) {
                            Some(true) => continue,
                            Some(false) => return Err(ProjectionError::CommitCycle(id)),
                            None => {}
                        }
                        if let Some(public_id) = mappings.by_canonical.get(&id).copied() {
                            read_object(&git_repo, public_id, GitObjectKind::Commit)?;
                            reached_mappings.insert(id, public_id);
                            states.insert(id, true);
                            continue;
                        }
                        let bytes = read_object(&git_repo, id, GitObjectKind::Commit)?;
                        let commit = parse_commit(&bytes)
                            .map_err(|source| ProjectionError::InvalidCommit { id, source })?;
                        let parents = commit.parents.clone();
                        states.insert(id, false);
                        commits.insert(id, bytes);
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

        let mut cache = ProjectionCache::default();
        let mut identities = BTreeSet::new();
        let mut new_mappings = Vec::new();
        for canonical_id in parent_first {
            let bytes = commits
                .remove(&canonical_id)
                .ok_or(ProjectionError::MissingStagedCommit(canonical_id))?;
            let commit = parse_commit(&bytes).map_err(|source| ProjectionError::InvalidCommit {
                id: canonical_id,
                source,
            })?;
            let public_parents = commit
                .parents
                .iter()
                .map(|parent| {
                    if let Some(public_id) = mappings.by_canonical.get(parent) {
                        Ok(*public_id)
                    } else if identities.contains(parent) {
                        Ok(*parent)
                    } else {
                        Err(ProjectionError::MissingParentProjection(*parent))
                    }
                })
                .collect::<Result<Vec<_>, _>>()?;

            let backend_id = CommitId::from_bytes(&canonical_id.0);
            let backend_commit = store.backend().read_commit(&backend_id).await?;
            let hidden_set =
                resolve_hidden_set(store, &backend_id, &backend_commit, &mut cache.hidden_sets)
                    .await?;
            if !commit.jj_trees.is_empty() {
                return Err(ProjectionError::ConflictedCommit(canonical_id));
            }
            let public_tree = rewrite_tree(
                &git_repo,
                RepoPath::root(),
                commit.tree,
                &HiddenChain::empty(),
                &hidden_set,
                &mut cache,
            )?;
            let parents_changed = commit.parents != public_parents;
            if public_tree == commit.tree && !parents_changed {
                identities.insert(canonical_id);
                continue;
            }

            let public_bytes = rewrite_commit(&bytes, public_tree, &public_parents, canonical_id)?;
            let public_id = write_object(&git_repo, GitObjectKind::Commit, &public_bytes)?;
            if mappings.record(canonical_id, public_id)? {
                new_mappings.push(CommitMapping {
                    canonical_id,
                    public_id,
                });
            }
        }

        let public_heads = canonical_heads
            .iter()
            .map(|id| mappings.by_canonical.get(id).copied().unwrap_or(*id))
            .collect();
        Ok(ProjectionResult {
            public_heads,
            new_mappings,
            reached_mappings: reached_mappings
                .into_iter()
                .map(|(canonical_id, public_id)| CommitMapping {
                    canonical_id,
                    public_id,
                })
                .collect(),
        })
    }

    pub async fn hidden_set_for_commit(
        &self,
        canonical_id: Oid,
    ) -> Result<HiddenSet, ProjectionError> {
        let store = self.repo().store();
        let commit_id = CommitId::from_bytes(&canonical_id.0);
        let commit = store.backend().read_commit(&commit_id).await?;
        resolve_hidden_set(store, &commit_id, &commit, &mut HiddenSetCache::default()).await
    }

    /// Scans a complete public tree with the canonical commit's hidden set.
    pub fn scan_hidden_paths(
        &self,
        public_head: Oid,
        hidden_set: &HiddenSet,
    ) -> Result<(), ProjectionError> {
        let git_repo = self.git_repo();
        let bytes = read_object(&git_repo, public_head, GitObjectKind::Commit)?;
        let commit = parse_commit(&bytes).map_err(|source| ProjectionError::InvalidCommit {
            id: public_head,
            source,
        })?;
        let mut leaked = Vec::new();
        scan_tree(
            &git_repo,
            RepoPath::root(),
            commit.tree,
            &hidden_set.matcher,
            &mut leaked,
        )?;
        if leaked.is_empty() {
            Ok(())
        } else {
            Err(ProjectionError::HiddenPathLeak {
                public_id: public_head,
                leaked,
            })
        }
    }
}

fn rewrite_tree(
    git_repo: &gix::Repository,
    path: &RepoPath,
    source_id: Oid,
    inherited_chain: &HiddenChain,
    hidden_set: &HiddenSet,
    cache: &mut ProjectionCache,
) -> Result<Oid, ProjectionError> {
    let source = read_object(git_repo, source_id, GitObjectKind::Tree)?;
    let mut chain = inherited_chain.clone();
    let dsprivate_path = join_component(path, DSPRIVATE.as_bytes())?;
    if let Some(id) = hidden_set.files.get(&dsprivate_path) {
        let bytes = cache
            .hidden_sets
            .blobs
            .get(id)
            .expect("hidden-set resolution caches every policy blob");
        chain = chain.chain(path, id, bytes);
    }
    let cache_key = (chain.identity.clone(), path.to_owned(), source_id);
    if let Some(target_id) = cache.trees.get(&cache_key) {
        return Ok(*target_id);
    }

    let entries = raw_tree_entries(source_id, &source)?;
    let mut output = Vec::with_capacity(source.len());
    let mut changed = false;
    for entry in entries {
        let entry_path = join_component(path, entry.name)?;
        let excluded = match entry.kind {
            TreeEntryKind::Tree => chain.matcher.matches_dir(&entry_path),
            _ => entry.name == DSPRIVATE.as_bytes() || chain.matcher.matches_file(&entry_path),
        };
        if excluded {
            changed = true;
            continue;
        }
        match entry.kind {
            TreeEntryKind::Tree => {
                let target_id =
                    rewrite_tree(git_repo, &entry_path, entry.oid, &chain, hidden_set, cache)?;
                let target_bytes = read_object(git_repo, target_id, GitObjectKind::Tree)?;
                if target_bytes.is_empty() {
                    changed = true;
                    continue;
                }
                output.extend_from_slice(entry.raw);
                if target_id != entry.oid {
                    let oid_start = output.len() - Oid::LENGTH;
                    output[oid_start..].copy_from_slice(&target_id.0);
                    changed = true;
                }
            }
            TreeEntryKind::Gitlink => {
                return Err(ProjectionError::Gitlink { path: entry_path });
            }
            TreeEntryKind::File | TreeEntryKind::Executable | TreeEntryKind::Symlink => {
                output.extend_from_slice(entry.raw);
            }
        }
    }
    let target_id = if changed {
        write_object(git_repo, GitObjectKind::Tree, &output)?
    } else {
        source_id
    };
    cache.trees.insert(cache_key, target_id);
    Ok(target_id)
}

struct RawTreeEntry<'a> {
    kind: TreeEntryKind,
    name: &'a [u8],
    oid: Oid,
    raw: &'a [u8],
}

fn raw_tree_entries(id: Oid, bytes: &[u8]) -> Result<Vec<RawTreeEntry<'_>>, ProjectionError> {
    let tree = parse_tree(bytes).map_err(|source| ProjectionError::InvalidTree { id, source })?;
    let mut offset = 0;
    let mut output = Vec::with_capacity(tree.entries.len());
    for entry in tree.entries {
        let remainder = &bytes[offset..];
        let mode_end = remainder
            .iter()
            .position(|byte| *byte == b' ')
            .expect("validated tree has a mode terminator");
        let after_mode = offset + mode_end + 1;
        let name_end = bytes[after_mode..]
            .iter()
            .position(|byte| *byte == 0)
            .expect("validated tree has a name terminator");
        let end = after_mode + name_end + 1 + Oid::LENGTH;
        output.push(RawTreeEntry {
            kind: entry.kind,
            name: entry.name,
            oid: entry.oid,
            raw: &bytes[offset..end],
        });
        offset = end;
    }
    Ok(output)
}

pub(crate) fn rewrite_commit(
    source: &[u8],
    tree: Oid,
    parents: &[Oid],
    id: Oid,
) -> Result<Vec<u8>, ProjectionError> {
    let commit =
        parse_commit(source).map_err(|source| ProjectionError::InvalidCommit { id, source })?;
    let message_start = source.len() - commit.message.len();
    let headers_end = message_start
        .checked_sub(1)
        .expect("validated commit has a blank-line separator");
    let mut output = Vec::with_capacity(source.len());
    let mut parent_index = 0;
    for (index, header) in commit.headers.iter().enumerate() {
        let end = commit
            .headers
            .get(index + 1)
            .map_or(headers_end, |next| next.offset);
        match header.name {
            b"tree" => write_oid_header(&mut output, b"tree", tree),
            b"parent" => {
                let parent = parents
                    .get(parent_index)
                    .expect("rewritten parent count matches canonical parents");
                write_oid_header(&mut output, b"parent", *parent);
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

fn write_oid_header(output: &mut Vec<u8>, name: &[u8], id: Oid) {
    output.extend_from_slice(name);
    output.push(b' ');
    output.extend_from_slice(oid_hex(id).as_bytes());
    output.push(b'\n');
}

fn scan_tree(
    git_repo: &gix::Repository,
    path: &RepoPath,
    tree_id: Oid,
    matcher: &GitIgnoreFile,
    leaked: &mut Vec<RepoPathBuf>,
) -> Result<(), ProjectionError> {
    let bytes = read_object(git_repo, tree_id, GitObjectKind::Tree)?;
    for entry in raw_tree_entries(tree_id, &bytes)? {
        let entry_path = join_component(path, entry.name)?;
        if entry.name == DSPRIVATE.as_bytes() {
            leaked.push(entry_path);
            continue;
        }
        match entry.kind {
            TreeEntryKind::Tree if matcher.matches_dir(&entry_path) => leaked.push(entry_path),
            TreeEntryKind::Tree => {
                scan_tree(git_repo, &entry_path, entry.oid, matcher, leaked)?;
            }
            _ if matcher.matches_file(&entry_path) => leaked.push(entry_path),
            _ => {}
        }
    }
    Ok(())
}

async fn resolve_hidden_set(
    store: &Arc<Store>,
    commit_id: &CommitId,
    commit: &BackendCommit,
    cache: &mut HiddenSetCache,
) -> Result<HiddenSet, ProjectionError> {
    let merged_tree = MergedTree::new(
        store.clone(),
        commit.root_tree.clone(),
        ConflictLabels::from_merge(commit.conflict_labels.clone()),
    );
    resolve_hidden_set_for_tree(store, commit_id, &merged_tree, cache).await
}

pub(crate) async fn resolve_hidden_set_for_tree(
    store: &Arc<Store>,
    context_id: &CommitId,
    merged_tree: &MergedTree,
    cache: &mut HiddenSetCache,
) -> Result<HiddenSet, ProjectionError> {
    let mut candidate_paths = BTreeSet::new();
    for tree_id in merged_tree.tree_ids().iter() {
        for path in
            collect_dsprivate_paths(store, RepoPath::root(), tree_id, &mut cache.trees).await?
        {
            candidate_paths.insert(path);
        }
    }

    let mut files = BTreeMap::new();
    let canonical_id = oid_from_commit_id(context_id)?;
    for path in &candidate_paths {
        let value = merged_tree
            .path_value(path)
            .await?
            .into_resolved()
            .map_err(|_| ProjectionError::ConflictedDsprivate {
                commit_id: canonical_id,
                path: path.clone(),
            })?;
        let Some(TreeValue::File { id, .. }) = value else {
            return Err(ProjectionError::InvalidDsprivateEntry {
                commit_id: canonical_id,
                path: path.clone(),
            });
        };
        files.insert(path.clone(), id);
    }

    let mut matcher = GitIgnoreFile::empty();
    for (path, id) in &files {
        let bytes = read_hidden_blob(store, path, id, &mut cache.blobs, context_id).await?;
        matcher = matcher
            .chain(
                path.parent().expect(".dsprivate has a parent directory"),
                Path::new(DSPRIVATE),
                &bytes,
            )
            .expect("in-memory gitignore patterns have no parse errors");
    }
    Ok(HiddenSet {
        identity: hidden_set_identity(&files),
        matcher,
        files,
    })
}

fn collect_dsprivate_paths<'a>(
    store: &'a Arc<Store>,
    path: &'a RepoPath,
    tree_id: &'a TreeId,
    cache: &'a mut BTreeMap<TreeId, Arc<Vec<RepoPathBuf>>>,
) -> Pin<Box<dyn Future<Output = Result<Vec<RepoPathBuf>, ProjectionError>> + 'a>> {
    Box::pin(async move {
        if let Some(relative_paths) = cache.get(tree_id) {
            return Ok(relative_paths
                .iter()
                .map(|relative| join_repo_paths(path, relative))
                .collect());
        }
        let tree = store.get_tree(path.to_owned(), tree_id).await?;
        let mut relative_paths = Vec::new();
        for entry in tree.entries_non_recursive() {
            let relative = RepoPathBuf::from_internal_string(entry.name().as_internal_str())
                .expect("jj tree entry names are repository paths");
            if entry.name().as_internal_str() == DSPRIVATE {
                relative_paths.push(relative);
            } else if let TreeValue::Tree(child_id) = entry.value() {
                let child_path = path.join(entry.name());
                for child_relative in
                    collect_dsprivate_paths(store, &child_path, child_id, cache).await?
                {
                    let child_relative = child_relative
                        .strip_prefix(&child_path)
                        .expect("child path is beneath its directory");
                    relative_paths.push(join_repo_paths(&relative, child_relative));
                }
            }
        }
        relative_paths.sort_unstable();
        let relative_paths = Arc::new(relative_paths);
        cache.insert(tree_id.clone(), relative_paths.clone());
        Ok(relative_paths
            .iter()
            .map(|relative| join_repo_paths(path, relative))
            .collect())
    })
}

async fn read_hidden_blob(
    store: &Arc<Store>,
    path: &RepoPath,
    id: &FileId,
    cache: &mut BTreeMap<FileId, Arc<Vec<u8>>>,
    commit_id: &CommitId,
) -> Result<Arc<Vec<u8>>, ProjectionError> {
    if let Some(bytes) = cache.get(id) {
        return Ok(bytes.clone());
    }
    let mut bytes = Vec::new();
    let contents = store.read_file(path, id).await?;
    jj_lib::file_util::copy_async_to_sync(contents, &mut bytes)
        .await
        .map_err(|source| ProjectionError::ReadDsprivate {
            commit_id: oid_from_commit_id(commit_id).expect("Git commit IDs are 20 bytes"),
            path: path.to_owned(),
            source,
        })?;
    let bytes = Arc::new(bytes);
    cache.insert(id.clone(), bytes.clone());
    Ok(bytes)
}

fn hidden_set_identity(files: &BTreeMap<RepoPathBuf, FileId>) -> HiddenSetIdentity {
    if files.is_empty() {
        return HiddenSetIdentity(None);
    }
    let mut hasher = Blake2b512::new();
    hasher.update(HIDDEN_SET_DOMAIN);
    for (path, id) in files {
        hash_hidden_file(&mut hasher, path, id);
    }
    HiddenSetIdentity(Some(hasher.finalize().into()))
}

fn hash_hidden_file(hasher: &mut Blake2b512, path: &RepoPath, id: &FileId) {
    let path = path.as_internal_file_string();
    hasher.update((path.len() as u64).to_le_bytes());
    hasher.update(path.as_bytes());
    hasher.update(id.as_bytes());
}

fn join_repo_paths(prefix: &RepoPath, suffix: &RepoPath) -> RepoPathBuf {
    if prefix.is_root() {
        return suffix.to_owned();
    }
    if suffix.is_root() {
        return prefix.to_owned();
    }
    RepoPathBuf::from_internal_string(format!(
        "{}/{}",
        prefix.as_internal_file_string(),
        suffix.as_internal_file_string()
    ))
    .expect("joined repository paths are valid")
}

fn join_component(path: &RepoPath, name: &[u8]) -> Result<RepoPathBuf, ProjectionError> {
    let name = std::str::from_utf8(name).map_err(|_| ProjectionError::NonUtf8Path {
        parent: path.to_owned(),
        name: name.to_vec(),
    })?;
    let component =
        jj_lib::repo_path::RepoPathComponentBuf::new(name.to_owned()).map_err(|_| {
            ProjectionError::InvalidPath {
                parent: path.to_owned(),
                name: name.to_owned(),
            }
        })?;
    Ok(path.join(&component))
}

fn is_dsprivate_path(path: &RepoPath) -> bool {
    path.components()
        .next_back()
        .is_some_and(|component| component.as_internal_str() == DSPRIVATE)
}

fn oid_from_commit_id(id: &CommitId) -> Result<Oid, ProjectionError> {
    Oid::from_bytes(id.as_bytes()).ok_or_else(|| ProjectionError::InvalidBackendCommitId {
        bytes: id.as_bytes().to_vec(),
    })
}

fn read_object(
    git_repo: &gix::Repository,
    id: Oid,
    expected: GitObjectKind,
) -> Result<Vec<u8>, ProjectionError> {
    let object = git_repo
        .find_object(gix::ObjectId::from_bytes_or_panic(&id.0))
        .map_err(|source| ProjectionError::ReadObject {
            id,
            source: Box::new(source),
        })?;
    if object.kind != expected {
        return Err(ProjectionError::WrongObjectKind {
            id,
            expected,
            actual: object.kind,
        });
    }
    Ok(object.data.clone())
}

fn write_object(
    git_repo: &gix::Repository,
    kind: GitObjectKind,
    bytes: &[u8],
) -> Result<Oid, ProjectionError> {
    let id = git_repo
        .objects
        .write_buf(kind, bytes)
        .map_err(|source| ProjectionError::WriteObject { kind, source })?;
    Oid::from_bytes(id.as_bytes()).ok_or(ProjectionError::UnexpectedObjectIdLength {
        actual: id.as_bytes().len(),
    })
}

fn oid_hex(id: Oid) -> String {
    crate::hex(&id.0)
}

#[derive(Debug, Error)]
pub enum ProjectionError {
    #[error(
        "canonical commit {canonical_id:?} is already mapped to {existing:?}, not {proposed:?}"
    )]
    ConflictingMapping {
        canonical_id: Oid,
        existing: Oid,
        proposed: Oid,
    },
    #[error("cycle in commit graph at {0:?}")]
    CommitCycle(Oid),
    #[error("missing staged canonical commit {0:?}")]
    MissingStagedCommit(Oid),
    #[error("canonical parent {0:?} has no projection result")]
    MissingParentProjection(Oid),
    #[error("cannot project conflicted commit {0:?}")]
    ConflictedCommit(Oid),
    #[error("cannot project commit {commit_id:?}: {path:?} is conflicted")]
    ConflictedDsprivate { commit_id: Oid, path: RepoPathBuf },
    #[error("cannot project commit {commit_id:?}: {path:?} is not a regular file")]
    InvalidDsprivateEntry { commit_id: Oid, path: RepoPathBuf },
    #[error("cannot read {path:?} in commit {commit_id:?}")]
    ReadDsprivate {
        commit_id: Oid,
        path: RepoPathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot project Gitlink at {path:?}")]
    Gitlink { path: RepoPathBuf },
    #[error("public commit {public_id:?} leaks hidden paths {leaked:?}")]
    HiddenPathLeak {
        public_id: Oid,
        leaked: Vec<RepoPathBuf>,
    },
    #[error("commit {id:?} is invalid")]
    InvalidCommit {
        id: Oid,
        #[source]
        source: CommitError,
    },
    #[error("tree {id:?} is invalid")]
    InvalidTree {
        id: Oid,
        #[source]
        source: TreeError,
    },
    #[error("failed to read Git object {id:?}")]
    ReadObject {
        id: Oid,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("Git object {id:?} is {actual:?}, expected {expected:?}")]
    WrongObjectKind {
        id: Oid,
        expected: GitObjectKind,
        actual: GitObjectKind,
    },
    #[error("failed to write {kind:?} object")]
    WriteObject {
        kind: GitObjectKind,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("Git object id has {actual} bytes, expected 20")]
    UnexpectedObjectIdLength { actual: usize },
    #[error("jj backend commit id is not a 20-byte Git id: {bytes:?}")]
    InvalidBackendCommitId { bytes: Vec<u8> },
    #[error("tree entry name is not UTF-8 below {parent:?}: {name:?}")]
    NonUtf8Path { parent: RepoPathBuf, name: Vec<u8> },
    #[error("invalid tree entry name {name:?} below {parent:?}")]
    InvalidPath { parent: RepoPathBuf, name: String },
    #[error(transparent)]
    Backend(#[from] jj_lib::backend::BackendError),
}
