use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    MachineConfig, MachineId, MachineStore, MachineStoreError, RepositoryId, RepositoryIdentity,
    RepositoryIncarnation, RepositoryName,
};

const JOURNAL_VERSION: u32 = 1;
const JOURNAL_FILE: &str = "repository-creations.json";
const JOURNAL_LOCK_FILE: &str = "repository-creations.lock";
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Eq, PartialEq)]
pub struct RepositoryCreationKey([u8; 16]);

impl RepositoryCreationKey {
    pub fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub fn bytes(&self) -> [u8; 16] {
        self.0
    }
}

impl fmt::Debug for RepositoryCreationKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RepositoryCreationKey([REDACTED])")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryCreationTarget {
    base_url: String,
    machine_id: MachineId,
}

impl RepositoryCreationTarget {
    pub fn from_config(config: &MachineConfig) -> Self {
        Self {
            base_url: config.base_url().to_owned(),
            machine_id: config.machine_id().clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryCreationIntent {
    name: RepositoryName,
    key: RepositoryCreationKey,
    target: RepositoryCreationTarget,
    identity: Option<RepositoryIdentity>,
    complete: bool,
}

impl RepositoryCreationIntent {
    pub fn name(&self) -> &RepositoryName {
        &self.name
    }

    pub fn key(&self) -> &RepositoryCreationKey {
        &self.key
    }

    pub fn identity(&self) -> Option<&RepositoryIdentity> {
        self.identity.as_ref()
    }

    pub fn is_complete(&self) -> bool {
        self.complete
    }
}

impl MachineStore {
    /// Starts a durable create saga or resumes the exact request already recorded.
    ///
    /// The proposed key is ignored when an intent already exists. Completed
    /// records are retained so a retry after the final local publication can be
    /// distinguished from an unrelated repository already in the catalog.
    pub fn begin_repository_creation(
        &self,
        name: RepositoryName,
        target: RepositoryCreationTarget,
        proposed_key: RepositoryCreationKey,
    ) -> Result<RepositoryCreationIntent, RepositoryCreationIntentError> {
        fs::create_dir_all(self.root()).map_err(|source| {
            RepositoryCreationIntentError::CreateRoot {
                path: self.root().to_owned(),
                source,
            }
        })?;
        self.with_creation_journal(true, |journal| {
            if let Some(persisted) = journal.creations.get(name.as_str()) {
                let intent = parse_intent(name.clone(), persisted)?;
                if intent.target != target {
                    return Err(RepositoryCreationIntentError::TargetChanged(name));
                }
                if intent.complete {
                    let recorded = intent
                        .identity
                        .as_ref()
                        .expect("completed creation intents have a cloud identity");
                    let binding = self.resolve(&name)?;
                    if binding
                        .as_ref()
                        .is_none_or(|entry| &entry.identity != recorded)
                    {
                        return Err(RepositoryCreationIntentError::CompletedBindingChanged(name));
                    }
                }
                return Ok(intent);
            }
            if self.resolve(&name)?.is_some() {
                return Err(RepositoryCreationIntentError::AlreadyRegistered(name));
            }
            let intent = RepositoryCreationIntent {
                name: name.clone(),
                key: proposed_key,
                target,
                identity: None,
                complete: false,
            };
            journal
                .creations
                .insert(name.as_str().to_owned(), PersistedIntent::from(&intent));
            self.persist_creation_journal(journal)?;
            Ok(intent)
        })
    }

    /// Durably records the cloud response before local catalog publication.
    pub fn record_repository_created(
        &self,
        intent: &RepositoryCreationIntent,
        identity: RepositoryIdentity,
    ) -> Result<RepositoryCreationIntent, RepositoryCreationIntentError> {
        self.with_creation_journal(true, |journal| {
            let current = current_intent(journal, intent)?;
            if let Some(recorded) = &current.identity {
                if recorded != &identity {
                    return Err(RepositoryCreationIntentError::DifferentCloudIdentity(
                        intent.name.clone(),
                    ));
                }
                return Ok(current);
            }
            let updated = RepositoryCreationIntent {
                identity: Some(identity),
                ..current
            };
            journal.creations.insert(
                updated.name.as_str().to_owned(),
                PersistedIntent::from(&updated),
            );
            self.persist_creation_journal(journal)?;
            Ok(updated)
        })
    }

    /// Marks local catalog registration and atomic native materialization complete.
    pub fn complete_repository_creation(
        &self,
        intent: &RepositoryCreationIntent,
    ) -> Result<RepositoryCreationIntent, RepositoryCreationIntentError> {
        self.with_creation_journal(true, |journal| {
            let current = current_intent(journal, intent)?;
            if current.identity.is_none() {
                return Err(RepositoryCreationIntentError::CloudIdentityMissing(
                    intent.name.clone(),
                ));
            }
            if current.complete {
                return Ok(current);
            }
            let updated = RepositoryCreationIntent {
                complete: true,
                ..current
            };
            journal.creations.insert(
                updated.name.as_str().to_owned(),
                PersistedIntent::from(&updated),
            );
            self.persist_creation_journal(journal)?;
            Ok(updated)
        })
    }

    pub fn repository_creation_intent(
        &self,
        name: &RepositoryName,
    ) -> Result<Option<RepositoryCreationIntent>, RepositoryCreationIntentError> {
        if !self.root().exists() {
            return Ok(None);
        }
        self.with_creation_journal(false, |journal| {
            journal
                .creations
                .get(name.as_str())
                .map(|persisted| parse_intent(name.clone(), persisted))
                .transpose()
        })
    }

    fn with_creation_journal<T>(
        &self,
        exclusive: bool,
        operation: impl FnOnce(&mut PersistedJournal) -> Result<T, RepositoryCreationIntentError>,
    ) -> Result<T, RepositoryCreationIntentError> {
        let lock_path = self.root().join(JOURNAL_LOCK_FILE);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| RepositoryCreationIntentError::OpenLock {
                path: lock_path.clone(),
                source,
            })?;
        if exclusive {
            lock.lock()
        } else {
            lock.lock_shared()
        }
        .map_err(|source| RepositoryCreationIntentError::Lock {
            path: lock_path,
            source,
        })?;

        let mut journal = self.read_creation_journal()?;
        validate_journal(&journal)?;
        operation(&mut journal)
    }

    fn read_creation_journal(&self) -> Result<PersistedJournal, RepositoryCreationIntentError> {
        let path = self.root().join(JOURNAL_FILE);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|source| RepositoryCreationIntentError::Decode { path, source }),
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                Ok(PersistedJournal::default())
            }
            Err(source) => Err(RepositoryCreationIntentError::Read { path, source }),
        }
    }

    fn persist_creation_journal(
        &self,
        journal: &PersistedJournal,
    ) -> Result<(), RepositoryCreationIntentError> {
        let path = self.root().join(JOURNAL_FILE);
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp_path = self.root().join(format!(
            ".{JOURNAL_FILE}.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        let mut temp = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|source| RepositoryCreationIntentError::Write {
                path: temp_path.clone(),
                source,
            })?;
        let result = (|| {
            serde_json::to_writer_pretty(&mut temp, journal).map_err(|source| {
                RepositoryCreationIntentError::Serialize {
                    path: temp_path.clone(),
                    source,
                }
            })?;
            temp.write_all(b"\n")
                .and_then(|()| temp.sync_all())
                .map_err(|source| RepositoryCreationIntentError::Write {
                    path: temp_path.clone(),
                    source,
                })?;
            fs::rename(&temp_path, &path).map_err(|source| {
                RepositoryCreationIntentError::Replace {
                    from: temp_path.clone(),
                    to: path,
                    source,
                }
            })?;
            sync_directory(self.root()).map_err(|source| RepositoryCreationIntentError::SyncRoot {
                path: self.root().to_owned(),
                source,
            })
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp_path);
        }
        result
    }
}

fn current_intent(
    journal: &PersistedJournal,
    expected: &RepositoryCreationIntent,
) -> Result<RepositoryCreationIntent, RepositoryCreationIntentError> {
    let persisted = journal
        .creations
        .get(expected.name.as_str())
        .ok_or_else(|| RepositoryCreationIntentError::IntentMissing(expected.name.clone()))?;
    let current = parse_intent(expected.name.clone(), persisted)?;
    if current.key != expected.key || current.target != expected.target {
        return Err(RepositoryCreationIntentError::IntentChanged(
            expected.name.clone(),
        ));
    }
    if let (Some(current_identity), Some(expected_identity)) =
        (&current.identity, &expected.identity)
        && current_identity != expected_identity
    {
        return Err(RepositoryCreationIntentError::IntentChanged(
            expected.name.clone(),
        ));
    }
    Ok(current)
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedJournal {
    version: u32,
    creations: BTreeMap<String, PersistedIntent>,
}

impl Default for PersistedJournal {
    fn default() -> Self {
        Self {
            version: JOURNAL_VERSION,
            creations: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedIntent {
    idempotency_key: String,
    base_url: String,
    machine_id: String,
    repository_id: Option<String>,
    incarnation: Option<String>,
    complete: bool,
}

impl From<&RepositoryCreationIntent> for PersistedIntent {
    fn from(intent: &RepositoryCreationIntent) -> Self {
        Self {
            idempotency_key: hex_bytes(&intent.key.0),
            base_url: intent.target.base_url.clone(),
            machine_id: intent.target.machine_id.as_str().to_owned(),
            repository_id: intent
                .identity
                .as_ref()
                .map(|identity| identity.repository_id.as_str().to_owned()),
            incarnation: intent
                .identity
                .as_ref()
                .map(|identity| identity.incarnation.as_str().to_owned()),
            complete: intent.complete,
        }
    }
}

fn parse_intent(
    name: RepositoryName,
    persisted: &PersistedIntent,
) -> Result<RepositoryCreationIntent, RepositoryCreationIntentError> {
    let repository_id = persisted
        .repository_id
        .as_ref()
        .map(|value| RepositoryId::parse(value.clone()))
        .transpose()?;
    let incarnation = persisted
        .incarnation
        .as_ref()
        .map(|value| RepositoryIncarnation::parse(value.clone()))
        .transpose()?;
    let identity = match (repository_id, incarnation) {
        (Some(repository_id), Some(incarnation)) => {
            Some(RepositoryIdentity::new(repository_id, incarnation))
        }
        (None, None) => None,
        _ => return Err(RepositoryCreationIntentError::IncompleteCloudIdentity(name)),
    };
    if persisted.complete && identity.is_none() {
        return Err(RepositoryCreationIntentError::CloudIdentityMissing(name));
    }
    Ok(RepositoryCreationIntent {
        name,
        key: RepositoryCreationKey(parse_hex_16(&persisted.idempotency_key)?),
        target: RepositoryCreationTarget {
            base_url: persisted.base_url.clone(),
            machine_id: MachineId::parse(persisted.machine_id.clone())?,
        },
        identity,
        complete: persisted.complete,
    })
}

fn validate_journal(journal: &PersistedJournal) -> Result<(), RepositoryCreationIntentError> {
    if journal.version != JOURNAL_VERSION {
        return Err(RepositoryCreationIntentError::UnsupportedVersion(
            journal.version,
        ));
    }
    for (name, persisted) in &journal.creations {
        parse_intent(RepositoryName::parse(name.clone())?, persisted)?;
    }
    Ok(())
}

fn parse_hex_16(value: &str) -> Result<[u8; 16], RepositoryCreationIntentError> {
    if value.len() != 32 {
        return Err(RepositoryCreationIntentError::InvalidKey);
    }
    let mut bytes = [0; 16];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        bytes[index] = (hex_digit(pair[0])? << 4) | hex_digit(pair[1])?;
    }
    Ok(bytes)
}

fn hex_digit(byte: u8) -> Result<u8, RepositoryCreationIntentError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(RepositoryCreationIntentError::InvalidKey),
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
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
pub enum RepositoryCreationIntentError {
    #[error("repository {0} is already registered locally by a different workflow")]
    AlreadyRegistered(RepositoryName),
    #[error(
        "machine configuration changed while repository {0} creation is pending; restore the original control-plane configuration to resume it"
    )]
    TargetChanged(RepositoryName),
    #[error("repository {0} creation intent changed while the command was running")]
    IntentChanged(RepositoryName),
    #[error("repository {0} creation intent is missing")]
    IntentMissing(RepositoryName),
    #[error("cloud returned a different identity for repository {0} than this creation recorded")]
    DifferentCloudIdentity(RepositoryName),
    #[error("repository {0} creation record has an incomplete cloud identity")]
    IncompleteCloudIdentity(RepositoryName),
    #[error("repository {0} creation has no recorded cloud identity")]
    CloudIdentityMissing(RepositoryName),
    #[error(
        "repository {0} was created previously, but its recorded local catalog binding is missing or changed; restore that binding or choose a different repository name"
    )]
    CompletedBindingChanged(RepositoryName),
    #[error("repository creation idempotency key is invalid")]
    InvalidKey,
    #[error("repository creation journal version {0} is unsupported")]
    UnsupportedVersion(u32),
    #[error("failed to create machine-store root at {path}")]
    CreateRoot {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to open repository creation lock at {path}")]
    OpenLock {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to lock repository creation journal at {path}")]
    Lock {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to read repository creation journal at {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to decode repository creation journal at {path}")]
    Decode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to serialize repository creation journal at {path}")]
    Serialize {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to write repository creation journal at {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to atomically replace repository creation journal {to} from {from}")]
    Replace {
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
    #[error(transparent)]
    MachineStore(Box<MachineStoreError>),
    #[error(transparent)]
    MachineConfig(#[from] crate::MachineConfigError),
}

impl From<MachineStoreError> for RepositoryCreationIntentError {
    fn from(error: MachineStoreError) -> Self {
        Self::MachineStore(Box::new(error))
    }
}
