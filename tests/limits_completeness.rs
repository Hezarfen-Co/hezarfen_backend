//! The forcing function behind `GET /limits`.
//!
//! `src/web/limits.rs` is a hand-written mapping of `src/constant.rs` into a
//! JSON document. That is a mirror, and mirrors drift: the day someone adds a
//! constant, nothing about the compiler or the test suite would notice that
//! the endpoint promising "every fixed bound" quietly stopped being complete.
//! The frontend would then be back to guessing that one value — the exact
//! problem the endpoint exists to remove.
//!
//! So this test reads both files as text and asserts every `pub const` in
//! `constant.rs` is either referenced by `limits.rs` or listed below as a
//! deliberate exclusion with a reason. A new constant fails the suite until
//! someone makes that call consciously.
//!
//! Text, not reflection, because Rust cannot enumerate a module's constants at
//! runtime. The trade-off is that this proves a constant is *mentioned*, not
//! that it lands on a sensible JSON key — `integration::limits_publishes_the_
//! bounds_the_api_actually_enforces` covers the values themselves.

use std::collections::BTreeSet;
use std::fs;

/// Constants that deliberately stay out of `GET /limits`, each with the reason
/// it is not a client-facing bound. Adding a name here is a decision, not a
/// formality: it says a frontend has no business knowing this number.
const EXCLUDED: &[(&str, &str)] = &[
    // Server-internal operational timing. A client cannot act on any of these,
    // and publishing them would invite a frontend to build its own retry or
    // liveness logic against numbers we want free to change.
    (
        "DB_KEEPALIVE_INTERVAL_SECS",
        "database socket keepalive cadence",
    ),
    ("DB_PING_TIMEOUT_SECS", "database liveness probe deadline"),
    (
        "DB_CONNECT_BACKOFF_MAX_SECS",
        "boot reconnect backoff ceiling",
    ),
    ("CAS_UPDATE_RETRIES", "internal compare-and-set retry count"),
    (
        "CAP_WRITE_TRIES",
        "internal retry count for a contended cap counter",
    ),
    (
        "CAP_WRITE_BACKOFF_MS",
        "internal backoff between cap counter retries",
    ),
    (
        "SUBMISSION_OPEN_GUARD",
        "SQL fragment: the condition every homework-submission write carries so \
         a grade freezes it in the database; the client sees the freeze as a 409, \
         never as a number",
    ),
    (
        "FEE_PLAN_UNASSIGNED_GUARD",
        "SQL fragment: the condition a fee plan's edit and delete carry so an \
         assigned plan freezes in the database; the client sees the freeze as a \
         409, never as a number",
    ),
    (
        "UPLOAD_BODY_OVERHEAD_BYTES",
        "multipart headroom over max_file_bytes; the client sizes against \
         max_file_bytes itself, which /limits does publish",
    ),
    // The AI bridge is a backend↔AI-service contract over QUIC. No browser
    // ever speaks it; the services that do are configured out of band.
    ("AI_PROTOCOL", "QUIC bridge wire identifier"),
    ("AI_ALPN", "QUIC bridge ALPN"),
    ("AI_MAX_FRAME_BYTES", "QUIC bridge frame ceiling"),
    ("AI_DEFAULT_REQUEST_TIMEOUT_SECS", "AI inference deadline"),
    (
        "AI_MAX_REQUEST_TIMEOUT_SECS",
        "ceiling on the configured AI inference deadline",
    ),
    ("AI_MAX_CONCURRENT_PER_WORKER", "AI worker concurrency cap"),
    (
        "AI_DEFAULT_CONCURRENT_PER_WORKER",
        "AI worker concurrency default",
    ),
    ("AI_IDLE_TIMEOUT_SECS", "QUIC connection idle timeout"),
    ("AI_KEEPALIVE_SECS", "QUIC connection keepalive"),
    ("AI_HANDSHAKE_TIMEOUT_SECS", "AI service handshake deadline"),
    (
        "AI_CHAT_CAPABILITY",
        "capability string an AI service declares",
    ),
    (
        "CHAT_STREAM_POLL_MS",
        "server-side poll cadence behind the SSE stream",
    ),
    (
        "CHATBOT_PENDING_STALE_SECS",
        "when a stuck answer is declared failed",
    ),
    // Seed data for the settings singleton, not a bound. The live list is on
    // `GET /settings`, which is where a client must read it — publishing the
    // default too would invite rendering it instead of the school's actual one.
    (
        "DEFAULT_EXAM_KINDS",
        "settings seed; the live list is on GET /settings",
    ),
    (
        "DEFAULT_MEAL_SLOTS",
        "settings seed; the live list is on GET /settings",
    ),
    (
        "DEFAULT_DIETARY_TAGS",
        "settings seed; the live list is on GET /settings",
    ),
    // The three migration batches and their list moved to
    // `src/migration_sql.rs` — they are SurrealQL text, not bounds, and the
    // numeric sweep below still catches any bound that tries to follow them.
    ("USAGE_COUNTS_SQL", "one-statement bank usage-count query"),
    // Storage-side keys and literals. Each names a row, a column value or a
    // fold rule the server writes; none is a number a client is held to.
    ("SETTINGS_KEY", "the settings singleton's record key"),
    (
        "DECOY_PASSWORD",
        "login-decoy input, never a real credential",
    ),
    ("STALE_ERROR_CODE", "error_code a stuck answer projects as"),
    ("TEXT_FOLD_REPLACEMENTS", "server-side search folding table"),
    // Accepted-value lists that stay off /limits deliberately: unlike roles,
    // themes and languages (which the endpoint does publish), these are read
    // back from the resource itself — a question carries its status, a message
    // its folder — so a client never has to know the set up front.
    ("STATUS_PENDING", "pool-question lifecycle state"),
    ("STATUS_APPROVED", "pool-question lifecycle state"),
    ("POOL_QUESTION_STATUSES", "pool-question lifecycle states"),
    ("BANK_VISIBILITY_PRIVATE", "bank-question visibility value"),
    ("BANK_VISIBILITY_SCHOOL", "bank-question visibility value"),
    ("SENDER_FOLDERS", "folders a sender may file into"),
    ("RECIPIENT_FOLDERS", "folders a recipient may file into"),
    // Client-facing, but deliberately not compile-time contracts: these are
    // per-deployment defaults an operator overrides by environment variable.
    // `GET /limits` publishes the running server's ACTUAL tiers (from
    // AppState) in its `rate` group, which is strictly better than publishing
    // a default a deployment may not use.
    (
        "DEFAULT_AUTH_RATE_LIMIT",
        "env-tunable; live value served in /limits `rate`",
    ),
    (
        "DEFAULT_API_RATE_LIMIT",
        "env-tunable; live value served in /limits `rate`",
    ),
    (
        "DEFAULT_CHATBOT_RATE_LIMIT",
        "env-tunable; live value served in /limits `rate`",
    ),
    ("MILLIS_PER_DAY", "unit conversion, not a limit"),
    ("MILLIS_PER_WEEK", "unit conversion, not a limit"),
    (
        "MAX_ETAG_BODY_BYTES",
        "ETag middleware's own buffering ceiling",
    ),
    ("PURGE_AT", "rate-limiter bucket eviction threshold"),
    // How a tier's window is carried through a restart. A client is held to
    // the tier itself (published live in /limits `rate`), never to the cadence
    // it is reconciled at — publishing that would invite pacing against it.
    (
        "RATE_SYNC_INTERVAL_SECS",
        "how often a replica reconciles its counters",
    ),
    ("RATE_SYNC_TIMEOUT_SECS", "deadline on one sync round"),
    (
        "RATE_SYNC_MAX_KEYS",
        "client buckets one sync round carries",
    ),
    (
        "MAX_ERROR_CODE_LEN",
        "bounds a code the SERVER writes from an AI service's reply; no client sends it",
    ),
    ("REPLY_CHUNKS", "how the SSE stream slices an answer"),
    ("MIN_CHUNK_CHARS", "how the SSE stream slices an answer"),
];

/// A `*_TABLE` constant is a SurrealDB table name and a `*_FIELD` one a column
/// name, not a bound. Listing all 37 by hand would bury the deliberate
/// exclusions above in boilerplate, so the whole shape is excused once — the
/// naming convention *is* the decision.
fn is_storage_identifier(name: &str) -> bool {
    (name.ends_with("_TABLE") || name.ends_with("_FIELD"))
        && !["MAX_", "MIN_", "DEFAULT_"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// Every `pub const` declared in `src/constant.rs`.
fn declared_constants(source: &str) -> BTreeSet<String> {
    source
        .lines()
        .filter_map(|line| line.strip_prefix("pub const "))
        .filter_map(|rest| rest.split(&[':', ' '][..]).next())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// True when `limits.rs` mentions the constant as a whole word — the response
/// is built by naming each constant directly, so a mention is the reference.
fn references(source: &str, name: &str) -> bool {
    source.match_indices(name).any(|(at, _)| {
        let before = source[..at].chars().next_back();
        let after = source[at + name.len()..].chars().next();
        let boundary = |c: Option<char>| !matches!(c, Some(c) if c.is_alphanumeric() || c == '_');
        boundary(before) && boundary(after)
    })
}

#[test]
fn every_constant_is_published_or_deliberately_excluded() {
    let root = env!("CARGO_MANIFEST_DIR");
    let constants =
        fs::read_to_string(format!("{root}/src/constant.rs")).expect("read constant.rs");
    let limits = fs::read_to_string(format!("{root}/src/web/limits.rs")).expect("read limits.rs");

    let declared = declared_constants(&constants);
    assert!(
        declared.len() > 50,
        "parsed only {} constants — the `pub const` parse broke, not the mapping",
        declared.len()
    );

    let excluded: BTreeSet<&str> = EXCLUDED.iter().map(|(name, _)| *name).collect();
    let missing: Vec<&String> = declared
        .iter()
        .filter(|name| {
            !excluded.contains(name.as_str())
                && !is_storage_identifier(name)
                && !references(&limits, name)
        })
        .collect();

    assert!(
        missing.is_empty(),
        "these constants are in src/constant.rs but reach neither GET /limits nor the \
         EXCLUDED list in this test:\n  {}\n\nAdd each to src/web/limits.rs so clients \
         stop guessing it, or to EXCLUDED with the reason it is not client-facing.",
        missing
            .iter()
            .map(|name| name.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

/// Constants outside `constant.rs` that the sweep below may ignore — either
/// because they are not a client-facing bound at all, or because they are
/// published by a route that reads the *live* value instead of the constant.
///
/// Empty since the consolidation that made `constant.rs` the only home for any
/// constant: every former entry now lives there and is excused (or published)
/// through `EXCLUDED` above instead. A new name here is a claim that a bound
/// belongs somewhere else, which needs an argument.
const NOT_A_CLIENT_BOUND: &[(&str, &str)] = &[];

#[test]
fn validation_bounds_live_in_constant_rs() {
    // The completeness test above only guards `constant.rs`, so a bound
    // declared anywhere else is invisible to it — it reaches neither `/limits`
    // nor the OpenAPI spec, and no test notices. That is not hypothetical:
    // `MAX_CHATBOT_THREAD_TITLE_LEN` sat private in `domain/chatbot_thread.rs`
    // and escaped both surfaces until this sweep found it. This test makes the
    // constants file the only legal home for a client-facing bound.
    let root = env!("CARGO_MANIFEST_DIR");
    let allowed: BTreeSet<&str> = NOT_A_CLIENT_BOUND.iter().map(|(name, _)| *name).collect();

    let mut strays = Vec::new();
    let mut stack = vec![format!("{root}/src")];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path.to_string_lossy().into_owned());
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs") || path.ends_with("constant.rs") {
                continue;
            }
            let source = fs::read_to_string(&path).expect("read source");
            for line in source.lines() {
                let trimmed = line.trim_start();
                // Only `const NAME: <numeric> = <digit>` — a bound. Arrays of
                // accepted values (`ROLES`) and string constants are not.
                let Some(rest) = trimmed
                    .strip_prefix("pub const ")
                    .or_else(|| trimmed.strip_prefix("const "))
                else {
                    continue;
                };
                let Some((name, tail)) = rest.split_once(':') else {
                    continue;
                };
                let looks_numeric = ["usize", "i64", "u64", "i32", "u32", "u16", "u8", "f64"]
                    .iter()
                    .any(|ty| tail.trim_start().starts_with(ty));
                // `DEFAULT_` and the `_LIMIT`/`_CAP` suffixes matter as much as
                // the `MAX_`/`MIN_` prefixes: the rate-limit tiers are named
                // `DEFAULT_API_RATE_LIMIT`, and a sweep keyed only on prefixes
                // walked straight past a budget every client is held to.
                let bound_shaped = name.starts_with("MAX_")
                    || name.starts_with("MIN_")
                    || name.starts_with("DEFAULT_")
                    || name.ends_with("_LIMIT")
                    || name.ends_with("_CAP");
                if looks_numeric && bound_shaped && !allowed.contains(name) {
                    let file = path.strip_prefix(root).unwrap_or(&path);
                    strays.push(format!("  {name} in {}", file.display()));
                }
            }
        }
    }

    assert!(
        strays.is_empty(),
        "these validation bounds live outside src/constant.rs, so they reach neither \
         GET /limits nor the OpenAPI spec and no test guards them:\n{}\n\nMove each into \
         src/constant.rs (and map it in src/web/limits.rs), or add it to \
         NOT_A_CLIENT_BOUND with the reason it is not a client-facing limit.",
        strays.join("\n")
    );
}

#[test]
fn exclusions_are_real_constants() {
    // An exclusion for a deleted constant is dead weight that also silently
    // widens the allowlist if the name is ever reused for something public.
    let root = env!("CARGO_MANIFEST_DIR");
    let constants =
        fs::read_to_string(format!("{root}/src/constant.rs")).expect("read constant.rs");
    let declared = declared_constants(&constants);

    let stale: Vec<&str> = EXCLUDED
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| !declared.contains(*name))
        .collect();

    assert!(
        stale.is_empty(),
        "EXCLUDED names constants that no longer exist in src/constant.rs: {stale:?} — drop them"
    );
}
