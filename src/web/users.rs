use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;
use crate::domain::enrollment::Enrollment;
use crate::domain::preferences::{Language, Theme};
use crate::domain::profile::{BirthDate, Email, PersonName, Phone};
use crate::domain::role::Role;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::dto::Role as RoleSchema;
use super::{
    CurrentUser, Page, PageParams, PersonRef, RequireAdmin, RequireTeacher, UserResponse, paginate,
};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_users))
        .routes(routes!(update_my_profile))
        .routes(routes!(update_my_preferences))
        .routes(routes!(search_users))
        .routes(routes!(get_user))
        .routes(routes!(set_role))
        .routes(routes!(update_user_profile))
        .routes(routes!(update_user_preferences))
}

#[derive(Deserialize, ToSchema)]
struct SetRole {
    /// The role to assign. Deserialized as a string so an unknown value returns a
    /// uniform `400`; documented as the `Role` enum so the docs list the choices.
    #[schema(value_type = RoleSchema, example = "teacher")]
    role: String,
}

/// Partial personal-info update. Per field: omitted (or `null`) keeps the
/// current value, an empty string clears it, anything else is validated and set.
#[derive(Deserialize, ToSchema)]
struct UpdateProfile {
    #[schema(example = "Ada")]
    name: Option<String>,
    #[schema(example = "Lovelace")]
    surname: Option<String>,
    #[schema(example = "ada@example.com")]
    email: Option<String>,
    #[schema(example = "+90 555 123 45 67")]
    phone: Option<String>,
    /// Birth date in `YYYY-MM-DD` form.
    #[schema(example = "1990-01-02")]
    birth_date: Option<String>,
}

/// Partial UI-preference update. Same field semantics as [`UpdateProfile`]:
/// omitted (or `null`) keeps the current value, an empty string clears it back
/// to "never chose" (the client then follows the device preference), anything
/// else is validated and set.
#[derive(Deserialize, ToSchema)]
struct UpdatePreferences {
    /// `light` or `dark`.
    #[schema(example = "dark")]
    theme: Option<String>,
    /// `tr` or `en` (ISO 639-1).
    #[schema(example = "tr")]
    language: Option<String>,
}

/// Resolve one patched field: absent keeps the current value, `""` clears it,
/// anything else must parse into the domain newtype.
fn merge_field<T: Clone>(
    current: Option<&T>,
    patch: Option<&str>,
    parse: impl Fn(&str) -> Result<T, ValidationError>,
) -> Result<Option<T>, ValidationError> {
    match patch {
        None => Ok(current.cloned()),
        Some("") => Ok(None),
        Some(value) => Ok(Some(parse(value)?)),
    }
}

/// Merge `req` over `user`'s current info and persist. Shared by the
/// self-service and admin profile endpoints — they differ only in whose row
/// they load and who may call them.
async fn apply_profile(
    user: User,
    req: &UpdateProfile,
    db: &Database,
) -> Result<UserResponse, AppError> {
    let name = merge_field(user.get_name(), req.name.as_deref(), |v| {
        PersonName::try_new("name", v)
    })?;
    let surname = merge_field(user.get_surname(), req.surname.as_deref(), |v| {
        PersonName::try_new("surname", v)
    })?;
    let email = merge_field(user.get_email(), req.email.as_deref(), Email::try_new)?;
    let phone = merge_field(user.get_phone(), req.phone.as_deref(), Phone::try_new)?;
    let birth_date = merge_field(
        user.get_birth_date(),
        req.birth_date.as_deref(),
        BirthDate::try_new,
    )?;
    let updated = user
        .set_profile(name, surname, email, phone, birth_date, db)
        .await?;
    Ok(UserResponse::new(&updated))
}

/// Merge `req` over `user`'s current preferences and persist. Shared by the
/// self-service and admin preference endpoints — they differ only in whose row
/// they load and who may call them.
async fn apply_preferences(
    user: User,
    req: &UpdatePreferences,
    db: &Database,
) -> Result<UserResponse, AppError> {
    let theme = merge_field(user.get_theme().as_ref(), req.theme.as_deref(), |v| {
        Theme::try_from_str(v)
    })?;
    let language = merge_field(
        user.get_language().as_ref(),
        req.language.as_deref(),
        Language::try_from_str,
    )?;
    let updated = user.set_preferences(theme, language, db).await?;
    Ok(UserResponse::new(&updated))
}

#[derive(Deserialize, IntoParams)]
struct SearchUsers {
    /// Case-insensitive fragment of a username, name, or surname. May be
    /// blank when `role` is given — that lists the whole role.
    q: String,
    /// Restrict matches to one role: `student`, `teacher`, `manager`, or
    /// `admin`. Omit to search every role.
    role: Option<String>,
    /// Max matches to return. Omit for every match; when given, `1`–`500`.
    #[param(minimum = 1, maximum = 500, example = 100)]
    limit: Option<i64>,
    /// Matches to skip from the start. Defaults to `0`.
    #[param(minimum = 0, example = 0)]
    offset: Option<i64>,
}

/// Find users by username or name — backs the pickers (enroll, grade, mark
/// attendance). Requires teacher+. `role` narrows to one role (e.g.
/// `role=student` for an enroll picker); a blank `q` with a `role` lists
/// everyone in that role. Paged via `?limit=&offset=` like the other lists
/// (omit `limit` for every match); returns a `{items, total, limit, offset}`
/// envelope carrying only id/username/display name — no contact details.
#[utoipa::path(
    get,
    path = "/search",
    tag = "users",
    security(("session_cookie" = [])),
    params(SearchUsers),
    responses(
        (status = 200, description = "A page of matching users (all matches when unpaged)", body = Page<PersonRef>),
        (status = 400, description = "Blank query without a role, unknown role, or invalid limit/offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
    ),
)]
async fn search_users(
    State(st): State<AppState>,
    _teacher: RequireTeacher,
    Query(req): Query<SearchUsers>,
) -> Result<Json<Page<PersonRef>>, AppError> {
    if req.q.trim().is_empty() && req.role.is_none() {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "q",
            reason: "must not be empty unless role is given",
        }));
    }
    let (limit, offset) = PageParams {
        limit: req.limit,
        offset: req.offset,
    }
    .resolve()?;
    let role = req.role.as_deref().map(Role::try_from_str).transpose()?;
    let users = User::search(&req.q, role, &st.db).await?;
    let total = users.len() as i64;
    let items = paginate(&users, limit, offset)
        .iter()
        .map(PersonRef::new)
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// List every user with their role, newest first. Admin only. Paged: pass
/// `?limit=&offset=` to take a window (omit `limit` for the whole list); the
/// response is a `{items, total, limit, offset}` envelope where `total` counts
/// every user.
#[utoipa::path(
    get,
    path = "/",
    tag = "users",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "A page of users (the full list when unpaged)", body = Page<UserResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires admin role", body = ErrorResponse),
    ),
)]
async fn list_users(
    State(st): State<AppState>,
    _admin: RequireAdmin,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<UserResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let users = User::list_all(&st.db).await?;
    let total = users.len() as i64;
    let items = paginate(&users, limit, offset)
        .iter()
        .map(UserResponse::new)
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Update the caller's own personal info: name, surname, email, phone, birth
/// date. Any authenticated role. Omitted fields stay as they are; an empty
/// string clears a field.
#[utoipa::path(
    patch,
    path = "/me",
    tag = "users",
    security(("session_cookie" = [])),
    request_body = UpdateProfile,
    responses(
        (status = 200, description = "Updated user", body = UserResponse),
        (status = 400, description = "Invalid field", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn update_my_profile(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Json(req): Json<UpdateProfile>,
) -> Result<Json<UserResponse>, AppError> {
    Ok(Json(apply_profile(user, &req, &st.db).await?))
}

/// Update the caller's own UI preferences: `theme` (`light`/`dark`) and
/// `language` (`tr`/`en`). Any authenticated role. Omitted fields stay as they
/// are; an empty string clears one back to "never chose" (the client then
/// follows the device preference). Read them back on any user response, e.g.
/// `GET /auth/me`.
#[utoipa::path(
    patch,
    path = "/me/preferences",
    tag = "users",
    security(("session_cookie" = [])),
    request_body = UpdatePreferences,
    responses(
        (status = 200, description = "Updated user", body = UserResponse),
        (status = 400, description = "Invalid theme or language", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn update_my_preferences(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Json(req): Json<UpdatePreferences>,
) -> Result<Json<UserResponse>, AppError> {
    Ok(Json(apply_preferences(user, &req, &st.db).await?))
}

/// Fetch one user with their role and personal info. Admin only.
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "users",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "User id")),
    responses(
        (status = 200, description = "The user", body = UserResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires admin role", body = ErrorResponse),
        (status = 404, description = "User not found", body = ErrorResponse),
    ),
)]
async fn get_user(
    State(st): State<AppState>,
    _admin: RequireAdmin,
    Path(id): Path<String>,
) -> Result<Json<UserResponse>, AppError> {
    let user = User::read(&UserId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(UserResponse::new(&user)))
}

/// Update any user's personal info. Admin only — the school-office path for
/// maintaining records on behalf of students and staff. Same field semantics
/// as `PATCH /users/me`.
#[utoipa::path(
    patch,
    path = "/{id}/profile",
    tag = "users",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "User id")),
    request_body = UpdateProfile,
    responses(
        (status = 200, description = "Updated user", body = UserResponse),
        (status = 400, description = "Invalid field", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires admin role", body = ErrorResponse),
        (status = 404, description = "User not found", body = ErrorResponse),
    ),
)]
async fn update_user_profile(
    State(st): State<AppState>,
    _admin: RequireAdmin,
    Path(id): Path<String>,
    Json(req): Json<UpdateProfile>,
) -> Result<Json<UserResponse>, AppError> {
    let user = User::read(&UserId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(apply_profile(user, &req, &st.db).await?))
}

/// Update any user's UI preferences. Admin only — everyone else manages their
/// own through `PATCH /users/me/preferences`, which this mirrors field for
/// field.
#[utoipa::path(
    patch,
    path = "/{id}/preferences",
    tag = "users",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "User id")),
    request_body = UpdatePreferences,
    responses(
        (status = 200, description = "Updated user", body = UserResponse),
        (status = 400, description = "Invalid theme or language", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires admin role", body = ErrorResponse),
        (status = 404, description = "User not found", body = ErrorResponse),
    ),
)]
async fn update_user_preferences(
    State(st): State<AppState>,
    _admin: RequireAdmin,
    Path(id): Path<String>,
    Json(req): Json<UpdatePreferences>,
) -> Result<Json<UserResponse>, AppError> {
    let user = User::read(&UserId::from_key(&id), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(apply_preferences(user, &req, &st.db).await?))
}

/// Set a user's role. Admin only. An admin cannot change their own role — that
/// guard keeps a sole admin from accidentally locking everyone out of role
/// management (recover such a lockout with the SurrealQL in the README).
/// Setting any non-`student` role also drops the user's course enrollments —
/// only students enroll, so a promoted user leaves every roster.
#[utoipa::path(
    patch,
    path = "/{id}/role",
    tag = "users",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "User id")),
    request_body = SetRole,
    responses(
        (status = 200, description = "Updated user", body = UserResponse),
        (status = 400, description = "Invalid role", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires admin, or attempted to change own role", body = ErrorResponse),
        (status = 404, description = "User not found", body = ErrorResponse),
    ),
)]
async fn set_role(
    State(st): State<AppState>,
    RequireAdmin(admin): RequireAdmin,
    Path(id): Path<String>,
    Json(req): Json<SetRole>,
) -> Result<Json<UserResponse>, AppError> {
    let role = Role::try_from_str(&req.role)?;
    let target = UserId::from_key(&id);
    if &target == admin.get_id() {
        return Err(AppError::Forbidden("cannot change your own role"));
    }
    let user = User::read(&target, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let updated = user.set_role(role, &st.db).await?;
    // Roster hygiene: only students enroll, so a non-student sheds all their
    // enrollment rows (security checks re-read the live role and never
    // trusted these; this just stops them polluting rosters and counts).
    if role != Role::Student {
        Enrollment::delete_for_user(&target, &st.db).await?;
    }
    Ok(Json(UserResponse::new(&updated)))
}
