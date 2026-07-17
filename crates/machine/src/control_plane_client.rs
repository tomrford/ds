//! Authenticated machine client for the repository directory.

use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::locked_json::hex_bytes;
use crate::{
    MachineConfig, RepositoryId, RepositoryIdentity, RepositoryIncarnation, RepositoryName,
    SharedSecret,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RepositoryResponse {
    name: String,
    repository_id: String,
    incarnation: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrorResponse {
    error: String,
    code: Option<String>,
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
            client: reqwest::Client::builder().build()?,
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

    fn create_repository_request(
        &self,
        name: &RepositoryName,
        idempotency_key: [u8; 16],
    ) -> Result<reqwest::Request, ControlPlaneClientError> {
        Ok(self
            .authenticated(self.client.post(format!("{}/repositories", self.base_url)))
            .json(&CreateRepositoryRequest {
                name: name.as_str().to_owned(),
                idempotency_key: hex_bytes(&idempotency_key),
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
        let response = self.client.execute(request).await?;
        let status = response.status();
        let bytes = read_bounded(response, MAX_DIRECTORY_RESPONSE_BYTES).await?;
        if !status.is_success() {
            let (raw_error, kind) = match serde_json::from_slice::<ErrorResponse>(&bytes) {
                Ok(body) => {
                    let kind = classify_remote_error(body.code.as_deref());
                    (body.error, kind)
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
        let response: RepositoryResponse = serde_json::from_slice(&bytes)?;
        let repository = CloudRepository {
            name: RepositoryName::parse(response.name).map_err(Box::new)?,
            identity: RepositoryIdentity::new(
                RepositoryId::parse(response.repository_id).map_err(Box::new)?,
                RepositoryIncarnation::parse(response.incarnation).map_err(Box::new)?,
            ),
        };
        if &repository.name != expected_name {
            return Err(ControlPlaneClientError::UnexpectedRepositoryName);
        }
        Ok(repository)
    }
}

fn classify_remote_error(code: Option<&str>) -> ControlPlaneRemoteErrorKind {
    match code {
        Some("repository-not-found") => ControlPlaneRemoteErrorKind::RepositoryNotFound,
        Some("name-in-use") => ControlPlaneRemoteErrorKind::RepositoryNameInUse,
        Some("creation-retired") => ControlPlaneRemoteErrorKind::RepositoryCreationRetired,
        Some("creation-retiring") => ControlPlaneRemoteErrorKind::RepositoryCreationRetiring,
        Some("idempotency-key-reused") => ControlPlaneRemoteErrorKind::IdempotencyKeyReused,
        _ => ControlPlaneRemoteErrorKind::Other,
    }
}

async fn read_bounded(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, ControlPlaneClientError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(ControlPlaneClientError::ResponseTooLarge(limit));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > limit {
            return Err(ControlPlaneClientError::ResponseTooLarge(limit));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
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
            classify_remote_error(Some("name-in-use")),
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
            let body = r#"{"error":"conflict mentions remote-error-secret","code":"name-in-use"}"#;
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
