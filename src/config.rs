use std::env;

use crate::constant::{
    AI_DEFAULT_REQUEST_TIMEOUT_SECS, AI_MAX_REQUEST_TIMEOUT_SECS, DEFAULT_API_RATE_LIMIT,
    DEFAULT_AUTH_RATE_LIMIT, DEFAULT_CHATBOT_RATE_LIMIT,
};
use crate::rate_limit::RateLimitConfig;

/// Runtime configuration, sourced from environment variables (see `.env.example`).
#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,
    /// SurrealDB server endpoint (`DB_URL`), e.g. `ws://127.0.0.1:8000`.
    pub db_url: String,
    /// Root credentials for the SurrealDB server (`DB_USER` / `DB_PASS`).
    pub db_user: String,
    pub db_pass: String,
    pub db_ns: String,
    pub db_name: String,
    /// Directory for uploaded note-file blobs (`FILES_PATH`).
    pub files_path: String,
    /// Add the `Secure` attribute to the session cookie (HTTPS-only). Off by
    /// default so plain-HTTP local dev keeps working; enable behind TLS.
    pub cookie_secure: bool,
    /// Per-IP request limits (`RATE_LIMIT_AUTH_PER_MINUTE`,
    /// `RATE_LIMIT_API_PER_MINUTE`, `TRUST_PROXY`). `0` disables a tier.
    pub rate_limit: RateLimitConfig,
    /// Chatbot messages one user may send per minute
    /// (`RATE_LIMIT_CHATBOT_PER_MINUTE`). `0` disables the tier. Kept out of
    /// [`RateLimitConfig`], which is the per-IP middleware bundle: this tier is
    /// keyed by user and is enforced inside the handler.
    pub chatbot_per_minute: u32,
    /// Startup admin seed (`ADMIN_USERNAME` + `ADMIN_PASSWORD`). When both are
    /// set, an admin account with these credentials is created at boot if the
    /// username doesn't exist yet. Blank values count as unset.
    pub admin_username: Option<String>,
    pub admin_password: Option<String>,
    /// AI bridge listen address (`AI_QUIC_ADDR`, e.g. `0.0.0.0:8090`). Unset
    /// leaves the bridge off entirely: the API runs exactly as before and any
    /// AI-backed feature reports that no service is connected.
    pub ai_quic_addr: Option<String>,
    /// Shared secret every AI service must present (`AI_SHARED_TOKEN`).
    /// Required whenever `ai_quic_addr` is set — the bridge refuses to start
    /// without one rather than listening unauthenticated.
    pub ai_shared_token: Option<String>,
    /// PEM certificate/key for the bridge listener (`AI_TLS_CERT` /
    /// `AI_TLS_KEY`). Both unset means a self-signed pair is generated at boot
    /// and its fingerprint logged for the services to pin.
    pub ai_tls_cert: Option<String>,
    pub ai_tls_key: Option<String>,
    /// Default per-request deadline for AI calls (`AI_REQUEST_TIMEOUT_SECS`).
    pub ai_request_timeout_secs: u64,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            host: env::var("HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            port: parse_port(env::var("PORT").ok()),
            db_url: env::var("DB_URL").unwrap_or_else(|_| "ws://127.0.0.1:8000".into()),
            db_user: env::var("DB_USER").unwrap_or_else(|_| "root".into()),
            db_pass: env::var("DB_PASS").unwrap_or_else(|_| "root".into()),
            db_ns: env::var("DB_NAMESPACE").unwrap_or_else(|_| "hezarfen".into()),
            db_name: env::var("DB_DATABASE").unwrap_or_else(|_| "hezarfen".into()),
            files_path: env::var("FILES_PATH").unwrap_or_else(|_| "./data/files".into()),
            cookie_secure: parse_flag(env::var("COOKIE_SECURE").ok()),
            rate_limit: RateLimitConfig {
                auth_per_minute: parse_limit(
                    env::var("RATE_LIMIT_AUTH_PER_MINUTE").ok(),
                    DEFAULT_AUTH_RATE_LIMIT,
                ),
                api_per_minute: parse_limit(
                    env::var("RATE_LIMIT_API_PER_MINUTE").ok(),
                    DEFAULT_API_RATE_LIMIT,
                ),
                trust_proxy: parse_flag(env::var("TRUST_PROXY").ok()),
            },
            chatbot_per_minute: parse_limit(
                env::var("RATE_LIMIT_CHATBOT_PER_MINUTE").ok(),
                DEFAULT_CHATBOT_RATE_LIMIT,
            ),
            admin_username: parse_optional(env::var("ADMIN_USERNAME").ok()),
            admin_password: parse_optional(env::var("ADMIN_PASSWORD").ok()),
            ai_quic_addr: parse_optional(env::var("AI_QUIC_ADDR").ok()),
            ai_shared_token: parse_optional(env::var("AI_SHARED_TOKEN").ok()),
            ai_tls_cert: parse_optional(env::var("AI_TLS_CERT").ok()),
            ai_tls_key: parse_optional(env::var("AI_TLS_KEY").ok()),
            ai_request_timeout_secs: parse_timeout(
                env::var("AI_REQUEST_TIMEOUT_SECS").ok(),
                AI_DEFAULT_REQUEST_TIMEOUT_SECS,
            ),
        }
    }
}

/// Treat an unset or blank variable as absent.
fn parse_optional(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

/// Parse a per-minute limit, falling back to `default` when unset or
/// unparseable. An explicit `0` is honoured: it turns that tier off.
fn parse_limit(value: Option<String>, default: u32) -> u32 {
    value.and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

/// Parse the AI request deadline in seconds. Unlike a rate limit, `0` is *not*
/// honoured — a zero deadline would fail every AI request instantly, which is
/// never what an operator means — so it falls back with the rest of the
/// garbage.
///
/// The ceiling is the load-bearing part, and it is enforced here because here
/// is the only place the number enters the process. A deadline above
/// [`AI_MAX_REQUEST_TIMEOUT_SECS`] lets one dispatch outlive
/// `CHATBOT_CLAIM_RECLAIM_SECS`, and the chat claim queue then hands the same
/// turn to a second worker while the first is still answering it — two
/// inferences for one reply. Clamping at the boundary is what makes that
/// unreachable *by configuration*, instead of true only for the default.
fn parse_timeout(value: Option<String>, default: u64) -> u64 {
    let asked = value
        .and_then(|v| v.trim().parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default);
    if asked > AI_MAX_REQUEST_TIMEOUT_SECS {
        // Loud: the operator asked for something they are not getting, and the
        // reason (a queue horizon) is not one they could guess from the name.
        tracing::warn!(
            "AI_REQUEST_TIMEOUT_SECS={asked} is above the {AI_MAX_REQUEST_TIMEOUT_SECS}s ceiling \
             the chat claim queue allows (a longer inference would be reclaimed and re-dispatched \
             mid-flight) — using {AI_MAX_REQUEST_TIMEOUT_SECS}s"
        );
        return AI_MAX_REQUEST_TIMEOUT_SECS;
    }
    asked
}

/// Parse the `PORT` value, falling back to 8080 when unset or unparseable.
fn parse_port(value: Option<String>) -> u16 {
    value.and_then(|p| p.parse().ok()).unwrap_or(8080)
}

/// Parse a boolean toggle: `1` / `true` / `yes` / `on` (any case, trimmed) are
/// true. Anything else — including unset — is false.
fn parse_flag(value: Option<String>) -> bool {
    value.is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{parse_flag, parse_limit, parse_port, parse_timeout};
    use crate::constant::{
        AI_DEFAULT_REQUEST_TIMEOUT_SECS, AI_MAX_REQUEST_TIMEOUT_SECS, CHATBOT_CLAIM_RECLAIM_SECS,
    };

    #[tokio::test]
    async fn timeout_falls_back_on_absent_garbage_and_zero() {
        assert_eq!(parse_timeout(None, 30), 30);
        assert_eq!(parse_timeout(Some("nope".into()), 30), 30);
        // A zero deadline would time out every AI call before it started.
        assert_eq!(parse_timeout(Some("0".into()), 30), 30);
        // Whitespace is trimmed; the ceiling has its own test below.
        assert_eq!(parse_timeout(Some(" 45 ".into()), 30), 45);
    }

    #[tokio::test]
    async fn a_configured_timeout_can_never_outlive_the_chat_claim_horizon() {
        // The one double-dispatch an operator could still reach: nothing used
        // to stop `AI_REQUEST_TIMEOUT_SECS=120`, and a dispatch outliving
        // `CHATBOT_CLAIM_RECLAIM_SECS` has its turn reclaimed and re-sent while
        // the first inference is still running — the model answers one turn
        // twice. The relationship, not a literal, is what is asserted: raising
        // one of the two numbers without the other cannot pass this.
        for asked in ["91", "120", "600", "18446744073709551615"] {
            let got = parse_timeout(Some(asked.into()), AI_DEFAULT_REQUEST_TIMEOUT_SECS);
            assert!(
                (got as i64) < CHATBOT_CLAIM_RECLAIM_SECS,
                "AI_REQUEST_TIMEOUT_SECS={asked} was honoured as {got}s, at or past the \
                 {CHATBOT_CLAIM_RECLAIM_SECS}s reclaim horizon"
            );
            assert_eq!(got, AI_MAX_REQUEST_TIMEOUT_SECS, "clamped to the ceiling");
        }
        // Everything under the ceiling — the default included — is honoured
        // verbatim: this is a ceiling, not a fixed deadline.
        assert_eq!(parse_timeout(Some("45".into()), 30), 45);
        assert_eq!(
            parse_timeout(None, AI_DEFAULT_REQUEST_TIMEOUT_SECS),
            AI_DEFAULT_REQUEST_TIMEOUT_SECS
        );
    }

    #[tokio::test]
    async fn limit_defaults_when_absent_or_garbage() {
        assert_eq!(parse_limit(None, 10), 10);
        assert_eq!(parse_limit(Some("not-a-number".into()), 10), 10);
        assert_eq!(parse_limit(Some("-5".into()), 10), 10);
    }

    #[tokio::test]
    async fn limit_parses_values_and_honours_explicit_zero() {
        assert_eq!(parse_limit(Some("25".into()), 10), 25);
        assert_eq!(parse_limit(Some(" 25 ".into()), 10), 25);
        assert_eq!(parse_limit(Some("0".into()), 10), 0);
    }

    #[tokio::test]
    async fn defaults_when_absent() {
        assert_eq!(parse_port(None), 8080);
    }

    #[tokio::test]
    async fn flag_defaults_off_and_accepts_truthy_spellings() {
        assert!(!parse_flag(None));
        for v in ["1", "true", "TRUE", " yes ", "On"] {
            assert!(parse_flag(Some(v.into())), "{v:?} should be true");
        }
        for v in ["0", "false", "nope", ""] {
            assert!(!parse_flag(Some(v.into())), "{v:?} should be false");
        }
    }

    #[tokio::test]
    async fn parses_valid_port() {
        assert_eq!(parse_port(Some("3000".into())), 3000);
    }

    #[tokio::test]
    async fn falls_back_on_garbage() {
        assert_eq!(parse_port(Some("not-a-port".into())), 8080);
        assert_eq!(parse_port(Some(String::new())), 8080);
    }
}
