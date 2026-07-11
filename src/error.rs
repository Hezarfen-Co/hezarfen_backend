use axum::Json;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;
use utoipa::ToSchema;

/// Uniform error body shape (`{ "error": "..." }`) used for OpenAPI docs.
#[derive(Serialize, ToSchema)]
pub struct ErrorResponse {
    /// Human-readable failure reason.
    pub error: String,
}

/// Why a newtype refused to be constructed from raw input. Always maps to `400`.
#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("{0} must not be empty")]
    Empty(&'static str),
    #[error("{field} must be at least {min} characters (got {got})")]
    TooShort {
        field: &'static str,
        min: usize,
        got: usize,
    },
    #[error("{field} must be at most {max} characters (got {got})")]
    TooLong {
        field: &'static str,
        max: usize,
        got: usize,
    },
    #[error("{0} must be ASCII")]
    NotAscii(&'static str),
    #[error("{field}: {reason}")]
    Invalid {
        field: &'static str,
        reason: &'static str,
    },
}

/// The single error type every fallible operation returns.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Validation(#[from] ValidationError),
    #[error("not found")]
    NotFound,
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden: {0}")]
    Forbidden(&'static str),
    #[error("conflict: {0}")]
    Conflict(&'static str),
    #[error("too many requests")]
    TooManyRequests { retry_after_secs: u64 },
    #[error("database error")]
    Db(#[from] surrealdb::Error),
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<argon2::password_hash::Error> for AppError {
    fn from(e: argon2::password_hash::Error) -> Self {
        AppError::Internal(format!("password hashing error: {e}"))
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            // The one arm with an extra header, so it builds its response here.
            AppError::TooManyRequests { retry_after_secs } => {
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    [(header::RETRY_AFTER, retry_after_secs.to_string())],
                    Json(json!({ "error": "too many requests" })),
                )
                    .into_response();
            }
            AppError::Validation(v) => (StatusCode::BAD_REQUEST, v.to_string()),
            AppError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized".to_string()),
            AppError::Forbidden(m) => (StatusCode::FORBIDDEN, m.to_string()),
            AppError::Conflict(m) => (StatusCode::CONFLICT, m.to_string()),
            AppError::Db(e) => {
                tracing::error!("database error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "database error".to_string(),
                )
            }
            AppError::Internal(m) => {
                tracing::error!("internal error: {m}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}
