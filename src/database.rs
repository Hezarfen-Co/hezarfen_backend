//! SurrealDB connection (WebSocket to a server; in-memory for tests) + schema.

use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::opt::auth::Root;

use crate::config::Config;
use crate::constant::{CAP_WRITE_BACKOFF_MS, CAP_WRITE_TRIES, CHATBOT_PENDING_STALE_SECS};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{Password, Username};
use crate::error::AppError;
use crate::migration_sql::{BACKFILL, CONTROL_MIGRATION_BATCHES, MIGRATION_BATCHES};
use crate::tenant::Tenants;

/// The shared database handle.
///
/// `Arc` is load-bearing, not decoration. `Surreal`'s own `Clone` mints a
/// fresh server-side session per clone (`Uuid::new_v4()` + a fire-and-forget
/// `SessionId::Clone` to the router, SDK `lib.rs:340`), and axum clones the
/// application state on every request — so a bare `Surreal<Any>` here means a
/// new session, and a matching `SessionId::Drop`, for each request served.
/// Under concurrency those lifecycle events race the queries riding on them:
/// sessions disappear ("Session not found"), and a session whose signin has
/// not been replayed answers "Anonymous access not allowed" / "Specify a
/// namespace" — the same strings [`crate::error::is_session_replay_error`]
/// treats as a transient reconnect, so the failures masquerade as one.
///
/// Cloning the `Arc` shares the single session established at boot instead.
pub type Database = std::sync::Arc<Surreal<Any>>;

/// Connect to the **control** database, sign in as root, apply the control
/// schema and seed the builder account. School databases are brought up
/// separately, one per school, by [`crate::tenant::Tenants`].
pub async fn init(cfg: &Config) -> Result<Tenants, AppError> {
    // Validated before the connection, not after: half a credential pair is a
    // deployment mistake, and reporting it only after a successful dial buries
    // it under an outage that isn't one.
    let builder = builder_credentials(cfg)?;
    let db = connect_with_retry(cfg).await;
    db.signin(Root {
        username: cfg.db_user.clone(),
        password: cfg.db_pass.clone(),
    })
    .await?;
    db.use_ns(cfg.db_ns.clone())
        .use_db(cfg.db_name.clone())
        .await?;
    let db = std::sync::Arc::new(db);
    migrate_control(&db).await?;
    if let Some((username, password)) = builder {
        crate::service::builder::ensure(&db, username, password).await?;
    }
    Ok(Tenants::new_remote(db, cfg))
}

/// The `BUILDER_USERNAME` / `BUILDER_PASSWORD` bootstrap pair, if configured.
/// Both-or-neither: half a pair aborts startup rather than silently running
/// without the seed — and without a builder, a fresh deployment has nobody who
/// can create the first school.
fn builder_credentials(cfg: &Config) -> Result<Option<(Username, Password)>, AppError> {
    match (&cfg.builder_username, &cfg.builder_password) {
        (Some(username), Some(password)) => Ok(Some((
            Username::try_new(username)
                .map_err(|err| AppError::Internal(format!("invalid BUILDER_USERNAME: {err}")))?,
            Password::try_new(password)
                .map_err(|err| AppError::Internal(format!("invalid BUILDER_PASSWORD: {err}")))?,
        ))),
        (None, None) => Ok(None),
        _ => Err(AppError::Internal(
            "BUILDER_USERNAME and BUILDER_PASSWORD must be set together".into(),
        )),
    }
}

/// The two ways a rival's claim shows up as an error on ours: our `CREATE` hit
/// their row, or the key-value layer aborted our transaction because theirs
/// touched the same key first.
///
/// Read from the *typed* error surface, not the message. Every conflict wording
/// the storage engines produce — surrealkv's "Transaction conflict: Write
/// conflict, retry the transaction. This transaction can be retried",
/// surrealmx's read-conflict variant, RocksDB's `Busy`/`TryAgain` — funnels
/// through one `KvsError::TransactionConflict` into
/// `QueryError::TransactionConflict` (surrealdb-core 3.2.3
/// `err/to_types.rs:295`, `kvs/err.rs:64-179`), and a duplicate record id into
/// `AlreadyExistsError::Record` (`to_types.rs:206`). So the two checks below
/// cover all of them, where matching substrings covered only whichever wording
/// the engine of the day happened to use.
///
/// The message fallback stays for one reason: these errors also travel over the
/// WebSocket, where the SDK rebuilds them from the wire (`Error::from_parts`),
/// and a server that omitted the typed details would otherwise turn a lost race
/// into a failed boot. Widest known wordings, cheap, and belt-and-braces.
pub(crate) fn lost_the_race(err: &surrealdb::Error) -> bool {
    use surrealdb::types::{ErrorDetails, QueryError};
    if err.is_already_exists()
        || matches!(
            err.details(),
            ErrorDetails::Query(Some(QueryError::TransactionConflict))
        )
    {
        return true;
    }
    let message = err.to_string();
    message.contains("already exists") || message.contains("can be retried")
}

/// Send one guarded single-statement write, re-sending it while the store
/// answers "conflict, retry", and hand back the rows it returned.
///
/// The guard on such a statement reads a column a rival writes, so the two
/// contend on one record *by design* — that contention is what makes the guard
/// atomic. Losing a round writes nothing at all, which makes re-sending the
/// whole of the recovery; without it an ordinary raced `PATCH` or `DELETE`
/// answers 500 instead of the 404 or 409 it owes. Only for statements that
/// cannot produce "already exists" (the other thing [`lost_the_race`] matches),
/// so `UPDATE` and `DELETE` and not `CREATE`.
pub(crate) async fn write_with_retry<T: surrealdb::types::SurrealValue>(
    db: &Database,
    sql: &str,
    bindings: &[(String, surrealdb::types::Value)],
) -> Result<Vec<T>, AppError> {
    let mut last = None;
    for attempt in 0..CAP_WRITE_TRIES {
        backoff(attempt).await;
        let mut query = db.query(sql);
        for (name, value) in bindings {
            query = query.bind((name.clone(), value.clone()));
        }
        match async { query.await?.check()?.take::<Vec<T>>(0) }.await {
            Ok(rows) => return Ok(rows),
            Err(err) if lost_the_race(&err) => last = Some(err),
            Err(err) => return Err(err.into()),
        }
    }
    Err(last
        .map(AppError::from)
        .unwrap_or_else(|| AppError::Internal("a guarded write never ran".into())))
}

/// Send one guarded `BEGIN…COMMIT` cascade, re-sending it while the store
/// answers "conflict, retry", and hand back both the response and the errors
/// drained off it.
///
/// [`write_with_retry`]'s story, for the cascades that cannot be one statement:
/// the guard reads a column a rival writes, so the two contend on one record by
/// design, and a lost round aborts the transaction having written *nothing* —
/// which is what makes re-sending the whole of the recovery.
///
/// Both halves come back because the caller needs both and `take_errors`
/// consumes: the response carries the result slots, and only the error map can
/// tell a deliberate `THROW` from a lost round. `refusals` are the caller's
/// `THROW` markers and they are matched *first* — a `THROW` is a decision, and
/// it outranks a conflict. Everything after that ordering is why this exists:
/// an aborted transaction errors *every* slot, and all but the failing one say
/// a generic "not executed", which is the consequence of the abort and never a
/// reason of its own. Picking one of those out of the unordered `HashMap` — or
/// taking the first via `check()` — retired a retryable round as a 500, measured
/// on `Course::delete` at 1-3 of 20 raced rounds.
///
/// [`write_with_retry`]'s restriction carries over, and a cascade makes it
/// easier to trip: no statement inside may be able to answer "already exists",
/// because that is the other thing [`lost_the_race`] matches and re-sending it
/// would just fail the same way until the tries run out — turning a 409 into a
/// 500, which is the exact defect this exists to prevent. The test to apply to
/// every statement in the batch is not which verb it uses but whether the store
/// could *legitimately* answer "already exists" to it — that answer is a
/// decision, and this loop cannot tell one from a conflict, so it would burn all
/// the tries re-asking a question already settled. `UPDATE` and `DELETE` never
/// can. A `CREATE` can, unless the id is unreachable by any rival: a freshly
/// generated ULID on a table with no `UNIQUE` index is such an id. An `UPSERT`
/// can, unless its id is *bijective* with every unique tuple its table indexes —
/// then the index entry can only ever point at the row the id already names, and
/// the write resolves onto it instead of colliding with it
/// ([`crate::domain::exam_result::ExamResult`]'s composite id is that shape).
/// A statement that fails this test belongs in front of the cascade, where its
/// 409 can be read as the answer it is.
pub(crate) async fn transaction_with_retry(
    db: &Database,
    sql: &str,
    bindings: &[(String, surrealdb::types::Value)],
    refusals: &[&str],
) -> Result<
    (
        surrealdb::IndexedResults,
        std::collections::HashMap<usize, surrealdb::Error>,
    ),
    AppError,
> {
    let mut last = None;
    for attempt in 0..CAP_WRITE_TRIES {
        backoff(attempt).await;
        let mut query = db.query(sql);
        for (name, value) in bindings {
            query = query.bind((name.clone(), value.clone()));
        }
        let mut result = match query.await {
            Ok(result) => result,
            Err(err) if lost_the_race(&err) => {
                last = Some(err);
                continue;
            }
            Err(err) => return Err(err.into()),
        };
        let mut errors = result.take_errors();
        let refused = |error: &surrealdb::Error| {
            let message = error.to_string();
            refusals.iter().any(|marker| message.contains(marker))
        };
        if errors.values().any(refused) || !errors.values().any(lost_the_race) {
            return Ok((result, errors));
        }
        last = errors.drain().map(|(_, error)| error).find(lost_the_race);
    }
    Err(last
        .map(AppError::from)
        .unwrap_or_else(|| AppError::Internal("a guarded cascade never ran".into())))
}

/// Wait out one lost round. Exponential with jitter, because racers arrive in
/// lockstep (one HTTP burst) and a fixed delay would just re-synchronize them.
/// Attempt zero waits not at all.
pub(crate) async fn backoff(attempt: usize) {
    if attempt == 0 {
        return;
    }
    let step = CAP_WRITE_BACKOFF_MS << (attempt - 1);
    let jitter = Timestamp::now().as_millis().unsigned_abs() % step.max(1);
    tokio::time::sleep(std::time::Duration::from_millis(step + jitter)).await;
}

/// Dial the database, retrying until it answers.
///
/// Never gives up, because giving up is worse than waiting: the database is
/// normally a sibling container booting in parallel, and a process that exits
/// on the first refusal gets restarted by the runtime, fails again in ~100ms,
/// and burns its whole restart budget inside a second — leaving the backend
/// permanently down over a startup skew that would have cleared on its own.
///
/// Only the dial is retried. Bad credentials or a broken migration still fail
/// hard, since no amount of waiting fixes those.
async fn connect_with_retry(cfg: &Config) -> Surreal<Any> {
    let mut backoff = 1;
    loop {
        match surrealdb::engine::any::connect(cfg.db_url.clone()).await {
            Ok(db) => return db,
            Err(err) => {
                tracing::warn!(
                    "database at {} unreachable ({err}) — retrying in {backoff}s",
                    cfg.db_url
                );
                tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(crate::constant::DB_CONNECT_BACKOFF_MAX_SECS);
            }
        }
    }
}

/// A fresh in-memory **school** database with the school schema applied. For
/// the unit tests whose subject is one school's own rows; a test that needs the
/// whole app (control database included) wants [`init_mem_tenants`].
pub async fn init_mem() -> Result<Database, AppError> {
    let db = surrealdb::engine::any::connect("memory").await?;
    db.use_ns("hezarfen").use_db("hezarfen").await?;
    migrate(&db).await?;
    Ok(std::sync::Arc::new(db))
}

/// A fresh in-memory deployment: an empty control database plus one school,
/// [`crate::tenant::DEMO_SLUG`]. The bootstrap every router-level test uses.
pub async fn init_mem_tenants() -> Result<Tenants, AppError> {
    let tenants = Tenants::new_mem().await?;
    let demo = crate::tenant::Slug::try_new(crate::tenant::DEMO_SLUG)
        .map_err(|err| AppError::Internal(format!("the demo slug is a slug: {err}")))?;
    tenants
        .create(&demo, "Demo School", crate::module::ModuleSet::all())
        .await?;
    Ok(tenants)
}

/// A handle on a **real** SurrealDB server, in a scratch namespace of its own
/// that is dropped and re-made on every call. For the tests whose subject is
/// the store's own conflict detection, which [`init_mem`]'s embedded engine
/// does not have: it drops one of two concurrent writes to a record and answers
/// `Ok` to both (see [`crate::db::cap`]), which *forges* the very
/// integrity failure those tests exist to catch — measured 2026-07-30 at 2
/// failed runs in 36 on `memory`, against 0 in 10 000 rounds here.
///
/// Every caller is `#[ignore]`d, so a machine with no server prints them as
/// `ignored` rather than passing: this is not a test that may quietly skip.
/// `HEZARFEN_TEST_DB` overrides the address; the credentials are `compose.yaml`'s.
///
/// The guard handed back with the handle serializes these tests against each
/// other and must be held for the whole test — hence the tuple, which cannot be
/// forgotten the way a separate `lock()` line can. `cargo test -- --ignored`
/// runs them in parallel, and while each has a namespace to itself they all
/// burst against the *one* server: their beats are sub-millisecond, so a
/// sibling's burst pushes a delete clean out of the window it is meant to land
/// in (measured 4 runs in 4 at 3 of 6 failing their own "the race was reached"
/// guard). Serialized, the plain documented command works.
#[cfg(test)]
mod builder_seed_tests {
    use super::builder_credentials;
    use crate::config::Config;

    fn cfg(username: Option<&str>, password: Option<&str>) -> Config {
        let mut cfg = Config::from_env();
        cfg.builder_username = username.map(str::to_string);
        cfg.builder_password = password.map(str::to_string);
        cfg
    }

    #[tokio::test]
    async fn the_builder_seed_is_both_or_neither() {
        assert!(builder_credentials(&cfg(None, None)).unwrap().is_none());
        assert!(
            builder_credentials(&cfg(Some("builder"), Some("secret1")))
                .unwrap()
                .is_some()
        );
        // Half a pair aborts startup: a deployment with no builder has nobody
        // who can create the first school, and a silent run would hide that.
        assert!(builder_credentials(&cfg(Some("builder"), None)).is_err());
        assert!(builder_credentials(&cfg(None, Some("secret1"))).is_err());
        // A pair that is set but invalid aborts too, naming the variable.
        match builder_credentials(&cfg(Some("!"), Some("secret1"))) {
            Err(err) => assert!(err.to_string().contains("BUILDER_USERNAME"), "{err}"),
            Ok(_) => panic!("an invalid BUILDER_USERNAME must abort startup"),
        }
    }
}

#[cfg(test)]
pub(crate) static RACE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
pub(crate) async fn init_test_server(
    scratch: &str,
) -> (Database, tokio::sync::MutexGuard<'static, ()>) {
    let serialized = RACE_LOCK.lock().await;
    let url =
        std::env::var("HEZARFEN_TEST_DB").unwrap_or_else(|_| "ws://127.0.0.1:8000".to_string());
    let db = surrealdb::engine::any::connect(url.clone())
        .await
        .unwrap_or_else(|err| panic!("no SurrealDB server at {url} ({err})"));
    db.signin(Root {
        username: "root".into(),
        password: "root".into(),
    })
    .await
    .expect("sign in to the test server");
    let ns = format!("test_{scratch}");
    db.query(format!("REMOVE NAMESPACE IF EXISTS {ns}"))
        .await
        .expect("clear the scratch namespace")
        .check()
        .expect("clear the scratch namespace");
    db.use_ns(ns.clone()).use_db(ns).await.expect("scratch ns");
    let db = std::sync::Arc::new(db);
    migrate(&db).await.expect("migrate the scratch namespace");
    (db, serialized)
}

/// Apply the schema + backfills. Idempotent — `init` runs it on every boot,
/// and tests re-run it on a live handle to simulate a second boot.
pub async fn migrate(db: &Surreal<Any>) -> Result<(), AppError> {
    for sql in MIGRATION_BATCHES {
        let mut query = db.query(sql);
        for (name, value) in migration_binds() {
            query = query.bind((name, value));
        }
        query.await?.check()?;
    }
    Ok(())
}

/// Apply the **control** schema (schools, builders, the shared rate-limit
/// window). Idempotent, like [`migrate`].
pub async fn migrate_control(db: &Surreal<Any>) -> Result<(), AppError> {
    // The one data-driven parameter in this batch: the module list a school
    // written before entitlements existed is backfilled to.
    let all_modules = crate::module::ModuleSet::all().names();
    for sql in CONTROL_MIGRATION_BATCHES {
        db.query(sql)
            .bind(("all_modules", all_modules.clone()))
            .await?
            .check()?;
    }
    Ok(())
}

/// Parameters the batches are executed with. Bound, not baked into the SQL
/// string: a `const` cannot be interpolated into another `const`, and a
/// hand-copied 300000 would drift silently. The one-time blocks' fingerprints
/// ride along — see [`marked_blocks`]. Bound to all three batches, which is
/// free: an unused parameter is not an error.
fn migration_binds() -> Vec<(String, i64)> {
    let mut binds = vec![("stale_ms".to_string(), CHATBOT_PENDING_STALE_SECS * 1_000)];
    binds.extend(marked_blocks());
    binds
}

/// The line every one-time block in [`BACKFILL`] opens with. The name straight
/// after it is the block's `migration_mark` key, and `fp_<name>` is the
/// parameter its fingerprint is bound to — so a block written in this idiom is
/// found, hashed and bound with nothing to keep by hand. A block written any
/// other way silently gets no fingerprint, which is what
/// `every_one_time_block_is_fingerprinted` exists to catch.
const MARK_GATE: &str = "IF array::len((SELECT VALUE id FROM migration_mark:";

/// Every one-time block in [`BACKFILL`], as (`fp_<name>`, hash of its own SQL).
///
/// A `migration_mark` used to say only *that* a block had run, which made a
/// marked block's content permanently uncorrectable: an edit to it never ran on
/// any volume that had booted once. The mark now stores this number and the
/// gate compares it against the number this binary computes, so an edited block
/// runs one more time and re-stamps. There is nothing to remember to bump — the
/// hash comes from the very string the server executes, which is the whole
/// point: a hand-kept version is the same discipline failure in a new place.
///
/// Hashed from the gate through the stamp with comments and indentation
/// dropped: every re-run costs something (the `profile_counters` block says
/// what, in its own comment), so re-wording a comment must not trigger one.
fn marked_blocks() -> Vec<(String, i64)> {
    let mut blocks = Vec::new();
    let mut rest = BACKFILL;
    while let Some(at) = rest.find(MARK_GATE) {
        let block = &rest[at..];
        let name: String = block[MARK_GATE.len()..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        let stamp = format!("UPSERT migration_mark:{name}");
        let end = block
            .find(&stamp)
            .and_then(|from| block[from..].find(';').map(|to| from + to + 1))
            .unwrap_or_else(|| panic!("the `{name}` block never stamps its own mark"));
        blocks.push((format!("fp_{name}"), block_fingerprint(&block[..end])));
        rest = &block[end..];
    }
    blocks
}

/// FNV-1a over a block's statements, comments and layout dropped.
///
/// Hand-rolled rather than [`std::hash`]: this number is *stored*, so it has to
/// mean the same thing after a compiler upgrade, and `DefaultHasher` promises
/// nothing across versions — a hash that drifted on its own would re-run every
/// one-time block on every volume.
fn block_fingerprint(block: &str) -> i64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for line in block
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("--"))
    {
        for byte in line.bytes().chain(std::iter::once(b'\n')) {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash as i64
}

#[cfg(test)]
mod tests {

    /// The fingerprint has to move when a *statement* moves and stay put when
    /// prose does: a re-run is never free (see the `profile_counters` comment),
    /// and a fingerprint that ignored an edit would put us back where this
    /// mechanism started — a marked block nobody can correct.
    #[test]
    fn a_block_fingerprint_tracks_statements_not_prose() {
        let block = "IF x = 0 {\n    -- why this runs\n    UPDATE t SET a = 1;\n};";
        let reworded = "IF x = 0 {\n        -- why this runs, at length\n  UPDATE t SET a = 1;\n};";
        let edited = "IF x = 0 {\n    -- why this runs\n    UPDATE t SET a = 2;\n};";
        assert_eq!(
            super::block_fingerprint(block),
            super::block_fingerprint(reworded),
            "a re-worded comment or a re-indent must not re-run a one-time block"
        );
        assert_ne!(
            super::block_fingerprint(block),
            super::block_fingerprint(edited),
            "an edited statement must re-run its block"
        );
    }

    /// The forcing function: a one-time block written outside the gate idiom
    /// gets no fingerprint parameter, and its `$fp_…` would bind to nothing — a
    /// gate that then matches every mark and skips forever. Nothing about the
    /// compiler notices, so this does.
    #[test]
    fn every_one_time_block_is_fingerprinted() {
        let blocks = super::marked_blocks();
        let names: Vec<&str> = blocks.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            ["fp_board_roster", "fp_profile_counters"],
            "{names:?}"
        );
        assert_ne!(blocks[0].1, blocks[1].1, "two blocks, two fingerprints");
        // Two mentions of the mark table per block — its gate and its stamp. A
        // third, or a block gated some other way, means a block the binder
        // never saw.
        assert_eq!(
            super::BACKFILL.matches("migration_mark:").count(),
            blocks.len() * 2,
            "a one-time block outside the `{}…` idiom is invisible to the binder",
            super::MARK_GATE
        );
        for (param, _) in &blocks {
            assert!(
                super::BACKFILL.contains(&format!("${param}")),
                "nothing reads ${param}"
            );
        }
    }

    /// The fingerprint the mark for `name` must carry once its block has run.
    fn expected_fingerprint(name: &str) -> i64 {
        super::marked_blocks()
            .into_iter()
            .find(|(param, _)| *param == format!("fp_{name}"))
            .unwrap_or_else(|| panic!("no one-time block named {name}"))
            .1
    }

    async fn stamped_fingerprint(db: &super::Database, name: &str) -> Vec<Option<i64>> {
        let mut rows = db
            .query(format!(
                "SELECT VALUE fingerprint FROM migration_mark:{name}"
            ))
            .await
            .unwrap();
        rows.take(0).unwrap()
    }

    /// The admin seed must not be interruptible into a state no later boot can
    /// repair. `create` + `set_role` was: killed between the two writes it
    /// leaves a *student* row holding ADMIN_USERNAME, which `ensure_admin` then
    /// refuses to promote on every subsequent boot (rightly — it cannot tell
    /// that row from a stranger who registered the name first). The row is
    /// therefore minted with its role in one statement, and this pins that.
    #[test]
    fn the_admin_seed_mints_the_role_with_the_row() {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/service/user.rs"))
                .expect("read service/user.rs");
        let start = source
            .find("pub async fn ensure_admin")
            .expect("ensure_admin is declared");
        let body = &source[start..start + source[start..].find("\n    }\n").expect("body ends")];
        assert!(
            !body.contains("set_role"),
            "ensure_admin promotes after creating; an interruption between those two writes \
             leaves a student account on ADMIN_USERNAME that no boot can heal:\n{body}"
        );
        assert!(
            body.contains("create_with_role"),
            "the seed must mint the row with Role::Admin in one statement"
        );
    }

    #[tokio::test]
    async fn boot_fails_chatbot_messages_left_pending() {
        let db = super::init_mem().await.unwrap();
        db.query(
            "CREATE user:u SET username = 'u', password_hash = 'x';
             CREATE chatbot_thread:c SET user_id = user:u, created_at = 1, updated_at = 1;
             CREATE chatbot_message:m SET thread_id = chatbot_thread:c, user_id = user:u,
                 role = 'assistant', content = '', status = 'pending', created_at = 1;
             CREATE chatbot_message:done SET thread_id = chatbot_thread:c, user_id = user:u,
                 role = 'assistant', content = 'hi', status = 'complete', created_at = 1;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

        // A second boot: the task that would have answered `m` died with the
        // previous process, so the row is failed rather than left waiting.
        super::migrate(&db).await.unwrap();

        let mut rows = db
            .query("SELECT id, status, error_code, completed_at FROM chatbot_message ORDER BY id")
            .await
            .unwrap();
        let rows: Vec<serde_json::Value> = rows.take(0).unwrap();
        // An already-answered turn is untouched; the pending one is failed.
        let done = &rows[0];
        let interrupted = &rows[1];
        assert_eq!(done["status"], "complete");
        assert_eq!(done["error_code"], serde_json::Value::Null);
        assert_eq!(interrupted["status"], "failed");
        assert_eq!(interrupted["error_code"], "interrupted");
        assert!(interrupted["completed_at"].as_i64().unwrap() > 0);
    }

    /// Stale-data path for the 2026-07-30 fee-plan refcount. A dev volume
    /// carries plans and assignments written before the counter column existed,
    /// and an absent counter reads as **zero** — which is exactly the value
    /// that licenses an edit and a delete. Unseeded, every already-assigned
    /// plan on every existing volume would go editable and deletable again, so
    /// the backfill is the whole of the stale-data story, not a nicety.
    #[tokio::test]
    async fn a_plan_assigned_before_the_counter_existed_is_still_refused() {
        use crate::db::fee_plan;
        use crate::domain::fee_plan::{FeePlanId, FeePlanName};
        use crate::error::AppError;

        let db = super::init_mem().await.unwrap();
        // The old shape: rows stating every column the older binary knew, and
        // no `assignment_count` at all. `used` carries an assignment, `free`
        // does not — a plan nobody is on must stay editable.
        db.query(
            "CREATE user:m SET username = 'm', password_hash = 'x';
             CREATE user:s SET username = 's', password_hash = 'x';
             CREATE fee_plan:used SET name = 'Yearly',
                 installments = [{ amount_minor: 100, due_at: 1 }],
                 created_by = user:m, created_at = 1;
             CREATE fee_plan:free SET name = 'Unused',
                 installments = [{ amount_minor: 100, due_at: 1 }],
                 created_by = user:m, created_at = 1;
             CREATE fee_plan_assignment:used_s SET plan = fee_plan:used,
                 student = user:s, assigned_by = user:m, created_at = 1;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

        // Twice: every boot runs the backfill, and a second pass must not
        // double a counter it already seeded.
        super::migrate(&db).await.unwrap();
        super::migrate(&db).await.unwrap();

        let mut counts = db
            .query("SELECT VALUE assignment_count FROM fee_plan ORDER BY id")
            .await
            .unwrap();
        let counts: Vec<Option<i64>> = counts.take(0).unwrap();
        // `free` before `used` by id; the unused plan is deliberately left
        // absent, which already reads as zero in the guard.
        assert_eq!(counts, vec![None, Some(1)]);

        // The load-bearing half: the seeded plan is frozen, and the untouched
        // one is not.
        let used = fee_plan::read(&db, &FeePlanId::from_key("used"))
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            fee_plan::update(
                &db,
                used.clone(),
                Some(FeePlanName::try_new("Edited").unwrap()),
                None
            )
            .await,
            Err(AppError::Conflict(_))
        ));
        assert!(!fee_plan::delete(&db, used).await.unwrap());
        let free = fee_plan::read(&db, &FeePlanId::from_key("free"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            fee_plan::update(
                &db,
                free.clone(),
                Some(FeePlanName::try_new("Edited").unwrap()),
                None
            )
            .await
            .is_ok()
        );
        assert!(fee_plan::delete(&db, free).await.unwrap());
    }

    /// Stale-data path for the 2026-08-02 board-roster repair. `set_role`
    /// sweeps a demoted user off every board going forward, but an existing
    /// volume still names parents on rosters — and ids of users deleted before
    /// any sweep existed. Both freeze the creator's roster `PATCH` at a 400
    /// (see `web::boards::resolve_participants`), so boot has to clean them.
    #[tokio::test]
    async fn a_board_roster_written_before_the_sweep_is_repaired_at_boot() {
        let db = super::init_mem().await.unwrap();
        db.query(
            "CREATE user:s SET username = 's', password_hash = 'x', role = 'student';
             CREATE user:p SET username = 'p', password_hash = 'x', role = 'parent';
             CREATE board:keep SET creator = user:s, title = 'B',
                 participants = [user:s, user:p, user:ghost], created_at = 1;
             CREATE board:gone SET creator = user:s, title = 'C',
                 participants = [user:p], created_at = 1;
             CREATE board:clean SET creator = user:s, title = 'D',
                 participants = [user:s], created_at = 1;
             DELETE migration_mark:board_roster;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
        // `init_mem` boots a *migrated* database, so it has already marked the
        // repair done on an empty store — which is right, and is exactly why the
        // mark is dropped here: a volume written before the mark existed carries
        // stale rosters and no mark at all, and that is the state under test.
        // A write probe on the table under repair: the event fires *inside* the
        // UPDATE, so it counts real writes rather than trusting a re-read.
        db.query(
            "DEFINE TABLE board_write_probe SCHEMALESS;
             DEFINE EVENT board_write ON board WHEN $event = 'UPDATE' THEN {
                 UPSERT type::record('board_write_probe', 'n') SET n = (n ?? 0) + 1;
             };",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

        super::migrate(&db).await.unwrap();

        async fn rosters(db: &super::Database) -> Vec<Vec<surrealdb::types::RecordId>> {
            let mut rows = db
                .query("SELECT VALUE participants FROM board ORDER BY id")
                .await
                .unwrap();
            rows.take(0).unwrap()
        }
        let after_first = rosters(&db).await;
        let s = surrealdb::types::RecordId::new("user", "s");
        // `clean`, `gone`, `keep` by id. The parent and the vanished user are
        // both gone; a board whose whole roster was stale ends empty, which is
        // the same shape a board opened without invites carries.
        assert_eq!(
            after_first,
            vec![vec![s.clone()], vec![], vec![s.clone()]],
            "{after_first:?}"
        );

        async fn writes(db: &super::Database) -> Vec<i64> {
            let mut rows = db
                .query("SELECT VALUE n FROM board_write_probe:n")
                .await
                .unwrap();
            rows.take(0).unwrap()
        }
        assert_eq!(
            writes(&db).await,
            vec![2],
            "only the two stale rows repaired"
        );

        // The second boot: every boot runs the backfill, and a converged row
        // must not be written again.
        super::migrate(&db).await.unwrap();
        assert_eq!(rosters(&db).await, after_first);
        assert_eq!(writes(&db).await, vec![2], "the second pass wrote nothing");

        // …and it did not *look*, either, which is the whole point of the mark:
        // the repair's cost is a `user` table scan per board, and writing
        // nothing is not the same as scanning nothing. A roster made stale after
        // the mark landed is the probe — it survives the next boot untouched,
        // which no converging `WHERE` could produce.
        let mut mark = db
            .query("SELECT VALUE done_at FROM migration_mark:board_roster")
            .await
            .unwrap();
        assert_eq!(
            mark.take::<Vec<i64>>(0).unwrap().len(),
            1,
            "the repair marks itself finished"
        );
        // …with the fingerprint of the text it ran, which is what a later edit
        // to this block is compared against.
        assert_eq!(
            stamped_fingerprint(&db, "board_roster").await,
            vec![Some(expected_fingerprint("board_roster"))]
        );
        db.query("CREATE board:late SET creator = user:s, title = 'E', participants = [user:p], created_at = 1")
            .await
            .unwrap()
            .check()
            .unwrap();
        super::migrate(&db).await.unwrap();
        assert_eq!(
            rosters(&db).await,
            vec![
                vec![s.clone()],
                vec![],
                vec![s.clone()],
                vec![surrealdb::types::RecordId::new("user", "p")]
            ],
            "a marked database skips the scan entirely"
        );
        assert_eq!(writes(&db).await, vec![2]);
    }

    /// Stale-data path for the 2026-08-04 badge counters. Every account on an
    /// existing volume predates the columns, and a badge is decided off the
    /// stored total — so a zero start would not lose a number, it would
    /// permanently mis-award the student who really did the work. Boot seeds the
    /// counters from the rows that exist, exactly once.
    #[tokio::test]
    async fn the_badge_counters_are_seeded_from_history_exactly_once() {
        let db = super::init_mem().await.unwrap();
        db.query(
            "CREATE user:a SET username = 'a', password_hash = 'x', role = 'student';
             CREATE user:b SET username = 'b', password_hash = 'x', role = 'student';
             CREATE user:c SET username = 'c', password_hash = 'x', role = 'student';
             CREATE homework:h SET course = course:c, subject = subject:s, title = 'H',
                 due_at = 100, created_by = user:a, created_at = 1;
             -- Three on time (the equal case counts as on time) and one whose
             -- homework was deleted out from under it: that last one is
             -- submitted but not on time, because the traversal yields nothing.
             CREATE homework_submission:s1 SET homework = homework:h, user = user:a,
                 submitted_at = 50, updated_at = 50;
             CREATE homework_submission:s2 SET homework = homework:h, user = user:a,
                 submitted_at = 60, updated_at = 100;
             -- Handed in before the deadline, then edited well after it — a
             -- `touch()` from a single file add does this. On time, because the
             -- live path credits the hand-in and debits by `submitted_at`; if
             -- the seed judged this one by `updated_at` a later withdrawal
             -- would decrement a credit that was never given.
             CREATE homework_submission:s3 SET homework = homework:h, user = user:a,
                 submitted_at = 70, updated_at = 200;
             CREATE homework_submission:s4 SET homework = homework:gone, user = user:a,
                 submitted_at = 80, updated_at = 1;
             CREATE exam_attempt:e1 SET exam = exam:x, user = user:a, seq = 1, started_at = 1;
             CREATE exam_attempt:e2 SET exam = exam:y, user = user:a, seq = 1, started_at = 2;
             -- Two finished stints and one still open, which counts for neither.
             CREATE pomodoro_session:p1 SET user = user:a, started_at = 1000, finished_at = 2000;
             CREATE pomodoro_session:p2 SET user = user:a, started_at = 5000, finished_at = 7500;
             CREATE pomodoro_session:p3 SET user = user:a, started_at = 9000;
             -- One real stint beside one written with a backwards clock. The
             -- floor is per row, like the live close, so the negative one
             -- contributes zero and the real 1000ms survives; flooring the
             -- per-user sum instead would seed 0 and lose the real stint.
             CREATE pomodoro_session:p4 SET user = user:c, started_at = 0, finished_at = 1000;
             CREATE pomodoro_session:p5 SET user = user:c, started_at = 9000, finished_at = 3000;
             DELETE migration_mark:profile_counters;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
        // `init_mem` boots a *migrated* database, so the seed is already marked
        // done on an empty store — which is right, and is exactly why the mark is
        // dropped here: a volume written before the columns existed carries
        // history and no mark at all, and that is the state under test.
        // A write probe on the table being seeded: the event fires *inside* the
        // UPDATE, so it counts real writes rather than trusting a re-read.
        db.query(
            "DEFINE TABLE user_write_probe SCHEMALESS;
             DEFINE EVENT user_write ON user WHEN $event = 'UPDATE' THEN {
                 UPSERT type::record('user_write_probe', 'n') SET n = (n ?? 0) + 1;
             };",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

        super::migrate(&db).await.unwrap();

        async fn counters(db: &super::Database) -> Vec<(i64, i64, i64, i64, i64)> {
            let mut rows = db
                .query(
                    "SELECT VALUE [homework_submitted_total, homework_on_time_total,
                                   exam_sat_total, pomodoro_finished_total,
                                   pomodoro_focus_ms_total]
                     FROM user ORDER BY id",
                )
                .await
                .unwrap();
            rows.take::<Vec<Vec<i64>>>(0)
                .unwrap()
                .into_iter()
                .map(|row| (row[0], row[1], row[2], row[3], row[4]))
                .collect()
        }
        let after_first = counters(&db).await;
        // `a`, `b`, `c` by id. `b` has no history at all and reads as a fresh
        // account; `c` keeps the 1000ms real stint the -6000ms one sits beside.
        assert_eq!(
            after_first,
            vec![(4, 3, 2, 2, 3500), (0, 0, 0, 0, 0), (0, 0, 0, 2, 1000)],
            "{after_first:?}"
        );

        // The seed also stamps each submission with the verdict it was counted
        // under, because `due_at` is mutable: a withdrawal after the teacher
        // moves the deadline must debit what was credited, not what the
        // deadline says by then. Asserted through `WHERE` rather than read back
        // as a column, so an unstamped row would show up as belonging to
        // neither set instead of deserializing as some default.
        async fn stamped(db: &super::Database, cond: &str) -> Vec<String> {
            let mut rows = db
                .query(format!(
                    "SELECT VALUE record::id(id) FROM homework_submission
                     WHERE {cond} ORDER BY id"
                ))
                .await
                .unwrap();
            rows.take(0).unwrap()
        }
        // s1/s2/s3 beat the deadline (50, 60, 70 vs 100). s4 hangs off a
        // deleted homework, so `submitted_at <= homework.due_at` compares
        // against NONE and lands on `false` — which is exactly what the count
        // above credited it as, so the stamp agrees with the counter rather
        // than leaving the row to a fallback that could later disagree.
        assert_eq!(
            stamped(&db, "counted_on_time = true").await,
            ["s1", "s2", "s3"]
        );
        assert_eq!(stamped(&db, "counted_on_time = false").await, ["s4"]);

        async fn writes(db: &super::Database) -> Vec<i64> {
            let mut rows = db
                .query("SELECT VALUE n FROM user_write_probe:n")
                .await
                .unwrap();
            rows.take(0).unwrap()
        }
        let first_writes = writes(&db).await;
        // Pinned non-zero, not just compared: "the second pass wrote nothing"
        // would hold vacuously if the probe itself stopped firing, and then the
        // test would pass on a seed that never ran.
        assert!(
            matches!(first_writes.as_slice(), [n] if *n > 0),
            "{first_writes:?}"
        );

        // The second boot: every boot runs the backfill, and a recount here
        // would not converge the way the counters above it do — these columns
        // are maintained by the request path from now on, so a second pass would
        // overwrite everything earned since. It must not write at all…
        super::migrate(&db).await.unwrap();
        assert_eq!(counters(&db).await, after_first);
        assert_eq!(
            writes(&db).await,
            first_writes,
            "the second pass wrote nothing"
        );

        // …and it must not *look*, either, which is the whole point of the mark:
        // writing nothing is not the same as scanning nothing. A submission that
        // lands after the mark is the probe — its counter stays where the request
        // path left it, which no recomputing backfill could produce.
        let mut mark = db
            .query("SELECT VALUE done_at FROM migration_mark:profile_counters")
            .await
            .unwrap();
        assert_eq!(
            mark.take::<Vec<i64>>(0).unwrap().len(),
            1,
            "the seed marks itself finished"
        );
        assert_eq!(
            stamped_fingerprint(&db, "profile_counters").await,
            vec![Some(expected_fingerprint("profile_counters"))]
        );
        db.query(
            "CREATE homework_submission:late SET homework = homework:h, user = user:b,
                 submitted_at = 90, updated_at = 90;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
        super::migrate(&db).await.unwrap();
        assert_eq!(
            counters(&db).await,
            after_first,
            "a mark carrying this binary's fingerprint skips the seed entirely"
        );
        assert_eq!(writes(&db).await, first_writes);
    }

    /// A marked block's *content* has to be correctable (2026-08-05). The mark
    /// used to record only that the block had run, so an edit to a marked block
    /// silently never ran on any volume that had booted once — which is how the
    /// `exam_sat_total` seed below (fixed to count sittings, not attempts)
    /// would have reached exactly nobody. The mark carries the fingerprint of
    /// the text it ran, and a different fingerprint runs the block once more.
    #[tokio::test]
    async fn an_edited_one_time_block_runs_once_more_and_restamps() {
        let db = super::init_mem().await.unwrap();
        // `init_mem` migrates an empty store, so the seed is already marked
        // done — the state every booted volume is in. History written *after*
        // that mark is the probe: a skip has to leave it uncounted, and a
        // re-run has to pick it up.
        db.query(
            "CREATE user:a SET username = 'a', password_hash = 'x', role = 'student';
             -- One sitting, taken twice. The corrected seed counts the exam,
             -- not the attempts, exactly as the request path credits it.
             CREATE exam_attempt:e1 SET exam = exam:x, user = user:a, seq = 1, started_at = 1;
             CREATE exam_attempt:e2 SET exam = exam:x, user = user:a, seq = 2, started_at = 2;
             CREATE pomodoro_session:p1 SET user = user:a, started_at = 1000, finished_at = 2000;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

        async fn counters(db: &super::Database) -> Vec<(i64, i64)> {
            let mut rows = db
                .query(
                    // `?? 0`, parenthesized: the columns are `option<int>` and
                    // a row the seed has not touched holds neither.
                    "SELECT VALUE [(exam_sat_total ?? 0), (pomodoro_finished_total ?? 0)]
                     FROM user ORDER BY id",
                )
                .await
                .unwrap();
            rows.take::<Vec<Vec<i64>>>(0)
                .unwrap()
                .into_iter()
                .map(|row| (row[0], row[1]))
                .collect()
        }
        let fp = expected_fingerprint("profile_counters");

        // Same binary, same fingerprint: the block does not run, and the
        // history above stays uncounted. That is the mark still doing its job.
        super::migrate(&db).await.unwrap();
        assert_eq!(counters(&db).await, vec![(0, 0)]);
        assert_eq!(
            stamped_fingerprint(&db, "profile_counters").await,
            vec![Some(fp)]
        );

        // A write probe on the seeded table, defined now so it counts only the
        // runs under test: the event fires *inside* the UPDATE, so "exactly
        // once" is measured rather than inferred from the numbers.
        db.query(
            "DEFINE TABLE user_write_probe SCHEMALESS;
             DEFINE EVENT user_write ON user WHEN $event = 'UPDATE' THEN {
                 UPSERT type::record('user_write_probe', 'n') SET n = (n ?? 0) + 1;
             };
             -- A mark written by the binary before fingerprints existed: the
             -- row is there, the column is not. This is the live volumes'
             -- state, and it must run the corrected block exactly once.
             UPDATE migration_mark:profile_counters UNSET fingerprint;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
        assert_eq!(
            stamped_fingerprint(&db, "profile_counters").await,
            vec![None]
        );

        super::migrate(&db).await.unwrap();
        // Two attempt rows, one exam: the corrected seed says one sitting, the
        // number the request path would have given. The old text said two.
        assert_eq!(counters(&db).await, vec![(1, 1)]);
        assert_eq!(
            stamped_fingerprint(&db, "profile_counters").await,
            vec![Some(fp)]
        );

        async fn writes(db: &super::Database) -> Vec<i64> {
            let mut rows = db
                .query("SELECT VALUE n FROM user_write_probe:n")
                .await
                .unwrap();
            rows.take(0).unwrap()
        }
        let one_run = writes(&db).await;
        // Pinned non-zero, not merely compared: "it did not run twice" holds
        // vacuously if the probe stopped firing, and the test would then pass
        // on a mechanism that never runs anything at all.
        assert!(matches!(one_run.as_slice(), [n] if *n > 0), "{one_run:?}");

        // Once more, not on every boot: the re-run re-stamped, so the next boot
        // is a plain skip again.
        super::migrate(&db).await.unwrap();
        assert_eq!(counters(&db).await, vec![(1, 1)]);
        assert_eq!(writes(&db).await, one_run, "the re-run happened once");

        // And now the edit this whole mechanism exists for: a mark whose stored
        // fingerprint is some *other* text's. The block runs again and lands on
        // the same numbers — every SET is absolute, so a re-run converges
        // instead of doubling what the first pass wrote.
        db.query("UPDATE migration_mark:profile_counters SET fingerprint = 1")
            .await
            .unwrap()
            .check()
            .unwrap();
        super::migrate(&db).await.unwrap();
        assert_eq!(
            counters(&db).await,
            vec![(1, 1)],
            "a re-run recomputes, it does not add to what is there"
        );
        assert_eq!(
            writes(&db).await,
            vec![one_run[0] * 2],
            "the edited block did the same work a second time, and no more"
        );
        assert_eq!(
            stamped_fingerprint(&db, "profile_counters").await,
            vec![Some(fp)]
        );
        super::migrate(&db).await.unwrap();
        assert_eq!(writes(&db).await, vec![one_run[0] * 2], "and stopped again");
    }

    #[tokio::test]
    async fn the_retired_claim_columns_are_dropped_off_a_live_row() {
        // Stale-data path for the 2026-07-30 removal of the chat claim queue.
        // A dev volume carries `chatbot_message` rows *with* claim values and an
        // `ai_worker` table, so the boot has to clear the values while their
        // definitions still stand (PRE_REPAIR) and only then retire the columns
        // — SCHEMAFULL rejects every write to a row storing a column that no
        // longer exists, which would make the row unanswerable forever.
        let db = super::init_mem().await.unwrap();
        db.query(
            "DEFINE FIELD claimed_by ON chatbot_message TYPE option<string>;
             DEFINE FIELD claimed_at ON chatbot_message TYPE option<int>;
             DEFINE TABLE ai_worker SCHEMAFULL;
             DEFINE FIELD seen_at ON ai_worker TYPE int;
             DEFINE TABLE migration_lock SCHEMALESS;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
        db.query(
            "CREATE user:u SET username = 'u', password_hash = 'x';
             CREATE chatbot_thread:c SET user_id = user:u, created_at = 1, updated_at = 1;
             CREATE chatbot_message:m SET thread_id = chatbot_thread:c, user_id = user:u,
                 role = 'assistant', content = '', status = 'pending',
                 created_at = time::unix(time::now()) * 1000,
                 claimed_by = 'a-dead-replica', claimed_at = 1;
             CREATE ai_worker:w SET seen_at = 1;
             CREATE migration_lock:the_lock SET holder = 'a-dead-replica', expires_at = 1;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

        // Twice: the repair and both removals have to be idempotent, since
        // every boot runs them.
        super::migrate(&db).await.unwrap();
        super::migrate(&db).await.unwrap();

        let mut info = db.query("INFO FOR TABLE chatbot_message").await.unwrap();
        let table: serde_json::Value = info.take::<Option<serde_json::Value>>(0).unwrap().unwrap();
        let fields = table["fields"].as_object().expect("fields");
        assert!(!fields.contains_key("claimed_by"), "{table}");
        assert!(!fields.contains_key("claimed_at"), "{table}");
        let mut tables = db.query("INFO FOR DB").await.unwrap();
        let dbinfo: serde_json::Value = tables
            .take::<Option<serde_json::Value>>(0)
            .unwrap()
            .unwrap();
        let tables = dbinfo["tables"].as_object().expect("tables");
        assert!(!tables.contains_key("ai_worker"), "{dbinfo}");
        // Same story one table over: the elected-boot lease, stale leader row
        // and all.
        assert!(!tables.contains_key("migration_lock"), "{dbinfo}");

        // The load-bearing half: the row that carried a claim is still
        // writable. A leftover value under no definition fails here.
        db.query("UPDATE chatbot_message:m SET status = 'complete', content = 'ok'")
            .await
            .unwrap()
            .check()
            .unwrap();
        let mut row = db
            .query("SELECT * FROM ONLY chatbot_message:m")
            .await
            .unwrap();
        let row: serde_json::Value = row.take::<Option<serde_json::Value>>(0).unwrap().unwrap();
        assert_eq!(row["status"], "complete", "{row}");
        assert!(row.get("claimed_by").is_none(), "{row}");
    }

    /// The other half of the sweep: a restart seconds after a dispatch must not
    /// kill the turn — the answering task may still be alive on the other side
    /// of the bridge, and the row is not yet past the stale horizon.
    #[tokio::test]
    async fn boot_spares_a_chatbot_message_pending_since_just_now() {
        let db = super::init_mem().await.unwrap();
        db.query(
            "CREATE user:u SET username = 'u', password_hash = 'x';
             CREATE chatbot_thread:c SET user_id = user:u, created_at = 1, updated_at = 1;
             CREATE chatbot_message:fresh SET thread_id = chatbot_thread:c, user_id = user:u,
                 role = 'assistant', content = '', status = 'pending',
                 created_at = time::unix(time::now()) * 1000 - 2000;
             CREATE chatbot_message:old SET thread_id = chatbot_thread:c, user_id = user:u,
                 role = 'assistant', content = '', status = 'pending',
                 created_at = time::unix(time::now()) * 1000
                     - ($stale + 1) * 1000;",
        )
        .bind(("stale", crate::constant::CHATBOT_PENDING_STALE_SECS))
        .await
        .unwrap()
        .check()
        .unwrap();

        super::migrate(&db).await.unwrap();

        let mut res = db
            .query(
                "SELECT status, error_code FROM ONLY chatbot_message:fresh;
                 SELECT VALUE status FROM ONLY chatbot_message:old;",
            )
            .await
            .unwrap();
        let fresh: Option<serde_json::Value> = res.take(0).unwrap();
        let fresh = fresh.unwrap();
        let old: Option<String> = res.take(1).unwrap();
        assert_eq!(
            fresh["status"], "pending",
            "a two-second-old turn is still being answered"
        );
        assert_eq!(fresh["error_code"], serde_json::Value::Null);
        assert_eq!(old.as_deref(), Some("failed"), "past the horizon it goes");
    }

    /// A row written before choices had ids survives the retyped columns: it is
    /// converted, it is writable again, and its stored *position* still names
    /// the same option — a wrong remap here silently regrades exams.
    #[tokio::test]
    async fn boot_converts_positional_choices_and_keeps_the_same_option() {
        let db = super::init_mem().await.unwrap();
        // Roll the columns back to their pre-2026-07-24 shapes and write the
        // kind of row an older binary left behind, `source_bank` included.
        db.query(
            "REMOVE FIELD IF EXISTS choices[*].id ON TABLE exam_question;
             REMOVE FIELD IF EXISTS choices[*].text ON TABLE exam_question;
             REMOVE FIELD IF EXISTS choices[*].id ON TABLE bank_question;
             REMOVE FIELD IF EXISTS choices[*].text ON TABLE bank_question;
             DEFINE FIELD OVERWRITE choices ON exam_question TYPE option<array<string>>;
             DEFINE FIELD OVERWRITE correct ON exam_question TYPE option<int>;
             DEFINE FIELD OVERWRITE source_bank ON exam_question TYPE option<string>;
             DEFINE FIELD OVERWRITE slot ON question_image TYPE option<int>;
             DEFINE FIELD OVERWRITE selected ON exam_answer TYPE option<int>;
             DEFINE FIELD OVERWRITE choices ON bank_question TYPE option<array<string>>;
             DEFINE FIELD OVERWRITE correct ON bank_question TYPE option<int>;
             DEFINE FIELD OVERWRITE slot ON bank_question_image TYPE option<int>;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
        db.query(
            "CREATE user:u SET username = 'u', password_hash = 'x';
             CREATE exam_question:q SET exam = exam:e, text = 'q', kind = 'multiple',
                 points = 5, subject = subject:s, choices = ['3', '4', '5'], correct = 1,
                 source_bank = 'bank_question:b';
             CREATE question_image:i SET exam = exam:e, question = exam_question:q, slot = 2,
                 file = 'f', content_type = 'image/png', size = 1;
             CREATE question_image:whole SET exam = exam:e, question = exam_question:q,
                 file = 'w', content_type = 'image/png', size = 1;
             CREATE exam_answer:a SET exam = exam:e, question = exam_question:q, user = user:u,
                 selected = 2, updated_at = 1;
             CREATE bank_question:b SET owner = user:u, text = 'b', kind = 'multiple',
                 points = 3, choices = ['a', 'b'], correct = 0, created_at = 1;
             CREATE bank_question_image:bi SET bank_question = bank_question:b, slot = 1,
                 file = 'bf', content_type = 'image/png', size = 1;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

        super::migrate(&db).await.unwrap();

        let mut res = db
            .query(
                "SELECT choices, correct FROM ONLY exam_question:q;
                 SELECT VALUE slot FROM ONLY question_image:i;
                 SELECT VALUE slot FROM ONLY question_image:whole;
                 SELECT VALUE selected FROM ONLY exam_answer:a;
                 SELECT choices, correct FROM ONLY bank_question:b;
                 SELECT VALUE slot FROM ONLY bank_question_image:bi;",
            )
            .await
            .unwrap();
        let q: Option<serde_json::Value> = res.take(0).unwrap();
        let q = q.unwrap();
        let img_slot: Option<String> = res.take(1).unwrap();
        let whole_slot: Option<String> = res.take(2).unwrap();
        let selected: Option<String> = res.take(3).unwrap();
        let bank: Option<serde_json::Value> = res.take(4).unwrap();
        let bank = bank.unwrap();
        let bank_slot: Option<String> = res.take(5).unwrap();

        let ids: Vec<&str> = q["choices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            q["choices"][0]["text"], "3",
            "option order (and so the meaning of every stored index) is preserved"
        );
        assert_eq!(q["correct"], ids[1], "correct was index 1");
        assert_eq!(
            img_slot.as_deref(),
            Some(ids[2]),
            "option picture was slot 2"
        );
        assert_eq!(selected.as_deref(), Some(ids[2]), "the answer was index 2");
        assert_eq!(whole_slot, None, "a question-level picture has no index");
        let bank_ids: Vec<&str> = bank["choices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect();
        assert_eq!(bank["correct"], bank_ids[0]);
        assert_eq!(bank_slot.as_deref(), Some(bank_ids[1]));

        // Writable again — every one of these failed coercion before the
        // backfill existed, and `source_bank` outlived its own column.
        db.query(
            "UPDATE exam_question:q SET points = 7;
             UPDATE question_image:i SET size = 2;
             UPDATE exam_answer:a SET updated_at = 2;
             UPDATE bank_question:b SET points = 4;
             UPDATE bank_question_image:bi SET size = 2;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

        // A second boot re-mints nothing.
        super::migrate(&db).await.unwrap();
        let mut res = db
            .query("SELECT VALUE choices.map(|$c| $c.id) FROM ONLY exam_question:q")
            .await
            .unwrap();
        let again: Vec<String> = res.take(0).unwrap();
        assert_eq!(again, ids, "conversion is idempotent");
    }
}
