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
        "REGISTRATION_FROZEN_GUARD",
        "SQL fragment: the condition the role cascade carries so a frozen signup \
         list is left exactly as it stands; the client sees the freeze as a 409, \
         never as a number",
    ),
    (
        "BOARD_REPLAY_CHUNK",
        "how the board-room socket batches its join replay; the client reads the \
         strokes, not the batch size",
    ),
    (
        "BOARD_HUB_CAPACITY",
        "depth of one board room's fan-out channel; a lagging socket is dropped \
         and rejoins, which the client handles as a reconnect, not as a number",
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
    (
        "AI_BLOB_WRITE_STALL_SECS",
        "how long a blob body may stall before its QUIC stream is reset",
    ),
    ("AI_HANDSHAKE_TIMEOUT_SECS", "AI service handshake deadline"),
    (
        "AI_API_ALLOWLIST",
        "the AI services' read scope; a service contract in README, and \
         publishing it would hand every browser the map",
    ),
    (
        "AI_CHAT_CAPABILITY",
        "capability string an AI service declares",
    ),
    (
        "AI_RAG_INDEX_CAPABILITY",
        "capability string an AI service declares",
    ),
    (
        "AI_RAG_INDEX_TIMEOUT_SECS",
        "deadline on a background course-note indexing round trip; no client \
         waits on it",
    ),
    (
        "AI_RAG_CHAT_CAPABILITY",
        "capability string an AI service declares",
    ),
    (
        "AI_RAG_CHAT_TIMEOUT_SECS",
        "deadline on one RAG ask round trip; the browser waits on its message \
         stream, never on this number",
    ),
    (
        "AI_RAG_SUMMARIZE_CAPABILITY",
        "capability string an AI service declares",
    ),
    (
        "AI_RAG_SUMMARIZE_TIMEOUT_SECS",
        "deadline on one one-shot summary of a range; the caller waits on the \
         artifact itself and acts on the door's 200/400/403/503, never on this \
         number",
    ),
    (
        "AI_RAG_QUESTIONS_CAPABILITY",
        "capability string an AI service declares",
    ),
    (
        "AI_RAG_QUESTIONS_TIMEOUT_SECS",
        "deadline on one one-shot question set over a range; the caller waits \
         on the artifact itself and acts on the door's 200/400/403/503, never \
         on this number",
    ),
    (
        "AI_INSIGHT_STUDENT_CAPABILITY",
        "capability string an AI service declares",
    ),
    (
        "AI_INSIGHT_STUDENT_TIMEOUT_SECS",
        "deadline on one student's compute; the dispatch is off the request \
         path and a reader polls the stored rows",
    ),
    (
        "AI_INSIGHT_CLASS_CAPABILITY",
        "capability string an AI service declares (declared for the contract; \
         no door dispatches it yet)",
    ),
    (
        "AI_INSIGHT_REFRESH_CAPABILITY",
        "capability string an AI service declares",
    ),
    (
        "AI_INSIGHT_REFRESH_TIMEOUT_SECS",
        "deadline on one school-wide recompute; the run's own ledger is \
         `zeka_run`, which the client polls",
    ),
    (
        "AI_INSIGHT_REPORT_CAPABILITY",
        "capability string an AI service declares",
    ),
    (
        "AI_INSIGHT_REPORT_TIMEOUT_SECS",
        "deadline on one school-level report render; the manager waits on the \
         artifact itself and a client acts on the door's 200/409/503, never on \
         this number",
    ),
    // The storage surface ZEKA's rows are written through (the service calls
    // these; the backend runs them). A browser never speaks the bridge, and
    // the HTTP doors that mirror them publish what a client can act on.
    (
        "AI_INSIGHT_SUMMARY_UPSERT_CAPABILITY",
        "capability string the backend serves to an AI service",
    ),
    (
        "AI_INSIGHT_RECOMMENDATION_UPSERT_CAPABILITY",
        "capability string the backend serves to an AI service",
    ),
    (
        "AI_INSIGHT_SEGMENT_UPSERT_CAPABILITY",
        "capability string the backend serves to an AI service",
    ),
    (
        "AI_INSIGHT_PROFILE_UPSERT_CAPABILITY",
        "capability string the backend serves to an AI service",
    ),
    (
        "AI_INSIGHT_RUN_UPSERT_CAPABILITY",
        "capability string the backend serves to an AI service",
    ),
    (
        "AI_INSIGHT_PENDING_LIST_CAPABILITY",
        "capability string the backend serves to an AI service",
    ),
    (
        "AI_INSIGHT_RETENTION_SWEEP_CAPABILITY",
        "capability string the backend serves to an AI service",
    ),
    (
        "AI_INSIGHT_DEPARTED_PURGE_CAPABILITY",
        "capability string the backend serves to an AI service",
    ),
    (
        "AI_INSIGHT_SCHOOLS_LIST_CAPABILITY",
        "capability string the backend serves to an AI service",
    ),
    (
        "MAX_INSIGHT_BATCH_ROWS",
        "rows per bridge call: the AI service's own batch size, enforced by the \
         doors as a 413 and by the bridge as `too_many_rows`; a client of \
         neither cannot act on it",
    ),
    (
        "MAX_INSIGHT_PENDING_STUDENTS",
        "students one bridge answer may carry; over it the call is refused \
         (`too_many_rows`) rather than clipped, so there is no client-side \
         value to size against",
    ),
    (
        "MAX_INSIGHT_PURGE_STUDENTS",
        "students one bridge purge call may name; the roster is the AI \
         service's own list, not something a browser assembles",
    ),
    (
        "MAX_INSIGHT_REFRESH_STUDENTS",
        "students one school-wide refresh may carry when its door fills the \
         roster itself; the refresh batch is the AI service's own work unit, \
         not a client-facing bound",
    ),
    (
        "AI_PODCAST_SUBMIT_CAPABILITY",
        "capability string an AI service declares",
    ),
    (
        "AI_PODCAST_REPORT_CAPABILITY",
        "capability string the backend serves; the AI service calls it",
    ),
    (
        "AI_PODCAST_CANCEL_CAPABILITY",
        "capability string an AI service declares",
    ),
    (
        "PODCAST_AUDIO_MAX_BYTES",
        "the largest episode the backend ingests from a service; a service-side \
         ingest bound a browser never assembles a body for",
    ),
    (
        "PODCAST_JOB_RETENTION_SECS",
        "how long a finished podcast job stays readable; the doors answer 410 \
         past it, which is the whole client-visible behavior — the number is \
         the operator's policy, not a value a client sizes against",
    ),
    (
        "PODCAST_JOB_STALE_FLOOR_SECS",
        "floor of the read-side staleness window a dead job is projected \
         failed in; a client reads the projected state, never sets this",
    ),
    (
        "PODCAST_JOB_STALE_ETA_FACTOR",
        "how many times its own ETA a podcast job may exceed before the read \
         side stops believing it runs; same as the floor — the projection is \
         the visible behavior",
    ),
    (
        "PODCAST_INTERRUPTED_CODE",
        "error_code value the backend writes for a job it concluded died; a \
         client reads it off the job, it is not a tunable",
    ),
    (
        "CHAT_STREAM_POLL_MS",
        "server-side poll cadence behind the SSE stream",
    ),
    (
        "CHATBOT_PENDING_STALE_SECS",
        "when a stuck answer is declared failed",
    ),
    (
        "RAG_PENDING_STALE_SECS",
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
    (
        "DEFAULT_GRADE_BANDS",
        "settings seed; the school's own bands are on GET /settings",
    ),
    // The school's zone is a settings *value*, not a bound: `GET /settings`
    // serves what this school runs on, and a client renders that, never a
    // default or the allow-list behind it. (`MAX_ABSENCE_DAYS` below is the
    // one settings bound this list still owes /limits.)
    (
        "DEFAULT_TIMEZONE",
        "settings fallback; the school's live zone is on GET /settings",
    ),
    (
        "TIMEZONES",
        "settings allow-list; the school's live zone is on GET /settings",
    ),
    // A settings field's own range (the office types an absence ceiling as a
    // number). It belongs beside `max_grade_bands` in the /limits settings
    // group — excluded here rather than silently, so the day a frontend needs
    // the range the decision is already written down.
    (
        "MAX_ABSENCE_DAYS",
        "settings field bound; /limits should carry it beside max_grade_bands",
    ),
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
    // Which folders a copy may be filed into is carried by the message itself
    // (its `folder` field), so the client reads the accepted set off the
    // resource it is acting on — never a public list, and /limits has no
    // message-folder group to place them in.
    (
        "SENDER_FOLDERS",
        "folders a sender may file their side into",
    ),
    (
        "RECIPIENT_FOLDERS",
        "folders a recipient may file their side into",
    ),
    ("BANK_VISIBILITY_PRIVATE", "bank-question visibility value"),
    ("BANK_VISIBILITY_SCHOOL", "bank-question visibility value"),
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
    (
        "DEFAULT_RAG_RATE_LIMIT",
        "env-tunable; live value served in /limits `rate`",
    ),
    ("MILLIS_PER_DAY", "unit conversion, not a limit"),
    ("MILLIS_PER_WEEK", "unit conversion, not a limit"),
    (
        "MAX_ETAG_BODY_BYTES",
        "ETag middleware's own buffering ceiling",
    ),
    ("PURGE_AT", "rate-limiter bucket eviction threshold"),
    // Not a per-client budget: one fleet-wide counter that only exists while
    // the bucket map is saturated. A client cannot compute its own allowance
    // from it, and publishing it would advertise the flood threshold.
    (
        "RATE_LIMIT_OVERFLOW_MAX",
        "aggregate budget for keyless clients while the bucket map is full",
    ),
    // How a tier's window is carried through a restart. A client is held to
    // the tier itself (published live in /limits `rate`), never to the cadence
    // it is reconciled at — publishing that would invite pacing against it.
    (
        "RATE_SYNC_INTERVAL_SECS",
        "how often the process folds its counters into the database",
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
    (
        "AI_PRINCIPAL_KEY",
        "record key of the in-memory AI service principal; a client can neither \
         assign nor address it",
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

/// Every `pub const` declared in `source`, in declaration order.
fn declared_constants(source: &str) -> BTreeSet<String> {
    source
        .lines()
        .filter_map(|line| line.strip_prefix("pub const "))
        .filter_map(|rest| rest.split(&[':', ' '][..]).next())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// True when `limits.rs` mentions the constant as a whole word in its *body* —
/// the response is built by naming each constant directly, so a mention there
/// is the reference. `use` lines are skipped: an import proves a name is in
/// scope, not that anything publishes it, and reading it as a reference would
/// let deleting a table from the response keep the guard green.
fn references(source: &str, name: &str) -> bool {
    source
        .lines()
        .filter(|line| !line.trim_start().starts_with("use "))
        .any(|line| {
            line.match_indices(name).any(|(at, _)| {
                let before = line[..at].chars().next_back();
                let after = line[at + name.len()..].chars().next();
                let boundary =
                    |c: Option<char>| !matches!(c, Some(c) if c.is_alphanumeric() || c == '_');
                boundary(before) && boundary(after)
            })
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

/// The accepted-value tables that moved out of `src/constant.rs` into their
/// domain modules (which is also their only legal home — `validation_bounds_
/// live_in_constant_rs` below bans the bound-shaped ones elsewhere, but an
/// array of enum values is neither a `MAX_`/`MIN_` bound nor numeric, so it
/// escapes that sweep). Each is either published by `GET /limits` or excused
/// with a reason; both are consumed by the test below.
const MOVED_TABLES: &[(&str, &str)] = &[
    ("src/domain/role.rs", "ROLES"),
    ("src/domain/preferences.rs", "THEMES"),
    ("src/domain/preferences.rs", "LANGUAGES"),
    ("src/domain/message.rs", "SENDER_FOLDERS"),
    ("src/domain/message.rs", "RECIPIENT_FOLDERS"),
    ("src/domain/badge.rs", "BADGES"),
];

#[test]
fn moved_value_tables_are_still_published_or_deliberately_excluded() {
    // The sweep above only reads `src/constant.rs`, so the day the accepted-
    // value tables moved into their domain modules they left its scan: nothing
    // then failed if a future edit dropped one from `src/web/limits.rs`. This
    // re-establishes that guarantee for the tables that moved — but for that
    // table list alone, not the whole modules, so the many server-side bounds
    // that also live there are not dragged in and do not each need an
    // EXCLUDED entry of their own.
    let root = env!("CARGO_MANIFEST_DIR");
    let limits = fs::read_to_string(format!("{root}/src/web/limits.rs")).expect("read limits.rs");
    let excluded: BTreeSet<&str> = EXCLUDED.iter().map(|(name, _)| *name).collect();

    let mut missing = Vec::new();
    for (module, name) in MOVED_TABLES {
        let source = fs::read_to_string(format!("{root}/{module}"))
            .unwrap_or_else(|_| panic!("read {module}"));
        assert!(
            declared_constants(&source).contains(*name),
            "{name} is no longer a `pub const` in {module} — MOVED_TABLES is stale"
        );
        if !excluded.contains(name) && !references(&limits, name) {
            missing.push(format!("  {name} ({module})"));
        }
    }

    assert!(
        missing.is_empty(),
        "these accepted-value tables left src/constant.rs and now reach neither GET /limits \
         nor the EXCLUDED list in this test:\n{}\n\nAdd each to src/web/limits.rs so clients \
         stop guessing it, or to EXCLUDED with the reason it is not client-facing.",
        missing.join("\n")
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
const NOT_A_CLIENT_BOUND: &[(&str, &str)] = &[(
    "MAX_CODE_LEN",
    "bound on the length of a podcast report's `stage`/`error_code` — values \
     the AI *service* writes onto the job row; a browser reads the stored \
     string back, it never sizes one against this",
)];

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
    // A name is real when it is declared in `src/constant.rs` or is one of the
    // accepted-value tables that moved into its domain module — the two homes
    // the completeness test above reads.
    let root = env!("CARGO_MANIFEST_DIR");
    let constants =
        fs::read_to_string(format!("{root}/src/constant.rs")).expect("read constant.rs");
    let mut declared = declared_constants(&constants);
    for (module, name) in MOVED_TABLES {
        let source = fs::read_to_string(format!("{root}/{module}"))
            .unwrap_or_else(|_| panic!("read {module}"));
        if declared_constants(&source).contains(*name) {
            declared.insert((*name).to_string());
        }
    }

    let stale: Vec<&str> = EXCLUDED
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| !declared.contains(*name))
        .collect();

    assert!(
        stale.is_empty(),
        "EXCLUDED names constants that no longer exist (neither in src/constant.rs nor in \
         the modules listed in MOVED_TABLES): {stale:?} — drop them"
    );
}
