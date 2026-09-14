//! The vendor's surface: the operator who owns the deployment, not anybody's
//! school. Every handler here takes `axum::extract::State` (the **control**
//! database) rather than `web::tenant_state::State`, because these routes are
//! *about* schools instead of inside one — the school a request touches is
//! named in its path and resolved through [`crate::tenant::Tenants`].
//!
//! The two principals never mix: [`RequireBuilder`] takes only a
//! `builder.<token>` cookie, and that cookie is `401` on every school surface
//! (see `web::tenant_state::resolve_tenant`). `POST /schools/{slug}/enter` is
//! the one bridge, and it does not blur the line — it mints an ordinary school
//! session for an admin account that already exists there.
//!
//! A suspended school is closed to *its users*, not to its vendor: every route
//! here keeps working on one (that is how it gets un-suspended) except `enter`,
//! so "suspended blocks everything" stays true of the school's own doors.

use axum::Json;
use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::Cookie;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::constant::MAX_SCHOOL_NAME_LEN;
use crate::domain::builder::Builder;
use crate::domain::role::Role;
use crate::domain::user::{Password, PasswordHash, User, Username};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::module::{Module, ModuleSet, Package};
use crate::rate_limit::{RateLimitConfig, RateLimiter};
use crate::service;
use crate::state::AppState;
use crate::tenant::{School, SchoolStatus, Slug};
use crate::validate::validate_required;
use crate::web::tenant_state::school_files_path;

use super::auth::session_cookie;
use super::modules::ModulesResponse;
use super::{BUILDER_COOKIE_PREFIX, Page, PageParams, RequireBuilder, UserResponse};

pub fn routes(state: &AppState) -> OpenApiRouter<AppState> {
    // The builder credential is the most valuable one in the deployment, so its
    // login sits behind the same strict per-IP tier `/auth/login` uses — its own
    // budget, since a school's brute-forcer must not spend the vendor's.
    let rate_limit: &RateLimitConfig = &state.rate_limit;
    let limiter = RateLimiter::per_minute(rate_limit.auth_per_minute, rate_limit.trust_proxy);
    limiter.share("builder", state.db.clone());
    OpenApiRouter::new()
        .routes(routes!(builder_login))
        .route_layer(middleware::from_fn(move |req: Request, next: Next| {
            let limiter = limiter.clone();
            async move { limiter.enforce(req, next).await }
        }))
        .routes(routes!(builder_logout))
        .routes(routes!(builder_me))
        .routes(routes!(create_school, list_schools))
        .routes(routes!(get_school, update_school, delete_school))
        .routes(routes!(list_school_modules, patch_school_modules))
        .routes(routes!(enable_school_module, disable_school_module))
        .routes(routes!(reset_admin_password))
        .routes(routes!(enter_school))
}

#[derive(Deserialize, ToSchema)]
struct BuilderCredentials {
    #[schema(example = "operator", min_length = 3, max_length = 32)]
    username: String,
    #[schema(example = "correct horse battery", min_length = 6, max_length = 128)]
    password: String,
}

/// The operator behind the cookie. Deliberately not [`UserResponse`]: a builder
/// has no role, no profile and no school.
#[derive(Serialize, ToSchema)]
struct BuilderResponse {
    id: String,
    #[schema(example = "operator")]
    username: String,
}

impl BuilderResponse {
    fn new(builder: &Builder) -> Self {
        Self {
            id: builder.get_id().key().to_string(),
            username: builder.get_username().as_str().to_string(),
        }
    }
}

#[derive(Serialize, ToSchema)]
struct SchoolResponse {
    #[schema(example = "ata-koleji")]
    slug: String,
    #[schema(example = "Ata Koleji")]
    name: String,
    /// `active` or `suspended`. A suspended school refuses every one of its own
    /// users, login included.
    #[schema(example = "active")]
    status: String,
    /// Registered at, UTC unix-milliseconds.
    created_at: i64,
    /// The modules this school has bought, sorted by name. The other half of
    /// the catalog is on `GET /schools/{slug}/modules`.
    #[schema(example = json!(["courses", "meals", "notes"]))]
    modules: Vec<String>,
}

impl SchoolResponse {
    fn new(school: &School) -> Self {
        Self {
            slug: school.slug().as_str().to_string(),
            name: school.name().to_string(),
            status: school.status().as_str().to_string(),
            created_at: school.created_at().as_millis(),
            modules: school.modules().names(),
        }
    }
}

#[derive(Deserialize, ToSchema)]
struct CreateSchool {
    /// Lowercase `a-z`, `0-9` and `-`, starting with a letter or digit. Names
    /// the school's database, its blob directory and its cookie prefix, so it
    /// is immutable once taken.
    #[schema(example = "ata-koleji", min_length = 2, max_length = 32)]
    slug: String,
    #[schema(example = "Ata Koleji", max_length = 120)]
    name: String,
    /// The school's first admin, created inside the new school's database.
    #[schema(example = "admin", min_length = 3, max_length = 32)]
    admin_username: String,
    #[schema(example = "correct horse battery", min_length = 6, max_length = 128)]
    admin_password: String,
    /// What the school buys. Omitted sells it everything — the deployment's
    /// whole catalog, which `GET /modules/catalog` publishes. A set that
    /// switches a module on without what it structurally needs is refused
    /// before anything is created.
    #[schema(example = json!(["courses", "subjects", "exams"]))]
    modules: Option<Vec<String>>,
}

#[derive(Deserialize, ToSchema)]
struct UpdateSchool {
    #[schema(example = "Ata Koleji", max_length = 120)]
    name: Option<String>,
    /// `active` or `suspended`.
    #[schema(example = "suspended")]
    status: Option<String>,
}

/// One side of a [`patch_school_modules`] request is a list of modules, a list
/// of packages, or both — a package is only a name for its modules, so the two
/// are expanded into the same set.
#[derive(Deserialize, ToSchema)]
struct PatchModules {
    /// Module names to switch on.
    #[schema(example = json!(["exams", "subjects"]))]
    enable: Option<Vec<String>>,
    /// Module names to switch off.
    #[schema(example = json!(["payments"]))]
    disable: Option<Vec<String>>,
    /// Package names to switch on, every module in them.
    #[schema(example = json!(["academics"]))]
    enable_packages: Option<Vec<String>>,
    /// Package names to switch off, every module in them.
    #[schema(example = json!(["ai"]))]
    disable_packages: Option<Vec<String>>,
}

impl PatchModules {
    /// The modules one direction asks for. An unknown name in either list is a
    /// `400` naming it rather than a silent drop — the same rule
    /// [`ModuleSet::from_names`] keeps.
    fn side(
        &self,
        modules: Option<&[String]>,
        packages: Option<&[String]>,
    ) -> Result<ModuleSet, AppError> {
        let mut set = ModuleSet::from_names(modules.unwrap_or_default())?;
        for name in packages.unwrap_or_default() {
            for module in Package::try_from_str(name)?.modules() {
                set.insert(module);
            }
        }
        Ok(set)
    }
}

#[derive(Deserialize, ToSchema)]
struct AdminPassword {
    /// The admin account in that school whose password is being reset.
    #[schema(example = "admin", min_length = 3, max_length = 32)]
    username: String,
    #[schema(example = "correct horse battery", min_length = 6, max_length = 128)]
    password: String,
}

#[derive(Deserialize, ToSchema)]
struct EnterSchool {
    /// The admin account to enter as. It must already exist in that school.
    #[schema(example = "admin", min_length = 3, max_length = 32)]
    username: String,
}

/// A school's display name: free text, unlike its slug.
fn school_name(value: &str) -> Result<String, AppError> {
    validate_required("name", value, MAX_SCHOOL_NAME_LEN)?;
    Ok(value.trim().to_string())
}

/// The slug in a path. Anything that is not a slug names no school, so it is
/// the same `404` an unknown one gets — and it never reaches
/// [`school_files_path`], which is exactly why that path is built from a
/// validated [`Slug`] and never from the raw segment.
fn path_slug(raw: &str) -> Result<Slug, AppError> {
    Slug::try_new(raw).map_err(|_| AppError::NotFound)
}

/// The named account in `db`, which must be an admin of that school: unknown →
/// `404` (a builder is authenticated, there is nothing to hide), present but
/// below admin → `409`, the repo's verdict for "the row exists, its state
/// refuses this".
async fn school_admin(username: &str, db: &crate::database::Database) -> Result<User, AppError> {
    let user = crate::service::user::find_by_username(db, username.trim())
        .await?
        .ok_or(AppError::NotFound)?;
    if user.get_role() != Role::Admin {
        return Err(AppError::Conflict(
            "that account is not an admin of this school",
        ));
    }
    Ok(user)
}

/// Log in as the deployment's builder. Sets a `session` cookie
/// (`builder.<token>`) that works on this surface and nowhere else.
#[utoipa::path(
    post,
    path = "/builder/login",
    tag = "builder",
    request_body = BuilderCredentials,
    responses(
        (status = 200, description = "Logged in; builder session cookie set", body = BuilderResponse),
        (status = 401, description = "Bad credentials", body = ErrorResponse),
        (status = 429, description = "Too many attempts from this address; see Retry-After", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn builder_login(
    State(st): State<AppState>,
    jar: CookieJar,
    Json(req): Json<BuilderCredentials>,
) -> Result<(CookieJar, Json<BuilderResponse>), AppError> {
    let control = st.tenants.control();
    let password = Password::try_new(&req.password).map_err(|_| AppError::Unauthorized)?;
    let builder =
        match crate::service::builder::find_by_username(control, req.username.trim()).await? {
            Some(builder) => {
                if !builder.get_password_hash().verify_async(&password).await {
                    return Err(AppError::Unauthorized);
                }
                builder
            }
            None => {
                // The same decoy hash `/auth/login` runs: a wrong name and a wrong
                // password must cost the same, or the reply time enumerates the
                // operator accounts.
                PasswordHash::verify_decoy_async(&password).await;
                return Err(AppError::Unauthorized);
            }
        };

    let session = crate::service::builder::create_session(control, builder.get_id()).await?;
    let cookie = session_cookie(
        format!("{BUILDER_COOKIE_PREFIX}.{}", session.token().as_str()),
        st.cookie_secure,
    );
    Ok((jar.add(cookie), Json(BuilderResponse::new(&builder))))
}

/// Log out a builder: revoke the session (if any) and clear the cookie.
/// Idempotent — answers `204` either way.
#[utoipa::path(
    post,
    path = "/builder/logout",
    tag = "builder",
    responses((status = 204, description = "Logged out (no-op without a builder session)")),
)]
async fn builder_logout(
    State(st): State<AppState>,
    jar: CookieJar,
) -> Result<(CookieJar, StatusCode), AppError> {
    if let Some((BUILDER_COOKIE_PREFIX, token)) = jar
        .get("session")
        .and_then(|cookie| super::tenant_state::split_cookie(cookie.value()))
    {
        crate::service::builder::delete_by_token(st.tenants.control(), token).await?;
    }
    let jar = jar.remove(Cookie::build(("session", "")).path("/").build());
    Ok((jar, StatusCode::NO_CONTENT))
}

/// The current builder.
#[utoipa::path(
    get,
    path = "/builder/me",
    tag = "builder",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "The current builder", body = BuilderResponse),
        (status = 401, description = "Not authenticated as a builder", body = ErrorResponse),
    ),
)]
async fn builder_me(RequireBuilder(builder): RequireBuilder) -> Json<BuilderResponse> {
    Json(BuilderResponse::new(&builder))
}

/// Create a school: its registry row, its database, its schema, and its first
/// admin account — one call, or none of it.
#[utoipa::path(
    post,
    path = "/schools",
    tag = "builder",
    security(("session_cookie" = [])),
    request_body = CreateSchool,
    responses(
        (status = 201, description = "School created, with its first admin", body = SchoolResponse),
        (status = 400, description = "Invalid slug, name, admin credentials, or module name", body = ErrorResponse, example = json!({"error": "module: `kantin` is not a known module"})),
        (status = 401, description = "Not authenticated as a builder", body = ErrorResponse),
        (status = 409, description = "That slug is already taken, the module set is unsatisfiable, or the admin username exists under a different password", body = ErrorResponse, example = json!({"error": "conflict: exams requires subjects, which is not enabled"})),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create_school(
    State(st): State<AppState>,
    RequireBuilder(_builder): RequireBuilder,
    Json(req): Json<CreateSchool>,
) -> Result<(StatusCode, Json<SchoolResponse>), AppError> {
    // Everything the request must get right is checked (and the admin password
    // hashed) *before* a database is defined, so the ordinary rejection never
    // creates anything to clean up.
    let slug = Slug::try_new(&req.slug)?;
    let name = school_name(&req.name)?;
    let username = Username::try_new(&req.admin_username)?;
    let password = Password::try_new(&req.admin_password)?;
    let password_hash = password.hash_async().await?;

    let modules = match req.modules.as_deref() {
        Some(names) => ModuleSet::from_names(names)?,
        None => ModuleSet::all(),
    };
    // Before `create`, so an unsatisfiable set is a refusal and not a school
    // that has to be deleted again.
    modules.validate()?;
    // The school's first admin is also a *person*: create the control-plane
    // account, or meet the one already standing. A taken username under a
    // different password is a `409` — a builder is authenticated, so there
    // is nothing to enumerate — and it refuses here, before anything is
    // provisioned, so there is nothing to clean up either.
    let person =
        service::person::create_or_load(&st.db, username.clone(), password_hash.clone()).await?;
    if !person.get_password_hash().verify_async(&password).await {
        return Err(AppError::Conflict(
            "an account with that username exists under a different password",
        ));
    }

    let db = st.tenants.create(&slug, &name, modules).await?;
    let seeded = async {
        crate::service::user::create_with_role(&db, username, Some(*person.get_id()), Role::Admin)
            .await?;
        // The admin's person gets the membership the school row answers for —
        // idempotent, so a retried create after a torn pair completes it.
        service::person::link_school(&st.db, person.get_id(), &slug).await?;
        Ok::<(), AppError>(())
    }
    .await;
    if let Err(err) = seeded {
        // A school nobody can log into is worse than no school: take the
        // database back so the very same request can simply be retried.
        if let Err(cleanup) = st.tenants.drop(&slug).await {
            tracing::error!("failed to drop {slug} after its admin seed failed: {cleanup}");
        }
        return Err(err);
    }

    let school = School::read(&slug, st.tenants.control())
        .await?
        .ok_or_else(|| AppError::Internal("the school vanished as it was created".into()))?;
    Ok((StatusCode::CREATED, Json(SchoolResponse::new(&school))))
}

/// Every school this deployment serves, newest first. Paged via
/// `?limit=&offset=` (omit `limit` for the full list).
#[utoipa::path(
    get,
    path = "/schools",
    tag = "builder",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "A page of schools (the full list when unpaged)", body = Page<SchoolResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated as a builder", body = ErrorResponse),
    ),
)]
async fn list_schools(
    State(st): State<AppState>,
    RequireBuilder(_builder): RequireBuilder,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<SchoolResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let (schools, total) = School::list(limit, offset, st.tenants.control()).await?;
    let items = schools.iter().map(SchoolResponse::new).collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// One school by slug.
#[utoipa::path(
    get,
    path = "/schools/{slug}",
    tag = "builder",
    security(("session_cookie" = [])),
    params(("slug" = String, Path, description = "School slug")),
    responses(
        (status = 200, description = "The school", body = SchoolResponse),
        (status = 401, description = "Not authenticated as a builder", body = ErrorResponse),
        (status = 404, description = "No such school", body = ErrorResponse),
    ),
)]
async fn get_school(
    State(st): State<AppState>,
    RequireBuilder(_builder): RequireBuilder,
    Path(slug): Path<String>,
) -> Result<Json<SchoolResponse>, AppError> {
    let school = School::read(&path_slug(&slug)?, st.tenants.control())
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(SchoolResponse::new(&school)))
}

/// Rename a school and/or flip it between `active` and `suspended`. Omitted
/// fields keep their value. The slug itself is immutable — it names a database
/// and a directory.
///
/// Suspending is immediate and total for the school's own users: their next
/// request is a `403`, live session or not.
#[utoipa::path(
    patch,
    path = "/schools/{slug}",
    tag = "builder",
    security(("session_cookie" = [])),
    params(("slug" = String, Path, description = "School slug")),
    request_body = UpdateSchool,
    responses(
        (status = 200, description = "The updated school", body = SchoolResponse),
        (status = 400, description = "Invalid name or status", body = ErrorResponse),
        (status = 401, description = "Not authenticated as a builder", body = ErrorResponse),
        (status = 404, description = "No such school", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn update_school(
    State(st): State<AppState>,
    RequireBuilder(_builder): RequireBuilder,
    Path(slug): Path<String>,
    Json(req): Json<UpdateSchool>,
) -> Result<Json<SchoolResponse>, AppError> {
    let slug = path_slug(&slug)?;
    let name = req.name.as_deref().map(school_name).transpose()?;
    let status = req
        .status
        .as_deref()
        .map(SchoolStatus::try_from_str)
        .transpose()?;

    let control = st.tenants.control();
    // An empty patch on an unknown school is still a 404, so the existence
    // check is not left to whichever field happened to be present.
    School::read(&slug, control)
        .await?
        .ok_or(AppError::NotFound)?;
    if let Some(name) = name {
        School::update_name(&slug, &name, control).await?;
    }
    if let Some(status) = status {
        st.tenants.set_status(&slug, status).await?;
    }
    let school = School::read(&slug, control)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(SchoolResponse::new(&school)))
}

/// Delete a school: its database, its registry row, and its uploaded files.
/// Irreversible — suspension is the reversible door.
#[utoipa::path(
    delete,
    path = "/schools/{slug}",
    tag = "builder",
    security(("session_cookie" = [])),
    params(("slug" = String, Path, description = "School slug")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated as a builder", body = ErrorResponse),
        (status = 404, description = "No such school", body = ErrorResponse),
    ),
)]
async fn delete_school(
    State(st): State<AppState>,
    RequireBuilder(_builder): RequireBuilder,
    Path(slug): Path<String>,
) -> Result<StatusCode, AppError> {
    let slug = path_slug(&slug)?;
    School::read(&slug, st.tenants.control())
        .await?
        .ok_or(AppError::NotFound)?;
    st.tenants.drop(&slug).await?;

    // Rows first, bytes second — the same order every blob delete here keeps, so
    // a crash between them strands unreachable files rather than rows pointing
    // at nothing. The directory is built from the validated `Slug`, never from
    // the raw path segment.
    let dir = school_files_path(&st.files_path, &slug);
    if let Err(err) = tokio::fs::remove_dir_all(&dir).await
        && err.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            "failed to remove the files of {slug} at {}: {err}",
            dir.display()
        );
    }
    Ok(StatusCode::NO_CONTENT)
}

/// A module named in a path segment. Unknown → `404`, the same verdict an
/// unknown slug gets: the segment names nothing this deployment sells, so
/// there is no resource to act on. Inside a *body* the same name is a `400`
/// instead — a bad field, not a bad address.
fn path_module(raw: &str) -> Result<Module, AppError> {
    Module::try_from_str(raw).map_err(|_| AppError::NotFound)
}

/// A school's entitlements, off the registry row. Deliberately not
/// [`Tenants::resolve`]: that one is the school's own door and refuses a
/// suspended school, and every route here keeps working on one.
async fn school_modules(
    slug: &Slug,
    control: &crate::database::Database,
) -> Result<ModuleSet, AppError> {
    Ok(School::read(slug, control)
        .await?
        .ok_or(AppError::NotFound)?
        .modules())
}

/// Refuse taking `module` back while something the school still has needs it,
/// naming every such module — the reverse direction of
/// [`ModuleSet::validate`], which speaks for the modules that are on.
fn refuse_if_needed(module: Module, set: &ModuleSet) -> Result<(), AppError> {
    let needed_by: Vec<String> = module
        .dependents()
        .into_iter()
        .filter(|dependent| set.contains(*dependent))
        .map(|dependent| dependent.as_str().to_string())
        .collect();
    if needed_by.is_empty() {
        return Ok(());
    }
    Err(AppError::ConflictOwned(format!(
        "{module} is required by {}",
        needed_by.join(", ")
    )))
}

/// Write a school's new set and answer with it. One write, or none — every
/// caller here has already decided.
async fn store_modules(
    st: &AppState,
    slug: &Slug,
    modules: ModuleSet,
) -> Result<Json<ModulesResponse>, AppError> {
    st.tenants.set_modules(slug, &modules).await?;
    Ok(Json(ModulesResponse::new(&modules)))
}

/// What a school has bought, and what is left to sell it. Works on a suspended
/// school — entitlements are the vendor's ledger, not one of the school's doors.
#[utoipa::path(
    get,
    path = "/schools/{slug}/modules",
    tag = "builder",
    security(("session_cookie" = [])),
    params(("slug" = String, Path, description = "School slug")),
    responses(
        (status = 200, description = "The school's enabled and disabled modules", body = ModulesResponse),
        (status = 401, description = "Not authenticated as a builder", body = ErrorResponse),
        (status = 404, description = "No such school", body = ErrorResponse),
    ),
)]
async fn list_school_modules(
    State(st): State<AppState>,
    RequireBuilder(_builder): RequireBuilder,
    Path(slug): Path<String>,
) -> Result<Json<ModulesResponse>, AppError> {
    let modules = school_modules(&path_slug(&slug)?, st.tenants.control()).await?;
    Ok(Json(ModulesResponse::new(&modules)))
}

/// Sell a school one module. Idempotent: a module it already has is a `200`
/// with the unchanged set. Refused while what the module structurally needs is
/// off — enable those in the same `PATCH` instead.
#[utoipa::path(
    post,
    path = "/schools/{slug}/modules/{module}",
    tag = "builder",
    security(("session_cookie" = [])),
    params(
        ("slug" = String, Path, description = "School slug"),
        ("module" = String, Path, description = "Module name, as `GET /modules/catalog` publishes it"),
    ),
    responses(
        (status = 200, description = "The school's modules after the change", body = ModulesResponse),
        (status = 401, description = "Not authenticated as a builder", body = ErrorResponse),
        (status = 404, description = "No such school, or no such module", body = ErrorResponse, example = json!({"error": "not found"})),
        (status = 409, description = "That module needs another this school does not have", body = ErrorResponse, example = json!({"error": "conflict: exams requires subjects, which is not enabled"})),
    ),
)]
async fn enable_school_module(
    State(st): State<AppState>,
    RequireBuilder(_builder): RequireBuilder,
    Path((slug, module)): Path<(String, String)>,
) -> Result<Json<ModulesResponse>, AppError> {
    let slug = path_slug(&slug)?;
    let module = path_module(&module)?;
    let mut modules = school_modules(&slug, st.tenants.control()).await?;
    if modules.contains(module) {
        return Ok(Json(ModulesResponse::new(&modules)));
    }
    modules.insert(module);
    modules.validate()?;
    store_modules(&st, &slug, modules).await
}

/// Take one module back. Idempotent, and refused while a module the school
/// still has depends on it — the mirror of the enable direction.
///
/// The school's data is untouched: a disabled module's rows stay put and come
/// back with it. Only its routes stop answering.
#[utoipa::path(
    delete,
    path = "/schools/{slug}/modules/{module}",
    tag = "builder",
    security(("session_cookie" = [])),
    params(
        ("slug" = String, Path, description = "School slug"),
        ("module" = String, Path, description = "Module name, as `GET /modules/catalog` publishes it"),
    ),
    responses(
        (status = 200, description = "The school's modules after the change", body = ModulesResponse),
        (status = 401, description = "Not authenticated as a builder", body = ErrorResponse),
        (status = 404, description = "No such school, or no such module", body = ErrorResponse, example = json!({"error": "not found"})),
        (status = 409, description = "Another module the school has requires this one", body = ErrorResponse, example = json!({"error": "conflict: courses is required by exams, subjects"})),
    ),
)]
async fn disable_school_module(
    State(st): State<AppState>,
    RequireBuilder(_builder): RequireBuilder,
    Path((slug, module)): Path<(String, String)>,
) -> Result<Json<ModulesResponse>, AppError> {
    let slug = path_slug(&slug)?;
    let module = path_module(&module)?;
    let mut modules = school_modules(&slug, st.tenants.control()).await?;
    if !modules.contains(module) {
        return Ok(Json(ModulesResponse::new(&modules)));
    }
    refuse_if_needed(module, &modules)?;
    modules.remove(module);
    store_modules(&st, &slug, modules).await
}

/// Re-sell a school's whole shelf in one call: any mix of modules and packages,
/// in either direction. Every list is optional and an empty body is a no-op.
///
/// The four lists are expanded into one resulting set, which is then checked
/// once — so a `PATCH` that enables `exams` and `subjects` together is fine
/// where two single calls would refuse the first, and a `409` names every
/// violation at once instead of one per round trip. Nothing is written unless
/// the whole request is accepted.
#[utoipa::path(
    patch,
    path = "/schools/{slug}/modules",
    tag = "builder",
    security(("session_cookie" = [])),
    params(("slug" = String, Path, description = "School slug")),
    request_body = PatchModules,
    responses(
        (status = 200, description = "The school's modules after the change", body = ModulesResponse),
        (status = 400, description = "An unknown module or package name, or one asked for in both directions", body = ErrorResponse, example = json!({"error": "package: `kantin` is not a known package"})),
        (status = 401, description = "Not authenticated as a builder", body = ErrorResponse),
        (status = 404, description = "No such school", body = ErrorResponse),
        (status = 409, description = "The resulting set breaks a dependency", body = ErrorResponse, example = json!({"error": "conflict: marks requires exams, which is not enabled"})),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn patch_school_modules(
    State(st): State<AppState>,
    RequireBuilder(_builder): RequireBuilder,
    Path(slug): Path<String>,
    Json(req): Json<PatchModules>,
) -> Result<Json<ModulesResponse>, AppError> {
    let slug = path_slug(&slug)?;
    let enable = req.side(req.enable.as_deref(), req.enable_packages.as_deref())?;
    let disable = req.side(req.disable.as_deref(), req.disable_packages.as_deref())?;
    // A name pulled both ways has no defensible resolution, and picking one
    // silently would sell (or unsell) a module the caller also asked for the
    // opposite of.
    if let Some(module) = enable.iter().find(|module| disable.contains(*module)) {
        return Err(ValidationError::Contradictory {
            field: "module",
            value: module.as_str().to_string(),
        }
        .into());
    }

    let current = school_modules(&slug, st.tenants.control()).await?;
    let mut result = current.clone();
    for module in enable.iter() {
        result.insert(module);
    }
    for module in disable.iter() {
        result.remove(module);
    }
    // One check for both directions: a disable that strands a dependent shows
    // up here as that dependent missing its requirement.
    result.validate()?;

    if result == current {
        return Ok(Json(ModulesResponse::new(&current)));
    }
    store_modules(&st, &slug, result).await
}

/// Reset an admin's password inside a school — the "we are locked out" call.
/// Every session that account held is revoked with it, so a stolen cookie does
/// not survive the reset. Works on a suspended school.
#[utoipa::path(
    post,
    path = "/schools/{slug}/admin-password",
    tag = "builder",
    security(("session_cookie" = [])),
    params(("slug" = String, Path, description = "School slug")),
    request_body = AdminPassword,
    responses(
        (status = 204, description = "Password reset; that account's sessions are revoked"),
        (status = 400, description = "Invalid password", body = ErrorResponse),
        (status = 401, description = "Not authenticated as a builder", body = ErrorResponse),
        (status = 404, description = "No such school, or no such account in it", body = ErrorResponse),
        (status = 409, description = "That account exists but is not an admin of this school", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn reset_admin_password(
    State(st): State<AppState>,
    RequireBuilder(_builder): RequireBuilder,
    Path(slug): Path<String>,
    Json(req): Json<AdminPassword>,
) -> Result<StatusCode, AppError> {
    let slug = path_slug(&slug)?;
    let (db, _status) = st.tenants.get_any_status(&slug).await?;
    let user = school_admin(&req.username, &db).await?;
    let password_hash = Password::try_new(&req.password)?.hash_async().await?;

    // The credential is the control person's — the one login reads. The
    // school row carries no password at all.
    service::person::set_password_hash(&st.db, user.get_username(), &password_hash).await?;
    // The other half: the new password means nothing while a cookie minted
    // under the old one still authenticates — school sessions *and* any
    // unbound `person.<token>` that could mint a fresh school session.
    service::session::delete_by_user(&db, user.get_id()).await?;
    if let Some(person) =
        service::person::find_by_username(&st.db, user.get_username().as_str()).await?
    {
        service::person::delete_sessions_by_person(&st.db, person.get_id()).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Enter a school as one of its admins — support access, with the school's own
/// session cookie (`<slug>.<token>`) and no builder power inside it.
///
/// Refused on a suspended school (`403`): a suspension closes the school's
/// doors, and this is one of them. Un-suspend it first.
#[utoipa::path(
    post,
    path = "/schools/{slug}/enter",
    tag = "builder",
    security(("session_cookie" = [])),
    params(("slug" = String, Path, description = "School slug")),
    request_body = EnterSchool,
    responses(
        (status = 200, description = "Entered; that school's session cookie is set", body = UserResponse),
        (status = 401, description = "Not authenticated as a builder", body = ErrorResponse),
        (status = 403, description = "The school is suspended", body = ErrorResponse),
        (status = 404, description = "No such school, or no such account in it", body = ErrorResponse),
        (status = 409, description = "That account exists but is not an admin of this school", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn enter_school(
    State(st): State<AppState>,
    RequireBuilder(_builder): RequireBuilder,
    jar: CookieJar,
    Path(slug): Path<String>,
    Json(req): Json<EnterSchool>,
) -> Result<(CookieJar, Json<UserResponse>), AppError> {
    let slug = path_slug(&slug)?;
    let (db, status) = st.tenants.get_any_status(&slug).await?;
    if status == SchoolStatus::Suspended {
        return Err(AppError::Forbidden("school is suspended"));
    }
    let user = school_admin(&req.username, &db).await?;

    let session = service::session::create(&db, user.get_id()).await?;
    let cookie = session_cookie(
        format!("{slug}.{}", session.token().as_str()),
        st.cookie_secure,
    );
    Ok((jar.add(cookie), Json(UserResponse::new(&user))))
}
