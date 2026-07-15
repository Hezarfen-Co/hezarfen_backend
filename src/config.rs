use std::env;

use crate::rate_limit::RateLimitConfig;

/// Default requests-per-minute-per-IP for `/auth/login` + `/auth/register`.
pub const DEFAULT_AUTH_RATE_LIMIT: u32 = 10;
/// Default requests-per-minute-per-IP across the whole API.
pub const DEFAULT_API_RATE_LIMIT: u32 = 300;

/// Runtime configuration, sourced from environment variables (see `.env.example`).
#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub db_path: String,
    pub db_ns: String,
    pub db_name: String,
    /// Directory for uploaded note-file blobs (`FILES_PATH`). Kept beside the
    /// database inside the same volume so one mount persists everything.
    pub files_path: String,
    /// Add the `Secure` attribute to the session cookie (HTTPS-only). Off by
    /// default so plain-HTTP local dev keeps working; enable behind TLS.
    pub cookie_secure: bool,
    /// Per-IP request limits (`RATE_LIMIT_AUTH_PER_MINUTE`,
    /// `RATE_LIMIT_API_PER_MINUTE`, `TRUST_PROXY`). `0` disables a tier.
    pub rate_limit: RateLimitConfig,
    /// Startup admin seed (`ADMIN_USERNAME` + `ADMIN_PASSWORD`). When both are
    /// set, an admin account with these credentials is created at boot if the
    /// username doesn't exist yet. Blank values count as unset.
    pub admin_username: Option<String>,
    pub admin_password: Option<String>,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            host: env::var("HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            port: parse_port(env::var("PORT").ok()),
            db_path: env::var("DB_PATH").unwrap_or_else(|_| "./data/hezarfen.db".into()),
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
            admin_username: parse_optional(env::var("ADMIN_USERNAME").ok()),
            admin_password: parse_optional(env::var("ADMIN_PASSWORD").ok()),
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
    use super::{parse_flag, parse_limit, parse_port};

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
