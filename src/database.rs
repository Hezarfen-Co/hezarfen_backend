//! PostgreSQL connections (sqlx) + the two migration sets + the
//! guarded-transaction retry loop.
//!
//! Two sets of migrations live side by side and never share a tracking table:
//! `migrations/control` for the control database (schools, builders, the
//! shared rate-limit window) and `migrations/school` for every school
//! database. They are applied by two separate [`sqlx::migrate::Migrator`]s —
//! the compile-time `migrate!` macro cannot be aimed at two directories, so
//! each is constructed at runtime from its path under `CARGO_MANIFEST_DIR`.
//! Their table names are disjoint by design, which is what lets one prepare
//! database carry the union for the compile-time `query!` checks.

use std::path::Path;
use std::time::Duration;

use sqlx::migrate::{MigrateError, Migrator};
use sqlx::postgres::{PgConnectOptions, PgConnection, PgPoolOptions};
use sqlx::PgPool;

use crate::config::Config;
use crate::constant::{CAP_WRITE_BACKOFF_MS, CAP_WRITE_TRIES, CHATBOT_PENDING_STALE_SECS};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{Password, Username};
use crate::error::AppError;
use crate::tenant::Tenants;

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
        AppError::Internal(format!("invalid DATABASE_URL ({}): {err}", cfg.database_url))
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
pub(crate) async fn school_pool(base: &PgConnectOptions, db_name: &str) -> Result<PgPool, AppError> {
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

/// The `migrations/<set>` directory under `CARGO_MANIFEST_DIR`. Two sets
/// cannot both feed the compile-time `migrate!` macro, so both are resolved
/// from their paths at runtime.
fn migrations_path(set: &str) -> std::path::PathBuf {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations")).join(set)
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
    if let Err(err) =
        sqlx::query(sqlx::AssertSqlSafe(create_database_sql("CREATE DATABASE", name)))
            .execute(control)
            .await
    {
        if !is_duplicate_database(&err) {
            return Err(err.into());
        }
    }
    // One migrator connection, made and closed: the template is never served.
    let pool = pool_options(1).connect_with(base.clone().database(name)).await?;
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
    Err(last.map(AppError::Db).unwrap_or_else(|| {
        AppError::Internal("a guarded transaction never ran".into())
    }))
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

