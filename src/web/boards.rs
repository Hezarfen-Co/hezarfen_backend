//! The whiteboard's HTTP surface: open a board, read its roster, its live
//! canvas and its whole history, and run the creator's four commands (clear,
//! lock, close, delete). Live drawing itself rides the board-room WebSocket
//! next door; everything here is the REST half it sits beside.
//!
//! The permission model is a role bar plus two predicates
//! (student-and-above, then [`Board::is_participant`], [`Board::is_creator`]),
//! applied in one order on
//! every route: a non-participant gets a **404** even for a board that plainly
//! exists (a 403 would confirm it — see the rationale at `src/lib.rs`), while a
//! participant who is not the creator gets a **403** on the four commands. They
//! are already rendering the board and its strokes, so hiding it from them
//! there would be a lie their client cannot act on (same two-tier line as
//! [`super::homework`]).
//!
//! Every mutation a live room must notice ends in a `board_hub` publish, and
//! one of them is a permission fix rather than a nicety: a participant dropped
//! mid-session keeps drawing over an already-open socket until the room hears
//! about it, so a roster change *must* fan out.

use crate::web::tenant_state::{SchoolSlug, State};
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::constant::MAX_BOARD_PARTICIPANTS;
use crate::database::Database;
use crate::domain::board::{BOARD_ROSTER_LOCK, Board, BoardId, BoardTitle};
use crate::domain::board_stroke::BoardStroke;
use crate::domain::class_group::{ClassGroup, ClassGroupId};
use crate::domain::class_member::ClassMember;
use crate::domain::course::CourseId;
use crate::domain::enrollment::Enrollment;
use crate::domain::event::{Event, EventId};
use crate::domain::role::Role;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;
use crate::tenant::Slug;

use super::courses::can_manage_course;
use super::{CurrentUser, Page, PageParams, RequireStudent, paginate, set_or_clear};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        // Plain route: OpenApiRouter can't describe a WebSocket upgrade, so
        // the board room lives outside the generated spec (see the `boards`
        // tag description and README for the protocol).
        .route("/{id}/ws", axum::routing::get(super::board_ws::board_ws))
        .routes(routes!(create_board, list_boards))
        .routes(routes!(get_board, update_board, delete_board))
        .routes(routes!(list_strokes))
        .routes(routes!(list_history))
        .routes(routes!(list_epochs))
        .routes(routes!(clear_board))
        .routes(routes!(close_board))
        .routes(routes!(invite_board))
}

/// The board, or a 404 — including the deliberate 404 for a caller who is not
/// on it. Every route starts here, so existence never leaks.
///
/// A `parent` is treated as an outsider rather than refused with a 403: the
/// role is barred from the whiteboard entirely, and a 403 would confirm the
/// board exists. [`resolve_participants`] keeps parents off every roster, so
/// this arm only ever fires for a row written before that rule.
async fn board_for(id: &str, user: &User, db: &Database) -> Result<Board, AppError> {
    let board = Board::read(&BoardId::from_key(id), db)
        .await?
        .ok_or(AppError::NotFound)?;
    if !user.get_role().at_least(Role::Student) || !board.is_participant(user.get_id()) {
        return Err(AppError::NotFound);
    }
    Ok(board)
}

/// The creator-only gate. A 403, never a 404: the caller reached it through
/// [`board_for`], so they are a participant and the board's existence is
/// already theirs to see.
fn ensure_creator(board: &Board, user: &User) -> Result<(), AppError> {
    if !board.is_creator(user.get_id()) {
        return Err(AppError::Forbidden(
            "only the board's creator can clear, lock, close or delete it",
        ));
    }
    Ok(())
}

/// The invite list off the wire: deduped, capped, and every id resolved against
/// a real user. The cap is applied *before* the lookup (the domain caps too, but
/// only after the read would already have run), and an unknown id is a 400
/// rather than a silently dropped invitation.
///
/// The eligible set is fetched in **one** read, not one per id. That was a
/// per-id loop while a roster could only be typed by hand and so was a handful
/// of ids; bulk invite made a full board an ordinary thing to own, and a
/// read-modify-write PATCH of one — read the board, drop a name, send the rest
/// back — is the common client shape, so the loop had become
/// `max_participants` sequential round trips on a routine edit.
///
/// A `parent` is refused here, and that is the cut that keeps the role off the
/// whiteboard: never on a roster means [`board_for`] and the room's door already
/// answer 404 on every id-scoped route, and no socket can ever open.
///
/// The creator is a participant by construction, so they are neither injected
/// into the list nor rejected from it.
///
/// `current` is the board's roster as it stands (empty when a board is being
/// created), and it is what makes a read-modify-write PATCH survive: an id
/// *already* on the board that no longer qualifies — demoted, or deleted
/// outright — is dropped silently instead of failing the whole call, so a
/// creator echoing back the roster they were just served gets a 200 and a
/// cleaned list. An id that is **new** to the board still 400s; without that
/// split the drop would be a hole letting a caller seed a roster with anyone.
async fn resolve_participants(
    ids: Option<Vec<String>>,
    current: &[UserId],
    db: &Database,
) -> Result<Vec<UserId>, AppError> {
    let Some(mut ids) = ids else {
        return Ok(Vec::new());
    };
    ids.sort();
    ids.dedup();
    if ids.len() > MAX_BOARD_PARTICIPANTS {
        return Err(AppError::Validation(ValidationError::TooLong {
            field: "participants",
            max: MAX_BOARD_PARTICIPANTS,
            got: ids.len(),
        }));
    }
    let wanted: Vec<UserId> = ids.iter().map(|id| UserId::from_key(id)).collect();
    // Absent from this list means "no such user, or below `student`" — the two
    // are one case here, and telling them apart is what the caller must not be
    // able to do anyway.
    let eligible: Vec<UserId> = crate::service::user::list_by_ids(db, &wanted)
        .await?
        .iter()
        .filter(|found| found.get_role().at_least(Role::Student))
        .map(|found| found.get_id().clone())
        .collect();
    let mut users = Vec::with_capacity(wanted.len());
    for user in wanted {
        if !eligible.contains(&user) {
            if current.contains(&user) {
                continue;
            }
            return Err(AppError::Validation(ValidationError::Invalid {
                field: "participants",
                reason: "every participant must be an existing user of at least the student role",
            }));
        }
        users.push(user);
    }
    Ok(users)
}

/// Push one frame to whoever is in the room right now. An empty room is the
/// normal case, not a failure.
fn fan_out(st: &AppState, slug: &Slug, board: &BoardId, frame: serde_json::Value) {
    st.board_hub.publish(slug, board.key(), frame.to_string());
}

/// The roster frame every path that changes the invite list must publish. A
/// participant dropped from the array keeps drawing over the socket they
/// already hold until the room hears this, so it is a permission fix rather
/// than a notification.
fn participants_frame(board: &Board) -> serde_json::Value {
    json!({
        "type": "participants",
        "creator": board.get_creator().key(),
        "participants": board
            .get_participants()
            .iter()
            .map(|user| user.key())
            .collect::<Vec<_>>(),
    })
}

#[derive(Serialize, ToSchema)]
struct BoardResponse {
    id: String,
    title: String,
    /// The creator: draws, and is the only one who may clear, lock, close or
    /// delete. Always a participant without appearing in `participants`.
    creator: String,
    /// The invited user ids. Everyone here draws.
    participants: Vec<String>,
    /// Drawing is paused while `true`; the creator toggles it.
    locked: bool,
    locked_by: Option<String>,
    /// UTC unix-milliseconds, `null` while unlocked.
    locked_at: Option<i64>,
    /// The live canvas' generation. A clear bumps it; strokes from earlier
    /// epochs stay stored and replayable through `/history`.
    epoch: i64,
    /// Stamped when the board was closed — by its creator or by the lifetime
    /// stroke cap. A closed board is permanently read-only, never deleted.
    closed_at: Option<i64>,
    created_at: i64,
}

impl BoardResponse {
    fn new(board: &Board) -> Self {
        Self {
            id: board.get_id().key().to_string(),
            title: board.get_title().as_str().to_string(),
            creator: board.get_creator().key().to_string(),
            participants: board
                .get_participants()
                .iter()
                .map(|user| user.key().to_string())
                .collect(),
            locked: board.is_locked(),
            locked_by: board.get_locked_by().map(|user| user.key().to_string()),
            locked_at: board.get_locked_at().map(|at| at.as_millis()),
            epoch: board.get_epoch(),
            closed_at: board.get_closed_at().map(|at| at.as_millis()),
            created_at: board.get_created_at().as_millis(),
        }
    }
}

/// One row of the append-only stroke log. Both kinds ride this shape: a
/// `stroke` carries `payload`, a `clear` marker carries `count` — the final
/// stroke count of the epoch it closed — and never the other way round.
#[derive(Serialize, ToSchema)]
struct StrokeResponse {
    id: String,
    author: String,
    /// `stroke` or `clear`.
    kind: String,
    /// The client's serialized mark. `null` on a `clear` marker.
    payload: Option<String>,
    /// The closed epoch's final stroke count. Present only on a `clear`.
    count: Option<i64>,
    epoch: i64,
    created_at: i64,
}

impl StrokeResponse {
    fn new(stroke: &BoardStroke) -> Self {
        Self {
            id: stroke.get_id().key().to_string(),
            author: stroke.get_author().key().to_string(),
            kind: stroke.get_kind().to_string(),
            payload: stroke.get_payload().map(str::to_string),
            count: stroke.get_count(),
            epoch: stroke.get_epoch(),
            created_at: stroke.get_created_at().as_millis(),
        }
    }
}

/// `deny_unknown_fields` on both board request bodies is deliberate: this DTO
/// used to spell the roster `participant_ids` while the response and the PATCH
/// spelled it `participants`, so a client posting a board it had just read got
/// a `201` with a silently empty roster. A misspelled field is now refused
/// instead of dropped.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateBoard {
    #[schema(max_length = 200)]
    title: String,
    /// Who may draw, besides the creator. Omit or send an empty list to open a
    /// board alone and invite later.
    #[schema(max_items = 200)]
    participants: Option<Vec<String>>,
}

/// Open a whiteboard. The caller becomes its creator — the only one who may
/// clear, lock, close or delete it — and everyone named in `participants`
/// may draw on it. Student and above: the `parent` role has no whiteboard
/// access at all, neither as a creator nor as a participant. `409` once the
/// caller holds `max_boards_per_creator` boards (`GET /limits`); delete one to
/// free a seat.
#[utoipa::path(
    post,
    path = "/",
    tag = "boards",
    security(("session_cookie" = [])),
    request_body = CreateBoard,
    responses(
        (status = 201, description = "The new board", body = BoardResponse),
        (status = 400, description = "Invalid title, or a participant list that is too long, names an unknown user, or names a parent", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "The caller is a parent", body = ErrorResponse),
        (status = 409, description = "The caller already holds the maximum number of boards", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, a required field is missing, or it carries a key this request does not accept — a board response cannot be posted back verbatim"),
    ),
)]
async fn create_board(
    State(st): State<AppState>,
    RequireStudent(user): RequireStudent,
    Json(req): Json<CreateBoard>,
) -> Result<(StatusCode, Json<BoardResponse>), AppError> {
    let title = BoardTitle::try_new(&req.title)?;
    // No roster yet, so nothing is grandfathered: every id must qualify.
    let participants = resolve_participants(req.participants, &[], &st.db).await?;
    let board = Board::create(user.get_id(), title, participants, &st.db).await?;
    Ok((StatusCode::CREATED, Json(BoardResponse::new(&board))))
}

#[derive(Deserialize, utoipa::IntoParams)]
struct ListFilter {
    /// Narrow by `closed_at`: `true` = still open, `false` = closed only. Omit
    /// for both. A closed board is never deleted, so this is how a heavy
    /// creator trims a list of retired boards.
    open: Option<bool>,
}

/// Every board the caller may open — the ones they created and the ones they
/// were invited to — newest first. `?open=` narrows by the closed flag, and
/// `total` counts the filtered list. Paged via `?limit=&offset=` (omit `limit`
/// for all of them).
///
/// `?open=true` means "not closed", nothing more: a **locked** board, and one
/// that has spent its lifetime stroke budget but was never drawn on again, are
/// both still open — the closing stamp is only ever written by `/close` or by
/// the append that the lifetime cap refuses.
#[utoipa::path(
    get,
    path = "/",
    tag = "boards",
    security(("session_cookie" = [])),
    params(ListFilter, PageParams),
    responses(
        (status = 200, description = "A page of the caller's boards", body = Page<BoardResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "The caller is a parent", body = ErrorResponse),
    ),
)]
async fn list_boards(
    State(st): State<AppState>,
    RequireStudent(user): RequireStudent,
    Query(filter): Query<ListFilter>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<BoardResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let (boards, total) =
        Board::list_for_user(user.get_id(), filter.open, limit, offset, &st.db).await?;
    let items = boards.iter().map(BoardResponse::new).collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Fetch one board. A caller who is neither its creator nor one of its
/// participants gets a `404`, not a `403` — an outsider must not learn that a
/// board exists.
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "boards",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Board id")),
    responses(
        (status = 200, description = "The board", body = BoardResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Not found, or the caller is not on it", body = ErrorResponse),
    ),
)]
async fn get_board(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<BoardResponse>, AppError> {
    let board = board_for(&id, &user, &st.db).await?;
    Ok(Json(BoardResponse::new(&board)))
}

/// The live canvas: the current epoch's strokes, oldest first, paged via
/// `?limit=&offset=`. This is what a client draws to catch up — earlier epochs
/// are still stored, and read through `/history`. Drawn marks only: a `clear`
/// marker never appears here, exactly as it never appears on the board room's
/// socket. Read `/history` or `/epochs` for the markers.
#[utoipa::path(
    get,
    path = "/{id}/strokes",
    tag = "boards",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Board id"), PageParams),
    responses(
        (status = 200, description = "A page of the current epoch's strokes", body = Page<StrokeResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Not found, or the caller is not on it", body = ErrorResponse),
    ),
)]
async fn list_strokes(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<StrokeResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let board = board_for(&id, &user, &st.db).await?;
    // The current epoch holds no `clear` marker — a marker is written with the
    // epoch it *closed* — so scoping history to it is the live canvas. Markers
    // are dropped anyway: a clear committing between the board read above and
    // this read files one under the epoch just named, and the room's socket
    // never shows it, so without the filter the two views disagree in exactly
    // that race.
    let (strokes, total) = BoardStroke::history(
        board.get_id(),
        Some(board.get_epoch()),
        true,
        limit,
        offset,
        &st.db,
    )
    .await?;
    let items = strokes.iter().map(StrokeResponse::new).collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

#[derive(Deserialize, utoipa::IntoParams)]
struct HistoryParams {
    /// One epoch's rows only. Omit for the board's whole life, oldest first.
    epoch: Option<i64>,
}

/// The whole stroke log, oldest first — every epoch the board ever had, or the
/// single `?epoch=` named. A clear deletes nothing, so this replays the entire
/// session; the `clear` markers in the stream are where one epoch ended.
/// Paged via `?limit=&offset=`.
#[utoipa::path(
    get,
    path = "/{id}/history",
    tag = "boards",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Board id"), HistoryParams, PageParams),
    responses(
        (status = 200, description = "A page of the stroke log", body = Page<StrokeResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Not found, or the caller is not on it", body = ErrorResponse),
    ),
)]
async fn list_history(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Query(scope): Query<HistoryParams>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<StrokeResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let board = board_for(&id, &user, &st.db).await?;
    let (strokes, total) =
        BoardStroke::history(board.get_id(), scope.epoch, false, limit, offset, &st.db).await?;
    let items = strokes.iter().map(StrokeResponse::new).collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// The epoch index: every `clear` marker this board has, oldest first. Each one
/// carries the epoch it closed (`epoch`), that epoch's final stroke count
/// (`count`), who cleared and when — which is all a client needs to offer
/// "replay session 3" without scanning the log. The markers *are* the index, so
/// the current (unclosed) epoch is deliberately absent. Paged via
/// `?limit=&offset=`.
#[utoipa::path(
    get,
    path = "/{id}/epochs",
    tag = "boards",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Board id"), PageParams),
    responses(
        (status = 200, description = "A page of the board's clear markers", body = Page<StrokeResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 404, description = "Not found, or the caller is not on it", body = ErrorResponse),
    ),
)]
async fn list_epochs(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<StrokeResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let board = board_for(&id, &user, &st.db).await?;
    let markers = BoardStroke::epochs(board.get_id(), &st.db).await?;
    // Paged in the web layer: the marker list is one bounded read, and a board
    // has at most one marker per clear.
    let items = paginate(&markers, limit, offset)
        .iter()
        .map(StrokeResponse::new)
        .collect();
    Ok(Json(Page::new(items, markers.len() as i64, limit, offset)))
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdateBoard {
    /// Re-title. Any participant may. Omit to keep.
    #[schema(max_length = 200)]
    title: Option<String>,
    /// Replace the invite list — **creator only**. Omit to keep; send `null`
    /// (or an empty list) to leave the creator alone on the board.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<Vec<String>>, max_items = 200)]
    participants: Option<Option<Vec<String>>>,
    /// Pause (`true`) or resume (`false`) drawing — **creator only**. Omit to
    /// keep.
    locked: Option<bool>,
}

/// Edit a board. Re-titling is open to every participant; changing the invite
/// list or the lock is the creator's alone (`403` for anyone else on the
/// board). Omitted fields keep their value.
///
/// A roster change and a lock change are both fanned out to the live room at
/// once — a participant removed here would otherwise keep drawing over the
/// socket they already hold.
///
/// The roster this route just served can always be sent back: an id already on
/// the board that stopped qualifying is dropped rather than refused (see
/// [`resolve_participants`]). Adding an unqualified id is still a 400.
#[utoipa::path(
    patch,
    path = "/{id}",
    tag = "boards",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Board id")),
    request_body = UpdateBoard,
    responses(
        (status = 200, description = "The updated board", body = BoardResponse),
        (status = 400, description = "Invalid title, or a participant list that is too long, or one that newly names an unknown user or a parent. An id already on the board that no longer qualifies is dropped instead, so the roster just read can be sent back verbatim", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Only the creator may change the participants or the lock", body = ErrorResponse),
        (status = 404, description = "Not found, or the caller is not on it", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, a required field is missing, or it carries a key this request does not accept — a board response cannot be posted back verbatim"),
    ),
)]
async fn update_board(
    State(st): State<AppState>,
    SchoolSlug(slug): SchoolSlug,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<UpdateBoard>,
) -> Result<Json<BoardResponse>, AppError> {
    // A roster PATCH reads the current list (to keep the ids that stopped
    // qualifying) before replacing it, so it is the other read-modify-write of
    // this field and takes the same lock — one an invite could otherwise land
    // inside. Title- and lock-only edits never touch the array and are not held.
    let _guard = match req.participants.is_some() {
        true => Some(BOARD_ROSTER_LOCK.lock().await),
        false => None,
    };
    let mut board = board_for(&id, &user, &st.db).await?;
    if req.participants.is_some() || req.locked.is_some() {
        ensure_creator(&board, &user)?;
    }

    if let Some(title) = req.title.as_deref() {
        board = board.set_title(BoardTitle::try_new(title)?, &st.db).await?;
    }
    if let Some(participants) = req.participants {
        let participants =
            resolve_participants(participants, board.get_participants(), &st.db).await?;
        board = board.set_participants(participants, &st.db).await?;
        fan_out(&st, &slug, board.get_id(), participants_frame(&board));
    }
    if let Some(locked) = req.locked {
        board = board.set_locked(locked, user.get_id(), &st.db).await?;
        fan_out(
            &st,
            &slug,
            board.get_id(),
            json!({"type": "locked", "locked": locked, "by": user.get_id().key()}),
        );
    }
    Ok(Json(BoardResponse::new(&board)))
}

/// Empty the canvas — creator only. **Nothing is deleted**: the board's epoch
/// is bumped and a `clear` marker is appended carrying the closed epoch's final
/// stroke count, so the live canvas is blank while every mark ever drawn stays
/// readable through `/history`. Also resets the live-canvas cap, which is how a
/// board that answered "clear it to keep drawing" is recovered. `409` on a
/// closed board, on a **locked** one — the pause holds against its own creator,
/// so a locked full board is recovered by unlock, clear, relock — and on a
/// canvas that is **already blank**: the marker is a real stroke row charged to
/// the board's lifetime cap, so a clear has to close at least one mark to be
/// worth a row. A board that is both locked and closed answers **closed** —
/// here and on every stroke path alike, since there is no reopen and the pause
/// can never lift.
#[utoipa::path(
    post,
    path = "/{id}/clear",
    tag = "boards",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Board id")),
    responses(
        (status = 201, description = "The clear marker that closed the epoch", body = StrokeResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Only the creator may clear the board", body = ErrorResponse),
        (status = 404, description = "Not found, or the caller is not on it", body = ErrorResponse),
        (status = 409, description = "The canvas is already blank, or the board is locked or closed", body = ErrorResponse),
    ),
)]
async fn clear_board(
    State(st): State<AppState>,
    SchoolSlug(slug): SchoolSlug,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<StrokeResponse>), AppError> {
    let board = board_for(&id, &user, &st.db).await?;
    ensure_creator(&board, &user)?;
    let marker = BoardStroke::clear(board.get_id(), user.get_id(), &st.db).await?;
    // The marker carries the epoch it *closed*; the room's new canvas is the
    // next one.
    fan_out(
        &st,
        &slug,
        board.get_id(),
        json!({
            "type": "cleared",
            "epoch": marker.get_epoch() + 1,
            "by": user.get_id().key(),
        }),
    );
    Ok((StatusCode::CREATED, Json(StrokeResponse::new(&marker))))
}

/// Retire the board — creator only. It becomes permanently read-only: no more
/// strokes and no more clears, while every stroke and every epoch stays
/// readable. Idempotent: closing an already-closed board returns it with its
/// original `closed_at` rather than re-stamping. There is no reopen — a closed
/// board is finished, and a new one is cheap.
#[utoipa::path(
    post,
    path = "/{id}/close",
    tag = "boards",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Board id")),
    responses(
        (status = 200, description = "The closed board", body = BoardResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Only the creator may close the board", body = ErrorResponse),
        (status = 404, description = "Not found, or the caller is not on it", body = ErrorResponse),
    ),
)]
async fn close_board(
    State(st): State<AppState>,
    SchoolSlug(slug): SchoolSlug,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<BoardResponse>, AppError> {
    let board = board_for(&id, &user, &st.db).await?;
    ensure_creator(&board, &user)?;
    let board = board.close(&st.db).await?;
    fan_out(
        &st,
        &slug,
        board.get_id(),
        json!({
            "type": "closed",
            "closed_at": board.get_closed_at().map(|at| at.as_millis()),
        }),
    );
    Ok(Json(BoardResponse::new(&board)))
}

/// A bulk invite's source: a roster that already exists somewhere else, named
/// on the wire and resolved to user ids **once**, at the moment of the call.
/// Tagged by `kind`, the same wire shape an event audience uses.
///
/// The result is a **snapshot**, not a subscription. A board holds a flat list
/// of ids and nothing else — it does not remember it was filled from a class —
/// so a student who joins that class tomorrow is not on yesterday's board, and
/// nothing the class does afterwards puts back a student the creator took off.
/// That is the point of the shape: the roster is a list the creator owns and can
/// edit, and re-inviting the same source is how it is topped up (adding only who
/// is missing). Re-inviting *is* also what undoes a removal — the source still
/// names that student — so a removal is not a per-board ban; there is no
/// exclusion list.
///
/// Live resolution was the alternative and is rejected for this feature: it
/// would make [`Board::is_participant`], the board room's door and
/// `list_for_user` cross-table queries — "which boards may I open" would stop
/// being one indexed read — and it would take the per-person removal away from
/// the creator, which is what a whiteboard is for.
#[derive(Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum InviteSource {
    /// Everyone currently in this class section (şube). The homeroom teacher is
    /// not implied — the field is a label and grants nothing.
    Class { class: String },
    /// Everyone currently enrolled in this course. Clubs and study groups are
    /// courses (`kind` `club` / `study`), so this is how a whole club is
    /// invited. Staff running the course are not enrolled and so are not
    /// included.
    Course { course: String },
    /// The event's expected-attendee roster, exactly as `GET
    /// /events/{id}/roster` resolves it — the signup list for a registration
    /// event, the class or the course roster for the others. A school-wide or
    /// role-wide event will normally overflow the board's participant cap; that
    /// is a `409`, not a truncation.
    Event { event: String },
}

impl InviteSource {
    /// Resolve to raw user ids, refusing a caller who may not read this roster
    /// in the first place.
    ///
    /// The gate is deliberately not a new permission: a board's roster is
    /// visible to everyone on the board, so a bulk invite *discloses* the source
    /// roster. Each arm therefore mirrors the gate on that roster's own listing
    /// route — teacher+ for a class (`GET /classes/{id}/members`),
    /// [`can_manage_course`] for a course (`GET /courses/{id}/enrollments`),
    /// teacher+ for an event (`GET /events/{id}/roster`). A student can still
    /// build a board one id at a time; they cannot dump a class roster into one.
    ///
    /// A source that does not exist is a `400` naming the field, never a `404`:
    /// on these routes a 404 means "no such board, or not yours", and reusing it
    /// here would tell a creator their own board had vanished.
    async fn resolve(self, user: &User, db: &Database) -> Result<Vec<UserId>, AppError> {
        match self {
            InviteSource::Class { class } => {
                if !user.get_role().at_least(Role::Teacher) {
                    return Err(AppError::Forbidden(
                        "requires teacher role or higher to invite a whole class section",
                    ));
                }
                let class = ClassGroupId::from_key(&class);
                if ClassGroup::read(&class, db).await?.is_none() {
                    return Err(AppError::Validation(ValidationError::Invalid {
                        field: "class",
                        reason: "no such class section",
                    }));
                }
                Ok(ClassMember::list_for_class(&class, None, 0, db)
                    .await?
                    .0
                    .iter()
                    .map(|member| member.get_user().clone())
                    .collect())
            }
            InviteSource::Course { course } => {
                let course = crate::service::course::read(db, &CourseId::from_key(&course))
                    .await?
                    .ok_or(AppError::Validation(ValidationError::Invalid {
                        field: "course",
                        reason: "no such course",
                    }))?;
                if !can_manage_course(&course, user) {
                    return Err(AppError::Forbidden(
                        "only the course creator, an assigned teacher, or a manager/admin can invite its roster",
                    ));
                }
                Ok(Enrollment::list_for_course(course.get_id(), None, 0, db)
                    .await?
                    .0
                    .iter()
                    .map(|enrollment| enrollment.get_user().clone())
                    .collect())
            }
            InviteSource::Event { event } => {
                if !user.get_role().at_least(Role::Teacher) {
                    return Err(AppError::Forbidden(
                        "requires teacher role or higher to invite an event's roster",
                    ));
                }
                let event = Event::read(&EventId::from_key(&event), db).await?.ok_or(
                    AppError::Validation(ValidationError::Invalid {
                        field: "event",
                        reason: "no such event",
                    }),
                )?;
                event.get_audience().members(event.get_id(), db).await
            }
        }
    }
}

/// Invite a whole roster at once — creator only, and additive: everyone the
/// source names is **added** to the invite list, nobody is ever removed by it.
/// Re-inviting the same source is therefore how a board is topped up after the
/// class gained a student, and it is idempotent when nothing changed.
///
/// The ids are resolved once, here. The board keeps a flat list and no memory
/// of where it came from, so the roster does not track the source afterwards —
/// see [`InviteSource`] for why. Removal stays `PATCH /boards/{id}`: send the
/// roster you want.
///
/// Three filters run on the resolved list before it lands, and all three are
/// silent — a source is a whole group, and one ineligible member must not fail
/// the invite for the other twenty-nine:
/// - ids that no longer resolve to a user row are dropped,
/// - anyone below the student role is dropped, which is what keeps `parent` off
///   the whiteboard by the same cut [`resolve_participants`] makes,
/// - anyone already on the board (the creator included) is not added twice.
///
/// The cap is **all-or-nothing**: if the union would exceed
/// `max_participants` (`GET /limits`) the whole invite is refused with a `409`
/// naming the two numbers, and the roster is left exactly as it was. A partial
/// invite would silently pick which half of a class gets to draw.
#[utoipa::path(
    post,
    path = "/{id}/invite",
    tag = "boards",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Board id")),
    request_body = InviteSource,
    responses(
        (status = 200, description = "The board with the widened roster", body = BoardResponse),
        (status = 400, description = "The named class, course or event does not exist", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the board's creator, or not allowed to read that roster", body = ErrorResponse),
        (status = 404, description = "Not found, or the caller is not on it", body = ErrorResponse),
        (status = 409, description = "The invite would put the board over its participant cap; nobody was added", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: an unknown or missing `kind`, or the field that `kind` requires is absent"),
    ),
)]
async fn invite_board(
    State(st): State<AppState>,
    SchoolSlug(slug): SchoolSlug,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<InviteSource>,
) -> Result<Json<BoardResponse>, AppError> {
    // Held across the read *and* the write: this is a read-modify-write of one
    // array, so two invites landing together would each union their group into
    // the same roster and the second write would drop the first one's people —
    // silently, with both callers told 200. See [`BOARD_ROSTER_LOCK`].
    let _guard = BOARD_ROSTER_LOCK.lock().await;
    let board = board_for(&id, &user, &st.db).await?;
    ensure_creator(&board, &user)?;
    let invited = req.resolve(&user, &st.db).await?;

    // One read for the whole group. The filters below are silent on purpose —
    // a source is a whole group, and one member who has left or was never
    // eligible must not fail the invite for the other twenty-nine.
    let mut roster = board.get_participants().to_vec();
    for candidate in crate::service::user::list_by_ids(&st.db, &invited).await? {
        if !candidate.get_role().at_least(Role::Student) {
            continue;
        }
        // The creator is a participant by construction and never sits in the
        // array; adding them there would spend a seat on someone who already
        // has access.
        if candidate.get_id() == board.get_creator() || roster.contains(candidate.get_id()) {
            continue;
        }
        roster.push(candidate.get_id().clone());
    }

    if roster.len() > MAX_BOARD_PARTICIPANTS {
        return Err(AppError::ConflictOwned(format!(
            "this invite would put the board at {} participants, over the limit of {MAX_BOARD_PARTICIPANTS}; nobody was added",
            roster.len()
        )));
    }
    // Unchanged rosters still write and still fan out: the alternative is a
    // branch that has to prove the two lists are equal, and a re-invite that
    // added nobody is the idempotent case, not the hot path.
    let board = board.set_participants(roster, &st.db).await?;
    fan_out(&st, &slug, board.get_id(), participants_frame(&board));
    Ok(Json(BoardResponse::new(&board)))
}

/// Delete the board and its whole stroke history — creator only, and the one
/// operation in this system that really destroys marks (a clear never does).
/// It also frees the creator's board seat. Use `/close` to retire a board while
/// keeping it readable.
#[utoipa::path(
    delete,
    path = "/{id}",
    tag = "boards",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Board id")),
    responses(
        (status = 204, description = "Deleted, with every stroke it held"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Only the creator may delete the board", body = ErrorResponse),
        (status = 404, description = "Not found, or the caller is not on it", body = ErrorResponse),
    ),
)]
async fn delete_board(
    State(st): State<AppState>,
    SchoolSlug(slug): SchoolSlug,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let board = board_for(&id, &user, &st.db).await?;
    ensure_creator(&board, &user)?;
    let id = board.get_id().clone();
    board.delete(&st.db).await?;
    // Told after the row is gone: a room that acts on this and then re-reads
    // must find nothing, not a board about to disappear.
    fan_out(&st, &slug, &id, json!({"type": "deleted"}));
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The security boundary of this module, in one test: an outsider is told
    /// the board does not exist, an insider without rights is told it is not
    /// theirs to command. Swapping those two leaks the school's board list on
    /// one side and hides a rendered board from its own participant on the
    /// other.
    #[tokio::test]
    async fn an_outsider_gets_404_and_a_participant_gets_403() {
        let db = crate::database::init_mem().await.unwrap();
        db.query(
            "CREATE user:c SET username = 'c', password_hash = 'x';
             CREATE user:p SET username = 'p', password_hash = 'x';
             CREATE user:s SET username = 's', password_hash = 'x';",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
        let who = async |key: &str| {
            crate::service::user::read(&db, &UserId::from_key(key))
                .await
                .unwrap()
                .unwrap()
        };
        let (creator, participant, stranger) = (who("c").await, who("p").await, who("s").await);
        let board = Board::create(
            creator.get_id(),
            BoardTitle::try_new("Geometri").unwrap(),
            vec![participant.get_id().clone()],
            &db,
        )
        .await
        .unwrap();
        let id = board.get_id().key().to_string();

        assert!(matches!(
            board_for(&id, &stranger, &db).await,
            Err(AppError::NotFound)
        ));
        // A board that does not exist at all answers the same way, so the two
        // are indistinguishable from outside.
        assert!(matches!(
            board_for("nope", &creator, &db).await,
            Err(AppError::NotFound)
        ));

        let seen = board_for(&id, &participant, &db).await.unwrap();
        assert!(matches!(
            ensure_creator(&seen, &participant),
            Err(AppError::Forbidden(_))
        ));
        assert!(ensure_creator(&board_for(&id, &creator, &db).await.unwrap(), &creator).is_ok());
    }
}
