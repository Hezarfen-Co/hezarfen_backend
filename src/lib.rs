pub mod config;
pub mod constant;
pub mod database;
pub mod domain;
pub mod error;
pub mod rate_limit;
pub mod state;
pub mod validate;
pub mod web;

use axum::extract::Request;
use axum::http::{HeaderValue, Method, header};
use axum::middleware::{self, Next};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;
use utoipa::openapi::security::{ApiKey, ApiKeyValue, SecurityScheme};
use utoipa::{Modify, OpenApi};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use utoipa_swagger_ui::SwaggerUi;

use crate::rate_limit::RateLimiter;
use crate::state::AppState;

/// Top-level OpenAPI document. Per-path operations and schemas are collected
/// automatically from the `#[utoipa::path]`-annotated handlers via `utoipa-axum`.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Hezarfen Backend API",
        version = env!("CARGO_PKG_VERSION"),
        description = "Notes, events, attendance, courses, and weighted exam marks behind cookie-session auth. Courses carry lesson sessions with teacher-taken roll call; staff clock in/out on a server-stamped work log; attendance reports tally events and per-course roll call with rates.",
    ),
    modifiers(&SecurityAddon),
    tags(
        (name = "meta", description = "Liveness and service metadata"),
        (name = "auth", description = "Registration, login, session lifecycle"),
        (name = "users", description = "User info: self-service profile, plus listing, lookup, and role/profile administration (admin only)"),
        (name = "notes", description = "Per-user notes CRUD"),
        (name = "events", description = "Events and attendance"),
        (name = "courses", description = "Courses, enrollment, course exams, and course sessions"),
        (name = "sessions", description = "Lesson sessions and their roll call (session teacher or course manager marks enrolled students; manager+ marks the teacher)"),
        (name = "work", description = "Staff work log: check-in/check-out stamped by the server clock, manager corrections"),
        (name = "attendance", description = "Attendance summary reports: event tallies + per-course lesson roll call with rates"),
        (name = "exams", description = "Exams (per course, weighted): results, sync/async scheduling, attempts, questions/answers, and live monitoring. Not in this spec (WebSocket): the student exam room at `GET /exams/{id}/attempt/ws` — JSON frames, see the README's \"Taking an exam\" section for the protocol"),
        (name = "marks", description = "Weighted mark reports per course and overall"),
    ),
)]
struct ApiDoc;

/// Registers the `session` cookie as an API-key security scheme so protected
/// operations render an auth requirement in the docs.
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "session_cookie",
                SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::new("session"))),
            );
        }
    }
}

/// Assemble the full application router. Shared by `main` and the test suites.
///
/// Also serves interactive docs: Swagger UI at `/swagger`, raw spec at
/// `/api-docs/openapi.json`. Root `/` mirrors the health probe.
pub fn build_router(state: AppState) -> Router {
    let (router, api) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .route("/", get(health))
        .routes(routes!(health))
        .routes(routes!(server_time))
        .nest("/auth", web::auth::routes(&state.rate_limit))
        .nest("/users", web::users::routes())
        .nest("/notes", web::notes::routes())
        .nest("/events", web::events::routes())
        .nest("/courses", web::courses::routes())
        .nest("/sessions", web::sessions::routes())
        .nest("/exams", web::exams::routes())
        .nest("/marks", web::marks::routes())
        .nest("/work", web::work::routes())
        .nest("/attendance", web::attendance::routes())
        .split_for_parts();

    // Catch-all per-IP limit over every route (Swagger included). Kept inside
    // the CORS layer so a 429 still carries the CORS headers a browser needs
    // to surface the error to frontend code.
    let api_limiter = RateLimiter::per_minute(
        state.rate_limit.api_per_minute,
        state.rate_limit.trust_proxy,
    );

    router
        .merge(SwaggerUi::new("/swagger").url("/api-docs/openapi.json", api))
        .with_state(state)
        .layer(middleware::from_fn(move |req: Request, next: Next| {
            let limiter = api_limiter.clone();
            async move { limiter.enforce(req, next).await }
        }))
        .layer(cors_layer())
        .layer(TraceLayer::new_for_http())
}

/// CORS for a cookie-authenticated API. `CorsLayer::permissive()` would send
/// `Access-Control-Allow-Origin: *`, which browsers refuse to combine with
/// credentialed (cookie) requests — so instead reflect the caller's origin, or a
/// `CORS_ALLOWED_ORIGINS` (comma-separated) allowlist when set, and advertise
/// `Access-Control-Allow-Credentials: true`.
fn cors_layer() -> CorsLayer {
    let layer = CorsLayer::new()
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([header::CONTENT_TYPE])
        .allow_credentials(true);

    let allowlist: Vec<HeaderValue> = std::env::var("CORS_ALLOWED_ORIGINS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|origin| !origin.is_empty())
        .filter_map(|origin| origin.parse().ok())
        .collect();

    if allowlist.is_empty() {
        layer.allow_origin(AllowOrigin::mirror_request())
    } else {
        layer.allow_origin(AllowOrigin::list(allowlist))
    }
}

/// Liveness probe.
#[utoipa::path(
    get,
    path = "/health",
    tag = "meta",
    responses((status = 200, description = "Service is up")),
)]
async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

/// The server's current time. All API timestamps are UTC unix-milliseconds
/// judged by this clock (session expiry included), so a frontend that renders
/// countdowns or "is this in the past?" logic should not trust the device
/// clock — fetch this once, keep `offset = now - Date.now()`, and add the
/// offset to `Date.now()` whenever it needs the authoritative time.
#[derive(serde::Serialize, utoipa::ToSchema)]
struct TimeResponse {
    /// Current server time, UTC unix-milliseconds.
    #[schema(example = 1_752_275_000_000_i64)]
    now: i64,
}

#[utoipa::path(
    get,
    path = "/time",
    tag = "meta",
    responses((status = 200, description = "Current server time (UTC unix-milliseconds)", body = TimeResponse)),
)]
async fn server_time() -> Json<TimeResponse> {
    Json(TimeResponse {
        now: domain::timestamp::Timestamp::now().as_millis(),
    })
}
