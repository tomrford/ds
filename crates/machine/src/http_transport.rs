//! `SyncTransport` over the spike Worker's HTTP protocol.

use std::collections::BTreeSet;

use serde::Deserialize;

use crate::object_closure::ObjectId;
use crate::pack_manifest::MAX_MANIFEST_BYTES;
use crate::sync_engine::{
    CloudHeads, DownloadedPack, HeadTransactionResult, PackCatalogEntry, PackCatalogPage,
    SyncTransport, TransportError,
};
use crate::{MAX_CHUNK_BYTES, PackManifest, PendingHeadTransaction};

const MAX_JSON_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_ERROR_RESPONSE_BYTES: usize = 16 * 1024;

pub struct HttpTransport {
    client: reqwest::Client,
    repository_url: String,
    authorization: String,
    incarnation: String,
}

#[derive(Deserialize)]
struct HeadsResponse {
    cursor: u64,
    heads: Vec<String>,
}

#[derive(Deserialize)]
struct CatalogResponse {
    packs: Vec<CatalogEntry>,
    #[serde(rename = "nextAfter")]
    next_after: u64,
    through: u64,
    #[serde(rename = "hasMore")]
    has_more: bool,
}

#[derive(Deserialize)]
struct CatalogEntry {
    sequence: u64,
    id: String,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: String,
}

impl HttpTransport {
    pub fn new(
        base_url: &str,
        repository: &str,
        token: &str,
        incarnation: [u8; 16],
    ) -> Result<Self, TransportError> {
        Ok(Self {
            client: reqwest::Client::builder().build()?,
            repository_url: format!(
                "{}/repositories/{repository}",
                base_url.trim_end_matches('/')
            ),
            authorization: format!("Bearer {token}"),
            incarnation: hex_bytes(&incarnation),
        })
    }

    /// Initializes the repository Durable Object with this transport's
    /// incarnation. Repeating the same initialization is safe.
    pub async fn initialize(&self) -> Result<(), TransportError> {
        let url = format!(
            "{}/initialize?incarnation={}",
            self.repository_url, self.incarnation
        );
        let response = self.send(self.client.post(url)).await?;
        read_json::<HeadsResponse>(response).await?;
        Ok(())
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, TransportError> {
        let response = request
            .header(reqwest::header::AUTHORIZATION, &self.authorization)
            .send()
            .await?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let message = read_bounded(response, MAX_ERROR_RESPONSE_BYTES)
            .await
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ErrorResponse>(&bytes).ok())
            .map_or_else(
                || "cloud request failed without an error body".to_string(),
                |body| body.error,
            );
        Err(format!("cloud request failed with status {status}: {message}").into())
    }

    async fn fetch_bytes(&self, url: String, limit: usize) -> Result<Vec<u8>, TransportError> {
        let response = self.send(self.client.get(url)).await?;
        read_bounded(response, limit).await
    }
}

async fn read_bounded(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, TransportError> {
    if let Some(length) = response.content_length()
        && length > limit as u64
    {
        return Err(format!(
            "cloud response declares {length} bytes, exceeding the {limit}-byte limit"
        )
        .into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > limit {
            return Err(format!("cloud response exceeds the {limit}-byte limit").into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn read_json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, TransportError> {
    let bytes = read_bounded(response, MAX_JSON_RESPONSE_BYTES).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

impl SyncTransport for HttpTransport {
    async fn list_packs(
        &mut self,
        after: u64,
        through: Option<u64>,
    ) -> Result<PackCatalogPage, TransportError> {
        let mut url = format!(
            "{}/packs?incarnation={}&after={after}",
            self.repository_url, self.incarnation
        );
        if let Some(through) = through {
            url.push_str(&format!("&through={through}"));
        }
        let response = self.send(self.client.get(url)).await?;
        let catalog = read_json::<CatalogResponse>(response).await?;
        Ok(PackCatalogPage {
            packs: catalog
                .packs
                .into_iter()
                .map(|entry| {
                    Ok(PackCatalogEntry {
                        sequence: entry.sequence,
                        id: parse_object_id(&entry.id)?,
                    })
                })
                .collect::<Result<Vec<_>, TransportError>>()?,
            next_after: catalog.next_after,
            through: catalog.through,
            has_more: catalog.has_more,
        })
    }

    async fn download_pack(&mut self, id: ObjectId) -> Result<DownloadedPack, TransportError> {
        let pack_url = format!("{}/packs/{}", self.repository_url, hex_bytes(&id));
        let manifest = self
            .fetch_bytes(
                format!("{pack_url}/manifest?incarnation={}", self.incarnation),
                MAX_MANIFEST_BYTES,
            )
            .await?;
        let chunk_count = PackManifest::decode(&manifest)?.chunks().len();
        let mut chunks = Vec::with_capacity(chunk_count);
        for position in 0..chunk_count {
            chunks.push(
                self.fetch_bytes(
                    format!(
                        "{pack_url}/chunks/{position}?incarnation={}",
                        self.incarnation
                    ),
                    MAX_CHUNK_BYTES as usize,
                )
                .await?,
            );
        }
        Ok(DownloadedPack {
            id,
            manifest,
            chunks,
        })
    }

    async fn upload_manifest(&mut self, id: ObjectId, bytes: &[u8]) -> Result<(), TransportError> {
        let url = format!("{}/packs/{}/manifest", self.repository_url, hex_bytes(&id));
        self.send(self.client.put(url).body(bytes.to_vec())).await?;
        Ok(())
    }

    async fn upload_chunk(
        &mut self,
        id: ObjectId,
        position: usize,
        bytes: &[u8],
    ) -> Result<(), TransportError> {
        let url = format!(
            "{}/packs/{}/chunks/{position}",
            self.repository_url,
            hex_bytes(&id)
        );
        self.send(self.client.put(url).body(bytes.to_vec())).await?;
        Ok(())
    }

    async fn install_pack(&mut self, id: ObjectId) -> Result<(), TransportError> {
        let url = format!("{}/packs/{}/install", self.repository_url, hex_bytes(&id));
        self.send(self.client.post(url)).await?;
        Ok(())
    }

    async fn get_heads(&mut self) -> Result<CloudHeads, TransportError> {
        let url = format!(
            "{}/heads?incarnation={}",
            self.repository_url, self.incarnation
        );
        let response = self.send(self.client.get(url)).await?;
        parse_heads(read_json::<HeadsResponse>(response).await?)
    }

    async fn transact_heads(
        &mut self,
        pending: &PendingHeadTransaction,
    ) -> Result<HeadTransactionResult, TransportError> {
        let url = format!("{}/heads", self.repository_url);
        let body = serde_json::json!({
            "incarnation": self.incarnation,
            "idempotencyKey": hex_bytes(&pending.idempotency_key),
            "newHead": hex_bytes(&pending.new_head),
            "observedHeads": pending
                .observed_heads
                .iter()
                .map(|head| hex_bytes(head))
                .collect::<Vec<_>>(),
        });
        let response = self.send(self.client.post(url).json(&body)).await?;
        parse_heads(read_json::<HeadsResponse>(response).await?)
    }
}

fn parse_heads(response: HeadsResponse) -> Result<CloudHeads, TransportError> {
    let mut heads = BTreeSet::new();
    for head in &response.heads {
        if !heads.insert(parse_object_id(head)?) {
            return Err("cloud head set contains duplicates".into());
        }
    }
    Ok(CloudHeads {
        cursor: response.cursor,
        heads,
    })
}

fn parse_object_id(value: &str) -> Result<ObjectId, TransportError> {
    if value.len() != 128 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("cloud ID must be 128 hex characters, got {value:?}").into());
    }
    let mut id = [0; 64];
    for (index, byte) in id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|error| format!("invalid cloud ID {value:?}: {error}"))?;
    }
    Ok(id)
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
