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
use crate::domain::builder::{Builder, BuilderSession};
use crate::domain::role::Role;
use crate::domain::session::Session;
use crate::domain::user::{Password, PasswordHash, User, Username};
use crate::error::{AppError, ErrorResponse};
use crate::rate_limit::{RateLimitConfig, RateLimiter};
use crate::state::AppState;
use crate::tenant::{School, SchoolStatus, Slug};
use crate::validate::validate_required;
use crate::web::tenant_state::school_files_path;

use super::auth::session_cookie;
use super::{BUILDER_COOKIE_PREFIX, Page, PageParams, RequireBuilder, UserResponse};

pub fn routes(state: &AppState) -> OpenApiRouter<AppState> {
    // The builder credential is the most valuable one in the deployment, so its
    // login sits behind the same strict per-IP tier `/auth/login` uses — its own
    // budget, since a school's brute-forcer must not spend the vendor's.
    let rate_limit: &RateLimitConfig = &state.rate_limit;
    let limiter = RateLimiter::per_minute(rate_limit.auth_per_minute, rate_limit.trust_proxy);
    limiter.share("builder", state.db.clone(), state.db_up.clone());
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
}

impl SchoolResponse {
    fn new(school: &School) -> Self {
        Self {
            slug: school.slug().as_str().to_string(),
            name: school.name().to_string(),
            status: school.status().as_str().to_string(),
            created_at: school.created_at().as_millis(),
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
}

#[derive(Deserialize, ToSchema)]
struct UpdateSchool {
    #[schema(example = "Ata Koleji", max_length = 120)]
    name: Option<String>,
    /// `active` or `suspended`.
    #[schema(example = "suspended")]
    status: Option<String>,
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
    let user = User::find_by_username(username.trim(), db)
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
    let builder = match Builder::find_by_username(req.username.trim(), control).await? {
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

    let session = BuilderSession::create(builder.get_id(), control).await?;
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
        BuilderSession::delete_by_token(token, st.tenants.control()).await?;
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
        (status = 400, description = "Invalid slug, name, or admin credentials", body = ErrorResponse),
        (status = 401, description = "Not authenticated as a builder", body = ErrorResponse),
        (status = 409, description = "That slug is already taken", body = ErrorResponse),
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
    let password_hash = Password::try_new(&req.admin_password)?.hash_async().await?;

    // Everything, for now: choosing a school's modules is the next lane's
    // HTTP surface (`crate::module` is the foundation it will call).
    let db = st
        .tenants
        .create(&slug, &name, crate::module::ModuleSet::all())
        .await?;
    if let Err(err) = User::create_with_role(username, password_hash, Role::Admin, &db).await {
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
    // `get_any_status`, not `get`: a lockout is exactly the situation a school
    // may be suspended in, and the vendor must still be able to fix it.
    let (db, _status) = st.tenants.get_any_status(&slug).await?;
    let user = school_admin(&req.username, &db).await?;
    let password_hash = Password::try_new(&req.password)?.hash_async().await?;

    User::set_password_hash(user.get_id(), password_hash, &db)
        .await?
        .ok_or(AppError::NotFound)?;
    // The other half: the new password means nothing while a cookie minted
    // under the old one still authenticates.
    Session::delete_by_user(user.get_id(), &db).await?;
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

    let session = Session::create(user.get_id(), &db).await?;
    let cookie = session_cookie(
        format!("{slug}.{}", session.token().as_str()),
        st.cookie_secure,
    );
    Ok((jar.add(cookie), Json(UserResponse::new(&user))))
}
