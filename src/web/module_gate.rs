//! The structural half of module entitlements: one gate per router nest.
//!
//! A module *is* a nest, so the gate is applied once where the nest is mounted
//! (`crate::build_router`) rather than route by route — there is no list of
//! guarded routes to keep in step with a list of routes, and a route added to
//! a gated nest is gated by construction.
//!
//! It goes on with `route_layer`, not `layer`: an unmatched path inside a
//! disabled nest must still be a `404`. A disabled module is not a wall around
//! a URL space, it is a refusal on the routes that exist.
//!
//! # The nested exceptions
//!
//! Four route pairs live under another nest but belong to a foreign module, and
//! so carry a second gate of their own ([`crate::web::courses`] and
//! [`crate::web::instances`] split them out for exactly this):
//!
//! - `/courses/{id}/subjects`   → `Module::Subjects`
//! - `/instances/{id}/exams`    → `Module::Exams`
//! - `/instances/{id}/sessions` → `Module::Sessions`
//! - `/instances/{id}/homework` → `Module::Homework`
//!
//! Both gates run (the child's, then the outer one), so either module being off
//! refuses — which is right: `POST /instances/{id}/exams` is an exams feature
//! reached through a class's course.
//!
//! One surface this cannot reach: `crate::ai::server`'s blob stream, which
//! answers `BlobRequest`s without going through the router at all. It checks
//! `Module::CourseNotes` itself.

use axum::extract::{Request, State};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use utoipa_axum::router::OpenApiRouter;

use crate::error::AppError;
use crate::module::Module;
use crate::state::AppState;
use crate::web::tenant_state::resolve_tenant;

/// Wrap every route currently in `router` in `module`'s entitlement check.
///
/// Takes and returns the router rather than handing back a bare `Layer`: the
/// `from_fn_with_state` layer's type is unnameable (it wraps an async closure),
/// so a function returning it could not be written down without boxing. The
/// call site reads the same either way — `gate(notes::routes(), &state,
/// Module::Notes)`.
pub fn gate(
    router: OpenApiRouter<AppState>,
    state: &AppState,
    module: Module,
) -> OpenApiRouter<AppState> {
    router.route_layer(from_fn_with_state((state.clone(), module), check))
}

async fn check(
    State((state, module)): State<(AppState, Module)>,
    request: Request,
    next: Next,
) -> Response {
    let (mut parts, body) = request.into_parts();
    match resolve_tenant(&mut parts, &state).await {
        Err(err) => err.into_response(),
        Ok(tenant) if !tenant.modules.contains(module) => {
            AppError::ModuleDisabled(module).into_response()
        }
        // `resolve_tenant` memoized the school on `parts`, so the handler's own
        // extractors reuse this read instead of making a second one.
        Ok(_) => next.run(Request::from_parts(parts, body)).await,
    }
}
