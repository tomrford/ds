use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use jj_lib::settings::UserSettings;

use super::{
    MATERIALIZATION_LOCK_FILE, MachineStore, MachineStoreError, RepositoryIdentity, RepositoryName,
    RepositorySyncGuard,
};
use crate::{MachineGitRepository, sync_directory};

const CLONE_STAGING_DIRECTORY: &str = ".clone-staging";

pub struct StagedRepositoryClone {
    store: MachineStore,
    name: RepositoryName,
    identity: RepositoryIdentity,
    repository: Option<MachineGitRepository>,
    staging_directory: PathBuf,
    native_path: PathBuf,
    sync_path: PathBuf,
    packs_path: PathBuf,
    repository_directory: PathBuf,
    _sync_guard: RepositorySyncGuard,
    _materialization_lock: File,
}

impl StagedRepositoryClone {
    pub fn repository_mut(&mut self) -> &mut MachineGitRepository {
        self.repository
            .as_mut()
            .expect("staged repository is available before publication")
    }

    pub fn sync_path(&self) -> &Path {
        &self.sync_path
    }

    pub fn packs_path(&self) -> &Path {
        &self.packs_path
    }

    pub async fn publish(
        mut self,
        settings: &UserSettings,
    ) -> Result<MachineGitRepository, MachineStoreError> {
        drop(self.repository.take());
        self.store.require_binding(&self.name, &self.identity)?;
        for (from, to, component) in [
            (
                &self.packs_path,
                self.repository_directory.join("packs"),
                "packs",
            ),
            (
                &self.sync_path,
                self.repository_directory.join("sync"),
                "sync state",
            ),
            (
                &self.native_path,
                self.repository_directory.join("native"),
                "native repository",
            ),
        ] {
            fs::rename(from, &to).map_err(|source| MachineStoreError::PublishCloneComponent {
                component,
                from: from.to_path_buf(),
                to,
                source,
            })?;
            sync_directory(&self.repository_directory).map_err(|source| {
                MachineStoreError::SyncRepositoryParent {
                    path: self.repository_directory.clone(),
                    source,
                }
            })?;
        }
        fs::remove_dir(&self.staging_directory).map_err(|source| {
            MachineStoreError::RemoveCloneStaging {
                path: self.staging_directory.clone(),
                source,
            }
        })?;
        sync_directory(&self.repository_directory).map_err(|source| {
            MachineStoreError::SyncRepositoryParent {
                path: self.repository_directory.clone(),
                source,
            }
        })?;
        MachineGitRepository::open(self.repository_directory.join("native"), settings)
            .await
            .map_err(MachineStoreError::Repository)
    }
}

impl Drop for StagedRepositoryClone {
    fn drop(&mut self) {
        let _ = remove_disposable_path(&self.staging_directory);
    }
}

impl MachineStore {
    /// Builds an incomplete cloud clone under disposable staging.
    ///
    /// The repository sync lock is retained through publication. Checkout
    /// destination locks are acquired only after publication.
    pub async fn stage_repository_clone(
        &self,
        sync_guard: RepositorySyncGuard,
        name: &RepositoryName,
        expected: &RepositoryIdentity,
        settings: &UserSettings,
    ) -> Result<Option<StagedRepositoryClone>, MachineStoreError> {
        if &sync_guard.identity != expected {
            return Err(MachineStoreError::MismatchedRepositorySyncLock);
        }
        let entry = self.require_binding(name, expected)?;
        let repository_directory = entry
            .native_repository_path
            .parent()
            .expect("native repository path has an incarnation parent")
            .to_owned();
        fs::create_dir_all(&repository_directory).map_err(|source| {
            MachineStoreError::CreateRepositoryParent {
                path: repository_directory.clone(),
                source,
            }
        })?;

        let lock_path = repository_directory.join(MATERIALIZATION_LOCK_FILE);
        let materialization_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| MachineStoreError::OpenMaterializationLock {
                path: lock_path.clone(),
                source,
            })?;
        materialization_lock
            .lock()
            .map_err(|source| MachineStoreError::LockMaterialization {
                path: lock_path,
                source,
            })?;

        let entry = self.require_binding(name, expected)?;
        if entry.native_repository_path.exists() {
            return Ok(None);
        }

        let staging_directory = repository_directory.join(CLONE_STAGING_DIRECTORY);
        let published_sync_path = self.repository_sync_path(expected);
        let published_packs_path = self.repository_packs_path(expected);
        for path in [
            staging_directory.as_path(),
            published_sync_path.as_path(),
            published_packs_path.as_path(),
        ] {
            remove_disposable_path(path).map_err(|source| {
                MachineStoreError::RemoveIncompleteCloneState {
                    path: path.to_owned(),
                    source,
                }
            })?;
        }
        sync_directory(&repository_directory).map_err(|source| {
            MachineStoreError::SyncRepositoryParent {
                path: repository_directory.clone(),
                source,
            }
        })?;
        fs::create_dir(&staging_directory).map_err(|source| {
            MachineStoreError::CreateRepositoryStaging {
                path: staging_directory.clone(),
                source,
            }
        })?;
        let native_path = staging_directory.join("native");
        let sync_path = staging_directory.join("sync");
        let packs_path = staging_directory.join("packs");
        fs::create_dir(&packs_path).map_err(|source| {
            MachineStoreError::CreateRepositoryStaging {
                path: packs_path.clone(),
                source,
            }
        })?;
        let repository = MachineGitRepository::init(&native_path, settings)
            .await
            .map_err(MachineStoreError::Repository)?;
        sync_directory(&repository_directory).map_err(|source| {
            MachineStoreError::SyncRepositoryParent {
                path: repository_directory.clone(),
                source,
            }
        })?;
        Ok(Some(StagedRepositoryClone {
            store: self.clone(),
            name: name.clone(),
            identity: expected.clone(),
            repository: Some(repository),
            staging_directory,
            native_path,
            sync_path,
            packs_path,
            repository_directory,
            _sync_guard: sync_guard,
            _materialization_lock: materialization_lock,
        }))
    }
}

fn remove_disposable_path(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
