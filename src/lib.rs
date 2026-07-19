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
        description = "Notes, events, attendance, courses, and weighted exam marks behind cookie-session auth. Notes carry file attachments (PDFs, documents — stored on disk, capped per file by the school's settings). Any two users can message each other one-to-one, mail-style: each side files its own copy through folders (recipient: inbox/archive/trash with a read flag the sender sees as a receipt; sender: sent/trash), and a message is truly gone only when both sides have deleted it from their trash. Exams run sync (one window), async (window + per-attempt duration), or open (sit anytime), with per-exam attempt limits (retakes) and a teacher-controlled rejoin door on the live exam room; questions can carry images — an illustration on any question, and per-option pictures on choice questions. Enrollment, exam-sitting, roll call, and marks are student-only — staff run them, they don't take part. Every event carries an audience — the whole school, one role, a course's enrollment, or a registration list — that defines its expected-attendee roster (visibility is unaffected: everyone sees every event). Registration-audience events hold a signup list built one seat at a time: teachers register students (students never register themselves), staff register only themselves, the list optionally caps at a capacity, and it closes the moment the event starts. Event attendance is taken by teachers+ against that audience (students never self-mark), and a per-event roster report joins the expected list with the marks to show who missed. Courses come in three behaviorally identical kinds — `course` (a regular class), `study` (a supervised study session — etüt), and `club` (a student club — kulüp) — may cap their roster with an optional enroll-time `capacity` (a full course refuses new members), and carry lesson sessions with teacher-taken roll call; each course also owns its curriculum as a list of subjects, and every exam question must be tagged with one of its course's subjects (a subject still referenced by questions cannot be deleted); staff clock in/out on a server-stamped work log; students run a pomodoro study timer whose focus sessions land in a server-stamped log (starting discards a dangling unfinished session; teachers can read any student's log and its total focus time); attendance reports tally events and per-course roll call with rates. School-varying policy (exam kinds with their course-average weights, attendance statuses, grade-display bands, the note-file size limit) lives in an editable settings singleton, and courses may link to academic terms. Each account also carries its own UI preferences — theme (`light`/`dark`) and language (`tr`/`en`) — self-managed, admin-editable for anyone, and `null` until chosen (the client then follows the device preference). A `parent` role observes without touching: admins tie any number of students to a parent account (`POST /users/{id}/students`), the parent lists them at `GET /users/me/students` and reads each one's mark, attendance, and pomodoro reports — and nothing else; parents hold no staff power and never act as students (no enrolling, sitting exams, or roll call). Every list endpoint accepts `?limit=&offset=` and returns a `{items, total, limit, offset}` page envelope — paging is opt-in, so omitting `limit` returns the full list and `total` always carries the unpaged count.",
    ),
    modifiers(&SecurityAddon),
    tags(
        (name = "meta", description = "Liveness and service metadata"),
        (name = "auth", description = "Registration, login, session lifecycle"),
        (name = "users", description = "User info: self-service profile and UI preferences (theme `light`/`dark`, language `tr`/`en` — returned on every user response, `null` until chosen), name search for the pickers (teacher+, optionally role-filtered), plus listing, lookup, and role/profile/preferences administration (admin only). Also the parent↔student ties: admins link students to a `parent` account under `/users/{id}/students` (link, list, unlink — a role change on either side drops its ties), and a parent lists their own students at `/users/me/students`; the tie grants the parent read access to those students' mark, attendance, and pomodoro reports and nothing else"),
        (name = "notes", description = "Per-user notes CRUD, plus file attachments per note (multipart upload, download, delete — at most 10 per note, each at most the school's settings-configured `max_file_bytes`)"),
        (name = "messages", description = "One-to-one mail-style messages between any two users (parents included — messaging is the one place a parent writes), each optionally tagged with a free-text `label` badge. Each party owns their copy independently: the recipient's moves through `inbox`/`archive`/`trash` with a read flag (visible to the sender as a read receipt), the sender's through `sent`/`trash`. `GET /messages?folder=` lists one folder (`&read=false` narrows to unread — with `limit=1`, `total` is the unread badge); permanent deletion (`DELETE`) works only from the trash and removes the row once both sides have deleted"),
        (name = "events", description = "Events with an audience (school-wide, one role, a course's enrollment, or a registration signup list — the expected-attendee roster; every event stays visible to all) and their attendance: teacher+ marks people in the audience (students never mark, not even themselves), and the roster report joins the expected list with the marks to show who missed. Registration lists are filled seat by seat (teachers place students, staff place only themselves), optionally capped by `capacity`, and close once the event starts"),
        (name = "courses", description = "Courses — kind `course` (regular class), `study` (etüt, a supervised study session), or `club` (kulüp, a student club; same behavior, different label) — optional enroll-time seat `capacity`, enrollment (students only), course exams, and course sessions"),
        (name = "sessions", description = "Lesson sessions and their roll call (session teacher or course manager marks enrolled students; manager+ marks the teacher)"),
        (name = "work", description = "Staff work log: check-in/check-out stamped by the server clock, manager corrections"),
        (name = "pomodoro", description = "Student pomodoro focus log: start/finish stamped by the server clock (starting discards any dangling unfinished session, so a crashed timer never blocks the next one), own history with the unpaged `total_focus_ms` sum, and teacher+ (or linked-parent) reads of any student's log. Breaks and the work/break rhythm stay in the frontend — the backend records only focus stints"),
        (name = "attendance", description = "Attendance summary reports: event tallies + per-course lesson roll call with rates. Own report at `/me`; another user's needs teacher+ (teachers narrowed to their courses) or a parent link to that student"),
        (name = "exams", description = "Exams (per course, weighted by their kind — see `settings`): results, sync/async/open scheduling, attempts (students only — staff never sit) with retakes (`max_attempts`, 0 = unlimited) and a live rejoin door (`allow_rejoin`), questions/answers, and live monitoring (no-shows flagged `absent` once the window closes). Questions may carry images: one illustration per question (any kind — the map the prompt asks about) and one picture per option on `choice` questions, uploaded per slot (multipart, raster types only, ≤ the school's `max_file_bytes`) and frozen with the rest of the question once attempts exist; bytes are served to course managers and to enrolled students once they hold an attempt. A modeless exam is offline-graded — attempts on it are a 409. An exam still being written can be saved as a **draft** (`draft: true` on create, published later by `PATCH`ing `draft: false`): drafts are visible only to the course's managers (students get 404s), cannot be sat or graded, and once attempts or results exist an exam cannot be re-drafted. Not in this spec (WebSocket): the student exam room at `GET /exams/{id}/attempt/ws` — JSON frames; entering clears the attempt's `left_at`, leaving mid-attempt stamps it. See the README's \"Taking an exam\" section for the protocol"),
        (name = "marks", description = "Weighted mark reports per course and overall, labeled by the school's grade bands when configured. Each mark counts its exam kind's settings-configured weight times (weight 1 when the kind was since removed from settings). Own report at `/me`; another user's needs teacher+ (teachers narrowed to their courses) or a parent link to that student"),
        (name = "settings", description = "School policy, one singleton: exam kinds with their course-average weights, attendance statuses, grade-display bands, and the per-file upload size limit (`max_file_bytes` — note files and question images alike). Read: any authenticated user; edit: manager+"),
        (name = "subjects", description = "A course's curriculum topics. Created and listed under `/courses/{id}/subjects`; lookup/edit/delete at `/subjects/{id}`. Every exam question links to one of its course's subjects, so a subject in use cannot be deleted (409) — re-tag or delete the questions first. View follows the course (enrolled users, creator, manager+); edit follows course management rights"),
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
        .nest("/messages", web::messages::routes())
        .nest("/events", web::events::routes())
        .nest("/courses", web::courses::routes())
        .nest("/sessions", web::sessions::routes())
        .nest("/exams", web::exams::routes())
        .nest("/marks", web::marks::routes())
        .nest("/work", web::work::routes())
        .nest("/pomodoro", web::pomodoro::routes())
        .nest("/attendance", web::attendance::routes())
        .nest("/settings", web::settings::routes())
        .nest("/subjects", web::subjects::routes())
        .nest("/terms", web::terms::routes())
        .split_for_parts();

    // Catch-all per-IP limit over every route (Swagger included). Kept inside
    // the CORS layer so a 429 still carries the CORS headers a browser needs
    // to surface the error to frontend code.
    let api_limiter = RateLimiter::per_minute(
        state.rate_limit.api_per_minute,
        state.rate_limit.trust_proxy,
    );

    let cors_allowlist = cors_allowlist_from_env();
    if state.cookie_secure && cors_allowlist.is_empty() {
        tracing::warn!(
            "COOKIE_SECURE is on but CORS_ALLOWED_ORIGINS is unset: production should list its frontend origins explicitly; mirror mode runs uncredentialed, so browser frontends cannot send the session cookie"
        );
    }

    router
        .merge(SwaggerUi::new("/swagger").url("/api-docs/openapi.json", api))
        .with_state(state)
        .layer(middleware::from_fn(move |req: Request, next: Next| {
            let limiter = api_limiter.clone();
            async move { limiter.enforce(req, next).await }
        }))
        .layer(cors_layer(cors_allowlist))
        .layer(TraceLayer::new_for_http())
}

/// Parse the `CORS_ALLOWED_ORIGINS` (comma-separated) allowlist; empty when unset.
fn cors_allowlist_from_env() -> Vec<HeaderValue> {
    std::env::var("CORS_ALLOWED_ORIGINS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|origin| !origin.is_empty())
        .filter_map(|origin| origin.parse().ok())
        .collect()
}

/// CORS for a cookie-authenticated API. `CorsLayer::permissive()` would send
/// `Access-Control-Allow-Origin: *`, which browsers refuse to combine with
/// credentialed (cookie) requests — so origins from the `CORS_ALLOWED_ORIGINS`
/// allowlist are echoed back with `Access-Control-Allow-Credentials: true`.
///
/// With no allowlist (dev) the caller's origin is mirrored, but WITHOUT
/// credentials: mirror + credentials would let any website ride a visitor's
/// session cookie, so the two must NEVER be recombined (the cookie's
/// `SameSite=Lax` in `web/auth.rs` is the only other guard on that door). A
/// cross-origin dev frontend can't send the Lax cookie anyway, so credentials
/// bought nothing in mirror mode; a credentialed browser frontend requires
/// listing its origin in `CORS_ALLOWED_ORIGINS`.
///
/// Takes the allowlist as a parameter (env read once in `build_router`) so
/// tests can exercise both modes without racing on process-global env vars.
pub fn cors_layer(allowlist: Vec<HeaderValue>) -> CorsLayer {
    let layer = CorsLayer::new()
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([header::CONTENT_TYPE])
        // Contract headers cross-origin JS must be able to read: `Retry-After`
        // on 429s and `Content-Disposition` (original filename) on downloads.
        .expose_headers([header::RETRY_AFTER, header::CONTENT_DISPOSITION]);

    if allowlist.is_empty() {
        layer
            .allow_origin(AllowOrigin::mirror_request())
            .allow_credentials(false)
    } else {
        layer
            .allow_origin(AllowOrigin::list(allowlist))
            .allow_credentials(true)
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
