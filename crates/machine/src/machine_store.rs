use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use jj_lib::settings::UserSettings;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{MachineRepository, MachineRepositoryError};

pub const MACHINE_STORE_OVERRIDE: &str = "DEVSPACE_MACHINE_STORE_DIR";

const CATALOG_VERSION: u32 = 1;
const CATALOG_FILE: &str = "repositories.json";
const CATALOG_LOCK_FILE: &str = "repositories.lock";
const MATERIALIZATION_LOCK_FILE: &str = "native.lock";
const MATERIALIZATION_TEMP_PREFIX: &str = ".native-";
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RepositoryName(String);

impl RepositoryName {
    pub fn parse(value: impl Into<String>) -> Result<Self, MachineStoreError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 128
            || !value.is_ascii()
            || !value.bytes().enumerate().all(|(index, byte)| match byte {
                b'a'..=b'z' | b'0'..=b'9' => true,
                b'.' | b'_' | b'-' => index != 0,
                _ => false,
            })
        {
            return Err(MachineStoreError::InvalidRepositoryName(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RepositoryName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryId(String);

impl RepositoryId {
    pub fn parse(value: impl Into<String>) -> Result<Self, MachineStoreError> {
        let value = value.into();
        validate_lower_hex(&value, 64, "repository ID")?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RepositoryId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryIncarnation(String);

impl RepositoryIncarnation {
    pub fn parse(value: impl Into<String>) -> Result<Self, MachineStoreError> {
        let value = value.into();
        validate_lower_hex(&value, 32, "repository incarnation")?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RepositoryIncarnation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryIdentity {
    pub repository_id: RepositoryId,
    pub incarnation: RepositoryIncarnation,
}

impl RepositoryIdentity {
    pub fn new(repository_id: RepositoryId, incarnation: RepositoryIncarnation) -> Self {
        Self {
            repository_id,
            incarnation,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogEntry {
    pub name: RepositoryName,
    pub identity: RepositoryIdentity,
    pub native_repository_path: PathBuf,
}

/// The durable machine-local directory and native-repository owner.
///
/// Catalog mutations serialize through one machine-local file lock and replace
/// the catalog only after the new file has reached durable storage. Native
/// repository paths contain only opaque cloud identity components, and each
/// identity has a materialization lock protecting atomic repository publication.
#[derive(Clone, Debug)]
pub struct MachineStore {
    root: PathBuf,
}

impl MachineStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn platform_default() -> Result<Self, MachineStoreError> {
        if let Some(root) = env::var_os(MACHINE_STORE_OVERRIDE) {
            if root.is_empty() {
                return Err(MachineStoreError::EmptyRootOverride);
            }
            return Ok(Self::new(root));
        }
        Ok(Self::new(platform_data_directory()?))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn catalog_path(&self) -> PathBuf {
        self.root.join(CATALOG_FILE)
    }

    pub fn native_repository_path(&self, identity: &RepositoryIdentity) -> PathBuf {
        self.root
            .join("repositories")
            .join(&identity.repository_id.as_str()[..2])
            .join(identity.repository_id.as_str())
            .join(identity.incarnation.as_str())
            .join("native")
    }

    pub fn resolve(
        &self,
        name: &RepositoryName,
    ) -> Result<Option<CatalogEntry>, MachineStoreError> {
        if !self.root.exists() {
            return Ok(None);
        }
        self.with_catalog_lock(false, |catalog| {
            catalog
                .repositories
                .get(name.as_str())
                .map(|persisted| self.entry_from_persisted(name.clone(), persisted))
                .transpose()
        })
    }

    pub fn entries(&self) -> Result<Vec<CatalogEntry>, MachineStoreError> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        self.with_catalog_lock(false, |catalog| {
            catalog
                .repositories
                .iter()
                .map(|(name, persisted)| {
                    self.entry_from_persisted(RepositoryName::parse(name.clone())?, persisted)
                })
                .collect()
        })
    }

    pub fn register_repository(
        &self,
        name: RepositoryName,
        identity: RepositoryIdentity,
    ) -> Result<CatalogEntry, MachineStoreError> {
        fs::create_dir_all(&self.root).map_err(|source| MachineStoreError::CreateRoot {
            path: self.root.clone(),
            source,
        })?;
        self.with_catalog_lock(true, |catalog| {
            if let Some(existing) = catalog.repositories.get(name.as_str()) {
                let existing = parse_identity(existing)?;
                if existing == identity {
                    return Ok(self.entry(name.clone(), identity.clone()));
                }
                if existing.repository_id == identity.repository_id {
                    return Err(MachineStoreError::StaleIncarnation {
                        name: name.clone(),
                        registered: existing.incarnation,
                        requested: identity.incarnation.clone(),
                    });
                }
                return Err(MachineStoreError::ConflictingName {
                    name: name.clone(),
                    registered: existing,
                    requested: identity.clone(),
                });
            }

            for (existing_name, existing) in &catalog.repositories {
                let existing = parse_identity(existing)?;
                if existing.repository_id == identity.repository_id {
                    if existing.incarnation != identity.incarnation {
                        return Err(MachineStoreError::StaleRepositoryIdentity {
                            repository_id: identity.repository_id.clone(),
                            registered_name: RepositoryName::parse(existing_name.clone())?,
                            registered: existing.incarnation,
                            requested: identity.incarnation.clone(),
                        });
                    }
                    return Err(MachineStoreError::ConflictingIdentity {
                        repository_id: identity.repository_id.clone(),
                        registered_name: RepositoryName::parse(existing_name.clone())?,
                        requested_name: name.clone(),
                    });
                }
            }

            catalog
                .repositories
                .insert(name.as_str().to_owned(), PersistedEntry::from(&identity));
            self.persist_catalog(catalog)?;
            Ok(self.entry(name, identity))
        })
    }

    pub fn unregister_repository(
        &self,
        name: &RepositoryName,
        expected: &RepositoryIdentity,
    ) -> Result<Option<CatalogEntry>, MachineStoreError> {
        if !self.root.exists() {
            return Ok(None);
        }
        self.with_catalog_lock(true, |catalog| {
            let Some(persisted) = catalog.repositories.get(name.as_str()) else {
                return Ok(None);
            };
            let registered = parse_identity(persisted)?;
            if &registered != expected {
                return Err(MachineStoreError::StaleRemoval {
                    name: name.clone(),
                    registered,
                    requested: expected.clone(),
                });
            }
            catalog.repositories.remove(name.as_str());
            self.persist_catalog(catalog)?;
            Ok(Some(self.entry(name.clone(), expected.clone())))
        })
    }

    pub async fn materialize_repository(
        &self,
        name: &RepositoryName,
        expected: &RepositoryIdentity,
        settings: &UserSettings,
    ) -> Result<MachineRepository, MachineStoreError> {
        let entry = self.require_binding(name, expected)?;
        let parent = entry
            .native_repository_path
            .parent()
            .expect("native repository path has an incarnation parent");
        fs::create_dir_all(parent).map_err(|source| MachineStoreError::CreateRepositoryParent {
            path: parent.to_owned(),
            source,
        })?;

        let lock_path = parent.join(MATERIALIZATION_LOCK_FILE);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| MachineStoreError::OpenMaterializationLock {
                path: lock_path.clone(),
                source,
            })?;
        lock.lock()
            .map_err(|source| MachineStoreError::LockMaterialization {
                path: lock_path,
                source,
            })?;

        // The catalog can change while this caller waits for another initializer.
        let entry = self.require_binding(name, expected)?;
        if entry.native_repository_path.exists() {
            return MachineRepository::open(&entry.native_repository_path, settings)
                .await
                .map_err(MachineStoreError::Repository);
        }

        let staging = tempfile::Builder::new()
            .prefix(MATERIALIZATION_TEMP_PREFIX)
            .tempdir_in(parent)
            .map_err(|source| MachineStoreError::CreateRepositoryStaging {
                path: parent.to_owned(),
                source,
            })?;
        let repository = MachineRepository::init(staging.path(), settings)
            .await
            .map_err(MachineStoreError::Repository)?;
        drop(repository);

        // Avoid publishing work for a binding removed or replaced during init.
        self.require_binding(name, expected)?;
        fs::rename(staging.path(), &entry.native_repository_path).map_err(|source| {
            MachineStoreError::PublishRepository {
                from: staging.path().to_owned(),
                to: entry.native_repository_path.clone(),
                source,
            }
        })?;
        sync_directory(parent).map_err(|source| MachineStoreError::SyncRepositoryParent {
            path: parent.to_owned(),
            source,
        })?;

        MachineRepository::open(&entry.native_repository_path, settings)
            .await
            .map_err(MachineStoreError::Repository)
    }

    pub async fn open_repository(
        &self,
        name: &RepositoryName,
        settings: &UserSettings,
    ) -> Result<MachineRepository, MachineStoreError> {
        let entry = self
            .resolve(name)?
            .ok_or_else(|| MachineStoreError::RepositoryNotRegistered(name.clone()))?;
        MachineRepository::open(entry.native_repository_path, settings)
            .await
            .map_err(MachineStoreError::Repository)
    }

    fn require_binding(
        &self,
        name: &RepositoryName,
        expected: &RepositoryIdentity,
    ) -> Result<CatalogEntry, MachineStoreError> {
        let entry = self
            .resolve(name)?
            .ok_or_else(|| MachineStoreError::RepositoryNotRegistered(name.clone()))?;
        if &entry.identity != expected {
            return Err(MachineStoreError::StaleMaterialization {
                name: name.clone(),
                registered: entry.identity,
                requested: expected.clone(),
            });
        }
        Ok(entry)
    }

    fn entry(&self, name: RepositoryName, identity: RepositoryIdentity) -> CatalogEntry {
        let native_repository_path = self.native_repository_path(&identity);
        CatalogEntry {
            name,
            identity,
            native_repository_path,
        }
    }

    fn entry_from_persisted(
        &self,
        name: RepositoryName,
        persisted: &PersistedEntry,
    ) -> Result<CatalogEntry, MachineStoreError> {
        Ok(self.entry(name, parse_identity(persisted)?))
    }

    fn with_catalog_lock<T>(
        &self,
        exclusive: bool,
        operation: impl FnOnce(&mut PersistedCatalog) -> Result<T, MachineStoreError>,
    ) -> Result<T, MachineStoreError> {
        let lock_path = self.root.join(CATALOG_LOCK_FILE);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| MachineStoreError::OpenCatalogLock {
                path: lock_path.clone(),
                source,
            })?;
        if exclusive {
            lock.lock()
        } else {
            lock.lock_shared()
        }
        .map_err(|source| MachineStoreError::LockCatalog {
            path: lock_path,
            source,
        })?;

        let mut catalog = self.read_catalog()?;
        validate_catalog(&catalog)?;
        operation(&mut catalog)
    }

    fn read_catalog(&self) -> Result<PersistedCatalog, MachineStoreError> {
        let path = self.catalog_path();
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|source| MachineStoreError::ReadCatalog { path, source }),
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                Ok(PersistedCatalog::default())
            }
            Err(source) => Err(MachineStoreError::OpenCatalog { path, source }),
        }
    }

    fn persist_catalog(&self, catalog: &PersistedCatalog) -> Result<(), MachineStoreError> {
        let path = self.catalog_path();
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp_path = self.root.join(format!(
            ".{CATALOG_FILE}.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        let mut temp = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|source| MachineStoreError::WriteCatalog {
                path: temp_path.clone(),
                source,
            })?;
        let result = (|| {
            serde_json::to_writer_pretty(&mut temp, catalog).map_err(|source| {
                MachineStoreError::SerializeCatalog {
                    path: temp_path.clone(),
                    source,
                }
            })?;
            temp.write_all(b"\n")
                .and_then(|()| temp.sync_all())
                .map_err(|source| MachineStoreError::WriteCatalog {
                    path: temp_path.clone(),
                    source,
                })?;
            fs::rename(&temp_path, &path).map_err(|source| MachineStoreError::ReplaceCatalog {
                from: temp_path.clone(),
                to: path,
                source,
            })?;
            sync_directory(&self.root).map_err(|source| MachineStoreError::SyncRoot {
                path: self.root.clone(),
                source,
            })
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp_path);
        }
        result
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedCatalog {
    version: u32,
    repositories: BTreeMap<String, PersistedEntry>,
}

impl Default for PersistedCatalog {
    fn default() -> Self {
        Self {
            version: CATALOG_VERSION,
            repositories: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedEntry {
    repository_id: String,
    incarnation: String,
}

impl From<&RepositoryIdentity> for PersistedEntry {
    fn from(identity: &RepositoryIdentity) -> Self {
        Self {
            repository_id: identity.repository_id.as_str().to_owned(),
            incarnation: identity.incarnation.as_str().to_owned(),
        }
    }
}

fn parse_identity(persisted: &PersistedEntry) -> Result<RepositoryIdentity, MachineStoreError> {
    Ok(RepositoryIdentity::new(
        RepositoryId::parse(persisted.repository_id.clone())?,
        RepositoryIncarnation::parse(persisted.incarnation.clone())?,
    ))
}

fn validate_catalog(catalog: &PersistedCatalog) -> Result<(), MachineStoreError> {
    if catalog.version != CATALOG_VERSION {
        return Err(MachineStoreError::UnsupportedCatalogVersion(
            catalog.version,
        ));
    }
    let mut identities = BTreeMap::<String, (RepositoryName, RepositoryIncarnation)>::new();
    for (name, persisted) in &catalog.repositories {
        let name = RepositoryName::parse(name.clone())?;
        let identity = parse_identity(persisted)?;
        if let Some((other_name, other_incarnation)) = identities.insert(
            identity.repository_id.as_str().to_owned(),
            (name.clone(), identity.incarnation.clone()),
        ) {
            return Err(MachineStoreError::InvalidCatalogBinding {
                repository_id: identity.repository_id,
                first_name: other_name,
                second_name: name,
                first_incarnation: other_incarnation,
                second_incarnation: identity.incarnation,
            });
        }
    }
    Ok(())
}

fn validate_lower_hex(
    value: &str,
    length: usize,
    field: &'static str,
) -> Result<(), MachineStoreError> {
    if value.len() != length
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(MachineStoreError::InvalidOpaqueIdentity {
            field,
            value: value.to_owned(),
            length,
        });
    }
    Ok(())
}

fn platform_data_directory() -> Result<PathBuf, MachineStoreError> {
    #[cfg(target_os = "macos")]
    {
        let home = env::var_os("HOME").ok_or(MachineStoreError::PlatformDataDirectory)?;
        return Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("devspace"));
    }
    #[cfg(target_os = "windows")]
    {
        let local = env::var_os("LOCALAPPDATA").ok_or(MachineStoreError::PlatformDataDirectory)?;
        return Ok(PathBuf::from(local).join("devspace"));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(data) = env::var_os("XDG_DATA_HOME") {
            return Ok(PathBuf::from(data).join("devspace"));
        }
        let home = env::var_os("HOME").ok_or(MachineStoreError::PlatformDataDirectory)?;
        return Ok(PathBuf::from(home).join(".local/share/devspace"));
    }
    #[allow(unreachable_code)]
    Err(MachineStoreError::PlatformDataDirectory)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[derive(Debug, Error)]
pub enum MachineStoreError {
    #[error("repository name {0:?} must match [a-z0-9][a-z0-9._-]{{0,127}}")]
    InvalidRepositoryName(String),
    #[error("{field} {value:?} must be exactly {length} lowercase hexadecimal characters")]
    InvalidOpaqueIdentity {
        field: &'static str,
        value: String,
        length: usize,
    },
    #[error("{MACHINE_STORE_OVERRIDE} must not be empty")]
    EmptyRootOverride,
    #[error("the platform data directory is unavailable")]
    PlatformDataDirectory,
    #[error("failed to create machine-store root at {path}")]
    CreateRoot {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to open machine-store catalog lock at {path}")]
    OpenCatalogLock {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to lock machine-store catalog at {path}")]
    LockCatalog {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to open machine-store catalog at {path}")]
    OpenCatalog {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to decode machine-store catalog at {path}")]
    ReadCatalog {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("machine-store catalog version {0} is unsupported")]
    UnsupportedCatalogVersion(u32),
    #[error("failed to serialize machine-store catalog at {path}")]
    SerializeCatalog {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to write machine-store catalog at {path}")]
    WriteCatalog {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to atomically replace machine-store catalog {to} from {from}")]
    ReplaceCatalog {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to sync machine-store root at {path}")]
    SyncRoot {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("repository name {name} is already bound to {registered:?}, not {requested:?}")]
    ConflictingName {
        name: RepositoryName,
        registered: RepositoryIdentity,
        requested: RepositoryIdentity,
    },
    #[error(
        "repository name {name} is registered at incarnation {registered}, not stale incarnation {requested}"
    )]
    StaleIncarnation {
        name: RepositoryName,
        registered: RepositoryIncarnation,
        requested: RepositoryIncarnation,
    },
    #[error(
        "repository ID {repository_id} is already bound to name {registered_name}, not {requested_name}"
    )]
    ConflictingIdentity {
        repository_id: RepositoryId,
        registered_name: RepositoryName,
        requested_name: RepositoryName,
    },
    #[error(
        "repository ID {repository_id} is registered as {registered_name} at incarnation {registered}, not stale incarnation {requested}"
    )]
    StaleRepositoryIdentity {
        repository_id: RepositoryId,
        registered_name: RepositoryName,
        registered: RepositoryIncarnation,
        requested: RepositoryIncarnation,
    },
    #[error(
        "catalog binds repository ID {repository_id} to both {first_name} ({first_incarnation}) and {second_name} ({second_incarnation})"
    )]
    InvalidCatalogBinding {
        repository_id: RepositoryId,
        first_name: RepositoryName,
        second_name: RepositoryName,
        first_incarnation: RepositoryIncarnation,
        second_incarnation: RepositoryIncarnation,
    },
    #[error("repository {0} is not registered in this machine store")]
    RepositoryNotRegistered(RepositoryName),
    #[error(
        "repository {name} cannot be materialized as {requested:?}; the catalog contains {registered:?}"
    )]
    StaleMaterialization {
        name: RepositoryName,
        registered: RepositoryIdentity,
        requested: RepositoryIdentity,
    },
    #[error(
        "repository {name} cannot be unregistered as {requested:?}; the catalog contains {registered:?}"
    )]
    StaleRemoval {
        name: RepositoryName,
        registered: RepositoryIdentity,
        requested: RepositoryIdentity,
    },
    #[error("failed to create native repository parent at {path}")]
    CreateRepositoryParent {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to open native repository materialization lock at {path}")]
    OpenMaterializationLock {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to lock native repository materialization at {path}")]
    LockMaterialization {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to create native repository staging directory under {path}")]
    CreateRepositoryStaging {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to atomically publish native repository {to} from {from}")]
    PublishRepository {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to sync native repository parent at {path}")]
    SyncRepositoryParent {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Repository(#[from] MachineRepositoryError),
}
