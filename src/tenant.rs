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
use crate::database::{
    Database, create_database_sql, is_duplicate_database, migrate_school, school_pool,
    unique_violation,
};
use crate::domain::timestamp::Timestamp;
use crate::error::{AppError, ValidationError};
use crate::module::ModuleSet;
use crate::telemetry::Metrics;
use crate::web::tenant_state::ResolvedTenant;

/// The school every in-memory test bootstrap got and every test suite still
/// names. Not special to the code — just the one name the test harness and
/// the suites agree on.
pub const DEMO_SLUG: &str = "demo";

/// Slugs that name something other than a school: the builder cookie prefix and
/// the control database itself. Refused at construction, so neither a login nor
/// a `CREATE DATABASE` can ever be aimed at them.
pub const RESERVED_SLUGS: [&str; 2] = ["builder", "control"];

/// A school's identity everywhere it is named: the cookie prefix, the database
/// name, and the files subdirectory. The character set is deliberately the
/// intersection of what all three accept — lowercase alphanumerics and `-`,
/// starting with an alphanumeric. It is also what makes the database names
/// injective: `school_db_name` maps `-` to `_`, and `_` is not in the slug
/// charset, so two slugs can never collide onto one database.
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

/// A school database's name: `{control_db}_school_{slug}`, the slug's `-`
/// spelled `_` (Postgres identifiers allow `-` only quoted, and the name is
/// built, not user-typed). Injective — see [`Slug`]'s charset note.
pub fn school_db_name(control_db: &str, slug: &Slug) -> String {
    format!("{}_school_{}", control_db, slug.as_str().replace('-', "_"))
}

/// Whether a school may be reached at all. Suspension is immediate and total:
/// [`Tenants::get`] refuses before any handler runs, and login is no exception.
///
/// Stored as a bare lowercase string, like [`crate::domain::role::Role`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum SchoolStatus {
    Active,
    Suspended,
}

impl SchoolStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SchoolStatus::Active => "active",
            SchoolStatus::Suspended => "suspended",
        }
    }

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

/// A school's id **is** its slug — the registry can never hold two rows for
/// one school, and the database name is derived from the same word.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SchoolId(Slug);

impl SchoolId {
    pub fn from_slug(slug: &Slug) -> Self {
        Self(slug.clone())
    }

    pub fn key(&self) -> &str {
        self.0.as_str()
    }
}

/// A row in the control database's school registry.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct School {
    slug: Slug,
    name: String,
    status: SchoolStatus,
    created_at: Timestamp,
    /// Which product modules this school has bought, as their stored names.
    /// Kept as strings rather than a [`ModuleSet`] so a name this binary does
    /// not know (an older deploy reading a newer row) is data to ignore, not a
    /// deserialization failure that would lock the school out entirely.
    modules: Vec<String>,
}

impl School {
    /// The id is the slug; rebuilt rather than stored a second copy.
    pub fn get_id(&self) -> SchoolId {
        SchoolId::from_slug(&self.slug)
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
            "SELECT slug, name, status, created_at, modules FROM school WHERE slug = $1",
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
            "SELECT slug, name, status, created_at, modules FROM school
             ORDER BY created_at DESC, slug DESC
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

    /// Rename a school. The slug is immutable — it names a database and a
    /// directory, so it is the one field a rename must not touch.
    pub async fn update_name(
        slug: &Slug,
        name: &str,
        control: &Database,
    ) -> Result<School, AppError> {
        sqlx::query_as::<_, School>(
            "UPDATE school SET name = $1 WHERE slug = $2
             RETURNING slug, name, status, created_at, modules",
        )
        .bind(name)
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
        prewarmed: impl IntoIterator<Item = (Slug, Database)>,
    ) -> Tenants {
        let cache: HashMap<String, Database> = prewarmed
            .into_iter()
            .map(|(slug, db)| (slug.as_str().to_owned(), db))
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
    /// would enumerate the customer list), suspended → `403`.
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
                self.evict(slug).await;
                Err(AppError::Forbidden("school is suspended"))
            }
            Some(school) => Ok(ResolvedTenant {
                slug: slug.clone(),
                db: self.handle(slug).await?,
                modules: school.modules(),
            }),
        }
    }

    /// [`Tenants::get`] for builder tooling, which must still reach a suspended
    /// school to un-suspend or inspect it. Unknown → `404`, because a builder is
    /// authenticated and there is nothing to hide from them.
    pub async fn get_any_status(&self, slug: &Slug) -> Result<(Database, SchoolStatus), AppError> {
        let school = School::read(slug, &self.control)
            .await?
            .ok_or(AppError::NotFound)?;
        Ok((self.handle(slug).await?, school.status))
    }

    /// Register a school and bring its database into being: registry row first
    /// (so a lost race is a `409` and not a half-made database), then
    /// `CREATE DATABASE`, then the school schema.
    pub async fn create(
        &self,
        slug: &Slug,
        name: &str,
        modules: ModuleSet,
    ) -> Result<Database, AppError> {
        // Defense in depth: the builder API validates first (its message names
        // every violation at once), but the registry refuses an unsatisfiable
        // set too, so no future caller can persist one.
        modules.validate()?;
        let inserted = sqlx::query(
            "INSERT INTO school (slug, name, status, created_at, modules)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(slug.as_str())
        .bind(name)
        .bind(SchoolStatus::Active)
        .bind(Timestamp::now().as_millis())
        .bind(modules.names())
        .execute(&self.control)
        .await;
        if let Err(err) = inserted {
            // The slug is the table's only unique key, so a unique violation
            // here is exactly "taken" — a rival's row, or the row already
            // stood. Same verdict either way, as it always was.
            if unique_violation(&err).is_some() {
                return Err(AppError::Conflict("school slug already taken"));
            }
            return Err(err.into());
        }

        // Everything past the registry row rolls that row back on failure:
        // the row is what makes the slug "taken", and a slug taken by a
        // school that never came to exist is wedged for good. (A database
        // created before the failure is not a wedge: the adopt branch in
        // [`Self::bring_up`] picks it up, migrations being idempotent.)
        let db = match self.bring_up(slug).await {
            Ok(db) => db,
            Err(err) => {
                let _ = sqlx::query("DELETE FROM school WHERE slug = $1")
                    .bind(slug.as_str())
                    .execute(&self.control)
                    .await;
                return Err(err);
            }
        };
        let size = {
            let mut cache = self.cache.write().expect("tenant cache lock");
            cache.insert(slug.as_str().to_string(), db.clone());
            cache.len()
        };
        Self::report_size(size);
        // A test deployment's lease learns every school the deployment
        // mints, so a school created mid-test dies with the deployment.
        if let Some(leases) = &self.leases {
            leases.track(&school_db_name(&self.control_db, slug));
        }
        Ok(db)
    }

    /// Create (or adopt) the school's database and apply the school schema to
    /// it. Split out of [`Tenants::create`] so one `?` chain can be rolled
    /// back as a unit.
    async fn bring_up(&self, slug: &Slug) -> Result<Database, AppError> {
        let db_name = school_db_name(&self.control_db, slug);
        // `CREATE DATABASE` cannot run inside a transaction; `Pool::execute`
        // sends it as a bare autocommit statement. A name left over by a
        // create that died before its rollback is adopted, not fought.
        if let Err(err) = sqlx::query(sqlx::AssertSqlSafe(create_database_sql(
            "CREATE DATABASE",
            &db_name,
        )))
        .execute(&self.control)
        .await
        {
            if !is_duplicate_database(&err) {
                return Err(err.into());
            }
        }
        let pool = school_pool(&self.base, &db_name).await?;
        let migrated = migrate_school(&pool).await;
        if migrated.is_err() {
            pool.close().await;
            migrated?;
        }
        Ok(pool)
    }

    /// Flip a school between active and suspended. Suspending also drops the
    /// cached pool, so nothing that already resolved keeps serving.
    pub async fn set_status(&self, slug: &Slug, status: SchoolStatus) -> Result<(), AppError> {
        let updated = sqlx::query_as::<_, School>(
            "UPDATE school SET status = $1 WHERE slug = $2
             RETURNING slug, name, status, created_at, modules",
        )
        .bind(status)
        .bind(slug.as_str())
        .fetch_optional(&self.control)
        .await?;
        if updated.is_none() {
            return Err(AppError::NotFound);
        }
        if status == SchoolStatus::Suspended {
            self.evict(slug).await;
        }
        Ok(())
    }

    /// Sell (or take back) modules. Mirrors [`Tenants::set_status`] minus the
    /// eviction: entitlements are read off the registry row on every request,
    /// so a change takes effect on the next call and no cached pool carries a
    /// stale answer.
    pub async fn set_modules(&self, slug: &Slug, modules: &ModuleSet) -> Result<(), AppError> {
        modules.validate()?;
        let updated = sqlx::query_as::<_, School>(
            "UPDATE school SET modules = $1 WHERE slug = $2
             RETURNING slug, name, status, created_at, modules",
        )
        .bind(modules.names())
        .bind(slug.as_str())
        .fetch_optional(&self.control)
        .await?;
        if updated.is_none() {
            return Err(AppError::NotFound);
        }
        Ok(())
    }

    /// Delete a school outright: its data, then its registry row, then its
    /// pool. Irreversible on purpose — there is no soft-delete state, that is
    /// what [`SchoolStatus::Suspended`] is for.
    pub async fn drop(&self, slug: &Slug) -> Result<(), AppError> {
        // Close the pool first: nothing should be dialling a school whose
        // database is about to go away, and `WITH (FORCE)` should never find
        // one of our connections to kill.
        self.evict(slug).await;
        sqlx::query("DELETE FROM school WHERE slug = $1")
            .bind(slug.as_str())
            .execute(&self.control)
            .await?;
        let db_name = school_db_name(&self.control_db, slug);
        sqlx::query(sqlx::AssertSqlSafe(
            create_database_sql("DROP DATABASE IF EXISTS", &db_name) + " WITH (FORCE)",
        ))
        .execute(&self.control)
        .await?;
        Ok(())
    }

    /// Forget a school's pool and close it. The next request dials a fresh
    /// one.
    ///
    /// Nothing depends on eviction for correctness — [`Tenants::get`]
    /// re-reads the registry row on every request, so a suspension is refused
    /// whether or not a pool survived it. [`Tenants::drop`], which does mean
    /// to destroy the store, evicts unconditionally.
    pub async fn evict(&self, slug: &Slug) {
        if let Some(pool) = self.forget(slug) {
            pool.close().await;
        }
    }

    fn forget(&self, slug: &Slug) -> Option<Database> {
        let (removed, size) = {
            let mut cache = self.cache.write().expect("tenant cache lock");
            (cache.remove(slug.as_str()), cache.len())
        };
        if removed.is_some() {
            tracing::info!(school = %slug, "tenant pool evicted");
            Self::report_size(size);
        }
        removed
    }

    /// The cached pool, or a freshly dialled one.
    async fn handle(&self, slug: &Slug) -> Result<Database, AppError> {
        if let Some(db) = self
            .cache
            .read()
            .expect("tenant cache lock")
            .get(slug.as_str())
        {
            return Ok(db.clone());
        }
        let db = school_pool(&self.base, &school_db_name(&self.control_db, slug)).await?;
        // Losing the race here is harmless — both pools are equivalent — but
        // the winner is kept so two requests never diverge onto two pools.
        let (db, size) = {
            let mut cache = self.cache.write().expect("tenant cache lock");
            let db = cache.entry(slug.as_str().to_string()).or_insert(db).clone();
            (db, cache.len())
        };
        tracing::info!(school = %slug, "tenant pool opened");
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

    fn slug(value: &str) -> Slug {
        Slug::try_new(value).expect(value)
    }

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
    /// schools sharing a database name would share every row in it. `-`
    /// becoming `_` is the only rewrite, and `_` is not in the slug charset,
    /// so no two slugs can land on one name.
    #[test]
    fn school_db_names_are_injective() {
        let control = "hezarfen_control";
        assert_eq!(
            school_db_name(control, &slug("demo")),
            "hezarfen_control_school_demo"
        );
        assert_eq!(
            school_db_name(control, &slug("ata-koleji")),
            "hezarfen_control_school_ata_koleji"
        );
        // Slugs are at least two characters, so the shortest pair stands in
        // for the old single-letter probe.
        for a in ["ab", "a-b", "abc", "a1b", "1a", "x-y-z"] {
            for b in ["ab", "a-b", "abc", "a1b", "1a", "x-y-z"] {
                if a != b {
                    assert_ne!(
                        school_db_name(control, &slug(a)),
                        school_db_name(control, &slug(b)),
                        "{a:?} and {b:?} must not share a database"
                    );
                }
            }
        }
        // The control database's own name is never a school's name: the
        // suffix keeps the namespaces apart.
        assert_ne!(school_db_name(control, &slug("school")), control);
    }
}
