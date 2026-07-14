use std::fmt;

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

impl std::error::Error for ValidationError {}

pub(crate) trait Context<T> {
    fn context(self, message: &'static str) -> Result<T, ValidationError>;
}

impl<T, E: fmt::Display> Context<T> for Result<T, E> {
    fn context(self, message: &'static str) -> Result<T, ValidationError> {
        self.map_err(|error| ValidationError::new(format!("{message}: {error}")))
    }
}
