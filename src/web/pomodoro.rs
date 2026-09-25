use crate::web::tenant_state::State;
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::constant::MAX_POMODORO_LABEL_LEN;
use crate::domain::pomodoro::PomodoroSession;
use crate::domain::role::Role;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::service::parent_link::ensure_can_observe;
use crate::service::pomodoro;
use crate::state::AppState;
use crate::validate::validate_optional;

use super::{CurrentUser, PageParams};

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
    /// Whether this stint moved the lifetime pomodoro counters (and so the
    /// badges and the study streak): it did when it ran at least
    /// `pomodoro.min_counted_ms` and was within that UTC day's
    /// `pomodoro.max_counted_per_day` (both on `GET /limits`). `null` while
    /// still running, and on stints closed before the rule existed. An
    /// uncounted stint is kept and listed exactly like any other.
    counted: Option<bool>,
    /// What the student called this stint when they started it — their own
    /// name for what it was for. `null` on an unnamed stint and on rows
    /// from before the field existed.
    label: Option<String>,
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
            counted: session.get_counted(),
            label: session.get_label().map(str::to_string),
        }
    }
}

/// The standard `{items, total, limit, offset}` page envelope over the
/// sessions, plus the unpaged focus total — spelled out (not `Page<T>`) so the
/// extra field rides inside the same flat object.
#[derive(Serialize, ToSchema)]
struct PomodoroLog {
    items: Vec<PomodoroResponse>,
    /// Total sessions in the log the filter selects, before `limit`/`offset`
    /// are applied.
    #[schema(example = 256)]
    total: i64,
    /// Echo of the applied `limit`; `null` when the response is unbounded.
    #[schema(example = 100)]
    limit: Option<i64>,
    /// Echo of the applied `offset`.
    #[schema(example = 0)]
    offset: i64,
    /// Sum of `duration_ms` over every *finished* session the filter selects —
    /// the whole log, not just this page. A running session counts nothing
    /// until finished.
    total_focus_ms: i64,
}

impl PomodoroLog {
    fn new(page: pomodoro::PomodoroLogPage, limit: Option<i64>, offset: i64) -> Self {
        Self {
            items: page.items.iter().map(PomodoroResponse::new).collect(),
            total: page.total,
            limit,
            offset,
            total_focus_ms: page.total_focus_ms,
        }
    }
}

/// The optional body names the stint: `{"label": "math"}` stores what the
/// student called it, trimmed, and the label then rides every response that
/// carries the session. A blank label starts an unnamed stint and an
/// over-long one is `400`; no body at all is exactly the old no-label start.
#[derive(Deserialize, ToSchema)]
struct StartPomodoro {
    /// The student's own name for what this stint is for — free text, not a
    /// subject reference, bounded by `pomodoro.max_label_len` on `GET
    /// /limits`.
    label: Option<String>,
}

/// Start a pomodoro focus session. Students only. The instant is stamped by
/// the server clock — clients never supply it. Always succeeds for a student:
/// a dangling unfinished session (the browser died mid-timer) is discarded
/// and replaced, so there is no way to lock yourself out of starting. The
/// frontend runs the visible countdown and the break rhythm; the backend
/// records only the focus stint. The body is optional and names the stint:
/// a `label` rides the response and every log it lists in — a blank or
/// absent body starts an unnamed stint, an over-long label is `400`.
#[utoipa::path(
    post,
    path = "/start",
    tag = "pomodoro",
    security(("session_cookie" = [])),
    request_body(content = StartPomodoro, description = "Optional; names the stint"),
    responses(
        (status = 201, description = "Session started — the running session", body = PomodoroResponse),
        (status = 400, description = "Label is over-long", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires the student role", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type", body = ErrorResponse),
    ),
)]
async fn start(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    body: Option<Json<StartPomodoro>>,
) -> Result<(StatusCode, Json<PomodoroResponse>), AppError> {
    ensure_student(&user)?;
    // A blank or absent label records none; a present one is validated (400
    // if over-long) and trimmed before it reaches the row.
    let label = match body.and_then(|Json(req)| req.label) {
        Some(label) if !label.trim().is_empty() => {
            let trimmed = label.trim();
            validate_optional("label", trimmed, MAX_POMODORO_LABEL_LEN)?;
            Some(trimmed.to_string())
        }
        _ => None,
    };
    let session = pomodoro::start(&st.db, user.get_id(), label).await?;
    Ok((StatusCode::CREATED, Json(PomodoroResponse::new(&session))))
}

/// Finish the running pomodoro session, closing it with a server-stamped
/// instant (`409` when nothing is running); answers `counted` — whether it
/// moved the badge counters (ran at least `pomodoro.min_counted_ms`, within
/// that UTC day's `pomodoro.max_counted_per_day`).
///
/// The counters `counted` speaks for are the lifetime pomodoro ones — the
/// badges and the study streak read those. Both bounds are published on
/// `GET /limits`; the day bucket rolls at midnight UTC. A stint that counts
/// for nothing is still
/// recorded, still listed, and still sums into `total_focus_ms` — only the
/// badge counters are held to the rule, because finishing is self-service and a
/// counter moved once per round-trip is farmable.
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
    let session = pomodoro::finish(&st.db, user.get_id()).await?;
    Ok(Json(PomodoroResponse::new(&session)))
}

/// The optional `?from=&to=` window over the log. Both bounds are UTC unix
/// milliseconds and half-open — `from` inclusive, `to` exclusive, so
/// adjacent windows tile without overlap — and independent: each present
/// bound narrows from its own end. Absent, the log is untouched.
#[derive(Debug, Deserialize, IntoParams)]
struct HistoryFilter {
    /// Keep sessions that started at or after this instant.
    #[param(example = 1_760_000_000_000i64)]
    from: Option<i64>,
    /// Keep sessions that started before this instant.
    #[param(example = 1_760_008_640_000i64)]
    to: Option<i64>,
}

impl HistoryFilter {
    /// The validated bounds, or `(None, None)` for the whole log. A negative
    /// bound is a `400` naming the field: the server stamps every instant
    /// itself, so nothing predates the epoch and a negative value is a
    /// malformed request, not an era to page through.
    fn resolve(&self) -> Result<(Option<i64>, Option<i64>), AppError> {
        let bound = |value: Option<i64>, field: &'static str| match value {
            Some(ms) if ms < 0 => Err(AppError::Validation(ValidationError::Invalid {
                field,
                reason: "must not be negative",
            })),
            other => Ok(other),
        };
        Ok((bound(self.from, "from")?, bound(self.to, "to")?))
    }
}

/// The caller's own pomodoro log, newest first — the running session (if any)
/// included (`finished_at: null`) — plus `total_focus_ms`, the unpaged sum of
/// finished-session durations. Paged via `?limit=&offset=` (omit `limit` for
/// the whole log).
///
/// `from`/`to` (unix milliseconds) narrow the log to sessions that started in
/// the half-open window `[from, to)` — both optional, AND-ed when both are
/// given, negative values a `400`. The window bounds the page, `total` and
/// `total_focus_ms` alike: the focus total is the sum over the same filtered
/// set the page reads, and with neither bound the response is the
/// whole-history log, unchanged.
#[utoipa::path(
    get,
    path = "/me",
    tag = "pomodoro",
    security(("session_cookie" = [])),
    params(PageParams, HistoryFilter),
    responses(
        (status = 200, description = "A page of the caller's pomodoro log with the unpaged focus total", body = PomodoroLog),
        (status = 400, description = "Invalid limit, offset, from, or to", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn my_pomodoro(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(page): Query<PageParams>,
    Query(filter): Query<HistoryFilter>,
) -> Result<Json<PomodoroLog>, AppError> {
    let (limit, offset) = page.resolve()?;
    let (from, to) = filter.resolve()?;
    let log = pomodoro::page_for_user(&st.db, user.get_id(), from, to, limit, offset).await?;
    Ok(Json(PomodoroLog::new(log, limit, offset)))
}

/// A student's pomodoro log, newest first, with `total_focus_ms` — the same
/// shape as `/me`. Requires teacher+ (study oversight), or a parent tied to
/// the target student. Paged via `?limit=&offset=`.
///
/// Like `/me`, the optional `from`/`to` window (unix milliseconds, the
/// half-open `[from, to)`) narrows the log — page, `total` and
/// `total_focus_ms` together — and leaves the observation rules untouched: a
/// caller who may not read the log cannot read a window of it either.
#[utoipa::path(
    get,
    path = "/{user}",
    tag = "pomodoro",
    security(("session_cookie" = [])),
    params(("user" = String, Path, description = "User id"), PageParams, HistoryFilter),
    responses(
        (status = 200, description = "A page of the user's pomodoro log with the unpaged focus total", body = PomodoroLog),
        (status = 400, description = "Invalid limit, offset, from, or to", body = ErrorResponse),
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
    Query(filter): Query<HistoryFilter>,
) -> Result<Json<PomodoroLog>, AppError> {
    let (limit, offset) = page.resolve()?;
    let (from, to) = filter.resolve()?;
    let target = UserId::from_key(&user);
    ensure_can_observe(&caller, &target, &st.db).await?;
    // User must exist — a missing user is a 404, not an empty log.
    crate::service::user::read(&st.db, &target)
        .await?
        .ok_or(AppError::NotFound)?;
    let log = pomodoro::page_for_user(&st.db, &target, from, to, limit, offset).await?;
    Ok(Json(PomodoroLog::new(log, limit, offset)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Uri;

    #[test]
    fn an_absent_window_leaves_the_log_untouched() {
        let filter = HistoryFilter {
            from: None,
            to: None,
        };
        assert_eq!(filter.resolve().unwrap(), (None, None));
    }

    #[test]
    fn a_negative_bound_is_a_400_naming_the_field() {
        let filter = HistoryFilter {
            from: Some(-1),
            to: None,
        };
        let AppError::Validation(ValidationError::Invalid { field, .. }) =
            filter.resolve().unwrap_err()
        else {
            panic!("a negative `from` must be a validation error");
        };
        assert_eq!(field, "from");

        let filter = HistoryFilter {
            from: None,
            to: Some(-1),
        };
        let AppError::Validation(ValidationError::Invalid { field, .. }) =
            filter.resolve().unwrap_err()
        else {
            panic!("a negative `to` must be a validation error");
        };
        assert_eq!(field, "to");
    }

    #[test]
    fn zero_is_a_legal_bound() {
        let filter = HistoryFilter {
            from: Some(0),
            to: Some(0),
        };
        assert_eq!(filter.resolve().unwrap(), (Some(0), Some(0)));
    }

    #[test]
    fn a_present_but_empty_bound_is_a_400_naming_the_field() {
        // `?from=` carries no i64: the query extractor itself refuses, the
        // same 400 `?limit=` has always given for an empty value.
        let err = Query::<HistoryFilter>::try_from_uri(&Uri::from_static("/?from=&to=5"))
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(err.body_text().contains("from"), "{}", err.body_text());
    }
}
