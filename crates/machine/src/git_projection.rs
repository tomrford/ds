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

use devspace_kernel::hidden::HiddenPathSet;
use jj_lib::backend::{
    Commit as BackendCommit, CommitId, CopyId, FileId, SymlinkId, Tree as BackendTree, TreeId,
    TreeValue,
};
use jj_lib::git_backend::GitBackend;
use jj_lib::repo_path::{RepoPath, RepoPathBuf, RepoPathComponentBuf};
use jj_lib::settings::UserSettings;
use jj_lib::signing::Signer;
use jj_lib::store::Store;
use jj_lib::tree_merge::MergeOptions;
use thiserror::Error;

const STORE_DIR: &str = "store";

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

    pub fn rows(&self) -> impl Iterator<Item = CommitMapping> + '_ {
        self.by_canonical
            .iter()
            .map(|(canonical_id, git_id)| CommitMapping {
                canonical_id: canonical_id.clone(),
                git_id: git_id.clone(),
            })
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

#[derive(Clone, Debug, Default)]
pub struct ExactPathFilter {
    excluded: BTreeSet<RepoPathBuf>,
}

impl ExactPathFilter {
    pub fn from_hidden_paths(paths: &HiddenPathSet) -> Result<Self, ProjectionError> {
        let excluded = paths
            .iter()
            .map(|path| {
                RepoPathBuf::from_internal_string(path.as_str().to_owned()).map_err(|source| {
                    ProjectionError::InvalidHiddenPath {
                        path: path.as_str().to_owned(),
                        source: Box::new(source),
                    }
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { excluded })
    }

    fn excludes(&self, path: &RepoPath) -> bool {
        self.excluded.contains(path)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExportResult {
    pub git_heads: Vec<CommitId>,
    pub new_mappings: Vec<CommitMapping>,
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
        hidden_paths: &ExactPathFilter,
        mappings: &mut ExportMappings,
    ) -> Result<ExportResult, ProjectionError> {
        let translated = translate_reachable(
            canonical_store,
            &self.store,
            canonical_heads,
            hidden_paths,
            &mut mappings.by_canonical,
            TranslationDirection::Export,
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
        let translated = translate_reachable(
            &self.store,
            canonical_store,
            git_heads,
            &ExactPathFilter::default(),
            &mut mappings.by_git,
            TranslationDirection::Import,
        )
        .await?;
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

    /// Projects one canonical snapshot as a Git-only child of an existing
    /// public tip. Policy changes are not canonical history, so an unchanged
    /// private commit can still require a new public tree.
    pub async fn project_snapshot_after(
        &self,
        canonical_store: &Arc<Store>,
        canonical_id: &CommitId,
        git_parent: &CommitId,
        hidden_paths: &ExactPathFilter,
        force_policy_commit: bool,
    ) -> Result<CommitId, ProjectionError> {
        let canonical = canonical_store.backend().read_commit(canonical_id).await?;
        let source_tree_id = canonical
            .root_tree
            .as_resolved()
            .ok_or_else(|| ProjectionError::ConflictedCommit(canonical_id.clone()))?;
        let projected_tree_id = copy_tree(
            canonical_store,
            &self.store,
            RepoPath::root(),
            source_tree_id,
            hidden_paths,
            &mut BTreeMap::new(),
            TranslationDirection::Export,
        )
        .await?;
        let parent = self.store.backend().read_commit(git_parent).await?;
        if !force_policy_commit && parent.root_tree.as_resolved() == Some(&projected_tree_id) {
            return Ok(git_parent.clone());
        }
        Ok(self
            .store
            .write_commit(
                BackendCommit {
                    parents: vec![git_parent.clone()],
                    predecessors: Vec::new(),
                    root_tree: jj_lib::merge::Merge::resolved(projected_tree_id),
                    conflict_labels: jj_lib::merge::Merge::resolved(String::new()),
                    change_id: canonical.change_id,
                    description: "Apply Devspace Git projection policy\n".to_owned(),
                    author: canonical.author,
                    committer: canonical.committer,
                    secure_sig: None,
                },
                None,
            )
            .await?
            .id()
            .clone())
    }

    /// Defense-in-depth check for the selected public tip immediately before
    /// Git transport.
    pub async fn scan_hidden_paths(
        &self,
        git_head: &CommitId,
        hidden_paths: &ExactPathFilter,
    ) -> Result<Vec<RepoPathBuf>, ProjectionError> {
        scan_hidden_paths(&self.store, git_head, hidden_paths).await
    }
}

struct TranslationResult {
    target_heads: Vec<CommitId>,
    new_pairs: Vec<(CommitId, CommitId)>,
    reached_pairs: Vec<(CommitId, CommitId)>,
}

#[derive(Clone, Copy)]
enum TranslationDirection {
    Export,
    Import,
}

enum Visit {
    Enter(CommitId),
    Exit(CommitId),
}

async fn translate_reachable(
    source: &Arc<Store>,
    target: &Arc<Store>,
    source_heads: &[CommitId],
    filter: &ExactPathFilter,
    source_to_target: &mut BTreeMap<CommitId, CommitId>,
    direction: TranslationDirection,
) -> Result<TranslationResult, ProjectionError> {
    let mut states = BTreeMap::<CommitId, bool>::new();
    let mut commits = BTreeMap::<CommitId, BackendCommit>::new();
    let mut tree_cache = BTreeMap::<(RepoPathBuf, TreeId), TreeId>::new();
    let mut new_pairs = Vec::new();
    let mut reached_pairs = BTreeMap::new();

    for head in source_heads {
        let mut stack = vec![Visit::Enter(head.clone())];
        while let Some(visit) = stack.pop() {
            match visit {
                Visit::Enter(source_id) => {
                    if source_id == *source.root_commit_id() {
                        continue;
                    }
                    if let Some(target_id) = source_to_target.get(&source_id) {
                        match target.backend().read_commit(target_id).await {
                            Ok(_) => {
                                if matches!(direction, TranslationDirection::Export) {
                                    let leaked =
                                        scan_hidden_paths(target, target_id, filter).await?;
                                    if !leaked.is_empty() {
                                        return Err(ProjectionError::StaleMapping {
                                            canonical_id: source_id,
                                            git_id: target_id.clone(),
                                            leaked,
                                        });
                                    }
                                }
                                reached_pairs.insert(source_id.clone(), target_id.clone());
                                states.insert(source_id, true);
                                continue;
                            }
                            Err(jj_lib::backend::BackendError::ObjectNotFound { .. }) => {}
                            Err(source) => return Err(source.into()),
                        }
                    }
                    match states.get(&source_id) {
                        Some(true) => continue,
                        Some(false) => return Err(ProjectionError::CommitCycle(source_id)),
                        None => {}
                    }
                    let commit = source.backend().read_commit(&source_id).await?;
                    states.insert(source_id.clone(), false);
                    commits.insert(source_id.clone(), commit.clone());
                    stack.push(Visit::Exit(source_id));
                    for parent_id in commit.parents.iter().rev() {
                        stack.push(Visit::Enter(parent_id.clone()));
                    }
                }
                Visit::Exit(source_id) => {
                    if states.get(&source_id) == Some(&true) {
                        continue;
                    }
                    let source_commit = commits
                        .remove(&source_id)
                        .ok_or_else(|| ProjectionError::MissingStagedCommit(source_id.clone()))?;
                    let parents = source_commit
                        .parents
                        .iter()
                        .map(|parent_id| {
                            mapped_commit_id(source, target, source_to_target, parent_id)
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    let source_tree_id = source_commit
                        .root_tree
                        .as_resolved()
                        .ok_or_else(|| ProjectionError::ConflictedCommit(source_id.clone()))?;
                    let target_tree_id = copy_tree(
                        source,
                        target,
                        RepoPath::root(),
                        source_tree_id,
                        filter,
                        &mut tree_cache,
                        direction,
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
                    if record_mapping(
                        source_to_target,
                        source_id.clone(),
                        target_id.clone(),
                        "source commit",
                    )? {
                        new_pairs.push((source_id.clone(), target_id));
                    }
                    states.insert(source_id, true);
                }
            }
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
    hidden_paths: &ExactPathFilter,
) -> Result<Vec<RepoPathBuf>, ProjectionError> {
    let commit = store.get_commit_async(commit_id).await?;
    let tree = commit.tree();
    let mut leaked = Vec::new();
    for path in &hidden_paths.excluded {
        let value = tree
            .path_value(path)
            .await?
            .into_resolved()
            .map_err(|_| ProjectionError::ConflictedProjectedPath(path.clone()))?;
        if value.is_some() {
            leaked.push(path.clone());
        }
    }
    Ok(leaked)
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
    source: &'a Arc<Store>,
    target: &'a Arc<Store>,
    path: &'a RepoPath,
    source_tree_id: &'a TreeId,
    filter: &'a ExactPathFilter,
    cache: &'a mut BTreeMap<(RepoPathBuf, TreeId), TreeId>,
    direction: TranslationDirection,
) -> Pin<Box<dyn Future<Output = Result<TreeId, ProjectionError>> + 'a>> {
    Box::pin(async move {
        if source_tree_id == source.empty_tree_id() {
            return Ok(target.empty_tree_id().clone());
        }
        let cache_key = (path.to_owned(), source_tree_id.clone());
        if let Some(target_tree_id) = cache.get(&cache_key) {
            return Ok(target_tree_id.clone());
        }

        let source_tree = source.get_tree(path.to_owned(), source_tree_id).await?;
        let mut target_entries = Vec::new();
        let mut leaf_copies = Vec::new();
        for entry in source_tree.entries_non_recursive() {
            let entry_path = path.join(entry.name());
            if filter.excludes(&entry_path) {
                if matches!(entry.value(), TreeValue::Tree(_)) {
                    return Err(ProjectionError::HiddenDirectory(entry_path));
                }
                continue;
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
                        copy_tree(source, target, &entry_path, id, filter, cache, direction)
                            .await?;
                    if target_id == *target.empty_tree_id() {
                        continue;
                    }
                    TreeValue::Tree(target_id)
                }
                TreeValue::GitSubmodule(_) => {
                    return Err(ProjectionError::GitLink {
                        path: entry_path,
                        operation: direction.label(),
                    });
                }
            };
            target_entries.push((entry.name().to_owned(), target_value));
        }
        for leaf in leaf_copies {
            target_entries.push(leaf.copy(source, target).await?);
        }
        target_entries.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
        let target_tree = target
            .write_tree(path, BackendTree::from_sorted_entries(target_entries))
            .await?;
        let target_tree_id = target_tree.id().clone();
        cache.insert(cache_key, target_tree_id.clone());
        Ok(target_tree_id)
    })
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
    #[error("hidden path {path:?} is not a valid jj repository path")]
    InvalidHiddenPath {
        path: String,
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
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
    #[error("projected path {0:?} is conflicted")]
    ConflictedProjectedPath(RepoPathBuf),
    #[error(
        "Git mapping {canonical_id} -> {git_id} violates the active hidden policy at {leaked:?}"
    )]
    StaleMapping {
        canonical_id: CommitId,
        git_id: CommitId,
        leaked: Vec<RepoPathBuf>,
    },
    #[error("cannot hide directory {0:?}; hidden paths must name exact files")]
    HiddenDirectory(RepoPathBuf),
    #[error("cannot {operation} Git link at {path:?}")]
    GitLink {
        path: RepoPathBuf,
        operation: &'static str,
    },
    #[error(transparent)]
    Backend(#[from] jj_lib::backend::BackendError),
}

impl TranslationDirection {
    fn label(self) -> &'static str {
        match self {
            Self::Export => "export",
            Self::Import => "import",
        }
    }
}
