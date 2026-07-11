use crate::database::Database;
use crate::rate_limit::RateLimitConfig;

/// Shared application state handed to every handler.
#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    /// Whether the session cookie carries the `Secure` attribute
    /// (from [`crate::config::Config::cookie_secure`]).
    pub cookie_secure: bool,
    /// Per-IP request limits (from [`crate::config::Config::rate_limit`]).
    /// Read once by [`crate::build_router`] when the limiters are built.
    pub rate_limit: RateLimitConfig,
}
