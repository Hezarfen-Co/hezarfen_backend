use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::domain::pomodoro::PomodoroSession;
use crate::domain::role::Role;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse};
use crate::state::AppState;

use super::{CurrentUser, PageParams, ensure_can_observe, paginate};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(start))
        .routes(routes!(finish))
        .routes(routes!(my_pomodoro))
        .routes(routes!(user_pomodoro))
}

/// A 403 unless `user` is a student — the pomodoro timer is the students'
/// study tool (staff time-keeping is the work log). Only `start` is gated:
/// finishing merely closes a session that only a student could have opened,
/// so a mid-session promotion still gets its last stint on the record.
fn ensure_student(user: &User) -> Result<(), AppError> {
    if user.get_role() != Role::Student {
        return Err(AppError::Forbidden("only students run pomodoro sessions"));
    }
    Ok(())
}

#[derive(Serialize, ToSchema)]
struct PomodoroResponse {
    id: String,
    /// The student this session belongs to.
    user: String,
    /// Session start, UTC unix-milliseconds (server-stamped).
    started_at: i64,
    /// Session end, UTC unix-milliseconds; `null` while still running.
    finished_at: Option<i64>,
    /// `finished_at - started_at`; `null` while still running.
    duration_ms: Option<i64>,
}

impl PomodoroResponse {
    fn new(session: &PomodoroSession) -> Self {
        let started_at = session.get_started_at().as_millis();
        let finished_at = session.get_finished_at().map(|t| t.as_millis());
        Self {
            id: session.get_id().key().to_string(),
            user: session.get_user().key().to_string(),
            started_at,
            finished_at,
            duration_ms: finished_at.map(|done| done.saturating_sub(started_at)),
        }
    }
}

/// The standard `{items, total, limit, offset}` page envelope over the
/// sessions, plus the unpaged focus total — spelled out (not `Page<T>`) so the
/// extra field rides inside the same flat object.
#[derive(Serialize, ToSchema)]
struct PomodoroLog {
    items: Vec<PomodoroResponse>,
    /// Total sessions in the full log, before `limit`/`offset` are applied.
    #[schema(example = 256)]
    total: i64,
    /// Echo of the applied `limit`; `null` when the response is unbounded.
    #[schema(example = 100)]
    limit: Option<i64>,
    /// Echo of the applied `offset`.
    #[schema(example = 0)]
    offset: i64,
    /// Sum of `duration_ms` over every *finished* session — the whole log,
    /// not just this page. A running session counts nothing until finished.
    total_focus_ms: i64,
}

impl PomodoroLog {
    fn new(sessions: &[PomodoroSession], limit: Option<i64>, offset: i64) -> Self {
        let total_focus_ms = sessions
            .iter()
            .filter_map(|session| {
                session.get_finished_at().map(|done| {
                    done.as_millis()
                        .saturating_sub(session.get_started_at().as_millis())
                })
            })
            .sum();
        Self {
            items: paginate(sessions, limit, offset)
                .iter()
                .map(PomodoroResponse::new)
                .collect(),
            total: sessions.len() as i64,
            limit,
            offset,
            total_focus_ms,
        }
    }
}

/// Start a pomodoro focus session. Students only. The instant is stamped by
/// the server clock — clients never supply it. Always succeeds for a student:
/// a dangling unfinished session (the browser died mid-timer) is discarded
/// and replaced, so there is no way to lock yourself out of starting. The
/// frontend runs the visible countdown and the break rhythm; the backend
/// records only the focus stint.
#[utoipa::path(
    post,
    path = "/start",
    tag = "pomodoro",
    security(("session_cookie" = [])),
    responses(
        (status = 201, description = "Session started — the running session", body = PomodoroResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires the student role", body = ErrorResponse),
    ),
)]
async fn start(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Result<(StatusCode, Json<PomodoroResponse>), AppError> {
    ensure_student(&user)?;
    let session = PomodoroSession::start(user.get_id(), &st.db).await?;
    Ok((StatusCode::CREATED, Json(PomodoroResponse::new(&session))))
}

/// Finish the running pomodoro session, closing it with a server-stamped
/// instant. `409` when nothing is running.
#[utoipa::path(
    post,
    path = "/finish",
    tag = "pomodoro",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "Session finished — the closed session", body = PomodoroResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 409, description = "No pomodoro session running", body = ErrorResponse),
    ),
)]
async fn finish(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Result<Json<PomodoroResponse>, AppError> {
    let session = PomodoroSession::finish(user.get_id(), &st.db).await?;
    Ok(Json(PomodoroResponse::new(&session)))
}

/// The caller's own pomodoro log, newest first — the running session (if any)
/// included (`finished_at: null`) — plus `total_focus_ms`, the unpaged sum of
/// finished-session durations. Paged via `?limit=&offset=` (omit `limit` for
/// the whole log).
#[utoipa::path(
    get,
    path = "/me",
    tag = "pomodoro",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "A page of the caller's pomodoro log with the unpaged focus total", body = PomodoroLog),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn my_pomodoro(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(page): Query<PageParams>,
) -> Result<Json<PomodoroLog>, AppError> {
    let (limit, offset) = page.resolve()?;
    let sessions = PomodoroSession::list_for_user(user.get_id(), &st.db).await?;
    Ok(Json(PomodoroLog::new(&sessions, limit, offset)))
}

/// A student's pomodoro log, newest first, with `total_focus_ms` — the same
/// shape as `/me`. Requires teacher+ (study oversight), or a parent tied to
/// the target student. Paged via `?limit=&offset=`.
#[utoipa::path(
    get,
    path = "/{user}",
    tag = "pomodoro",
    security(("session_cookie" = [])),
    params(("user" = String, Path, description = "User id"), PageParams),
    responses(
        (status = 200, description = "A page of the user's pomodoro log with the unpaged focus total", body = PomodoroLog),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher, or a parent link to this student", body = ErrorResponse),
        (status = 404, description = "User not found", body = ErrorResponse),
    ),
)]
async fn user_pomodoro(
    State(st): State<AppState>,
    CurrentUser(caller): CurrentUser,
    Path(user): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<PomodoroLog>, AppError> {
    let (limit, offset) = page.resolve()?;
    let target = UserId::from_key(&user);
    ensure_can_observe(&caller, &target, &st.db).await?;
    // User must exist — a missing user is a 404, not an empty log.
    User::read(&target, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let sessions = PomodoroSession::list_for_user(&target, &st.db).await?;
    Ok(Json(PomodoroLog::new(&sessions, limit, offset)))
}
