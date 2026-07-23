//! `SyncTransport` over the devspace Worker's HTTP protocol.

use serde::Deserialize;
use std::collections::BTreeSet;

use crate::http_client::hardened_http_client;
use crate::object_closure::{ObjectId, ObjectKey};
use crate::pack_manifest::MAX_MANIFEST_BYTES;
use crate::sync_engine::{
    CloudHeads, DownloadedPack, HeadTransactionResult, PackCatalogEntry, PackCatalogPage,
    SyncTransport, TransportError,
};
use crate::wire::{read_bounded, send};
use crate::{
    MAX_CHUNK_BYTES, MachineConfig, ObjectKind, PackManifest, PendingHeadTransaction,
    decode_lower_hex, encode_lower_hex,
};

const MAX_JSON_RESPONSE_BYTES: usize = 1024 * 1024;

pub struct HttpTransport {
    pub(crate) client: reqwest::Client,
    pub(crate) repository_url: String,
    authorization: reqwest::header::HeaderValue,
    machine_id: String,
    pub(crate) incarnation: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HeadsResponse {
    cursor: u64,
    heads: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogResponse {
    packs: Vec<CatalogEntry>,
    #[serde(rename = "nextAfter")]
    next_after: u64,
    through: u64,
    #[serde(rename = "hasMore")]
    has_more: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogEntry {
    sequence: u64,
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryResponse {
    objects: Vec<InventoryObject>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryObject {
    kind: u8,
    id: String,
}

impl HttpTransport {
    pub fn new(
        config: &MachineConfig,
        repository: &str,
        incarnation: [u8; 16],
    ) -> Result<Self, TransportError> {
        let mut authorization = reqwest::header::HeaderValue::from_str(&format!(
            "Bearer {}",
            config.shared_secret().as_str()
        ))?;
        authorization.set_sensitive(true);
        Ok(Self {
            client: hardened_http_client()?,
            repository_url: format!(
                "{}/repositories/{repository}",
                config.base_url().trim_end_matches('/')
            ),
            authorization,
            machine_id: config.machine_id().as_str().to_owned(),
            incarnation: encode_lower_hex(&incarnation),
        })
    }

    /// Probes whether this machine can read the repository's current heads.
    pub async fn probe_access(&self) -> Result<(), TransportError> {
        let url = format!(
            "{}/heads?incarnation={}",
            self.repository_url, self.incarnation
        );
        let response = self.send(self.client.get(url)).await?;
        read_json::<HeadsResponse>(response).await?;
        Ok(())
    }

    pub(crate) async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, TransportError> {
        send(
            request,
            &self.authorization,
            &self.machine_id,
            &self.incarnation,
        )
        .await
    }

    async fn fetch_bytes(&self, url: String, limit: usize) -> Result<Vec<u8>, TransportError> {
        let response = self.send(self.client.get(url)).await?;
        read_bounded(response, limit)
            .await
            .map_err(|error| Box::new(error) as TransportError)
    }
}

async fn read_json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, TransportError> {
    let bytes = read_bounded(response, MAX_JSON_RESPONSE_BYTES).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

impl SyncTransport for HttpTransport {
    async fn inventory_objects(
        &mut self,
        candidates: &[ObjectKey],
    ) -> Result<BTreeSet<ObjectKey>, TransportError> {
        let url = format!("{}/objects/inventory", self.repository_url);
        let body = serde_json::json!({
            "incarnation": self.incarnation,
            "objects": candidates
                .iter()
                .map(|object| serde_json::json!({
                    "kind": object.kind as u8,
                    "id": encode_lower_hex(&object.id),
                }))
                .collect::<Vec<_>>(),
        });
        let response = self.send(self.client.post(url).json(&body)).await?;
        let response = read_json::<InventoryResponse>(response).await?;
        let mut objects = BTreeSet::new();
        let mut previous = None;
        for object in response.objects {
            let key = ObjectKey {
                kind: parse_object_kind(object.kind)?,
                id: parse_object_id(&object.id)?,
            };
            if previous.is_some_and(|previous| previous >= key) {
                return Err("cloud object inventory is not strictly sorted and unique".into());
            }
            previous = Some(key);
            objects.insert(key);
        }
        Ok(objects)
    }

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
        let pack_url = format!("{}/packs/{}", self.repository_url, encode_lower_hex(&id));
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
        let url = format!(
            "{}/packs/{}/manifest",
            self.repository_url,
            encode_lower_hex(&id)
        );
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
            encode_lower_hex(&id)
        );
        self.send(self.client.put(url).body(bytes.to_vec())).await?;
        Ok(())
    }

    async fn install_pack(&mut self, id: ObjectId) -> Result<(), TransportError> {
        let url = format!(
            "{}/packs/{}/install",
            self.repository_url,
            encode_lower_hex(&id)
        );
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
            "idempotencyKey": encode_lower_hex(&pending.idempotency_key),
            "newHead": encode_lower_hex(&pending.new_head),
            "observedHeads": pending
                .observed_heads
                .iter()
                .map(|head| encode_lower_hex(head))
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
    decode_lower_hex(value)
        .map_err(|_| format!("cloud ID must be 128 lowercase hex characters, got {value:?}").into())
}

fn parse_object_kind(value: u8) -> Result<ObjectKind, TransportError> {
    ObjectKind::try_from(value)
        .map_err(|_| format!("cloud object kind must be from 0 through 5, got {value}").into())
}
