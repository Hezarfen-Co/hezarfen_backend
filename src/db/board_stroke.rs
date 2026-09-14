//! The `board_stroke` table: every mark a board ever carried, plus the `clear`
//! markers that index its epochs, and the board's two stroke counters — all
//! written through the guarded claims in here and nowhere else.

use crate::constant::{
    BOARD_REPLAY_CHUNK, MAX_BOARD_STROKES, MAX_EPOCH_STROKES, MAX_STROKE_PAYLOAD_LEN,
};
use crate::database::{Database, tx_with_retry};
use crate::db::cap;
use crate::db::page::PagedList;
use crate::domain::board::{Board, BoardId, BoardTitle};
use crate::domain::board_stroke::{
    BOARD_CLOSED, BOARD_MOVED, BoardStroke, BoardStrokeId, CANVAS_BLANK, CLEAR_REFUSED, EPOCH_FULL,
    KIND_CLEAR, KIND_STROKE, NOT_THE_CREATOR, state_refusal,
};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;
use crate::validate::validate_required;

/// Append one mark to `board`'s current `epoch`.
///
/// Both stroke counters and the row commit together or not at all: the
/// resettable epoch counter, the lifetime counter, and the open-board guard
/// are one conditional write (the `cap::claim_two_when_and_create` recipe —
/// the lifetime cap's conditions first, both counters bumped by the single
/// `UPDATE` — spelled out at its call site), so a locked or closed board can
/// never be drawn on by a writer that read it a moment earlier.
///
/// `epoch` rides in that guard too. Without it a clear landing between the
/// caller's board read and this write filed the row under the epoch that
/// just ended while its increment counted against the *new* epoch's
/// counter — one race, two markers corrupted for good (the closed one short
/// by a stroke, the next one claiming a stroke that is not there), and
/// markers are never rewritten. Now such a stroke is refused instead.
pub async fn append(
    db: &Database,
    board: &BoardId,
    author: &UserId,
    payload: &str,
    epoch: i64,
) -> Result<BoardStroke, AppError> {
    validate_required("payload", payload, MAX_STROKE_PAYLOAD_LEN)?;
    let saved = sqlx::query_as!(
        BoardStroke,
        r#"WITH bump AS (
               UPDATE board
                  SET epoch_stroke_count = epoch_stroke_count + 1,
                      total_stroke_count = total_stroke_count + 1
                WHERE id = $1
                  AND total_stroke_count < $2
                  AND locked = false AND closed_at IS NULL
                  AND epoch = $3
                  AND epoch_stroke_count < $4
                RETURNING 1)
           INSERT INTO board_stroke (id, board, author, kind, payload, count, epoch, created_at)
           SELECT $5, $1, $6, $7, $8, NULL, $3, $9
           WHERE EXISTS (SELECT 1 FROM bump)
           RETURNING id AS "id: BoardStrokeId", board AS "board: BoardId",
               author AS "author: UserId", kind, payload, count, epoch,
               created_at AS "created_at: Timestamp""#,
        board.uuid(),
        MAX_BOARD_STROKES,
        epoch,
        MAX_EPOCH_STROKES,
        BoardStrokeId::generate().uuid(),
        author.uuid(),
        KIND_STROKE,
        payload,
        Timestamp::now().as_millis()
    )
    .fetch_optional(db)
    .await?;
    match saved {
        Some(stroke) => Ok(stroke),
        // Zero rows means "lifetime full, guard failed, board gone, or soft
        // full" — one marker, as the recipe keeps it. The re-read below maps
        // them with `cap::full_kind` semantics: the terminal cap outranks
        // the recoverable one.
        None => Err(why_refused(db, board).await?),
    }
}

/// The one place `closed` is ordered above `locked` lives in
/// [`state_refusal`]; both refusal paths re-read the board and
/// ask it.
async fn why_refused(db: &Database, board: &BoardId) -> Result<AppError, AppError> {
    let board = crate::db::board::read(db, board)
        .await?
        .ok_or(AppError::NotFound)?;
    if let Some(refusal) = state_refusal(&board) {
        return Ok(AppError::Conflict(refusal));
    }
    // Neither flag, so the counters have to say so *themselves* — this is
    // [`cap::full_kind`]'s mapping, read off the live row: soft full with
    // lifetime room is the recoverable `EPOCH_FULL`. A board that was
    // locked (or a clear that moved the epoch) when the claim ran and is
    // open again now must not be stamped read-only at one stroke of a
    // 50 000 budget — irreversibly, with no reopen — so only a re-read that
    // *proves* the lifetime counter is spent may stamp `closed_at`.
    let (epoch_count, total_count): (i64, i64) = sqlx::query!(
        r#"SELECT epoch_stroke_count AS "epoch_count: i64",
                  total_stroke_count AS "total_count: i64"
           FROM board WHERE id = $1"#,
        board.get_id().uuid()
    )
    .fetch_one(db)
    .await
    .map(|row| (row.epoch_count, row.total_count))?;
    if matches!(
        cap::full_kind(
            epoch_count,
            MAX_EPOCH_STROKES,
            total_count,
            MAX_BOARD_STROKES
        ),
        cap::FullKind::Soft
    ) {
        return Ok(AppError::Conflict(EPOCH_FULL));
    }
    if total_count < MAX_BOARD_STROKES {
        return Ok(AppError::Conflict(BOARD_MOVED));
    }
    // `Board::close` is the one-way idempotent stamp: a second append takes
    // this branch too and leaves the first `closed_at` standing.
    crate::db::board::close(db, &board).await?;
    Ok(AppError::Conflict(BOARD_CLOSED))
}

/// End the current epoch: the canvas empties, the history does not.
///
/// The bump and the marker are one transaction, because the marker carries
/// the epoch's final count — written apart, a crash between them would
/// leave an epoch no marker indexes, and the playback would silently join
/// two sessions into one.
///
/// **The marker pays the lifetime counter.** It is a real `board_stroke`
/// row, so a marker that claimed nothing left `total_stroke_count` counting
/// strokes *drawn* rather than rows *stored*, and `MAX_BOARD_STROKES`
/// stopped bounding the table. Charged here, the counter is exactly the row
/// count again — the transaction takes the board row's write lock (`SELECT
/// … FOR UPDATE`) and reads the epoch and its count from the locked row, so
/// the increment cannot be split off from the marker it pays for, and a
/// stroke claiming under a concurrent writer either lands before this read
/// or waits for this commit — never inside it.
///
/// **A blank canvas cannot be cleared.** Without it every call minted a row
/// for free, one per press of a button the creator can hold down. Requiring
/// the epoch counter to be non-zero pays for each marker with at least one
/// stroke it closes — and "there is nothing on this canvas to clear" is the
/// honest answer to the call it refuses. The epochs index keeps its meaning
/// too: no zero-stroke epoch can enter it.
///
/// **A locked board cannot be cleared.** The lock is the creator's own
/// pause on the canvas, and it holds against every write to it including
/// theirs — the guard is the same condition the stroke path claims under,
/// so "locked" means one thing everywhere. The cost is accepted: a locked
/// board sitting at `MAX_EPOCH_STROKES` is recovered by unlock, clear,
/// relock.
///
/// The marker's own increment is not capped: the last stroke of a board's
/// budget may be closed by a marker, so a board holds at most
/// `MAX_BOARD_STROKES + 1` rows. Capping it would refuse the clear that
/// files the final epoch in the index, which is the one clear the history
/// needs.
pub async fn clear(db: &Database, board: &BoardId, by: &UserId) -> Result<BoardStroke, AppError> {
    // Owned captures (`Send` rule of `tx_with_retry` closures).
    let board = board.clone();
    let by = *by;
    tx_with_retry(db, false, async move |conn| {
        // The pre-image, under the row's write lock: the epoch the marker
        // closes, the count it carries, and the guards it must satisfy all
        // come from this read, and nothing interlocks between it and the
        // guarded bump below.
        let live = sqlx::query_as!(
            Board,
            r#"SELECT id AS "id: BoardId", creator AS "creator: UserId",
                      title AS "title: BoardTitle",
                      ARRAY(SELECT p.participant FROM board_participant p
                            WHERE p.board = board.id ORDER BY p.participant)
                          AS "participants!: Vec<UserId>", locked,
                      locked_by AS "locked_by: UserId",
                      locked_at AS "locked_at: Timestamp", epoch,
                      closed_at AS "closed_at: Timestamp",
                      created_at AS "created_at: Timestamp"
               FROM board WHERE id = $1 FOR UPDATE"#,
            board.uuid()
        )
        .fetch_optional(&mut *conn)
        .await?
        .ok_or(AppError::NotFound)?;
        // Refusal precedence, the old THROW-then-reread order preserved
        // against the locked row: the terminal answer outranks the caller's
        // identity, and identity outranks the recoverable pause.
        if let Some(BOARD_CLOSED) = state_refusal(&live) {
            return Err(AppError::Conflict(BOARD_CLOSED));
        }
        if !live.is_creator(&by) {
            return Err(AppError::Forbidden(NOT_THE_CREATOR));
        }
        if let Some(refusal) = state_refusal(&live) {
            return Err(AppError::Conflict(refusal));
        }
        let (epoch, epoch_count): (i64, i64) = sqlx::query!(
            r#"SELECT epoch AS "epoch: i64", epoch_stroke_count AS "epoch_count: i64"
               FROM board WHERE id = $1"#,
            board.uuid()
        )
        .fetch_one(&mut *conn)
        .await
        .map(|row| (row.epoch, row.epoch_count))?;
        if epoch_count == 0 {
            return Err(AppError::Conflict(CANVAS_BLANK));
        }
        // The bump itself is still guarded — the row lock already
        // guarantees these hold, so a clear can never land on a state it
        // did not read.
        let bumped = sqlx::query!(
            r#"UPDATE board
               SET epoch = epoch + 1,
                   epoch_stroke_count = 0,
                   total_stroke_count = total_stroke_count + 1
               WHERE id = $1 AND epoch = $2 AND epoch_stroke_count = $3"#,
            board.uuid(),
            epoch,
            epoch_count
        )
        .execute(&mut *conn)
        .await?;
        if bumped.rows_affected() != 1 {
            // Unreachable under the row lock; the text is the old
            // refusal marker, kept for the audit trail.
            return Err(AppError::Internal(CLEAR_REFUSED.to_string()));
        }
        let marker = sqlx::query_as!(
            BoardStroke,
            r#"INSERT INTO board_stroke (id, board, author, kind, payload, count, epoch, created_at)
               VALUES ($1, $2, $3, $4, NULL, $5, $6, $7)
               RETURNING id AS "id: BoardStrokeId", board AS "board: BoardId",
               author AS "author: UserId", kind, payload, count, epoch,
               created_at AS "created_at: Timestamp""#,
            BoardStrokeId::generate().uuid(),
            board.uuid(),
            by.uuid(),
            KIND_CLEAR,
            epoch_count,
            epoch,
            Timestamp::now().as_millis()
        )
        .fetch_one(&mut *conn)
        .await?;
        Ok(marker)
    })
    .await
}

/// What a joining socket draws: the current epoch's marks only, in mint
/// order, one [`BOARD_REPLAY_CHUNK`] at a time.
///
/// Paged by `id`, never `created_at` — a millisecond stamp has ties by
/// construction, and the UUIDv7 monotonic time field is exactly what breaks
/// them. `after` is the key of the last stroke already drawn; a key that
/// parses as no UUID reads as the nil id, which every real stroke sorts
/// above — the same full replay a cursor from before the epoch would get.
pub async fn replay_current(
    db: &Database,
    board: &BoardId,
    epoch: i64,
    after: Option<&str>,
) -> Result<Vec<BoardStroke>, AppError> {
    let chunk = BOARD_REPLAY_CHUNK as i64;
    match after.map(BoardStrokeId::from_key) {
        Some(after) => Ok(sqlx::query_as!(
            BoardStroke,
            r#"SELECT id AS "id: BoardStrokeId", board AS "board: BoardId",
               author AS "author: UserId", kind, payload, count, epoch,
               created_at AS "created_at: Timestamp"
               FROM board_stroke
               WHERE board = $1 AND epoch = $2 AND kind = $3 AND id > $4
               ORDER BY id LIMIT $5"#,
            board.uuid(),
            epoch,
            KIND_STROKE,
            after.uuid(),
            chunk
        )
        .fetch_all(db)
        .await?),
        None => Ok(sqlx::query_as!(
            BoardStroke,
            r#"SELECT id AS "id: BoardStrokeId", board AS "board: BoardId",
               author AS "author: UserId", kind, payload, count, epoch,
               created_at AS "created_at: Timestamp"
               FROM board_stroke
               WHERE board = $1 AND epoch = $2 AND kind = $3
               ORDER BY id LIMIT $4"#,
            board.uuid(),
            epoch,
            KIND_STROKE,
            chunk
        )
        .fetch_all(db)
        .await?),
    }
}

/// The whole log, oldest first — every epoch, or one named epoch. Both
/// kinds of row come back: the `clear` markers are what tell a reader where
/// one epoch ended and the next began.
///
/// `marks_only` drops the markers, which is what the *live canvas* wants:
/// the current epoch holds no marker by construction, except in the one
/// race where a clear commits between the board read and this read and
/// files its marker under the epoch just named. The room's replay filters
/// to `kind = 'stroke'` ([`replay_current`]), so without this the two
/// views disagree in exactly that window.
pub async fn history(
    db: &Database,
    board: &BoardId,
    epoch: Option<i64>,
    marks_only: bool,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<BoardStroke>, i64), AppError> {
    // The builder binds positionally in call order, so each optional clause
    // takes the placeholder number its position in the bind sequence gives
    // it — epoch (when present) is always $2, the marker filter $3 only
    // when the epoch clause is there too.
    let (scope, kinds) = match (epoch.is_some(), marks_only) {
        (true, true) => (" AND epoch = $2", " AND kind != $3"),
        (true, false) => (" AND epoch = $2", ""),
        (false, true) => ("", " AND kind != $2"),
        (false, false) => ("", ""),
    };
    let mut list = PagedList::new(
        format!("board_stroke WHERE board = $1{scope}{kinds}"),
        "ORDER BY id",
    )
    .bind(board.uuid());
    if let Some(epoch) = epoch {
        list = list.bind(epoch);
    }
    if marks_only {
        list = list.bind(KIND_CLEAR.to_string());
    }
    list.run::<BoardStroke>(limit, offset, db).await
}

/// The epoch index: every `clear` marker this board has, oldest first.
pub async fn epochs(db: &Database, board: &BoardId) -> Result<Vec<BoardStroke>, AppError> {
    Ok(sqlx::query_as!(
        BoardStroke,
        r#"SELECT id AS "id: BoardStrokeId", board AS "board: BoardId",
               author AS "author: UserId", kind, payload, count, epoch,
               created_at AS "created_at: Timestamp"
           FROM board_stroke WHERE board = $1 AND kind = $2 ORDER BY id"#,
        board.uuid(),
        KIND_CLEAR
    )
    .fetch_all(db)
    .await?)
}

#[cfg(test)]
mod tests {
    // Rebuilt against Postgres in wave 3 (the fixtures were raw SurrealQL).
}
