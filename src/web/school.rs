//! The caller's own school, on the school side of the line.
//!
//! `GET /schools` is the vendor registry and answers only a builder cookie.
//! This route is the inverse: the school the session cookie is already bound
//! to, readable by every authenticated role. The uuid is the identity; the
//! slug stays off the wire.

use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::error::{AppError, ErrorResponse};
use crate::state::AppState;
use crate::tenant::{ResolvedTenant, School};
use crate::web::CurrentUser;
use crate::web::tenant_state::State;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(current_school))
}

/// The school behind the session cookie. `id` is the registry uuid — the
/// identity the database name and every structural reference mint from — and
/// `name` is the display name. The slug is not here: it is a label, not an
/// identity, and this response does not publish it.
#[derive(Serialize, ToSchema)]
struct SchoolIdentity {
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    id: String,
    #[schema(example = "Ata Koleji")]
    name: String,
}

/// The school the session cookie is bound to: its uuid and display name.
///
/// Any authenticated school user may read it, including a student or a parent.
/// A builder cookie names no school and is `401`, the same as on every other
/// school surface.
#[utoipa::path(
    get,
    path = "/school",
    tag = "school",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "The school this session is bound to", body = SchoolIdentity),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn current_school(
    State(st): State<AppState>,
    tenant: ResolvedTenant,
    _user: CurrentUser,
) -> Result<Json<SchoolIdentity>, AppError> {
    let school = School::read(&tenant.slug, st.tenants.control())
        .await?
        .ok_or_else(|| {
            AppError::Internal(format!(
                "the resolved school `{}` has no control row",
                tenant.slug
            ))
        })?;
    Ok(Json(SchoolIdentity {
        id: school.get_id().uuid().to_string(),
        name: school.name().to_string(),
    }))
}
