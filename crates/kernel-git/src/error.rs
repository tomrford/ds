use core::fmt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValidationError {
    UnknownObjectKind(u8),
    Commit(CommitError),
    Tree(TreeError),
    Hash(HashError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommitError {
    MissingHeader(&'static str),
    UnexpectedHeader {
        expected: &'static str,
        offset: usize,
    },
    InvalidHeader {
        offset: usize,
    },
    UnexpectedContinuation {
        offset: usize,
    },
    MissingHeaderTerminator,
    InvalidObjectId {
        header: &'static str,
        offset: usize,
    },
    InvalidSignature {
        header: &'static str,
        offset: usize,
    },
    InvalidJjTrees,
    InvalidConflictLabels,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TreeError {
    MissingMode { offset: usize },
    InvalidMode { offset: usize },
    MissingNameTerminator { offset: usize },
    InvalidName { offset: usize },
    TruncatedObjectId { offset: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HashError {
    CollisionDetected,
}

impl From<CommitError> for ValidationError {
    fn from(error: CommitError) -> Self {
        Self::Commit(error)
    }
}

impl From<TreeError> for ValidationError {
    fn from(error: TreeError) -> Self {
        Self::Tree(error)
    }
}

impl From<HashError> for ValidationError {
    fn from(error: HashError) -> Self {
        Self::Hash(error)
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownObjectKind(value) => write!(formatter, "unknown object kind: {value}"),
            Self::Commit(error) => error.fmt(formatter),
            Self::Tree(error) => error.fmt(formatter),
            Self::Hash(error) => error.fmt(formatter),
        }
    }
}

impl fmt::Display for CommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHeader(header) => write!(formatter, "missing {header} header"),
            Self::UnexpectedHeader { expected, offset } => {
                write!(formatter, "expected {expected} header at byte {offset}")
            }
            Self::InvalidHeader { offset } => {
                write!(formatter, "invalid commit header at byte {offset}")
            }
            Self::UnexpectedContinuation { offset } => {
                write!(formatter, "unexpected continuation line at byte {offset}")
            }
            Self::MissingHeaderTerminator => {
                formatter.write_str("commit headers have no blank-line terminator")
            }
            Self::InvalidObjectId { header, offset } => {
                write!(formatter, "invalid {header} object id at byte {offset}")
            }
            Self::InvalidSignature { header, offset } => {
                write!(formatter, "invalid {header} signature at byte {offset}")
            }
            Self::InvalidJjTrees => formatter.write_str("invalid jj:trees header"),
            Self::InvalidConflictLabels => formatter.write_str("invalid jj:conflict-labels header"),
        }
    }
}

impl fmt::Display for TreeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingMode { offset } => {
                write!(
                    formatter,
                    "tree entry at byte {offset} has no mode terminator"
                )
            }
            Self::InvalidMode { offset } => {
                write!(formatter, "tree entry at byte {offset} has an invalid mode")
            }
            Self::MissingNameTerminator { offset } => {
                write!(
                    formatter,
                    "tree entry at byte {offset} has no name terminator"
                )
            }
            Self::InvalidName { offset } => {
                write!(formatter, "tree entry at byte {offset} has an invalid name")
            }
            Self::TruncatedObjectId { offset } => {
                write!(
                    formatter,
                    "tree entry at byte {offset} has a truncated object id"
                )
            }
        }
    }
}

impl fmt::Display for HashError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CollisionDetected => formatter.write_str("SHA-1 collision detected"),
        }
    }
}

impl core::error::Error for ValidationError {}
impl core::error::Error for CommitError {}
impl core::error::Error for TreeError {}
impl core::error::Error for HashError {}
