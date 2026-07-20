//! Rebuildable Git projection for a native machine repository.
//!
//! The sidecar contains only public Git objects. Canonical jj objects and all
//! durable projection receipts remain outside it.

use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use jj_lib::backend::{
    Commit as BackendCommit, CommitId, CopyId, FileId, SymlinkId, Tree as BackendTree, TreeId,
    TreeValue,
};
use jj_lib::conflict_labels::ConflictLabels;
use jj_lib::git_backend::GitBackend;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::merged_tree::MergedTree;
use jj_lib::repo_path::{RepoPath, RepoPathBuf, RepoPathComponentBuf};
use jj_lib::settings::UserSettings;
use jj_lib::signing::Signer;
use jj_lib::store::Store;
use jj_lib::tree_merge::MergeOptions;
use thiserror::Error;

const STORE_DIR: &str = "store";
const DSHIDE: &str = ".dshide";

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

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HiddenSetIdentity(Option<FileId>);

impl HiddenSetIdentity {
    pub fn file_id(&self) -> Option<&FileId> {
        self.0.as_ref()
    }
}

#[derive(Clone, Debug)]
pub struct HiddenSet {
    identity: HiddenSetIdentity,
    matcher: Arc<GitIgnoreFile>,
}

impl HiddenSet {
    pub fn identity(&self) -> &HiddenSetIdentity {
        &self.identity
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
        mappings: &mut ExportMappings,
    ) -> Result<ExportResult, ProjectionError> {
        let translated = translate_reachable(
            canonical_store,
            &self.store,
            canonical_heads,
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
            &mut BTreeMap::new(),
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
    source_to_target: &mut BTreeMap<CommitId, CommitId>,
    direction: TranslationDirection,
) -> Result<TranslationResult, ProjectionError> {
    let mut states = BTreeMap::<CommitId, bool>::new();
    let mut commits = BTreeMap::<CommitId, BackendCommit>::new();
    let mut tree_cache = BTreeMap::<(HiddenSetIdentity, RepoPathBuf, TreeId), TreeId>::new();
    let mut matcher_cache = BTreeMap::<FileId, Arc<GitIgnoreFile>>::new();
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
                                    let source_commit =
                                        source.backend().read_commit(&source_id).await?;
                                    let hidden_set = resolve_hidden_set(
                                        source,
                                        &source_id,
                                        &source_commit,
                                        &mut matcher_cache,
                                    )
                                    .await?;
                                    if source_commit.root_tree.as_resolved().is_none() {
                                        return Err(ProjectionError::ConflictedCommit(source_id));
                                    }
                                    let leaked =
                                        scan_hidden_paths(target, target_id, &hidden_set).await?;
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
                    let hidden_set = match direction {
                        TranslationDirection::Export => Some(
                            resolve_hidden_set(
                                source,
                                &source_id,
                                &source_commit,
                                &mut matcher_cache,
                            )
                            .await?,
                        ),
                        TranslationDirection::Import => None,
                    };
                    let source_tree_id = source_commit
                        .root_tree
                        .as_resolved()
                        .ok_or_else(|| ProjectionError::ConflictedCommit(source_id.clone()))?;
                    let target_tree_id = copy_tree(
                        source,
                        target,
                        RepoPath::root(),
                        source_tree_id,
                        hidden_set.as_ref(),
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
            if is_root_dshide(path, entry.name()) {
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
    source: &'a Arc<Store>,
    target: &'a Arc<Store>,
    path: &'a RepoPath,
    source_tree_id: &'a TreeId,
    hidden_set: Option<&'a HiddenSet>,
    cache: &'a mut BTreeMap<(HiddenSetIdentity, RepoPathBuf, TreeId), TreeId>,
    direction: TranslationDirection,
) -> Pin<Box<dyn Future<Output = Result<TreeId, ProjectionError>> + 'a>> {
    Box::pin(async move {
        if source_tree_id == source.empty_tree_id() {
            return Ok(target.empty_tree_id().clone());
        }
        let cache_key = (
            hidden_set
                .map(|hidden_set| hidden_set.identity.clone())
                .unwrap_or(HiddenSetIdentity(None)),
            path.to_owned(),
            source_tree_id.clone(),
        );
        if let Some(target_tree_id) = cache.get(&cache_key) {
            return Ok(target_tree_id.clone());
        }

        let source_tree = source.get_tree(path.to_owned(), source_tree_id).await?;
        let mut target_entries = Vec::new();
        let mut leaf_copies = Vec::new();
        for entry in source_tree.entries_non_recursive() {
            let entry_path = path.join(entry.name());
            if let Some(hidden_set) = hidden_set {
                let excluded = match entry.value() {
                    TreeValue::Tree(_) => hidden_set.matcher.matches_dir(&entry_path),
                    _ => {
                        is_root_dshide(path, entry.name())
                            || hidden_set.matcher.matches_file(&entry_path)
                    }
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
                    let target_id = copy_tree(
                        source,
                        target,
                        &entry_path,
                        id,
                        hidden_set,
                        cache,
                        direction,
                    )
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

async fn resolve_hidden_set(
    store: &Arc<Store>,
    commit_id: &CommitId,
    commit: &BackendCommit,
    cache: &mut BTreeMap<FileId, Arc<GitIgnoreFile>>,
) -> Result<HiddenSet, ProjectionError> {
    let merged_tree = MergedTree::new(
        store.clone(),
        commit.root_tree.clone(),
        ConflictLabels::from_merge(commit.conflict_labels.clone()),
    );
    let dshide_path = RepoPath::from_internal_string(DSHIDE).expect(".dshide is a repository path");
    let value = merged_tree
        .path_value(dshide_path)
        .await?
        .into_resolved()
        .map_err(|_| ProjectionError::ConflictedDshide(commit_id.clone()))?;
    let Some(value) = value else {
        return Ok(HiddenSet {
            identity: HiddenSetIdentity(None),
            matcher: GitIgnoreFile::empty(),
        });
    };
    let TreeValue::File { id, .. } = value else {
        return Err(ProjectionError::InvalidDshideEntry(commit_id.clone()));
    };
    if let Some(matcher) = cache.get(&id) {
        return Ok(HiddenSet {
            identity: HiddenSetIdentity(Some(id)),
            matcher: matcher.clone(),
        });
    }
    let mut bytes = Vec::new();
    let contents = store.read_file(dshide_path, &id).await?;
    jj_lib::file_util::copy_async_to_sync(contents, &mut bytes)
        .await
        .map_err(|source| ProjectionError::ReadDshide {
            commit_id: commit_id.clone(),
            source,
        })?;
    let matcher = GitIgnoreFile::empty()
        .chain(RepoPath::root(), Path::new(DSHIDE), &bytes)
        .expect("in-memory gitignore patterns have no parse errors");
    cache.insert(id.clone(), matcher.clone());
    Ok(HiddenSet {
        identity: HiddenSetIdentity(Some(id)),
        matcher,
    })
}

fn is_root_dshide(path: &RepoPath, name: &jj_lib::repo_path::RepoPathComponent) -> bool {
    path.is_root() && name.as_internal_str() == DSHIDE
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
    #[error("cannot export commit {0}: .dshide is conflicted")]
    ConflictedDshide(CommitId),
    #[error("cannot export commit {0}: .dshide is not a regular file")]
    InvalidDshideEntry(CommitId),
    #[error("cannot read .dshide in commit {commit_id}")]
    ReadDshide {
        commit_id: CommitId,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(bytes: &[u8]) -> Arc<GitIgnoreFile> {
        GitIgnoreFile::empty()
            .chain(RepoPath::root(), Path::new(DSHIDE), bytes)
            .unwrap()
    }

    fn repo_path(value: &str) -> &RepoPath {
        RepoPath::from_internal_string(value).unwrap()
    }

    #[test]
    fn dshide_gitignore_semantics_are_canonical() {
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
}
