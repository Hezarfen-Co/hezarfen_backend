use axum::Json;
use axum::extract::{Path, State};
use serde::Deserialize;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::domain::role::Role;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse};
use crate::state::AppState;

use super::dto::Role as RoleSchema;
use super::{RequireAdmin, UserResponse};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_users))
        .routes(routes!(set_role))
}

#[derive(Deserialize, ToSchema)]
struct SetRole {
    /// The role to assign. Deserialized as a string so an unknown value returns a
    /// uniform `400`; documented as the `Role` enum so the docs list the choices.
    #[schema(value_type = RoleSchema, example = "teacher")]
    role: String,
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
