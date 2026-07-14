use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jj_lib::default_index::DefaultIndexStore;
use jj_lib::default_submodule_store::DefaultSubmoduleStore;
use jj_lib::repo::{ReadonlyRepo, RepoInitError, RepoLoader, RepoLoaderError, StoreFactories};
use jj_lib::settings::UserSettings;
use jj_lib::signing::{SignInitError, Signer};
use jj_lib::simple_backend::SimpleBackend;
use jj_lib::simple_op_heads_store::SimpleOpHeadsStore;
use jj_lib::simple_op_store::SimpleOpStore;
use thiserror::Error;

/// A native jj repository in a machine store.
///
/// Devspace owns replication around this repository, not replacements for its
/// backend, operation store, or operation-head store.
pub struct MachineRepository {
    path: PathBuf,
    repo: Arc<ReadonlyRepo>,
}

impl MachineRepository {
    pub async fn init(
        path: impl AsRef<Path>,
        settings: &UserSettings,
    ) -> Result<Self, MachineRepositoryError> {
        let path = path.as_ref();
        fs::create_dir_all(path).map_err(|source| MachineRepositoryError::CreateRepository {
            path: path.to_path_buf(),
            source,
        })?;

        let signer = Signer::from_settings(settings)?;
        let repo = ReadonlyRepo::init(
            settings,
            path,
            &|_settings, store_path| Ok(Box::new(SimpleBackend::init(store_path))),
            signer,
            ReadonlyRepo::default_op_store_initializer(),
            ReadonlyRepo::default_op_heads_store_initializer(),
            ReadonlyRepo::default_index_store_initializer(),
            ReadonlyRepo::default_submodule_store_initializer(),
        )
        .await?;

        Ok(Self {
            path: path.to_path_buf(),
            repo,
        })
    }

    pub async fn open(
        path: impl AsRef<Path>,
        settings: &UserSettings,
    ) -> Result<Self, MachineRepositoryError> {
        let path = path.as_ref();
        require_store_type(path, "store", SimpleBackend::name())?;
        require_store_type(path, "op_store", SimpleOpStore::name())?;
        require_store_type(path, "op_heads", SimpleOpHeadsStore::name())?;
        require_store_type(path, "index", DefaultIndexStore::name())?;
        require_store_type(path, "submodule_store", DefaultSubmoduleStore::name())?;

        let loader = RepoLoader::init_from_file_system(settings, path, &StoreFactories::default())?;
        let repo = loader.load_at_head().await?;
        Ok(Self {
            path: path.to_path_buf(),
            repo,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn repo(&self) -> &Arc<ReadonlyRepo> {
        &self.repo
    }
}

fn require_store_type(
    root: &Path,
    directory: &'static str,
    expected: &'static str,
) -> Result<(), MachineRepositoryError> {
    let path = root.join(directory).join("type");
    let actual =
        fs::read_to_string(&path).map_err(|source| MachineRepositoryError::ReadStoreType {
            path: path.clone(),
            source,
        })?;
    if actual == expected {
        Ok(())
    } else {
        Err(MachineRepositoryError::UnsupportedStore {
            store: directory,
            expected,
            actual,
        })
    }
}

#[derive(Debug, Error)]
pub enum MachineRepositoryError {
    #[error("failed to create machine repository at {path}")]
    CreateRepository {
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
    #[error(transparent)]
    Initialize(#[from] RepoInitError),
    #[error(transparent)]
    LoadStores(#[from] jj_lib::repo::StoreLoadError),
    #[error(transparent)]
    LoadRepository(#[from] RepoLoaderError),
    #[error(transparent)]
    Signing(#[from] SignInitError),
}
