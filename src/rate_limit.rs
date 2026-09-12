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
//! **rightmost entry of the last** `X-Forwarded-For` header instead — that hop
//! is appended by the nearest proxy and is the only one the client cannot
//! forge. Both halves matter: a proxy may append its hop as a separate header
//! line rather than into the client's, and reading only the first line would
//! hand the key straight back to the client. Never enable it when clients can
//! reach the server directly: the header is then entirely attacker-controlled
//! and the limiter is trivially bypassed.
//!
//! Fixed windows admit up to a 2× burst straddling a window boundary. That is
//! an accepted trade-off for an implementation simple enough to read in one
//! sitting; argon2 keeps each allowed login attempt expensive anyway.
//!
//! # Bounding the bucket map
//!
//! One client, one bucket, so the map is only as bounded as the client set —
//! and an IPv6 /64 is not bounded at all. At [`crate::constant::PURGE_AT`] a
//! new key first sweeps out the lapsed windows and then, if that frees
//! nothing, evicts down to three quarters of the cap ([`evict_cheapest`]).
//! Exhausted buckets are exempt from eviction: they are the only ones actually
//! refusing anyone, and freeing one *raises* the limit for a client that had
//! reached it. When every bucket is exhausted there is nothing to evict and
//! the map stays at the cap; the unknown key is then metered against one shared
//! overflow counter ([`crate::constant::RATE_LIMIT_OVERFLOW_MAX`] per window,
//! for every keyless client together) instead of getting a bucket.
//!
//! Be precise about what that buys, because it is not everyone. A client
//! **already in the map** keeps its own counter throughout: no flood can starve
//! it, evict it, or spend its budget, which is the property worth having.
//! A **newcomer during** the flood is a different story — it never gets a
//! bucket while the map stays saturated, so *all* of its traffic (not just its
//! first request) comes out of the one shared budget, and once that budget is
//! spent every further newcomer is refused. An attacker who first fills the map
//! and then burns the shared budget therefore does `429` every newcomer for the
//! rest of the window; `tests/rate_limit.rs` asserts exactly that, ending on a
//! refused `newcomer-last`.
//!
//! That is the accepted trade, not an oversight. The alternative — admitting
//! keyless clients freely — makes a filled map the way to buy unmetered
//! throughput from fresh keys, which is an unbounded bypass rather than a
//! bounded outage, and refusing them outright would hand the attacker the same
//! `429`-the-newcomers result for free. The shared budget costs an attacker the
//! flood *plus* [`crate::constant::RATE_LIMIT_OVERFLOW_MAX`] requests a minute,
//! every minute, to keep it going.
//!
//! # The shared window
//!
//! The counters above are per process. [`RateLimiter::share`] also carries them
//! through the process: a background task folds each bucket's new admits into
//! one shared row per tier + client + wall window (`rate_limit`, see
//! [`crate::constant::RATE_LIMIT_TABLE`]) every
//! [`crate::constant::RATE_SYNC_INTERVAL_SECS`], and the stored total that
//! comes back caps what the local bucket admits for the rest of that window —
//! so a restart mid-window does not hand every client a fresh budget.
//!
//! Admission itself stays in memory and stays synchronous — no request ever
//! waits on the database to be let in, and a database that is down or slow
//! costs nothing but the sharing (the tiers fall back to their local budgets
//! rather than failing anyone). The cost is a lag: a process that has just
//! started can spend up to its own full budget for one interval before the
//! shared total tells it to stop.

use std::collections::HashMap;
use std::hash::Hash;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::extract::{ConnectInfo, Request};
use axum::middleware::Next;
use axum::response::Response;
// tokio's `Instant` wraps `std::time::Instant` in production but obeys
// `tokio::time::pause`/`advance` under `start_paused` tests, which makes the
// window arithmetic below testable without sleeping.
use tokio::time::Instant;

use crate::constant::{
    PURGE_AT, RATE_LIMIT_OVERFLOW_MAX, RATE_SYNC_INTERVAL_SECS, RATE_SYNC_MAX_KEYS,
    RATE_SYNC_TIMEOUT_SECS,
};
use crate::database::Database;
use crate::domain::timestamp::Timestamp;
use crate::error::AppError;
use crate::telemetry::Metrics;

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
    /// What this tier is called in telemetry (`api`, `auth`, `chatbot`,
    /// `builder`). Named by [`RateLimiter::share`], which is where the tier's
    /// name already lives — the constructors take a number and nothing else,
    /// and every tier the router builds is shared. Shared with the clones, so
    /// the middleware's clone sees the name too.
    tier: Arc<OnceLock<&'static str>>,
    /// The single bucket every client shares once the map is saturated and no
    /// eviction is allowed (see the module docs). Local only: it is keyless, so
    /// it is never synced to the shared table.
    overflow: Arc<Mutex<Bucket>>,
}

struct Bucket {
    window_start: Instant,
    /// Requests this process admitted in the current local window.
    count: u32,
    /// How many of `count` are accounted for against the *current* shared row
    /// — folded into it, or charged to an earlier one before the wall window
    /// rolled. Never above `count`, so `count - pushed` is what is still ours
    /// to report.
    pushed: u32,
    /// The stored total the last sync read back — every admit in this wall
    /// window, our `pushed` share included. Zero until a sync lands, which is
    /// what makes an unshared limiter behave exactly like the local-only one it
    /// replaced.
    remote: u32,
    /// The wall window (`RATE_LIMIT_TABLE` row) `pushed`/`remote` describe, or
    /// `None` until the first sync. The local window rolls per client while the
    /// shared one is aligned to the clock, so this says which shared row those
    /// two numbers came from.
    epoch: Option<i64>,
}

impl Bucket {
    /// A fresh window: nothing admitted, nothing shared, nothing known.
    fn opened_at(now: Instant) -> Self {
        Self {
            window_start: now,
            count: 0,
            pushed: 0,
            remote: 0,
            epoch: None,
        }
    }

    /// Requests spent as far as this process can tell: what the shared row held
    /// at the last sync — this process's earlier life included — plus our own
    /// admits since, and never less than this local window's own count. Equals
    /// `count` until a sync lands.
    ///
    /// The local floor is what a wall-window roll needs: the admits it moves
    /// out of the shared reckoning (they belong to the row that has passed)
    /// must not come back as fresh budget inside a local window that is still
    /// running.
    fn spent(&self) -> u32 {
        self.count
            .max(self.remote.saturating_add(self.count - self.pushed))
    }
}

impl<K> RateLimiter<K> {
    /// This tier's telemetry name, or `unnamed` for a limiter nobody shared
    /// (tests build those; the router shares every tier it wires).
    fn tier(&self) -> &'static str {
        self.tier.get().copied().unwrap_or("unnamed")
    }

    /// One refusal, counted for the operator. Deliberately *only* a counter:
    /// a log line per rejection is a flood amplifier, and the only fields
    /// worth having (who, from where) are the ones telemetry may not carry.
    fn count_rejection(&self) {
        Metrics::global()
            .rate_limit_rejections_total
            .add(1, &[opentelemetry::KeyValue::new("tier", self.tier())]);
    }

    /// Requests allowed per window, `0` meaning the tier is off. Read by
    /// `GET /limits` so a client learns its own budget instead of discovering
    /// it by getting a `429`.
    pub fn max_per_window(&self) -> u32 {
        self.max
    }
}

/// The per-user tier, keyed by user id instead of client IP. Lives in
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
            tracing::warn!(retry_after_secs, "per-user rate limit exceeded");
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
                tracing::warn!(retry_after_secs, "rate limit exceeded");
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
            tier: Arc::new(OnceLock::new()),
            overflow: Arc::new(Mutex::new(Bucket::opened_at(Instant::now()))),
        }
    }

    /// Meter one keyless request against the shared overflow budget. Same fixed
    /// window as a real bucket, one counter for everyone who lands here.
    fn check_overflow(&self, now: Instant) -> Result<(), u64> {
        let mut bucket = self.overflow.lock().expect("rate limiter mutex poisoned");
        if now.duration_since(bucket.window_start) >= self.window {
            *bucket = Bucket::opened_at(now);
        }
        // `count` and not `spent()`: nothing shared ever reaches this bucket.
        if bucket.count < RATE_LIMIT_OVERFLOW_MAX {
            bucket.count += 1;
            return Ok(());
        }
        let remaining = self.window - now.duration_since(bucket.window_start);
        Err((remaining.as_secs_f64().ceil() as u64).max(1))
    }

    /// [`RateLimiter::check_key`], counting whatever it refuses. Every tier's
    /// rejections pass through here — both `enforce` paths and the overflow
    /// budget — so the counter cannot miss one.
    fn check(&self, key: K) -> Result<(), u64> {
        self.check_key(key).inspect_err(|_| self.count_rejection())
    }

    /// Count one request from `key`. `Err` carries the whole seconds (rounded
    /// up, at least 1) until the window resets — the `Retry-After` value.
    fn check_key(&self, key: K) -> Result<(), u64> {
        if self.max == 0 {
            return Ok(());
        }

        let now = Instant::now();
        let mut buckets = self.buckets.lock().expect("rate limiter mutex poisoned");

        // Only a *new* key can grow the map, so only a new key pays for a sweep
        // — a client already in there stays one hash lookup, saturated or not.
        // corner-cut: while the map is saturated every new key still pays one
        // O(n) scan. Ceiling: it takes 10k live exhausted clients to get there
        // and the scan is the attacker's own request. Upgrade path: keep the
        // earliest `window_start` beside the map and skip the scan until then.
        if buckets.len() >= PURGE_AT && !buckets.contains_key(&key) {
            buckets.retain(|_, b| now.duration_since(b.window_start) < self.window);
            // Lapsed buckets alone are no bound: enough distinct clients inside
            // one window (one IPv6 /64 is enough) frees nothing, and the map
            // would grow while every request paid for the scan.
            if buckets.len() >= PURGE_AT {
                evict_cheapest(&mut buckets, PURGE_AT - PURGE_AT / 4, self.max);
            }
            if buckets.len() >= PURGE_AT {
                // Every bucket left is exhausted and none may be dropped. This
                // key gets no bucket, but it is still metered: everyone in that
                // position shares one aggregate counter. Refusing outright
                // would 429 every newcomer for free; admitting freely would
                // make a saturated map the bypass. Clients already in the map
                // are untouched either way (see the module docs).
                drop(buckets);
                return self.check_overflow(now);
            }
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

/// Drop buckets until at most `target` remain, cheapest first, and **never** a
/// bucket that has spent `max` or more.
///
/// The exemption is the whole point. An exhausted bucket is the only thing in
/// here actually refusing anyone, and evicting it hands that refusal straight
/// back to whoever earned it. Cheapest-first alone does not protect it: the
/// cutoff is a `<=`, so ties go arbitrarily, and an attacker who drives every
/// flood key to exactly the victim's spend puts the victim inside the tie band
/// (measured: a victim at `max` freed after ~12.5k flood keys). Exhaustion,
/// not rank, is what decides.
///
/// Among the rest, least-spent-first: those are the throwaway keys a flood is
/// made of, and evicting one gives away only what it had spent. Evicting by
/// age would instead pick out long-running buckets, which is backwards.
///
/// One pass over the map, and only when a new key arrives at the cap: the
/// target sits a quarter below it, so the sweep is amortised over that many
/// further inserts and the common path stays a single hash lookup.
fn evict_cheapest<K: Eq + Hash>(buckets: &mut HashMap<K, Bucket>, target: usize, max: u32) {
    let Some(excess) = buckets.len().checked_sub(target).filter(|n| *n > 0) else {
        return;
    };
    let mut spent: Vec<u32> = buckets
        .values()
        .map(Bucket::spent)
        .filter(|spent| *spent < max)
        .collect();
    // Nothing but exhausted buckets: the caller decides what to do instead.
    let Some(excess) = Some(excess.min(spent.len())).filter(|n| *n > 0) else {
        return;
    };
    // The `excess`-th smallest spend among the evictable: at least `excess`
    // buckets are at or under it, which is what makes the budgeted retain
    // below drop exactly `excess`.
    let (_, &mut cutoff, _) = spent.select_nth_unstable(excess - 1);
    let mut budget = excess;
    buckets.retain(|_, b| {
        let spent = b.spent();
        let evict = budget > 0 && spent < max && spent <= cutoff;
        budget -= usize::from(evict);
        !evict
    });
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
    /// Make this tier's budget outlive the process: every
    /// [`RATE_SYNC_INTERVAL_SECS`] a background task reports what this limiter
    /// has admitted and reads back the window's total, which caps further local
    /// admits in the same window (see the module docs).
    ///
    /// `tier` names the counter's namespace in the shared table — two limiters
    /// sharing a name share a budget, which is exactly what the two processes
    /// running the same tier want and what `auth` and `api` must avoid.
    ///
    /// The task holds a *weak* reference to the buckets, so it stops with the
    /// limiter rather than keeping a dropped one alive (test suites build
    /// dozens).
    pub fn share(&self, tier: &'static str, db: Database) {
        self.share_windowed(tier, db, None);
    }

    /// [`RateLimiter::share`] with the wall window pinned, for tests only.
    ///
    /// A round reads the real clock, so any assertion about *the* shared row
    /// is otherwise a bet that the run does not cross a minute mid-round —
    /// two limiters of one tier would then report into two rows and neither
    /// would see the other's spend. An accidental relationship to the wall
    /// clock is precisely what kept the epoch-roll double-charge out of
    /// `tests/rate_limit.rs`, so the sharing tests choose their window instead.
    pub fn share_pinned(&self, tier: &'static str, db: Database, epoch: i64) {
        self.share_windowed(tier, db, Some(epoch));
    }

    fn share_windowed(&self, tier: &'static str, db: Database, pinned: Option<i64>) {
        // Before the early return: a disabled tier still wants its name, so a
        // limiter turned on later reports under it.
        let _ = self.tier.set(tier);
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
                // `with_deadline` bounds every round, so an outage costs the
                // sharing nothing but skipped rounds: local budgets keep
                // admitting and the deltas ride the first round after recovery.
                let epoch = pinned.unwrap_or_else(|| current_epoch(window));
                sync_once(tier, &buckets, window, epoch, &db).await;
                // Piggybacked cleanup, once per window: rows for a window that
                // has passed can never be read again.
                if swept != epoch {
                    swept = epoch;
                    if let Err(err) = with_deadline(
                        sqlx::query("DELETE FROM rate_limit WHERE window_start < $1")
                            .bind(epoch)
                            .execute(&db),
                    )
                    .await
                    {
                        tracing::warn!(%err, "rate-limit sweep failed; retrying next window");
                    }
                }
            }
        });
    }
}

/// Fold every live bucket's new admits into their shared rows and take the
/// window's totals back.
///
/// The whole round is one statement: `INSERT … ON CONFLICT (id) DO UPDATE SET
/// hits = rate_limit.hits + EXCLUDED.hits RETURNING id, hits`, so a fold is
/// atomic — the old engine could fail one bucket's statement inside a round,
/// Postgres cannot fail half a statement. A round that fails (deadline,
/// outage) lands nothing: every bucket keeps its delta unreported and the
/// next round (2s later) carries it, which is why no retry loop is needed
/// here. Retrying the *statement* would double-apply the hits that landed,
/// since the fold accumulates — skipping a round is the only honest failure.
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
                if b.epoch != Some(epoch) {
                    // A different shared row from the one `pushed`/`remote`
                    // describe: that total is about a window that has passed.
                    // What was already charged to it must not be charged again
                    // here — every local window straddles a boundary, so
                    // re-reporting the whole count would bill each of those
                    // requests to two rows and spend the client's next budget
                    // before it began. Only a bucket that has never synced owes
                    // its whole count, having been billed nowhere yet.
                    //
                    // corner-cut: admits between the boundary and this round land
                    // on the old row. Ceiling: RATE_SYNC_INTERVAL_SECS of one
                    // client's traffic, charged once either way. Upgrade path:
                    // stamp each admit with its epoch.
                    if b.epoch.is_some() {
                        b.pushed = b.count;
                    }
                    b.epoch = Some(epoch);
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

    // One statement for the whole round. A row's id pins its wall window
    // (`row_key` folds the epoch in), so a conflict can only be another
    // limiter of the same tier folding the same client inside the same window
    // — the ON CONFLICT fold is exactly the replay that must add, not clobber.
    let mut fold = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        "INSERT INTO rate_limit (id, hits, window_start) ",
    );
    fold.push_values(pending.iter(), |mut b, p| {
        b.push_bind(row_key(tier, &p.key, epoch))
            .push_bind(i64::from(p.delta))
            .push_bind(epoch);
    });
    fold.push(
        " ON CONFLICT (id) DO UPDATE SET hits = rate_limit.hits + EXCLUDED.hits RETURNING id, hits",
    );
    let rows: Vec<(String, i64)> = match with_deadline(fold.build_query_as().fetch_all(db)).await {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!(%err, tier, "rate-limit sync failed; counting locally until it recovers");
            return;
        }
    };
    let totals: HashMap<String, i64> = rows.into_iter().collect();

    let mut guard = buckets.lock().expect("rate limiter mutex poisoned");
    for p in &pending {
        // A row missing from the fold's answer stands in for the failed
        // statement it replaces: its bucket stays untouched, the delta stays
        // unreported, and the next round carries it.
        let Some(total) = totals.get(&row_key(tier, &p.key, epoch)) else {
            continue;
        };
        let Some(bucket) = guard.get_mut(&p.key) else {
            continue;
        };
        // The window may have rolled while the query was in flight, in which
        // case this total is about a bucket that no longer exists.
        if bucket.epoch != Some(epoch) || bucket.window_start != p.window_start {
            continue;
        }
        bucket.pushed = p.counted;
        bucket.remote = (*total).clamp(0, u32::MAX.into()) as u32;
    }
}

/// The wall-clock window a shared row is keyed by, aligned so a restarted
/// process lands on the same one — the local windows cannot be used for this,
/// since each starts whenever that client's first request happened to land.
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

/// Run one sync statement under a deadline. Postgres fails a dead connection
/// instead of parking the query, but a slow or overloaded server can still
/// stall a round past what the sharing can afford — every tier's budget rides
/// this one task, so a wedged round would stall all of them. A dropped round
/// leaves its deltas unreported and the next round carries them.
async fn with_deadline<T, Fut>(query: Fut) -> Result<T, AppError>
where
    Fut: Future<Output = Result<T, sqlx::Error>>,
{
    tokio::time::timeout(Duration::from_secs(RATE_SYNC_TIMEOUT_SECS), query)
        .await
        .map_err(|_| AppError::Internal("rate-limit sync timed out".into()))?
        .map_err(AppError::from)
}

/// Resolve the client IP a request is billed against.
///
/// With `trust_proxy`, prefer the rightmost entry of the *last*
/// `X-Forwarded-For` header — appended by the nearest (trusted) proxy, unlike
/// the left entries and the earlier header lines, which arrive
/// attacker-controlled (a proxy that appends its own header line instead of
/// extending the client's leaves the client's line first). Earlier lines are
/// only consulted if the last one is empty or unparseable; if none yield an
/// address (or the header is missing) fall back to the connection's peer
/// address. `oneshot`-driven routers carry no [`ConnectInfo`]; those requests
/// share the unspecified-address bucket.
fn client_ip(req: &Request, trust_proxy: bool) -> IpAddr {
    if trust_proxy
        && let Some(ip) = req
            .headers()
            .get_all("x-forwarded-for")
            .iter()
            .rev()
            .find_map(|v| v.to_str().ok()?.rsplit(',').next()?.trim().parse().ok())
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

    /// A proxy that appends its hop as its own header line instead of
    /// extending the client's: the client's line comes first and is entirely
    /// forged, so only the last line may be billed.
    #[tokio::test]
    async fn client_ip_takes_the_last_forwarded_header_line() {
        let two_lines = |first: &str, last: &str| {
            axum::http::Request::builder()
                .uri("/")
                .header("x-forwarded-for", first)
                .header("x-forwarded-for", last)
                .body(Body::empty())
                .unwrap()
        };

        assert_eq!(
            client_ip(&two_lines("1.1.1.1, 6.6.6.6", "9.9.9.9"), true),
            "9.9.9.9".parse::<IpAddr>().unwrap()
        );
        // An unusable last line falls back through the earlier ones before it
        // gives up on the header entirely.
        assert_eq!(
            client_ip(&two_lines("1.1.1.1, 6.6.6.6", "not-an-ip"), true),
            "6.6.6.6".parse::<IpAddr>().unwrap()
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

        // All those windows lapse; the next *new* client sweeps them out (only
        // a key that would grow the map pays for a sweep — 10.x is taken above,
        // so the new client has to come from somewhere else).
        tokio::time::advance(Duration::from_secs(61)).await;
        let newcomer = IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1));
        assert!(limiter.check(newcomer).is_ok());
        assert_eq!(limiter.buckets.lock().unwrap().len(), 1);
    }

    /// The bound has to hold when *nothing* has lapsed — the sweep above frees
    /// nothing then, and one IPv6 /64 supplies all the live keys it takes. The
    /// eviction must spend itself on the throwaway keys, never on the
    /// exhausted bucket that the flood is trying to buy a fresh window for.
    #[tokio::test(start_paused = true)]
    async fn live_buckets_are_evicted_so_the_map_stays_bounded() {
        let limiter = RateLimiter::per_minute(5, false);
        for _ in 0..5 {
            assert!(limiter.check(ip(1)).is_ok());
        }
        assert!(limiter.check(ip(1)).is_err(), "victim starts exhausted");

        // Twice the threshold in distinct clients, all inside one window.
        for i in 0..2 * PURGE_AT as u32 {
            let addr = IpAddr::V4(Ipv4Addr::from(0x0b00_0000 + i));
            assert!(limiter.check(addr).is_ok());
            assert!(
                limiter.buckets.lock().unwrap().len() <= PURGE_AT,
                "map grew past the cap at client {i}"
            );
        }

        assert!(
            limiter.check(ip(1)).is_err(),
            "the flood must not have bought the exhausted bucket a fresh window"
        );
    }

    /// The flood above was cheap to tell apart. This one is not: every flood
    /// key is driven to *exactly* the spend of the bucket it is trying to
    /// free, so a cheapest-first cutoff of `<=` puts the victim inside the tie
    /// band and picks arbitrarily. Nothing that has reached its limit may be
    /// evicted, whoever else shares its spend — a limiter that forgets an
    /// exhausted bucket fails open, which is worse than the unbounded map.
    #[tokio::test(start_paused = true)]
    async fn a_flood_at_the_victims_own_spend_frees_nobody() {
        const MAX: u32 = 5;
        let limiter = RateLimiter::per_minute(MAX, false);
        // Every key here is driven to exhaustion, the named victim first.
        let keys: Vec<IpAddr> = std::iter::once(ip(1))
            .chain((0..PURGE_AT as u32).map(|i| IpAddr::V4(Ipv4Addr::from(0x0b00_0000 + i))))
            .collect();
        for key in &keys {
            for _ in 0..MAX {
                assert!(limiter.check(*key).is_ok());
            }
        }

        assert!(
            limiter.buckets.lock().unwrap().len() <= PURGE_AT,
            "map grew past the cap"
        );

        // The first `PURGE_AT` keys — the victim among them — filled the map
        // before it saturated, so every one of them was metered all the way to
        // its limit and every one of them must still be refused. An eviction
        // that dropped an exhausted bucket shows up right here, as an admit.
        for key in &keys[..PURGE_AT] {
            assert!(
                limiter.check(*key).is_err(),
                "{key} was exhausted and is being served again"
            );
        }
    }

    /// How many of `tries` requests the limiter admits for `user`.
    fn admits(limiter: &UserRateLimiter, user: &str, tries: usize) -> usize {
        (0..tries)
            .filter(|_| limiter.enforce_user(user).is_ok())
            .count()
    }

    /// A wall-window roll must not bill one request to two shared rows: the
    /// fold reports only a bucket's *unreported* delta, so when the wall
    /// window rolls under a still-live local window, the new row receives
    /// nothing the old row had already taken. (Rebuilt over the control
    /// database — `rate_limit` is a control table — after the SurrealDB port
    /// dropped it along with the embedded engine.)
    #[tokio::test]
    async fn a_wall_epoch_roll_charges_no_request_twice() {
        const MAX: u32 = 5;
        let tenants = crate::database::init_test_tenants().await;
        let db = tenants.control().clone();
        let limiter = UserRateLimiter::per_user_minute(MAX);
        let window = Duration::from_secs(60);
        let epoch = current_epoch(window);
        assert_eq!(window.as_millis() as i64, 60_000, "epochs are 60s apart");

        // A burst early in the client's local window, folded into row `epoch`.
        for _ in 0..MAX {
            assert!(limiter.enforce_user("user:a").is_ok());
        }
        sync_once("test", &limiter.buckets, window, epoch, &db).await;

        // The wall clock rolls while that local window is still running.
        sync_once("test", &limiter.buckets, window, epoch + 60_000, &db).await;

        // The local window lapses, so the client opens a fresh one — inside
        // the *same* new wall window — and spends one request in it.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(61)).await;
        tokio::time::resume();
        assert_eq!(admits(&limiter, "user:a", 1), 1, "a fresh local window");
        sync_once("test", &limiter.buckets, window, epoch + 60_000, &db).await;

        let rows =
            sqlx::query_scalar::<_, i64>("SELECT hits FROM rate_limit ORDER BY window_start")
                .fetch_all(&db)
                .await
                .expect("read shared counters");
        assert_eq!(
            rows,
            vec![i64::from(MAX), 1],
            "the new wall row may hold only what was admitted inside it"
        );
        assert_eq!(
            admits(&limiter, "user:a", 4),
            4,
            "the client spent 1 of {MAX} in this window and must keep the rest"
        );
    }
}
