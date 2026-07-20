use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::locked_json::{LockedJsonError, LockedJsonFile};
use crate::{
    MachineConfig, MachineId, MachineStore, MachineStoreError, RepositoryId, RepositoryIdentity,
    RepositoryIncarnation, RepositoryName, decode_lower_hex, encode_lower_hex,
};

const JOURNAL_VERSION: u32 = 2;
const JOURNAL_FILE: &str = "repository-creations.json";
const JOURNAL_LOCK_FILE: &str = "repository-creations.lock";

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
}

impl MachineStore {
    /// Starts a durable create saga or resumes the exact request already recorded.
    ///
    /// The proposed key is ignored when an intent already exists.
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

    /// Removes an intent after local catalog registration and materialization succeed.
    pub fn complete_repository_creation(
        &self,
        intent: &RepositoryCreationIntent,
    ) -> Result<(), RepositoryCreationIntentError> {
        self.with_creation_journal(true, |journal| {
            let current = current_intent(journal, intent)?;
            if current.identity.is_none() {
                return Err(RepositoryCreationIntentError::CloudIdentityMissing(
                    intent.name.clone(),
                ));
            }
            require_exact_intent(&current, intent)?;
            journal.creations.remove(intent.name.as_str());
            self.persist_creation_journal(journal)?;
            Ok(())
        })
    }

    /// Discards an intent after the cloud proves that request cannot succeed.
    pub fn discard_repository_creation(
        &self,
        intent: &RepositoryCreationIntent,
    ) -> Result<(), RepositoryCreationIntentError> {
        self.with_creation_journal(true, |journal| {
            let current = current_intent(journal, intent)?;
            require_exact_intent(&current, intent)?;
            journal.creations.remove(intent.name.as_str());
            self.persist_creation_journal(journal)?;
            Ok(())
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
        let storage = self.creation_journal_storage();
        let _lock = storage.lock(exclusive)?;
        let mut journal = storage.read_or_default()?;
        validate_journal(&journal)?;
        operation(&mut journal)
    }

    fn creation_journal_storage(&self) -> LockedJsonFile<'_> {
        LockedJsonFile::new(self.root(), JOURNAL_FILE, JOURNAL_LOCK_FILE)
    }

    fn persist_creation_journal(
        &self,
        journal: &PersistedJournal,
    ) -> Result<(), RepositoryCreationIntentError> {
        self.creation_journal_storage().persist(journal)?;
        Ok(())
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

fn require_exact_intent(
    current: &RepositoryCreationIntent,
    expected: &RepositoryCreationIntent,
) -> Result<(), RepositoryCreationIntentError> {
    if current.identity != expected.identity {
        return Err(RepositoryCreationIntentError::IntentChanged(
            expected.name.clone(),
        ));
    }
    Ok(())
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
}

impl From<&RepositoryCreationIntent> for PersistedIntent {
    fn from(intent: &RepositoryCreationIntent) -> Self {
        Self {
            idempotency_key: encode_lower_hex(&intent.key.0),
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
    Ok(RepositoryCreationIntent {
        name,
        key: RepositoryCreationKey(parse_hex_16(&persisted.idempotency_key)?),
        target: RepositoryCreationTarget {
            base_url: persisted.base_url.clone(),
            machine_id: MachineId::parse(persisted.machine_id.clone())?,
        },
        identity,
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
    decode_lower_hex(value).map_err(|_| RepositoryCreationIntentError::InvalidKey)
}

#[derive(Debug, Error)]
pub enum RepositoryCreationIntentError {
    #[error("repository {0} already exists on this machine")]
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
    #[error("repository creation journal: {0}")]
    JournalStorage(#[source] Box<dyn std::error::Error + Send + Sync>),
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

impl From<LockedJsonError> for RepositoryCreationIntentError {
    fn from(error: LockedJsonError) -> Self {
        Self::JournalStorage(Box::new(error))
    }
}
