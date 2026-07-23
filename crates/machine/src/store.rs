use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jj_lib::default_index::DefaultIndexStore;
use jj_lib::default_submodule_store::DefaultSubmoduleStore;
use jj_lib::git_backend::GitBackend;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_heads_store::OpHeadsStoreError;
use jj_lib::op_store::OperationId;
use jj_lib::repo::{
    ReadonlyRepo, Repo as _, RepoInitError, RepoLoader, RepoLoaderError, StoreFactories,
    StoreLoadError,
};
use jj_lib::settings::UserSettings;
use jj_lib::signing::{SignInitError, Signer};
use jj_lib::simple_op_heads_store::SimpleOpHeadsStore;
use jj_lib::simple_op_store::SimpleOpStore;
use thiserror::Error;

use crate::OpId;

/// A jj repository whose canonical commit/tree/blob storage is a bare Git ODB.
pub struct MachineGitRepository {
    path: PathBuf,
    repo: Arc<ReadonlyRepo>,
    git_repo_path: PathBuf,
}

impl MachineGitRepository {
    pub async fn init(
        path: impl AsRef<Path>,
        settings: &UserSettings,
    ) -> Result<Self, MachineGitRepositoryError> {
        let path = path.as_ref();
        fs::create_dir_all(path).map_err(|source| MachineGitRepositoryError::CreateRepository {
            path: path.to_owned(),
            source,
        })?;
        let signer = Signer::from_settings(settings)?;
        let repo = ReadonlyRepo::init(
            settings,
            path,
            &|settings, store_path| Ok(Box::new(GitBackend::init_internal(settings, store_path)?)),
            signer,
            ReadonlyRepo::default_op_store_initializer(),
            ReadonlyRepo::default_op_heads_store_initializer(),
            ReadonlyRepo::default_index_store_initializer(),
            ReadonlyRepo::default_submodule_store_initializer(),
        )
        .await?;
        Self::from_repo(path, repo)
    }

    pub async fn open(
        path: impl AsRef<Path>,
        settings: &UserSettings,
    ) -> Result<Self, MachineGitRepositoryError> {
        let path = path.as_ref();
        require_store_type(path, "store", GitBackend::name())?;
        require_store_type(path, "op_store", SimpleOpStore::name())?;
        require_store_type(path, "op_heads", SimpleOpHeadsStore::name())?;
        require_store_type(path, "index", DefaultIndexStore::name())?;
        require_store_type(path, "submodule_store", DefaultSubmoduleStore::name())?;
        // `extra/` is a cache, but GitBackend::load expects its TableStore
        // scaffolding to exist. Recreate only that empty scaffolding; commit
        // reads repopulate it from Git object headers or synthetic IDs.
        let extra_heads = path.join("store/extra/heads");
        fs::create_dir_all(&extra_heads).map_err(|source| {
            MachineGitRepositoryError::CreateExtraCache {
                path: extra_heads,
                source,
            }
        })?;
        let loader = RepoLoader::init_from_file_system(settings, path, &StoreFactories::default())?;
        let repo = loader.load_at_head().await?;
        Self::from_repo(path, repo)
    }

    fn from_repo(path: &Path, repo: Arc<ReadonlyRepo>) -> Result<Self, MachineGitRepositoryError> {
        let backend = repo
            .store()
            .backend_impl::<GitBackend>()
            .ok_or(MachineGitRepositoryError::UnexpectedBackend)?;
        Ok(Self {
            path: path.to_owned(),
            git_repo_path: backend.git_repo_path().to_owned(),
            repo,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn repo(&self) -> &Arc<ReadonlyRepo> {
        &self.repo
    }

    pub fn git_repo_path(&self) -> &Path {
        &self.git_repo_path
    }

    /// Stock jj simple operation-store location. Git packs do not include it.
    pub fn operation_store_path(&self) -> PathBuf {
        self.path.join("op_store")
    }

    /// Stock jj simple operation-head-store location. Git packs do not include it.
    pub fn operation_heads_path(&self) -> PathBuf {
        self.path.join("op_heads")
    }

    pub(crate) fn git_repo(&self) -> gix::Repository {
        self.repo
            .store()
            .backend_impl::<GitBackend>()
            .expect("backend type checked at construction")
            .git_repo()
    }

    pub async fn current_operation_heads(&self) -> Result<Vec<OpId>, OpReconcileError> {
        let mut heads = self
            .repo
            .op_heads_store()
            .get_op_heads()
            .await?
            .into_iter()
            .map(|id| {
                id.as_bytes()
                    .try_into()
                    .map_err(|_| OpReconcileError::InvalidIdLength(id.as_bytes().len()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        heads.sort_unstable();
        heads.dedup();
        Ok(heads)
    }

    pub async fn reconcile_operation_heads(
        &mut self,
        cloud_heads: &BTreeSet<OpId>,
    ) -> Result<OperationId, OpReconcileError> {
        let op_heads_store = self.repo.op_heads_store().clone();
        {
            let _lock = op_heads_store.lock().await?;
            for head in cloud_heads {
                op_heads_store
                    .update_op_heads(&[], &OperationId::new(head.to_vec()))
                    .await?;
            }
        }
        let repo = self.repo.loader().load_at_head().await?;
        let operation = repo.op_id().clone();
        self.repo = repo;
        Ok(operation)
    }
}

fn require_store_type(
    root: &Path,
    directory: &'static str,
    expected: &'static str,
) -> Result<(), MachineGitRepositoryError> {
    let path = root.join(directory).join("type");
    let actual =
        fs::read_to_string(&path).map_err(|source| MachineGitRepositoryError::ReadStoreType {
            path: path.clone(),
            source,
        })?;
    if actual == expected {
        Ok(())
    } else {
        Err(MachineGitRepositoryError::UnsupportedStore {
            store: directory,
            expected,
            actual,
        })
    }
}

#[derive(Debug, Error)]
pub enum MachineGitRepositoryError {
    #[error("failed to create machine Git repository at {path}")]
    CreateRepository {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to recreate empty Git metadata cache at {path}")]
    CreateExtraCache {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to read machine repository store type at {path}")]
    ReadStoreType {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("machine repository {store} type must be {expected}, got {actual:?}")]
    UnsupportedStore {
        store: &'static str,
        expected: &'static str,
        actual: String,
    },
    #[error("jj loaded a non-Git backend for the machine Git repository")]
    UnexpectedBackend,
    #[error(transparent)]
    Init(#[from] RepoInitError),
    #[error(transparent)]
    Load(#[from] RepoLoaderError),
    #[error(transparent)]
    StoreLoad(#[from] StoreLoadError),
    #[error(transparent)]
    Sign(#[from] SignInitError),
}

#[derive(Debug, Error)]
pub enum OpReconcileError {
    #[error("native operation ID must be 64 bytes, got {0}")]
    InvalidIdLength(usize),
    #[error(transparent)]
    Heads(#[from] OpHeadsStoreError),
    #[error(transparent)]
    Load(#[from] RepoLoaderError),
}
