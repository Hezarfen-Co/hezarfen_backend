//! Multi-school tenancy: one PostgreSQL **database** per school on one
//! server, plus a **control** database holding the school registry, the
//! builder accounts that manage it, and the deployment-wide rate-limit
//! window.
//!
//! Isolation is the server's, not ours. A school database sees only its own
//! rows, so no handler needs a `WHERE school = …` it could forget: the school
//! is chosen once, at the edge (`crate::web::tenant_state::State`), and every
//! query underneath runs against that handle.
//!
//! # One pool per school, cached
//!
//! `crate::database::Database` is a `PgPool`, and a school's pool is its own
//! dial — switching databases on a shared connection is not a thing the pool
//! API offers, which is just as well: a cache miss dials a fresh pool
//! (`max_connections(4)`) pinned to the school's database for its lifetime,
//! and the cache is the only thing that finds it again.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde::Serialize;
use sqlx::postgres::PgConnectOptions;

use crate::constant::{MAX_SLUG_LEN, MIN_SLUG_LEN};
use uuid::Uuid;

use crate::database::{
    Database, applied_versions, create_database_sql, ensure_database, migrate_school,
    pending_school_migrations, school_pool, unique_violation,
};
use crate::domain::monotonic_id::next_uuid;
use crate::domain::timestamp::Timestamp;
use crate::error::{AppError, ValidationError};
use crate::module::ModuleSet;
use crate::telemetry::Metrics;

/// The school every in-memory test bootstrap got and every test suite still
/// names. Not special to the code — just the one name the test harness and
/// the suites agree on.
pub const DEMO_SLUG: &str = "demo";

/// Slugs that name something other than a school: the builder cookie prefix,
/// the control database itself, and the person cookie prefix. Refused at
/// construction, so neither a login nor a `CREATE DATABASE` can ever be
/// aimed at them.
pub const RESERVED_SLUGS: [&str; 3] = ["builder", "control", "person"];

/// A school's public label: the cookie prefix, the URL paths and the files
/// subdirectory. The character set is deliberately the intersection of what
/// all three accept — lowercase alphanumerics and `-`, starting with an
/// alphanumeric. It is no longer the school's identity: the registry row's
/// uuid is (see [`SchoolId`]), which is why a label rename can no longer
/// orphan anything structural.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, sqlx::Type)]
#[serde(transparent)]
#[sqlx(transparent)]
pub struct Slug(String);

/// `^[a-z0-9][a-z0-9-]{1,31}$`, spelled out. `regex` is a dev-dependency here
/// and this is five lines of `char` predicates — a runtime dependency would
/// cost more than it saves.
fn is_slug(value: &str) -> bool {
    let ok_len = (MIN_SLUG_LEN..=MAX_SLUG_LEN).contains(&value.len());
    let ok_first = value
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let ok_rest = value
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    ok_len && ok_first && ok_rest
}

impl Slug {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        if !is_slug(value) {
            return Err(ValidationError::Invalid {
                field: "school",
                reason: "must be 2-32 characters of a-z, 0-9 and -, starting with a letter or digit",
            });
        }
        if RESERVED_SLUGS.contains(&value) {
            return Err(ValidationError::Invalid {
                field: "school",
                reason: "this name is reserved",
            });
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Slug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A school database's name: `{control_db}_school_{uuid hex}` — the uuid's
/// lowercase hex without dashes (the contract's example:
/// `019732e3-7b00-7000-8000-00000000dead` →
/// `…_school_019732e37b007000800000000000dead`). Fixed-width and injective:
/// two uuids never share a database, and the slug appears nowhere, so a slug
/// rename can never orphan one. The server's 63-byte identifier cap can clip
/// the name's tail on deployments with a long control-database name — uuid
/// v7's random tail still separates schools far past any realistic registry
/// size, and creator and dialler truncate identically.
pub fn school_db_name(control_db: &str, id: Uuid) -> String {
    format!("{}_school_{}", control_db, id.simple())
}

/// Whether a school may be reached at all. Suspension is immediate and total:
/// [`Tenants::get`] refuses before any handler runs, and login is no exception.
///
/// [`SchoolStatus::Provisioning`] is the boot's bookkeeping, not the vendor's:
/// it marks the window between the registry row committing and the school's
/// database carrying the schema (the two cannot be one write). A client can
/// never ask for it — [`SchoolStatus::try_from_str`] refuses the word — and a
/// boot closes the window by finishing the provisioning or leaving the row for
/// the next one.
///
/// Stored as a bare lowercase string, like [`crate::domain::role::Role`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum SchoolStatus {
    Active,
    Suspended,
    Provisioning,
}

impl SchoolStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SchoolStatus::Active => "active",
            SchoolStatus::Suspended => "suspended",
            SchoolStatus::Provisioning => "provisioning",
        }
    }

    /// The statuses a *client* may name. `provisioning` is deliberately absent:
    /// it is the boot's word for a school that is being made, so a PATCH that
    /// asks for it is a validation error rather than a state anyone can enter.
    pub fn try_from_str(value: &str) -> Result<Self, ValidationError> {
        match value {
            "active" => Ok(SchoolStatus::Active),
            "suspended" => Ok(SchoolStatus::Suspended),
            _ => Err(ValidationError::Invalid {
                field: "status",
                reason: "must be one of: active, suspended",
            }),
        }
    }
}

/// A school's identity inside the control plane: the registry row's uuid
/// primary key, app-minted uuid v7. The database name and every structural
/// reference (person_school) mint from it; the slug — the public label — never.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SchoolId(Uuid);

impl SchoolId {
    /// Wrap a uuid read off a registry row — naming a school that already
    /// exists (a test evicting the demo pool, say) rather than minting one.
    pub fn from_uuid(id: Uuid) -> Self {
        Self(id)
    }

    /// The next id in write order — see
    /// [`crate::domain::monotonic_id::next_uuid`].
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    pub fn uuid(self) -> Uuid {
        self.0
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct School {
    id: Uuid,
    slug: Slug,
    name: String,
    status: SchoolStatus,
    created_at: Timestamp,
    /// Which product modules this school has bought, as their stored names —
    /// joined in from `school_module` by every read below, never a column of
    /// `school`. Kept as strings rather than a [`ModuleSet`] so a name this
    /// binary does not know (an older deploy reading a newer row) is data to
    /// ignore, not a deserialization failure that would lock the school out
    /// entirely.
    modules: Vec<String>,
}

impl School {
    /// The row's uuid — the identity the database name and every structural
    /// reference (person_school) mint from. The slug is not it.
    pub fn get_id(&self) -> SchoolId {
        SchoolId(self.id)
    }

    pub fn slug(&self) -> &Slug {
        &self.slug
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn status(&self) -> SchoolStatus {
        self.status
    }

    pub fn created_at(&self) -> Timestamp {
        self.created_at
    }

    /// The stored names as modules. An unknown name is dropped with a warning
    /// — see the field's comment: a school must never become unreachable
    /// because of a word this binary has not heard of.
    pub fn modules(&self) -> ModuleSet {
        let mut set = ModuleSet::empty();
        for name in &self.modules {
            match crate::module::Module::try_from_str(name) {
                Ok(module) => set.insert(module),
                Err(_) => tracing::warn!(
                    "school {} has an unknown module `{name}` — ignoring it",
                    self.slug
                ),
            }
        }
        set
    }

    pub async fn read(slug: &Slug, control: &Database) -> Result<Option<School>, AppError> {
        sqlx::query_as::<_, School>(
            "SELECT s.id, s.slug, s.name, s.status, s.created_at,
                    COALESCE(array_agg(sm.module) FILTER (WHERE sm.module IS NOT NULL), '{}') AS modules
             FROM school s LEFT JOIN school_module sm ON sm.school = s.id
             WHERE s.slug = $1
             GROUP BY s.id",
        )
        .bind(slug.as_str())
        .fetch_optional(control)
        .await
        .map_err(Into::into)
    }

    /// One page of the registry, newest first. Same `(rows, total)` shape every
    /// other list domain returns, so the builder API can wrap it in the usual
    /// [`crate::web::Page`] envelope. `None` limits to every row.
    pub async fn list(
        limit: Option<i64>,
        offset: i64,
        control: &Database,
    ) -> Result<(Vec<School>, i64), AppError> {
        let rows = sqlx::query_as::<_, School>(
            "SELECT s.id, s.slug, s.name, s.status, s.created_at,
                    COALESCE(array_agg(sm.module) FILTER (WHERE sm.module IS NOT NULL), '{}') AS modules
             FROM school s LEFT JOIN school_module sm ON sm.school = s.id
             GROUP BY s.id
             ORDER BY s.created_at DESC, s.slug DESC
             LIMIT $1 OFFSET $2",
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(control)
        .await?;
        let (total,): (i64,) = sqlx::query_as("SELECT count(*) FROM school")
            .fetch_one(control)
            .await?;
        Ok((rows, total))
    }

    /// Rename a school and/or flip its status — its display `name`, not its
    /// slug, which is immutable in this cut (it is the cookie prefix and the
    /// files subdirectory), though it no longer names the database: the uuid
    /// does.
    ///
    /// Both fields land in **one** statement, so a patch that carries both can
    /// never leave half of itself behind, and an omitted field is kept by
    /// `COALESCE` over the row being written — never over a snapshot the caller
    /// read, so a concurrent patch of the other field cannot be reverted
    /// either.
    pub async fn update(
        slug: &Slug,
        name: Option<&str>,
        status: Option<SchoolStatus>,
        control: &Database,
    ) -> Result<School, AppError> {
        sqlx::query_as::<_, School>(
            "WITH updated AS (
                 UPDATE school
                 SET name = COALESCE($1, name), status = COALESCE($2, status)
                 WHERE slug = $3
                 RETURNING id, slug, name, status, created_at
             )
             SELECT u.id, u.slug, u.name, u.status, u.created_at,
                    COALESCE(array_agg(sm.module) FILTER (WHERE sm.module IS NOT NULL), '{}') AS modules
             FROM updated u
             LEFT JOIN school_module sm ON sm.school = u.id
             GROUP BY u.id, u.slug, u.name, u.status, u.created_at",
        )
        .bind(name)
        .bind(status)
        .bind(slug.as_str())
        .fetch_optional(control)
        .await?
        .ok_or(AppError::NotFound)
    }
}

/// The school registry plus its live pools.
#[derive(Clone)]
pub struct Tenants {
    control: Database,
    /// Live school pools, keyed by the school's uuid hex (no dashes — the
    /// same form [`school_db_name`] spells). The slug never keys it: a pool
    /// belongs to a school's identity, not to its current label.
    cache: Arc<RwLock<HashMap<String, Database>>>,
    /// Dial options for the Postgres server the control pool came from — a
    /// school pool is the same dial with only the database swapped.
    base: PgConnectOptions,
    /// The control database's name; every school database is
    /// [`school_db_name`] of it.
    control_db: String,
    /// Databases a test deployment minted, adopted via
    /// [`Tenants::new_test_adopting`]; dropped with this registry's last
    /// handle. `None` in production, where databases outlive the process.
    leases: Option<crate::database::TestDatabases>,
}

/// Everything one registry read tells us about the school behind a request:
/// its handle and what it has bought. Cached in the request's extensions by
/// [`crate::web::tenant_state::resolve_tenant`], so a request that needs the
/// school twice (the shadow [`crate::web::tenant_state::State`] *and*
/// [`crate::web::CurrentUser`], which is most of them) reads the registry once.
#[derive(Clone)]
pub struct ResolvedTenant {
    pub slug: Slug,
    pub db: Database,
    pub modules: ModuleSet,
}

impl Tenants {
    /// Wrap an already-connected, already-migrated control pool. Production
    /// constructor — [`crate::database::init`] calls it once at boot.
    pub fn new(control: Database, base: PgConnectOptions, control_db: String) -> Tenants {
        Tenants {
            control,
            cache: Arc::new(RwLock::new(HashMap::new())),
            base,
            control_db,
            leases: None,
        }
    }

    /// Test constructor: wrap already-created databases and adopt their
    /// [`crate::database::TestDatabases`] lease, so everything the harness
    /// minted dies with this registry's last handle. `prewarmed` pools enter
    /// the cache directly — for a template clone there is no re-dial to win.
    #[doc(hidden)]
    pub fn new_test_adopting(
        control: Database,
        base: PgConnectOptions,
        control_db: String,
        leases: crate::database::TestDatabases,
        prewarmed: impl IntoIterator<Item = (SchoolId, Database)>,
    ) -> Tenants {
        let cache: HashMap<String, Database> = prewarmed
            .into_iter()
            .map(|(id, db)| (id.uuid().simple().to_string(), db))
            .collect();
        Tenants {
            control,
            cache: Arc::new(RwLock::new(cache)),
            base,
            control_db,
            leases: Some(leases),
        }
    }

    /// The databases this deployment minted, when it is a test deployment —
    /// for a suite that wants to name what will die with it. `None` in
    /// production.
    #[doc(hidden)]
    pub fn test_leases(&self) -> Option<&crate::database::TestDatabases> {
        self.leases.as_ref()
    }

    /// The control database: schools, builders, the shared rate-limit window.
    /// It is never re-purposed for a school's rows — see this module's header.
    pub fn control(&self) -> &Database {
        &self.control
    }

    /// The handle for a school a request may use, or the refusal a request
    /// gets: unknown → `401` (a wrong school is a wrong credential, and a `404`
    /// would enumerate the customer list), suspended → `403`, still being
    /// provisioned → `403` as well (its database has no schema to serve yet).
    ///
    /// corner-cut: the registry row is read on every request, so a suspension
    /// takes effect on the next call with no invalidation protocol. That is one
    /// extra control-db round trip per request; cache the row with a short TTL
    /// if it ever shows up in a profile — the eviction in [`Tenants::set_status`]
    /// is already there to make a cached decision safe.
    pub async fn get(&self, slug: &Slug) -> Result<Database, AppError> {
        Ok(self.resolve(slug).await?.db)
    }

    /// [`Tenants::get`] plus what that one registry read already knew: the
    /// school's module entitlements. Same verdicts, same single read — a
    /// caller that needs both must never pay for two.
    pub async fn resolve(&self, slug: &Slug) -> Result<ResolvedTenant, AppError> {
        match School::read(slug, &self.control).await? {
            None => Err(AppError::Unauthorized),
            Some(school) if school.status == SchoolStatus::Suspended => {
                self.evict(school.get_id()).await;
                Err(AppError::Forbidden("school is suspended"))
            }
            Some(school) if school.status == SchoolStatus::Provisioning => {
                self.evict(school.get_id()).await;
                Err(AppError::Forbidden("school is still being provisioned"))
            }
            Some(school) => Ok(ResolvedTenant {
                slug: slug.clone(),
                db: self.handle(school.get_id()).await?,
                modules: school.modules(),
            }),
        }
    }

    /// [`Tenants::get`] for builder tooling, which must still reach a suspended
    /// school to un-suspend or inspect it. Unknown → `404`, because a builder is
    /// authenticated and there is nothing to hide from them. A school that is
    /// still being provisioned is a `409`: the two routes that come through
    /// here (`enter`, `reset_admin_password`) need its database to answer, and a
    /// table-less one would surface as a `500` instead of "not ready yet".
    pub async fn get_any_status(&self, slug: &Slug) -> Result<(Database, SchoolStatus), AppError> {
        let school = School::read(slug, &self.control)
            .await?
            .ok_or(AppError::NotFound)?;
        if school.status == SchoolStatus::Provisioning {
            return Err(AppError::Conflict("school is still being provisioned"));
        }
        Ok((self.handle(school.get_id()).await?, school.status))
    }

    /// Register a school and bring its database into being: registry row first
    /// (so a lost race is a `409` and not a half-made database), then
    /// `CREATE DATABASE`, then the school schema. The caller mints the id —
    /// the web create path is the one minter — and it becomes the row's PK,
    /// the database name's source and the pool cache key.
    pub async fn create(
        &self,
        id: SchoolId,
        slug: &Slug,
        name: &str,
        modules: ModuleSet,
    ) -> Result<Database, AppError> {
        // Defense in depth: the builder API validates first (its message names
        // every violation at once), but the registry refuses an unsatisfiable
        // set too, so no future caller can persist one.
        modules.validate()?;
        // The registry row and its entitlements land as one transaction: a
        // school row never stands without the modules its creation asked for.
        let mut tx = self.control.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO school (id, slug, name, status, created_at)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id.uuid())
        .bind(slug.as_str())
        .bind(name)
        .bind(SchoolStatus::Provisioning)
        .bind(Timestamp::now().as_millis())
        .execute(&mut *tx)
        .await;
        if let Err(err) = inserted {
            // The slug's UNIQUE constraint (`school_slug`) is the only key a
            // rival can collide on — a fresh uuid v7 does not repeat. So a
            // unique violation here is exactly "taken": a rival's row, or the
            // row already stood. Same verdict either way, as it always was.
            if unique_violation(&err).is_some() {
                return Err(AppError::Conflict("school slug already taken"));
            }
            return Err(err.into());
        }
        for name in modules.names() {
            sqlx::query("INSERT INTO school_module (school, module) VALUES ($1, $2)")
                .bind(id.uuid())
                .bind(name)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;

        // Everything past the registry row rolls that row back on failure:
        // the row is what makes the slug "taken", and a slug taken by a
        // school that never came to exist is wedged for good. (A database
        // created before the failure is not a wedge: the adopt branch in
        // [`Self::bring_up`] picks it up, migrations being idempotent.)
        let db = match self.bring_up(id).await {
            Ok(db) => db,
            Err(err) => {
                let _ = sqlx::query("DELETE FROM school_module WHERE school = $1")
                    .bind(id.uuid())
                    .execute(&self.control)
                    .await;
                let _ = sqlx::query("DELETE FROM school WHERE id = $1")
                    .bind(id.uuid())
                    .execute(&self.control)
                    .await;
                // The rows go first — a slug taken by a school that never came
                // to exist is wedged for good — then the database, which is the
                // expensive half and whose name nothing will ever reuse (the id
                // is minted per create, never replayed). Best-effort, but not
                // silent: a failure here is a half-provisioned database nobody
                // can see or reach, so it earns the one log line this path has.
                if let Err(leak) = sqlx::query(sqlx::AssertSqlSafe(
                    create_database_sql(
                        "DROP DATABASE IF EXISTS",
                        &school_db_name(&self.control_db, id.uuid()),
                    ) + " WITH (FORCE)",
                ))
                .execute(&self.control)
                .await
                {
                    tracing::warn!(
                        "failed to drop the database of a school whose create failed: {leak}"
                    );
                }
                return Err(err);
            }
        };
        let size = {
            let mut cache = self.cache.write().expect("tenant cache lock");
            cache.insert(id.uuid().simple().to_string(), db.clone());
            cache.len()
        };
        Self::report_size(size);
        // A test deployment's lease learns every school the deployment
        // mints, so a school created mid-test dies with the deployment.
        if let Some(leases) = &self.leases {
            leases.track(&school_db_name(&self.control_db, id.uuid()));
        }
        Ok(db)
    }

    /// Create (or adopt) the school's database and apply the school schema to
    /// it. Split out of [`Tenants::create`] so one `?` chain can be rolled
    /// back as a unit.
    async fn bring_up(&self, id: SchoolId) -> Result<Database, AppError> {
        let db_name = school_db_name(&self.control_db, id.uuid());
        // `CREATE DATABASE` cannot run inside a transaction; `Pool::execute`
        // sends it as a bare autocommit statement. A name left over by a
        // create that died before its rollback is adopted, not fought.
        ensure_database(&self.control, &db_name).await?;
        let pool = school_pool(&self.base, &db_name).await?;
        let migrated = migrate_school(&pool).await;
        if migrated.is_err() {
            pool.close().await;
            migrated?;
        }
        // The schema is there, so the school stops saying it is being made
        // (see [`SchoolStatus::Provisioning`]). Both callers want exactly this:
        // the create that minted the row a moment ago, and the boot that is
        // finishing one a previous boot left behind.
        sqlx::query("UPDATE school SET status = $1 WHERE id = $2")
            .bind(SchoolStatus::Active)
            .bind(id.uuid())
            .execute(&self.control)
            .await?;
        Ok(pool)
    }

    /// Flip a school between `active` and `suspended` — the one-field form of
    /// [`School::update`], which is where the write itself lives. Suspending
    /// also drops the cached pool, so nothing that already resolved keeps
    /// serving.
    pub async fn set_status(&self, slug: &Slug, status: SchoolStatus) -> Result<(), AppError> {
        let school = School::update(slug, None, Some(status), &self.control).await?;
        if status == SchoolStatus::Suspended {
            self.evict(school.get_id()).await;
        }
        Ok(())
    }

    /// Sell (or take back) modules. Mirrors [`Tenants::set_status`] minus the
    /// eviction: entitlements are read off the registry row on every request,
    /// so a change takes effect on the next call and no cached pool carries a
    /// stale answer.
    pub async fn set_modules(&self, slug: &Slug, modules: &ModuleSet) -> Result<(), AppError> {
        modules.validate()?;
        // The swap is parent + children in one transaction: take the registry
        // row's lock so racing sellers serialize, then replace the whole shelf.
        let mut tx = self.control.begin().await?;
        let id: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM school WHERE slug = $1 FOR UPDATE")
                .bind(slug.as_str())
                .fetch_optional(&mut *tx)
                .await?;
        let Some(id) = id else {
            return Err(AppError::NotFound);
        };
        sqlx::query("DELETE FROM school_module WHERE school = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        for name in modules.names() {
            sqlx::query("INSERT INTO school_module (school, module) VALUES ($1, $2)")
                .bind(id)
                .bind(name)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Delete a school outright: its data, then its registry row, then its
    /// pool. Irreversible on purpose — there is no soft-delete state, that is
    /// what [`SchoolStatus::Suspended`] is for.
    pub async fn drop(&self, slug: &Slug) -> Result<(), AppError> {
        // The id comes off the row: the database is named by the uuid, not
        // the slug. A row already gone means the goal state stands — nothing
        // to destroy left.
        let Some(school) = School::read(slug, &self.control).await? else {
            return Ok(());
        };
        let id = school.get_id();
        // Close the pool first: nothing should be dialling a school whose
        // database is about to go away, and `WITH (FORCE)` should never find
        // one of our connections to kill.
        self.evict(id).await;
        // The database goes first — it is the half that can fail on its own
        // (`DROP DATABASE` cannot join a transaction) — and everything after it
        // is one transaction. A failure can then only leave "database gone,
        // registry rows intact", which re-running this same call completes,
        // rather than a destroyed membership set on a school that still stands
        // (every one of its people locked out, nothing able to put them back).
        let db_name = school_db_name(&self.control_db, id.uuid());
        sqlx::query(sqlx::AssertSqlSafe(
            create_database_sql("DROP DATABASE IF EXISTS", &db_name) + " WITH (FORCE)",
        ))
        .execute(&self.control)
        .await?;
        // The control-plane rows pointing at the school by uuid — membership
        // and entitlement alike — go as one write: `ON DELETE NO ACTION` would
        // refuse the registry delete while any stand. Person sessions carry no
        // school, so only memberships go — the persons themselves survive (a
        // person is a global account, not the school's).
        let mut tx = self.control.begin().await?;
        crate::db::person::delete_memberships_by_school(&mut *tx, &id).await?;
        sqlx::query("DELETE FROM school_module WHERE school = $1")
            .bind(id.uuid())
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM school WHERE id = $1")
            .bind(id.uuid())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        Ok(())
    }

    /// Finish the schools a previous boot left half-made.
    ///
    /// A school's registry row commits before its database exists — the two
    /// cannot be one write, `CREATE DATABASE` being the one statement Postgres
    /// refuses inside a transaction — so a boot that dies in between leaves a
    /// row marked [`SchoolStatus::Provisioning`] and no schema behind it. This
    /// runs the half that boot could not: create the database, migrate it, and
    /// take the row to `active` (all of it [`Tenants::bring_up`]). A school that
    /// fails again stays `provisioning` and is retried on the next boot, and the
    /// builder can always `DELETE` it.
    ///
    /// What it cannot do is seed a first admin — that only ever lived in the web
    /// create path, after this point — so a finished school may have no account
    /// anybody can log into. The builder sees it in the list and either
    /// `DELETE`s it and creates it again, or uses an account it already has.
    pub async fn reconcile_provisioning(&self) -> Result<(), AppError> {
        let rows: Vec<(Uuid,)> =
            sqlx::query_as("SELECT id FROM school WHERE status = $1 ORDER BY created_at")
                .bind(SchoolStatus::Provisioning)
                .fetch_all(&self.control)
                .await?;
        for (id,) in rows {
            let id = SchoolId(id);
            match self.bring_up(id).await {
                Ok(pool) => {
                    // Nothing has dialled it: this boot's pool is the cache's
                    // only candidate and no request exists yet, so closing it
                    // costs nothing and keeps the gauge honest.
                    pool.close().await;
                    tracing::info!(
                        school = %id.uuid(),
                        "finished provisioning a school the previous boot left half-made — \
                         its first admin was never seeded, so DELETE and re-create it if nobody can enter"
                    );
                }
                Err(err) => tracing::warn!(
                    school = %id.uuid(),
                    "a school is still provisioning after a retry ({err}) — the next boot tries again"
                ),
            }
        }
        Ok(())
    }

    /// Apply the school schema to every school database in the registry.
    ///
    /// The invariant this restores: **every school database in the control
    /// database is at the school-schema head after boot**, because a school
    /// database is only ever migrated at mint or at boot. A school is minted
    /// through `bring_up` — the create path and
    /// [`Tenants::reconcile_provisioning`] both — with whatever schema head
    /// the process that minted it carried, and nothing afterwards ever touches
    /// its schema: a re-dial applies no migrations (see
    /// `crate::database::school_pool`), and `reconcile_provisioning` only
    /// reaches rows still marked `provisioning`. A school minted before a
    /// migration landed therefore keeps yesterday's tables for good, and its
    /// routes answer `500` on whatever that migration added — the live
    /// deployment served `relation "podcast_job" does not exist` on
    /// `POST /podcast/jobs` for exactly this reason.
    ///
    /// Failure policy: **one school must never take the boot down.** A school
    /// that fails to migrate is logged at ERROR with its id and slug and left
    /// exactly as it stood — no registry row and no status is written here, so
    /// a failed school stays as usable (or unusable) as it was before the
    /// sweep, and the next boot retries it. sqlx's tracking table is what
    /// makes the retry safe: a migration that died mid-flight is re-run, not
    /// half-believed.
    pub async fn sweep_school_schemas(&self) {
        let rows: Vec<(Uuid, String)> =
            match sqlx::query_as("SELECT id, slug FROM school ORDER BY created_at")
                .fetch_all(&self.control)
                .await
            {
                Ok(rows) => rows,
                // The control database was migrated moments ago in this same
                // boot, so this is a surprise and not a state — but a boot
                // that refuses to serve over it would be down for every school
                // at once, which the failure policy above will not have.
                Err(err) => {
                    tracing::error!("school schema sweep could not list the registry: {err}");
                    return;
                }
            };
        let total = rows.len();
        let mut migrated = 0usize;
        for (id, slug) in rows {
            let id = SchoolId(id);
            match self.sweep_school(id).await {
                Ok(pending) if pending > 0 => {
                    migrated += 1;
                    tracing::info!(
                        school = %id.uuid(),
                        slug = %slug,
                        "school schema sweep applied {pending} migration(s) to a school behind the head"
                    );
                }
                Ok(_) => {}
                Err(err) => tracing::error!(
                    school = %id.uuid(),
                    slug = %slug,
                    "school schema sweep failed — this school is left as it was and the next boot \
                     retries it: {err}"
                ),
            }
        }
        tracing::info!("school schema sweep: {total} schools, {migrated} migrated");
    }

    /// One school's step of [`Tenants::sweep_school_schemas`]: dial it, bring
    /// it to head, close the pool. The pool is made and closed inside this
    /// call — a school this boot never serves must not leave one behind (the
    /// cache is what keeps a served school's pool, and the sweep is not a
    /// serve), and the close has to sit outside [`Tenants::bring_to_head`] so
    /// no `?` on its error path can skip it.
    async fn sweep_school(&self, id: SchoolId) -> Result<usize, AppError> {
        let db_name = school_db_name(&self.control_db, id.uuid());
        let pool = school_pool(&self.base, &db_name).await?;
        let swept = Self::bring_to_head(&pool).await;
        pool.close().await;
        swept
    }

    /// Apply whatever the school schema has that `pool` has not — idempotent,
    /// so a school already at head costs one `_sqlx_migrations` read and a
    /// no-op migrator run. Returns how many migrations were pending, which is
    /// what the sweep's per-school line reports.
    ///
    /// [`crate::database::migrate_school`], not the bare migrator: it also
    /// sweeps the chatbot and RAG turns the previous process owed an answer,
    /// which is exactly as due on a school being swept as on one being minted.
    async fn bring_to_head(pool: &Database) -> Result<usize, AppError> {
        let applied = applied_versions(pool).await?;
        let pending = pending_school_migrations(&applied).await?;
        migrate_school(pool).await?;
        Ok(pending)
    }

    /// Forget a school's pool and close it. The next request dials a fresh
    /// one.
    ///
    /// Nothing depends on eviction for correctness — [`Tenants::get`]
    /// re-reads the registry row on every request, so a suspension is refused
    /// whether or not a pool survived it. [`Tenants::drop`], which does mean
    /// to destroy the store, evicts unconditionally.
    pub async fn evict(&self, id: SchoolId) {
        if let Some(pool) = self.forget(id) {
            pool.close().await;
        }
    }

    fn forget(&self, id: SchoolId) -> Option<Database> {
        let key = id.uuid().simple().to_string();
        let (removed, size) = {
            let mut cache = self.cache.write().expect("tenant cache lock");
            (cache.remove(&key), cache.len())
        };
        if removed.is_some() {
            tracing::info!(school = %id.uuid(), "tenant pool evicted");
            Self::report_size(size);
        }
        removed
    }

    /// The cached pool, or a freshly dialled one.
    async fn handle(&self, id: SchoolId) -> Result<Database, AppError> {
        let key = id.uuid().simple().to_string();
        if let Some(db) = self.cache.read().expect("tenant cache lock").get(&key) {
            return Ok(db.clone());
        }
        let db = school_pool(&self.base, &school_db_name(&self.control_db, id.uuid())).await?;
        // Losing the race here is harmless — both pools are equivalent — but
        // the winner is kept so two requests never diverge onto two pools.
        let (db, size) = {
            let mut cache = self.cache.write().expect("tenant cache lock");
            let db = cache.entry(key).or_insert(db).clone();
            (db, cache.len())
        };
        tracing::info!(school = %id.uuid(), "tenant pool opened");
        Self::report_size(size);
        Ok(db)
    }

    /// Publish how many schools the cache holds. Called after every insert and
    /// every eviction, which are the only two things that move it — a gauge
    /// sampled anywhere else would report a number nobody can act on.
    fn report_size(size: usize) {
        Metrics::global().tenant_cache_size.record(size as u64, &[]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slug_names_a_school_and_nothing_reserved() {
        for good in ["demo", "a1", "ata-koleji", &"x".repeat(32)] {
            assert!(Slug::try_new(good).is_ok(), "{good:?} should be a slug");
        }
        // Reserved words would collide with the builder cookie prefix and the
        // control database itself — the two things a slug must never name.
        for bad in [
            "builder",
            "control",
            "Demo",
            "..",
            "",
            "x",
            "-lead",
            "has_underscore",
            "has.dot",
            &"x".repeat(33),
        ] {
            assert!(Slug::try_new(bad).is_err(), "{bad:?} must not be a slug");
        }
    }

    /// The database-per-school model lives or dies by this mapping: two
    /// schools sharing a database name would share every row in it. The name
    /// now mints from the school's uuid — lowercase hex without dashes, fixed
    /// width — so it is injective by construction, and the slug appears
    /// nowhere: a slug rename can never orphan a database.
    #[test]
    fn school_db_names_are_injective() {
        let control = "hezarfen_control";
        // The contract's example, pinned byte for byte.
        let id = Uuid::try_parse("019732e3-7b00-7000-8000-00000000dead").unwrap();
        assert_eq!(
            school_db_name(control, id),
            "hezarfen_control_school_019732e37b007000800000000000dead"
        );
        // Two uuids never collide onto one database, and no slug is in the
        // name — not even the slug the uuid was minted for.
        let other = Uuid::try_parse("019732e3-7b00-7000-8000-00000000beef").unwrap();
        assert_ne!(
            school_db_name(control, id),
            school_db_name(control, other),
            "two uuids must not share a database"
        );
        assert!(!school_db_name(control, id).contains("demo"));
        // The control database's own name is never a school's name: the
        // suffix keeps the namespaces apart.
        assert_ne!(school_db_name(control, id), control);
    }
}
