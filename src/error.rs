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
    /// One request asked for a name in two opposite directions at once — a
    /// batch that contradicts itself. Names the value, like
    /// [`ValidationError::Unknown`], because "one of your lists overlaps" would
    /// leave the caller diffing them by hand.
    #[error("{field}: `{value}` is asked for in both directions at once")]
    Contradictory { field: &'static str, value: String },
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
    Db(#[source] sqlx::Error),
    /// The pool refused before the statement ran: the pool timed out waiting
    /// for a connection, was closed, or TLS to the server failed to come up.
    /// Nothing executed, so a retry is safe. Transient, self-healing,
    /// retryable.
    #[error("database unavailable")]
    DbUnavailable,
    /// The request outran [`crate::constant::REQUEST_TIMEOUT_SECS`], or the
    /// connection it rode died mid-flight (Postgres reports that as an I/O
    /// error, not a verdict).
    ///
    /// Deliberately NOT `DbUnavailable`: that one promises the statement was
    /// refused before it executed, so a retry is safe. A request that died on
    /// the wire may still have applied server-side (the commit can race the
    /// connection dropping), so retrying can apply the same write twice.
    /// Same 503, honest message.
    #[error("request timed out")]
    DbTimeout,
    #[error("internal error: {0}")]
    Internal(String),
}


impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        match e {
            // A `fetch_one` that found nothing is the caller's 404, not a 500.
            sqlx::Error::RowNotFound => AppError::NotFound,
            // Pool problems refuse on the acquire path: nothing was queued,
            // let alone executed, so a retry cannot double-apply a write.
            sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Tls(_) => {
                AppError::DbUnavailable
            }
            // A connection that died under a request. The statement may or
            // may not have applied — the honest, retry-unsafe 503.
            sqlx::Error::Io(_) => AppError::DbTimeout,
            // Everything else is a verdict (or a bug) the server returned —
            // refusals included; the helpers in [`crate::database`] read the
            // SQLSTATE off it.
            _ => AppError::Db(e),
        }
    }
}

impl From<argon2::password_hash::Error> for AppError {
    fn from(e: argon2::password_hash::Error) -> Self {
        AppError::Internal(format!("password hashing error: {e}"))
    }
}

impl AppError {
    /// The stable, snake_case name of this refusal's *class* — the value of the
    /// `error.type` attribute on the span and on `errors_total`. It is a
    /// dashboard's grouping key, so it never carries anything from the request:
    /// no field name, no id, no message. Rename an arm freely; never rename one
    /// of these strings.
    fn class(&self) -> &'static str {
        match self {
            AppError::Validation(_) => "validation",
            AppError::NotFound => "not_found",
            AppError::Unauthorized => "unauthorized",
            AppError::Forbidden(_) => "forbidden",
            AppError::ModuleDisabled(_) => "module_disabled",
            AppError::Conflict(_) | AppError::ConflictOwned(_) => "conflict",
            AppError::ConflictCoded { .. } => "conflict_coded",
            AppError::PayloadTooLarge(_) => "payload_too_large",
            AppError::TooManyRequests { .. } => "too_many_requests",
            AppError::Db(_) => "db",
            AppError::DbUnavailable => "db_unavailable",
            AppError::DbTimeout => "db_timeout",
            AppError::Internal(_) => "internal",
        }
    }

    /// What a `5xx` log line says beyond its class — the underlying error, which
    /// the arm's own `Display` hides (`AppError::Db` renders as "database
    /// error", keeping the SDK's text out of the response body). `None` for
    /// everything the caller caused: a `4xx` is not an incident and its message
    /// is derived from what was sent.
    fn detail(&self) -> Option<String> {
        match self {
            AppError::Db(e) => Some(e.to_string()),
            AppError::Internal(m) => Some(m.clone()),
            AppError::DbTimeout => Some("the write may or may not have applied".to_owned()),
            _ => None,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let class = self.class();
        let detail = self.detail();
        let response = self.render();
        let status = response.status();

        // The request span carries the class; the route is already on the span
        // and on every metric `measure` records, so the counter needs only the
        // class and the status.
        tracing::Span::current().record("error.type", class);
        crate::telemetry::Metrics::global().errors_total.add(
            1,
            &[
                opentelemetry::KeyValue::new("error.type", class),
                opentelemetry::KeyValue::new(
                    "http.response.status_code",
                    i64::from(status.as_u16()),
                ),
            ],
        );
        let detail = detail.as_deref().unwrap_or("");
        match status.as_u16() {
            // Expected under load or during a reconnect: worth a line, not a page.
            429 | 503 if class != "db_timeout" => {
                tracing::warn!(error.type = class, "refused the request");
            }
            500..=599 => tracing::error!(error.type = class, "{detail}"),
            // The caller's fault, and the loudest thing in the log if it were
            // any higher: a bad request is not an incident.
            _ => tracing::debug!(error.type = class, "refused the request"),
        }
        response
    }
}

impl AppError {
    /// The wire response, byte for byte what each arm has always answered.
    fn render(self) -> Response {
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
            AppError::Db(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "database error".to_string(),
            ),
            AppError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal server error".to_string(),
            ),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;


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
    fn sqlx_errors_split_into_honest_classes() {
        use sqlx::Error;
        // Pool problems refuse on the acquire path: nothing executed, so a
        // retry is safe.
        assert!(matches!(
            AppError::from(Error::PoolTimedOut),
            AppError::DbUnavailable
        ));
        assert!(matches!(
            AppError::from(Error::PoolClosed),
            AppError::DbUnavailable
        ));
        assert!(matches!(
            AppError::from(Error::Tls("tls".into())),
            AppError::DbUnavailable
        ));
        // A connection that died under a request: the honest 503 — the write
        // may or may not have applied, so a retry is NOT advertised as safe.
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "gone");
        assert!(matches!(AppError::from(Error::Io(io)), AppError::DbTimeout));
        // A query that found no row is the caller's 404, never a 500.
        assert!(matches!(AppError::from(Error::RowNotFound), AppError::NotFound));
    }

}
