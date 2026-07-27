//! SurrealDB connection (WebSocket to a server; in-memory for tests) + schema.

use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::opt::auth::Root;
use surrealdb::types::SurrealValue;

use crate::config::Config;
use crate::constant::{
    CAS_UPDATE_RETRIES, CHATBOT_PENDING_STALE_SECS, MIGRATION_LOCK_CLAIM, MIGRATION_LOCK_DDL,
    MIGRATION_LOCK_HEARTBEAT, MIGRATION_LOCK_HEARTBEAT_SECS, MIGRATION_LOCK_LEASE_SECS,
    MIGRATION_LOCK_POLL_MS, MIGRATION_LOCK_STAMP, MIGRATION_LOCK_STATE, MIGRATION_LOCK_TAKEOVER,
    MIGRATION_LOCK_WAIT_SECS,
};
use crate::domain::user::{Password, User, Username};
use crate::error::AppError;
use crate::migration_sql::MIGRATION_BATCHES;

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

/// Connect to the SurrealDB server, sign in as root, and apply the schema —
/// the latter through the boot election, so that N overlapping processes apply
/// it once between them (see [`boot_once`]).
pub async fn init(cfg: &Config) -> Result<Database, AppError> {
    // Validated before the connection, not after the election: half a
    // credential pair is a deployment mistake, and finding that out only in the
    // process that happened to win the lock would make it a coin flip.
    let admin = admin_credentials(cfg)?;
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
    boot_once(
        &db,
        admin,
        std::time::Duration::from_secs(MIGRATION_LOCK_WAIT_SECS),
    )
    .await?;
    Ok(db)
}

/// The `ADMIN_USERNAME` / `ADMIN_PASSWORD` bootstrap pair, if configured.
/// Both-or-neither: half a pair aborts startup rather than silently running
/// without the seed.
fn admin_credentials(cfg: &Config) -> Result<Option<(Username, Password)>, AppError> {
    match (&cfg.admin_username, &cfg.admin_password) {
        (Some(username), Some(password)) => Ok(Some((
            Username::try_new(username)
                .map_err(|err| AppError::Internal(format!("invalid ADMIN_USERNAME: {err}")))?,
            Password::try_new(password)
                .map_err(|err| AppError::Internal(format!("invalid ADMIN_PASSWORD: {err}")))?,
        ))),
        (None, None) => Ok(None),
        _ => Err(AppError::Internal(
            "ADMIN_USERNAME and ADMIN_PASSWORD must be set together".into(),
        )),
    }
}

/// Run the boot work that must happen exactly once — the schema batches and the
/// admin seed — under a leader election, and hold every other process at the
/// door until it *has* happened. Returns whether we were the leader.
///
/// Two processes overlap on every rolling restart, long before any replica
/// work: they used to race the same SCHEMAFULL DDL, the same data backfills
/// (including the chatbot pending sweep, which writes live rows) and the same
/// `ensure_admin` read-then-create. The election is a deterministic-id
/// `CREATE`: SurrealDB v3 answers a create on an existing id with "already
/// exists" instead of overwriting, so of N racing processes exactly one gets an
/// `Ok`, and that one is the leader.
///
/// The lock is a *heartbeated lease*, not a once-only flag, and both halves of
/// that are deliberate:
///
/// * Heartbeated, because a leader killed mid-migration must not wedge every
///   future boot, yet a leader that is merely slow must never be overtaken —
///   two processes in the DDL at once is the exact race being prevented. The
///   leader renews `claimed_at` every `MIGRATION_LOCK_HEARTBEAT_SECS` for as
///   long as it works, and a peer takes over only after
///   `MIGRATION_LOCK_LEASE_SECS` of silence. The price is that recovery from a
///   killed leader waits out the lease; the alternative, a plain age horizon,
///   would have to exceed the slowest imaginable migration to be safe.
/// * A lease, not a flag, because a boot an hour later may be a new binary with
///   new DDL and must re-apply, and because the data backfills (the chatbot
///   pending sweep above all) are meant to run on every restart. Only a boot
///   *inside* the lease trusts a peer's `applied_at` — i.e. exactly the
///   rolling-restart window — and only when that stamp carries this binary's own
///   [`migration_fingerprint`], so a mixed-version window cannot make the newer
///   process skip its own DDL.
///
/// `wait` is how long a non-leader keeps polling before it fails the boot; a
/// parameter rather than a constant read inline so a test can prove that
/// deadline actually bites without sitting out `MIGRATION_LOCK_WAIT_SECS`.
async fn boot_once(
    db: &Database,
    admin: Option<(Username, Password)>,
    wait: std::time::Duration,
) -> Result<bool, AppError> {
    let lease_ms = MIGRATION_LOCK_LEASE_SECS * 1_000;
    // Any unique string; the lock only ever compares it for equality.
    let me = ulid::Ulid::new().to_string();
    // No query deadline anywhere in here on purpose: the SDK parks queries
    // while the socket is down instead of failing them, so a timeout would
    // report an outage as a dead leader and hand the migration to a second
    // process (see `crate::error::AppError::DbTimeout`).
    //
    // The bootstrap step: the lock's own table has to exist before anyone can
    // claim a row in it, so every process defines it (`IF NOT EXISTS`) rather
    // than the leader — the leader is not known yet. N processes issuing that
    // at once makes the key-value layer abort the losers with a retryable write
    // conflict, and a conflict here means a rival committed the very definition
    // we wanted, so it counts as success.
    if let Err(err) = db
        .query(MIGRATION_LOCK_DDL)
        .await
        .and_then(|response| response.check())
        && !lost_the_race(&err)
    {
        return Err(err.into());
    }

    let deadline = tokio::time::Instant::now() + wait;
    loop {
        if claim(db, MIGRATION_LOCK_CLAIM, &me, lease_ms).await? {
            return leader_applies(db, &me, admin).await.map(|()| true);
        }
        let read = db
            .query(MIGRATION_LOCK_STATE)
            .bind(("lease_ms", lease_ms))
            .bind(("fingerprint", migration_fingerprint()))
            .await
            .and_then(|response| response.check())
            .and_then(|mut response| response.take::<Option<LockState>>(0));
        let state = match read {
            Ok(state) => state,
            // The leader heartbeats this very row every few seconds and rivals
            // take it over, so a read of it can be aborted as retryable — which
            // is not an error about the lock, just "no answer this round". This
            // poll loop, with its deadline, *is* the bounded retry the conflict
            // asks for, so treat it like a missing row and look again. Retrying
            // a `SELECT` is free of side effects by construction.
            Err(err) if lost_the_race(&err) => None,
            Err(err) => return Err(err.into()),
        };
        match state {
            // A live lease, stamped with our own fingerprint: the exact schema
            // this process needs is in place, so stop waiting.
            Some(state) if state.live && state.applied => {
                tracing::info!(leader = %state.holder, "schema applied by another process");
                return Ok(false);
            }
            // Anything else may be takeable — the holder went silent past the
            // lease, or it finished but with a different schema than ours. The
            // statement's `WHERE` is the arbiter, so a live holder still inside
            // the DDL refuses us and we fall through to waiting.
            Some(state) if claim(db, MIGRATION_LOCK_TAKEOVER, &me, lease_ms).await? => {
                tracing::warn!(
                    previous = %state.holder,
                    live = state.live,
                    "taking over the boot migration lock: the holder went silent or applied \
                     a different schema"
                );
                return leader_applies(db, &me, admin).await.map(|()| true);
            }
            Some(state) => {
                tracing::debug!(leader = %state.holder, "waiting for the boot migration");
            }
            // The row vanished; the next iteration's `CREATE` picks it up.
            None => {}
        }
        if tokio::time::Instant::now() >= deadline {
            // Loudly, not optimistically: SCHEMAFULL means an unmigrated
            // database fails every write anyway, and it would do so as
            // scattered 500s instead of one startup error. Exiting hands the
            // retry to the container runtime, which is where boot retries live.
            //
            // One production sequence reaches this branch: a leader that keeps
            // renewing its lease for longer than `wait` without ever stamping —
            // a migration slower than `MIGRATION_LOCK_WAIT_SECS`. A leader that
            // *stops* renewing never gets here, because the takeover above fires
            // a lease (30s) rather than a wait (180s) after the last renewal. So
            // the relative size of those two constants is not what selects this
            // branch; "the lock stayed live-and-unstamped for the whole wait"
            // is — which is precisely the state
            // `a_live_lock_is_waited_for_and_the_wait_is_bounded` constructs,
            // renewer and all, at a scaled-down wait.
            return Err(AppError::Internal(format!(
                "the boot migration lock stayed held for {wait:?}; \
                 refusing to serve on a schema no process has confirmed"
            )));
        }
        tokio::time::sleep(std::time::Duration::from_millis(MIGRATION_LOCK_POLL_MS)).await;
    }
}

/// The lock row as a loser reads it. `live`/`applied` are computed by the
/// database so the verdict never depends on this process's clock.
#[derive(SurrealValue)]
struct LockState {
    holder: String,
    applied: bool,
    live: bool,
}

/// Run one of the two acquiring statements and report whether we now hold the
/// lock — both return our own holder id when we do, and nothing when a rival
/// got there first.
///
/// Two failures mean "not ours", not "broken". "Already exists" is the
/// deterministic-id create losing the race, and a key-value *write conflict* is
/// the same race decided one layer down: when two processes create the same id
/// at genuinely the same instant, the loser's transaction is aborted as
/// retryable rather than answered with the duplicate-key message (observed on
/// every run of `two_concurrent_boots_apply_the_schema_once`). Either way the
/// caller loops, re-reads the lock and finds out who holds it — which is also
/// the retry the conflict asks for.
async fn claim(db: &Database, sql: &str, me: &str, lease_ms: i64) -> Result<bool, AppError> {
    let result = db
        .query(sql)
        .bind(("me", me.to_string()))
        .bind(("lease_ms", lease_ms))
        .bind(("fingerprint", migration_fingerprint()))
        .await
        .and_then(|response| response.check());
    match result {
        Ok(mut response) => {
            let holders: Vec<String> = response.take(0)?;
            Ok(holders.iter().any(|holder| holder == me))
        }
        Err(err) if lost_the_race(&err) => Ok(false),
        Err(err) => Err(err.into()),
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

/// A hash of the exact DDL and backfills this binary intends to apply.
///
/// It is what makes a peer's "applied" stamp trustworthy. Without it, a process
/// booting inside the lease window trusts *any* finished predecessor — so a new
/// binary starting seconds after an old one stamped would skip its own newer
/// DDL and then serve on a schema missing the very fields its handlers write.
/// With it, "somebody applied a schema" narrows to "somebody applied *this*
/// schema", and anything else is treated as unapplied.
///
/// Derived from the three SQL constants, never a hand-bumped version number: a
/// number nobody remembers to raise is worse than no check at all, while this
/// one changes on its own the moment the schema does.
fn migration_fingerprint() -> &'static str {
    use sha2::{Digest, Sha256};
    static FINGERPRINT: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        let mut hasher = Sha256::new();
        // Length-prefixed, not concatenated: without a separator, moving a
        // statement from one batch into the next leaves the byte stream — and so
        // the fingerprint — unchanged, while changing what the boot actually
        // does (batch boundaries are what keep the DDL out of the backfills).
        for sql in MIGRATION_BATCHES {
            hasher.update((sql.len() as u64).to_le_bytes());
            hasher.update(sql.as_bytes());
        }
        for (name, value) in migration_binds() {
            hasher.update(name.as_bytes());
            hasher.update(value.to_le_bytes());
        }
        hex::encode(hasher.finalize())
    });
    &FINGERPRINT
}

/// Leader path: keep the lease alive, apply everything, then stamp it done.
///
/// The renewal runs *concurrently with* the work rather than after it, in the
/// same task: `select!` polls both, so a lease that cannot be renewed is
/// noticed while the DDL is still going instead of at the end.
async fn leader_applies(
    db: &Database,
    me: &str,
    admin: Option<(Username, Password)>,
) -> Result<(), AppError> {
    let apply = async {
        migrate(db).await?;
        if let Some((username, password)) = admin {
            User::ensure_admin(username, password, db).await?;
        }
        Ok::<(), AppError>(())
    };
    tokio::select! {
        applied = apply => applied?,
        lost = renew_lease(db, me) => return Err(lost),
    }

    // The stamp is the one write in this path with nothing above it to retry
    // it: waiters are polling the same row and taking it over, so the
    // key-value layer can abort ours as retryable, and an escaping conflict
    // fails a boot that had already migrated everything. Repeating it is safe
    // despite `applies += 1` — a conflicted transaction commits nothing, and
    // the statement's `holder` guard turns a *genuine* takeover into an empty
    // result rather than a second count. Bounded, because a conflict that
    // never clears is a real failure and must stay one.
    let mut stamped: Vec<String> = Vec::new();
    for attempt in 0..CAS_UPDATE_RETRIES {
        let attempted = db
            .query(MIGRATION_LOCK_STAMP)
            .bind(("me", me.to_string()))
            .bind(("fingerprint", migration_fingerprint()))
            .await
            .and_then(|response| response.check());
        match attempted {
            Ok(mut response) => {
                stamped = response.take(0)?;
                break;
            }
            Err(err) if lost_the_race(&err) && attempt + 1 < CAS_UPDATE_RETRIES => {
                tracing::warn!("stamping the boot migration lock conflicted ({err}) — retrying");
                tokio::time::sleep(std::time::Duration::from_millis(MIGRATION_LOCK_POLL_MS)).await;
            }
            Err(err) => return Err(err.into()),
        }
    }
    if stamped.is_empty() {
        // The same verdict as a lost renewal, for the same reason — this is the
        // backstop for losing the lock in the window between the last renewal
        // and the stamp.
        return Err(lost_lease());
    }
    Ok(())
}

/// Renew the lease until it can no longer be renewed. Resolves *only* on that
/// loss, so it can sit opposite the real work in a `select!`.
///
/// A refused renewal (empty result) means a peer declared us dead and took the
/// lock, so it is now applying the same DDL underneath us. We treat that as
/// fatal to this boot and stop: our statements are all idempotent, so there is
/// nothing to undo, but continuing would keep two processes writing schema at
/// once — the exact race this module exists to prevent — and would leave us
/// serving as if we had migrated. Failing hands the retry to the container
/// runtime, which re-elects cleanly.
///
/// A renewal that *errors* is different: the socket may simply be down, and the
/// SDK parks queries rather than failing them, so a real error here is rare and
/// says nothing definite. Log it and keep trying — if it persists, the lease
/// expires, a peer takes over, and the next renewal is refused, which is the
/// verdict above. `.check()` is load-bearing: a statement-level failure comes
/// back inside an `Ok` response, so without it a renewal could fail forever
/// while logging nothing and renewing nothing.
async fn renew_lease(db: &Database, me: &str) -> AppError {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(
        MIGRATION_LOCK_HEARTBEAT_SECS,
    ));
    loop {
        interval.tick().await;
        match db
            .query(MIGRATION_LOCK_HEARTBEAT)
            .bind(("me", me.to_string()))
            .await
            .and_then(|response| response.check())
        {
            Ok(mut response) => match response.take::<Vec<String>>(0) {
                Ok(holders) if holders.is_empty() => return lost_lease(),
                Ok(_) => tracing::debug!("boot migration lease renewed"),
                Err(err) => tracing::warn!("boot lock renewal returned unusable rows: {err}"),
            },
            Err(err) => tracing::warn!("boot lock renewal failed: {err}"),
        }
    }
}

fn lost_lease() -> AppError {
    AppError::Internal(
        "lost the boot migration lock while applying it — another process took it over and is \
         applying the schema, so this boot stops rather than race it"
            .into(),
    )
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

/// A fresh in-memory database with the schema applied. For tests.
pub async fn init_mem() -> Result<Database, AppError> {
    let db = surrealdb::engine::any::connect("memory").await?;
    db.use_ns("hezarfen").use_db("hezarfen").await?;
    migrate(&db).await?;
    Ok(std::sync::Arc::new(db))
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

/// Parameters the batches are executed with. Bound, not baked into the SQL
/// string: a `const` cannot be interpolated into another `const`, and a
/// hand-copied 300000 would drift silently. They are part of what the migration
/// *does*, so [`migration_fingerprint`] hashes them alongside the SQL — a
/// changed horizon is a changed migration.
fn migration_binds() -> [(&'static str, i64); 1] {
    [("stale_ms", CHATBOT_PENDING_STALE_SECS * 1_000)]
}

#[cfg(test)]
mod tests {
    use super::{Database, boot_once};
    use crate::constant::{MIGRATION_LOCK_LEASE_SECS, MIGRATION_LOCK_TABLE};
    use crate::domain::user::{Password, Username};
    use std::time::Duration;

    /// An empty in-memory database with *no* schema — what a booting process
    /// actually finds. [`super::init_mem`] cannot be used here: it migrates.
    async fn unmigrated() -> Database {
        let db = surrealdb::engine::any::connect("memory").await.unwrap();
        db.use_ns("hezarfen").use_db("hezarfen").await.unwrap();
        std::sync::Arc::new(db)
    }

    fn admin() -> Option<(Username, Password)> {
        Some((
            Username::try_new("root").unwrap(),
            Password::try_new("secret1").unwrap(),
        ))
    }

    /// How many times the lock has been stamped applied — the observable side
    /// effect that makes "exactly one process applied" checkable.
    async fn applies(db: &Database) -> i64 {
        let mut res = db
            .query("SELECT VALUE applies FROM ONLY migration_lock:boot")
            .await
            .unwrap();
        res.take::<Option<i64>>(0).unwrap().unwrap()
    }

    /// How many `root` rows the seed left. Guarded on the table existing: a
    /// waiter that failed its boot never ran the DDL, so `user` may be absent,
    /// which is an error rather than an empty result.
    async fn admin_count(db: &Database) -> i64 {
        let mut res = db
            .query(
                "RETURN IF 'user' IN object::keys((INFO FOR DB).tables) {
                     array::len((SELECT id FROM user WHERE username = 'root'))
                 } ELSE { 0 };",
            )
            .await
            .unwrap();
        res.take::<Option<i64>>(0).unwrap().unwrap()
    }

    /// Two processes booting at once — the rolling-restart case, which happens
    /// today without any replica work. Exactly one may run the DDL, the
    /// backfills and the admin seed; the other has to wait for it and then
    /// serve, not repeat it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_concurrent_boots_apply_the_schema_once() {
        let db = unmigrated().await;
        let (a, b) = (db.clone(), db.clone());
        let one =
            tokio::spawn(async move { boot_once(&a, admin(), Duration::from_secs(30)).await });
        let two =
            tokio::spawn(async move { boot_once(&b, admin(), Duration::from_secs(30)).await });
        let (one, two) = (one.await.unwrap().unwrap(), two.await.unwrap().unwrap());

        assert!(one ^ two, "exactly one process leads the boot: {one}/{two}");
        assert_eq!(applies(&db).await, 1, "the migration was applied once");
        assert_eq!(admin_count(&db).await, 1, "the admin was seeded once");
        // The loser still needs the schema it skipped applying.
        assert!(
            db.query("CREATE user:x SET username = 'x', password_hash = 'h'")
                .await
                .unwrap()
                .check()
                .is_ok(),
            "the schema is in place once both boots return"
        );
    }

    /// The same race with enough processes in it to actually hit the
    /// key-value layer's retryable aborts: eight boots contending for one lock
    /// row collide on the losers' state read and the leader's stamp, which with
    /// two boots surfaces only once in a few full-suite runs.
    ///
    /// What it asserts is deliberately narrow: no boot may fail with a
    /// *retryable* conflict. A conflict is the layer asking to be asked again,
    /// so one reaching the caller is always a missing retry — while the other
    /// outcomes of a crowded election (a leader that loses its lease to a
    /// taker) are documented behaviour, not bugs, and asserting against them
    /// here would only make this flaky. Exactly-once stays the two-boot test's
    /// job.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_crowd_of_boots_never_surfaces_a_retryable_conflict() {
        let db = unmigrated().await;
        let boots: Vec<_> = (0..8)
            .map(|_| {
                let db = db.clone();
                tokio::spawn(async move { boot_once(&db, admin(), Duration::from_secs(30)).await })
            })
            .collect();
        for boot in boots {
            if let Err(err) = boot.await.unwrap() {
                let message = err.to_string();
                assert!(
                    !message.contains("can be retried") && !message.contains("conflict"),
                    "a retryable conflict escaped boot_once instead of being retried: {message}"
                );
            }
        }
    }

    /// A leader killed mid-migration leaves its claim behind. Nothing may
    /// require a human with a SurrealQL prompt to clear it: the next boot takes
    /// the lease over once it has gone unrenewed past the horizon.
    #[tokio::test]
    async fn an_abandoned_lock_is_taken_over_by_the_next_boot() {
        let db = unmigrated().await;
        // Exactly what a SIGKILL at the worst moment leaves: claimed, never
        // stamped, and no heartbeat since.
        db.query(
            "DEFINE TABLE migration_lock SCHEMALESS;
             CREATE migration_lock:boot SET holder = 'dead', applies = 0,
                 claimed_at = time::unix(time::now()) * 1000 - ($lease + 1) * 1000;",
        )
        .bind(("lease", MIGRATION_LOCK_LEASE_SECS))
        .await
        .unwrap()
        .check()
        .unwrap();

        assert!(
            boot_once(&db, admin(), Duration::from_secs(5))
                .await
                .unwrap(),
            "the abandoned lock is taken over, not waited on"
        );
        assert_eq!(applies(&db).await, 1);
        assert_eq!(admin_count(&db).await, 1);
    }

    /// The other side of that horizon: a lock claimed a moment ago belongs to a
    /// process that is probably still working, so it is waited for — and if the
    /// wait runs out the boot fails loudly instead of serving on a schema
    /// nobody has confirmed.
    #[tokio::test]
    async fn a_live_lock_is_waited_for_and_the_wait_is_bounded() {
        let db = unmigrated().await;
        db.query(
            "DEFINE TABLE migration_lock SCHEMALESS;
             CREATE migration_lock:boot SET holder = 'busy', applies = 0,
                 claimed_at = time::unix(time::now()) * 1000;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

        // A real leader renews; without a renewer this would test "the lease
        // happened to still be inside its horizon", which is a different branch
        // from the one production reaches (a leader slower than the wait).
        let renewer = tokio::spawn({
            let db = db.clone();
            async move {
                loop {
                    db.query(super::MIGRATION_LOCK_HEARTBEAT)
                        .bind(("me", "busy".to_string()))
                        .await
                        .unwrap()
                        .check()
                        .unwrap();
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        });
        let err = boot_once(&db, admin(), Duration::from_millis(600))
            .await
            .expect_err("a held, unstamped lock must not let a boot through");
        renewer.abort();
        assert!(
            err.to_string().contains("refusing to serve"),
            "the boot fails loudly: {err}"
        );
        assert_eq!(applies(&db).await, 0, "the waiter applied nothing");
        assert_eq!(admin_count(&db).await, 0, "and seeded nothing");
    }

    /// The rolling-deploy hole the fingerprint exists for: a peer stamped the
    /// lock moments ago, inside the lease, so its lease is live and its work is
    /// done — but it applied a *different* schema. Trusting that stamp means
    /// serving on a database missing the fields this binary's own handlers
    /// write. It must be treated as unapplied and taken over.
    #[tokio::test]
    async fn a_peers_stamp_for_a_different_schema_is_not_trusted() {
        let db = unmigrated().await;
        db.query(
            "DEFINE TABLE migration_lock SCHEMALESS;
             CREATE migration_lock:boot SET holder = 'old-binary', applies = 1,
                 claimed_at = time::unix(time::now()) * 1000,
                 applied_at = time::unix(time::now()) * 1000,
                 fingerprint = 'the DDL of some other build';",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

        assert!(
            boot_once(&db, admin(), Duration::from_secs(5))
                .await
                .unwrap(),
            "a stamp from a different schema is no evidence about ours"
        );
        assert_eq!(applies(&db).await, 2, "we applied ours on top");
        assert_eq!(admin_count(&db).await, 1);
        let mut res = db
            .query("SELECT VALUE fingerprint FROM ONLY migration_lock:boot")
            .await
            .unwrap();
        assert_eq!(
            res.take::<Option<String>>(0).unwrap().as_deref(),
            Some(super::migration_fingerprint()),
            "and left our own fingerprint behind"
        );
    }

    /// The same lock, stamped with *our* fingerprint, is the case the skip
    /// exists for — otherwise every rolling restart re-runs the whole migration
    /// and the peer's boot is pointless.
    #[tokio::test]
    async fn a_peers_stamp_for_our_own_schema_is_trusted() {
        let db = unmigrated().await;
        db.query(
            "DEFINE TABLE migration_lock SCHEMALESS;
             CREATE migration_lock:boot SET holder = 'peer', applies = 1,
                 claimed_at = time::unix(time::now()) * 1000,
                 applied_at = time::unix(time::now()) * 1000,
                 fingerprint = $fingerprint;",
        )
        .bind(("fingerprint", super::migration_fingerprint()))
        .await
        .unwrap()
        .check()
        .unwrap();

        assert!(
            !boot_once(&db, admin(), Duration::from_secs(5))
                .await
                .unwrap(),
            "the peer applied this very schema, so this process just proceeds"
        );
        assert_eq!(applies(&db).await, 1, "and applies nothing itself");
    }

    /// A hash of the same bytes, computed independently: the fingerprint has to
    /// cover every batch, every bound value, and the boundaries between them.
    /// Any of those left out is a schema change two binaries could disagree on
    /// while claiming the same fingerprint.
    #[test]
    fn the_fingerprint_covers_every_batch_its_binds_and_the_boundaries() {
        use sha2::{Digest, Sha256};
        let hash = |batches: &[String], binds: &[(&str, i64)]| {
            let mut hasher = Sha256::new();
            for sql in batches {
                hasher.update((sql.len() as u64).to_le_bytes());
                hasher.update(sql.as_bytes());
            }
            for (name, value) in binds {
                hasher.update(name.as_bytes());
                hasher.update(value.to_le_bytes());
            }
            hex::encode(hasher.finalize())
        };
        let batches: Vec<String> = crate::migration_sql::MIGRATION_BATCHES
            .iter()
            .map(|sql| (*sql).to_string())
            .collect();
        let binds = super::migration_binds();
        assert_eq!(super::migration_fingerprint(), hash(&batches, &binds));

        let mut edited = batches.clone();
        edited[2].push_str("-- one more backfill\n");
        assert_ne!(
            super::migration_fingerprint(),
            hash(&edited, &binds),
            "an edited batch must change the fingerprint"
        );
        // The boundary itself: the same bytes, one statement moved from the DDL
        // batch into the backfill batch. Concatenation without a separator would
        // hash these two arrangements identically.
        let mut moved = batches.clone();
        let at = moved[1].len() - 20;
        let tail = moved[1].split_off(at);
        moved[2].insert_str(0, &tail);
        assert_ne!(
            super::migration_fingerprint(),
            hash(&moved, &binds),
            "moving text across a batch boundary must change the fingerprint"
        );
        assert_ne!(
            super::migration_fingerprint(),
            hash(&batches, &[("stale_ms", 1)]),
            "a changed bound value is a changed migration"
        );
    }

    /// The forcing function behind all of that: `migrate` and
    /// `migration_fingerprint` must read the *same* list. A fourth batch
    /// executed by one and not hashed by the other would let two binaries with
    /// different schemas claim the same fingerprint — and then skip each other's
    /// migration, which is the whole hole the fingerprint was added to close.
    #[test]
    fn what_the_boot_executes_is_what_the_fingerprint_hashes() {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/database.rs"))
                .expect("read database.rs");
        let body = |name: &str| {
            let start = source.find(name).expect("function is declared");
            let rest = &source[start..];
            let end = rest.find("\n}\n").expect("function ends");
            rest[..end].to_string()
        };
        for (name, body) in [
            ("pub async fn migrate", body("pub async fn migrate")),
            ("fn migration_fingerprint", body("fn migration_fingerprint")),
        ] {
            assert!(
                body.contains("MIGRATION_BATCHES") && body.contains("migration_binds()"),
                "{name} must derive from MIGRATION_BATCHES + migration_binds(), \
                 or the executed and the hashed migration can drift apart"
            );
        }
        let executed = body("pub async fn migrate");
        assert_eq!(
            executed.matches("db.query(").count(),
            1,
            "migrate() runs exactly one statement source — the batch list. A second \
             `db.query(...)` here is a batch the fingerprint does not hash:\n{executed}"
        );
    }

    /// Path one of two: the renewal itself is refused. Driven straight at
    /// `renew_lease` rather than through `leader_applies`, because inside the
    /// `select!` a stamp failure would produce the same error and the test would
    /// pass without this path working at all.
    #[tokio::test]
    async fn a_refused_renewal_ends_the_lease_fatally() {
        let db = unmigrated().await;
        db.query(
            "DEFINE TABLE migration_lock SCHEMALESS;
             CREATE migration_lock:boot SET holder = 'somebody-else', applies = 0,
                 claimed_at = time::unix(time::now()) * 1000;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

        // Only ever resolves on a loss, so reaching this line is the assertion;
        // the timeout is there to fail the test instead of hanging it.
        let err = tokio::time::timeout(Duration::from_secs(5), super::renew_lease(&db, "me"))
            .await
            .expect("a renewal refused by the lock must resolve, not keep renewing");
        assert!(
            err.to_string().contains("lost the boot migration lock"),
            "{err}"
        );
    }

    /// Path two: the lock is stolen *after* the last renewal, so nothing refuses
    /// us until the stamp. The thief strikes while the DDL is running, far
    /// inside the renewal interval, which is exactly the window the stamp
    /// backstop exists for.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_lock_stolen_after_the_last_renewal_is_caught_by_the_stamp() {
        let db = unmigrated().await;
        db.query(
            "DEFINE TABLE migration_lock SCHEMALESS;
             CREATE migration_lock:boot SET holder = 'me', applies = 0,
                 claimed_at = time::unix(time::now()) * 1000;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
        let thief = tokio::spawn({
            let db = db.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(5)).await;
                db.query("UPDATE migration_lock:boot SET holder = 'thief'")
                    .await
                    .unwrap()
                    .check()
                    .unwrap();
            }
        });

        let err = super::leader_applies(&db, "me", admin())
            .await
            .expect_err("a leader that cannot stamp its own lock must not report success");
        thief.await.unwrap();
        assert!(
            err.to_string().contains("lost the boot migration lock"),
            "{err}"
        );
        assert_eq!(
            applies(&db).await,
            0,
            "and the lock is not stamped as applied by us"
        );
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
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/domain/user.rs"))
                .expect("read user.rs");
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

    /// The lock's table name is spelled in six places (the DDL and every
    /// statement), and a rename that misses one would split the election in two
    /// silently — every process would then "win" its own lock.
    #[test]
    fn every_lock_statement_names_the_lock_table() {
        for sql in [
            crate::constant::MIGRATION_LOCK_DDL,
            crate::constant::MIGRATION_LOCK_CLAIM,
            crate::constant::MIGRATION_LOCK_STATE,
            crate::constant::MIGRATION_LOCK_TAKEOVER,
            crate::constant::MIGRATION_LOCK_HEARTBEAT,
            crate::constant::MIGRATION_LOCK_STAMP,
        ] {
            assert!(
                sql.contains(MIGRATION_LOCK_TABLE),
                "{sql} does not name {MIGRATION_LOCK_TABLE}"
            );
        }
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
