//! Rate limiting, hand-rolled on a fixed 60-second window — no extra crates,
//! and nothing on the request path but a mutex (the one background task below
//! only shares counters). The counter is keyed generically: by client IP for the
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
//!
//! # Across replicas
//!
//! The counters above are per process, and the backend runs as two replicas —
//! so a client hitting both got twice its budget. [`RateLimiter::share`] closes
//! that: a background task folds each bucket's new admits into one shared row
//! per tier + client + wall window (`rate_limit`, see
//! [`crate::constant::RATE_LIMIT_TABLE`]) every
//! [`crate::constant::RATE_SYNC_INTERVAL_SECS`], and the fleet-wide total that
//! comes back caps what the local bucket admits for the rest of that window.
//!
//! Admission itself stays in memory and stays synchronous — no request ever
//! waits on the database to be let in, and a database that is down or slow
//! costs nothing but the sharing (the tiers fall back to their local budgets
//! rather than failing anyone). The cost is a lag: within one interval a
//! replica can spend up to its own full budget before the shared total tells
//! it to stop, so the fleet's worst case is `replicas × max` for one interval
//! and `max` from then on.

use std::collections::HashMap;
use std::hash::Hash;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{ConnectInfo, Request};
use axum::middleware::Next;
use axum::response::Response;
use surrealdb::types::RecordId;
// tokio's `Instant` wraps `std::time::Instant` in production but obeys
// `tokio::time::pause`/`advance` under `start_paused` tests, which makes the
// window arithmetic below testable without sleeping.
use tokio::time::Instant;

use crate::constant::{
    PURGE_AT, RATE_LIMIT_TABLE, RATE_SYNC_INTERVAL_SECS, RATE_SYNC_MAX_KEYS, RATE_SYNC_TIMEOUT_SECS,
};
use crate::database::Database;
use crate::domain::timestamp::Timestamp;
use crate::error::AppError;
use crate::state::DbHealth;

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
    /// Requests this process admitted in the current local window.
    count: u32,
    /// How many of `count` the sync task has already folded into the shared
    /// row. Never above `count`, so `count - pushed` is what is still ours to
    /// report.
    pushed: u32,
    /// The fleet-wide total the last sync read back — everyone's admits,
    /// `pushed` included. Zero until a sync lands, which is what makes an
    /// unshared limiter behave exactly like the local-only one it replaced.
    remote: u32,
    /// The wall window (`RATE_LIMIT_TABLE` row) `pushed`/`remote` describe.
    /// The local window rolls per client while the shared one is aligned to the
    /// clock, so this says which shared row those two numbers came from.
    epoch: i64,
}

impl Bucket {
    /// A fresh window: nothing admitted, nothing shared, nothing known.
    fn opened_at(now: Instant) -> Self {
        Self {
            window_start: now,
            count: 0,
            pushed: 0,
            remote: 0,
            epoch: 0,
        }
    }

    /// Requests the whole fleet has spent in this window as far as this process
    /// can tell: what every replica had reported at the last sync, plus our own
    /// admits since. Equals `count` until a sync lands.
    fn spent(&self) -> u32 {
        self.remote.saturating_add(self.count - self.pushed)
    }
}

impl<K> RateLimiter<K> {
    /// Requests allowed per window, `0` meaning the tier is off. Read by
    /// `GET /limits` so a client learns its own budget instead of discovering
    /// it by getting a `429`.
    pub fn max_per_window(&self) -> u32 {
        self.max
    }
}

/// The per-user tier, keyed by user record key instead of client IP. Lives in
/// [`crate::state::AppState`] and is called from inside a handler, after
/// `CurrentUser` has identified the caller.
pub type UserRateLimiter = RateLimiter<String>;

impl Default for UserRateLimiter {
    /// The chatbot tier at its shipped default, for tests and any caller that
    /// has no [`crate::config::Config`] to hand.
    fn default() -> Self {
        Self::per_user_minute(crate::constant::DEFAULT_CHATBOT_RATE_LIMIT)
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

        let bucket = buckets.entry(key).or_insert_with(|| Bucket::opened_at(now));
        if now.duration_since(bucket.window_start) >= self.window {
            // A fresh window starts owing nothing and knowing nothing: keeping
            // the old total would hold the client at a budget it no longer
            // spent. The next sync re-reads what the fleet has spent since.
            *bucket = Bucket::opened_at(now);
        }

        if bucket.spent() < self.max {
            bucket.count += 1;
            Ok(())
        } else {
            let remaining = self.window - now.duration_since(bucket.window_start);
            Err((remaining.as_secs_f64().ceil() as u64).max(1))
        }
    }
}

/// One bucket's report, taken while the map was locked so the round can run
/// its query without holding it. `counted` is the `count` the delta was read
/// from — a request admitted mid-round bumps `count` past it and is simply
/// carried into the next round.
struct Pending<K> {
    key: K,
    delta: u32,
    counted: u32,
    window_start: Instant,
}

impl<K> RateLimiter<K>
where
    K: Eq + Hash + Clone + std::fmt::Display + Send + Sync + 'static,
{
    /// Make this tier's budget fleet-wide: every
    /// [`RATE_SYNC_INTERVAL_SECS`] a background task reports what this replica
    /// has admitted and reads back what everyone has, which caps further local
    /// admits in the same window (see the module docs).
    ///
    /// `tier` names the counter's namespace in the shared table — two limiters
    /// sharing a name share a budget, which is exactly what the two processes
    /// running the same tier want and what `auth` and `api` must avoid.
    ///
    /// The task holds a *weak* reference to the buckets, so it stops with the
    /// limiter rather than keeping a dropped one alive (test suites build
    /// dozens).
    pub fn share(&self, tier: &'static str, db: Database, db_up: DbHealth) {
        if self.max == 0 {
            // The tier is off: `check` records nothing, so there is nothing to
            // share and no reason to hold a task or a table row.
            return;
        }
        let buckets = Arc::downgrade(&self.buckets);
        let window = self.window;
        tokio::spawn(async move {
            let mut swept = 0;
            loop {
                tokio::time::sleep(Duration::from_secs(RATE_SYNC_INTERVAL_SECS)).await;
                let Some(buckets) = buckets.upgrade() else {
                    return;
                };
                // Never queue work on a database that is down: the SDK parks a
                // query instead of failing it, so this task would sit on a
                // round for the whole outage and then apply a stale total.
                if !db_up.is_up() {
                    continue;
                }
                let epoch = current_epoch(window);
                sync_once(tier, &buckets, window, epoch, &db).await;
                // Piggybacked cleanup, once per window: rows for a window that
                // has passed can never be read again.
                if swept != epoch {
                    swept = epoch;
                    let sql = format!("DELETE {RATE_LIMIT_TABLE} WHERE window_start < $cutoff");
                    if let Err(err) = with_deadline(db.query(sql).bind(("cutoff", epoch))).await {
                        tracing::warn!(%err, "rate-limit sweep failed; retrying next window");
                    }
                }
            }
        });
    }
}

/// Fold every live bucket's new admits into its shared row and take the
/// fleet-wide total back.
///
/// One statement per bucket in one query, and each statement is its own
/// transaction — so a statement that loses a write race fails alone. Its bucket
/// simply keeps the delta unreported and the next round (2s later) carries it,
/// which is why no retry loop is needed here: retrying the *query* would
/// double-apply the statements that did land, since `hits` accumulates.
async fn sync_once<K: Eq + Hash + Clone + std::fmt::Display>(
    tier: &str,
    buckets: &Mutex<HashMap<K, Bucket>>,
    window: Duration,
    epoch: i64,
    db: &Database,
) {
    let now = Instant::now();
    let pending: Vec<Pending<K>> = {
        let mut guard = buckets.lock().expect("rate limiter mutex poisoned");
        guard
            .iter_mut()
            .filter(|(_, b)| b.count > 0 && now.duration_since(b.window_start) < window)
            .take(RATE_SYNC_MAX_KEYS)
            .map(|(key, b)| {
                if b.epoch != epoch {
                    // The shared window rolled: the row this bucket is about to
                    // write knows nothing of us, so the whole local count is
                    // the delta and the previous total describes a dead row.
                    b.epoch = epoch;
                    b.pushed = 0;
                    b.remote = 0;
                }
                Pending {
                    key: key.clone(),
                    delta: b.count - b.pushed,
                    counted: b.count,
                    window_start: b.window_start,
                }
            })
            .collect()
    };
    if pending.is_empty() {
        return;
    }

    let sql: String = (0..pending.len())
        .map(|i| {
            format!("UPSERT $id{i} SET hits = (hits ?? 0) + $delta{i}, window_start = $window RETURN VALUE hits;")
        })
        .collect();
    let mut query = db.query(sql).bind(("window", epoch));
    for (i, p) in pending.iter().enumerate() {
        let id = RecordId::new(RATE_LIMIT_TABLE, row_key(tier, &p.key, epoch));
        query = query
            .bind((format!("id{i}"), id))
            .bind((format!("delta{i}"), i64::from(p.delta)));
    }
    let mut response = match with_deadline(query).await {
        Ok(response) => response,
        Err(err) => {
            tracing::warn!(%err, "rate-limit sync failed; counting locally until it recovers");
            return;
        }
    };

    let mut guard = buckets.lock().expect("rate limiter mutex poisoned");
    for (i, p) in pending.iter().enumerate() {
        // A statement that failed (a lost write race) leaves its bucket
        // untouched: the delta stays unreported and the next round carries it.
        let Ok(total) = response.take::<Vec<i64>>(i) else {
            continue;
        };
        let (Some(total), Some(bucket)) = (total.first(), guard.get_mut(&p.key)) else {
            continue;
        };
        // The window may have rolled while the query was in flight, in which
        // case this total is about a bucket that no longer exists.
        if bucket.epoch != epoch || bucket.window_start != p.window_start {
            continue;
        }
        bucket.pushed = p.counted;
        bucket.remote = (*total).clamp(0, u32::MAX.into()) as u32;
    }
}

/// The wall-clock window a shared row is keyed by, aligned so every replica
/// agrees on it — the local windows cannot be used for this, since each one
/// starts whenever that client's first request happened to land.
fn current_epoch(window: Duration) -> i64 {
    let ms = i64::try_from(window.as_millis()).unwrap_or(i64::MAX).max(1);
    Timestamp::now().as_millis().div_euclid(ms) * ms
}

/// The shared row's key: tier, client, window. The client part is hashed
/// because it is a raw IP or user id — free of the `:` and `_` that would
/// otherwise let one client's key collide with another's by construction.
fn row_key<K: std::fmt::Display>(tier: &str, key: &K, epoch: i64) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(key.to_string().as_bytes());
    format!("{tier}_{:x}_{epoch}", ByteSlice(&digest[..8]))
}

/// Lowercase hex of a byte slice, for [`row_key`].
struct ByteSlice<'a>(&'a [u8]);

impl std::fmt::LowerHex for ByteSlice<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// Run a sync query under a deadline. The liveness flag catches a *known*
/// outage; this catches the socket that died between the last ping and now,
/// which the SDK would otherwise park until the database returned — wedging
/// the one task every tier's sharing depends on.
async fn with_deadline<T>(
    query: impl std::future::IntoFuture<Output = surrealdb::Result<T>>,
) -> Result<T, AppError> {
    tokio::time::timeout(
        Duration::from_secs(RATE_SYNC_TIMEOUT_SECS),
        query.into_future(),
    )
    .await
    .map_err(|_| AppError::Internal("rate-limit sync timed out".into()))?
    .map_err(AppError::from)
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
        use crate::constant::{DEFAULT_API_RATE_LIMIT, DEFAULT_AUTH_RATE_LIMIT};

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
        for _ in 0..crate::constant::DEFAULT_CHATBOT_RATE_LIMIT {
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
