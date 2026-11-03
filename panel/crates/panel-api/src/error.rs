use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use panel_core::error::CoreError;
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error(transparent)]
    Core(#[from] CoreError),
    #[error("database error")]
    Db(#[from] sqlx::Error),
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            ApiError::Core(CoreError::Validation(m)) => (StatusCode::BAD_REQUEST, m.clone()),
            ApiError::Core(CoreError::NotFound(m)) => (StatusCode::NOT_FOUND, m.clone()),
            ApiError::Core(CoreError::AlreadyExists(m)) => (StatusCode::CONFLICT, m.clone()),
            ApiError::Core(CoreError::PermissionDenied(m)) => (StatusCode::FORBIDDEN, m.clone()),
            ApiError::Core(CoreError::Io(m)) | ApiError::Core(CoreError::External(m)) => {
                tracing::error!(error = %m, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
            ApiError::Db(e) => {
                tracing::error!(error = %e, "database error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized".to_string()),
            ApiError::Forbidden => (StatusCode::FORBIDDEN, "forbidden".to_string()),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
            ApiError::Internal(e) => {
                tracing::error!(error = %e, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
