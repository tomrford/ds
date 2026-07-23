//! HTTP transport for the Worker's v2 Git-object pack store.

use std::time::Duration;

use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde::Deserialize;
use thiserror::Error;

use crate::pack_manifest::MAX_MANIFEST_BYTES;
use crate::{BuiltPack, Digest, MAX_CHUNK_BYTES, PackManifest, PackManifestError, hex};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_JSON_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_ERROR_RESPONSE_BYTES: usize = 16 * 1024;
const TEST_HOOKS_ENV: &str = "DEVSPACE_HTTP_TEST_HOOKS";
const TEST_REQUEST_TIMEOUT_MS_ENV: &str = "DEVSPACE_HTTP_TEST_REQUEST_TIMEOUT_MS";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitPackCatalogEntry {
    pub sequence: u64,
    pub id: Digest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitPackCatalogPage {
    pub packs: Vec<GitPackCatalogEntry>,
    pub next_after: u64,
    pub through: u64,
    pub has_more: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownloadedGitPack {
    pub id: Digest,
    pub manifest: Vec<u8>,
    pub chunks: Vec<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GitUploadReceipt {
    pub inserted: bool,
    pub installed: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GitInstallReceipt {
    pub installed: bool,
    pub inserted_objects: usize,
}

#[derive(Debug)]
pub struct GitHttpTransport {
    client: reqwest::Client,
    repository_url: String,
    authorization: HeaderValue,
    machine_id: String,
    incarnation: String,
}

#[derive(Debug, Error)]
pub enum GitHttpTransportError {
    #[error("invalid Worker base URL: {0}")]
    InvalidBaseUrl(String),
    #[error("machine ID must be 32 lowercase hex characters")]
    InvalidMachineId,
    #[error("repository ID must be 64 lowercase hex characters")]
    InvalidRepositoryId,
    #[error("repository incarnation must be 32 lowercase hex characters")]
    InvalidIncarnation,
    #[error("shared secret must be non-empty printable ASCII")]
    InvalidSharedSecret,
    #[error("failed to construct HTTP authorization header")]
    InvalidAuthorization(#[source] reqwest::header::InvalidHeaderValue),
    #[error("failed to construct hardened HTTP client")]
    Client(#[source] reqwest::Error),
    #[error("Worker HTTP request failed")]
    Request(#[source] reqwest::Error),
    #[error("Worker request failed with status {status}: {message}")]
    Status {
        status: reqwest::StatusCode,
        message: String,
    },
    #[error("Worker response declares {length} bytes, exceeding the {limit}-byte limit")]
    DeclaredTooLarge { length: u64, limit: usize },
    #[error("Worker response exceeds the {limit}-byte limit")]
    TooLarge { limit: usize },
    #[error("Worker returned malformed JSON")]
    Json(#[source] serde_json::Error),
    #[error("Worker returned invalid pack manifest")]
    Manifest(#[source] PackManifestError),
    #[error("Worker protocol violation: {0}")]
    Protocol(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CatalogResponse {
    packs: Vec<CatalogEntry>,
    next_after: u64,
    through: u64,
    has_more: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogEntry {
    sequence: u64,
    id: String,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: String,
    code: Option<String>,
}

impl GitHttpTransport {
    pub fn new(
        base_url: &str,
        shared_secret: &str,
        machine_id: &str,
        repository_id: &str,
        incarnation: &str,
    ) -> Result<Self, GitHttpTransportError> {
        validate_lower_hex(machine_id, 32).map_err(|_| GitHttpTransportError::InvalidMachineId)?;
        validate_lower_hex(repository_id, 64)
            .map_err(|_| GitHttpTransportError::InvalidRepositoryId)?;
        validate_lower_hex(incarnation, 32)
            .map_err(|_| GitHttpTransportError::InvalidIncarnation)?;
        if shared_secret.is_empty()
            || shared_secret.len() > 8 * 1024
            || !shared_secret
                .bytes()
                .all(|byte| (0x21..=0x7e).contains(&byte))
        {
            return Err(GitHttpTransportError::InvalidSharedSecret);
        }
        let base = reqwest::Url::parse(base_url)
            .map_err(|error| GitHttpTransportError::InvalidBaseUrl(error.to_string()))?;
        if !matches!(base.scheme(), "http" | "https") {
            return Err(GitHttpTransportError::Protocol(
                "Worker base URL must use HTTP or HTTPS".to_owned(),
            ));
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {shared_secret}"))
            .map_err(GitHttpTransportError::InvalidAuthorization)?;
        authorization.set_sensitive(true);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "x-devspace-client",
            HeaderValue::from_str(&format!("ds/{} git-pack/2", env!("CARGO_PKG_VERSION")))
                .expect("client version header is static ASCII"),
        );
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(request_timeout())
            .build()
            .map_err(GitHttpTransportError::Client)?;
        Ok(Self {
            client,
            repository_url: format!(
                "{}/repositories/{repository_id}/git",
                base.as_str().trim_end_matches('/')
            ),
            authorization,
            machine_id: machine_id.to_owned(),
            incarnation: incarnation.to_owned(),
        })
    }

    pub async fn list_packs(
        &self,
        after: u64,
        through: Option<u64>,
    ) -> Result<GitPackCatalogPage, GitHttpTransportError> {
        let mut url = format!("{}/packs?after={after}", self.repository_url);
        if let Some(through) = through {
            url.push_str(&format!("&through={through}"));
        }
        let response: CatalogResponse = self
            .read_json(self.send(self.client.get(url)).await?)
            .await?;
        let packs = response
            .packs
            .into_iter()
            .map(|entry| {
                Ok(GitPackCatalogEntry {
                    sequence: entry.sequence,
                    id: decode_digest(&entry.id)?,
                })
            })
            .collect::<Result<Vec<_>, GitHttpTransportError>>()?;
        Ok(GitPackCatalogPage {
            packs,
            next_after: response.next_after,
            through: response.through,
            has_more: response.has_more,
        })
    }

    pub async fn download_pack(
        &self,
        id: Digest,
    ) -> Result<DownloadedGitPack, GitHttpTransportError> {
        let pack_url = format!("{}/packs/{}", self.repository_url, hex(&id));
        let manifest = self
            .fetch_bytes(format!("{pack_url}/manifest"), MAX_MANIFEST_BYTES)
            .await?;
        let chunk_count = PackManifest::decode(&manifest)
            .map_err(GitHttpTransportError::Manifest)?
            .chunks()
            .len();
        let mut chunks = Vec::with_capacity(chunk_count);
        for position in 0..chunk_count {
            chunks.push(
                self.fetch_bytes(
                    format!("{pack_url}/chunks/{position}"),
                    MAX_CHUNK_BYTES as usize,
                )
                .await?,
            );
        }
        Ok(DownloadedGitPack {
            id,
            manifest,
            chunks,
        })
    }

    pub async fn upload_pack(
        &self,
        pack: &BuiltPack,
    ) -> Result<GitInstallReceipt, GitHttpTransportError> {
        self.upload_manifest(pack.id, &pack.manifest_bytes).await?;
        for (position, chunk) in pack.chunks.iter().enumerate() {
            self.upload_chunk(pack.id, position, chunk).await?;
        }
        self.install_pack(pack.id).await
    }

    pub async fn upload_manifest(
        &self,
        id: Digest,
        bytes: &[u8],
    ) -> Result<GitUploadReceipt, GitHttpTransportError> {
        let url = format!("{}/packs/{}/manifest", self.repository_url, hex(&id));
        let response = self.send(self.client.put(url).body(bytes.to_vec())).await?;
        self.read_json(response).await
    }

    pub async fn upload_chunk(
        &self,
        id: Digest,
        position: usize,
        bytes: &[u8],
    ) -> Result<GitUploadReceipt, GitHttpTransportError> {
        let url = format!(
            "{}/packs/{}/chunks/{position}",
            self.repository_url,
            hex(&id)
        );
        let response = self.send(self.client.put(url).body(bytes.to_vec())).await?;
        self.read_json(response).await
    }

    pub async fn install_pack(
        &self,
        id: Digest,
    ) -> Result<GitInstallReceipt, GitHttpTransportError> {
        let url = format!("{}/packs/{}/install", self.repository_url, hex(&id));
        let response = self.send(self.client.post(url)).await?;
        self.read_json(response).await
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, GitHttpTransportError> {
        let response = request
            .header(AUTHORIZATION, &self.authorization)
            .header("x-devspace-machine-id", &self.machine_id)
            .header("x-devspace-incarnation", &self.incarnation)
            .send()
            .await
            .map_err(GitHttpTransportError::Request)?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let message = self
            .read_bounded(response, MAX_ERROR_RESPONSE_BYTES)
            .await
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ErrorResponse>(&bytes).ok())
            .map_or_else(
                || "cloud request failed without an error body".to_owned(),
                |body| match body.code {
                    Some(code) => format!("{code}: {}", body.error),
                    None => body.error,
                },
            );
        Err(GitHttpTransportError::Status { status, message })
    }

    async fn fetch_bytes(
        &self,
        url: String,
        limit: usize,
    ) -> Result<Vec<u8>, GitHttpTransportError> {
        let response = self.send(self.client.get(url)).await?;
        self.read_bounded(response, limit).await
    }

    async fn read_json<T: serde::de::DeserializeOwned>(
        &self,
        response: reqwest::Response,
    ) -> Result<T, GitHttpTransportError> {
        let bytes = self.read_bounded(response, MAX_JSON_RESPONSE_BYTES).await?;
        serde_json::from_slice(&bytes).map_err(GitHttpTransportError::Json)
    }

    async fn read_bounded(
        &self,
        mut response: reqwest::Response,
        limit: usize,
    ) -> Result<Vec<u8>, GitHttpTransportError> {
        if let Some(length) = response.content_length()
            && length > limit as u64
        {
            return Err(GitHttpTransportError::DeclaredTooLarge { length, limit });
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(GitHttpTransportError::Request)?
        {
            if bytes.len() + chunk.len() > limit {
                return Err(GitHttpTransportError::TooLarge { limit });
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

fn request_timeout() -> Duration {
    if std::env::var_os(TEST_HOOKS_ENV).as_deref() == Some(std::ffi::OsStr::new("1"))
        && let Some(milliseconds) = std::env::var(TEST_REQUEST_TIMEOUT_MS_ENV)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
        && milliseconds > 0
    {
        return Duration::from_millis(milliseconds);
    }
    REQUEST_TIMEOUT
}

fn validate_lower_hex(value: &str, length: usize) -> Result<(), ()> {
    if value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(())
    }
}

fn decode_digest(value: &str) -> Result<Digest, GitHttpTransportError> {
    validate_lower_hex(value, 128).map_err(|_| {
        GitHttpTransportError::Protocol(format!(
            "pack ID must be 128 lowercase hex characters, got {value:?}"
        ))
    })?;
    Ok(std::array::from_fn(|index| {
        u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .expect("validated lowercase hexadecimal pair")
    }))
}
