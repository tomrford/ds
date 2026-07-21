use std::fmt;

use serde::Deserialize;
use thiserror::Error;

use crate::sync_engine::TransportError;

const MAX_ERROR_RESPONSE_BYTES: usize = 16 * 1024;

#[derive(Debug, Deserialize)]
pub(crate) struct ErrorResponse {
    pub(crate) error: String,
    pub(crate) code: Option<String>,
}

impl fmt::Display for ErrorResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(code) = &self.code {
            write!(formatter, "{code}: {}", self.error)
        } else {
            formatter.write_str(&self.error)
        }
    }
}

pub(crate) async fn send(
    request: reqwest::RequestBuilder,
    authorization: &reqwest::header::HeaderValue,
    machine_id: &str,
    incarnation: &str,
) -> Result<reqwest::Response, TransportError> {
    let response = request
        .header(reqwest::header::AUTHORIZATION, authorization)
        .header("x-devspace-machine-id", machine_id)
        .header("x-devspace-incarnation", incarnation)
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
            || "cloud request failed without an error body".to_owned(),
            |body| body.to_string(),
        );
    Err(format!("cloud request failed with status {status}: {message}").into())
}

pub(crate) async fn read_bounded(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, BoundedResponseError> {
    if let Some(length) = response.content_length()
        && length > limit as u64
    {
        return Err(BoundedResponseError::DeclaredTooLarge { length, limit });
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > limit {
            return Err(BoundedResponseError::TooLarge { limit });
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[derive(Debug, Error)]
pub(crate) enum BoundedResponseError {
    #[error("cloud response declares {length} bytes, exceeding the {limit}-byte limit")]
    DeclaredTooLarge { length: u64, limit: usize },
    #[error("cloud response exceeds the {limit}-byte limit")]
    TooLarge { limit: usize },
    #[error(transparent)]
    Request(#[from] reqwest::Error),
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::thread;

    use reqwest::header::HeaderValue;

    use super::*;

    #[tokio::test]
    async fn retired_repository_error_is_coded_and_comprehensible() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let _ = connection.read(&mut request).unwrap();
            let body = r#"{"error":"repository was deleted","code":"repository-retired"}"#;
            write!(
                connection,
                "HTTP/1.1 409 Conflict\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        let error = send(
            reqwest::Client::new().get(format!("http://{address}/heads")),
            &HeaderValue::from_static("Bearer test-secret"),
            &"12".repeat(16),
            &"34".repeat(16),
        )
        .await
        .unwrap_err();
        server.join().unwrap();
        let message = error.to_string();
        assert!(message.contains("repository-retired: repository was deleted"));
        assert!(!message.contains(r#"{"error""#));
    }
}

pub fn encode_lower_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

pub fn decode_lower_hex<const N: usize>(value: &str) -> Result<[u8; N], LowerHexError> {
    if value.len() != N * 2 {
        return Err(LowerHexError::InvalidLength {
            expected: N * 2,
            actual: value.len(),
        });
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(LowerHexError::InvalidDigit);
    }
    let mut output = [0; N];
    for (index, byte) in output.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .expect("validated lowercase hexadecimal pair");
    }
    Ok(output)
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum LowerHexError {
    #[error("expected {expected} lowercase hex characters, got {actual}")]
    InvalidLength { expected: usize, actual: usize },
    #[error("value is not lowercase hexadecimal")]
    InvalidDigit,
}
