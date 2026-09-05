//! Module entitlements as data, on the school side of the line.
//!
//! Two reads, no writes — selling a module is the vendor's act and lives on
//! `/schools/{slug}/modules` in [`crate::web::builder`]. What stands here is
//! what a client needs to draw the product: the deployment's catalog (which
//! modules exist, what each one needs, how they are packaged) and the caller's
//! own switched-on set, so a frontend hides a nest the school never bought
//! instead of discovering it as a `403`.
//!
//! Both routes are ungated on purpose: an entitlement lookup that a disabled
//! module could switch off would be unusable exactly when it is needed. The
//! catalog is unauthenticated for the same reason `GET /limits` is — it is a
//! deploy-time constant, identical for every school.

use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::error::ErrorResponse;
use crate::module::{Module, ModuleSet, Package};
use crate::state::AppState;
use crate::web::CurrentUser;
use crate::web::tenant_state::ResolvedTenant;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(module_catalog))
        .routes(routes!(my_modules))
}

/// One school's entitlements, both halves of the partition. `disabled` is
/// carried rather than left to the client to subtract: the two lists together
/// are the whole catalog, so a client can render the switchboard from one
/// response.
#[derive(Serialize, ToSchema)]
pub struct ModulesResponse {
    /// Switched on, sorted by name.
    #[schema(example = json!(["courses", "meals", "notes"]))]
    pub enabled: Vec<String>,
    /// Every other module in the catalog, sorted by name.
    #[schema(example = json!(["chatbot", "payments"]))]
    pub disabled: Vec<String>,
}

impl ModulesResponse {
    pub fn new(modules: &ModuleSet) -> Self {
        let mut disabled: Vec<String> = Module::ALL
            .into_iter()
            .filter(|m| !modules.contains(*m))
            .map(|m| m.as_str().to_string())
            .collect();
        disabled.sort();
        Self {
            enabled: modules.names(),
            disabled,
        }
    }
}

/// What the caller's own school has, without the vendor's half of the picture.
#[derive(Serialize, ToSchema)]
struct EnabledResponse {
    /// Switched on, sorted by name.
    #[schema(example = json!(["courses", "meals", "notes"]))]
    enabled: Vec<String>,
}

/// One sellable module: its name, the bundle it is sold in, and what it cannot
/// work without.
#[derive(Serialize, ToSchema)]
struct CatalogModule {
    #[schema(example = "exams")]
    module: String,
    #[schema(example = "academics")]
    package: String,
    /// Modules that must be enabled alongside this one. Structural, not
    /// commercial: each edge is a stored reference into the other module's
    /// data, so a set that breaks one is refused.
    #[schema(example = json!(["courses", "subjects"]))]
    requires: Vec<String>,
}

/// One commercial bundle and the modules it sells.
#[derive(Serialize, ToSchema)]
struct CatalogPackage {
    #[schema(example = "academics")]
    package: String,
    #[schema(example = json!(["classes", "courses", "exams"]))]
    modules: Vec<String>,
}

/// The whole catalog, identical for every school in the deployment.
#[derive(Serialize, ToSchema)]
struct CatalogResponse {
    /// Every module, sorted by name.
    modules: Vec<CatalogModule>,
    /// Every package, in the order they are sold.
    packages: Vec<CatalogPackage>,
}

fn names(modules: &[Module]) -> Vec<String> {
    let mut names: Vec<String> = modules.iter().map(|m| m.as_str().to_string()).collect();
    names.sort();
    names
}

/// Every module this deployment can sell, what each one requires, and the
/// packages they are bundled in. Unauthenticated and deploy-constant, like
/// `GET /limits`: fetch once, cache for the session.
#[utoipa::path(
    get,
    path = "/modules/catalog",
    tag = "meta",
    responses((status = 200, description = "The module catalog", body = CatalogResponse)),
)]
async fn module_catalog() -> Json<CatalogResponse> {
    let mut modules: Vec<CatalogModule> = Module::ALL
        .into_iter()
        .map(|module| CatalogModule {
            module: module.as_str().to_string(),
            package: module.package().as_str().to_string(),
            requires: names(module.requires()),
        })
        .collect();
    modules.sort_by(|a, b| a.module.cmp(&b.module));
    let packages = Package::ALL
        .into_iter()
        .map(|package| CatalogPackage {
            package: package.as_str().to_string(),
            modules: names(&package.modules()),
        })
        .collect();
    Json(CatalogResponse { modules, packages })
}

/// What the caller's school has switched on. Any logged-in user may read it —
/// it is what the client needs to draw its own navigation, not a privileged
/// fact.
#[utoipa::path(
    get,
    path = "/modules",
    tag = "meta",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "The school's enabled modules", body = EnabledResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn my_modules(_user: CurrentUser, tenant: ResolvedTenant) -> Json<EnabledResponse> {
    Json(EnabledResponse {
        enabled: tenant.modules.names(),
    })
}
