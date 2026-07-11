use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use axum_extra::extract::CookieJar;

use crate::domain::role::Role;
use crate::domain::session::Session;
use crate::domain::user::User;
use crate::error::AppError;
use crate::state::AppState;

/// Resolve the user behind the `session` cookie, or `401`. The role is read
/// fresh from the row on every request, so a role change takes effect on the
/// user's next call — no re-login required.
async fn authed_user<S>(parts: &mut Parts, state: &S) -> Result<User, AppError>
where
    S: Send + Sync,
    AppState: FromRef<S>,
{
    let jar = CookieJar::from_request_parts(parts, state)
        .await
        .map_err(|_| AppError::Unauthorized)?;
    let token = jar
        .get("session")
        .map(|cookie| cookie.value().to_owned())
        .ok_or(AppError::Unauthorized)?;

    let app = AppState::from_ref(state);

    let session = Session::find_by_token(&token, &app.db)
        .await?
        .ok_or(AppError::Unauthorized)?;
    if session.is_expired() {
        return Err(AppError::Unauthorized);
    }

    User::read(session.user(), &app.db)
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
