//! Rebuildable Git projection for a native machine repository.
//!
//! The sidecar contains only public Git objects. Canonical jj objects and all
//! durable projection receipts remain outside it.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use blake2::{Blake2b512, Digest as _};
use jj_lib::backend::{
    Commit as BackendCommit, CommitId, CopyId, FileId, SymlinkId, Tree as BackendTree, TreeId,
    TreeValue,
};
use jj_lib::conflict_labels::ConflictLabels;
use jj_lib::git_backend::GitBackend;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo_path::{RepoPath, RepoPathBuf, RepoPathComponentBuf};
use jj_lib::settings::UserSettings;
use jj_lib::signing::Signer;
use jj_lib::store::Store;
use jj_lib::tree_merge::MergeOptions;
use thiserror::Error;

const STORE_DIR: &str = "store";
const DSPRIVATE: &str = ".dsprivate";

/// Current import safety limits. These are sanity ceilings, not transaction
/// bounds or production quotas.
pub const MAX_IMPORT_HEADS: usize = 256;
pub const MAX_IMPORT_TOTAL_COMMITS: usize = 1_048_576;
pub const MAX_IMPORT_TREE_DEPTH: usize = 256;
pub const MAX_IMPORT_TREE_ENTRIES: usize = 8_192;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitMapping {
    pub canonical_id: CommitId,
    pub git_id: CommitId,
}

#[derive(Clone, Debug, Default)]
pub struct ExportMappings {
    by_canonical: BTreeMap<CommitId, CommitId>,
}

impl ExportMappings {
    pub fn from_rows(
        rows: impl IntoIterator<Item = CommitMapping>,
    ) -> Result<Self, ProjectionError> {
        let mut mappings = Self::default();
        for row in rows {
            record_mapping(
                &mut mappings.by_canonical,
                row.canonical_id,
                row.git_id,
                "canonical commit",
            )?;
        }
        Ok(mappings)
    }
}

#[derive(Clone, Debug, Default)]
pub struct ImportMappings {
    by_git: BTreeMap<CommitId, CommitId>,
}

impl ImportMappings {
    pub fn from_rows(
        rows: impl IntoIterator<Item = CommitMapping>,
    ) -> Result<Self, ProjectionError> {
        let mut mappings = Self::default();
        for row in rows {
            record_mapping(
                &mut mappings.by_git,
                row.git_id,
                row.canonical_id,
                "Git commit",
            )?;
        }
        Ok(mappings)
    }

    pub fn rows(&self) -> impl Iterator<Item = CommitMapping> + '_ {
        self.by_git
            .iter()
            .map(|(git_id, canonical_id)| CommitMapping {
                canonical_id: canonical_id.clone(),
                git_id: git_id.clone(),
            })
    }
}

fn record_mapping(
    mappings: &mut BTreeMap<CommitId, CommitId>,
    source_id: CommitId,
    target_id: CommitId,
    source_name: &'static str,
) -> Result<bool, ProjectionError> {
    if let Some(existing) = mappings.get(&source_id) {
        if existing != &target_id {
            return Err(ProjectionError::ConflictingMapping {
                source_name,
                source_id,
                existing: existing.clone(),
                proposed: target_id,
            });
        }
        return Ok(false);
    }
    mappings.insert(source_id, target_id);
    Ok(true)
}

const HIDDEN_SET_DOMAIN: &[u8] = b"devspace-hidden-set-v1";
const HIDDEN_CHAIN_DOMAIN: &[u8] = b"devspace-hidden-chain-v1";

/// Canonical identity of every `.dsprivate` path and blob in a commit.
///
/// The projection journal binds a public object to the identity of the hidden
/// set under which it was exported.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HiddenSetIdentity(Option<[u8; 64]>);

impl HiddenSetIdentity {
    pub fn from_projection_id(id: Option<[u8; 64]>) -> Self {
        Self(id)
    }

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

    pub(crate) fn hides_file(&self, path: &RepoPath) -> bool {
        if path.components().next_back().is_some_and(is_dsprivate)
            || self.matcher.matches_file(path)
        {
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

#[derive(Default)]
pub(crate) struct HiddenSetCache {
    blobs: BTreeMap<FileId, Arc<Vec<u8>>>,
    trees: BTreeMap<TreeId, Arc<Vec<RepoPathBuf>>>,
}

#[derive(Default)]
struct TranslationCache {
    hidden_sets: HiddenSetCache,
    trees: BTreeMap<(HiddenChainIdentity, RepoPathBuf, TreeId), TreeId>,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct HiddenChainIdentity([u8; 64]);

#[derive(Clone)]
struct HiddenChain {
    identity: HiddenChainIdentity,
    hasher: Blake2b512,
    matcher: Arc<GitIgnoreFile>,
}

struct TreeCopyContext<'a> {
    source: &'a Arc<Store>,
    target: &'a Arc<Store>,
    hidden_set: Option<&'a HiddenSet>,
    direction: TranslationDirection,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExportResult {
    pub git_heads: Vec<CommitId>,
    pub new_mappings: Vec<CommitMapping>,
}

/// Controls how accepted canonical-to-Git mappings are handled when their Git
/// objects are absent from the rebuildable sidecar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExportMode {
    /// Normal exports require every accepted mapping to name an existing Git
    /// object. Missing objects indicate a sidecar that must be repaired first.
    Strict,
    /// Journal replay may deterministically rebuild missing Git objects. The
    /// caller must validate the rebuilt head against the journaled Git ID.
    Replay,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportResult {
    pub canonical_heads: Vec<CommitId>,
    pub new_mappings: Vec<CommitMapping>,
    pub reached_mappings: Vec<CommitMapping>,
}

pub struct GitProjection {
    root: PathBuf,
    store: Arc<Store>,
    git_repo_path: PathBuf,
}

impl GitProjection {
    pub fn init(root: impl AsRef<Path>, settings: &UserSettings) -> Result<Self, ProjectionError> {
        let root = root.as_ref();
        fs::create_dir_all(root).map_err(|source| ProjectionError::CreateSidecar {
            path: root.to_owned(),
            source,
        })?;
        let store_path = root.join(STORE_DIR);
        fs::create_dir(&store_path).map_err(|source| ProjectionError::CreateSidecar {
            path: store_path.clone(),
            source,
        })?;
        let backend = GitBackend::init_internal(settings, &store_path)
            .map_err(|source| ProjectionError::Setup(source))?;
        Self::from_backend(root, settings, backend)
    }

    pub fn open(root: impl AsRef<Path>, settings: &UserSettings) -> Result<Self, ProjectionError> {
        let root = root.as_ref();
        let backend = GitBackend::load(settings, &root.join(STORE_DIR))
            .map_err(|source| ProjectionError::Setup(source))?;
        Self::from_backend(root, settings, backend)
    }

    fn from_backend(
        root: &Path,
        settings: &UserSettings,
        backend: GitBackend,
    ) -> Result<Self, ProjectionError> {
        let git_repo_path = backend.git_repo_path().to_owned();
        let signer = Signer::from_settings(settings)
            .map_err(|source| ProjectionError::Setup(Box::new(source)))?;
        let merge_options = MergeOptions::from_settings(settings)
            .map_err(|source| ProjectionError::Setup(Box::new(source)))?;
        Ok(Self {
            root: root.to_owned(),
            store: Store::new(Box::new(backend), signer, merge_options),
            git_repo_path,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn git_repo_path(&self) -> &Path {
        &self.git_repo_path
    }

    pub async fn export_reachable(
        &self,
        canonical_store: &Arc<Store>,
        canonical_heads: &[CommitId],
        mappings: &mut ExportMappings,
        mode: ExportMode,
    ) -> Result<ExportResult, ProjectionError> {
        let translated = translate_reachable(
            canonical_store,
            &self.store,
            canonical_heads,
            &mut mappings.by_canonical,
            None,
            None,
            TranslationDirection::Export(mode),
        )
        .await?;
        Ok(ExportResult {
            git_heads: translated.target_heads,
            new_mappings: translated
                .new_pairs
                .into_iter()
                .map(|(canonical_id, git_id)| CommitMapping {
                    canonical_id,
                    git_id,
                })
                .collect(),
        })
    }

    pub async fn import_reachable(
        &self,
        canonical_store: &Arc<Store>,
        git_heads: &[CommitId],
        mappings: &mut ImportMappings,
    ) -> Result<ImportResult, ProjectionError> {
        self.import_reachable_with_stops(
            canonical_store,
            git_heads,
            &ImportMappings::default(),
            mappings,
        )
        .await
    }

    pub async fn import_reachable_with_stops(
        &self,
        canonical_store: &Arc<Store>,
        git_heads: &[CommitId],
        stop_at: &ImportMappings,
        mappings: &mut ImportMappings,
    ) -> Result<ImportResult, ProjectionError> {
        if git_heads.len() > MAX_IMPORT_HEADS {
            return Err(ProjectionError::ImportHeadLimit {
                actual: git_heads.len(),
                limit: MAX_IMPORT_HEADS,
            });
        }
        let expected = mappings.by_git.clone();
        let mut computed = BTreeMap::new();
        let translated = translate_reachable(
            &self.store,
            canonical_store,
            git_heads,
            &mut computed,
            Some(&stop_at.by_git),
            Some(&expected),
            TranslationDirection::Import,
        )
        .await?;
        for (git_id, canonical_id) in computed {
            record_mapping(&mut mappings.by_git, git_id, canonical_id, "Git commit")?;
        }
        Ok(ImportResult {
            canonical_heads: translated.target_heads,
            new_mappings: translated
                .new_pairs
                .into_iter()
                .map(|(git_id, canonical_id)| CommitMapping {
                    canonical_id,
                    git_id,
                })
                .collect(),
            reached_mappings: translated
                .reached_pairs
                .into_iter()
                .map(|(git_id, canonical_id)| CommitMapping {
                    canonical_id,
                    git_id,
                })
                .collect(),
        })
    }

    pub async fn hidden_set_for_commit(
        &self,
        canonical_store: &Arc<Store>,
        canonical_id: &CommitId,
    ) -> Result<HiddenSet, ProjectionError> {
        let canonical = canonical_store.backend().read_commit(canonical_id).await?;
        resolve_hidden_set(
            canonical_store,
            canonical_id,
            &canonical,
            &mut HiddenSetCache::default(),
        )
        .await
    }

    /// Defense-in-depth check for the selected public tip immediately before
    /// Git transport.
    pub async fn scan_hidden_paths(
        &self,
        git_head: &CommitId,
        hidden_set: &HiddenSet,
    ) -> Result<Vec<RepoPathBuf>, ProjectionError> {
        scan_hidden_paths(&self.store, git_head, hidden_set).await
    }
}

struct TranslationResult {
    target_heads: Vec<CommitId>,
    new_pairs: Vec<(CommitId, CommitId)>,
    reached_pairs: Vec<(CommitId, CommitId)>,
}

#[derive(Clone, Copy)]
enum TranslationDirection {
    Export(ExportMode),
    Import,
}

enum AncestryVisit {
    Enter(CommitId),
    Exit(CommitId),
}

pub(crate) trait AncestryStops {
    async fn contains(&mut self, id: &CommitId) -> Result<bool, ProjectionError>;
}

pub(crate) struct NoAncestryStops;

impl AncestryStops for NoAncestryStops {
    async fn contains(&mut self, _id: &CommitId) -> Result<bool, ProjectionError> {
        Ok(false)
    }
}

pub(crate) struct AncestryWalk {
    pub(crate) reached: BTreeSet<CommitId>,
    parent_first: Vec<(CommitId, BackendCommit)>,
}

pub(crate) async fn walk_ancestry_bounded(
    store: &Arc<Store>,
    heads: &[CommitId],
    stops: &mut impl AncestryStops,
    limit: usize,
) -> Result<AncestryWalk, ProjectionError> {
    let mut states = BTreeMap::<CommitId, bool>::new();
    let mut reached = BTreeSet::new();
    let mut commits = BTreeMap::<CommitId, BackendCommit>::new();
    let mut parent_first = Vec::new();
    let mut read_count = 0;

    for head in heads {
        let mut stack = vec![AncestryVisit::Enter(head.clone())];
        while let Some(visit) = stack.pop() {
            match visit {
                AncestryVisit::Enter(id) => {
                    if id == *store.root_commit_id() {
                        continue;
                    }
                    match states.get(&id) {
                        Some(true) => continue,
                        Some(false) => return Err(ProjectionError::CommitCycle(id)),
                        None => {}
                    }
                    reached.insert(id.clone());
                    if stops.contains(&id).await? {
                        states.insert(id, true);
                        continue;
                    }
                    if read_count >= limit {
                        return Err(ProjectionError::ImportCommitLimit {
                            actual: read_count + 1,
                            limit,
                        });
                    }
                    let commit = store.backend().read_commit(&id).await?;
                    read_count += 1;
                    states.insert(id.clone(), false);
                    commits.insert(id.clone(), commit.clone());
                    stack.push(AncestryVisit::Exit(id));
                    for parent in commit.parents.iter().rev() {
                        stack.push(AncestryVisit::Enter(parent.clone()));
                    }
                }
                AncestryVisit::Exit(id) => {
                    if states.get(&id) == Some(&true) {
                        continue;
                    }
                    let commit = commits
                        .remove(&id)
                        .ok_or_else(|| ProjectionError::MissingStagedCommit(id.clone()))?;
                    states.insert(id.clone(), true);
                    parent_first.push((id, commit));
                }
            }
        }
    }
    Ok(AncestryWalk {
        reached,
        parent_first,
    })
}

struct TranslationStops<'a> {
    target: &'a Arc<Store>,
    source_to_target: &'a mut BTreeMap<CommitId, CommitId>,
    stop_at: Option<&'a BTreeMap<CommitId, CommitId>>,
    reached_pairs: &'a mut BTreeMap<CommitId, CommitId>,
    direction: TranslationDirection,
}

impl AncestryStops for TranslationStops<'_> {
    async fn contains(&mut self, source_id: &CommitId) -> Result<bool, ProjectionError> {
        if let Some(target_id) = self.stop_at.and_then(|stops| stops.get(source_id)) {
            self.target.backend().read_commit(target_id).await?;
            record_mapping(
                self.source_to_target,
                source_id.clone(),
                target_id.clone(),
                "Git commit stop",
            )?;
            self.reached_pairs
                .insert(source_id.clone(), target_id.clone());
            return Ok(true);
        }
        let Some(target_id) = self.source_to_target.get(source_id).cloned() else {
            return Ok(false);
        };
        match self.target.backend().read_commit(&target_id).await {
            Ok(_) => {
                self.reached_pairs.insert(source_id.clone(), target_id);
                Ok(true)
            }
            Err(jj_lib::backend::BackendError::ObjectNotFound { .. }) => match self.direction {
                TranslationDirection::Export(ExportMode::Strict) => {
                    Err(ProjectionError::MissingMappedObject {
                        canonical_id: source_id.clone(),
                        git_id: target_id,
                    })
                }
                TranslationDirection::Export(ExportMode::Replay) => Ok(false),
                TranslationDirection::Import => Ok(false),
            },
            Err(source) => Err(source.into()),
        }
    }
}

async fn translate_reachable(
    source: &Arc<Store>,
    target: &Arc<Store>,
    source_heads: &[CommitId],
    source_to_target: &mut BTreeMap<CommitId, CommitId>,
    stop_at: Option<&BTreeMap<CommitId, CommitId>>,
    expected: Option<&BTreeMap<CommitId, CommitId>>,
    direction: TranslationDirection,
) -> Result<TranslationResult, ProjectionError> {
    let mut cache = TranslationCache::default();
    let mut new_pairs = Vec::new();
    let mut reached_pairs = BTreeMap::new();
    let limit = match direction {
        TranslationDirection::Import => MAX_IMPORT_TOTAL_COMMITS,
        TranslationDirection::Export(_) => usize::MAX,
    };
    let walk = walk_ancestry_bounded(
        source,
        source_heads,
        &mut TranslationStops {
            target,
            source_to_target,
            stop_at,
            reached_pairs: &mut reached_pairs,
            direction,
        },
        limit,
    )
    .await?;

    for (source_id, source_commit) in walk.parent_first {
        let parents = source_commit
            .parents
            .iter()
            .map(|parent_id| mapped_commit_id(source, target, source_to_target, parent_id))
            .collect::<Result<Vec<_>, _>>()?;
        let hidden_set = match direction {
            TranslationDirection::Export(_) => Some(
                resolve_hidden_set(source, &source_id, &source_commit, &mut cache.hidden_sets)
                    .await?,
            ),
            TranslationDirection::Import => None,
        };
        let source_tree_id = source_commit
            .root_tree
            .as_resolved()
            .ok_or_else(|| ProjectionError::ConflictedCommit(source_id.clone()))?;
        let context = TreeCopyContext {
            source,
            target,
            hidden_set: hidden_set.as_ref(),
            direction,
        };
        let target_tree_id = copy_tree(
            &context,
            RepoPath::root(),
            source_tree_id,
            &HiddenChain::empty(),
            &mut cache,
            0,
        )
        .await?;
        let target_commit = BackendCommit {
            parents,
            predecessors: Vec::new(),
            root_tree: jj_lib::merge::Merge::resolved(target_tree_id),
            conflict_labels: jj_lib::merge::Merge::resolved(String::new()),
            change_id: source_commit.change_id,
            description: source_commit.description,
            author: source_commit.author,
            committer: source_commit.committer,
            secure_sig: None,
        };
        let target_id = target.write_commit(target_commit, None).await?.id().clone();
        if let Some(expected_id) = expected.and_then(|rows| rows.get(&source_id))
            && expected_id != &target_id
        {
            return Err(ProjectionError::ConflictingMapping {
                source_name: "Git commit",
                source_id,
                existing: expected_id.clone(),
                proposed: target_id,
            });
        }
        if record_mapping(
            source_to_target,
            source_id.clone(),
            target_id.clone(),
            "source commit",
        )? {
            new_pairs.push((source_id.clone(), target_id));
        }
    }

    let target_heads = source_heads
        .iter()
        .map(|source_id| mapped_commit_id(source, target, source_to_target, source_id))
        .collect::<Result<_, _>>()?;
    Ok(TranslationResult {
        target_heads,
        new_pairs,
        reached_pairs: reached_pairs.into_iter().collect(),
    })
}

async fn scan_hidden_paths(
    store: &Arc<Store>,
    commit_id: &CommitId,
    hidden_set: &HiddenSet,
) -> Result<Vec<RepoPathBuf>, ProjectionError> {
    let commit = store.backend().read_commit(commit_id).await?;
    let tree_id = commit
        .root_tree
        .as_resolved()
        .ok_or_else(|| ProjectionError::ConflictedProjectedCommit(commit_id.clone()))?;
    let mut leaked = Vec::new();
    scan_tree(
        store,
        RepoPath::root(),
        tree_id,
        &hidden_set.matcher,
        &mut leaked,
    )
    .await?;
    Ok(leaked)
}

fn scan_tree<'a>(
    store: &'a Arc<Store>,
    path: &'a RepoPath,
    tree_id: &'a TreeId,
    matcher: &'a GitIgnoreFile,
    leaked: &'a mut Vec<RepoPathBuf>,
) -> Pin<Box<dyn Future<Output = Result<(), ProjectionError>> + 'a>> {
    Box::pin(async move {
        let tree = store.get_tree(path.to_owned(), tree_id).await?;
        for entry in tree.entries_non_recursive() {
            let entry_path = path.join(entry.name());
            if is_dsprivate(entry.name()) {
                leaked.push(entry_path);
                continue;
            }
            match entry.value() {
                TreeValue::Tree(id) => {
                    if matcher.matches_dir(&entry_path) {
                        leaked.push(entry_path);
                    } else {
                        scan_tree(store, &entry_path, id, matcher, leaked).await?;
                    }
                }
                _ if matcher.matches_file(&entry_path) => {
                    leaked.push(entry_path);
                }
                _ => {}
            }
        }
        Ok(())
    })
}

fn mapped_commit_id(
    source: &Arc<Store>,
    target: &Arc<Store>,
    source_to_target: &BTreeMap<CommitId, CommitId>,
    source_id: &CommitId,
) -> Result<CommitId, ProjectionError> {
    if source_id == source.root_commit_id() {
        Ok(target.root_commit_id().clone())
    } else {
        source_to_target
            .get(source_id)
            .cloned()
            .ok_or_else(|| ProjectionError::MissingParentMapping(source_id.clone()))
    }
}

enum LeafCopy {
    File {
        name: RepoPathComponentBuf,
        path: RepoPathBuf,
        id: FileId,
        executable: bool,
    },
    Symlink {
        name: RepoPathComponentBuf,
        path: RepoPathBuf,
        id: SymlinkId,
    },
}

impl LeafCopy {
    async fn copy(
        self,
        source: &Arc<Store>,
        target: &Arc<Store>,
    ) -> Result<(RepoPathComponentBuf, TreeValue), ProjectionError> {
        match self {
            Self::File {
                name,
                path,
                id,
                executable,
            } => {
                let mut contents = source.read_file(&path, &id).await?;
                let target_id = target.write_file(&path, &mut contents).await?;
                Ok((
                    name,
                    TreeValue::File {
                        id: target_id,
                        executable,
                        copy_id: CopyId::placeholder(),
                    },
                ))
            }
            Self::Symlink { name, path, id } => {
                let value = source.read_symlink(&path, &id).await?;
                let target_id = target.write_symlink(&path, &value).await?;
                Ok((name, TreeValue::Symlink(target_id)))
            }
        }
    }
}

fn copy_tree<'a>(
    context: &'a TreeCopyContext<'a>,
    path: &'a RepoPath,
    source_tree_id: &'a TreeId,
    inherited_chain: &'a HiddenChain,
    cache: &'a mut TranslationCache,
    depth: usize,
) -> Pin<Box<dyn Future<Output = Result<TreeId, ProjectionError>> + 'a>> {
    Box::pin(async move {
        if matches!(context.direction, TranslationDirection::Import)
            && depth > MAX_IMPORT_TREE_DEPTH
        {
            return Err(ProjectionError::ImportTreeDepthLimit {
                path: path.to_owned(),
                depth,
                limit: MAX_IMPORT_TREE_DEPTH,
            });
        }
        if source_tree_id == context.source.empty_tree_id() {
            return Ok(context.target.empty_tree_id().clone());
        }

        let source_tree = context
            .source
            .get_tree(path.to_owned(), source_tree_id)
            .await?;
        let mut chain = inherited_chain.clone();
        if let Some(hidden_set) = context.hidden_set {
            let dsprivate_path = path.join(
                &RepoPathComponentBuf::new(DSPRIVATE.to_owned())
                    .expect(".dsprivate is a valid path component"),
            );
            if let Some(id) = hidden_set.files.get(&dsprivate_path) {
                let bytes = cache
                    .hidden_sets
                    .blobs
                    .get(id)
                    .expect("hidden-set resolution caches every policy blob");
                chain = chain.chain(path, id, bytes);
            }
        }
        let cache_key = (
            chain.identity.clone(),
            path.to_owned(),
            source_tree_id.clone(),
        );
        if let Some(target_tree_id) = cache.trees.get(&cache_key) {
            return Ok(target_tree_id.clone());
        }

        let source_entries = source_tree.entries_non_recursive().collect::<Vec<_>>();
        if matches!(context.direction, TranslationDirection::Import)
            && source_entries.len() > MAX_IMPORT_TREE_ENTRIES
        {
            return Err(ProjectionError::ImportTreeWidthLimit {
                path: path.to_owned(),
                actual: source_entries.len(),
                limit: MAX_IMPORT_TREE_ENTRIES,
            });
        }
        let mut target_entries = Vec::new();
        let mut leaf_copies = Vec::new();
        for entry in source_entries {
            let entry_path = path.join(entry.name());
            if context.hidden_set.is_some() {
                let excluded = match entry.value() {
                    TreeValue::Tree(_) => chain.matcher.matches_dir(&entry_path),
                    _ => is_dsprivate(entry.name()) || chain.matcher.matches_file(&entry_path),
                };
                if excluded {
                    continue;
                }
            }
            let target_value = match entry.value() {
                TreeValue::File {
                    id,
                    executable,
                    copy_id: _,
                } => {
                    leaf_copies.push(LeafCopy::File {
                        name: entry.name().to_owned(),
                        path: entry_path,
                        id: id.clone(),
                        executable: *executable,
                    });
                    continue;
                }
                TreeValue::Symlink(id) => {
                    leaf_copies.push(LeafCopy::Symlink {
                        name: entry.name().to_owned(),
                        path: entry_path,
                        id: id.clone(),
                    });
                    continue;
                }
                TreeValue::Tree(id) => {
                    let target_id =
                        copy_tree(context, &entry_path, id, &chain, cache, depth + 1).await?;
                    if target_id == *context.target.empty_tree_id() {
                        continue;
                    }
                    TreeValue::Tree(target_id)
                }
                TreeValue::GitSubmodule(_) => {
                    return Err(ProjectionError::GitLink {
                        path: entry_path,
                        operation: context.direction.label(),
                    });
                }
            };
            target_entries.push((entry.name().to_owned(), target_value));
        }
        for leaf in leaf_copies {
            target_entries.push(leaf.copy(context.source, context.target).await?);
        }
        target_entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
        let target_tree = context
            .target
            .write_tree(path, BackendTree::from_sorted_entries(target_entries))
            .await?;
        let target_tree_id = target_tree.id().clone();
        cache.trees.insert(cache_key, target_tree_id.clone());
        Ok(target_tree_id)
    })
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
    for path in &candidate_paths {
        let value = merged_tree
            .path_value(path)
            .await?
            .into_resolved()
            .map_err(|_| ProjectionError::ConflictedDsprivate {
                commit_id: context_id.clone(),
                path: path.clone(),
            })?;
        let Some(TreeValue::File { id, .. }) = value else {
            return Err(ProjectionError::InvalidDsprivateEntry {
                commit_id: context_id.clone(),
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
                .expect("tree entry name is a repository path");
            if is_dsprivate(entry.name()) {
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
            commit_id: commit_id.clone(),
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

fn is_dsprivate(name: &jj_lib::repo_path::RepoPathComponent) -> bool {
    name.as_internal_str() == DSPRIVATE
}

#[derive(Debug, Error)]
pub enum ProjectionError {
    #[error("failed to create Git projection sidecar at {path}")]
    CreateSidecar {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to configure Git projection sidecar")]
    Setup(#[source] Box<dyn StdError + Send + Sync>),
    #[error("{source_name} {source_id} is already mapped to {existing}, not {proposed}")]
    ConflictingMapping {
        source_name: &'static str,
        source_id: CommitId,
        existing: CommitId,
        proposed: CommitId,
    },
    #[error("cycle in commit graph at {0}")]
    CommitCycle(CommitId),
    #[error("missing staged source commit {0}")]
    MissingStagedCommit(CommitId),
    #[error("source commit {0} has no translated parent")]
    MissingParentMapping(CommitId),
    #[error("cannot project conflicted commit {0}")]
    ConflictedCommit(CommitId),
    #[error("cannot inspect conflicted projected commit {0}")]
    ConflictedProjectedCommit(CommitId),
    #[error("cannot export commit {commit_id}: {path:?} is conflicted")]
    ConflictedDsprivate {
        commit_id: CommitId,
        path: RepoPathBuf,
    },
    #[error("cannot export commit {commit_id}: {path:?} is not a regular file")]
    InvalidDsprivateEntry {
        commit_id: CommitId,
        path: RepoPathBuf,
    },
    #[error("cannot read {path:?} in commit {commit_id}")]
    ReadDsprivate {
        commit_id: CommitId,
        path: RepoPathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "Git mapping {canonical_id} -> {git_id} violates that canonical commit's hidden set at {leaked:?}"
    )]
    StaleMapping {
        canonical_id: CommitId,
        git_id: CommitId,
        leaked: Vec<RepoPathBuf>,
    },
    #[error(
        "Git projection sidecar is missing mapped Git object {git_id} for canonical commit {canonical_id}; fetching the mapped bookmark repairs it"
    )]
    MissingMappedObject {
        canonical_id: CommitId,
        git_id: CommitId,
    },
    #[error("cannot {operation} Git link at {path:?}")]
    GitLink {
        path: RepoPathBuf,
        operation: &'static str,
    },
    #[error("Git import has {actual} input heads, exceeding the safety limit of {limit}")]
    ImportHeadLimit { actual: usize, limit: usize },
    #[error("Git import has {actual} commits, exceeding the safety limit of {limit}")]
    ImportCommitLimit { actual: usize, limit: usize },
    #[error("Git import tree depth {depth} at {path:?} exceeds the safety limit of {limit}")]
    ImportTreeDepthLimit {
        path: RepoPathBuf,
        depth: usize,
        limit: usize,
    },
    #[error(
        "Git import tree at {path:?} has {actual} entries, exceeding the safety limit of {limit}"
    )]
    ImportTreeWidthLimit {
        path: RepoPathBuf,
        actual: usize,
        limit: usize,
    },
    #[error(transparent)]
    Backend(#[from] jj_lib::backend::BackendError),
}

impl TranslationDirection {
    fn label(self) -> &'static str {
        match self {
            Self::Export(_) => "export",
            Self::Import => "import",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(bytes: &[u8]) -> Arc<GitIgnoreFile> {
        GitIgnoreFile::empty()
            .chain(RepoPath::root(), Path::new(DSPRIVATE), bytes)
            .unwrap()
    }

    fn repo_path(value: &str) -> &RepoPath {
        RepoPath::from_internal_string(value).unwrap()
    }

    #[test]
    fn dsprivate_gitignore_semantics_are_canonical() {
        let anchored = matcher(b"/root-only\nunanchored\n");
        assert!(anchored.matches_file(repo_path("root-only")));
        assert!(!anchored.matches_file(repo_path("nested/root-only")));
        assert!(anchored.matches_file(repo_path("unanchored")));
        assert!(anchored.matches_file(repo_path("nested/unanchored")));

        let globs = matcher(b"*.tmp\ncache/**/token\n");
        assert!(globs.matches_file(repo_path("scratch.tmp")));
        assert!(globs.matches_file(repo_path("nested/scratch.tmp")));
        assert!(globs.matches_file(repo_path("cache/token")));
        assert!(globs.matches_file(repo_path("cache/a/b/token")));
        assert!(!globs.matches_file(repo_path("cache/a/b/token.txt")));

        let directory = matcher(b"build/\n");
        assert!(directory.matches_dir(repo_path("build")));
        assert!(directory.matches_dir(repo_path("nested/build")));
        assert!(!directory.matches_file(repo_path("build")));

        let negation = matcher(b"*.pem\n!public.pem\nprivate/\n!private/keep.pem\n");
        assert!(negation.matches_file(repo_path("secret.pem")));
        assert!(!negation.matches_file(repo_path("public.pem")));
        assert!(negation.matches_dir(repo_path("private")));
        assert!(!negation.matches_file(repo_path("private/keep.pem")));

        let syntax = matcher(b"# comment\n\n\\#literal\n\\!literal\n");
        assert!(!syntax.matches_file(repo_path("comment")));
        assert!(syntax.matches_file(repo_path("#literal")));
        assert!(syntax.matches_file(repo_path("!literal")));
    }

    #[test]
    fn hidden_set_identity_encoding_is_canonical() {
        let files = BTreeMap::from([
            (
                RepoPathBuf::from_internal_string(".dsprivate").unwrap(),
                FileId::new(vec![0x11; 64]),
            ),
            (
                RepoPathBuf::from_internal_string("sub/.dsprivate").unwrap(),
                FileId::new(vec![0x22; 64]),
            ),
        ]);
        let digest = hidden_set_identity(&files).to_projection_id().unwrap();
        let hex = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            hex,
            "4896563e1c9edb27e10b76091cf6b552541340818fbc2ce3d04d3674e8b9e4a8\
             ee60ce371af0bac792c3dc9bac6a506fca7973252e1439f7baeb1c5a5552cfbd"
        );
    }
}
