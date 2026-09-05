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
    /// Machine-readable refusal code, present only where a route documents its
    /// vocabulary (today: the two manual class-attach routes, which answer the
    /// codes a blueprint pump reports as a skip). Absent everywhere else, so a
    /// client branches on it where it is published and reads `error` otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
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
    /// A closed value set refused a name it does not contain — and says which
    /// name, which [`ValidationError::Invalid`] cannot: its `reason` is
    /// `&'static str`, so the offending value could only be described, never
    /// quoted.
    #[error("{field}: `{value}` is not a known {field}")]
    Unknown { field: &'static str, value: String },
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
    /// The school has not bought this module. A dedicated arm because the body
    /// carries a second key — the module's name — so a client can tell "your
    /// school does not have this" from every other `403` without parsing prose.
    #[error("module disabled: {0}")]
    ModuleDisabled(crate::module::Module),
    #[error("conflict: {0}")]
    Conflict(&'static str),
    /// A 409 whose message is only known at runtime — e.g. the specific
    /// students a homework's narrowed `assigned` list would orphan. Owned
    /// string, like [`AppError::PayloadTooLarge`]; static conflicts use
    /// [`AppError::Conflict`].
    #[error("conflict: {0}")]
    ConflictOwned(String),
    /// A 409 that also carries a **machine code**, so a client branches on the
    /// cause instead of parsing the sentence (which it cannot translate). The
    /// prose is unchanged from what the same refusal always said — `code` is
    /// additive, and only a route that publishes its vocabulary uses this.
    #[error("conflict: {message}")]
    ConflictCoded { code: &'static str, message: String },
    /// A request body (file upload) over the allowed size. Owned string: the
    /// school-configured limit is only known at runtime.
    #[error("payload too large: {0}")]
    PayloadTooLarge(String),
    #[error("too many requests")]
    TooManyRequests { retry_after_secs: u64 },
    #[error("database error")]
    Db(#[source] surrealdb::Error),
    /// The database WebSocket dropped and is mid-reconnect. Covers a query in
    /// flight when the socket died (the SDK fails it as a connection error)
    /// and a query racing the SDK's replay of session state (signin,
    /// namespace), which the server refuses before execution. Transient,
    /// self-healing, retryable.
    #[error("database unavailable")]
    DbUnavailable,
    /// The request outran [`crate::constant::REQUEST_TIMEOUT_SECS`], which in
    /// practice means it reached the database in the window between the socket
    /// dying and the keepalive noticing, and got parked in the SDK's queue.
    ///
    /// Deliberately NOT `DbUnavailable`: that one promises the query was
    /// refused before execution, so a retry is safe. A parked query is still
    /// queued and *does* execute once the socket heals (verified: pings
    /// abandoned during an outage all fire on reconnect), so retrying can
    /// apply the same write twice. Same 503, honest message.
    #[error("request timed out")]
    DbTimeout,
    #[error("internal error: {0}")]
    Internal(String),
}

/// Is this database error a query refused during the SDK's post-reconnect
/// session replay? Two signatures, matched narrowly: "Specify a namespace"
/// (signin replayed, namespace not yet) and "Anonymous access not allowed"
/// (signin not yet). The backend signs in as root, so neither can be a real
/// authorization verdict — but a bare "Not enough permissions" could be, so
/// that alone must never match.
pub(crate) fn is_session_replay_error(message: &str) -> bool {
    message.contains("Specify a namespace") || message.contains("Anonymous access not allowed")
}

impl From<surrealdb::Error> for AppError {
    fn from(e: surrealdb::Error) -> Self {
        // `is_connection()`: the SDK's own verdict that the socket itself
        // failed ("Connection reset", "WebSocket error: ..."). Structurally
        // distinct from query/permission errors, so it can't misfile one.
        if e.is_connection() || is_session_replay_error(&e.to_string()) {
            AppError::DbUnavailable
        } else {
            AppError::Db(e)
        }
    }
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
            // Also carries a header: retry in a second, the reconnect is quick.
            AppError::DbUnavailable => {
                tracing::warn!("database reconnecting — refusing the query with 503");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [(header::RETRY_AFTER, "1")],
                    Json(json!({ "error": "database reconnecting — retry shortly" })),
                )
                    .into_response();
            }
            // Same 503, but no Retry-After: the query may still be queued and
            // land later, so a blind retry is not advertised as safe.
            AppError::DbTimeout => {
                tracing::error!("request timed out waiting on the database");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({ "error": "request timed out — the write may or may not have applied" })),
                )
                    .into_response();
            }
            // The one conflict with a second key, so it builds its body here.
            AppError::ConflictCoded { code, message } => {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({ "error": message, "code": code })),
                )
                    .into_response();
            }
            // Also carries a second key: which module was refused.
            AppError::ModuleDisabled(module) => {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({ "error": "module disabled", "module": module.as_str() })),
                )
                    .into_response();
            }
            AppError::Validation(v) => (StatusCode::BAD_REQUEST, v.to_string()),
            AppError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized".to_string()),
            AppError::Forbidden(m) => (StatusCode::FORBIDDEN, m.to_string()),
            AppError::Conflict(m) => (StatusCode::CONFLICT, m.to_string()),
            AppError::ConflictOwned(m) => (StatusCode::CONFLICT, m.clone()),
            AppError::PayloadTooLarge(m) => (StatusCode::PAYLOAD_TOO_LARGE, m.clone()),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_signatures_classify_as_unavailable() {
        // The two exact refusals a query racing the post-reconnect session
        // replay gets, as they appear in the wire error.
        assert!(is_session_replay_error("Specify a namespace to use"));
        assert!(is_session_replay_error(
            "Anonymous access not allowed: Not enough permissions to perform this action"
        ));
    }

    #[test]
    fn connection_errors_classify_as_unavailable() {
        // What the SDK fails an in-flight query with when the socket dies —
        // same wire error `clear_pending_requests` produces.
        let e = surrealdb::Error::connection(
            "Connection reset".to_string(),
            surrealdb::types::ConnectionError::ConnectionFailed,
        );
        assert!(matches!(AppError::from(e), AppError::DbUnavailable));
    }

    /// The refusal a gated nest answers with. Both keys, and the `403` — a
    /// client switches on `module`, so neither may drift.
    #[tokio::test]
    async fn a_disabled_module_is_a_403_naming_itself() {
        use crate::module::Module;
        let response = AppError::ModuleDisabled(Module::CourseNotes).into_response();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"], "module disabled");
        assert_eq!(body["module"], "course_notes");
    }

    #[test]
    fn real_errors_stay_db_errors() {
        // A genuine authorization verdict shares the suffix but must not match.
        assert!(!is_session_replay_error(
            "Not enough permissions to perform this action"
        ));
        assert!(!is_session_replay_error("Parse error: unexpected token"));
        assert!(!is_session_replay_error(
            "Database index `user_username` already contains 'admin'"
        ));
    }
}
