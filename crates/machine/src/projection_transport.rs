//! Authenticated machine client for the projection journal owned by a repository Durable Object.

use serde::Deserialize;

use crate::HttpTransport;
use crate::sync_engine::TransportError;
use crate::wire::read_bounded;
use crate::{decode_lower_hex, encode_lower_hex};

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionState {
    #[serde(deserialize_with = "deserialize_git_oid")]
    pub git_oid: [u8; 20],
    #[serde(deserialize_with = "deserialize_object_id")]
    pub canonical_commit_id: [u8; 64],
    #[serde(deserialize_with = "deserialize_object_id")]
    pub public_commit_id: [u8; 64],
    #[serde(deserialize_with = "deserialize_optional_object_id")]
    pub hidden_set_id: Option<[u8; 64]>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionUpdate {
    pub bookmark: String,
    #[serde(default, deserialize_with = "deserialize_optional_git_oid")]
    pub expected_old_oid: Option<[u8; 20]>,
    pub states: Vec<ProjectionState>,
    pub proposed_state: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FetchRef {
    pub bookmark: String,
    pub observed_git_oid: [u8; 20],
    pub expected_cursor_oid: Option<[u8; 20]>,
    pub states: Vec<ProjectionState>,
    pub proposed_state: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FetchReceipt {
    pub git_oid: [u8; 20],
    pub public_commit_id: [u8; 64],
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FetchResult {
    #[serde(deserialize_with = "deserialize_short_id")]
    pub fetch_id: [u8; 16],
    pub activation_cursor: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionObservation {
    pub bookmark: String,
    pub live_oid: Option<[u8; 20]>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct RegisteredRemote {
    pub name: String,
    pub url: String,
}

#[derive(Deserialize)]
struct RemoteList {
    remotes: Vec<RegisteredRemote>,
}

#[derive(Deserialize)]
struct RemoteUpsert {
    remote: RegisteredRemote,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionBatchResult {
    pub pending: bool,
    pub fence: u64,
    pub outcome: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionClaimResult {
    pub fence: u64,
    previous_fence: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionReplay {
    #[serde(deserialize_with = "deserialize_short_id")]
    batch_id: [u8; 16],
    pub remote: String,
    #[serde(deserialize_with = "deserialize_short_id")]
    pub owner_machine: [u8; 16],
    pub fence: u64,
    pub updates: Vec<ProjectionUpdate>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionMapping {
    pub remote: String,
    pub bookmark: String,
    #[serde(deserialize_with = "deserialize_git_oid")]
    pub git_oid: [u8; 20],
    #[serde(deserialize_with = "deserialize_object_id")]
    pub canonical_commit_id: [u8; 64],
    #[serde(deserialize_with = "deserialize_object_id")]
    pub public_commit_id: [u8; 64],
    #[serde(deserialize_with = "deserialize_optional_object_id")]
    pub hidden_set_id: Option<[u8; 64]>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionCursor {
    pub remote: String,
    pub bookmark: String,
    #[serde(deserialize_with = "deserialize_git_oid")]
    pub git_oid: [u8; 20],
    #[serde(deserialize_with = "deserialize_object_id")]
    pub canonical_commit_id: [u8; 64],
    #[serde(deserialize_with = "deserialize_object_id")]
    pub public_commit_id: [u8; 64],
    #[serde(deserialize_with = "deserialize_optional_object_id")]
    pub hidden_set_id: Option<[u8; 64]>,
    pub activation_sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PendingProjectionBatch {
    #[serde(deserialize_with = "deserialize_short_id")]
    pub batch_id: [u8; 16],
    pub remote: String,
    #[serde(deserialize_with = "deserialize_short_id")]
    owner_machine: [u8; 16],
    fence: u64,
    pub refs: Vec<PendingProjectionRef>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PendingProjectionRef {
    pub bookmark: String,
    #[serde(default, deserialize_with = "deserialize_optional_git_oid")]
    expected_old_oid: Option<[u8; 20]>,
    #[serde(default, deserialize_with = "deserialize_optional_git_oid")]
    pub proposed_git_oid: Option<[u8; 20]>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionSnapshot {
    pub activation_cursor: u64,
    pub cursors: Vec<ProjectionCursor>,
    pub mappings: Vec<ProjectionMapping>,
    pub next_after: u64,
    pub through: u64,
    pub has_more: bool,
    pub pending: Vec<PendingProjectionBatch>,
}

impl HttpTransport {
    pub async fn get(
        &self,
        after: u64,
        through: Option<u64>,
    ) -> Result<ProjectionSnapshot, TransportError> {
        let high_water = through.map_or_else(String::new, |value| format!("&through={value}"));
        let response = self
            .send(self.client.get(format!(
                "{}/projection?incarnation={}&after={after}{high_water}",
                self.repository_url, self.incarnation,
            )))
            .await?;
        self.read_json(response).await
    }

    pub async fn set_remote(
        &self,
        name: &str,
        url: &str,
    ) -> Result<RegisteredRemote, TransportError> {
        let body = serde_json::json!({
            "incarnation": self.incarnation,
            "url": url,
        });
        let response = self
            .send(self.client.put(self.remote_url(name)?).json(&body))
            .await?;
        Ok(self.read_json::<RemoteUpsert>(response).await?.remote)
    }

    pub async fn list_remotes(&self) -> Result<Vec<RegisteredRemote>, TransportError> {
        let response = self
            .send(self.client.get(format!(
                "{}/remotes?incarnation={}",
                self.repository_url, self.incarnation,
            )))
            .await?;
        Ok(self.read_json::<RemoteList>(response).await?.remotes)
    }

    pub async fn begin_push(
        &self,
        batch_id: [u8; 16],
        machine_id: [u8; 16],
        remote: &str,
        updates: &[ProjectionUpdate],
    ) -> Result<ProjectionBatchResult, TransportError> {
        let updates = updates
            .iter()
            .map(|update| {
                serde_json::json!({
                    "bookmark": update.bookmark,
                    "expectedOldOid": update.expected_old_oid.as_ref().map(|id| encode_lower_hex(id)),
                    "states": update.states.iter().map(projection_state_json).collect::<Vec<_>>(),
                    "proposedState": update.proposed_state,
                })
            })
            .collect::<Vec<_>>();
        let body = serde_json::json!({
            "incarnation": self.incarnation,
            "batchId": encode_lower_hex(&batch_id),
            "machineId": encode_lower_hex(&machine_id),
            "remote": remote,
            "updates": updates,
        });
        let response = self
            .send(
                self.client
                    .post(format!("{}/git/pushes", self.repository_url))
                    .json(&body),
            )
            .await?;
        self.read_json(response).await
    }

    pub async fn record_fetch(
        &self,
        fetch_id: [u8; 16],
        machine_id: [u8; 16],
        remote: &str,
        refs: &[FetchRef],
        receipts: &[FetchReceipt],
    ) -> Result<FetchResult, TransportError> {
        let refs = refs
            .iter()
            .map(|fetch_ref| {
                serde_json::json!({
                    "bookmark": fetch_ref.bookmark,
                    "observedGitOid": encode_lower_hex(&fetch_ref.observed_git_oid),
                    "expectedCursorOid": fetch_ref.expected_cursor_oid.as_ref().map(|id| encode_lower_hex(id)),
                    "states": fetch_ref.states.iter().map(projection_state_json).collect::<Vec<_>>(),
                    "proposedState": fetch_ref.proposed_state,
                })
            })
            .collect::<Vec<_>>();
        let receipts = receipts
            .iter()
            .map(|receipt| {
                serde_json::json!({
                    "gitOid": encode_lower_hex(&receipt.git_oid),
                    "publicCommitId": encode_lower_hex(&receipt.public_commit_id),
                })
            })
            .collect::<Vec<_>>();
        let body = serde_json::json!({
            "incarnation": self.incarnation,
            "fetchId": encode_lower_hex(&fetch_id),
            "machineId": encode_lower_hex(&machine_id),
            "remote": remote,
            "refs": refs,
            "receipts": receipts,
        });
        let response = self
            .send(
                self.client
                    .post(format!("{}/git/fetches", self.repository_url))
                    .json(&body),
            )
            .await?;
        self.read_json(response).await
    }

    pub async fn claim_push(
        &self,
        batch_id: [u8; 16],
        machine_id: [u8; 16],
    ) -> Result<ProjectionClaimResult, TransportError> {
        let body = serde_json::json!({
            "incarnation": self.incarnation,
            "machineId": encode_lower_hex(&machine_id),
        });
        let response = self
            .send(
                self.client
                    .post(format!(
                        "{}/git/pushes/{}/claim",
                        self.repository_url,
                        encode_lower_hex(&batch_id)
                    ))
                    .json(&body),
            )
            .await?;
        self.read_json(response).await
    }

    pub async fn get_push_replay(
        &self,
        batch_id: [u8; 16],
    ) -> Result<ProjectionReplay, TransportError> {
        let response = self
            .send(self.client.get(format!(
                "{}/git/pushes/{}/replay?incarnation={}",
                self.repository_url,
                encode_lower_hex(&batch_id),
                self.incarnation,
            )))
            .await?;
        self.read_json(response).await
    }

    pub async fn recover_push(
        &self,
        batch_id: [u8; 16],
        machine_id: [u8; 16],
        fence: u64,
        observations: &[ProjectionObservation],
    ) -> Result<ProjectionBatchResult, TransportError> {
        let observations = observations
            .iter()
            .map(|observation| {
                serde_json::json!({
                    "bookmark": observation.bookmark,
                    "liveOid": observation.live_oid.as_ref().map(|id| encode_lower_hex(id)),
                })
            })
            .collect::<Vec<_>>();
        let body = serde_json::json!({
            "incarnation": self.incarnation,
            "machineId": encode_lower_hex(&machine_id),
            "fence": fence,
            "observations": observations,
        });
        let response = self
            .send(
                self.client
                    .post(format!(
                        "{}/git/pushes/{}/recover",
                        self.repository_url,
                        encode_lower_hex(&batch_id)
                    ))
                    .json(&body),
            )
            .await?;
        self.read_json(response).await
    }

    fn remote_url(&self, name: &str) -> Result<reqwest::Url, TransportError> {
        let mut url = reqwest::Url::parse(&self.repository_url)?;
        url.path_segments_mut()
            .map_err(|()| "repository URL cannot contain path segments")?
            .extend(["remotes", name]);
        Ok(url)
    }

    async fn read_json<T: serde::de::DeserializeOwned>(
        &self,
        response: reqwest::Response,
    ) -> Result<T, TransportError> {
        Ok(serde_json::from_slice(
            &read_bounded(response, MAX_RESPONSE_BYTES).await?,
        )?)
    }
}

fn deserialize_short_id<'de, D>(deserializer: D) -> Result<[u8; 16], D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_hex(deserializer, "short ID")
}

fn deserialize_git_oid<'de, D>(deserializer: D) -> Result<[u8; 20], D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_hex(deserializer, "Git object ID")
}

fn deserialize_optional_git_oid<'de, D>(deserializer: D) -> Result<Option<[u8; 20]>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    value
        .map(|value| {
            decode_lower_hex(&value)
                .map_err(|_| serde::de::Error::custom("invalid optional Git object ID"))
        })
        .transpose()
}

fn deserialize_object_id<'de, D>(deserializer: D) -> Result<[u8; 64], D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_hex(deserializer, "object ID")
}

fn deserialize_optional_object_id<'de, D>(deserializer: D) -> Result<Option<[u8; 64]>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    value
        .map(|value| {
            decode_lower_hex(&value)
                .map_err(|_| serde::de::Error::custom("invalid optional object ID"))
        })
        .transpose()
}

fn deserialize_hex<'de, D, const N: usize>(
    deserializer: D,
    label: &str,
) -> Result<[u8; N], D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    decode_lower_hex(&value).map_err(|_| serde::de::Error::custom(format!("invalid {label}")))
}

fn projection_state_json(state: &ProjectionState) -> serde_json::Value {
    serde_json::json!({
        "gitOid": encode_lower_hex(&state.git_oid),
        "canonicalCommitId": encode_lower_hex(&state.canonical_commit_id),
        "publicCommitId": encode_lower_hex(&state.public_commit_id),
        "hiddenSetId": state.hidden_set_id.as_ref().map(|id| encode_lower_hex(id)),
    })
}
