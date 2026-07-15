use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{MachineStore, MachineStoreError, RepositoryIdentity, RepositoryName};

const JOURNAL_VERSION: u32 = 2;
const JOURNAL_FILE: &str = "checkout-creations.json";
const JOURNAL_LOCK_FILE: &str = "checkout-creations.lock";
const ACTION_LOCK_FILE: &str = "checkout-creations.action.lock";
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Eq, PartialEq)]
pub struct CheckoutCreationToken([u8; 16]);

impl CheckoutCreationToken {
    pub fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub fn bytes(&self) -> [u8; 16] {
        self.0
    }

    pub fn hex(&self) -> String {
        hex_bytes(&self.0)
    }
}

impl fmt::Debug for CheckoutCreationToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CheckoutCreationToken([REDACTED])")
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutCreationPhase {
    Planned,
    WorkspaceRegistered,
    Staged,
    Published,
    Complete,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckoutCreationTarget {
    destination: PathBuf,
    repository_name: RepositoryName,
    repository_identity: RepositoryIdentity,
    base_commit: Vec<u8>,
    revision: String,
}

impl CheckoutCreationTarget {
    pub fn new(
        destination: PathBuf,
        repository_name: RepositoryName,
        repository_identity: RepositoryIdentity,
        base_commit: Vec<u8>,
        revision: String,
    ) -> Self {
        Self {
            destination,
            repository_name,
            repository_identity,
            base_commit,
            revision,
        }
    }

    pub fn destination(&self) -> &Path {
        &self.destination
    }

    pub fn repository_name(&self) -> &RepositoryName {
        &self.repository_name
    }

    pub fn repository_identity(&self) -> &RepositoryIdentity {
        &self.repository_identity
    }

    pub fn base_commit(&self) -> &[u8] {
        &self.base_commit
    }

    pub fn revision(&self) -> &str {
        &self.revision
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckoutCreationIntent {
    target: CheckoutCreationTarget,
    token: CheckoutCreationToken,
    workspace_id: String,
    working_copy_commit: Option<Vec<u8>>,
    phase: CheckoutCreationPhase,
}

impl CheckoutCreationIntent {
    pub fn target(&self) -> &CheckoutCreationTarget {
        &self.target
    }

    pub fn token(&self) -> &CheckoutCreationToken {
        &self.token
    }

    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    pub fn phase(&self) -> CheckoutCreationPhase {
        self.phase
    }

    pub fn working_copy_commit(&self) -> Option<&[u8]> {
        self.working_copy_commit.as_deref()
    }

    pub fn staging_name(&self) -> String {
        format!(".devspace-checkout-{}", self.token.hex())
    }
}

pub struct CheckoutCreationGuard {
    _file: File,
}

impl MachineStore {
    /// Serializes checkout recovery so 2 processes cannot advance one intent.
    pub fn lock_checkout_creation(
        &self,
    ) -> Result<CheckoutCreationGuard, CheckoutCreationIntentError> {
        fs::create_dir_all(self.root()).map_err(|source| {
            CheckoutCreationIntentError::CreateRoot {
                path: self.root().to_owned(),
                source,
            }
        })?;
        let path = self.root().join(ACTION_LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| CheckoutCreationIntentError::OpenLock {
                path: path.clone(),
                source,
            })?;
        file.lock()
            .map_err(|source| CheckoutCreationIntentError::Lock { path, source })?;
        Ok(CheckoutCreationGuard { _file: file })
    }

    pub fn checkout_creation_intent(
        &self,
        destination: &Path,
    ) -> Result<Option<CheckoutCreationIntent>, CheckoutCreationIntentError> {
        if !self.root().exists() {
            return Ok(None);
        }
        let key = encode_path(destination);
        self.with_checkout_journal(false, |journal| {
            journal.creations.get(&key).map(parse_intent).transpose()
        })
    }

    /// Persists the stable workspace and staging ownership before checkout effects.
    pub fn begin_checkout_creation(
        &self,
        target: CheckoutCreationTarget,
        token: CheckoutCreationToken,
        workspace_id: String,
    ) -> Result<CheckoutCreationIntent, CheckoutCreationIntentError> {
        if workspace_id.is_empty() {
            return Err(CheckoutCreationIntentError::InvalidWorkspaceId);
        }
        fs::create_dir_all(self.root()).map_err(|source| {
            CheckoutCreationIntentError::CreateRoot {
                path: self.root().to_owned(),
                source,
            }
        })?;
        let key = encode_path(target.destination());
        self.with_checkout_journal(true, |journal| {
            if let Some(persisted) = journal.creations.get(&key) {
                let intent = parse_intent(persisted)?;
                if intent.target != target {
                    return Err(CheckoutCreationIntentError::TargetChanged(
                        target.destination,
                    ));
                }
                return Ok(intent);
            }
            let intent = CheckoutCreationIntent {
                target,
                token,
                workspace_id,
                working_copy_commit: None,
                phase: CheckoutCreationPhase::Planned,
            };
            journal
                .creations
                .insert(key, PersistedIntent::from(&intent));
            self.persist_checkout_journal(journal)?;
            Ok(intent)
        })
    }

    pub fn advance_checkout_creation(
        &self,
        intent: &CheckoutCreationIntent,
        phase: CheckoutCreationPhase,
    ) -> Result<CheckoutCreationIntent, CheckoutCreationIntentError> {
        let key = encode_path(intent.target.destination());
        self.with_checkout_journal(true, |journal| {
            let persisted = journal.creations.get(&key).ok_or_else(|| {
                CheckoutCreationIntentError::IntentMissing(intent.target.destination().to_owned())
            })?;
            let current = parse_intent(persisted)?;
            if !same_intent(&current, intent) {
                return Err(CheckoutCreationIntentError::IntentChanged(
                    intent.target.destination().to_owned(),
                ));
            }
            if phase < current.phase {
                return Err(CheckoutCreationIntentError::PhaseRegression {
                    current: current.phase,
                    requested: phase,
                });
            }
            if phase == current.phase {
                return Ok(current);
            }
            let updated = CheckoutCreationIntent { phase, ..current };
            journal
                .creations
                .insert(key, PersistedIntent::from(&updated));
            self.persist_checkout_journal(journal)?;
            Ok(updated)
        })
    }

    /// Records the exact commit which this intent is allowed to register as
    /// its working-copy commit. The object may already exist, but no workspace
    /// view is changed until this identity is durable.
    pub fn record_checkout_working_copy_commit(
        &self,
        intent: &CheckoutCreationIntent,
        commit_id: Vec<u8>,
    ) -> Result<CheckoutCreationIntent, CheckoutCreationIntentError> {
        let key = encode_path(intent.target.destination());
        self.with_checkout_journal(true, |journal| {
            let persisted = journal.creations.get(&key).ok_or_else(|| {
                CheckoutCreationIntentError::IntentMissing(intent.target.destination().to_owned())
            })?;
            let current = parse_intent(persisted)?;
            if !same_intent(&current, intent) {
                return Err(CheckoutCreationIntentError::IntentChanged(
                    intent.target.destination().to_owned(),
                ));
            }
            if let Some(existing) = &current.working_copy_commit {
                if existing != &commit_id {
                    return Err(CheckoutCreationIntentError::WorkingCopyCommitChanged(
                        intent.target.destination().to_owned(),
                    ));
                }
                return Ok(current);
            }
            let updated = CheckoutCreationIntent {
                working_copy_commit: Some(commit_id),
                ..current
            };
            journal
                .creations
                .insert(key, PersistedIntent::from(&updated));
            self.persist_checkout_journal(journal)?;
            Ok(updated)
        })
    }

    fn with_checkout_journal<T>(
        &self,
        exclusive: bool,
        operation: impl FnOnce(&mut PersistedJournal) -> Result<T, CheckoutCreationIntentError>,
    ) -> Result<T, CheckoutCreationIntentError> {
        let path = self.root().join(JOURNAL_LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| CheckoutCreationIntentError::OpenLock {
                path: path.clone(),
                source,
            })?;
        if exclusive {
            file.lock()
        } else {
            file.lock_shared()
        }
        .map_err(|source| CheckoutCreationIntentError::Lock { path, source })?;
        let mut journal = self.read_checkout_journal()?;
        if journal.version != JOURNAL_VERSION {
            return Err(CheckoutCreationIntentError::UnsupportedVersion(
                journal.version,
            ));
        }
        operation(&mut journal)
    }

    fn read_checkout_journal(&self) -> Result<PersistedJournal, CheckoutCreationIntentError> {
        let path = self.root().join(JOURNAL_FILE);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|source| CheckoutCreationIntentError::Decode { path, source }),
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                Ok(PersistedJournal::default())
            }
            Err(source) => Err(CheckoutCreationIntentError::Read { path, source }),
        }
    }

    fn persist_checkout_journal(
        &self,
        journal: &PersistedJournal,
    ) -> Result<(), CheckoutCreationIntentError> {
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
            .map_err(|source| CheckoutCreationIntentError::Write {
                path: temp_path.clone(),
                source,
            })?;
        let result = (|| {
            serde_json::to_writer_pretty(&mut temp, journal).map_err(|source| {
                CheckoutCreationIntentError::Serialize {
                    path: temp_path.clone(),
                    source,
                }
            })?;
            temp.write_all(b"\n")
                .and_then(|()| temp.sync_all())
                .map_err(|source| CheckoutCreationIntentError::Write {
                    path: temp_path.clone(),
                    source,
                })?;
            fs::rename(&temp_path, &path).map_err(|source| {
                CheckoutCreationIntentError::Replace {
                    from: temp_path.clone(),
                    to: path,
                    source,
                }
            })?;
            sync_directory(self.root()).map_err(|source| CheckoutCreationIntentError::SyncRoot {
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
    destination: String,
    repository_name: String,
    repository_id: String,
    incarnation: String,
    base_commit: String,
    revision: String,
    token: String,
    workspace_id: String,
    working_copy_commit: Option<String>,
    phase: CheckoutCreationPhase,
}

impl From<&CheckoutCreationIntent> for PersistedIntent {
    fn from(intent: &CheckoutCreationIntent) -> Self {
        Self {
            destination: encode_path(intent.target.destination()),
            repository_name: intent.target.repository_name.as_str().to_owned(),
            repository_id: intent
                .target
                .repository_identity
                .repository_id
                .as_str()
                .to_owned(),
            incarnation: intent
                .target
                .repository_identity
                .incarnation
                .as_str()
                .to_owned(),
            base_commit: hex_bytes(&intent.target.base_commit),
            revision: intent.target.revision.clone(),
            token: intent.token.hex(),
            workspace_id: intent.workspace_id.clone(),
            working_copy_commit: intent.working_copy_commit.as_deref().map(hex_bytes),
            phase: intent.phase,
        }
    }
}

fn parse_intent(
    persisted: &PersistedIntent,
) -> Result<CheckoutCreationIntent, CheckoutCreationIntentError> {
    let repository_name = RepositoryName::parse(persisted.repository_name.clone())?;
    let repository_id = crate::RepositoryId::parse(persisted.repository_id.clone())?;
    let incarnation = crate::RepositoryIncarnation::parse(persisted.incarnation.clone())?;
    if persisted.workspace_id.is_empty() {
        return Err(CheckoutCreationIntentError::InvalidWorkspaceId);
    }
    Ok(CheckoutCreationIntent {
        target: CheckoutCreationTarget {
            destination: decode_path(&persisted.destination)?,
            repository_name,
            repository_identity: RepositoryIdentity::new(repository_id, incarnation),
            base_commit: parse_hex(&persisted.base_commit)?,
            revision: persisted.revision.clone(),
        },
        token: CheckoutCreationToken(parse_hex_16(&persisted.token)?),
        workspace_id: persisted.workspace_id.clone(),
        working_copy_commit: persisted
            .working_copy_commit
            .as_deref()
            .map(parse_hex)
            .transpose()?,
        phase: persisted.phase,
    })
}

fn same_intent(left: &CheckoutCreationIntent, right: &CheckoutCreationIntent) -> bool {
    left.target == right.target
        && left.token == right.token
        && left.workspace_id == right.workspace_id
        && left.working_copy_commit == right.working_copy_commit
}

#[cfg(unix)]
fn encode_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    format!("unix:{}", hex_bytes(path.as_os_str().as_bytes()))
}

#[cfg(unix)]
fn decode_path(encoded: &str) -> Result<PathBuf, CheckoutCreationIntentError> {
    use std::os::unix::ffi::OsStringExt as _;
    let bytes = parse_path_encoding(encoded, "unix:")?;
    Ok(std::ffi::OsString::from_vec(bytes).into())
}

#[cfg(windows)]
fn encode_path(path: &Path) -> String {
    use std::os::windows::ffi::OsStrExt as _;
    let bytes = path
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    format!("windows:{}", hex_bytes(&bytes))
}

#[cfg(windows)]
fn decode_path(encoded: &str) -> Result<PathBuf, CheckoutCreationIntentError> {
    use std::os::windows::ffi::OsStringExt as _;
    let bytes = parse_path_encoding(encoded, "windows:")?;
    if bytes.len() % 2 != 0 {
        return Err(CheckoutCreationIntentError::InvalidPathEncoding);
    }
    let wide = bytes
        .chunks_exact(2)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
        .collect::<Vec<_>>();
    Ok(std::ffi::OsString::from_wide(&wide).into())
}

fn parse_path_encoding(
    encoded: &str,
    prefix: &str,
) -> Result<Vec<u8>, CheckoutCreationIntentError> {
    let value = encoded
        .strip_prefix(prefix)
        .ok_or(CheckoutCreationIntentError::InvalidPathEncoding)?;
    parse_hex(value)
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn parse_hex(value: &str) -> Result<Vec<u8>, CheckoutCreationIntentError> {
    if !value.len().is_multiple_of(2) {
        return Err(CheckoutCreationIntentError::InvalidHex);
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).expect("hex is ASCII-sized");
            u8::from_str_radix(pair, 16).map_err(|_| CheckoutCreationIntentError::InvalidHex)
        })
        .collect()
}

fn parse_hex_16(value: &str) -> Result<[u8; 16], CheckoutCreationIntentError> {
    parse_hex(value)?
        .try_into()
        .map_err(|_| CheckoutCreationIntentError::InvalidHex)
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
pub enum CheckoutCreationIntentError {
    #[error("failed to create machine-store root {path}")]
    CreateRoot { path: PathBuf, source: io::Error },
    #[error("failed to open checkout-creation lock {path}")]
    OpenLock { path: PathBuf, source: io::Error },
    #[error("failed to lock checkout creations at {path}")]
    Lock { path: PathBuf, source: io::Error },
    #[error("failed to read checkout-creation journal {path}")]
    Read { path: PathBuf, source: io::Error },
    #[error("failed to decode checkout-creation journal {path}")]
    Decode {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("failed to serialize checkout-creation journal {path}")]
    Serialize {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("failed to write checkout-creation journal {path}")]
    Write { path: PathBuf, source: io::Error },
    #[error("failed to replace checkout-creation journal {to} from {from}")]
    Replace {
        from: PathBuf,
        to: PathBuf,
        source: io::Error,
    },
    #[error("failed to sync machine-store root {path}")]
    SyncRoot { path: PathBuf, source: io::Error },
    #[error("unsupported checkout-creation journal version {0}")]
    UnsupportedVersion(u32),
    #[error("checkout creation for {0} was already planned for a different repository or revision")]
    TargetChanged(PathBuf),
    #[error("checkout creation intent for {0} is missing")]
    IntentMissing(PathBuf),
    #[error("checkout creation intent for {0} changed while it was running")]
    IntentChanged(PathBuf),
    #[error("checkout creation for {0} already has a different working-copy commit")]
    WorkingCopyCommitChanged(PathBuf),
    #[error("checkout creation cannot move backwards from {current:?} to {requested:?}")]
    PhaseRegression {
        current: CheckoutCreationPhase,
        requested: CheckoutCreationPhase,
    },
    #[error("checkout creation journal contains an invalid native path")]
    InvalidPathEncoding,
    #[error("checkout creation journal contains invalid hexadecimal data")]
    InvalidHex,
    #[error("checkout creation workspace ID must not be empty")]
    InvalidWorkspaceId,
    #[error(transparent)]
    MachineStore(Box<MachineStoreError>),
}

impl From<MachineStoreError> for CheckoutCreationIntentError {
    fn from(error: MachineStoreError) -> Self {
        Self::MachineStore(Box::new(error))
    }
}
