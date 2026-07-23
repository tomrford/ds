use std::fmt;

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Deserialize)]
pub(crate) struct ErrorResponse {
    error: String,
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
