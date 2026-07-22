use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use devspace_machine::{CatalogEntry, MachineStore, RepositoryName};
use jj_lib::commit::Commit;
use jj_lib::dag_walk_async;
use jj_lib::default_index::DefaultIndexStore;
use jj_lib::default_submodule_store::DefaultSubmoduleStore;
use jj_lib::merged_tree::MergedTree;
use jj_lib::op_heads_store::{OpHeadsStore, OpHeadsStoreError, OpHeadsStoreLock};
use jj_lib::op_store::{OpStore, OperationId};
use jj_lib::operation::Operation;
use jj_lib::ref_name::{WorkspaceName, WorkspaceNameBuf};
use jj_lib::repo::{ReadonlyRepo, Repo as _, RepoLoader, StoreFactories};
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::settings::UserSettings;
use jj_lib::simple_backend::SimpleBackend;
use jj_lib::simple_op_heads_store::SimpleOpHeadsStore;
use jj_lib::simple_op_store::SimpleOpStore;
use jj_lib::working_copy::{
    CheckoutError, CheckoutStats, LockedWorkingCopy, ResetError, SnapshotError, SnapshotOptions,
    SnapshotStats, WorkingCopy, WorkingCopyStateError,
};
use jj_lib::workspace::{
    DefaultWorkspaceLoaderFactory, WorkingCopyFactories, Workspace, WorkspaceLoadError,
    WorkspaceLoader, WorkspaceLoaderFactory,
};

const BARE_WORKING_COPY_NAME: &str = "devspace-bare-read-only";
// OS command-line arguments cannot contain NUL, so this can never name a
// workspace selected by a user. Its absence from the view makes `@` undefined.
const BARE_WORKSPACE_NAME: &str = "\0devspace-bare-read-only";
pub struct DevspaceWorkspaceLoaderFactory {
    repository_selector: Arc<RepositorySelector>,
}

struct CatalogSelection {
    name: RepositoryName,
    requested_path: PathBuf,
}

#[derive(clap::Args)]
pub(crate) struct ParsedRepositoryArgs {
    #[arg(from_global)]
    pub repository: Option<String>,
}

pub(crate) struct RepositorySelector {
    cwd: Option<PathBuf>,
    selection: OnceLock<Option<CatalogSelection>>,
    catalog_entry: OnceLock<Result<Option<CatalogEntry>, String>>,
}

impl RepositorySelector {
    pub fn from_process_cwd() -> Self {
        let cwd = std::env::current_dir()
            .ok()
            .map(|cwd| dunce::canonicalize(&cwd).unwrap_or(cwd));
        Self {
            cwd,
            selection: OnceLock::new(),
            catalog_entry: OnceLock::new(),
        }
    }

    pub fn set_parsed_repository(&self, repository: Option<String>) {
        let selection = repository
            .as_deref()
            .and_then(|argument| repository_name(Path::new(argument)))
            .and_then(|name| {
                Some(CatalogSelection {
                    requested_path: self.cwd.as_ref()?.join(name.as_str()),
                    name,
                })
            });
        assert!(
            self.selection.set(selection).is_ok(),
            "parsed repository selection is set once"
        );
    }

    fn selection(&self) -> Option<&CatalogSelection> {
        self.selection.get().and_then(Option::as_ref)
    }

    pub fn selected_name(&self) -> Option<&RepositoryName> {
        self.selection().map(|selection| &selection.name)
    }

    pub fn catalog_entry(&self) -> Result<Option<&CatalogEntry>, &str> {
        self.catalog_entry_with(|name| {
            let store = MachineStore::platform_default().map_err(|error| error.to_string())?;
            store.resolve(name).map_err(|error| error.to_string())
        })
    }

    fn catalog_entry_with(
        &self,
        resolve: impl FnOnce(&RepositoryName) -> Result<Option<CatalogEntry>, String>,
    ) -> Result<Option<&CatalogEntry>, &str> {
        let Some(selection) = self.selection() else {
            return Ok(None);
        };
        let entry = self
            .catalog_entry
            .get_or_init(|| resolve(&selection.name))
            .as_ref()
            .map(Option::as_ref)
            .map_err(String::as_str)?;
        if let Some(entry) = entry {
            crate::boundary_sync::record(entry);
        }
        Ok(entry)
    }
}

impl DevspaceWorkspaceLoaderFactory {
    pub fn new(repository_selector: Arc<RepositorySelector>) -> Self {
        Self {
            repository_selector,
        }
    }
}

impl WorkspaceLoaderFactory for DevspaceWorkspaceLoaderFactory {
    fn create(
        &self,
        workspace_root: &Path,
    ) -> Result<Box<dyn WorkspaceLoader>, WorkspaceLoadError> {
        if workspace_root.join(".jj").is_dir() {
            let loader = DefaultWorkspaceLoaderFactory.create(workspace_root)?;
            crate::boundary_sync::record_repository_path(loader.repo_path());
            if crate::checkout::read_checkout_owner(workspace_root).is_ok() {
                crate::boundary_sync::record_checkout(workspace_root);
                Ok(crate::working_copy::wrap_workspace_loader(loader))
            } else {
                Ok(loader)
            }
        } else if let Some(selection) = self.repository_selector.selection()
            && workspace_root == selection.requested_path
        {
            let entry = self.repository_selector.catalog_entry().ok().flatten();
            let path = entry
                .filter(|entry| is_stock_bare_repository(&entry.native_repository_path))
                .map_or_else(
                    || workspace_root.to_owned(),
                    |entry| entry.native_repository_path.clone(),
                );
            Ok(bare_repository_loader(path))
        } else {
            DefaultWorkspaceLoaderFactory.create(workspace_root)
        }
    }
}

fn bare_repository_loader(path: PathBuf) -> Box<dyn WorkspaceLoader> {
    Box::new(BareRepositoryLoader {
        path,
        selected_operation: Arc::new(OnceLock::new()),
    })
}

/// Builds the workspace-less command context used to resolve revisions for a
/// checkout before that checkout exists. The repository remains writable; the
/// sentinel only makes bare `@` unavailable.
pub(crate) fn workspace_for_repository(path: PathBuf, repo: Arc<ReadonlyRepo>) -> Workspace {
    let working_copy = BareWorkingCopy {
        operation_id: repo.op_id().clone(),
        tree: repo.store().empty_merged_tree(),
        sparse_patterns: Vec::new(),
    };
    Workspace::new_no_canonicalize(
        path.clone(),
        path,
        Box::new(working_copy),
        repo.loader().clone(),
    )
}

pub(crate) fn is_stock_bare_repository(path: &Path) -> bool {
    let config_markers = [
        path.join("config.toml"),
        path.join("config-id"),
        path.join(".jj/workspace-config.toml"),
        path.join(".jj/workspace-config-id"),
    ];
    if config_markers
        .iter()
        .any(|marker| marker.symlink_metadata().is_ok())
    {
        return false;
    }
    let stock_store_types = [
        ("store", SimpleBackend::name()),
        ("op_store", SimpleOpStore::name()),
        ("op_heads", SimpleOpHeadsStore::name()),
        ("index", DefaultIndexStore::name()),
        ("submodule_store", DefaultSubmoduleStore::name()),
    ];
    stock_store_types.iter().all(|(directory, expected)| {
        std::fs::read_to_string(path.join(directory).join("type"))
            .is_ok_and(|actual| actual == *expected)
    })
}

fn repository_name(path: &Path) -> Option<RepositoryName> {
    let mut components = path.components();
    let Component::Normal(name) = components.next()? else {
        return None;
    };
    if components.next().is_some() {
        return None;
    }
    RepositoryName::parse(name.to_str()?.to_owned()).ok()
}

struct BareRepositoryLoader {
    path: PathBuf,
    selected_operation: Arc<OnceLock<OperationId>>,
}

impl WorkspaceLoader for BareRepositoryLoader {
    fn workspace_root(&self) -> &Path {
        &self.path
    }

    fn repo_path(&self) -> &Path {
        &self.path
    }

    fn load(
        &self,
        settings: &UserSettings,
        store_factories: &StoreFactories,
        _working_copy_factories: &WorkingCopyFactories,
    ) -> Result<Workspace, WorkspaceLoadError> {
        let file_loader = RepoLoader::init_from_file_system(settings, &self.path, store_factories)?;
        let repo_loader = RepoLoader::new(
            settings.clone(),
            file_loader.store().clone(),
            file_loader.op_store().clone(),
            Arc::new(ReadOnlyOpHeadsStore::new(
                file_loader.op_heads_store().clone(),
                file_loader.op_store().clone(),
                self.selected_operation.clone(),
            )),
            file_loader.index_store().clone(),
            file_loader.submodule_store().clone(),
        );
        let working_copy = BareWorkingCopy {
            operation_id: repo_loader.op_store().root_operation_id().clone(),
            tree: repo_loader.store().empty_merged_tree(),
            sparse_patterns: Vec::new(),
        };
        Ok(Workspace::new_no_canonicalize(
            self.path.clone(),
            self.path.clone(),
            Box::new(working_copy),
            repo_loader,
        ))
    }

    fn get_working_copy_type(&self) -> Result<String, jj_lib::repo::StoreLoadError> {
        Ok(BARE_WORKING_COPY_NAME.to_owned())
    }
}

#[derive(Debug)]
struct ReadOnlyOpHeadsStore {
    inner: Arc<dyn OpHeadsStore>,
    op_store: Arc<dyn OpStore>,
    selected: Arc<OnceLock<OperationId>>,
}

impl ReadOnlyOpHeadsStore {
    fn new(
        inner: Arc<dyn OpHeadsStore>,
        op_store: Arc<dyn OpStore>,
        selected: Arc<OnceLock<OperationId>>,
    ) -> Self {
        Self {
            inner,
            op_store,
            selected,
        }
    }
}

#[derive(Debug)]
pub(crate) struct MultipleOperationHeads(pub usize);

impl std::fmt::Display for MultipleOperationHeads {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Bare machine repository has {} operation heads; sync must reconcile them before read-only `log`.",
            self.0
        )
    }
}

impl std::error::Error for MultipleOperationHeads {}

struct ReadOnlyOpHeadsStoreLock;

impl OpHeadsStoreLock for ReadOnlyOpHeadsStoreLock {}

#[async_trait]
impl OpHeadsStore for ReadOnlyOpHeadsStore {
    fn name(&self) -> &str {
        "devspace_read_only_op_heads_store"
    }

    async fn update_op_heads(
        &self,
        _old_ids: &[OperationId],
        new_id: &OperationId,
    ) -> Result<(), OpHeadsStoreError> {
        Err(OpHeadsStoreError::Write {
            new_op_id: new_id.clone(),
            source: io::Error::new(
                io::ErrorKind::PermissionDenied,
                "bare machine repository operation heads are read-only",
            )
            .into(),
        })
    }

    async fn get_op_heads(&self) -> Result<Vec<OperationId>, OpHeadsStoreError> {
        if let Some(selected) = self.selected.get() {
            return Ok(vec![selected.clone()]);
        }

        let op_heads = match self.inner.get_op_heads().await {
            Ok(op_heads) if !op_heads.is_empty() => op_heads,
            Ok(_) | Err(OpHeadsStoreError::Read(_)) => self.inner.get_op_heads().await?,
            Err(error) => return Err(error),
        };
        if let Some(selected) = self.selected.get() {
            return Ok(vec![selected.clone()]);
        }
        if op_heads.is_empty() {
            return Err(OpHeadsStoreError::Read(
                "Bare machine repository has no operation head after a read retry".into(),
            ));
        }
        if let [op_head] = op_heads.as_slice() {
            let selected = self.selected.get_or_init(|| op_head.clone());
            return Ok(vec![selected.clone()]);
        }
        let mut operations = Vec::with_capacity(op_heads.len());
        for operation_id in op_heads {
            let operation = self
                .op_store
                .read_operation(&operation_id)
                .await
                .map_err(|error| OpHeadsStoreError::Read(Box::new(error)))?;
            operations.push(Operation::new(
                self.op_store.clone(),
                operation_id,
                operation,
            ));
        }
        let head_operations = dag_walk_async::heads(
            operations,
            |operation| operation.id().clone(),
            async |operation| operation.parents().await,
        )
        .await
        .map_err(|error| OpHeadsStoreError::Read(Box::new(error)))?;
        let Some(op_head) = head_operations.iter().next() else {
            return Err(OpHeadsStoreError::Read(
                "Bare machine repository has no operation head".into(),
            ));
        };
        if head_operations.len() != 1 {
            return Err(OpHeadsStoreError::Read(Box::new(MultipleOperationHeads(
                head_operations.len(),
            ))));
        }
        let selected = self.selected.get_or_init(|| op_head.id().clone());
        Ok(vec![selected.clone()])
    }

    async fn lock(&self) -> Result<Box<dyn OpHeadsStoreLock + '_>, OpHeadsStoreError> {
        Ok(Box::new(ReadOnlyOpHeadsStoreLock))
    }
}

struct BareWorkingCopy {
    operation_id: OperationId,
    tree: MergedTree,
    sparse_patterns: Vec<RepoPathBuf>,
}

#[async_trait(?Send)]
impl WorkingCopy for BareWorkingCopy {
    fn name(&self) -> &str {
        BARE_WORKING_COPY_NAME
    }

    fn workspace_name(&self) -> &WorkspaceName {
        WorkspaceName::new(BARE_WORKSPACE_NAME)
    }

    fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }

    fn tree(&self) -> Result<&MergedTree, WorkingCopyStateError> {
        Ok(&self.tree)
    }

    fn sparse_patterns(&self) -> Result<&[RepoPathBuf], WorkingCopyStateError> {
        Ok(&self.sparse_patterns)
    }

    async fn start_mutation(&self) -> Result<Box<dyn LockedWorkingCopy>, WorkingCopyStateError> {
        Ok(Box::new(LockedBareWorkingCopy {
            operation_id: self.operation_id.clone(),
            tree: self.tree.clone(),
            sparse_patterns: Vec::new(),
        }))
    }
}

struct LockedBareWorkingCopy {
    operation_id: OperationId,
    tree: MergedTree,
    sparse_patterns: Vec<RepoPathBuf>,
}

#[async_trait]
impl LockedWorkingCopy for LockedBareWorkingCopy {
    fn old_operation_id(&self) -> &OperationId {
        &self.operation_id
    }

    fn old_tree(&self) -> &MergedTree {
        &self.tree
    }

    async fn snapshot(
        &mut self,
        _options: &SnapshotOptions,
    ) -> Result<(MergedTree, SnapshotStats), SnapshotError> {
        Err(SnapshotError::Other {
            message: "bare machine repositories have no working copy to snapshot".to_owned(),
            err: unsupported_working_copy_error(),
        })
    }

    async fn check_out(&mut self, _commit: &Commit) -> Result<CheckoutStats, CheckoutError> {
        Err(CheckoutError::Other {
            message: "bare machine repositories have no working copy to update".to_owned(),
            err: unsupported_working_copy_error(),
        })
    }

    fn rename_workspace(&mut self, _new_workspace_name: WorkspaceNameBuf) {}

    async fn reset(&mut self, _commit: &Commit) -> Result<(), ResetError> {
        Err(ResetError::Other {
            message: "bare machine repositories have no working copy to reset".to_owned(),
            err: unsupported_working_copy_error(),
        })
    }

    async fn recover(&mut self, _commit: &Commit) -> Result<(), ResetError> {
        Err(ResetError::Other {
            message: "bare machine repositories have no working copy to recover".to_owned(),
            err: unsupported_working_copy_error(),
        })
    }

    fn sparse_patterns(&self) -> Result<&[RepoPathBuf], WorkingCopyStateError> {
        Ok(&self.sparse_patterns)
    }

    async fn set_sparse_patterns(
        &mut self,
        _new_sparse_patterns: Vec<RepoPathBuf>,
    ) -> Result<CheckoutStats, CheckoutError> {
        Err(CheckoutError::Other {
            message: "bare machine repositories have no sparse working copy".to_owned(),
            err: unsupported_working_copy_error(),
        })
    }

    async fn finish(
        self: Box<Self>,
        _operation_id: OperationId,
    ) -> Result<Box<dyn WorkingCopy>, WorkingCopyStateError> {
        Err(working_copy_state_error())
    }
}

fn unsupported_working_copy_error() -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(io::Error::new(
        io::ErrorKind::Unsupported,
        "bare machine repository",
    ))
}

fn working_copy_state_error() -> WorkingCopyStateError {
    WorkingCopyStateError {
        message: "bare machine repositories have no working-copy state".to_owned(),
        err: unsupported_working_copy_error(),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use clap::FromArgMatches as _;
    use jj_lib::backend::CommitId;
    use jj_lib::op_store::RootOperationData;

    use super::*;

    #[derive(Debug)]
    struct ChangingOpHeadsStore {
        heads: Mutex<Vec<OperationId>>,
    }

    #[derive(Debug)]
    struct SequencedOpHeadsStore {
        observations: Mutex<VecDeque<Vec<OperationId>>>,
    }

    #[async_trait]
    impl OpHeadsStore for ChangingOpHeadsStore {
        fn name(&self) -> &str {
            "changing"
        }

        async fn update_op_heads(
            &self,
            _old_ids: &[OperationId],
            _new_id: &OperationId,
        ) -> Result<(), OpHeadsStoreError> {
            panic!("read-only selection must not update operation heads")
        }

        async fn get_op_heads(&self) -> Result<Vec<OperationId>, OpHeadsStoreError> {
            Ok(self.heads.lock().unwrap().clone())
        }

        async fn lock(&self) -> Result<Box<dyn OpHeadsStoreLock + '_>, OpHeadsStoreError> {
            panic!("read-only selection must not lock operation heads")
        }
    }

    #[async_trait]
    impl OpHeadsStore for SequencedOpHeadsStore {
        fn name(&self) -> &str {
            "sequenced"
        }

        async fn update_op_heads(
            &self,
            _old_ids: &[OperationId],
            _new_id: &OperationId,
        ) -> Result<(), OpHeadsStoreError> {
            panic!("read-only selection must not update operation heads")
        }

        async fn get_op_heads(&self) -> Result<Vec<OperationId>, OpHeadsStoreError> {
            Ok(self.observations.lock().unwrap().pop_front().unwrap())
        }

        async fn lock(&self) -> Result<Box<dyn OpHeadsStoreLock + '_>, OpHeadsStoreError> {
            panic!("read-only selection must not lock operation heads")
        }
    }

    fn test_op_store(path: &Path) -> Arc<dyn OpStore> {
        Arc::new(
            SimpleOpStore::init(
                path,
                RootOperationData {
                    root_commit_id: CommitId::from_bytes(&[0]),
                },
            )
            .unwrap(),
        )
    }

    fn parsed_repository(args: &[&str]) -> Option<String> {
        let matches = jj_cli::commands::default_app()
            .try_get_matches_from(args)
            .unwrap();
        ParsedRepositoryArgs::from_arg_matches(&matches)
            .unwrap()
            .repository
    }

    #[test]
    fn clap_selection_ignores_repository_text_in_argument_values() {
        for args in [
            vec!["ds", "log", "--template=-R literal"],
            vec!["ds", "--config=aliases.literal=['log', '-Rdecoy']", "log"],
            vec!["ds", "log", "--", "-Rdecoy"],
        ] {
            assert_eq!(parsed_repository(&args), None, "{args:?}");
        }
    }

    #[test]
    fn catalog_selection_is_resolved_once_for_loader_and_dispatch() {
        let selector = RepositorySelector {
            cwd: Some(PathBuf::from("/tmp")),
            selection: OnceLock::new(),
            catalog_entry: OnceLock::new(),
        };
        selector.set_parsed_repository(Some("machine-repo".to_owned()));
        let calls = Cell::new(0);

        assert!(
            selector
                .catalog_entry_with(|_| {
                    calls.set(calls.get() + 1);
                    Ok(None)
                })
                .unwrap()
                .is_none()
        );
        assert!(
            selector
                .catalog_entry_with(|_| panic!("cached catalog selection must be reused"))
                .unwrap()
                .is_none()
        );
        assert_eq!(calls.get(), 1);
    }

    #[tokio::test]
    async fn selected_operation_is_shared_across_workspace_loads() {
        let first = OperationId::new(vec![1]);
        let second = OperationId::new(vec![2]);
        let inner = Arc::new(ChangingOpHeadsStore {
            heads: Mutex::new(vec![first.clone()]),
        });
        let selected = Arc::new(OnceLock::new());
        let temp = tempfile::tempdir().unwrap();
        let op_store = test_op_store(temp.path());
        let first_load =
            ReadOnlyOpHeadsStore::new(inner.clone(), op_store.clone(), selected.clone());
        assert_eq!(
            first_load.get_op_heads().await.unwrap(),
            vec![first.clone()]
        );

        *inner.heads.lock().unwrap() = vec![first.clone(), second];
        let stock_load = ReadOnlyOpHeadsStore::new(inner, op_store, selected);
        assert_eq!(stock_load.get_op_heads().await.unwrap(), vec![first]);
    }

    #[tokio::test]
    async fn empty_head_observation_is_retried_once() {
        let operation = OperationId::new(vec![1]);
        let inner = Arc::new(SequencedOpHeadsStore {
            observations: Mutex::new(VecDeque::from([vec![], vec![operation.clone()]])),
        });
        let selected = Arc::new(OnceLock::new());
        let temp = tempfile::tempdir().unwrap();
        let store = ReadOnlyOpHeadsStore::new(inner.clone(), test_op_store(temp.path()), selected);

        assert_eq!(store.get_op_heads().await.unwrap(), vec![operation]);
        assert!(inner.observations.lock().unwrap().is_empty());
    }
}
