//! Rate limiting, hand-rolled on a fixed 60-second window — no extra crates,
//! no background tasks. The counter is keyed generically: by client IP for the
//! middleware tiers below, by user id for the per-user chatbot tier
//! ([`UserRateLimiter`]), which cannot be middleware because the caller is only
//! known once `CurrentUser` has run.
//!
//! Two IP tiers are wired in: a strict one on the credential endpoints
//! (`/auth/login`, `/auth/register`) in [`crate::web::auth::routes`] to blunt
//! brute-force and enumeration attempts, and a generous catch-all over the
//! whole API in [`crate::build_router`]. Those two, and the per-user chat tier,
//! are all configured per-minute via environment variables (see
//! [`crate::config::Config`]); a limit of `0` switches that tier off entirely,
//! which is also what the test suites use to stay unaffected.
//!
//! Requests are keyed by client IP. By default that is the peer address of the
//! TCP connection ([`ConnectInfo`]). Behind a reverse proxy every connection
//! shows the proxy's address, so set `TRUST_PROXY=true` to key on the
//! **rightmost** `X-Forwarded-For` entry instead — that hop is appended by the
//! nearest proxy and is the only one the client cannot forge. Never enable it
//! when clients can reach the server directly: the header is then entirely
//! attacker-controlled and the limiter is trivially bypassed.
//!
//! Fixed windows admit up to a 2× burst straddling a window boundary. That is
//! an accepted trade-off for an implementation simple enough to read in one
//! sitting; argon2 keeps each allowed login attempt expensive anyway.

use std::collections::HashMap;
use std::hash::Hash;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{ConnectInfo, Request};
use axum::middleware::Next;
use axum::response::Response;
// tokio's `Instant` wraps `std::time::Instant` in production but obeys
// `tokio::time::pause`/`advance` under `start_paused` tests, which makes the
// window arithmetic below testable without sleeping.
use tokio::time::Instant;

use crate::error::AppError;

/// Once the bucket map holds this many distinct client IPs, expired entries are
/// swept out on the next check. Keeps memory bounded without a reaper task.
const PURGE_AT: usize = 10_000;

/// Per-IP rate-limit knobs, sourced from the environment (see `.env.example`) and
/// carried in [`crate::state::AppState`]. A `0` limit disables that tier.
#[derive(Clone, Debug)]
pub struct RateLimitConfig {
    /// Requests per minute per IP for `/auth/login` + `/auth/register`.
    pub auth_per_minute: u32,
    /// Requests per minute per IP for the whole API.
    pub api_per_minute: u32,
    /// Key clients by the rightmost `X-Forwarded-For` entry instead of the
    /// socket peer address. Only safe behind a proxy that appends it.
    pub trust_proxy: bool,
}

impl RateLimitConfig {
    /// Both tiers off. What the test helpers use so unrelated suites never
    /// trip a limit; dedicated rate-limit tests opt into tight configs.
    pub const fn unlimited() -> Self {
        Self {
            auth_per_minute: 0,
            api_per_minute: 0,
            trust_proxy: false,
        }
    }
}

/// A fixed-window counter per caller `K`: `max` requests per `window`, shared
/// across clones (clones see the same buckets, so one limiter can be captured
/// by a middleware closure and cloned per request for free).
#[derive(Clone)]
pub struct RateLimiter<K = IpAddr> {
    max: u32,
    window: Duration,
    /// Only meaningful for the IP tiers; the keyed tiers never look at it.
    trust_proxy: bool,
    buckets: Arc<Mutex<HashMap<K, Bucket>>>,
}

struct Bucket {
    window_start: Instant,
    count: u32,
}

/// The per-user tier, keyed by user record key instead of client IP. Lives in
/// [`crate::state::AppState`] and is called from inside a handler, after
/// `CurrentUser` has identified the caller.
pub type UserRateLimiter = RateLimiter<String>;

impl Default for UserRateLimiter {
    /// The chatbot tier at its shipped default, for tests and any caller that
    /// has no [`crate::config::Config`] to hand.
    fn default() -> Self {
        Self::per_user_minute(crate::config::DEFAULT_CHATBOT_RATE_LIMIT)
    }
}

impl UserRateLimiter {
    /// The chatbot tier: `max` messages per user per minute, from
    /// `RATE_LIMIT_CHATBOT_PER_MINUTE`. `max == 0` disables it, like the IP tiers.
    pub fn per_user_minute(max: u32) -> Self {
        Self::new(max, false)
    }

    /// Count one message from `user_id`, answering `429` with a `Retry-After`
    /// once the user is over the cap. The handler-side twin of [`RateLimiter::enforce`].
    pub fn enforce_user(&self, user_id: &str) -> Result<(), AppError> {
        self.check(user_id.to_string()).map_err(|retry_after_secs| {
            tracing::warn!(%user_id, retry_after_secs, "per-user rate limit exceeded");
            AppError::TooManyRequests { retry_after_secs }
        })
    }
}

impl RateLimiter<IpAddr> {
    /// A limiter allowing `max` requests per fixed one-minute window. `max == 0`
    /// means disabled: every check passes and nothing is recorded.
    pub fn per_minute(max: u32, trust_proxy: bool) -> Self {
        Self::new(max, trust_proxy)
    }

    /// Axum middleware entry point: identify the caller, count the request,
    /// and either pass it along or answer `429` with a `Retry-After`.
    pub async fn enforce(&self, req: Request, next: Next) -> Result<Response, AppError> {
        let ip = client_ip(&req, self.trust_proxy);
        match self.check(ip) {
            Ok(()) => Ok(next.run(req).await),
            Err(retry_after_secs) => {
                tracing::warn!(%ip, retry_after_secs, "rate limit exceeded");
                Err(AppError::TooManyRequests { retry_after_secs })
            }
        }
    }
}

impl<K: Eq + Hash> RateLimiter<K> {
    /// A limiter allowing `max` requests per fixed one-minute window.
    fn new(max: u32, trust_proxy: bool) -> Self {
        Self {
            max,
            window: Duration::from_secs(60),
            trust_proxy,
            buckets: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Count one request from `key`. `Err` carries the whole seconds (rounded
    /// up, at least 1) until the window resets — the `Retry-After` value.
    fn check(&self, key: K) -> Result<(), u64> {
        if self.max == 0 {
            return Ok(());
        }

        let now = Instant::now();
        let mut buckets = self.buckets.lock().expect("rate limiter mutex poisoned");

        if buckets.len() >= PURGE_AT {
            buckets.retain(|_, b| now.duration_since(b.window_start) < self.window);
        }

        let bucket = buckets.entry(key).or_insert(Bucket {
            window_start: now,
            count: 0,
        });
        if now.duration_since(bucket.window_start) >= self.window {
            bucket.window_start = now;
            bucket.count = 0;
        }

        if bucket.count < self.max {
            bucket.count += 1;
            Ok(())
        } else {
            let remaining = self.window - now.duration_since(bucket.window_start);
            Err((remaining.as_secs_f64().ceil() as u64).max(1))
        }
    }
}

/// Resolve the client IP a request is billed against.
///
/// With `trust_proxy`, prefer the rightmost `X-Forwarded-For` entry — appended
/// by the nearest (trusted) proxy, unlike the left entries which arrive
/// attacker-controlled. Otherwise (or when the header is missing/unparseable)
/// fall back to the connection's peer address. `oneshot`-driven routers carry
/// no [`ConnectInfo`]; those requests share the unspecified-address bucket.
fn client_ip(req: &Request, trust_proxy: bool) -> IpAddr {
    if trust_proxy
        && let Some(ip) = req
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .and_then(|entry| entry.trim().parse().ok())
    {
        return ip;
    }

    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn ip(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, last))
    }

    /// A request with an optional `X-Forwarded-For` header and an optional
    /// peer address, mirroring what a real connection / a proxy would produce.
    fn req(xff: Option<&str>, peer: Option<&str>) -> Request {
        let mut builder = axum::http::Request::builder().uri("/");
        if let Some(v) = xff {
            builder = builder.header("x-forwarded-for", v);
        }
        let mut req = builder.body(Body::empty()).unwrap();
        if let Some(addr) = peer {
            let addr: SocketAddr = addr.parse().unwrap();
            req.extensions_mut().insert(ConnectInfo(addr));
        }
        req
    }

    #[tokio::test]
    async fn client_ip_takes_rightmost_forwarded_hop_when_proxy_is_trusted() {
        let request = req(Some("1.1.1.1, 2.2.2.2"), Some("10.9.8.7:443"));
        assert_eq!(
            client_ip(&request, true),
            "2.2.2.2".parse::<IpAddr>().unwrap()
        );

        let single = req(Some("2001:db8::7"), Some("10.9.8.7:443"));
        assert_eq!(
            client_ip(&single, true),
            "2001:db8::7".parse::<IpAddr>().unwrap()
        );
    }

    #[tokio::test]
    async fn client_ip_ignores_forwarded_header_when_proxy_is_untrusted() {
        let request = req(Some("1.1.1.1"), Some("10.9.8.7:443"));
        assert_eq!(
            client_ip(&request, false),
            "10.9.8.7".parse::<IpAddr>().unwrap()
        );
    }

    #[tokio::test]
    async fn client_ip_falls_back_to_peer_on_unparseable_forwarded_entries() {
        // Not an address at all, and the `ip:port` form some proxies emit —
        // neither may be billed as a distinct client.
        for garbage in ["not-an-ip", "1.2.3.4:5678", ""] {
            let request = req(Some(garbage), Some("10.9.8.7:443"));
            assert_eq!(
                client_ip(&request, true),
                "10.9.8.7".parse::<IpAddr>().unwrap(),
                "{garbage:?} must fall back to the peer address"
            );
        }
    }

    #[tokio::test]
    async fn client_ip_is_unspecified_without_any_connection_info() {
        // `oneshot`-driven requests: no header, no peer. Everyone shares 0.0.0.0.
        let request = req(None, None);
        assert_eq!(client_ip(&request, true), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    }

    #[tokio::test(start_paused = true)]
    async fn allows_up_to_max_then_rejects() {
        let limiter = RateLimiter::per_minute(3, false);
        for _ in 0..3 {
            assert!(limiter.check(ip(1)).is_ok());
        }
        assert!(limiter.check(ip(1)).is_err());
    }

    /// The boundary is exact at realistic limits too, the shipped defaults
    /// included: request `max` passes, request `max + 1` is rejected — never
    /// an off-by-one in either direction.
    #[tokio::test(start_paused = true)]
    async fn boundary_is_exact_at_production_scale_limits() {
        use crate::config::{DEFAULT_API_RATE_LIMIT, DEFAULT_AUTH_RATE_LIMIT};

        for max in [1, DEFAULT_AUTH_RATE_LIMIT, 50, DEFAULT_API_RATE_LIMIT] {
            let limiter = RateLimiter::per_minute(max, false);
            for n in 1..=max {
                assert_eq!(
                    limiter.check(ip(1)),
                    Ok(()),
                    "request {n}/{max} must be admitted"
                );
            }
            assert_eq!(
                limiter.check(ip(1)),
                Err(60),
                "request {}/{max} must be rejected",
                max + 1
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn window_resets_after_a_minute() {
        let limiter = RateLimiter::per_minute(1, false);
        assert!(limiter.check(ip(1)).is_ok());
        assert!(limiter.check(ip(1)).is_err());

        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(limiter.check(ip(1)).is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_counts_down_and_never_hits_zero() {
        let limiter = RateLimiter::per_minute(1, false);
        assert!(limiter.check(ip(1)).is_ok());
        assert_eq!(limiter.check(ip(1)), Err(60));

        tokio::time::advance(Duration::from_secs(45)).await;
        assert_eq!(limiter.check(ip(1)), Err(15));

        // 59.5s in: 0.5s remain, which must round up to 1, not down to 0.
        tokio::time::advance(Duration::from_millis(14_500)).await;
        assert_eq!(limiter.check(ip(1)), Err(1));
    }

    #[tokio::test(start_paused = true)]
    async fn ips_get_independent_buckets() {
        let limiter = RateLimiter::per_minute(1, false);
        assert!(limiter.check(ip(1)).is_ok());
        assert!(limiter.check(ip(1)).is_err());
        assert!(limiter.check(ip(2)).is_ok());
    }

    /// The keyed tier the chatbot handler calls: users are metered
    /// independently, refused past the cap, and freed again a minute later.
    #[tokio::test(start_paused = true)]
    async fn users_get_independent_windows_that_reset() {
        let limiter = UserRateLimiter::default();
        for _ in 0..crate::config::DEFAULT_CHATBOT_RATE_LIMIT {
            assert!(limiter.enforce_user("user:a").is_ok());
        }
        assert!(matches!(
            limiter.enforce_user("user:a"),
            Err(AppError::TooManyRequests {
                retry_after_secs: 60
            })
        ));
        assert!(
            limiter.enforce_user("user:b").is_ok(),
            "another user must not inherit a's window"
        );

        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(limiter.enforce_user("user:a").is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn zero_means_disabled() {
        let limiter = RateLimiter::per_minute(0, false);
        for _ in 0..1_000 {
            assert!(limiter.check(ip(1)).is_ok());
        }
        assert!(limiter.buckets.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn stale_buckets_are_purged_once_map_is_large() {
        let limiter = RateLimiter::per_minute(5, false);
        for i in 0..PURGE_AT as u32 {
            let addr = IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + i));
            assert!(limiter.check(addr).is_ok());
        }
        assert_eq!(limiter.buckets.lock().unwrap().len(), PURGE_AT);

        // All those windows lapse; the next check sweeps them out.
        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(limiter.check(ip(1)).is_ok());
        assert_eq!(limiter.buckets.lock().unwrap().len(), 1);
    }
}
