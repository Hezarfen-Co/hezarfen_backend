//! Response DTOs shared across more than one handler module.

use serde::Serialize;
use utoipa::ToSchema;

use crate::domain::role::Role as DomainRole;
use crate::domain::user::User;

/// The four access roles, lowest to highest privilege. The web-facing mirror of
/// [`crate::domain::role::Role`] — it carries the serde + OpenAPI derives (which
/// the domain type deliberately omits), so it renders as a proper `enum` in the
/// docs. Serializes to the same lowercase strings the domain stores.
#[derive(Serialize, Clone, Copy, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Student,
    Teacher,
    Manager,
    Admin,
}

impl From<DomainRole> for Role {
    fn from(role: DomainRole) -> Self {
        match role {
            DomainRole::Student => Role::Student,
            DomainRole::Teacher => Role::Teacher,
            DomainRole::Manager => Role::Manager,
            DomainRole::Admin => Role::Admin,
        }
    }
}

/// Public shape of a user: id, username, role. Never carries the password hash.
/// Shared by `auth` (register/login/me) and `users` (admin listing/role changes).
#[derive(Serialize, ToSchema)]
pub struct UserResponse {
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    pub id: String,
    #[schema(example = "ada")]
    pub username: String,
    pub role: Role,
}

impl UserResponse {
    pub fn new(user: &User) -> Self {
        Self {
            id: user.get_id().key().to_string(),
            username: user.get_username().as_str().to_string(),
            role: user.get_role().into(),
        }
    }
}
