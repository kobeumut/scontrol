//! Error types for panel-core.

use thiserror::Error;

#[derive(Debug, Clone, Error)]
pub enum CoreError {
    #[error("validation error: {0}")]
    Validation(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("already exists: {0}")]
    AlreadyExists(String),

    #[error("permission denied: {0}")]
    PermissionDenied(String),

    #[error("io error: {0}")]
    Io(String),

    #[error("external error: {0}")]
    External(String),

    /// An operator cancelled the operation (e.g. a running deployment).
    #[error("cancelled: {0}")]
    Cancelled(String),
}

impl CoreError {
    /// The message without the kind prefix that `Display` adds. The agent
    /// protocol sends this, so the receiving side's `Display` adds the
    /// prefix exactly once.
    pub fn message(&self) -> &str {
        match self {
            CoreError::Validation(m)
            | CoreError::NotFound(m)
            | CoreError::AlreadyExists(m)
            | CoreError::PermissionDenied(m)
            | CoreError::Io(m)
            | CoreError::External(m)
            | CoreError::Cancelled(m) => m,
        }
    }
}

pub type CoreResult<T> = std::result::Result<T, CoreError>;
