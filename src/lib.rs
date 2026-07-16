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
        description = "Notes, events, attendance, courses, and weighted exam marks behind cookie-session auth. Notes carry file attachments (PDFs, documents — stored on disk, capped per file by the school's settings). Exams run sync (one window), async (window + per-attempt duration), or open (sit anytime), with per-exam attempt limits (retakes) and a teacher-controlled rejoin door on the live exam room. Enrollment, exam-sitting, roll call, and marks are student-only — staff run them, they don't take part. Every event carries an audience — the whole school, one role, a course's enrollment, or a registration list — that defines its expected-attendee roster (visibility is unaffected: everyone sees every event). Registration-audience events hold a signup list built one seat at a time: teachers register students (students never register themselves), staff register only themselves, the list optionally caps at a capacity, and it closes the moment the event starts. Event attendance is taken by teachers+ against that audience (students never self-mark), and a per-event roster report joins the expected list with the marks to show who missed. Courses come in two behaviorally identical kinds — `course` (a regular class) and `study` (a supervised study session — etüt) — and carry lesson sessions with teacher-taken roll call; staff clock in/out on a server-stamped work log; attendance reports tally events and per-course roll call with rates. School-varying policy (exam kinds with their course-average weights, attendance statuses, grade-display bands, the note-file size limit) lives in an editable settings singleton, and courses may link to academic terms. Each account also carries its own UI preferences — theme (`light`/`dark`) and language (`tr`/`en`) — self-managed, admin-editable for anyone, and `null` until chosen (the client then follows the device preference). Every list endpoint accepts `?limit=&offset=` and returns a `{items, total, limit, offset}` page envelope — paging is opt-in, so omitting `limit` returns the full list and `total` always carries the unpaged count.",
    ),
    modifiers(&SecurityAddon),
    tags(
        (name = "meta", description = "Liveness and service metadata"),
        (name = "auth", description = "Registration, login, session lifecycle"),
        (name = "users", description = "User info: self-service profile and UI preferences (theme `light`/`dark`, language `tr`/`en` — returned on every user response, `null` until chosen), name search for the pickers (teacher+, optionally role-filtered), plus listing, lookup, and role/profile/preferences administration (admin only)"),
        (name = "notes", description = "Per-user notes CRUD, plus file attachments per note (multipart upload, download, delete — at most 10 per note, each at most the school's settings-configured `max_file_bytes`)"),
        (name = "events", description = "Events with an audience (school-wide, one role, a course's enrollment, or a registration signup list — the expected-attendee roster; every event stays visible to all) and their attendance: teacher+ marks people in the audience (students never mark, not even themselves), and the roster report joins the expected list with the marks to show who missed. Registration lists are filled seat by seat (teachers place students, staff place only themselves), optionally capped by `capacity`, and close once the event starts"),
        (name = "courses", description = "Courses — kind `course` (regular class) or `study` (etüt, a supervised study session; same behavior, different label) — enrollment (students only), course exams, and course sessions"),
        (name = "sessions", description = "Lesson sessions and their roll call (session teacher or course manager marks enrolled students; manager+ marks the teacher)"),
        (name = "work", description = "Staff work log: check-in/check-out stamped by the server clock, manager corrections"),
        (name = "attendance", description = "Attendance summary reports: event tallies + per-course lesson roll call with rates"),
        (name = "exams", description = "Exams (per course, weighted by their kind — see `settings`): results, sync/async/open scheduling, attempts (students only — staff never sit) with retakes (`max_attempts`, 0 = unlimited) and a live rejoin door (`allow_rejoin`), questions/answers, and live monitoring (no-shows flagged `absent` once the window closes). A modeless exam is an offline-graded draft — attempts on it are a 409. Not in this spec (WebSocket): the student exam room at `GET /exams/{id}/attempt/ws` — JSON frames; entering clears the attempt's `left_at`, leaving mid-attempt stamps it. See the README's \"Taking an exam\" section for the protocol"),
        (name = "marks", description = "Weighted mark reports per course and overall, labeled by the school's grade bands when configured. Each mark counts its exam kind's settings-configured weight times (weight 1 when the kind was since removed from settings)"),
        (name = "settings", description = "School policy, one singleton: exam kinds with their course-average weights, attendance statuses, grade-display bands, and the per-file note-upload size limit (`max_file_bytes`). Read: any authenticated user; edit: manager+"),
        (name = "terms", description = "Academic terms (semester/trimester/quarter — whatever the school runs); courses may link to one. Read: any authenticated user; edit: manager+"),
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
        .nest("/settings", web::settings::routes())
        .nest("/terms", web::terms::routes())
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
