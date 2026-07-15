//! Authenticated machine client for the projection journal owned by a repository Durable Object.

use serde::Deserialize;

use crate::sync_engine::TransportError;

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionState {
    #[serde(deserialize_with = "deserialize_git_oid")]
    pub git_oid: [u8; 20],
    #[serde(deserialize_with = "deserialize_object_id")]
    pub canonical_commit_id: [u8; 64],
    #[serde(deserialize_with = "deserialize_object_id")]
    pub public_commit_id: [u8; 64],
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
pub struct ProjectionObservation {
    pub bookmark: String,
    pub live_oid: Option<[u8; 20]>,
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
    pub previous_fence: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionReplay {
    #[serde(deserialize_with = "deserialize_short_id")]
    pub batch_id: [u8; 16],
    pub remote: String,
    pub policy_epoch: u64,
    #[serde(deserialize_with = "deserialize_short_id")]
    pub owner_machine: [u8; 16],
    pub fence: u64,
    pub updates: Vec<ProjectionUpdate>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HiddenPolicyResult {
    pub changed: bool,
    pub policy_epoch: u64,
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
    pub policy_epoch: u64,
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
    pub policy_epoch: u64,
    pub activation_sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PendingProjectionBatch {
    #[serde(deserialize_with = "deserialize_short_id")]
    pub batch_id: [u8; 16],
    pub remote: String,
    pub policy_epoch: u64,
    #[serde(deserialize_with = "deserialize_short_id")]
    pub owner_machine: [u8; 16],
    pub fence: u64,
    pub refs: Vec<PendingProjectionRef>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PendingProjectionRef {
    pub bookmark: String,
    #[serde(default, deserialize_with = "deserialize_optional_git_oid")]
    pub expected_old_oid: Option<[u8; 20]>,
    #[serde(default, deserialize_with = "deserialize_optional_git_oid")]
    pub proposed_git_oid: Option<[u8; 20]>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionSnapshot {
    pub policy_epoch: u64,
    pub hidden_paths: Vec<String>,
    pub activation_cursor: u64,
    pub cursors: Vec<ProjectionCursor>,
    pub mappings: Vec<ProjectionMapping>,
    pub next_after: u64,
    pub through: u64,
    pub has_more: bool,
    pub pending: Vec<PendingProjectionBatch>,
}

pub struct ProjectionTransport {
    client: reqwest::Client,
    repository_url: String,
    authorization: String,
    incarnation: String,
}

impl ProjectionTransport {
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
            incarnation: hex(&incarnation),
        })
    }

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

    pub async fn mutate_hidden_path(
        &self,
        path: &str,
        hidden: bool,
    ) -> Result<HiddenPolicyResult, TransportError> {
        let body = serde_json::json!({
            "incarnation": self.incarnation,
            "path": path,
            "hidden": hidden,
        });
        let response = self
            .send(
                self.client
                    .post(format!("{}/hidden-policy", self.repository_url))
                    .json(&body),
            )
            .await?;
        self.read_json(response).await
    }

    pub async fn begin_push(
        &self,
        batch_id: [u8; 16],
        machine_id: [u8; 16],
        remote: &str,
        policy_epoch: u64,
        updates: &[ProjectionUpdate],
    ) -> Result<ProjectionBatchResult, TransportError> {
        let updates = updates
            .iter()
            .map(|update| {
                serde_json::json!({
                    "bookmark": update.bookmark,
                    "expectedOldOid": update.expected_old_oid.as_ref().map(|id| hex(id)),
                    "states": update.states.iter().map(|state| serde_json::json!({
                        "gitOid": hex(&state.git_oid),
                        "canonicalCommitId": hex(&state.canonical_commit_id),
                        "publicCommitId": hex(&state.public_commit_id),
                    })).collect::<Vec<_>>(),
                    "proposedState": update.proposed_state,
                })
            })
            .collect::<Vec<_>>();
        let body = serde_json::json!({
            "incarnation": self.incarnation,
            "batchId": hex(&batch_id),
            "machineId": hex(&machine_id),
            "remote": remote,
            "policyEpoch": policy_epoch,
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

    pub async fn claim_push(
        &self,
        batch_id: [u8; 16],
        machine_id: [u8; 16],
    ) -> Result<ProjectionClaimResult, TransportError> {
        let body = serde_json::json!({
            "incarnation": self.incarnation,
            "machineId": hex(&machine_id),
        });
        let response = self
            .send(
                self.client
                    .post(format!(
                        "{}/git/pushes/{}/claim",
                        self.repository_url,
                        hex(&batch_id)
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
                hex(&batch_id),
                self.incarnation,
            )))
            .await?;
        self.read_json(response).await
    }

    pub async fn confirm_push(
        &self,
        batch_id: [u8; 16],
        machine_id: [u8; 16],
        fence: u64,
    ) -> Result<ProjectionBatchResult, TransportError> {
        let body = serde_json::json!({
            "incarnation": self.incarnation,
            "machineId": hex(&machine_id),
            "fence": fence,
        });
        let response = self
            .send(
                self.client
                    .post(format!(
                        "{}/git/pushes/{}/confirm",
                        self.repository_url,
                        hex(&batch_id)
                    ))
                    .json(&body),
            )
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
                    "liveOid": observation.live_oid.as_ref().map(|id| hex(id)),
                })
            })
            .collect::<Vec<_>>();
        let body = serde_json::json!({
            "incarnation": self.incarnation,
            "machineId": hex(&machine_id),
            "fence": fence,
            "observations": observations,
        });
        let response = self
            .send(
                self.client
                    .post(format!(
                        "{}/git/pushes/{}/recover",
                        self.repository_url,
                        hex(&batch_id)
                    ))
                    .json(&body),
            )
            .await?;
        self.read_json(response).await
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
        let bytes = read_bounded(response, MAX_ERROR_BYTES).await?;
        let error = serde_json::from_slice::<ErrorResponse>(&bytes).map_or_else(
            |_| "cloud request failed without an error body".to_owned(),
            |body| body.error,
        );
        Err(format!("cloud request failed with status {status}: {error}").into())
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

#[derive(Deserialize)]
struct ErrorResponse {
    error: String,
}

async fn read_bounded(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, TransportError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > limit {
            return Err(format!("cloud response exceeds the {limit}-byte limit").into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
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
            parse_hex(&value)
                .map_err(|()| serde::de::Error::custom("invalid optional Git object ID"))
        })
        .transpose()
}

fn deserialize_object_id<'de, D>(deserializer: D) -> Result<[u8; 64], D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_hex(deserializer, "object ID")
}

fn deserialize_hex<'de, D, const N: usize>(
    deserializer: D,
    label: &str,
) -> Result<[u8; N], D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    parse_hex(&value).map_err(|()| serde::de::Error::custom(format!("invalid {label}")))
}

fn parse_hex<const N: usize>(value: &str) -> Result<[u8; N], ()> {
    if value.len() != N * 2
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(());
    }
    let mut output = [0; N];
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).map_err(|_| ())?;
    }
    Ok(output)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}
