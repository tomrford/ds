//! Authenticated machine client for the repository directory.

use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::http_client::hardened_http_client;
use crate::wire::{BoundedResponseError, ErrorResponse, read_bounded};
use crate::{
    MachineConfig, RepositoryId, RepositoryIdentity, RepositoryIncarnation, RepositoryName,
    SharedSecret, encode_lower_hex,
};

const MAX_DIRECTORY_RESPONSE_BYTES: usize = 64 * 1024;
const MACHINE_ID_HEADER: &str = "x-devspace-machine-id";

pub struct ControlPlaneClient {
    client: reqwest::Client,
    base_url: String,
    authorization: HeaderValue,
    machine_id: HeaderValue,
    shared_secret: SharedSecret,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CloudRepository {
    pub name: RepositoryName,
    pub identity: RepositoryIdentity,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateRepositoryRequest {
    name: String,
    idempotency_key: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RenameRepositoryRequest {
    new_name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeleteRepositoryRequest<'a> {
    repository_id: &'a str,
    incarnation: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteRepositoryResponse {
    deleted: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryListResponse {
    repositories: Vec<RepositoryResponse>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RepositoryResponse {
    name: String,
    repository_id: String,
    incarnation: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlPlaneRemoteErrorKind {
    RepositoryNotFound,
    RepositoryNameInUse,
    RepositoryCreationRetired,
    RepositoryCreationRetiring,
    IdempotencyKeyReused,
    Other,
}

impl ControlPlaneClient {
    pub fn new(config: &MachineConfig) -> Result<Self, ControlPlaneClientError> {
        let mut authorization =
            HeaderValue::from_str(&format!("Bearer {}", config.shared_secret().expose()))
                .map_err(|_| ControlPlaneClientError::InvalidCredential)?;
        authorization.set_sensitive(true);
        let machine_id = HeaderValue::from_str(config.machine_id().as_str())
            .map_err(|_| ControlPlaneClientError::InvalidMachineId)?;
        Ok(Self {
            client: hardened_http_client()?,
            base_url: config.base_url().to_owned(),
            authorization,
            machine_id,
            shared_secret: config.shared_secret().clone(),
        })
    }

    pub async fn create_repository(
        &self,
        name: &RepositoryName,
        idempotency_key: [u8; 16],
    ) -> Result<CloudRepository, ControlPlaneClientError> {
        let request = self.create_repository_request(name, idempotency_key)?;
        self.send_repository(request, name).await
    }

    pub async fn resolve_repository(
        &self,
        name: &RepositoryName,
    ) -> Result<CloudRepository, ControlPlaneClientError> {
        let request = self
            .authenticated(self.client.get(format!(
                "{}/repositories/{}",
                self.base_url,
                name.as_str()
            )))
            .build()?;
        self.send_repository(request, name).await
    }

    pub async fn list_repositories(&self) -> Result<Vec<CloudRepository>, ControlPlaneClientError> {
        let request = self
            .authenticated(self.client.get(format!("{}/repositories", self.base_url)))
            .build()?;
        let bytes = self.send(request).await?;
        let response: RepositoryListResponse = serde_json::from_slice(&bytes)?;
        response
            .repositories
            .into_iter()
            .map(decode_repository)
            .collect()
    }

    pub async fn rename_repository(
        &self,
        old_name: &RepositoryName,
        new_name: &RepositoryName,
    ) -> Result<CloudRepository, ControlPlaneClientError> {
        let request = self
            .authenticated(self.client.patch(format!(
                "{}/repositories/{}",
                self.base_url,
                old_name.as_str()
            )))
            .json(&RenameRepositoryRequest {
                new_name: new_name.as_str().to_owned(),
            })
            .build()?;
        self.send_repository(request, new_name).await
    }

    pub async fn delete_repository(
        &self,
        name: &RepositoryName,
        identity: &RepositoryIdentity,
    ) -> Result<bool, ControlPlaneClientError> {
        let request = self.delete_repository_request(name, identity)?;
        let bytes = self.send(request).await?;
        Ok(serde_json::from_slice::<DeleteRepositoryResponse>(&bytes)?.deleted)
    }

    fn delete_repository_request(
        &self,
        name: &RepositoryName,
        identity: &RepositoryIdentity,
    ) -> Result<reqwest::Request, ControlPlaneClientError> {
        Ok(self
            .authenticated(self.client.delete(format!(
                "{}/repositories/{}",
                self.base_url,
                name.as_str()
            )))
            .json(&DeleteRepositoryRequest {
                repository_id: identity.repository_id.as_str(),
                incarnation: identity.incarnation.as_str(),
            })
            .build()?)
    }

    fn create_repository_request(
        &self,
        name: &RepositoryName,
        idempotency_key: [u8; 16],
    ) -> Result<reqwest::Request, ControlPlaneClientError> {
        Ok(self
            .authenticated(self.client.post(format!("{}/repositories", self.base_url)))
            .json(&CreateRepositoryRequest {
                name: name.as_str().to_owned(),
                idempotency_key: encode_lower_hex(&idempotency_key),
            })
            .build()?)
    }

    fn authenticated(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request
            .header(AUTHORIZATION, self.authorization.clone())
            .header(MACHINE_ID_HEADER, self.machine_id.clone())
    }

    async fn send_repository(
        &self,
        request: reqwest::Request,
        expected_name: &RepositoryName,
    ) -> Result<CloudRepository, ControlPlaneClientError> {
        let bytes = self.send(request).await?;
        let repository = decode_repository(serde_json::from_slice(&bytes)?)?;
        if &repository.name != expected_name {
            return Err(ControlPlaneClientError::UnexpectedRepositoryName);
        }
        Ok(repository)
    }

    async fn send(&self, request: reqwest::Request) -> Result<Vec<u8>, ControlPlaneClientError> {
        let response = self.client.execute(request).await?;
        let status = response.status();
        let bytes = read_bounded_directory(response, MAX_DIRECTORY_RESPONSE_BYTES).await?;
        if !status.is_success() {
            let (raw_error, kind) = match serde_json::from_slice::<ErrorResponse>(&bytes) {
                Ok(body) => {
                    let kind = classify_remote_error(body.code.as_deref());
                    (body.to_string(), kind)
                }
                Err(_) => (
                    "cloud request failed without a valid error body".to_owned(),
                    ControlPlaneRemoteErrorKind::Other,
                ),
            };
            let error = raw_error.replace(self.shared_secret.expose(), "[REDACTED]");
            return Err(ControlPlaneClientError::Remote {
                status: status.as_u16(),
                kind,
                error,
            });
        }
        Ok(bytes)
    }
}

fn decode_repository(
    response: RepositoryResponse,
) -> Result<CloudRepository, ControlPlaneClientError> {
    Ok(CloudRepository {
        name: RepositoryName::parse(response.name).map_err(Box::new)?,
        identity: RepositoryIdentity::new(
            RepositoryId::parse(response.repository_id).map_err(Box::new)?,
            RepositoryIncarnation::parse(response.incarnation).map_err(Box::new)?,
        ),
    })
}

fn classify_remote_error(code: Option<&str>) -> ControlPlaneRemoteErrorKind {
    match code {
        Some("repository-not-found") => ControlPlaneRemoteErrorKind::RepositoryNotFound,
        Some("repository-name-taken") => ControlPlaneRemoteErrorKind::RepositoryNameInUse,
        Some("creation-retired") => ControlPlaneRemoteErrorKind::RepositoryCreationRetired,
        Some("creation-retiring") => ControlPlaneRemoteErrorKind::RepositoryCreationRetiring,
        Some("idempotency-key-reused") => ControlPlaneRemoteErrorKind::IdempotencyKeyReused,
        _ => ControlPlaneRemoteErrorKind::Other,
    }
}

async fn read_bounded_directory(
    response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, ControlPlaneClientError> {
    read_bounded(response, limit)
        .await
        .map_err(|error| match error {
            BoundedResponseError::Request(source) => ControlPlaneClientError::Request(source),
            BoundedResponseError::DeclaredTooLarge { .. }
            | BoundedResponseError::TooLarge { .. } => {
                ControlPlaneClientError::ResponseTooLarge(limit)
            }
        })
}

#[derive(Debug, Error)]
pub enum ControlPlaneClientError {
    #[error("shared development credential cannot be represented as an HTTP header")]
    InvalidCredential,
    #[error("machine ID cannot be represented as an HTTP header")]
    InvalidMachineId,
    #[error("cloud directory request failed")]
    Request(#[from] reqwest::Error),
    #[error("cloud directory response exceeds the {0}-byte limit")]
    ResponseTooLarge(usize),
    #[error("cloud directory returned HTTP {status}: {error}")]
    Remote {
        status: u16,
        kind: ControlPlaneRemoteErrorKind,
        error: String,
    },
    #[error("cloud directory returned an invalid response")]
    Decode(#[from] serde_json::Error),
    #[error("cloud directory returned a different repository name")]
    UnexpectedRepositoryName,
    #[error(transparent)]
    InvalidRepository(#[from] Box<crate::MachineStoreError>),
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::thread;

    use super::*;
    use crate::{MachineId, SharedSecret};

    fn client(secret: &str) -> ControlPlaneClient {
        let config = MachineConfig::new(
            "https://worker.example.test/base/",
            MachineId::parse("12".repeat(16)).unwrap(),
            SharedSecret::new(secret).unwrap(),
        )
        .unwrap();
        ControlPlaneClient::new(&config).unwrap()
    }

    #[test]
    fn create_request_carries_machine_auth_and_stable_idempotency() {
        let client = client("request-construction-secret");
        let request = client
            .create_repository_request(
                &RepositoryName::parse("request-repository").unwrap(),
                [0x34; 16],
            )
            .unwrap();
        assert_eq!(
            request.url().as_str(),
            "https://worker.example.test/base/repositories"
        );
        assert_eq!(request.headers()[MACHINE_ID_HEADER], "12".repeat(16));
        assert_eq!(
            request.headers()[AUTHORIZATION],
            "Bearer request-construction-secret"
        );
        let body: serde_json::Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "name": "request-repository",
                "idempotencyKey": "34".repeat(16),
            })
        );
    }

    #[test]
    fn delete_request_carries_the_resolved_identity() {
        let client = client("request-construction-secret");
        let identity = RepositoryIdentity::new(
            RepositoryId::parse("ab".repeat(32)).unwrap(),
            RepositoryIncarnation::parse("cd".repeat(16)).unwrap(),
        );
        let request = client
            .delete_repository_request(
                &RepositoryName::parse("removed-repository").unwrap(),
                &identity,
            )
            .unwrap();
        assert_eq!(request.method(), reqwest::Method::DELETE);
        assert_eq!(
            request.url().as_str(),
            "https://worker.example.test/base/repositories/removed-repository"
        );
        let body: serde_json::Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "repositoryId": "ab".repeat(32),
                "incarnation": "cd".repeat(16),
            })
        );
    }

    #[test]
    fn credential_is_redacted_from_debug_output() {
        let secret = "must-never-appear";
        let config = MachineConfig::new(
            "https://worker.example.test",
            MachineId::parse("56".repeat(16)).unwrap(),
            SharedSecret::new(secret).unwrap(),
        )
        .unwrap();
        assert!(!format!("{config:?}").contains(secret));
        let client = ControlPlaneClient::new(&config).unwrap();
        assert!(!format!("{:?}", client.authorization).contains(secret));
    }

    #[test]
    fn classifies_terminal_repository_creation_conflicts() {
        assert_eq!(
            classify_remote_error(Some("repository-not-found")),
            ControlPlaneRemoteErrorKind::RepositoryNotFound
        );
        assert_eq!(
            classify_remote_error(Some("repository-name-taken")),
            ControlPlaneRemoteErrorKind::RepositoryNameInUse
        );
        assert_eq!(
            classify_remote_error(Some("creation-retired")),
            ControlPlaneRemoteErrorKind::RepositoryCreationRetired
        );
        assert_eq!(
            classify_remote_error(Some("creation-retiring")),
            ControlPlaneRemoteErrorKind::RepositoryCreationRetiring
        );
        assert_eq!(
            classify_remote_error(Some("idempotency-key-reused")),
            ControlPlaneRemoteErrorKind::IdempotencyKeyReused
        );
        assert_eq!(
            classify_remote_error(None),
            ControlPlaneRemoteErrorKind::Other
        );
        assert_eq!(
            classify_remote_error(Some("another-conflict")),
            ControlPlaneRemoteErrorKind::Other
        );
    }

    #[tokio::test]
    async fn maps_structured_remote_errors_without_echoing_the_credential() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let _ = connection.read(&mut request).unwrap();
            let body = r#"{"error":"conflict mentions remote-error-secret","code":"repository-name-taken"}"#;
            write!(
                connection,
                "HTTP/1.1 409 Conflict\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        let config = MachineConfig::new(
            format!("http://{address}"),
            MachineId::parse("78".repeat(16)).unwrap(),
            SharedSecret::new("remote-error-secret").unwrap(),
        )
        .unwrap();
        let error = ControlPlaneClient::new(&config)
            .unwrap()
            .resolve_repository(&RepositoryName::parse("missing").unwrap())
            .await
            .unwrap_err();
        server.join().unwrap();
        assert!(matches!(
            &error,
            ControlPlaneClientError::Remote {
                status: 409,
                kind: ControlPlaneRemoteErrorKind::RepositoryNameInUse,
                ..
            }
        ));
        let message = error.to_string();
        assert!(message.contains("[REDACTED]"));
        assert!(!message.contains("remote-error-secret"));
    }
}
