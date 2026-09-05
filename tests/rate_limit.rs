//! Router-level rate limiting tests. Each test builds the app with a
//! deliberately tight [`RateLimitConfig`] and, with `trust_proxy` on, poses as
//! different clients via the `X-Forwarded-For` header — `oneshot` requests
//! carry no real peer address, so the header path is also the only way to
//! separate callers here. The final tests instead boot a real TCP server to
//! exercise the peer-address (`ConnectInfo`) keying exactly as production runs
//! it. Window-arithmetic and header-parsing edge cases live as unit tests in
//! `src/rate_limit.rs` under paused tokio time.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use hezarfen_backend::rate_limit::RateLimitConfig;
use hezarfen_backend::state::AppState;
use hezarfen_backend::{build_router, database};
use serde_json::{Value, json};
use tower::ServiceExt;

/// App with the given limits, backed by a fresh in-memory database.
async fn app_with(rate_limit: RateLimitConfig) -> Router {
    let tenants = database::init_mem_tenants()
        .await
        .expect("in-memory deployment");
    build_router(AppState {
        db: tenants.control().clone(),
        tenants,
        files_path: tempfile::tempdir().expect("files dir").keep(),
        cookie_secure: false,
        rate_limit,
        chatbot_limit: Default::default(),
        exam_presence: Default::default(),
        board_hub: Default::default(),
        db_up: Default::default(),
        ai: None,
    })
}

/// Send one request posing as client `ip` (via `X-Forwarded-For`). Returns the
/// status, the parsed body, and the `Retry-After` header if present.
async fn send_as(
    app: &Router,
    ip: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value, Option<u64>) {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-forwarded-for", ip);
    let request = match body {
        Some(b) => builder
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body, retry_after)
}

fn bad_login() -> Option<Value> {
    Some(json!({ "school": "demo", "username": "ghost", "password": "wrong-pass" }))
}

fn auth_only(per_minute: u32) -> RateLimitConfig {
    RateLimitConfig {
        auth_per_minute: per_minute,
        api_per_minute: 0,
        trust_proxy: true,
    }
}

#[tokio::test]
async fn login_attempts_hit_the_auth_limit() {
    let app = app_with(auth_only(3)).await;

    for i in 1..=3 {
        let (status, _, _) = send_as(&app, "1.1.1.1", "POST", "/auth/login", bad_login()).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "attempt {i} still allowed"
        );
    }

    let (status, body, retry_after) =
        send_as(&app, "1.1.1.1", "POST", "/auth/login", bad_login()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["error"], "too many requests");
    let secs = retry_after.expect("429 must carry Retry-After");
    assert!((1..=60).contains(&secs), "Retry-After {secs} out of range");
}

#[tokio::test]
async fn register_and_login_share_the_auth_bucket() {
    let app = app_with(auth_only(2)).await;
    let creds = json!({ "school": "demo", "username": "ada", "password": "secret1" });

    let (status, _, _) = send_as(
        &app,
        "1.1.1.1",
        "POST",
        "/auth/register",
        Some(creds.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, _) = send_as(&app, "1.1.1.1", "POST", "/auth/login", Some(creds.clone())).await;
    assert_eq!(status, StatusCode::OK);

    // Two credential requests spent the whole budget; the third is metered
    // regardless of whether it would have succeeded.
    let (status, _, _) = send_as(&app, "1.1.1.1", "POST", "/auth/login", Some(creds)).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn clients_are_limited_independently() {
    let app = app_with(auth_only(1)).await;

    let (status, _, _) = send_as(&app, "1.1.1.1", "POST", "/auth/login", bad_login()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = send_as(&app, "1.1.1.1", "POST", "/auth/login", bad_login()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "1.1.1.1 exhausted");

    // A different client is untouched by 1.1.1.1's exhausted bucket.
    let (status, _, _) = send_as(&app, "2.2.2.2", "POST", "/auth/login", bad_login()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn auth_limit_spares_me_logout_and_the_rest_of_the_api() {
    let app = app_with(auth_only(1)).await;

    // Exhaust the credential budget.
    let (status, _, _) = send_as(&app, "1.1.1.1", "POST", "/auth/login", bad_login()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = send_as(&app, "1.1.1.1", "POST", "/auth/login", bad_login()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // Same client: session probing, logout, and plain API stay reachable.
    let (status, _, _) = send_as(&app, "1.1.1.1", "GET", "/auth/me", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "me is not rate limited");
    let (status, _, _) = send_as(&app, "1.1.1.1", "POST", "/auth/logout", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "logout is not rate limited");
    let (status, _, _) = send_as(&app, "1.1.1.1", "GET", "/health", None).await;
    assert_eq!(status, StatusCode::OK, "health is not rate limited");
}

#[tokio::test]
async fn api_limit_meters_every_route() {
    let app = app_with(RateLimitConfig {
        auth_per_minute: 0,
        api_per_minute: 5,
        trust_proxy: true,
    })
    .await;

    for i in 1..=5 {
        let (status, _, _) = send_as(&app, "1.1.1.1", "GET", "/health", None).await;
        assert_eq!(status, StatusCode::OK, "request {i} still allowed");
    }
    let (status, body, retry_after) = send_as(&app, "1.1.1.1", "GET", "/health", None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["error"], "too many requests");
    assert!(retry_after.is_some());
}

#[tokio::test]
async fn zero_disables_a_tier() {
    let app = app_with(RateLimitConfig::unlimited()).await;
    for _ in 0..20 {
        let (status, _, _) = send_as(&app, "1.1.1.1", "POST", "/auth/login", bad_login()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "never 429 when disabled");
    }
}

#[tokio::test]
async fn forwarded_header_is_ignored_unless_proxy_is_trusted() {
    let app = app_with(RateLimitConfig {
        auth_per_minute: 2,
        api_per_minute: 0,
        trust_proxy: false,
    })
    .await;

    // Without TRUST_PROXY, spoofed X-Forwarded-For must not mint fresh
    // buckets: all three land on the shared fallback address and the third
    // request trips the limit despite claiming three different clients.
    for (i, ip) in ["1.1.1.1", "2.2.2.2"].iter().enumerate() {
        let (status, _, _) = send_as(&app, ip, "POST", "/auth/login", bad_login()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "attempt {i} allowed");
    }
    let (status, _, _) = send_as(&app, "3.3.3.3", "POST", "/auth/login", bad_login()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn only_the_rightmost_forwarded_hop_counts() {
    let app = app_with(auth_only(2)).await;

    // Different left (client-forgeable) entries, same rightmost hop: one bucket.
    let (status, _, _) =
        send_as(&app, "6.6.6.6, 9.9.9.9", "POST", "/auth/login", bad_login()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) =
        send_as(&app, "7.7.7.7, 9.9.9.9", "POST", "/auth/login", bad_login()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) =
        send_as(&app, "8.8.8.8, 9.9.9.9", "POST", "/auth/login", bad_login()).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "forged left-hand entries must not evade the limit"
    );
}

/// A proxy that appends its hop as a *separate* `X-Forwarded-For` line leaves
/// the client's forged line first. Billing that first line would mint a bucket
/// per attempt and the credential tier would never fire, so the last line —
/// the only one the client cannot write — is what keys the request.
#[tokio::test]
async fn only_the_last_forwarded_header_line_counts() {
    let app = app_with(auth_only(2)).await;

    let attempt = |forged: &'static str| {
        let app = app.clone();
        async move {
            let request = Request::builder()
                .method("POST")
                .uri("/auth/login")
                .header("x-forwarded-for", forged)
                .header("x-forwarded-for", "9.9.9.9")
                .header("content-type", "application/json")
                .body(Body::from(bad_login().unwrap().to_string()))
                .unwrap();
            app.oneshot(request).await.unwrap().status()
        }
    };

    assert_eq!(attempt("1.1.1.1").await, StatusCode::UNAUTHORIZED);
    assert_eq!(attempt("2.2.2.2").await, StatusCode::UNAUTHORIZED);
    assert_eq!(
        attempt("3.3.3.3").await,
        StatusCode::TOO_MANY_REQUESTS,
        "a forged first line must not mint a fresh bucket per attempt"
    );
}

#[tokio::test]
async fn ipv6_clients_are_keyed_too() {
    let app = app_with(auth_only(1)).await;

    let (status, _, _) = send_as(&app, "2001:db8::1", "POST", "/auth/login", bad_login()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = send_as(&app, "2001:db8::1", "POST", "/auth/login", bad_login()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    let (status, _, _) = send_as(&app, "2001:db8::2", "POST", "/auth/login", bad_login()).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "distinct v6 client is fresh"
    );
}

#[tokio::test]
async fn malformed_forwarded_entries_share_the_fallback_bucket() {
    let app = app_with(auth_only(2)).await;

    // Unparseable entries cannot mint per-garbage buckets — they all collapse
    // onto the fallback address, so the third request trips the shared limit.
    for garbage in ["not-an-ip", "999.1.2.3"] {
        let (status, _, _) = send_as(&app, garbage, "POST", "/auth/login", bad_login()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{garbage:?} allowed once");
    }
    let (status, _, _) = send_as(&app, "also;garbage", "POST", "/auth/login", bad_login()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn auth_denials_still_spend_the_api_budget() {
    // Both tiers on: the catch-all api tier is the outer layer, so it meters
    // raw request volume — including credential attempts the inner auth tier
    // goes on to reject.
    let app = app_with(RateLimitConfig {
        auth_per_minute: 1,
        api_per_minute: 3,
        trust_proxy: true,
    })
    .await;

    let (status, _, _) = send_as(&app, "1.1.1.1", "POST", "/auth/login", bad_login()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED); // api 1/3, auth 1/1
    for _ in 0..2 {
        let (status, _, _) = send_as(&app, "1.1.1.1", "POST", "/auth/login", bad_login()).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS); // auth tier; api 2/3 then 3/3
    }

    // The api budget is gone even though only one login reached a handler.
    let (status, _, _) = send_as(&app, "1.1.1.1", "GET", "/health", None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // Per-IP, not global: another client is unaffected.
    let (status, _, _) = send_as(&app, "2.2.2.2", "GET", "/health", None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn concurrent_bursts_never_over_admit() {
    let app = app_with(RateLimitConfig {
        auth_per_minute: 0,
        api_per_minute: 5,
        trust_proxy: true,
    })
    .await;

    // 20 requests race one 5-token bucket; the mutex must admit exactly 5,
    // no matter how the tasks interleave.
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..20 {
        let app = app.clone();
        set.spawn(async move { send_as(&app, "1.1.1.1", "GET", "/health", None).await.0 });
    }

    let (mut ok, mut limited) = (0, 0);
    while let Some(status) = set.join_next().await {
        match status.unwrap() {
            StatusCode::OK => ok += 1,
            StatusCode::TOO_MANY_REQUESTS => limited += 1,
            other => panic!("unexpected status {other}"),
        }
    }
    assert_eq!((ok, limited), (5, 15));
}

/// A saturated bucket map must not be a bypass. Fill it to `PURGE_AT` with
/// exhausted buckets — nothing lapsed, nothing evictable — and every further
/// *new* key gets no bucket of its own. It is still metered: all of them share
/// one aggregate budget, `RATE_LIMIT_OVERFLOW_MAX` per window, and past that
/// they are refused. Admitting them unmetered would let an attacker buy
/// unlimited throughput from fresh keys after paying to fill the map once.
#[tokio::test]
async fn keyless_clients_share_one_budget_once_the_map_is_saturated() {
    use hezarfen_backend::constant::{PURGE_AT, RATE_LIMIT_OVERFLOW_MAX};

    // One request per key exhausts it, so the map fills with buckets that can
    // neither be swept (same window) nor evicted (all at their limit).
    let limiter = UserRateLimiter::per_user_minute(1);
    for i in 0..PURGE_AT {
        assert!(limiter.enforce_user(&format!("flood-{i}")).is_ok());
    }
    assert!(
        limiter.enforce_user("flood-0").is_err(),
        "the map must be full of exhausted buckets"
    );

    // Distinct newcomers, none of which can get into the map.
    for i in 0..RATE_LIMIT_OVERFLOW_MAX {
        assert!(
            limiter.enforce_user(&format!("newcomer-{i}")).is_ok(),
            "newcomer {i} is within the shared overflow budget"
        );
    }
    assert!(
        limiter.enforce_user("newcomer-last").is_err(),
        "past the shared budget a keyless client must be refused, not admitted"
    );
}

// --- the window outlives the process ------------------------------------
//
// Two `UserRateLimiter`s over one database stand in for the process before and
// after a restart: two separate sets of in-memory buckets sharing only the
// `rate_limit` table, which is what stops a restart mid-window from handing
// every client a fresh budget. Time is paused, so a round of the sync task is
// driven by advancing past `RATE_SYNC_INTERVAL_SECS` — and because the task
// blocks on the database (not on time) mid-round, the short sleep afterwards
// cannot resolve until the round has finished. Assertions are on admissions,
// never on which limiter won a race: the embedded engine can drop one of two
// concurrent writes and still answer `Ok`.

use hezarfen_backend::constant::RATE_SYNC_INTERVAL_SECS;
use hezarfen_backend::database::Database;
use hezarfen_backend::rate_limit::UserRateLimiter;
use hezarfen_backend::state::DbHealth;
use std::time::Duration;

/// A migrated in-memory database, with the clock frozen only afterwards: the
/// embedded engine has internal deadlines of its own and cannot start up under
/// a paused clock ("Insert node failed after 5 attempts due to timeout").
async fn shared_db() -> Database {
    let db = database::init_mem().await.expect("in-memory db");
    tokio::time::pause();
    db
}

/// Let the sync task run one round: fire its timer, then hand it *real* time
/// to finish in. The real time is not optional — a round waits on the database,
/// and a paused clock auto-advances straight past a virtual sleep while it
/// does, so the assertions would race the round they are about.
async fn sync_round() {
    // Let a freshly spawned task reach its `sleep` first: `advance` moves the
    // clock *before* it yields, so a timer registered after it is registered
    // against the new now and would sit out the whole round.
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(RATE_SYNC_INTERVAL_SECS)).await;
    tokio::time::resume();
    tokio::time::sleep(Duration::from_millis(250)).await;
    tokio::time::pause();
}

/// One fixed wall window for every round below. The sync task reads the real
/// clock, so without this the tests here assert about "the" shared row while a
/// minute boundary landing between two limiters' rounds would give them a row
/// each — a run that passes because of when it ran, which is the same
/// accidental clock dependency that hid the epoch-roll double-charge from this
/// suite. Rolling *between* windows is covered by the unit tests, which drive
/// `sync_once`'s epoch directly.
const PINNED_WINDOW: i64 = 60_000;

/// Two limiters of one tier, sharing `db` and one wall window — the process
/// before a restart and the process after, which is the only way one deployment
/// runs two sets of buckets over one `rate_limit` row.
fn two_processes(max: u32, db: &Database) -> (UserRateLimiter, UserRateLimiter, DbHealth) {
    let health = DbHealth::default();
    let (a, b) = (
        UserRateLimiter::per_user_minute(max),
        UserRateLimiter::per_user_minute(max),
    );
    a.share_pinned("test", db.clone(), health.clone(), PINNED_WINDOW);
    b.share_pinned("test", db.clone(), health.clone(), PINNED_WINDOW);
    (a, b, health)
}

/// How many of `tries` requests a limiter admits for `user`.
fn admits(limiter: &UserRateLimiter, user: &str, tries: usize) -> usize {
    (0..tries)
        .filter(|_| limiter.enforce_user(user).is_ok())
        .count()
}

#[tokio::test]
async fn a_restarted_process_inherits_the_windows_spend() {
    let db = shared_db().await;
    let (a, b, _health) = two_processes(6, &db);

    // Each spends freely until its first sync — the accepted one-interval
    // overshoot, and the whole reason the shared row exists at all.
    let spent = admits(&a, "user:a", 6) + admits(&b, "user:a", 6);
    assert_eq!(spent, 12, "each process starts on its own local budget");

    // From the first sync on, the shared total is what binds: neither limiter
    // admits anything more in this window, whatever the interleaving was.
    sync_round().await;
    assert_eq!(
        admits(&a, "user:a", 6) + admits(&b, "user:a", 6),
        0,
        "the shared budget is spent"
    );

    // And the shared row agrees with what was actually admitted.
    let mut rows = db
        .query("SELECT VALUE hits FROM rate_limit")
        .await
        .expect("read shared counters");
    assert_eq!(rows.take::<Vec<i64>>(0).unwrap(), vec![spent as i64]);
}

#[tokio::test]
async fn a_process_that_never_admitted_still_learns_the_budget_is_gone() {
    let db = shared_db().await;
    let (a, b, _health) = two_processes(4, &db);

    // b spends one request, so it has a bucket to sync; a spends the rest.
    assert_eq!(admits(&b, "user:a", 1), 1);
    assert_eq!(admits(&a, "user:a", 3), 3);
    sync_round().await;

    assert_eq!(
        admits(&b, "user:a", 3),
        0,
        "the shared total, not b's own count, is the cap"
    );
    // Other users are untouched by a spent bucket.
    assert_eq!(admits(&b, "user:b", 4), 4);
}

#[tokio::test]
async fn a_down_database_leaves_each_process_on_its_local_budget() {
    let db = shared_db().await;
    let (a, b, health) = two_processes(3, &db);
    health.set(false);

    // Nothing is shared while the database is down — and nothing stalls: both
    // limiters keep serving their own budgets at full speed.
    for _ in 0..3 {
        sync_round().await;
        assert_eq!(admits(&a, "user:a", 1), 1);
        assert_eq!(admits(&b, "user:a", 1), 1);
    }
    assert_eq!(admits(&a, "user:a", 1), 0, "the local budget still binds");
    assert_eq!(admits(&b, "user:a", 1), 0);

    // Nothing was written, so the row the sync would have made does not exist.
    let mut rows = db
        .query("SELECT VALUE hits FROM rate_limit")
        .await
        .expect("read shared counters");
    assert_eq!(rows.take::<Vec<i64>>(0).unwrap(), Vec::<i64>::new());

    // Recovery needs no restart: the next round shares again.
    health.set(true);
    sync_round().await;
    let mut rows = db
        .query("SELECT VALUE hits FROM rate_limit")
        .await
        .expect("read shared counters");
    assert_eq!(rows.take::<Vec<i64>>(0).unwrap(), vec![6]);
}

/// Boot the app on a real TCP port, `ConnectInfo` wired exactly like `main`.
async fn spawn_server(rate_limit: RateLimitConfig) -> String {
    let tenants = database::init_mem_tenants()
        .await
        .expect("in-memory deployment");
    let app = build_router(AppState {
        db: tenants.control().clone(),
        tenants,
        files_path: tempfile::tempdir().expect("files dir").keep(),
        cookie_secure: false,
        rate_limit,
        chatbot_limit: Default::default(),
        exam_presence: Default::default(),
        board_hub: Default::default(),
        db_up: Default::default(),
        ai: None,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn real_connections_are_keyed_by_peer_address() {
    let base = spawn_server(RateLimitConfig {
        auth_per_minute: 2,
        api_per_minute: 0,
        trust_proxy: false,
    })
    .await;
    let client = reqwest::Client::new();

    // Every request arrives from 127.0.0.1. Spoofed X-Forwarded-For values
    // must not matter with TRUST_PROXY off: one peer, one bucket.
    for spoof in ["1.1.1.1", "2.2.2.2"] {
        let res = client
            .post(format!("{base}/auth/login"))
            .header("x-forwarded-for", spoof)
            .json(&json!({ "school": "demo", "username": "ghost", "password": "wrong-pass" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    let res = client
        .post(format!("{base}/auth/login"))
        .header("x-forwarded-for", "3.3.3.3")
        .json(&json!({ "school": "demo", "username": "ghost", "password": "wrong-pass" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry_after = res
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .expect("429 over real HTTP carries Retry-After");
    assert!((1..=60).contains(&retry_after));
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"], "too many requests");
}

#[tokio::test]
async fn a_limit_of_fifty_admits_exactly_fifty_requests() {
    // Production-scale boundary through the whole HTTP stack: all 50 budgeted
    // requests answer 200, the 51st answers 429 — no off-by-one at real sizes.
    let app = app_with(RateLimitConfig {
        auth_per_minute: 0,
        api_per_minute: 50,
        trust_proxy: true,
    })
    .await;

    for n in 1..=50 {
        let (status, _, _) = send_as(&app, "1.1.1.1", "GET", "/health", None).await;
        assert_eq!(status, StatusCode::OK, "request {n}/50 must pass");
    }

    let (status, body, retry_after) = send_as(&app, "1.1.1.1", "GET", "/health", None).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "request 51 must trip"
    );
    assert_eq!(body["error"], "too many requests");
    assert!(retry_after.is_some_and(|s| (1..=60).contains(&s)));

    // The 51st being rejected spent nothing extra: a fresh client still has
    // its own full budget.
    let (status, _, _) = send_as(&app, "2.2.2.2", "GET", "/health", None).await;
    assert_eq!(status, StatusCode::OK);
}

/// Every route, at the ACTUAL shipped limits: request `limit` passes, request
/// `limit + 1` answers 429. The route list is pulled from the app's own
/// OpenAPI document, so an endpoint added tomorrow is covered automatically;
/// `/` and the Swagger mount don't appear there and are appended by hand.
///
/// Each (method, path) pair poses as its own client IP, giving it a fresh
/// bucket. Requests carry no body — extractors reject them long after the
/// limiter has counted, which is exactly what makes hammering the credential
/// endpoints cheap (no argon2 runs for a body-less request).
#[tokio::test]
async fn every_route_enforces_the_shipped_limit_plus_one() {
    use hezarfen_backend::constant::{DEFAULT_API_RATE_LIMIT, DEFAULT_AUTH_RATE_LIMIT};

    let app = app_with(RateLimitConfig {
        auth_per_minute: DEFAULT_AUTH_RATE_LIMIT,
        api_per_minute: DEFAULT_API_RATE_LIMIT,
        trust_proxy: true,
    })
    .await;

    // Enumerate routes from the spec (fetched under a reserved IP so the
    // fetch itself doesn't spend any route's budget).
    let (status, spec, _) = send_as(&app, "254.0.0.1", "GET", "/api-docs/openapi.json", None).await;
    assert_eq!(status, StatusCode::OK);
    let paths = spec["paths"].as_object().expect("openapi paths object");

    let mut routes: Vec<(String, String)> = vec![
        ("GET".into(), "/".into()),
        ("GET".into(), "/swagger".into()),
    ];
    for (path, item) in paths {
        // "/notes/{id}" -> "/notes/id": a well-formed URL whose dummy segment
        // fails lookup in the handler, long after the limiter counted it.
        let concrete = path.replace(['{', '}'], "");
        for method in item.as_object().expect("path item").keys() {
            routes.push((method.to_uppercase(), concrete.clone()));
        }
    }
    assert!(
        routes.len() >= 25,
        "suspiciously few routes ({}) — OpenAPI enumeration broke?",
        routes.len()
    );

    for (i, (method, path)) in routes.iter().enumerate() {
        // One unique client per route (203.0.x.x is TEST-NET-3).
        let ip = format!("203.0.{}.{}", i / 200, (i % 200) + 1);
        // The strict tier owns the two credential endpoints; everything else
        // runs into the catch-all api tier.
        let limit = if path == "/auth/login" || path == "/auth/register" {
            DEFAULT_AUTH_RATE_LIMIT
        } else {
            DEFAULT_API_RATE_LIMIT
        };

        for n in 1..=limit {
            let (status, _, _) = send_as(&app, &ip, method, path, None).await;
            assert_ne!(
                status,
                StatusCode::TOO_MANY_REQUESTS,
                "{method} {path}: request {n}/{limit} must not be limited"
            );
        }
        let (status, _, retry_after) = send_as(&app, &ip, method, path, None).await;
        assert_eq!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "{method} {path}: request {}/{limit} must be limited",
            limit + 1
        );
        assert!(
            retry_after.is_some_and(|s| (1..=60).contains(&s)),
            "{method} {path}: 429 must carry a sane Retry-After"
        );
    }
}
