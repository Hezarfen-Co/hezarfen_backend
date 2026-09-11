use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use axum_extra::extract::CookieJar;

use crate::database::Database;
use crate::domain::builder::{Builder, BuilderSession};
use crate::domain::role::Role;
use crate::domain::user::User;
use crate::error::AppError;
use crate::service;
use crate::state::AppState;
use crate::web::tenant_state::{resolve_tenant, split_cookie};

/// A principal injected as a request extension by the AI bridge, for the
/// synthetic requests it dispatches into the router. Extensions cannot be set
/// from outside the process, so this is unforgeable over HTTP.
#[derive(Clone)]
pub(crate) struct AiPrincipal(pub User);

/// Resolve the user behind the `session` cookie, or `401`. An injected
/// [`AiPrincipal`] extension wins over the cookie. The role is read fresh from
/// the row on every request, so a role change takes effect on the user's next
/// call — no re-login required.
///
/// The school is resolved by the very helper `State<AppState>` uses
/// ([`resolve_tenant`]), so the principal and the rows a handler then reads can
/// never come from two different databases. A `builder.<token>` cookie names no
/// school and so cannot reach here at all.
async fn authed_user<S>(parts: &mut Parts, state: &S) -> Result<User, AppError>
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    if let Some(principal) = parts.extensions.get::<AiPrincipal>() {
        return Ok(principal.0.clone());
    }

    let db = resolve_tenant(parts, state).await?.db;

    let jar = CookieJar::from_request_parts(parts, state)
        .await
        .map_err(|_| AppError::Unauthorized)?;
    let cookie = jar.get("session").ok_or(AppError::Unauthorized)?;
    let (_, token) = split_cookie(cookie.value()).ok_or(AppError::Unauthorized)?;

    let session = service::session::find_by_token(&db, token)
        .await?
        .ok_or(AppError::Unauthorized)?;
    if session.is_expired() {
        return Err(AppError::Unauthorized);
    }

    service::user::read(&db, session.user())
        .await?
        .ok_or(AppError::Unauthorized)
}

/// The authenticated user, resolved from the `session` cookie. Add it as a
/// handler argument to require authentication; missing/expired → `401`.
pub struct CurrentUser(pub User);

impl<S> FromRequestParts<S> for CurrentUser
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Ok(CurrentUser(authed_user(parts, state).await?))
    }
}

/// Like [`CurrentUser`], but also requires at least the `student` role —
/// keeps the read-only `parent` role out of write-capable surfaces.
pub struct RequireStudent(pub User);

impl<S> FromRequestParts<S> for RequireStudent
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let user = authed_user(parts, state).await?;
        if !user.get_role().at_least(Role::Student) {
            return Err(AppError::Forbidden("requires student role or higher"));
        }
        Ok(RequireStudent(user))
    }
}

/// Like [`CurrentUser`], but also requires at least the `teacher` role.
/// Authenticated-but-under-privileged callers get `403`.
pub struct RequireTeacher(pub User);

impl<S> FromRequestParts<S> for RequireTeacher
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let user = authed_user(parts, state).await?;
        if !user.get_role().at_least(Role::Teacher) {
            return Err(AppError::Forbidden("requires teacher role or higher"));
        }
        Ok(RequireTeacher(user))
    }
}

/// Like [`CurrentUser`], but also requires at least the `manager` role.
/// Authenticated-but-under-privileged callers get `403`.
pub struct RequireManager(pub User);

impl<S> FromRequestParts<S> for RequireManager
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let user = authed_user(parts, state).await?;
        if !user.get_role().at_least(Role::Manager) {
            return Err(AppError::Forbidden("requires manager role or higher"));
        }
        Ok(RequireManager(user))
    }
}

/// Like [`CurrentUser`], but also requires the `admin` role. Anyone below admin
/// gets `403`.
pub struct RequireAdmin(pub User);

impl<S> FromRequestParts<S> for RequireAdmin
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let user = authed_user(parts, state).await?;
        if !user.get_role().at_least(Role::Admin) {
            return Err(AppError::Forbidden("requires admin role"));
        }
        Ok(RequireAdmin(user))
    }
}

/// The deployment operator behind a `builder.<token>` cookie, resolved against
/// the **control** database. Any school cookie is `401` here, and this cookie
/// is `401` on every school surface (`resolve_tenant` refuses the `builder`
/// prefix as a slug) — the two principals share a cookie name and nothing else.
pub struct RequireBuilder(pub Builder);

impl<S> FromRequestParts<S> for RequireBuilder
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let jar = CookieJar::from_request_parts(parts, state)
            .await
            .map_err(|_| AppError::Unauthorized)?;
        let cookie = jar.get("session").ok_or(AppError::Unauthorized)?;
        let (prefix, token) = split_cookie(cookie.value()).ok_or(AppError::Unauthorized)?;
        if prefix != BUILDER_COOKIE_PREFIX {
            return Err(AppError::Unauthorized);
        }

        let control: Database = AppState::from_ref(state).tenants.control().clone();
        let session = BuilderSession::find_by_token(token, &control)
            .await?
            .ok_or(AppError::Unauthorized)?;
        if session.is_expired() {
            return Err(AppError::Unauthorized);
        }
        Ok(RequireBuilder(
            Builder::read(session.builder(), &control)
                .await?
                .ok_or(AppError::Unauthorized)?,
        ))
    }
}

/// The cookie prefix a builder session carries in place of a school slug.
/// `Slug::try_new` reserves this word, so it can never also name a school.
pub const BUILDER_COOKIE_PREFIX: &str = "builder";
