//! HTTP transport for the Worker's v2 Git-object pack store.

use std::collections::BTreeSet;
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde::Deserialize;
use thiserror::Error;

use crate::pack_manifest::MAX_MANIFEST_BYTES;
use crate::{
    BuiltPack, CloudOpHeads, Digest, MAX_CHUNK_BYTES, Oid, OpId, OpObjectKey, OpObjectKind,
    OpSyncTransport, OpTransportError, PackManifest, PackManifestError, PackOptions,
    PendingOpHeadTransaction, build_packs, hex,
};

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionGitState {
    pub canonical_oid: Oid,
    pub public_oid: Oid,
    pub hidden_set_id: Option<[u8; 64]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionGitUpdate {
    pub bookmark: String,
    pub expected_old_oid: Option<Oid>,
    pub states: Vec<ProjectionGitState>,
    pub proposed_state: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionGitFetchRef {
    pub bookmark: String,
    pub observed_public_oid: Oid,
    pub expected_cursor_oid: Option<Oid>,
    pub states: Vec<ProjectionGitState>,
    pub proposed_state: Option<usize>,
    pub identity_oid: Option<Oid>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionGitFetchResult {
    #[serde(deserialize_with = "deserialize_short_id")]
    pub fetch_id: [u8; 16],
    pub activation_cursor: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionGitObservation {
    pub bookmark: String,
    pub live_oid: Option<Oid>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct RegisteredGitRemote {
    pub name: String,
    pub url: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionGitBatchResult {
    pub pending: bool,
    pub fence: u64,
    pub outcome: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionGitClaimResult {
    #[serde(default)]
    pub pending: bool,
    pub fence: u64,
    pub previous_fence: Option<u64>,
    pub outcome: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionGitReplay {
    #[serde(deserialize_with = "deserialize_short_id")]
    pub batch_id: [u8; 16],
    pub remote: String,
    #[serde(deserialize_with = "deserialize_short_id")]
    pub owner_machine: [u8; 16],
    pub fence: u64,
    #[serde(deserialize_with = "deserialize_updates")]
    pub updates: Vec<ProjectionGitUpdate>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionGitMapping {
    pub remote: String,
    pub bookmark: String,
    #[serde(deserialize_with = "deserialize_oid")]
    pub canonical_oid: Oid,
    #[serde(deserialize_with = "deserialize_oid")]
    pub public_oid: Oid,
    #[serde(deserialize_with = "deserialize_optional_hidden_set")]
    pub hidden_set_id: Option<[u8; 64]>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionGitCursor {
    pub remote: String,
    pub bookmark: String,
    #[serde(deserialize_with = "deserialize_oid")]
    pub canonical_oid: Oid,
    #[serde(deserialize_with = "deserialize_oid")]
    pub public_oid: Oid,
    #[serde(deserialize_with = "deserialize_optional_hidden_set")]
    pub hidden_set_id: Option<[u8; 64]>,
    pub activation_sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PendingProjectionGitRef {
    pub bookmark: String,
    #[serde(default, deserialize_with = "deserialize_optional_oid")]
    pub expected_old_oid: Option<Oid>,
    #[serde(default, deserialize_with = "deserialize_optional_oid")]
    pub proposed_public_oid: Option<Oid>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PendingProjectionGitBatch {
    #[serde(deserialize_with = "deserialize_short_id")]
    pub batch_id: [u8; 16],
    pub remote: String,
    #[serde(deserialize_with = "deserialize_short_id")]
    pub owner_machine: [u8; 16],
    pub fence: u64,
    pub refs: Vec<PendingProjectionGitRef>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionGitSnapshot {
    pub activation_cursor: u64,
    pub cursors: Vec<ProjectionGitCursor>,
    pub mappings: Vec<ProjectionGitMapping>,
    pub next_after: u64,
    pub through: u64,
    pub has_more: bool,
    pub pending: Vec<PendingProjectionGitBatch>,
}

#[derive(Deserialize)]
struct RemoteList {
    remotes: Vec<RegisteredGitRemote>,
}

#[derive(Deserialize)]
struct RemoteUpsert {
    remote: RegisteredGitRemote,
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
        code: Option<String>,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpInventoryResponse {
    keys: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpHeadsResponse {
    cursor: u64,
    heads: Vec<String>,
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

    async fn op_inventory(
        &self,
        candidates: &[OpObjectKey],
    ) -> Result<BTreeSet<OpObjectKey>, GitHttpTransportError> {
        let mut keys = candidates.iter().map(op_key).collect::<Vec<_>>();
        keys.sort_unstable();
        let response = self
            .send(
                self.client
                    .post(format!("{}/ops/inventory", self.repository_url))
                    .json(&serde_json::json!({ "keys": keys })),
            )
            .await?;
        let response: OpInventoryResponse = self.read_json(response).await?;
        let present = response
            .keys
            .into_iter()
            .map(|key| decode_op_key(&key))
            .collect::<Result<BTreeSet<_>, _>>()?;
        if !present
            .iter()
            .all(|key| candidates.binary_search(key).is_ok())
        {
            return Err(GitHttpTransportError::Protocol(
                "operation inventory returned an unrequested object".to_owned(),
            ));
        }
        Ok(present)
    }

    async fn op_download(&self, key: OpObjectKey) -> Result<Vec<u8>, GitHttpTransportError> {
        self.fetch_bytes(
            format!("{}/ops/{}", self.repository_url, op_key_path(key)),
            1024 * 1024,
        )
        .await
    }

    async fn op_upload(&self, key: OpObjectKey, bytes: &[u8]) -> Result<(), GitHttpTransportError> {
        let response = self
            .send(
                self.client
                    .put(format!("{}/ops/{}", self.repository_url, op_key_path(key)))
                    .body(bytes.to_vec()),
            )
            .await?;
        let _: serde_json::Value = self.read_json(response).await?;
        Ok(())
    }

    async fn op_heads(&self) -> Result<CloudOpHeads, GitHttpTransportError> {
        let response = self
            .send(
                self.client
                    .get(format!("{}/ops/heads", self.repository_url)),
            )
            .await?;
        decode_op_heads(self.read_json(response).await?)
    }

    async fn op_transact(
        &self,
        pending: &PendingOpHeadTransaction,
    ) -> Result<CloudOpHeads, GitHttpTransportError> {
        let body = serde_json::json!({
            "incarnation": self.incarnation,
            "idempotencyKey": hex(&pending.idempotency_key),
            "newHead": hex(&pending.new_head),
            "observedHeads": pending.observed_heads.iter().map(|head| hex(head)).collect::<Vec<_>>(),
        });
        let response = self
            .send(
                self.client
                    .post(format!("{}/ops/heads/transactions", self.repository_url))
                    .json(&body),
            )
            .await?;
        decode_op_heads(self.read_json(response).await?)
    }

    pub async fn projection_snapshot(
        &self,
        after: u64,
        through: Option<u64>,
    ) -> Result<ProjectionGitSnapshot, GitHttpTransportError> {
        let high_water = through.map_or_else(String::new, |value| format!("&through={value}"));
        let url = format!(
            "{}/projection?incarnation={}&after={after}{high_water}",
            self.repository_url, self.incarnation
        );
        let response = self.send(self.client.get(url)).await?;
        self.read_json(response).await
    }

    pub async fn projection_snapshot_all(
        &self,
    ) -> Result<ProjectionGitSnapshot, GitHttpTransportError> {
        let mut page = self.projection_snapshot(0, None).await?;
        let through = page.through;
        let mut mappings = std::mem::take(&mut page.mappings);
        let mut after = page.next_after;
        while page.has_more {
            page = self.projection_snapshot(after, Some(through)).await?;
            after = page.next_after;
            mappings.append(&mut page.mappings);
        }
        page.mappings = mappings;
        Ok(page)
    }

    pub async fn set_remote(
        &self,
        name: &str,
        url: &str,
    ) -> Result<RegisteredGitRemote, GitHttpTransportError> {
        let body = serde_json::json!({ "incarnation": self.incarnation, "url": url });
        let response = self
            .send(self.client.put(self.remote_url(name)?).json(&body))
            .await?;
        Ok(self.read_json::<RemoteUpsert>(response).await?.remote)
    }

    pub async fn list_remotes(&self) -> Result<Vec<RegisteredGitRemote>, GitHttpTransportError> {
        let url = format!(
            "{}/remotes?incarnation={}",
            self.repository_url, self.incarnation
        );
        let response = self.send(self.client.get(url)).await?;
        Ok(self.read_json::<RemoteList>(response).await?.remotes)
    }

    pub async fn begin_push(
        &self,
        batch_id: [u8; 16],
        remote: &str,
        updates: &[ProjectionGitUpdate],
    ) -> Result<ProjectionGitBatchResult, GitHttpTransportError> {
        let body = serde_json::json!({
            "incarnation": self.incarnation,
            "batchId": hex(&batch_id),
            "machineId": self.machine_id,
            "remote": remote,
            "updates": updates.iter().map(update_json).collect::<Vec<_>>(),
        });
        let response = self
            .send(
                self.client
                    .post(format!("{}/projection/pushes", self.repository_url))
                    .json(&body),
            )
            .await?;
        self.read_json(response).await
    }

    pub async fn claim_push(
        &self,
        batch_id: [u8; 16],
    ) -> Result<ProjectionGitClaimResult, GitHttpTransportError> {
        let body = serde_json::json!({
            "incarnation": self.incarnation,
            "machineId": self.machine_id,
        });
        let response = self
            .send(
                self.client
                    .post(format!(
                        "{}/projection/pushes/{}/claim",
                        self.repository_url,
                        hex(&batch_id)
                    ))
                    .json(&body),
            )
            .await?;
        self.read_json(response).await
    }

    pub async fn push_replay(
        &self,
        batch_id: [u8; 16],
    ) -> Result<ProjectionGitReplay, GitHttpTransportError> {
        let url = format!(
            "{}/projection/pushes/{}/replay?incarnation={}",
            self.repository_url,
            hex(&batch_id),
            self.incarnation
        );
        let response = self.send(self.client.get(url)).await?;
        self.read_json(response).await
    }

    pub async fn recover_push(
        &self,
        batch_id: [u8; 16],
        fence: u64,
        observations: &[ProjectionGitObservation],
    ) -> Result<ProjectionGitBatchResult, GitHttpTransportError> {
        let body = serde_json::json!({
            "incarnation": self.incarnation,
            "machineId": self.machine_id,
            "fence": fence,
            "observations": observations.iter().map(|observation| serde_json::json!({
                "bookmark": observation.bookmark,
                "liveOid": observation.live_oid.map(|oid| hex(&oid.0)),
            })).collect::<Vec<_>>(),
        });
        let response = self
            .send(
                self.client
                    .post(format!(
                        "{}/projection/pushes/{}/recover",
                        self.repository_url,
                        hex(&batch_id)
                    ))
                    .json(&body),
            )
            .await?;
        self.read_json(response).await
    }

    pub async fn record_fetch(
        &self,
        fetch_id: [u8; 16],
        remote: &str,
        refs: &[ProjectionGitFetchRef],
    ) -> Result<ProjectionGitFetchResult, GitHttpTransportError> {
        let body = serde_json::json!({
            "incarnation": self.incarnation,
            "fetchId": hex(&fetch_id),
            "machineId": self.machine_id,
            "remote": remote,
            "refs": refs.iter().map(|fetch_ref| serde_json::json!({
                "bookmark": fetch_ref.bookmark,
                "observedPublicOid": hex(&fetch_ref.observed_public_oid.0),
                "expectedCursorOid": fetch_ref.expected_cursor_oid.map(|oid| hex(&oid.0)),
                "states": fetch_ref.states.iter().map(state_json).collect::<Vec<_>>(),
                "proposedState": fetch_ref.proposed_state,
                "identityOid": fetch_ref.identity_oid.map(|oid| hex(&oid.0)),
            })).collect::<Vec<_>>(),
        });
        let response = self
            .send(
                self.client
                    .post(format!("{}/projection/fetches", self.repository_url))
                    .json(&body),
            )
            .await?;
        self.read_json(response).await
    }

    fn remote_url(&self, name: &str) -> Result<reqwest::Url, GitHttpTransportError> {
        let mut url = reqwest::Url::parse(&self.repository_url)
            .map_err(|error| GitHttpTransportError::InvalidBaseUrl(error.to_string()))?;
        url.path_segments_mut()
            .map_err(|()| {
                GitHttpTransportError::Protocol(
                    "repository URL cannot contain path segments".to_owned(),
                )
            })?
            .extend(["remotes", name]);
        Ok(url)
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
        let error = self
            .read_bounded(response, MAX_ERROR_RESPONSE_BYTES)
            .await
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ErrorResponse>(&bytes).ok());
        let code = error.as_ref().and_then(|body| body.code.clone());
        let message = error.map_or_else(
            || "cloud request failed without an error body".to_owned(),
            |body| match &body.code {
                Some(code) => format!("{code}: {}", body.error),
                None => body.error,
            },
        );
        Err(GitHttpTransportError::Status {
            status,
            code,
            message,
        })
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

impl OpSyncTransport for GitHttpTransport {
    async fn download_git_objects(
        &mut self,
        repository: &crate::MachineGitRepository,
        mut after: u64,
    ) -> Result<u64, OpTransportError> {
        let mut through = None;
        loop {
            let page = self
                .list_packs(after, through)
                .await
                .map_err(|error| Box::new(error) as OpTransportError)?;
            through = Some(page.through);
            for pack in page.packs {
                let downloaded = self
                    .download_pack(pack.id)
                    .await
                    .map_err(|error| Box::new(error) as OpTransportError)?;
                repository
                    .install_pack(downloaded.id, &downloaded.manifest, &downloaded.chunks)
                    .map_err(|error| Box::new(error) as OpTransportError)?;
            }
            if !page.has_more {
                return Ok(page.through);
            }
            after = page.next_after;
        }
    }

    async fn upload_git_objects(
        &mut self,
        repository: &crate::MachineGitRepository,
        heads: &BTreeSet<Oid>,
    ) -> Result<(), OpTransportError> {
        let heads = heads.iter().copied().filter(|oid| oid.0 != [0; 20]);
        let closure = repository
            .object_closure(heads)
            .map_err(|error| Box::new(error) as OpTransportError)?;
        if closure.objects.is_empty() {
            return Ok(());
        }
        let packs = build_packs(
            repository,
            &closure,
            &BTreeSet::new(),
            PackOptions::default(),
        )
        .map_err(|error| Box::new(error) as OpTransportError)?;
        for pack in &packs.packs {
            self.upload_pack(pack)
                .await
                .map_err(|error| Box::new(error) as OpTransportError)?;
        }
        Ok(())
    }

    async fn inventory_op_objects(
        &mut self,
        candidates: &[OpObjectKey],
    ) -> Result<BTreeSet<OpObjectKey>, OpTransportError> {
        self.op_inventory(candidates)
            .await
            .map_err(|error| Box::new(error) as OpTransportError)
    }

    async fn download_op_object(&mut self, key: OpObjectKey) -> Result<Vec<u8>, OpTransportError> {
        self.op_download(key)
            .await
            .map_err(|error| Box::new(error) as OpTransportError)
    }

    async fn upload_op_object(
        &mut self,
        key: OpObjectKey,
        bytes: &[u8],
    ) -> Result<(), OpTransportError> {
        self.op_upload(key, bytes)
            .await
            .map_err(|error| Box::new(error) as OpTransportError)
    }

    async fn get_op_heads(&mut self) -> Result<CloudOpHeads, OpTransportError> {
        self.op_heads()
            .await
            .map_err(|error| Box::new(error) as OpTransportError)
    }

    async fn transact_op_heads(
        &mut self,
        pending: &PendingOpHeadTransaction,
    ) -> Result<CloudOpHeads, OpTransportError> {
        self.op_transact(pending)
            .await
            .map_err(|error| Box::new(error) as OpTransportError)
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

fn op_key(key: &OpObjectKey) -> String {
    let prefix = match key.kind {
        OpObjectKind::View => "v",
        OpObjectKind::Operation => "o",
    };
    format!("{prefix}:{}", hex(&key.id))
}

fn op_key_path(key: OpObjectKey) -> String {
    let directory = match key.kind {
        OpObjectKind::View => "views",
        OpObjectKind::Operation => "operations",
    };
    format!("{directory}/{}", hex(&key.id))
}

fn decode_op_key(value: &str) -> Result<OpObjectKey, GitHttpTransportError> {
    let (prefix, id) = value.split_once(':').ok_or_else(|| {
        GitHttpTransportError::Protocol("operation inventory key lacks a kind".to_owned())
    })?;
    let kind = match prefix {
        "v" => OpObjectKind::View,
        "o" => OpObjectKind::Operation,
        _ => {
            return Err(GitHttpTransportError::Protocol(
                "operation inventory key has an unknown kind".to_owned(),
            ));
        }
    };
    Ok(OpObjectKey {
        kind,
        id: decode_op_id(id)?,
    })
}

fn decode_op_id(value: &str) -> Result<OpId, GitHttpTransportError> {
    validate_lower_hex(value, 128).map_err(|_| {
        GitHttpTransportError::Protocol(
            "operation ID must be 128 lowercase hex characters".to_owned(),
        )
    })?;
    Ok(std::array::from_fn(|index| {
        u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).expect("validated operation ID")
    }))
}

fn decode_op_heads(response: OpHeadsResponse) -> Result<CloudOpHeads, GitHttpTransportError> {
    let heads = response
        .heads
        .into_iter()
        .map(|head| decode_op_id(&head))
        .collect::<Result<BTreeSet<_>, _>>()?;
    Ok(CloudOpHeads {
        cursor: response.cursor,
        heads,
    })
}

fn state_json(state: &ProjectionGitState) -> serde_json::Value {
    serde_json::json!({
        "canonicalOid": hex(&state.canonical_oid.0),
        "publicOid": hex(&state.public_oid.0),
        "hiddenSetId": state.hidden_set_id.as_ref().map(|id| hex(id)),
    })
}

fn update_json(update: &ProjectionGitUpdate) -> serde_json::Value {
    serde_json::json!({
        "bookmark": update.bookmark,
        "expectedOldOid": update.expected_old_oid.map(|oid| hex(&oid.0)),
        "states": update.states.iter().map(state_json).collect::<Vec<_>>(),
        "proposedState": update.proposed_state,
    })
}

fn deserialize_hex<'de, const N: usize, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<[u8; N], D::Error> {
    let value = String::deserialize(deserializer)?;
    validate_lower_hex(&value, N * 2).map_err(|()| {
        serde::de::Error::custom(format!("expected {} lowercase hex characters", N * 2))
    })?;
    Ok(std::array::from_fn(|index| {
        u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).expect("validated hex")
    }))
}

fn deserialize_short_id<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<[u8; 16], D::Error> {
    deserialize_hex(deserializer)
}

fn deserialize_oid<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Oid, D::Error> {
    deserialize_hex(deserializer).map(Oid)
}

fn deserialize_optional_oid<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Oid>, D::Error> {
    Option::<String>::deserialize(deserializer)?.map_or(Ok(None), |value| {
        validate_lower_hex(&value, 40)
            .map_err(|()| serde::de::Error::custom("expected 40 lowercase hex characters"))?;
        Oid::from_hex(value.as_bytes())
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom("invalid Git object ID"))
    })
}

fn deserialize_optional_hidden_set<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<[u8; 64]>, D::Error> {
    Option::<String>::deserialize(deserializer)?.map_or(Ok(None), |value| {
        let deserializer = serde::de::value::StringDeserializer::<D::Error>::new(value);
        deserialize_hex(deserializer).map(Some)
    })
}

fn deserialize_updates<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<ProjectionGitUpdate>, D::Error> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct WireUpdate {
        bookmark: String,
        #[serde(default, deserialize_with = "deserialize_optional_oid")]
        expected_old_oid: Option<Oid>,
        states: Vec<WireState>,
        proposed_state: Option<usize>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct WireState {
        #[serde(deserialize_with = "deserialize_oid")]
        canonical_oid: Oid,
        #[serde(deserialize_with = "deserialize_oid")]
        public_oid: Oid,
        #[serde(deserialize_with = "deserialize_optional_hidden_set")]
        hidden_set_id: Option<[u8; 64]>,
    }
    Vec::<WireUpdate>::deserialize(deserializer).map(|updates| {
        updates
            .into_iter()
            .map(|update| ProjectionGitUpdate {
                bookmark: update.bookmark,
                expected_old_oid: update.expected_old_oid,
                states: update
                    .states
                    .into_iter()
                    .map(|state| ProjectionGitState {
                        canonical_oid: state.canonical_oid,
                        public_oid: state.public_oid,
                        hidden_set_id: state.hidden_set_id,
                    })
                    .collect(),
                proposed_state: update.proposed_state,
            })
            .collect()
    })
}
