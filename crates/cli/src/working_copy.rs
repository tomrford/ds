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
use jj_lib::repo_path::{RepoPath, RepoPathBuf};
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

const DSHIDE: &str = ".dshide";

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
        let mut factories = WorkingCopyFactories::new();
        factories.insert("local".to_owned(), Box::new(DevspaceWorkingCopyFactory));
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
        self.inner.name()
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
        }))
    }
}

struct LockedDevspaceWorkingCopy {
    inner: Box<dyn LockedWorkingCopy>,
    working_copy_path: PathBuf,
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
        let Some(hidden) = hidden_track_matcher(&self.working_copy_path)? else {
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
        self.inner.check_out(commit).await
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
        self.inner.set_sparse_patterns(new_sparse_patterns).await
    }

    async fn finish(
        self: Box<Self>,
        operation_id: OperationId,
    ) -> Result<Box<dyn WorkingCopy>, WorkingCopyStateError> {
        let inner = self.inner.finish(operation_id).await?;
        Ok(Box::new(DevspaceWorkingCopy {
            inner,
            working_copy_path: self.working_copy_path,
        }))
    }
}

fn hidden_track_matcher(root: &Path) -> Result<Option<HiddenTrackMatcher>, SnapshotError> {
    let path = root.join(DSHIDE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(SnapshotError::Other {
                message: format!("Failed to read {}", path.display()),
                err: error.into(),
            });
        }
    };
    let matcher = GitIgnoreFile::empty().chain(RepoPath::root(), Path::new(DSHIDE), &bytes)?;
    Ok(Some(HiddenTrackMatcher { matcher }))
}

#[derive(Debug)]
struct HiddenTrackMatcher {
    matcher: Arc<GitIgnoreFile>,
}

impl Matcher for HiddenTrackMatcher {
    fn matches(&self, file: &RepoPath) -> bool {
        file.as_internal_file_string() == DSHIDE || hidden_file_matches(&self.matcher, file)
    }

    fn visit(&self, _dir: &RepoPath) -> Visit {
        Visit::Specific {
            dirs: VisitDirs::All,
            files: VisitFiles::All,
        }
    }
}

fn hidden_file_matches(matcher: &GitIgnoreFile, path: &RepoPath) -> bool {
    matcher.matches_file(path)
        || path
            .ancestors()
            .skip(1)
            .filter(|ancestor| !ancestor.is_root())
            .any(|ancestor| matcher.matches_dir(ancestor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn force_tracking_includes_children_of_a_hidden_directory() {
        let matcher = GitIgnoreFile::empty()
            .chain(RepoPath::root(), Path::new(DSHIDE), b"private/\n")
            .unwrap();
        let matcher = HiddenTrackMatcher { matcher };
        assert!(matcher.matches(RepoPath::from_internal_string("private/secret").unwrap()));
    }
}
