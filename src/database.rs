//! PostgreSQL connections (sqlx) + the two migration sets + the
//! guarded-transaction retry loop.
//!
//! Two sets of migrations live side by side and never share a tracking table:
//! `migrations/control` for the control database (schools, builders, the
//! shared rate-limit window) and `migrations/school` for every school
//! database. They are applied by two separate [`sqlx::migrate::Migrator`]s —
//! the compile-time `migrate!` macro cannot be aimed at two directories, so
//! each is resolved at runtime from the checkout's `CARGO_MANIFEST_DIR` or,
//! for a deployed binary, from beside the executable (see [`migrations_path`]).
//! Their table names are disjoint by design, which is what lets one prepare
//! database carry the union for the compile-time `query!` checks.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use sqlx::migrate::{MigrateError, Migrator};
use sqlx::postgres::{PgConnectOptions, PgConnection, PgPoolOptions};
use uuid::Uuid;

use crate::config::Config;
use crate::constant::{CAP_WRITE_BACKOFF_MS, CAP_WRITE_TRIES, CHATBOT_PENDING_STALE_SECS};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{Password, Username};
use crate::error::AppError;
use crate::module::ModuleSet;
use crate::tenant::{DEMO_SLUG, SchoolStatus, Slug, Tenants, school_db_name};

/// The shared database handle.
///
/// `PgPool` is an `Arc` around its internals, so cloning shares the one pool —
/// axum cloning the application state per request costs an atomic increment,
/// nothing more.
pub type Database = sqlx::PgPool;

/// Connect to the **control** database, apply the control schema, seed the
/// builder account and ensure the school-template database. School databases
/// are brought up separately, one per school, by [`crate::tenant::Tenants`].
///
/// The dial is retried forever (one-second cadence): the database is normally
/// a sibling container booting in parallel, and a process that exits on the
/// first refusal gets restarted by the runtime, fails again in ~100ms, and
/// burns its whole restart budget inside a second — leaving the backend
/// permanently down over a startup skew that would have cleared on its own.
/// Only the *connection* is retried. Bad credentials or a broken migration
/// still fail hard, since no amount of waiting fixes those.
pub async fn init(cfg: &Config) -> Result<Tenants, AppError> {
    // Validated before the connection, not after: half a credential pair is a
    // deployment mistake, and reporting it only after a successful dial buries
    // it under an outage that isn't one.
    let builder = builder_credentials(cfg)?;

    let (base, control_db) = parse_base(cfg)?;

    let control = loop {
        match boot_control(&base).await {
            Ok(pool) => break pool,
            Err(err) if boot_failure_retryable(&err) => {
                tracing::warn!(
                    "database at postgres://{}:{} unreachable ({err}) — retrying in 1s",
                    base.get_host(),
                    base.get_port(),
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(err) => return Err(err.into()),
        }
    };

    if let Some((username, password)) = builder {
        crate::service::builder::ensure(&control, username, password).await?;
    }

    // The template every school schema is validated against at boot: created
    // outside any transaction (`CREATE DATABASE` cannot run inside one —
    // `Pool::execute` runs bare autocommit statements), migrated once, then
    // closed. A first boot lost to a concurrent boot of the same deployment
    // adopts the winner's template via the duplicate-database swallow.
    let template = format!("{control_db}_school_template");
    ensure_template(&control, &base, &template).await?;

    Ok(Tenants::new(control, base, control_db))
}

/// Parse `DATABASE_URL` into dial options plus the control database's name —
/// the name every school database hangs off (`{control}_school_{slug}`).
pub(crate) fn parse_base(cfg: &Config) -> Result<(PgConnectOptions, String), AppError> {
    let opts: PgConnectOptions = cfg.database_url.parse().map_err(|err| {
        AppError::Internal(format!(
            "invalid DATABASE_URL ({}): {err}",
            cfg.database_url
        ))
    })?;
    let db = opts
        .get_database()
        .ok_or_else(|| {
            AppError::Internal(
                "DATABASE_URL names no database — the school databases are named after it".into(),
            )
        })?
        .to_string();
    Ok((opts, db))
}

/// The pool knobs. The control pool holds 10 connections; each school pool
/// holds 4 (a school's traffic is one building's worth, and every school pool
/// draws from the same server's connection budget).
fn pool_options(max_connections: u32) -> PgPoolOptions {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .min_connections(0)
        .acquire_timeout(Duration::from_secs(10))
        .idle_timeout(Duration::from_secs(300))
        .max_lifetime(Duration::from_secs(1800))
}

/// A pool over one school database with the school pool sizing. The schema is
/// the caller's business — [`migrate_school`] for a mint, nothing for a
/// re-dial of an existing school.
pub(crate) async fn school_pool(
    base: &PgConnectOptions,
    db_name: &str,
) -> Result<PgPool, AppError> {
    pool_options(4)
        .connect_with(base.clone().database(db_name))
        .await
        .map_err(Into::into)
}

/// A `CREATE DATABASE` / `DROP DATABASE` statement for a quoted identifier.
/// The names passed here are the control database's name (from
/// `DATABASE_URL`), the template's (`{control}_school_template`) or
/// [`crate::tenant::school_db_name`]'s output — never user input: the slug
/// charset is `[a-z0-9-]` and an embedded quote is doubled, so the statement
/// cannot be escaped from. That manual audit is what licenses the
/// `AssertSqlSafe` wrapper every caller applies (sqlx refuses non-literal
/// query strings without it).
pub(crate) fn create_database_sql(statement: &str, name: &str) -> String {
    format!("{statement} \"{}\"", name.replace('"', "\"\""))
}

async fn boot_control(base: &PgConnectOptions) -> Result<PgPool, sqlx::Error> {
    let pool = pool_options(10).connect_with(base.clone()).await?;
    // The migrator is built here rather than in [`migrator`]: its resolution
    // failure must classify as a boot error (retry or abort), not skip
    // straight to `AppError`.
    let migrator = Migrator::new(migrations_path("control").as_path())
        .await
        .map_err(sqlx::Error::from)?;
    migrator.run(&pool).await?;
    Ok(pool)
}

/// Is this boot failure the database not being *there* yet — the only thing
/// a second of patience can fix? Everything else (a bad migration, a bad
/// credential, a missing migrations directory) aborts startup immediately.
fn boot_failure_retryable(err: &sqlx::Error) -> bool {
    match err {
        sqlx::Error::Migrate(migrate) => match migrate.as_ref() {
            MigrateError::Execute(e) | MigrateError::ExecuteMigration(e, _) => is_dial_error(e),
            _ => false,
        },
        e => is_dial_error(e),
    }
}

/// Connection-class errors: the server was unreachable or the pool could not
/// hand out a connection. A migrations statement that *failed* is not here —
/// its error is `Error::Database`, and no sleep rewrites SQL.
fn is_dial_error(err: &sqlx::Error) -> bool {
    matches!(
        err,
        sqlx::Error::Io(_)
            | sqlx::Error::Tls(_)
            | sqlx::Error::PoolTimedOut
            | sqlx::Error::PoolClosed
            // A connection that died mid-handshake can present as protocol
            // breakage; a fresh dial either works or reports something else.
            | sqlx::Error::Protocol(_)
    )
}

/// The `migrations/<set>` directory: the one under `CARGO_MANIFEST_DIR` (the
/// dev/CI checkout) when that exists, else the one beside the executable.
/// Two sets cannot both feed the compile-time `migrate!` macro, so both are
/// resolved from their paths at runtime — and the compile-time path is baked
/// on the build machine, so a binary deployed outside a checkout (the release
/// layout ships `migrations/` next to the binary) must fall back to its own
/// directory or look for the schema where it was compiled.
fn migrations_path(set: &str) -> std::path::PathBuf {
    let manifest = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations")).join(set);
    if manifest.is_dir() {
        return manifest;
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let beside = dir.join("migrations").join(set);
        if beside.is_dir() {
            return beside;
        }
    }
    manifest
}

async fn migrator(set: &str) -> Result<Migrator, AppError> {
    Migrator::new(migrations_path(set).as_path())
        .await
        .map_err(|err| AppError::Internal(format!("cannot read the {set} migrations: {err}")))
}

/// Apply the school schema to a school database — the template at boot, every
/// newly minted school at create — then sweep the chatbot turns the previous
/// process owed an answer. Idempotent: an already-migrated database runs only
/// the sweep.
/// Apply the control schema (schools, builders, the shared rate-limit
/// window) to a control database. Idempotent: an already-migrated database
/// runs nothing. The boot path folds this into [`boot_control`]; suites that
/// re-boot an existing control database call it directly.
pub async fn migrate_control(pool: &PgPool) -> Result<(), AppError> {
    migrator("control")
        .await?
        .run(pool)
        .await
        .map_err(|err| AppError::from(sqlx::Error::from(err)))?;
    Ok(())
}

pub async fn migrate_school(pool: &PgPool) -> Result<(), AppError> {
    migrator("school")
        .await?
        .run(pool)
        .await
        .map_err(|err| AppError::from(sqlx::Error::from(err)))?;

    // An assistant turn is answered by a task in this process, so a restart
    // can leave its row `pending` with nobody left to complete it. Only rows
    // past the stale horizon (`CHATBOT_PENDING_STALE_SECS`) are certainly
    // abandoned though: a young one may still be being answered on the other
    // side of the AI bridge, and a deploy must not shoot down a turn
    // dispatched seconds ago. Nothing is lost by waiting — a reader already
    // presents an over-age `pending` row as failed, and the next boot sweeps
    // for real whatever crossed the horizon in the meantime.
    let now = Timestamp::now().as_millis();
    sqlx::query(
        "UPDATE chatbot_message SET status = 'failed', error_code = 'interrupted',
         completed_at = $1
         WHERE status = 'pending' AND created_at < $2",
    )
    .bind(now)
    .bind(now - CHATBOT_PENDING_STALE_SECS * 1_000)
    .execute(pool)
    .await?;
    Ok(())
}

/// Ensure the school-template database exists and carries the current school
/// schema. `CREATE DATABASE` runs on the control pool outside any
/// transaction; the loser of a race adopts the winner's database.
async fn ensure_template(
    control: &PgPool,
    base: &PgConnectOptions,
    name: &str,
) -> Result<(), AppError> {
    if let Err(err) = sqlx::query(sqlx::AssertSqlSafe(create_database_sql(
        "CREATE DATABASE",
        name,
    )))
    .execute(control)
    .await
        && !is_duplicate_database(&err) {
            return Err(err.into());
        }
    // One migrator connection, made and closed: the template is never served.
    let pool = pool_options(1)
        .connect_with(base.clone().database(name))
        .await?;
    let migrated = migrate_school(&pool).await;
    pool.close().await;
    migrated
}

/// `42P04` — the database already exists. The other side of every
/// "create it unless a rival got there first" move.
pub(crate) fn is_duplicate_database(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .is_some_and(|db| db.code().as_deref() == Some("42P04"))
}

/// The violated `UNIQUE` constraint's name, when `err` is a unique violation
/// (`23505`). The constraint name is the stable handle call sites map
/// refusals onto — `unique_violation(&e) == Some("school_pkey")` — and it is
/// borrowed from the error, not `'static`: the server owns the text.
pub fn unique_violation(err: &sqlx::Error) -> Option<&str> {
    let db = err.as_database_error()?;
    if db.code().as_deref() == Some("23505") {
        db.constraint()
    } else {
        None
    }
}

/// The violated `EXCLUDE` constraint's name, when `err` is an exclusion
/// violation (`23P01`) — the SQLSTATE an exclusion constraint raises, as
/// distinct from a plain `UNIQUE` (`23505`).
pub fn exclusion_violation(err: &sqlx::Error) -> Option<&str> {
    let db = err.as_database_error()?;
    if db.code().as_deref() == Some("23P01") {
        db.constraint()
    } else {
        None
    }
}

/// A foreign-key violation (`23503`): a parent row a statement names is gone.
pub fn foreign_key_violation(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .is_some_and(|db| db.code().as_deref() == Some("23503"))
}

/// Is this error the transaction *contended*, rather than decided? `40001`
/// (serialization failure) and `40P01` (deadlock) are Postgres telling two
/// writers to take turns — the whole reason the retry loop below exists.
pub fn is_retryable_tx(err: &sqlx::Error) -> bool {
    err.as_database_error().is_some_and(|db| {
        let code = db.code();
        code.as_deref() == Some("40001") || code.as_deref() == Some("40P01")
    })
}

/// [`is_retryable_tx`] plus the foreign-key violation. For **cascade**
/// transactions only: a child insert whose parent vanished mid-cascade means
/// a racing cascade is still in flight and a re-run sees the settled state —
/// but to a plain insert a `23503` is a real refusal (the parent is simply
/// gone), and retrying it would only burn the budget before returning the
/// same verdict.
pub fn is_retryable_cascade(err: &sqlx::Error) -> bool {
    is_retryable_tx(err) || foreign_key_violation(err)
}

/// Send one guarded `BEGIN…COMMIT` transaction, re-running it while Postgres
/// answers "contended, retry", and hand back the closure's value.
///
/// The closure writes through the transaction's connection (`&mut
/// PgConnection`), returns its value, and reports refusals as ordinary `Err`s
/// carrying exactly the `AppError` the web layer owes the caller. A refusal
/// is a *decision*, so it outranks the retry loop — only `AppError::Db`
/// errors that [`is_retryable_tx`] (or, when `cascade`, [`is_retryable_cascade`])
/// recognise are re-sent; everything else comes straight back.
///
/// Isolation is Postgres's default **READ COMMITTED** everywhere. Invariants
/// that span rows are enforced by single-statement CTEs or row locks
/// (`FOR UPDATE` / `FOR NO KEY UPDATE`) inside the closure, not by stronger
/// isolation.
/// The closure is an [`AsyncFnMut`] (edition-2024 async closure, or a plain
/// closure whose async block the compiler infers as such) because every real
/// call site's future borrows the connection — a named `Fut` generic cannot
/// express that lifetime relationship.
///
/// Isolation is Postgres's default **READ COMMITTED** everywhere. Invariants
/// that span rows are enforced by single-statement CTEs or row locks
/// (`FOR UPDATE` / `FOR NO KEY UPDATE`) inside the closure, not by stronger
/// isolation; if a transaction genuinely cannot be row-locked, escalate that
/// one to `SERIALIZABLE` (the retry loop already covers the `40001` it can
/// then raise) and record the reason in its own call site's comment.
///
/// The budget and backoff are the guard layer's usual: `CAP_WRITE_TRIES`
/// rounds, `backoff`'s exponential-with-jitter wait, attempt zero waiting
/// not at all.
pub async fn tx_with_retry<T, F>(pool: &PgPool, cascade: bool, mut f: F) -> Result<T, AppError>
where
    F: AsyncFnMut(&mut PgConnection) -> Result<T, AppError>,
{
    let mut last: Option<sqlx::Error> = None;
    for attempt in 0..CAP_WRITE_TRIES {
        backoff(attempt).await;
        let mut tx = pool.begin().await?;
        match f(&mut tx).await {
            Ok(value) => {
                // A commit can itself lose the race — `40001` fires at COMMIT
                // in Postgres — so the commit sits inside the classification.
                match tx.commit().await {
                    Ok(()) => return Ok(value),
                    Err(err) if is_retryable(&err, cascade) => last = Some(err),
                    Err(err) => return Err(err.into()),
                }
            }
            Err(AppError::Db(err)) if is_retryable(&err, cascade) => {
                // The losing round wrote nothing durable, which is what makes
                // re-sending the whole of the recovery. Settle the abort
                // before the wait, so a connection is not left
                // idle-in-transaction across the backoff.
                let _ = tx.rollback().await;
                last = Some(err);
            }
            // A refusal is a decision. Same shape for a `Db` error the
            // classifier does not recognise: it failed for a reason waiting
            // cannot fix.
            Err(err) => return Err(err),
        }
    }
    Err(last
        .map(AppError::Db)
        .unwrap_or_else(|| AppError::Internal("a guarded transaction never ran".into())))
}

fn is_retryable(err: &sqlx::Error, cascade: bool) -> bool {
    if cascade {
        is_retryable_cascade(err)
    } else {
        is_retryable_tx(err)
    }
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

// ---- per-test databases -----------------------------------------------------
//
// Every test run gets a **private pair of databases** on the compose Postgres:
// a control database and the demo school's database, both named after one
// random suffix — `heztest_<16 hex>` and `heztest_<…>_school_demo` — so
// nextest's parallel processes neither collide nor see each other's rows. The
// school schema is not migrated per test: it is cloned from a shared template
// database whose name carries a hash of `migrations/school`, so a schema edit
// mints a fresh template exactly once and every test after that pays only a
// cheap `CREATE DATABASE … TEMPLATE`.
//
// Cleanup is a lease: every database a deployment minted is recorded in a
// [`TestDatabases`], and its `Drop` — fired when the deployment's last handle
// dies — sends `DROP DATABASE … WITH (FORCE)`. A best-effort janitor at first
// use per process sweeps what a crashed run left behind: only databases past
// a minimum age are candidates, and their `DROP` carries no `WITH (FORCE)`,
// so a live parallel test's databases (connected backends, young name) are
// never touched.
//
// These functions are `pub` rather than `#[cfg(test)]` because the
// integration tests live outside the crate, and `#[doc(hidden)]` because they
// are not part of the API. Their SQL is runtime-checked — the query-style
// rule exempts all test-side SQL from the macros.

/// The Postgres server the test databases are minted on. The default is the
/// compose stack's maintenance database (`.env.example` documents it).
const TEST_DATABASE_URL_ENV: &str = "HEZARFEN_TEST_DATABASE_URL";
const TEST_DATABASE_URL_DEFAULT: &str = "postgres://hezarfen:hezarfen@127.0.0.1:5432/postgres";

/// Every test database carries this prefix; the janitor only ever drops
/// databases it names.
const TEST_DB_PREFIX: &str = "heztest_";

/// How old a `heztest_%` database must be before the janitor will touch it.
/// Anything younger belongs to a test in a parallel process — nextest mints
/// them by the dozen in overlapping seconds.
const TEST_DB_LEFTOVER_MIN_AGE_MS: i64 = 5 * 60 * 1000;

/// The advisory-lock key the template's create-and-migrate critical section
/// holds (see [`ensure_test_template`]). An arbitrary constant — it only has
/// to be one nobody else on the server uses.
const TEST_TEMPLATE_LOCK_KEY: i64 = 0x6865_7A74_6500_0001;

/// The databases one test deployment minted, and the promise to drop them.
///
/// Cloning shares the lease; the `DROP` statements fire when the **last**
/// copy drops — which is why a [`Tenants`] can carry one: the databases die
/// exactly when the test deployment does, even though the `Router` and the
/// test body hold separate clones of the registry.
#[doc(hidden)]
#[derive(Clone)]
pub struct TestDatabases(Arc<TestLease>);

struct TestLease {
    /// How to reach the maintenance database when the drops run. Dial options,
    /// not a pool: the pools a test built belong to the test's runtime and
    /// cannot be borrowed from the drop thread after that runtime dies — the
    /// drop dials its own one-connection pool instead.
    maintenance: PgConnectOptions,
    names: Mutex<Vec<String>>,
}

impl TestDatabases {
    fn new(maintenance: PgConnectOptions) -> Self {
        Self(Arc::new(TestLease {
            maintenance,
            names: Mutex::new(Vec::new()),
        }))
    }

    pub(crate) fn track(&self, name: &str) {
        self.0
            .names
            .lock()
            .expect("test lease names")
            .push(name.to_owned());
    }

    /// The database names this lease still owes a drop. Diagnostics only.
    pub fn names(&self) -> Vec<String> {
        self.0.names.lock().expect("test lease names").clone()
    }
}

impl std::fmt::Debug for TestDatabases {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestDatabases")
            .field("names", &self.names())
            .finish()
    }
}

impl Drop for TestLease {
    fn drop(&mut self) {
        let names = std::mem::take(&mut *self.names.lock().expect("test lease names"));
        let maintenance = self.maintenance.clone();
        if names.is_empty() {
            return;
        }
        let run = move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test-lease drop runtime");
            runtime.block_on(async move {
                let Ok(maintenance) = pool_options(1).connect_with(maintenance).await else {
                    eprintln!(
                        "test lease: cannot reach the maintenance database to drop {names:?}"
                    );
                    return;
                };
                for name in &names {
                    let sql =
                        create_database_sql("DROP DATABASE IF EXISTS", name) + " WITH (FORCE)";
                    if let Err(err) = sqlx::query(sqlx::AssertSqlSafe(sql))
                        .execute(&maintenance)
                        .await
                    {
                        eprintln!("test lease: dropping {name} failed: {err}");
                    }
                }
                maintenance.close().await;
            });
        };
        // A lease usually dies inside the test's runtime (the last registry
        // handle drops at the end of the test future), and a runtime may not
        // be built inside another — so the drops always run on a dedicated
        // thread. The wait is bounded: a wedged server cannot hang the test's
        // teardown forever, and the janitor is the backstop for whatever
        // outlives it.
        match tokio::runtime::Handle::try_current() {
            Ok(_) => {
                let (done, done_rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    run();
                    let _ = done.send(());
                });
                let _ = done_rx.recv_timeout(Duration::from_secs(10));
            }
            Err(_) => run(),
        }
    }
}

/// A complete test deployment: a fresh control database plus the demo school,
/// whose database is cloned from the shared school template and registered in
/// the control registry under the slug every suite names ([`DEMO_SLUG`]).
///
/// The databases die with the returned registry's last handle (see
/// [`TestDatabases`]); the janitor sweeps anything a crashed run left behind.
#[doc(hidden)]
pub async fn init_test_tenants() -> Tenants {
    let maintenance = maintenance_pool().await;
    janitor(&maintenance).await;
    let base = test_base();
    let lease = TestDatabases::new(base.clone());

    let control_db = format!("{TEST_DB_PREFIX}{}", test_suffix());
    create_database(&maintenance, &control_db, None).await;
    lease.track(&control_db);
    let control = boot_control(&base.clone().database(&control_db))
        .await
        .unwrap_or_else(|err| panic!("migrate the test control database {control_db}: {err}"));

    let template = ensure_test_template(&maintenance, &base).await;

    let demo = Slug::try_new(DEMO_SLUG).expect("the demo slug");
    let school_db = school_db_name(&control_db, &demo);
    create_database(&maintenance, &school_db, Some(&template)).await;
    lease.track(&school_db);
    let school = school_pool(&base, &school_db)
        .await
        .unwrap_or_else(|err| panic!("dial the test demo school {school_db}: {err}"));

    // The registry row, exactly as [`Tenants::create`] would write it — but
    // without the per-school migration its `bring_up` runs, which the
    // template clone has just replaced.
    sqlx::query(
        "INSERT INTO school (slug, name, status, created_at, modules)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(DEMO_SLUG)
    .bind("Demo School")
    .bind(SchoolStatus::Active)
    .bind(Timestamp::now().as_millis())
    .bind(ModuleSet::all().names())
    .execute(&control)
    .await
    .expect("register the test demo school");

    Tenants::new_test_adopting(
        control,
        base.database(&control_db),
        control_db,
        lease,
        [(demo, school)],
    )
}

/// One school database cloned from the template, for the src-side unit tests
/// that exercise a single store with no tenancy around it. Keep the lease
/// alive for the database's lifetime (`let (db, _leases) = …` binds it to the
/// test's scope); dropping it early drops the database.
#[doc(hidden)]
pub async fn init_test_db() -> (Database, TestDatabases) {
    let maintenance = maintenance_pool().await;
    janitor(&maintenance).await;
    let base = test_base();
    let lease = TestDatabases::new(base.clone());
    let template = ensure_test_template(&maintenance, &base).await;
    let name = format!("{TEST_DB_PREFIX}{}", test_suffix());
    create_database(&maintenance, &name, Some(&template)).await;
    lease.track(&name);
    let db = school_pool(&base, &name)
        .await
        .unwrap_or_else(|err| panic!("dial the test school database {name}: {err}"));
    (db, lease)
}

/// The school template's name, and as a side effect its existence: created
/// and migrated on first use, adopted when something else already made it.
/// The name carries a hash of `migrations/school`, so a schema edit mints a
/// fresh template and every process converges on the same new one.
///
/// Competing first uses are real: nextest mints tests in parallel processes
/// and in parallel test threads, all overlapping on the same millisecond.
/// One server-wide advisory lock makes the create-and-migrate a critical
/// section — without it, two migrators interleave their DDL on the same
/// fresh database and one dies on a duplicate table mid-flight (observed).
/// The probe + `42P04` swallow are the belt to that brace: a template made
/// before the lock was ever taken is adopted, not fought.
async fn ensure_test_template(maintenance: &PgPool, base: &PgConnectOptions) -> String {
    static ENSURED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);
    let name = template_name();
    let ensured = &*ENSURED;
    if ensured.lock().expect("template set").contains(&name) {
        return name;
    }
    let mut session = maintenance
        .acquire()
        .await
        .unwrap_or_else(|err| panic!("lease a maintenance connection: {err}"));
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(TEST_TEMPLATE_LOCK_KEY)
        .execute(&mut *session)
        .await
        .unwrap_or_else(|err| panic!("lock the school template: {err}"));
    let exists = sqlx::query_as::<_, (i64,)>("SELECT count(*) FROM pg_database WHERE datname = $1")
        .bind(&name)
        .fetch_one(&mut *session)
        .await
        .map(|(count,)| count > 0)
        .unwrap_or_else(|err| panic!("probe for the school template {name}: {err}"));
    if !exists
        && let Err(err) = sqlx::query(sqlx::AssertSqlSafe(create_database_sql(
            "CREATE DATABASE",
            &name,
        )))
        .execute(&mut *session)
        .await
            && !is_duplicate_database(&err) {
                panic!("create the school template {name}: {err}");
            }
    let pool = pool_options(1)
        .connect_with(base.clone().database(&name))
        .await
        .unwrap_or_else(|err| panic!("dial the school template {name}: {err}"));
    let migrated = migrate_school(&pool).await;
    pool.close().await;
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(TEST_TEMPLATE_LOCK_KEY)
        .execute(&mut *session)
        .await
        .unwrap_or_else(|err| panic!("unlock the school template: {err}"));
    migrated.unwrap_or_else(|err| panic!("migrate the school template {name}: {err:?}"));
    ensured.lock().expect("template set").insert(name.clone());
    name
}

/// `heztest_tpl_school_<hash>` — the hash is over `migrations/school`'s file
/// names and bytes, sorted, so any schema edit changes it.
fn template_name() -> String {
    let mut migrations: Vec<std::path::PathBuf> = std::fs::read_dir(migrations_path("school"))
        .expect("read the school migrations directory")
        .map(|entry| entry.expect("migration directory entry").path())
        .collect();
    migrations.sort();
    let mut hasher = Sha256::new();
    for path in migrations {
        hasher.update(
            path.file_name()
                .expect("file name")
                .to_string_lossy()
                .as_bytes(),
        );
        hasher.update(std::fs::read(&path).expect("read a school migration"));
    }
    format!(
        "{TEST_DB_PREFIX}tpl_school_{}",
        &hex::encode(hasher.finalize())[..8]
    )
}

/// One sweep per process, at the first mint: `heztest_%` databases a crashed
/// run left behind. See the section header for why a candidate must be old
/// *and* unconnected before it is dropped.
async fn janitor(maintenance: &PgPool) {
    static SWEPT: LazyLock<()> = LazyLock::new(|| {});
    LazyLock::force(&SWEPT);
    let template = template_name();
    let leftovers = sqlx::query_as::<_, (String,)>(
        "SELECT datname FROM pg_database WHERE datname LIKE 'heztest\\_%' ESCAPE '\\'",
    )
    .fetch_all(maintenance)
    .await
    .unwrap_or_else(|err| panic!("list leftover test databases: {err}"));
    for (name,) in leftovers {
        if name == template || !sweep_candidate(&name) {
            continue;
        }
        // No `WITH (FORCE)`: a database with live backends belongs to a
        // running test in a parallel process, and the failed drop *is* the
        // skip.
        let _ = sqlx::query(sqlx::AssertSqlSafe(create_database_sql(
            "DROP DATABASE IF EXISTS",
            &name,
        )))
        .execute(maintenance)
        .await;
    }
}

/// Should this `heztest_%` database be swept? The suffix's leading eleven hex
/// digits are the mint time in unix milliseconds; anything that cannot be
/// read that way is either a template of an older schema hash (swept) or
/// junk (swept too).
fn sweep_candidate(name: &str) -> bool {
    let Some(minted_hex) = name.get(TEST_DB_PREFIX.len()..TEST_DB_PREFIX.len() + 11) else {
        return true;
    };
    match i64::from_str_radix(minted_hex, 16) {
        Ok(minted_ms) => Timestamp::now().as_millis() - minted_ms > TEST_DB_LEFTOVER_MIN_AGE_MS,
        Err(_) => name.starts_with(&format!("{TEST_DB_PREFIX}tpl_")),
    }
}

/// `<11 hex of unix-ms><5 hex random>`: the mint time, so the janitor can
/// tell a crashed run's leftovers from a parallel test's fresh databases,
/// plus just enough randomness that two processes minting in the same
/// millisecond never collide.
fn test_suffix() -> String {
    let minted_ms = Timestamp::now().as_millis() as u64;
    format!(
        "{minted_ms:011x}{}",
        &Uuid::new_v4().simple().to_string()[..5]
    )
}

/// `CREATE DATABASE` for a harness-minted name, optionally `TEMPLATE`-cloned.
/// Both names are generated here (prefix + hex suffix), never user input —
/// the same audit that licenses [`create_database_sql`]'s `AssertSqlSafe`
/// callers.
async fn create_database(maintenance: &PgPool, name: &str, template: Option<&str>) {
    let mut sql = create_database_sql("CREATE DATABASE", name);
    if let Some(template) = template {
        sql.push_str(&format!(" TEMPLATE \"{}\"", template.replace('"', "\"\"")));
    }
    if let Err(err) = sqlx::query(sqlx::AssertSqlSafe(sql))
        .execute(maintenance)
        .await
    {
        panic!("create the test database {name}: {err}");
    }
}

/// Dial options for the test Postgres — `HEZARFEN_TEST_DATABASE_URL` or the
/// compose default. The URL's database is the *maintenance* database the
/// `CREATE DATABASE`s run against; the harness names every database itself.
fn test_base() -> PgConnectOptions {
    let url = std::env::var(TEST_DATABASE_URL_ENV)
        .ok()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| TEST_DATABASE_URL_DEFAULT.to_owned());
    url.parse().unwrap_or_else(|err| {
        panic!("{TEST_DATABASE_URL_ENV}={url:?} is not a Postgres URL: {err}")
    })
}

/// A small pool over the maintenance database: the janitor's listing and
/// every `CREATE DATABASE` (DDL — not usable inside a transaction) go
/// through it, and each lease keeps a clone for its drops.
async fn maintenance_pool() -> PgPool {
    pool_options(2)
        .connect_with(test_base())
        .await
        .unwrap_or_else(|err| {
            panic!(
                "cannot reach the test Postgres (set {TEST_DATABASE_URL_ENV}, default \
                 {TEST_DATABASE_URL_DEFAULT}): {err} — is the compose stack up?"
            )
        })
}

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
