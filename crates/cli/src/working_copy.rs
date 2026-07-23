use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use jj_lib::commit::Commit;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::{Matcher, UnionMatcher, Visit, VisitDirs, VisitFiles};
use jj_lib::merged_tree::MergedTree;
use jj_lib::op_store::OperationId;
use jj_lib::ref_name::{WorkspaceName, WorkspaceNameBuf};
use jj_lib::repo::StoreFactories;
use jj_lib::repo_path::{RepoPath, RepoPathBuf, RepoPathComponentBuf};
use jj_lib::settings::UserSettings;
use jj_lib::store::Store;
use jj_lib::working_copy::{
    CheckoutError, CheckoutStats, LockedWorkingCopy, ResetError, SnapshotError, SnapshotOptions,
    SnapshotStats, WorkingCopy, WorkingCopyFactory, WorkingCopyStateError,
};
use jj_lib::workspace::{
    WorkingCopyFactories, Workspace, WorkspaceLoadError, WorkspaceLoader,
    default_working_copy_factory,
};

const DSPRIVATE: &str = ".dsprivate";
pub(crate) const DEVSPACE_WORKING_COPY_TYPE: &str = "devspace-local";

pub(crate) fn devspace_working_copy_factory() -> Box<dyn WorkingCopyFactory> {
    Box::new(DevspaceWorkingCopyFactory)
}

pub(crate) fn devspace_working_copy_factories() -> WorkingCopyFactories {
    let mut factories = WorkingCopyFactories::new();
    factories.insert(
        DEVSPACE_WORKING_COPY_TYPE.to_owned(),
        devspace_working_copy_factory(),
    );
    factories
}

pub(crate) fn wrap_workspace_loader(inner: Box<dyn WorkspaceLoader>) -> Box<dyn WorkspaceLoader> {
    Box::new(DevspaceWorkspaceLoader { inner })
}

struct DevspaceWorkspaceLoader {
    inner: Box<dyn WorkspaceLoader>,
}

impl WorkspaceLoader for DevspaceWorkspaceLoader {
    fn workspace_root(&self) -> &Path {
        self.inner.workspace_root()
    }

    fn repo_path(&self) -> &Path {
        self.inner.repo_path()
    }

    fn load(
        &self,
        settings: &UserSettings,
        store_factories: &StoreFactories,
        _working_copy_factories: &WorkingCopyFactories,
    ) -> Result<Workspace, WorkspaceLoadError> {
        let factories = devspace_working_copy_factories();
        self.inner.load(settings, store_factories, &factories)
    }

    fn get_working_copy_type(&self) -> Result<String, jj_lib::repo::StoreLoadError> {
        self.inner.get_working_copy_type()
    }
}

struct DevspaceWorkingCopyFactory;

impl WorkingCopyFactory for DevspaceWorkingCopyFactory {
    fn init_working_copy(
        &self,
        store: Arc<Store>,
        working_copy_path: PathBuf,
        state_path: PathBuf,
        operation_id: OperationId,
        workspace_name: WorkspaceNameBuf,
        settings: &UserSettings,
    ) -> Result<Box<dyn WorkingCopy>, WorkingCopyStateError> {
        let inner = default_working_copy_factory().init_working_copy(
            store,
            working_copy_path.clone(),
            state_path,
            operation_id,
            workspace_name,
            settings,
        )?;
        Ok(Box::new(DevspaceWorkingCopy {
            inner,
            working_copy_path,
        }))
    }

    fn load_working_copy(
        &self,
        store: Arc<Store>,
        working_copy_path: PathBuf,
        state_path: PathBuf,
        settings: &UserSettings,
    ) -> Result<Box<dyn WorkingCopy>, WorkingCopyStateError> {
        let inner = default_working_copy_factory().load_working_copy(
            store,
            working_copy_path.clone(),
            state_path,
            settings,
        )?;
        Ok(Box::new(DevspaceWorkingCopy {
            inner,
            working_copy_path,
        }))
    }
}

struct DevspaceWorkingCopy {
    inner: Box<dyn WorkingCopy>,
    working_copy_path: PathBuf,
}

#[async_trait(?Send)]
impl WorkingCopy for DevspaceWorkingCopy {
    fn name(&self) -> &str {
        DEVSPACE_WORKING_COPY_TYPE
    }

    fn workspace_name(&self) -> &WorkspaceName {
        self.inner.workspace_name()
    }

    fn operation_id(&self) -> &OperationId {
        self.inner.operation_id()
    }

    fn tree(&self) -> Result<&MergedTree, WorkingCopyStateError> {
        self.inner.tree()
    }

    fn sparse_patterns(&self) -> Result<&[RepoPathBuf], WorkingCopyStateError> {
        self.inner.sparse_patterns()
    }

    async fn start_mutation(&self) -> Result<Box<dyn LockedWorkingCopy>, WorkingCopyStateError> {
        Ok(Box::new(LockedDevspaceWorkingCopy {
            inner: self.inner.start_mutation().await?,
            working_copy_path: self.working_copy_path.clone(),
            checkout_tree_moved: false,
            clear_messages: Vec::new(),
        }))
    }
}

struct LockedDevspaceWorkingCopy {
    inner: Box<dyn LockedWorkingCopy>,
    working_copy_path: PathBuf,
    checkout_tree_moved: bool,
    clear_messages: Vec<crate::context::SyncMessage>,
}

impl LockedDevspaceWorkingCopy {
    fn capture_context_before_tree_movement(&mut self) {
        if !crate::boundary_sync::context_auto_sync_enabled() {
            return;
        }
        match crate::context::clear_at(&self.working_copy_path) {
            Ok(messages) => self.clear_messages.extend(messages),
            Err(error) => self.clear_messages.push(crate::context::SyncMessage {
                kind: crate::context::SyncMessageKind::Warning,
                text: format!(
                    "could not clear context before working-copy movement in {} ({error:#}); the context state may be inconsistent after movement, so rewrite or rebuild it manually",
                    self.working_copy_path.display()
                ),
            }),
        }
    }

    fn tree_will_move_to(&self, commit: &Commit) -> bool {
        self.inner.old_tree().tree_ids_and_labels() != commit.tree().tree_ids_and_labels()
    }

    fn record_failed_tree_movement(&self) {
        crate::boundary_sync::record_checkout_movement(
            &self.working_copy_path,
            self.clear_messages.clone(),
        );
    }
}

#[async_trait]
impl LockedWorkingCopy for LockedDevspaceWorkingCopy {
    fn old_operation_id(&self) -> &OperationId {
        self.inner.old_operation_id()
    }

    fn old_tree(&self) -> &MergedTree {
        self.inner.old_tree()
    }

    async fn snapshot(
        &mut self,
        options: &SnapshotOptions,
    ) -> Result<(MergedTree, SnapshotStats), SnapshotError> {
        let Some(hidden) = hidden_track_matcher(&self.working_copy_path, &options.base_ignores)?
        else {
            return self.inner.snapshot(options).await;
        };
        let start_tracking = UnionMatcher::new(options.start_tracking_matcher, &hidden);
        let force_tracking = UnionMatcher::new(options.force_tracking_matcher, &hidden);
        let options = SnapshotOptions {
            start_tracking_matcher: &start_tracking,
            force_tracking_matcher: &force_tracking,
            ..options.clone()
        };
        self.inner.snapshot(&options).await
    }

    async fn check_out(&mut self, commit: &Commit) -> Result<CheckoutStats, CheckoutError> {
        let tree_moved = self.tree_will_move_to(commit);
        if tree_moved {
            self.capture_context_before_tree_movement();
        }
        let stats = match self.inner.check_out(commit).await {
            Ok(stats) => stats,
            Err(error) => {
                if tree_moved {
                    self.record_failed_tree_movement();
                }
                return Err(error);
            }
        };
        self.checkout_tree_moved |= tree_moved;
        Ok(stats)
    }

    fn rename_workspace(&mut self, new_workspace_name: WorkspaceNameBuf) {
        self.inner.rename_workspace(new_workspace_name);
    }

    async fn reset(&mut self, commit: &Commit) -> Result<(), ResetError> {
        self.inner.reset(commit).await
    }

    async fn recover(&mut self, commit: &Commit) -> Result<(), ResetError> {
        self.inner.recover(commit).await
    }

    fn sparse_patterns(&self) -> Result<&[RepoPathBuf], WorkingCopyStateError> {
        self.inner.sparse_patterns()
    }

    async fn set_sparse_patterns(
        &mut self,
        new_sparse_patterns: Vec<RepoPathBuf>,
    ) -> Result<CheckoutStats, CheckoutError> {
        self.capture_context_before_tree_movement();
        let stats = match self.inner.set_sparse_patterns(new_sparse_patterns).await {
            Ok(stats) => stats,
            Err(error) => {
                self.record_failed_tree_movement();
                return Err(error);
            }
        };
        self.checkout_tree_moved = true;
        Ok(stats)
    }

    async fn finish(
        self: Box<Self>,
        operation_id: OperationId,
    ) -> Result<Box<dyn WorkingCopy>, WorkingCopyStateError> {
        let Self {
            inner,
            working_copy_path,
            checkout_tree_moved,
            clear_messages,
        } = *self;
        let inner = match inner.finish(operation_id).await {
            Ok(inner) => inner,
            Err(error) => {
                if checkout_tree_moved {
                    crate::boundary_sync::record_checkout_movement(
                        &working_copy_path,
                        clear_messages,
                    );
                }
                return Err(error);
            }
        };
        if checkout_tree_moved {
            crate::boundary_sync::record_checkout_movement(&working_copy_path, clear_messages);
        }
        Ok(Box::new(DevspaceWorkingCopy {
            inner,
            working_copy_path,
        }))
    }
}

fn hidden_track_matcher(
    root: &Path,
    base_ignores: &Arc<GitIgnoreFile>,
) -> Result<Option<HiddenTrackMatcher>, SnapshotError> {
    let mut hidden = HiddenTrackMatcher::default();
    discover_hidden_paths_into(
        root,
        RepoPath::root(),
        &GitIgnoreFile::empty(),
        base_ignores,
        base_ignores,
        &mut hidden,
    )?;
    if hidden.files.is_empty() && hidden.directories.is_empty() {
        Ok(None)
    } else {
        Ok(Some(hidden))
    }
}

pub(crate) struct ShimDiscovery {
    pub hidden_paths: BTreeSet<RepoPathBuf>,
    pub base_ignored_paths: BTreeSet<RepoPathBuf>,
}

pub(crate) fn discover_shim_paths(
    root: &Path,
    base_ignores: &Arc<GitIgnoreFile>,
) -> Result<ShimDiscovery, SnapshotError> {
    let mut hidden = HiddenTrackMatcher::default();
    discover_hidden_paths_into(
        root,
        RepoPath::root(),
        &GitIgnoreFile::empty(),
        base_ignores,
        base_ignores,
        &mut hidden,
    )?;
    hidden.files.extend(hidden.directories);
    Ok(ShimDiscovery {
        hidden_paths: hidden.files,
        base_ignored_paths: hidden.base_ignored_paths,
    })
}

/// Mirrors jj-cli 0.42's Git-backend `WorkspaceCommandHelper::base_ignores`:
/// the backend repository config's global excludes plus `.git/info/exclude`.
pub(crate) fn base_ignores(
    workspace_root: &Path,
    settings: &UserSettings,
) -> Result<Arc<GitIgnoreFile>, String> {
    fn xdg_config_home() -> Option<PathBuf> {
        if let Ok(config_home) = std::env::var("XDG_CONFIG_HOME")
            && !config_home.is_empty()
        {
            return Some(PathBuf::from(config_home));
        }
        etcetera::home_dir().ok().map(|home| home.join(".config"))
    }

    let workspace = Workspace::load(
        settings,
        workspace_root,
        &StoreFactories::default(),
        &devspace_working_copy_factories(),
    )
    .map_err(|error| error.to_string())?;
    let backend = jj_lib::git::get_git_backend(workspace.repo_loader().store())
        .map_err(|error| error.to_string())?;
    let git_repo = backend.git_repo();
    let config = git_repo.config_snapshot();
    let excludes_file_path = match config.string("core.excludesFile") {
        // A relative configured path is resolved at the work-tree
        // directory, matching jj-cli and `git status`.
        Some(value) => str::from_utf8(&value)
            .ok()
            .map(jj_lib::file_util::expand_home_path)
            .map(|path| workspace_root.join(path)),
        None => xdg_config_home().map(|dir| dir.join("git").join("ignore")),
    };
    let mut ignores = GitIgnoreFile::empty();
    if let Some(path) = excludes_file_path {
        ignores = ignores
            .chain_with_file(RepoPath::root(), path)
            .map_err(|error| error.to_string())?;
    }
    ignores
        .chain_with_file(
            RepoPath::root(),
            backend.git_repo_path().join("info").join("exclude"),
        )
        .map_err(|error| error.to_string())
}

#[derive(Debug, Default)]
struct HiddenTrackMatcher {
    files: BTreeSet<RepoPathBuf>,
    directories: BTreeSet<RepoPathBuf>,
    visited_directories: BTreeSet<RepoPathBuf>,
    base_ignored_paths: BTreeSet<RepoPathBuf>,
}

impl Matcher for HiddenTrackMatcher {
    fn matches(&self, file: &RepoPath) -> bool {
        self.files.contains(file)
            || file
                .ancestors()
                .skip(1)
                .any(|ancestor| self.directories.contains(ancestor))
    }

    fn visit(&self, dir: &RepoPath) -> Visit {
        if self.visited_directories.contains(dir)
            || dir
                .ancestors()
                .any(|ancestor| self.directories.contains(ancestor))
        {
            Visit::Specific {
                dirs: VisitDirs::All,
                files: VisitFiles::All,
            }
        } else {
            Visit::Nothing
        }
    }
}

fn discover_hidden_paths_into(
    disk_dir: &Path,
    dir: &RepoPath,
    inherited_hidden: &Arc<GitIgnoreFile>,
    inherited_gitignore: &Arc<GitIgnoreFile>,
    base_ignores: &Arc<GitIgnoreFile>,
    result: &mut HiddenTrackMatcher,
) -> Result<(), SnapshotError> {
    let entries = fs::read_dir(disk_dir)
        .and_then(|entries| entries.collect::<Result<Vec<_>, _>>())
        .map_err(|error| SnapshotError::Other {
            message: format!("Failed to read directory {}", disk_dir.display()),
            err: error.into(),
        })?;
    result.visited_directories.insert(dir.to_owned());

    let mut hidden = inherited_hidden.clone();
    if let Some(entry) = entries
        .iter()
        .find(|entry| entry.file_name().as_encoded_bytes() == DSPRIVATE.as_bytes())
    {
        let path = entry.path();
        let bytes = read_ignore_file(&path)?;
        hidden = hidden.chain(dir, &path, &bytes)?;
        result.files.insert(
            dir.join(
                &RepoPathComponentBuf::new(DSPRIVATE.to_owned())
                    .expect(".dsprivate is a valid path component"),
            ),
        );
    }

    let mut gitignore = inherited_gitignore.clone();
    if let Some(entry) = entries
        .iter()
        .find(|entry| entry.file_name().as_encoded_bytes() == b".gitignore")
    {
        let path = entry.path();
        if path.is_file() {
            let bytes = read_ignore_file(&path)?;
            gitignore = gitignore.chain(dir, &path, &bytes)?;
        }
    }

    for entry in entries {
        let file_name = entry.file_name();
        let name = file_name
            .into_string()
            .map_err(|path| SnapshotError::InvalidUtf8Path { path })?;
        if name == DSPRIVATE {
            continue;
        }
        if dir.is_root() && matches!(name.as_str(), ".git" | ".jj") {
            continue;
        }
        let component = RepoPathComponentBuf::new(name)
            .expect("filesystem entry name is a valid path component");
        let path = dir.join(&component);
        let file_type = entry.file_type().map_err(|error| SnapshotError::Other {
            message: format!("Failed to inspect {}", entry.path().display()),
            err: error.into(),
        })?;
        if file_type.is_dir() {
            if hidden.matches_dir(&path) {
                result.directories.insert(path);
            } else if gitignore.matches_dir(&path) {
                if base_ignores.matches_dir(&path) {
                    result.base_ignored_paths.insert(path);
                }
            } else {
                discover_hidden_paths_into(
                    &entry.path(),
                    &path,
                    &hidden,
                    &gitignore,
                    base_ignores,
                    result,
                )?;
            }
        } else if hidden.matches_file(&path) {
            result.files.insert(path);
        } else if gitignore.matches_file(&path) && base_ignores.matches_file(&path) {
            result.base_ignored_paths.insert(path);
        }
    }
    Ok(())
}

fn read_ignore_file(path: &Path) -> Result<Vec<u8>, SnapshotError> {
    fs::read(path).map_err(|error| SnapshotError::Other {
        message: format!("Failed to read {}", path.display()),
        err: error.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn force_tracking_includes_children_of_a_hidden_directory() {
        let matcher = HiddenTrackMatcher {
            directories: [RepoPathBuf::from_internal_string("private").unwrap()]
                .into_iter()
                .collect(),
            ..HiddenTrackMatcher::default()
        };
        assert!(matcher.matches(RepoPath::from_internal_string("private/secret").unwrap()));
    }
}
