//! Multi-school tenancy: one SurrealDB **database** per school inside one
//! namespace, plus a **control** database holding the school registry, the
//! builder accounts that manage it, and the deployment-wide rate-limit window.
//!
//! Isolation is the store's, not ours. A school database sees only its own
//! rows, so no handler needs a `WHERE school = …` it could forget: the school
//! is chosen once, at the edge (`crate::web::tenant_state::State`), and every
//! query underneath runs against that handle.
//!
//! # One connection per school, never a clone
//!
//! [`crate::database::Database`] is `Arc<Surreal<Any>>` for the reason spelled
//! out there — `Surreal::clone()` mints a racing server-side session — and the
//! same reasoning forbids the obvious shortcut here: switching `use_db` on the
//! control handle would repoint *every* in-flight query on it. So a cache miss
//! dials a **new** connection, signs it in, and pins it to the school's
//! database for its lifetime.
//!
//! In-memory mode (tests) has no server to define databases on: each
//! `connect("memory")` is an independent datastore, so a school *is* its own
//! store and the cache is the only thing that finds it again.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde::Serialize;
use surrealdb::opt::auth::Root;
use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::config::Config;
use crate::constant::{MAX_SLUG_LEN, MIN_SLUG_LEN};
use crate::database::{Database, lost_the_race, migrate, migrate_control};
use crate::domain::page::PagedList;
use crate::domain::timestamp::Timestamp;
use crate::error::{AppError, ValidationError};
use crate::module::ModuleSet;
use crate::telemetry::Metrics;
use crate::web::tenant_state::ResolvedTenant;

pub(crate) const SCHOOL_TABLE: &str = "school";

/// The school every in-memory test bootstrap gets. Not special to the code —
/// just the one name the test harness and the suites agree on.
pub const DEMO_SLUG: &str = "demo";

/// Slugs that name something other than a school: the builder cookie prefix and
/// the control database itself. Refused at construction, so neither a login nor
/// a `DEFINE DATABASE` can ever be aimed at them.
pub const RESERVED_SLUGS: [&str; 2] = ["builder", "control"];

/// A school's identity everywhere it is named: the cookie prefix, the database
/// name, and the files subdirectory. The character set is deliberately the
/// intersection of what all three accept — lowercase alphanumerics and `-`,
/// starting with an alphanumeric — which is also what makes it safe to
/// interpolate into `DEFINE DATABASE` / `REMOVE DATABASE`, the two statements
/// SurrealQL gives no way to bind a name into.
#[derive(Debug, Clone, PartialEq, Eq, Hash, SurrealValue, Serialize)]
#[serde(transparent)]
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

/// A school slug as a SurrealQL identifier. A database name cannot be bound
/// as a parameter, so it is interpolated — and it MUST be backtick-quoted: an
/// unquoted `ata-koleji` parses as a subtraction, `2024school` as a duration,
/// `12345` as a number (SurrealDB 3.2). The `Slug` charset (`[a-z0-9-]`) can
/// never contain a backtick, so the quoting cannot be escaped from.
pub fn quoted_ident(slug: &Slug) -> String {
    format!("`{}`", slug.as_str())
}

/// Whether a school may be reached at all. Suspension is immediate and total:
/// [`Tenants::get`] refuses before any handler runs, and login is no exception.
///
/// Stored as a bare lowercase string, like [`crate::domain::role::Role`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, SurrealValue, Serialize)]
#[surreal(untagged, rename_all = "lowercase")]
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

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct SchoolId(RecordId);

impl SchoolId {
    /// The id **is** the slug, so the registry can never hold two rows for one
    /// school even if the unique index were dropped.
    pub fn from_slug(slug: &Slug) -> Self {
        Self(RecordId::new(SCHOOL_TABLE, slug.as_str()))
    }

    pub fn record(&self) -> RecordId {
        self.0.clone()
    }

    pub fn key(&self) -> &str {
        match &self.0.key {
            RecordIdKey::String(key) => key,
            _ => "",
        }
    }
}

/// A row in the control database's school registry.
#[derive(Debug, Clone, SurrealValue)]
pub struct School {
    id: SchoolId,
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
    pub fn get_id(&self) -> &SchoolId {
        &self.id
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
        Ok(control.select(SchoolId::from_slug(slug).record()).await?)
    }

    /// One page of the registry, newest first. Same `(rows, total)` shape every
    /// other list domain returns, so the builder API can wrap it in the usual
    /// [`crate::web::Page`] envelope.
    pub async fn list(
        limit: Option<i64>,
        offset: i64,
        control: &Database,
    ) -> Result<(Vec<School>, i64), AppError> {
        PagedList::new(SCHOOL_TABLE, "ORDER BY created_at DESC, id DESC")
            .run::<School>(limit, offset, control)
            .await
    }

    /// Rename a school. The slug is immutable — it names a database and a
    /// directory, so it is the one field a rename must not touch.
    pub async fn update_name(
        slug: &Slug,
        name: &str,
        control: &Database,
    ) -> Result<School, AppError> {
        let updated: Option<School> = control
            .query("UPDATE $id SET name = $name RETURN AFTER")
            .bind(("id", SchoolId::from_slug(slug).record()))
            .bind(("name", name.to_string()))
            .await?
            .check()?
            .take::<Vec<School>>(0)?
            .into_iter()
            .next();
        updated.ok_or(AppError::NotFound)
    }
}

/// How a school handle is minted. Remote dials the server the control handle
/// came from; `Mem` mints an independent embedded datastore per school.
#[derive(Debug)]
enum Mode {
    Remote {
        url: String,
        user: String,
        pass: String,
        ns: String,
    },
    Mem,
}

/// The school registry plus its live connections.
#[derive(Clone)]
pub struct Tenants {
    control: Database,
    cache: Arc<RwLock<HashMap<String, Database>>>,
    mode: Arc<Mode>,
}

impl Tenants {
    /// Wrap an already-connected, already-migrated control handle.
    pub fn new_remote(control: Database, cfg: &Config) -> Tenants {
        Tenants {
            control,
            cache: Arc::new(RwLock::new(HashMap::new())),
            mode: Arc::new(Mode::Remote {
                url: cfg.db_url.clone(),
                user: cfg.db_user.clone(),
                pass: cfg.db_pass.clone(),
                ns: cfg.db_ns.clone(),
            }),
        }
    }

    /// A fresh in-memory control database with the control schema applied, and
    /// no schools. For tests.
    pub async fn new_mem() -> Result<Tenants, AppError> {
        let control = surrealdb::engine::any::connect("memory").await?;
        control.use_ns("hezarfen").use_db("control").await?;
        migrate_control(&control).await?;
        Ok(Tenants {
            control: Arc::new(control),
            cache: Arc::new(RwLock::new(HashMap::new())),
            mode: Arc::new(Mode::Mem),
        })
    }

    /// A fresh in-memory registry that already knows `slug`, backed by a handle
    /// that already exists rather than a new store.
    ///
    /// This is how a suite simulates a reboot: the process (and so the registry,
    /// the connection cache and the control database) is new, while the school's
    /// data is the very data the previous process wrote.
    pub async fn new_mem_adopting(
        slug: &Slug,
        name: &str,
        db: Database,
    ) -> Result<Tenants, AppError> {
        let tenants = Tenants::new_mem().await?;
        let school = School {
            id: SchoolId::from_slug(slug),
            slug: slug.clone(),
            name: name.to_string(),
            status: SchoolStatus::Active,
            created_at: Timestamp::now(),
            modules: ModuleSet::all().names(),
        };
        let _: Option<School> = tenants
            .control
            .create(school.id.record())
            .content(school)
            .await?;
        tenants
            .cache
            .write()
            .expect("tenant cache lock")
            .insert(slug.as_str().to_string(), db);
        Ok(tenants)
    }

    /// The control database: schools, builders, the shared rate-limit window.
    /// Never `use_db`-switched — see this module's header.
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
                self.evict(slug);
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
    /// `DEFINE DATABASE`, then the school schema.
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
        let school = School {
            id: SchoolId::from_slug(slug),
            slug: slug.clone(),
            name: name.to_string(),
            status: SchoolStatus::Active,
            created_at: Timestamp::now(),
            modules: modules.names(),
        };
        let created: Result<Option<School>, surrealdb::Error> = self
            .control
            .create(school.id.record())
            .content(school.clone())
            .await;
        match created {
            Ok(Some(_)) => {}
            Ok(None) => return Err(AppError::Internal("failed to create the school".into())),
            // The id is the slug, so "already exists" is exactly "taken" — and
            // a lost transaction race is the same verdict reached differently.
            Err(err) if lost_the_race(&err) => {
                return Err(AppError::Conflict("school slug already taken"));
            }
            Err(err) => return Err(err.into()),
        }

        // Everything past the registry row rolls that row back on failure:
        // the row is what makes the slug "taken", and a slug taken by a
        // school that never came to exist is wedged for good (found by the
        // delete-path verifier: `ata-koleji` failed `DEFINE DATABASE` unquoted
        // and could then be neither re-created nor deleted).
        let db = match self.bring_up(slug).await {
            Ok(db) => db,
            Err(err) => {
                let _ = self
                    .control
                    .query("DELETE $id")
                    .bind(("id", SchoolId::from_slug(slug).record()))
                    .await;
                return Err(err);
            }
        };
        self.cache
            .write()
            .expect("tenant cache lock")
            .insert(slug.as_str().to_string(), db.clone());
        Ok(db)
    }

    /// Define (remote) or mint (memory) the school's database and apply the
    /// school schema to it. Split out of [`Tenants::create`] so one `?` chain
    /// can be rolled back as a unit.
    async fn bring_up(&self, slug: &Slug) -> Result<Database, AppError> {
        let db = match &*self.mode {
            Mode::Remote { .. } => {
                self.control
                    .query(format!(
                        "DEFINE DATABASE IF NOT EXISTS {}",
                        quoted_ident(slug)
                    ))
                    .await?
                    .check()?;
                self.connect(slug).await?
            }
            Mode::Mem => self.connect(slug).await?,
        };
        migrate(&db).await?;
        Ok(db)
    }

    /// Flip a school between active and suspended. Suspending also drops the
    /// cached handle, so nothing that already resolved keeps serving.
    pub async fn set_status(&self, slug: &Slug, status: SchoolStatus) -> Result<(), AppError> {
        let updated: Vec<School> = self
            .control
            .query("UPDATE $id SET status = $status RETURN AFTER")
            .bind(("id", SchoolId::from_slug(slug).record()))
            .bind(("status", status))
            .await?
            .check()?
            .take(0)?;
        if updated.is_empty() {
            return Err(AppError::NotFound);
        }
        if status == SchoolStatus::Suspended {
            self.evict(slug);
        }
        Ok(())
    }

    /// Sell (or take back) modules. Mirrors [`Tenants::set_status`] minus the
    /// eviction: entitlements are read off the registry row on every request,
    /// so a change takes effect on the next call and no cached handle carries a
    /// stale answer.
    pub async fn set_modules(&self, slug: &Slug, modules: &ModuleSet) -> Result<(), AppError> {
        modules.validate()?;
        let updated: Vec<School> = self
            .control
            .query("UPDATE $id SET modules = $modules RETURN AFTER")
            .bind(("id", SchoolId::from_slug(slug).record()))
            .bind(("modules", modules.names()))
            .await?
            .check()?
            .take(0)?;
        if updated.is_empty() {
            return Err(AppError::NotFound);
        }
        Ok(())
    }

    /// Delete a school outright: its data, then its registry row, then its
    /// handle. Irreversible on purpose — there is no soft-delete state, that is
    /// what [`SchoolStatus::Suspended`] is for.
    pub async fn drop(&self, slug: &Slug) -> Result<(), AppError> {
        if let Mode::Remote { .. } = &*self.mode {
            self.control
                .query(format!("REMOVE DATABASE IF EXISTS {}", quoted_ident(slug)))
                .await?
                .check()?;
        }
        self.control
            .query("DELETE $id")
            .bind(("id", SchoolId::from_slug(slug).record()))
            .await?
            .check()?;
        self.forget(slug);
        Ok(())
    }

    /// Forget a school's connection. The next request dials a fresh one.
    ///
    /// A no-op in memory mode, where the cached handle *is* the school's
    /// datastore and dropping it would delete the school's data rather than its
    /// connection. Nothing depends on eviction for correctness — [`Tenants::get`]
    /// re-reads the registry row on every request, so a suspension is refused
    /// whether or not a handle survived it. [`Tenants::drop`], which does mean to
    /// destroy the store, forgets it unconditionally.
    pub fn evict(&self, slug: &Slug) {
        if matches!(&*self.mode, Mode::Mem) {
            return;
        }
        self.forget(slug);
    }

    fn forget(&self, slug: &Slug) {
        let (removed, size) = {
            let mut cache = self.cache.write().expect("tenant cache lock");
            (cache.remove(slug.as_str()).is_some(), cache.len())
        };
        if removed {
            tracing::info!(school = %slug, "tenant connection evicted");
            Self::report_size(size);
        }
    }

    /// The cached handle, or a freshly dialled one.
    async fn handle(&self, slug: &Slug) -> Result<Database, AppError> {
        if let Some(db) = self
            .cache
            .read()
            .expect("tenant cache lock")
            .get(slug.as_str())
        {
            return Ok(db.clone());
        }
        let db = self.connect(slug).await?;
        // In-memory mode has nowhere for a school's rows to have survived a
        // cache miss, so a fresh store needs the schema. Remote already has it.
        if matches!(&*self.mode, Mode::Mem) {
            migrate(&db).await?;
        }
        // Losing the race here is harmless — both handles are equivalent — but
        // the winner is kept so two requests never diverge onto two sessions.
        let (db, size) = {
            let mut cache = self.cache.write().expect("tenant cache lock");
            let db = cache.entry(slug.as_str().to_string()).or_insert(db).clone();
            (db, cache.len())
        };
        tracing::info!(school = %slug, "tenant connection opened");
        Self::report_size(size);
        Ok(db)
    }

    /// Publish how many schools the cache holds. Called after every insert and
    /// every eviction, which are the only two things that move it — a gauge
    /// sampled anywhere else would report a number nobody can act on.
    fn report_size(size: usize) {
        Metrics::global().tenant_cache_size.record(size as u64, &[]);
    }

    /// One new connection, pinned to `slug`'s database for its lifetime.
    async fn connect(&self, slug: &Slug) -> Result<Database, AppError> {
        let db = match &*self.mode {
            Mode::Remote {
                url,
                user,
                pass,
                ns,
            } => {
                let db = surrealdb::engine::any::connect(url.clone()).await?;
                db.signin(Root {
                    username: user.clone(),
                    password: pass.clone(),
                })
                .await?;
                db.use_ns(ns.clone()).use_db(slug.as_str()).await?;
                db
            }
            Mode::Mem => {
                let db = surrealdb::engine::any::connect("memory").await?;
                db.use_ns("hezarfen").use_db(slug.as_str()).await?;
                db
            }
        };
        Ok(Arc::new(db))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slug(value: &str) -> Slug {
        Slug::try_new(value).expect(value)
    }

    async fn count(db: &Database, table: &str) -> usize {
        let mut rows = db
            .query(format!("SELECT id FROM {table}"))
            .await
            .unwrap()
            .check()
            .unwrap();
        rows.take::<Vec<surrealdb::types::RecordId>>(0)
            .unwrap()
            .len()
    }

    #[tokio::test]
    async fn a_slug_names_a_school_and_nothing_reserved() {
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

    #[tokio::test]
    async fn two_schools_do_not_see_each_others_rows() {
        let tenants = Tenants::new_mem().await.unwrap();
        let (a, b) = (slug("alpha"), slug("beta"));
        let one = tenants.create(&a, "Alpha", ModuleSet::all()).await.unwrap();
        let two = tenants.create(&b, "Beta", ModuleSet::all()).await.unwrap();

        one.query("CREATE user SET username = 'ada', password_hash = 'x', role = 'student'")
            .await
            .unwrap()
            .check()
            .unwrap();
        assert_eq!(count(&one, "user").await, 1);
        assert_eq!(
            count(&two, "user").await,
            0,
            "a row written in one school must be invisible in the other"
        );
        // A second `create` on a taken slug is a conflict, not a second school.
        assert!(matches!(
            tenants.create(&a, "Alpha again", ModuleSet::all()).await,
            Err(AppError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn suspending_refuses_the_school_and_the_handle_it_already_had() {
        let tenants = Tenants::new_mem().await.unwrap();
        let s = slug("alpha");
        tenants.create(&s, "Alpha", ModuleSet::all()).await.unwrap();
        // Cached by the first resolve; the suspension must beat the cache.
        tenants.get(&s).await.expect("active school resolves");

        tenants
            .set_status(&s, SchoolStatus::Suspended)
            .await
            .unwrap();
        assert!(matches!(
            tenants.get(&s).await,
            Err(AppError::Forbidden("school is suspended"))
        ));
        // Builder tooling still reaches it, which is how it gets un-suspended.
        let (_, status) = tenants.get_any_status(&s).await.unwrap();
        assert_eq!(status, SchoolStatus::Suspended);

        tenants.set_status(&s, SchoolStatus::Active).await.unwrap();
        assert!(tenants.get(&s).await.is_ok());
        // An unknown school is a bad credential, never a `404`.
        assert!(matches!(
            tenants.get(&slug("ghost")).await,
            Err(AppError::Unauthorized)
        ));
        assert!(matches!(
            tenants.get_any_status(&slug("ghost")).await,
            Err(AppError::NotFound)
        ));
    }

    #[tokio::test]
    async fn dropping_a_school_takes_its_rows_with_it() {
        let tenants = Tenants::new_mem().await.unwrap();
        let s = slug("alpha");
        let db = tenants.create(&s, "Alpha", ModuleSet::all()).await.unwrap();
        db.query("CREATE user SET username = 'ada', password_hash = 'x', role = 'student'")
            .await
            .unwrap()
            .check()
            .unwrap();
        assert_eq!(count(&db, "user").await, 1);

        tenants.drop(&s).await.unwrap();
        assert!(matches!(tenants.get(&s).await, Err(AppError::Unauthorized)));

        let fresh = tenants
            .create(&s, "Alpha reborn", ModuleSet::all())
            .await
            .unwrap();
        assert_eq!(
            count(&fresh, "user").await,
            0,
            "a re-created school must not inherit the dropped one's rows"
        );
    }

    #[tokio::test]
    async fn the_registry_lists_and_renames() {
        let tenants = Tenants::new_mem().await.unwrap();
        tenants
            .create(&slug("alpha"), "Alpha", ModuleSet::all())
            .await
            .unwrap();
        tenants
            .create(&slug("beta"), "Beta", ModuleSet::all())
            .await
            .unwrap();

        let (rows, total) = School::list(None, 0, tenants.control()).await.unwrap();
        assert_eq!(total, 2);
        assert_eq!(rows.len(), 2);

        let renamed = School::update_name(&slug("alpha"), "Alpha Koleji", tenants.control())
            .await
            .unwrap();
        assert_eq!(renamed.name(), "Alpha Koleji");
        assert_eq!(
            renamed.slug().as_str(),
            "alpha",
            "a rename never moves the slug"
        );
        assert!(matches!(
            School::update_name(&slug("ghost"), "Nope", tenants.control()).await,
            Err(AppError::NotFound)
        ));
    }
    /// The builder API validates a set before it calls here (its message names
    /// every violation at once), but the registry is the last gate: no caller,
    /// present or future, may persist a set a school could not run on.
    #[tokio::test]
    async fn the_registry_refuses_an_unsatisfiable_set_on_create_and_on_sale() {
        let tenants = Tenants::new_mem().await.unwrap();
        let s = slug("alpha");
        let mut lone = ModuleSet::empty();
        lone.insert(crate::module::Module::Exams);

        assert!(
            matches!(
                tenants.create(&s, "Alpha", lone.clone()).await,
                Err(AppError::ConflictOwned(_))
            ),
            "exams alone is unsatisfiable, so the school must not come into being"
        );
        assert!(
            matches!(tenants.get(&s).await, Err(AppError::Unauthorized)),
            "a refused create leaves no registry row behind"
        );

        tenants.create(&s, "Alpha", ModuleSet::all()).await.unwrap();
        assert!(matches!(
            tenants.set_modules(&s, &lone).await,
            Err(AppError::ConflictOwned(_))
        ));
        assert_eq!(
            tenants.resolve(&s).await.unwrap().modules,
            ModuleSet::all(),
            "the refused sale must not have touched the row"
        );
    }
}
