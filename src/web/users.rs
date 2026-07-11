use axum::Json;
use axum::extract::{Path, State};
use serde::Deserialize;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;
use crate::domain::profile::{BirthDate, Email, PersonName, Phone};
use crate::domain::role::Role;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::dto::Role as RoleSchema;
use super::{CurrentUser, RequireAdmin, UserResponse};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_users))
        .routes(routes!(update_my_profile))
        .routes(routes!(get_user))
        .routes(routes!(set_role))
        .routes(routes!(update_user_profile))
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

/// List every user with their role. Admin only.
#[utoipa::path(
    get,
    path = "/",
    tag = "users",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "All users", body = [UserResponse]),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires admin role", body = ErrorResponse),
    ),
)]
async fn list_users(
    State(st): State<AppState>,
    _admin: RequireAdmin,
) -> Result<Json<Vec<UserResponse>>, AppError> {
    let users = User::list_all(&st.db).await?;
    Ok(Json(users.iter().map(UserResponse::new).collect()))
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

/// Set a user's role. Admin only. An admin cannot change their own role — that
/// guard keeps a sole admin from accidentally locking everyone out of role
/// management (recover such a lockout with the SurrealQL in the README).
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
    Ok(Json(UserResponse::new(&updated)))
}
