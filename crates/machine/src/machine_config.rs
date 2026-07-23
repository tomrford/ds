use std::fs;
use std::io;
use std::path::PathBuf;

use reqwest::Url;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::machine_store::MachineStore;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
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

    pub fn as_str(&self) -> &str {
        &self.0
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
    git_shim: bool,
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
            git_shim: false,
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

    pub fn with_git_shim(mut self, enabled: bool) -> Self {
        self.git_shim = enabled;
        self
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

    pub fn git_shim(&self) -> bool {
        self.git_shim
    }
}

impl MachineStore {
    pub fn write_config(&self, config: &MachineConfig) -> Result<(), MachineConfigError> {
        let path = self.config_path();
        let directory = path.parent().expect("machine config path has a parent");
        fs::create_dir_all(directory).map_err(|source| MachineConfigError::CreateRoot {
            path: directory.to_owned(),
            source,
        })?;
        let serialized =
            toml::to_string_pretty(&PersistedConfig::from(config)).map_err(|source| {
                MachineConfigError::Serialize {
                    path: path.clone(),
                    source,
                }
            })?;
        fs::write(&path, serialized).map_err(|source| MachineConfigError::Write { path, source })
    }

    pub fn load_config(&self) -> Result<MachineConfig, MachineConfigError> {
        let path = self.config_path();
        let text = fs::read_to_string(&path).map_err(|source| MachineConfigError::Read {
            path: path.clone(),
            source,
        })?;
        let persisted: PersistedConfig =
            toml::from_str(&text).map_err(|source| MachineConfigError::Decode {
                path: path.clone(),
                source,
            })?;
        let mut config = MachineConfig::new(
            persisted.base_url,
            MachineId::parse(persisted.machine_id)?,
            SharedSecret::new(persisted.shared_secret)?,
        )?;
        if let Some(machine_name) = persisted.machine_name {
            config = config.with_machine_name(machine_name)?;
        }
        Ok(config.with_git_shim(persisted.git_shim))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedConfig {
    base_url: String,
    machine_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    machine_name: Option<String>,
    shared_secret: String,
    #[serde(default)]
    git_shim: bool,
}

impl From<&MachineConfig> for PersistedConfig {
    fn from(config: &MachineConfig) -> Self {
        Self {
            base_url: config.base_url.clone(),
            machine_id: config.machine_id.0.clone(),
            machine_name: config.machine_name.clone(),
            shared_secret: config.shared_secret.0.clone(),
            git_shim: config.git_shim,
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
    #[error("failed to read machine configuration at {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to decode machine configuration at {path}: {source}")]
    Decode {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}
