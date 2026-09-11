//! Board-stroke workflows: the drawing, clearing and replay paths the REST
//! surface and the board room drive. The queries live in
//! [`crate::db::board_stroke`]; the room's publish ordering stays in
//! [`crate::web::board_ws`].

use crate::database::Database;
use crate::db::board_stroke;
use crate::domain::board::BoardId;
use crate::domain::board_stroke::BoardStroke;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Append one mark to the board's current `epoch`; the conditional claim in
/// the db layer refuses a full canvas, a locked or closed board, and a stale
/// epoch.
pub async fn append(
    db: &Database,
    board: &BoardId,
    author: &UserId,
    payload: &str,
    epoch: i64,
) -> Result<BoardStroke, AppError> {
    board_stroke::append(db, board, author, payload, epoch).await
}

/// End the current epoch — creator-only upstream: the canvas empties, the
/// history does not.
pub async fn clear(db: &Database, board: &BoardId, by: &UserId) -> Result<BoardStroke, AppError> {
    board_stroke::clear(db, board, by).await
}

/// The current epoch's marks, in mint order, one chunk at a time (`after` is
/// the last stroke already drawn).
pub async fn replay_current(
    db: &Database,
    board: &BoardId,
    epoch: i64,
    after: Option<&str>,
) -> Result<Vec<BoardStroke>, AppError> {
    board_stroke::replay_current(db, board, epoch, after).await
}

/// The whole log, oldest first; `marks_only` drops the `clear` markers.
pub async fn history(
    db: &Database,
    board: &BoardId,
    epoch: Option<i64>,
    marks_only: bool,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<BoardStroke>, i64), AppError> {
    board_stroke::history(db, board, epoch, marks_only, limit, offset).await
}

/// The epoch index: every `clear` marker this board has, oldest first.
pub async fn epochs(db: &Database, board: &BoardId) -> Result<Vec<BoardStroke>, AppError> {
    board_stroke::epochs(db, board).await
}
