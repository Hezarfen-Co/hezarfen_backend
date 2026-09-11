//! The `board_stroke` table: every mark a board ever carried, plus the `clear`
//! markers that index its epochs, and the board's two stroke counters — all
//! written through the conditional claims in here and nowhere else.

use surrealdb::types::{RecordId, SurrealValue, Value};

use crate::constant::{
    BOARD_EPOCH_STROKE_COUNT_FIELD, BOARD_OPEN_GUARD, BOARD_REPLAY_CHUNK, BOARD_STROKE_TABLE,
    BOARD_TOTAL_STROKE_COUNT_FIELD, MAX_BOARD_STROKES, MAX_EPOCH_STROKES, MAX_STROKE_PAYLOAD_LEN,
};
use crate::database::{Database, transaction_with_retry};
use crate::db::cap;
use crate::db::page::PagedList;
use crate::domain::board::BoardId;
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
/// are one conditional write, so a locked or closed board can never be
/// drawn on by a writer that read it a moment earlier.
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
    let stroke = BoardStroke {
        id: BoardStrokeId::generate(),
        board: board.clone(),
        author: author.clone(),
        kind: KIND_STROKE.to_string(),
        payload: Some(payload.to_string()),
        count: None,
        epoch,
        created_at: Timestamp::now(),
    };
    // `epoch` is an in-crate `i64` read off the board row, never text from
    // a client, so it interpolates into the guard as a bare integer.
    let guard = format!("{BOARD_OPEN_GUARD} AND epoch = {epoch}");
    match cap::claim_two_when_and_create(
        &board.record(),
        (BOARD_EPOCH_STROKE_COUNT_FIELD, MAX_EPOCH_STROKES),
        (BOARD_TOTAL_STROKE_COUNT_FIELD, MAX_BOARD_STROKES),
        &guard,
        &stroke.id.record(),
        &stroke,
        db,
    )
    .await?
    {
        cap::ClaimedTwo::Made(saved) => Ok(saved),
        // The live canvas is full and nothing else: recoverable by a clear,
        // which resets this counter without losing a single mark.
        cap::ClaimedTwo::FullSoft => Err(AppError::Conflict(EPOCH_FULL)),
        // Three refusals share this answer (lifetime full / guard failed /
        // board gone), so the board is re-read to tell them apart. A board
        // that changed state in between still gets told "no" — but only a
        // re-read that *proves* the lifetime counter is spent may stamp
        // `closed_at`, never elimination.
        cap::ClaimedTwo::FullHard => Err(why_refused(db, board).await?),
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
    // Neither flag, so the lifetime counter has to say so *itself*:
    // `FullHard` also fires when the guard failed, so a board that was
    // locked (or a clear that moved the epoch) when the claim ran and is
    // open again now would otherwise be stamped read-only at one stroke of
    // a 50 000 budget — irreversibly, with no reopen.
    if total_strokes(db, board.get_id()).await? < MAX_BOARD_STROKES {
        return Ok(AppError::Conflict(BOARD_MOVED));
    }
    // `Board::close` is the one-way idempotent stamp: a second append takes
    // this branch too and leaves the first `closed_at` standing.
    crate::db::board::close(db, &board).await?;
    Ok(AppError::Conflict(BOARD_CLOSED))
}

/// The lifetime counter as the store holds it — the board struct
/// deliberately does not carry it (src/domain/board.rs:9-13).
async fn total_strokes(db: &Database, board: &BoardId) -> Result<i64, AppError> {
    let mut result = db
        .query(format!(
            "SELECT VALUE ({BOARD_TOTAL_STROKE_COUNT_FIELD} ?? 0) FROM $b"
        ))
        .bind(("b", board.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<i64>>(0)?.into_iter().next().unwrap_or(0))
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
/// count again — the counter is reset and re-read in the same statement, so
/// the increment cannot be split off from the marker it pays for.
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
/// theirs — [`BOARD_OPEN_GUARD`] is the same condition the stroke path
/// claims under, so "locked" means one thing everywhere. The cost is
/// accepted: a locked board sitting at `MAX_EPOCH_STROKES` is recovered by
/// unlock, clear, relock.
///
/// The marker's own increment is not capped: the last stroke of a board's
/// budget may be closed by a marker, so a board holds at most
/// `MAX_BOARD_STROKES + 1` rows. Capping it would refuse the clear that
/// files the final epoch in the index, which is the one clear the history
/// needs.
pub async fn clear(db: &Database, board: &BoardId, by: &UserId) -> Result<BoardStroke, AppError> {
    let marker = BoardStrokeId::generate();
    // `($before[0].epoch_stroke_count ?? 0)` is parenthesized: the bare form
    // parses as `?? (0 = …)` and stores a boolean count (src/constant.rs:699).
    let sql = format!(
        "BEGIN TRANSACTION;
         LET $before = (UPDATE $b SET epoch += 1, {BOARD_EPOCH_STROKE_COUNT_FIELD} = 0, \
             {BOARD_TOTAL_STROKE_COUNT_FIELD} = ({BOARD_TOTAL_STROKE_COUNT_FIELD} ?? 0) + 1 \
             WHERE creator = $by AND {BOARD_OPEN_GUARD} \
               AND ({BOARD_EPOCH_STROKE_COUNT_FIELD} ?? 0) > 0 RETURN BEFORE);
         IF array::len($before) = 0 {{ THROW '{CLEAR_REFUSED}' }};
         CREATE $mid CONTENT {{ board: $b, author: $by, kind: $kind, \
             epoch: $before[0].epoch, \
             count: ($before[0].{BOARD_EPOCH_STROKE_COUNT_FIELD} ?? 0), \
             created_at: $now }};
         COMMIT TRANSACTION;"
    );
    let bound: Vec<(String, Value)> = vec![
        ("b".into(), board.record().into_value()),
        ("by".into(), by.record().into_value()),
        ("mid".into(), marker.record().into_value()),
        ("kind".into(), KIND_CLEAR.into_value()),
        ("now".into(), Timestamp::now().as_millis().into_value()),
    ];
    // The epoch counter is reset here, which is a counter write like any
    // other, so it owes the process the same one-at-a-time discipline
    // (`cap::counter_lock`) — and unlike a lone conditional claim it is
    // read back in the same statement, by the marker's `count`.
    let _guard = cap::counter_lock().await;
    let (mut result, mut errors) =
        transaction_with_retry(db, &sql, &bound, &[CLEAR_REFUSED]).await?;
    if errors
        .values()
        .any(|error| error.to_string().contains(CLEAR_REFUSED))
    {
        return Err(why_clear_refused(db, board, by).await?);
    }
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    // Slots count BEGIN, the LET and the IF: the CREATE is slot 3. Only
    // read once the error map is empty — `take_errors` swap-removes.
    result
        .take::<Vec<BoardStroke>>(3)?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::Internal("board clear wrote no marker".into()))
}

/// Which of the clear guard's four conditions said no. One `WHERE` cannot
/// report that itself, so the board is re-read — on the refusal path only,
/// like [`why_refused`]. The distinction is not cosmetic: a closed
/// board is terminal for the room, a blank canvas is an ordinary "nothing
/// to do" and a room told otherwise stops drawing for good.
async fn why_clear_refused(
    db: &Database,
    board: &BoardId,
    by: &UserId,
) -> Result<AppError, AppError> {
    let board = crate::db::board::read(db, board)
        .await?
        .ok_or(AppError::NotFound)?;
    let state = state_refusal(&board);
    // Terminal outranks the caller's identity: a closed board is read-only
    // for its creator too, so "closed" is the honest answer to anyone.
    if state == Some(BOARD_CLOSED) {
        return Ok(AppError::Conflict(BOARD_CLOSED));
    }
    if !board.is_creator(by) {
        return Ok(AppError::Forbidden(NOT_THE_CREATOR));
    }
    // A lock pauses the *creator* too, and it is a state the caller can
    // undo — a `Conflict`, not the `Forbidden` a wrong caller gets. Unlock,
    // clear, relock is the recovery for a locked board that is also full.
    if let Some(refusal) = state {
        return Ok(AppError::Conflict(refusal));
    }
    Ok(AppError::Conflict(CANVAS_BLANK))
}

/// What a joining socket draws: the current epoch's marks only, in mint
/// order, one [`BOARD_REPLAY_CHUNK`] at a time.
///
/// Paged by `id`, never `created_at` — a millisecond stamp has ties by
/// construction, and the ULID's monotonic low half is exactly what breaks
/// them. `after` is the key of the last stroke already drawn.
pub async fn replay_current(
    db: &Database,
    board: &BoardId,
    epoch: i64,
    after: Option<&str>,
) -> Result<Vec<BoardStroke>, AppError> {
    let cursor = match after {
        Some(_) => " AND id > $after",
        None => "",
    };
    let mut result = db
        .query(format!(
            "SELECT * FROM {} \
             WHERE board = $b AND epoch = $e AND kind = $kind{cursor} \
             ORDER BY id LIMIT $chunk",
            BOARD_STROKE_TABLE
        ))
        .bind(("b", board.record()))
        .bind(("e", epoch))
        .bind(("kind", KIND_STROKE))
        .bind((
            "after",
            RecordId::new(BOARD_STROKE_TABLE, after.unwrap_or_default()),
        ))
        .bind(("chunk", BOARD_REPLAY_CHUNK as i64))
        .await?
        .check()?;
    Ok(result.take::<Vec<BoardStroke>>(0)?)
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
    let scope = match epoch {
        Some(_) => " AND epoch = $e",
        None => "",
    };
    let kinds = if marks_only {
        " AND kind != $clear"
    } else {
        ""
    };
    PagedList::new(
        format!("{} WHERE board = $b{scope}{kinds}", BOARD_STROKE_TABLE),
        "ORDER BY id",
    )
    .bind("b", board.record())
    .bind("e", epoch.unwrap_or_default())
    .bind("clear", KIND_CLEAR.to_string())
    .run(limit, offset, db)
    .await
}

/// The epoch index: every `clear` marker this board has, oldest first.
pub async fn epochs(db: &Database, board: &BoardId) -> Result<Vec<BoardStroke>, AppError> {
    let mut result = db
        .query(format!(
            "SELECT * FROM {} \
             WHERE board = $b AND kind = $kind ORDER BY id",
            BOARD_STROKE_TABLE
        ))
        .bind(("b", board.record()))
        .bind(("kind", KIND_CLEAR))
        .await?
        .check()?;
    Ok(result.take::<Vec<BoardStroke>>(0)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::board::Board;
    use crate::domain::board::BoardTitle;
    use crate::domain::board_stroke::{BOARD_CLOSED, BOARD_LOCKED};

    async fn a_db() -> Database {
        let db = crate::database::init_mem().await.unwrap();
        db.query(
            "CREATE user:c SET username = 'c', password_hash = 'x';
             CREATE user:p SET username = 'p', password_hash = 'x';",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
        db
    }

    fn user(key: &str) -> UserId {
        UserId::from_key(key)
    }

    async fn a_board(db: &Database) -> Board {
        crate::db::board::create(
            db,
            &user("c"),
            BoardTitle::try_new("Geometri").unwrap(),
            vec![user("p")],
        )
        .await
        .unwrap()
    }

    /// Every stroke row of a board, read straight out of the store — never off
    /// a return value, which the in-memory engine forges wins on
    /// (src/db/cap.rs:44-49).
    async fn rows(db: &Database, board: &BoardId) -> Vec<BoardStroke> {
        let mut result = db
            .query("SELECT * FROM board_stroke WHERE board = $b ORDER BY id")
            .bind(("b", board.record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        result.take(0).unwrap()
    }

    async fn reread(db: &Database, board: &Board) -> Board {
        crate::db::board::read(db, board.get_id())
            .await
            .unwrap()
            .unwrap()
    }

    async fn set_counters(db: &Database, board: &Board, epoch: i64, total: i64) {
        db.query("UPDATE $id SET epoch_stroke_count = $e, total_stroke_count = $t")
            .bind(("id", board.get_id().record()))
            .bind(("e", epoch))
            .bind(("t", total))
            .await
            .unwrap()
            .check()
            .unwrap();
    }

    async fn draw(db: &Database, board: &Board) -> Result<BoardStroke, AppError> {
        append(
            db,
            board.get_id(),
            &user("p"),
            "{\"p\":[1,2]}",
            board.get_epoch(),
        )
        .await
    }

    #[tokio::test]
    async fn a_full_canvas_is_refused_but_the_board_stays_open() {
        let db = a_db().await;
        let board = a_board(&db).await;
        set_counters(&db, &board, MAX_EPOCH_STROKES, 10).await;
        let refused = draw(&db, &board).await;
        assert!(matches!(refused, Err(AppError::Conflict(msg)) if msg.contains("clear it")));
        // Nothing written, and above all: NOT closed — a full canvas is
        // recoverable, and closing it here would make it permanent.
        assert!(rows(&db, board.get_id()).await.is_empty());
        assert!(reread(&db, &board).await.get_closed_at().is_none());
    }

    /// The user's decision in one test: a clear empties the canvas and loses
    /// nothing. If this ever passes with the pre-clear rows gone, the feature
    /// is wrong.
    #[tokio::test]
    async fn a_clear_keeps_every_pre_clear_stroke() {
        let db = a_db().await;
        let board = a_board(&db).await;
        for _ in 0..3 {
            draw(&db, &board).await.unwrap();
        }
        let before: Vec<String> = rows(&db, board.get_id())
            .await
            .iter()
            .map(|row| row.get_id().key().to_string())
            .collect();

        clear(&db, board.get_id(), &user("c")).await.unwrap();
        let board = reread(&db, &board).await;
        assert_eq!(board.get_epoch(), 1);
        draw(&db, &board).await.unwrap();

        // The three original rows are still in the table, by direct read.
        let after: Vec<String> = rows(&db, board.get_id())
            .await
            .iter()
            .map(|row| row.get_id().key().to_string())
            .collect();
        for key in &before {
            assert!(after.contains(key), "a clear deleted stroke {key}");
        }
        // 3 old + 1 marker + 1 new, and the live canvas is the new one alone.
        assert_eq!(after.len(), 5);
        let live = replay_current(&db, board.get_id(), 1, None).await.unwrap();
        assert_eq!(live.len(), 1);
        // The old epoch is still replayable in full.
        assert_eq!(
            replay_current(&db, board.get_id(), 0, None)
                .await
                .unwrap()
                .len(),
            3
        );
    }

    /// The marker IS the epoch index, so its `count` has to be that epoch's
    /// final stroke count — and `count` must appear on no other row.
    #[tokio::test]
    async fn the_marker_counts_the_epoch_it_closed() {
        let db = a_db().await;
        let board = a_board(&db).await;
        for _ in 0..4 {
            draw(&db, &board).await.unwrap();
        }
        let marker = clear(&db, board.get_id(), &user("c")).await.unwrap();
        assert_eq!(marker.get_count(), Some(4));
        assert_eq!(marker.get_epoch(), 0);
        assert!(marker.is_clear());

        // And the epoch counter reset, so drawing resumes against a fresh cap.
        let board = reread(&db, &board).await;
        draw(&db, &board).await.unwrap();
        let epochs = epochs(&db, board.get_id()).await.unwrap();
        assert_eq!(epochs.len(), 1);
        assert_eq!(epochs[0].get_count(), Some(4));
        // `count` lives on clear markers only — this module is that guard.
        for row in rows(&db, board.get_id()).await {
            assert_eq!(row.get_count().is_some(), row.is_clear());
            assert_eq!(row.get_payload().is_some(), !row.is_clear());
        }
    }

    #[tokio::test]
    async fn the_lifetime_cap_closes_the_board_exactly_once() {
        let db = a_db().await;
        let board = a_board(&db).await;
        set_counters(&db, &board, 0, MAX_BOARD_STROKES).await;
        let refused = draw(&db, &board).await;
        assert!(matches!(refused, Err(AppError::Conflict(msg)) if msg.contains("read-only")));
        let stamp = reread(&db, &board).await.get_closed_at();
        assert!(stamp.is_some());

        // A whole millisecond apart, so a re-stamp could not go unnoticed.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        assert!(draw(&db, &board).await.is_err());
        assert_eq!(reread(&db, &board).await.get_closed_at(), stamp);
        assert!(rows(&db, board.get_id()).await.is_empty());
    }

    /// A locked board must be told it is locked. Both refusals arrive as the
    /// same `FullHard`, so a re-read that got this wrong would tell a paused
    /// room its canvas is full — and send it clearing a board it need not clear.
    #[tokio::test]
    async fn a_locked_board_is_refused_as_locked() {
        let db = a_db().await;
        let board = a_board(&db).await;
        let board = crate::db::board::set_locked(&db, &board, true, &user("c"))
            .await
            .unwrap();
        let refused = draw(&db, &board).await;
        assert!(matches!(refused, Err(AppError::Conflict(msg)) if msg.contains("locked")));
        assert!(rows(&db, board.get_id()).await.is_empty());
        // A lock is not a close: unlocking restores drawing.
        assert!(reread(&db, &board).await.get_closed_at().is_none());
        let board = crate::db::board::set_locked(&db, &board, false, &user("c"))
            .await
            .unwrap();
        assert!(draw(&db, &board).await.is_ok());
    }

    /// A lock the stroke lost to, unlocked again before the refusal re-reads
    /// the row — which is the state `why_refused` is handed after a guard
    /// failure. Reading "neither locked nor closed" as proof of the lifetime
    /// cap stamped `closed_at` on a board holding one stroke of 50 000, with
    /// no reopen. Only the counter itself may close a board.
    #[tokio::test]
    async fn a_refusal_on_an_open_board_never_closes_it() {
        let db = a_db().await;
        let board = a_board(&db).await;
        let board = crate::db::board::set_locked(&db, &board, true, &user("c"))
            .await
            .unwrap();
        // The claim loses the open-guard while the board is locked...
        assert!(matches!(
            draw(&db, &board).await,
            Err(AppError::Conflict(_))
        ));
        // ...and the creator unlocks before the refusal picks its message.
        let board = crate::db::board::set_locked(&db, &board, false, &user("c"))
            .await
            .unwrap();
        let refused = why_refused(&db, board.get_id()).await.unwrap();
        assert!(matches!(refused, AppError::Conflict(msg) if !msg.contains("read-only")));
        // Stored state, not the return value: the board is still open, and
        // still drawable.
        assert!(reread(&db, &board).await.get_closed_at().is_none());
        assert!(draw(&db, &reread(&db, &board).await).await.is_ok());
    }

    /// The epoch index must never drift, in either direction. A stroke carrying
    /// a pre-clear epoch used to land under the closed epoch while its
    /// increment counted against the new one — the closed marker short by a
    /// stroke, the next marker claiming one that is not there, both permanent.
    #[tokio::test]
    async fn a_stale_epoch_append_leaves_no_marker_drifted() {
        let db = a_db().await;
        let board = a_board(&db).await;
        draw(&db, &board).await.unwrap();
        clear(&db, board.get_id(), &user("c")).await.unwrap();

        // Exactly what `live_board` hands `append` when a clear lands between
        // the board read and the write (src/web/board_ws.rs:431).
        let stale = append(&db, board.get_id(), &user("p"), "{\"p\":[9]}", 0).await;
        assert!(matches!(stale, Err(AppError::Conflict(_))));

        let board = reread(&db, &board).await;
        draw(&db, &board).await.unwrap();
        clear(&db, board.get_id(), &user("c")).await.unwrap();

        // Every marker's count is the rows the store actually holds at the
        // epoch it closed.
        let stored = rows(&db, board.get_id()).await;
        for marker in stored.iter().filter(|row| row.is_clear()) {
            let actual = stored
                .iter()
                .filter(|row| !row.is_clear() && row.get_epoch() == marker.get_epoch())
                .count() as i64;
            assert_eq!(
                marker.get_count(),
                Some(actual),
                "marker for epoch {} drifted",
                marker.get_epoch()
            );
        }
        // And the refused stroke claimed nothing: two marks and the two
        // markers that closed them, which is every row on the table.
        assert_eq!(total_strokes(&db, board.get_id()).await.unwrap(), 4);
        assert_eq!(stored.len(), 4);
    }

    /// The lock pauses the canvas against *every* write to it, its creator's
    /// clear included — a `Conflict`, since unlocking undoes it, not the
    /// `Forbidden` a non-creator gets. Asserted off stored rows: the refusal
    /// must mint no marker and must not bump the epoch.
    #[tokio::test]
    async fn a_locked_board_refuses_its_creator_s_clear() {
        let db = a_db().await;
        let board = a_board(&db).await;
        draw(&db, &board).await.unwrap();
        let board = crate::db::board::set_locked(&db, &board, true, &user("c"))
            .await
            .unwrap();

        let refused = clear(&db, board.get_id(), &user("c")).await;
        assert!(matches!(refused, Err(AppError::Conflict(msg)) if msg == BOARD_LOCKED));
        assert_eq!(rows(&db, board.get_id()).await.len(), 1);
        assert_eq!(reread(&db, &board).await.get_epoch(), 0);
        assert!(epochs(&db, board.get_id()).await.unwrap().is_empty());

        // Unlock, and the same clear lands: unlock → clear → relock is the
        // recovery for a locked board sitting at the epoch cap.
        let board = crate::db::board::set_locked(&db, &board, false, &user("c"))
            .await
            .unwrap();
        let marker = clear(&db, board.get_id(), &user("c")).await.unwrap();
        assert_eq!(marker.get_count(), Some(1));
        assert_eq!(reread(&db, &board).await.get_epoch(), 1);
    }

    /// Both classifiers, one board: the words a stroke is refused with and the
    /// words the *creator's* clear is refused with. They must match — the two
    /// used to be independent chains and drifted apart on the state below.
    async fn both_refusals(db: &Database, board: &Board) -> (&'static str, &'static str) {
        let stroke = match draw(db, board).await {
            Err(AppError::Conflict(msg)) => msg,
            other => panic!("the stroke was not refused: {other:?}"),
        };
        let clear = match clear(db, board.get_id(), &user("c")).await {
            Err(AppError::Conflict(msg)) => msg,
            other => panic!("the clear was not refused: {other:?}"),
        };
        (stroke, clear)
    }

    /// A board that is locked *and* closed is terminal, and both paths have to
    /// say so. Reaching the state is ungated in either direction — `set_locked`
    /// carries no `closed_at` test and `close` carries no lock test — and the
    /// stroke path used to test the lock first, so a permanently read-only
    /// board answered every mark with "the creator has paused drawing" while
    /// the same board answered a clear with the terminal words. A room told to
    /// wait for a pause that cannot lift waits forever.
    ///
    /// Locking a closed board stays *allowed*: it is a no-op on a board that is
    /// already read-only, and refusing it would break the second half of a
    /// close-then-lock a client may legitimately send in either order.
    #[tokio::test]
    async fn locked_and_closed_answers_closed_whichever_came_first() {
        for closed_first in [false, true] {
            let db = a_db().await;
            let board = a_board(&db).await;
            // A mark on the canvas, so "nothing to clear" cannot be the reason.
            draw(&db, &board).await.unwrap();

            let board = if closed_first {
                let board = crate::db::board::close(&db, &board).await.unwrap();
                crate::db::board::set_locked(&db, &board, true, &user("c"))
                    .await
                    .unwrap()
            } else {
                let board = crate::db::board::set_locked(&db, &board, true, &user("c"))
                    .await
                    .unwrap();
                crate::db::board::close(&db, &board).await.unwrap()
            };
            // Stored state: both flags really are up, and the close stamp stood.
            let stored = reread(&db, &board).await;
            assert!(stored.is_locked() && stored.get_closed_at().is_some());

            assert_eq!(
                both_refusals(&db, &board).await,
                (BOARD_CLOSED, BOARD_CLOSED),
                "closed_first = {closed_first}"
            );
            // And the refusals wrote nothing: one mark, no marker, epoch 0.
            assert_eq!(rows(&db, board.get_id()).await.len(), 1);
            assert_eq!(reread(&db, &board).await.get_epoch(), 0);
        }
    }

    /// The other half of the same contract: a board holding *one* of the flags
    /// keeps its own answer on both paths. A lock is recoverable, a close is
    /// not, and collapsing either into the other is the mislabel above.
    #[tokio::test]
    async fn one_flag_alone_keeps_its_own_answer_on_both_paths() {
        let db = a_db().await;
        let locked = a_board(&db).await;
        draw(&db, &locked).await.unwrap();
        let locked = crate::db::board::set_locked(&db, &locked, true, &user("c"))
            .await
            .unwrap();
        assert_eq!(
            both_refusals(&db, &locked).await,
            (BOARD_LOCKED, BOARD_LOCKED)
        );

        let closed = a_board(&db).await;
        draw(&db, &closed).await.unwrap();
        let closed = crate::db::board::close(&db, &closed).await.unwrap();
        assert_eq!(
            both_refusals(&db, &closed).await,
            (BOARD_CLOSED, BOARD_CLOSED)
        );
    }

    /// The `next_ulid` hazard: rows minted inside one millisecond must replay
    /// in mint order, not at random (src/domain/monotonic_id.rs:41-46).
    #[tokio::test]
    async fn replay_is_in_mint_order_within_a_millisecond() {
        let db = a_db().await;
        let board = a_board(&db).await;
        let mut minted = Vec::new();
        for _ in 0..25 {
            minted.push(draw(&db, &board).await.unwrap().get_id().key().to_string());
        }
        let replayed: Vec<String> = replay_current(&db, board.get_id(), 0, None)
            .await
            .unwrap()
            .iter()
            .map(|row| row.get_id().key().to_string())
            .collect();
        assert_eq!(replayed, minted);

        // The cursor resumes exactly where the last chunk stopped.
        let rest: Vec<String> = replay_current(&db, board.get_id(), 0, Some(&minted[9]))
            .await
            .unwrap()
            .iter()
            .map(|row| row.get_id().key().to_string())
            .collect();
        assert_eq!(rest, minted[10..]);
    }

    /// The one legitimate delete of stroke rows: without it, deleting a board
    /// orphans its whole history under a record that no longer exists.
    #[tokio::test]
    async fn deleting_a_board_takes_its_strokes_with_it() {
        let db = a_db().await;
        let board = a_board(&db).await;
        let other = a_board(&db).await;
        draw(&db, &board).await.unwrap();
        draw(&db, &board).await.unwrap();
        clear(&db, board.get_id(), &user("c")).await.unwrap();
        draw(&db, &other).await.unwrap();

        let id = board.get_id().clone();
        crate::db::board::delete(&db, board).await.unwrap();
        assert!(rows(&db, &id).await.is_empty());
        // The cascade is scoped to its own board.
        assert_eq!(rows(&db, other.get_id()).await.len(), 1);
    }

    #[tokio::test]
    async fn an_oversized_payload_is_refused_before_the_write() {
        let db = a_db().await;
        let board = a_board(&db).await;
        let big = "x".repeat(MAX_STROKE_PAYLOAD_LEN + 1);
        assert!(
            append(&db, board.get_id(), &user("p"), &big, 0)
                .await
                .is_err()
        );
        assert!(
            append(&db, board.get_id(), &user("p"), "   ", 0)
                .await
                .is_err()
        );
        assert!(rows(&db, board.get_id()).await.is_empty());
        // Nothing was claimed either — the counters must not have moved.
        assert_eq!(reread(&db, &board).await.get_epoch(), 0);
    }

    /// History spans epochs; a named epoch is that epoch alone.
    #[tokio::test]
    async fn history_spans_every_epoch_unless_one_is_named() {
        let db = a_db().await;
        let board = a_board(&db).await;
        draw(&db, &board).await.unwrap();
        clear(&db, board.get_id(), &user("c")).await.unwrap();
        let board = reread(&db, &board).await;
        draw(&db, &board).await.unwrap();

        let (all, total) = history(&db, board.get_id(), None, false, None, 0)
            .await
            .unwrap();
        assert_eq!((all.len(), total), (3, 3));
        let (first, total) = history(&db, board.get_id(), Some(0), false, None, 0)
            .await
            .unwrap();
        // Epoch 0: its stroke and the marker that closed it.
        assert_eq!((first.len(), total), (2, 2));
        // `marks_only` drops the marker from that same epoch — and its `total`
        // with it, so the envelope's count stays the count of what is served.
        let (marks, total) = history(&db, board.get_id(), Some(0), true, None, 0)
            .await
            .unwrap();
        assert_eq!((marks.len(), total), (1, 1));
        assert!(marks.iter().all(|row| !row.is_clear()));
    }
}
