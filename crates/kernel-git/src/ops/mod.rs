//! Stock jj 0.42 simple operation-store validation for GitBackend repositories.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

mod hash;
mod proto;
mod validate;

pub const OP_STORE_ID_LENGTH: usize = 64;
pub const COMMIT_ID_LENGTH: usize = 20;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum OpObjectKind {
    View = 0,
    Operation = 1,
}

impl TryFrom<u8> for OpObjectKind {
    type Error = ValidationError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::View),
            1 => Ok(Self::Operation),
            _ => Err(ValidationError::new(format!(
                "unknown operation-store object kind: {value}"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum OpReferenceKind {
    Commit = 0,
    View = 1,
    Operation = 2,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct OpObjectReference {
    pub kind: OpReferenceKind,
    pub id: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpValidatedObject {
    pub id: [u8; OP_STORE_ID_LENGTH],
    pub references: Vec<OpObjectReference>,
}

pub fn validate_op(kind: OpObjectKind, bytes: &[u8]) -> Result<OpValidatedObject, ValidationError> {
    match kind {
        OpObjectKind::View => validate::validate_view(bytes),
        OpObjectKind::Operation => validate::validate_operation(bytes),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationError(String);

impl ValidationError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl core::error::Error for ValidationError {}

pub(crate) trait Context<T> {
    fn context(self, message: &'static str) -> Result<T, ValidationError>;
}

impl<T, E: fmt::Display> Context<T> for Result<T, E> {
    fn context(self, message: &'static str) -> Result<T, ValidationError> {
        self.map_err(|error| ValidationError::new(format!("{message}: {error}")))
    }
}
