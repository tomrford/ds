use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use reqwest::Url;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::machine_store::MachineStore;

const CONFIG_FILE: &str = "config.toml";
const CONFIG_VERSION: u32 = 1;
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SharedSecret(String);

impl SharedSecret {
    pub fn new(value: impl Into<String>) -> Result<Self, MachineConfigError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 8 * 1024
            || !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
        {
            return Err(MachineConfigError::InvalidSharedSecret);
        }
        Ok(Self(value))
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SharedSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SharedSecret([REDACTED])")
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MachineId(String);

impl MachineId {
    pub fn parse(value: impl Into<String>) -> Result<Self, MachineConfigError> {
        let value = value.into();
        if value.len() != 32
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(MachineConfigError::InvalidMachineId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MachineConfig {
    base_url: String,
    machine_id: MachineId,
    machine_name: Option<String>,
    shared_secret: SharedSecret,
}

impl MachineConfig {
    pub fn new(
        base_url: impl Into<String>,
        machine_id: MachineId,
        shared_secret: SharedSecret,
    ) -> Result<Self, MachineConfigError> {
        let base_url = normalize_base_url(base_url.into())?;
        Ok(Self {
            base_url,
            machine_id,
            machine_name: None,
            shared_secret,
        })
    }

    pub fn with_machine_name(
        mut self,
        machine_name: impl Into<String>,
    ) -> Result<Self, MachineConfigError> {
        let machine_name = machine_name.into();
        validate_machine_name(&machine_name)?;
        self.machine_name = Some(machine_name);
        Ok(self)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn machine_id(&self) -> &MachineId {
        &self.machine_id
    }

    pub fn machine_name(&self) -> Option<&str> {
        self.machine_name.as_deref()
    }

    pub fn shared_secret(&self) -> &SharedSecret {
        &self.shared_secret
    }
}

impl MachineStore {
    pub fn config_path(&self) -> PathBuf {
        self.root().join(CONFIG_FILE)
    }

    pub fn write_config(&self, config: &MachineConfig) -> Result<(), MachineConfigError> {
        fs::create_dir_all(self.root()).map_err(|source| MachineConfigError::CreateRoot {
            path: self.root().to_owned(),
            source,
        })?;
        protect_directory(self.root())?;

        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp_path = self.root().join(format!(
            ".{CONFIG_FILE}.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        protect_new_file(&mut options);
        let mut temp = options
            .open(&temp_path)
            .map_err(|source| MachineConfigError::Write {
                path: temp_path.clone(),
                source,
            })?;
        let persisted = PersistedConfig::from(config);
        let result = (|| {
            let serialized = toml::to_string_pretty(&persisted).map_err(|source| {
                MachineConfigError::Serialize {
                    path: temp_path.clone(),
                    source,
                }
            })?;
            temp.write_all(serialized.as_bytes())
                .and_then(|()| temp.sync_all())
                .map_err(|source| MachineConfigError::Write {
                    path: temp_path.clone(),
                    source,
                })?;
            fs::rename(&temp_path, self.config_path()).map_err(|source| {
                MachineConfigError::Replace {
                    from: temp_path.clone(),
                    to: self.config_path(),
                    source,
                }
            })?;
            sync_directory(self.root()).map_err(|source| MachineConfigError::SyncRoot {
                path: self.root().to_owned(),
                source,
            })
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp_path);
        }
        result
    }

    pub fn load_config(&self) -> Result<MachineConfig, MachineConfigError> {
        let path = self.config_path();
        require_protected_file(&path)?;
        let metadata = fs::metadata(&path).map_err(|source| MachineConfigError::Read {
            path: path.clone(),
            source,
        })?;
        if metadata.len() > MAX_CONFIG_BYTES {
            return Err(MachineConfigError::TooLarge(path));
        }
        let bytes = fs::read(&path).map_err(|source| MachineConfigError::Read {
            path: path.clone(),
            source,
        })?;
        let persisted: PersistedConfig = toml::from_slice(&bytes)
            .map_err(|source| MachineConfigError::Decode { path, source })?;
        if persisted.version != CONFIG_VERSION {
            return Err(MachineConfigError::UnsupportedVersion(persisted.version));
        }
        let mut config = MachineConfig::new(
            persisted.base_url,
            MachineId::parse(persisted.machine_id)?,
            SharedSecret::new(persisted.shared_secret)?,
        )?;
        if let Some(machine_name) = persisted.machine_name {
            config = config.with_machine_name(machine_name)?;
        }
        Ok(config)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedConfig {
    version: u32,
    base_url: String,
    machine_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    machine_name: Option<String>,
    shared_secret: String,
}

impl From<&MachineConfig> for PersistedConfig {
    fn from(config: &MachineConfig) -> Self {
        Self {
            version: CONFIG_VERSION,
            base_url: config.base_url.clone(),
            machine_id: config.machine_id.0.clone(),
            machine_name: config.machine_name.clone(),
            shared_secret: config.shared_secret.0.clone(),
        }
    }
}

fn validate_machine_name(value: &str) -> Result<(), MachineConfigError> {
    if value.is_empty()
        || value.len() > 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(MachineConfigError::InvalidMachineName);
    }
    Ok(())
}

fn normalize_base_url(value: String) -> Result<String, MachineConfigError> {
    let mut url = Url::parse(&value).map_err(|_| MachineConfigError::InvalidBaseUrl)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(MachineConfigError::InvalidBaseUrl);
    }
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(&path);
    Ok(url.to_string().trim_end_matches('/').to_owned())
}

#[cfg(unix)]
fn protect_directory(path: &Path) -> Result<(), MachineConfigError> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
        MachineConfigError::Protect {
            path: path.to_owned(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn protect_directory(_path: &Path) -> Result<(), MachineConfigError> {
    Ok(())
}

#[cfg(unix)]
fn protect_new_file(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt as _;
    options.mode(0o600);
}

#[cfg(not(unix))]
fn protect_new_file(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn require_protected_file(path: &Path) -> Result<(), MachineConfigError> {
    use std::os::unix::fs::PermissionsExt as _;
    let metadata = fs::metadata(path).map_err(|source| MachineConfigError::Read {
        path: path.to_owned(),
        source,
    })?;
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(MachineConfigError::InsecurePermissions(path.to_owned()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn require_protected_file(_path: &Path) -> Result<(), MachineConfigError> {
    Ok(())
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
pub enum MachineConfigError {
    #[error(
        "shared development secret is empty, too long, or contains spaces or characters outside printable ASCII"
    )]
    InvalidSharedSecret,
    #[error("machine ID must be exactly 32 lowercase hexadecimal characters")]
    InvalidMachineId,
    #[error("machine name must be 1-32 ASCII letters, digits, '_' or '-'")]
    InvalidMachineName,
    #[error(
        "Worker base URL must be an HTTP(S) origin or path without credentials, query, or fragment"
    )]
    InvalidBaseUrl,
    #[error("failed to create machine configuration directory at {path}")]
    CreateRoot {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to protect machine configuration at {path}")]
    Protect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to write machine configuration at {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to serialize machine configuration at {path}")]
    Serialize {
        path: PathBuf,
        #[source]
        source: toml::ser::Error,
    },
    #[error("failed to atomically replace machine configuration {to} from {from}")]
    Replace {
        from: PathBuf,
        to: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to sync machine configuration directory at {path}")]
    SyncRoot {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to read machine configuration at {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("machine configuration at {0} is not private to the current user")]
    InsecurePermissions(PathBuf),
    #[error("machine configuration at {0} exceeds the 64 KiB limit")]
    TooLarge(PathBuf),
    #[error("failed to decode machine configuration at {path}")]
    Decode {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("machine configuration version {0} is unsupported")]
    UnsupportedVersion(u32),
}
