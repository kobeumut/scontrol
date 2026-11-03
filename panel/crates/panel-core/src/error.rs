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
}

pub type CoreResult<T> = std::result::Result<T, CoreError>;
