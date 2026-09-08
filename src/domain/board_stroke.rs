//! One board's marks, appended and never deleted.
//!
//! A clear does NOT remove strokes — it bumps the board's `epoch`, so the live
//! canvas empties while every mark the board ever carried stays on the table
//! and stays replayable. That is the whole storage model: the only legitimate
//! delete of a stroke row in the feature is [`crate::domain::board::Board::delete`]'s
//! cascade, which runs inside the same transaction as the board's own delete.
//!
//! The `clear` marker row IS the epoch index: it carries the epoch it closed,
//! that epoch's final stroke count, who cleared and when — so replaying a whole
//! session needs no second table and no `GROUP BY` over the history.
//!
//! This module is the sole write funnel for `board_stroke`, and for the board's
//! two stroke counters. Nothing else may write either, because two invariants
//! the schema cannot state depend on it: `count` is populated *only* on a
//! `clear` row (SCHEMAFULL types both kinds the same), and the two counters
//! must move together with the row they count (`cap::claim_two_when_and_create`
//! is what makes that one transaction).

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue, Value};

use crate::constant::{
    BOARD_EPOCH_STROKE_COUNT_FIELD, BOARD_OPEN_GUARD, BOARD_REPLAY_CHUNK, BOARD_STROKE_KINDS,
    BOARD_STROKE_TABLE, BOARD_TOTAL_STROKE_COUNT_FIELD, MAX_BOARD_STROKES, MAX_EPOCH_STROKES,
    MAX_STROKE_PAYLOAD_LEN,
};
use crate::database::{Database, transaction_with_retry};
use crate::domain::board::{Board, BoardId};
use crate::domain::cap;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::page::PagedList;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;
use crate::validate::validate_required;

/// A drawn mark, and the marker that ends an epoch.
const KIND_STROKE: &str = BOARD_STROKE_KINDS[0];
const KIND_CLEAR: &str = BOARD_STROKE_KINDS[1];

/// The `THROW` the clear transaction refuses with: a blank canvas, not the
/// creator, or the board is locked or already closed. A decision, so it
/// outranks a lost round.
const CLEAR_REFUSED: &str = "board_clear_refused";

/// The public words of every refusal this module raises. `pub` because
/// `src/web/board_ws.rs` turns each into the machine-readable `code` its room
/// sends, and it matches these constants by value — a classifier that guessed
/// from a substring silently re-labelled a blank canvas as the *terminal*
/// `board_closed` the moment this wording changed.
pub const EPOCH_FULL: &str = "this board is full — clear it to keep drawing";
pub const BOARD_LOCKED: &str = "this board is locked — the creator has paused drawing";
pub const BOARD_CLOSED: &str = "this board is closed — it is permanently read-only";
pub const BOARD_MOVED: &str = "this board changed while you were drawing — draw it again";
pub const CANVAS_BLANK: &str = "there is nothing on this canvas to clear";
pub const NOT_THE_CREATOR: &str = "only the board's creator can clear the board";

/// Every *conflict* refusal above — `NOT_THE_CREATOR` is a `Forbidden`, coded
/// by its variant alone. Listed so the room's classifier can be *proved* to
/// cover them all (src/web/board_ws.rs) rather than trusted to.
pub const REFUSALS: [&str; 5] = [
    EPOCH_FULL,
    BOARD_LOCKED,
    BOARD_CLOSED,
    BOARD_MOVED,
    CANVAS_BLANK,
];

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct BoardStrokeId(RecordId);

impl BoardStrokeId {
    /// Monotonic, not `Ulid::new()`: this id *is* the board's total order, and
    /// a random low half scrambles every stroke drawn in the same millisecond —
    /// which is what a burst of drawing looks like.
    pub fn generate() -> Self {
        Self(RecordId::new(BOARD_STROKE_TABLE, next_ulid().to_string()))
    }

    pub fn record(&self) -> RecordId {
        self.0.clone()
    }

    pub fn key(&self) -> &str {
        match &self.0.key {
            RecordIdKey::String(key) => key,
            _ => "",
        }
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct BoardStroke {
    id: BoardStrokeId,
    board: BoardId,
    author: UserId,
    kind: String,
    payload: Option<String>,
    count: Option<i64>,
    epoch: i64,
    created_at: Timestamp,
}

impl BoardStroke {
    pub fn get_id(&self) -> &BoardStrokeId {
        &self.id
    }

    pub fn get_board(&self) -> &BoardId {
        &self.board
    }

    pub fn get_author(&self) -> &UserId {
        &self.author
    }

    pub fn get_kind(&self) -> &str {
        &self.kind
    }

    pub fn get_payload(&self) -> Option<&str> {
        self.payload.as_deref()
    }

    /// The epoch's final stroke count — present only on a `clear` marker.
    pub fn get_count(&self) -> Option<i64> {
        self.count
    }

    pub fn get_epoch(&self) -> i64 {
        self.epoch
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }

    pub fn is_clear(&self) -> bool {
        self.kind == KIND_CLEAR
    }

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
        board: &BoardId,
        author: &UserId,
        payload: &str,
        epoch: i64,
        db: &Database,
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
            cap::ClaimedTwo::FullHard => Err(Self::why_refused(board, db).await?),
        }
    }

    /// The one place the module orders `closed` above `locked`, because a board
    /// can hold both flags at once (nothing gates `/close` on the lock or the
    /// lock on `closed_at`) and **closed outranks locked**: there is no reopen
    /// route, so the pause never lifts and only the terminal answer is true.
    /// Both classifiers read the precedence from here rather than keeping a
    /// copy of it — the copies are what drifted, and a room told "the creator
    /// has paused drawing" about a permanently read-only board waits forever.
    ///
    /// `None` means the board is neither: whatever refused the write, this is
    /// not why.
    fn state_refusal(board: &Board) -> Option<&'static str> {
        if board.get_closed_at().is_some() {
            Some(BOARD_CLOSED)
        } else if board.is_locked() {
            Some(BOARD_LOCKED)
        } else {
            None
        }
    }

    async fn why_refused(board: &BoardId, db: &Database) -> Result<AppError, AppError> {
        let board = Board::read(board, db).await?.ok_or(AppError::NotFound)?;
        if let Some(refusal) = Self::state_refusal(&board) {
            return Ok(AppError::Conflict(refusal));
        }
        // Neither flag, so the lifetime counter has to say so *itself*:
        // `FullHard` also fires when the guard failed, so a board that was
        // locked (or a clear that moved the epoch) when the claim ran and is
        // open again now would otherwise be stamped read-only at one stroke of
        // a 50 000 budget — irreversibly, with no reopen.
        if Self::total_strokes(board.get_id(), db).await? < MAX_BOARD_STROKES {
            return Ok(AppError::Conflict(BOARD_MOVED));
        }
        // `Board::close` is the one-way idempotent stamp: a second append takes
        // this branch too and leaves the first `closed_at` standing.
        board.close(db).await?;
        Ok(AppError::Conflict(BOARD_CLOSED))
    }

    /// The lifetime counter as the store holds it — the board struct
    /// deliberately does not carry it (src/domain/board.rs:9-13).
    async fn total_strokes(board: &BoardId, db: &Database) -> Result<i64, AppError> {
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
    pub async fn clear(
        board: &BoardId,
        by: &UserId,
        db: &Database,
    ) -> Result<BoardStroke, AppError> {
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
            return Err(Self::why_clear_refused(board, by, db).await?);
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
    /// like [`Self::why_refused`]. The distinction is not cosmetic: a closed
    /// board is terminal for the room, a blank canvas is an ordinary "nothing
    /// to do" and a room told otherwise stops drawing for good.
    async fn why_clear_refused(
        board: &BoardId,
        by: &UserId,
        db: &Database,
    ) -> Result<AppError, AppError> {
        let board = Board::read(board, db).await?.ok_or(AppError::NotFound)?;
        let state = Self::state_refusal(&board);
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
        board: &BoardId,
        epoch: i64,
        after: Option<&str>,
        db: &Database,
    ) -> Result<Vec<BoardStroke>, AppError> {
        let cursor = match after {
            Some(_) => " AND id > $after",
            None => "",
        };
        let mut result = db
            .query(format!(
                "SELECT * FROM {BOARD_STROKE_TABLE} \
                 WHERE board = $b AND epoch = $e AND kind = $kind{cursor} \
                 ORDER BY id LIMIT $chunk"
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
    /// to `kind = 'stroke'` ([`Self::replay_current`]), so without this the two
    /// views disagree in exactly that window.
    pub async fn history(
        board: &BoardId,
        epoch: Option<i64>,
        marks_only: bool,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
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
            format!("{BOARD_STROKE_TABLE} WHERE board = $b{scope}{kinds}"),
            "ORDER BY id",
        )
        .bind("b", board.record())
        .bind("e", epoch.unwrap_or_default())
        .bind("clear", KIND_CLEAR.to_string())
        .run(limit, offset, db)
        .await
    }

    /// The epoch index: every `clear` marker this board has, oldest first.
    pub async fn epochs(board: &BoardId, db: &Database) -> Result<Vec<BoardStroke>, AppError> {
        let mut result = db
            .query(format!(
                "SELECT * FROM {BOARD_STROKE_TABLE} \
                 WHERE board = $b AND kind = $kind ORDER BY id"
            ))
            .bind(("b", board.record()))
            .bind(("kind", KIND_CLEAR))
            .await?
            .check()?;
        Ok(result.take::<Vec<BoardStroke>>(0)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::board::BoardTitle;

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
        Board::create(
            &user("c"),
            BoardTitle::try_new("Geometri").unwrap(),
            vec![user("p")],
            db,
        )
        .await
        .unwrap()
    }

    /// Every stroke row of a board, read straight out of the store — never off
    /// a return value, which the in-memory engine forges wins on
    /// (src/domain/cap.rs:44-49).
    async fn rows(board: &BoardId, db: &Database) -> Vec<BoardStroke> {
        let mut result = db
            .query("SELECT * FROM board_stroke WHERE board = $b ORDER BY id")
            .bind(("b", board.record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        result.take(0).unwrap()
    }

    async fn reread(board: &Board, db: &Database) -> Board {
        Board::read(board.get_id(), db).await.unwrap().unwrap()
    }

    async fn set_counters(board: &Board, epoch: i64, total: i64, db: &Database) {
        db.query("UPDATE $id SET epoch_stroke_count = $e, total_stroke_count = $t")
            .bind(("id", board.get_id().record()))
            .bind(("e", epoch))
            .bind(("t", total))
            .await
            .unwrap()
            .check()
            .unwrap();
    }

    async fn draw(board: &Board, db: &Database) -> Result<BoardStroke, AppError> {
        BoardStroke::append(
            board.get_id(),
            &user("p"),
            "{\"p\":[1,2]}",
            board.get_epoch(),
            db,
        )
        .await
    }

    #[tokio::test]
    async fn a_full_canvas_is_refused_but_the_board_stays_open() {
        let db = a_db().await;
        let board = a_board(&db).await;
        set_counters(&board, MAX_EPOCH_STROKES, 10, &db).await;
        let refused = draw(&board, &db).await;
        assert!(matches!(refused, Err(AppError::Conflict(msg)) if msg.contains("clear it")));
        // Nothing written, and above all: NOT closed — a full canvas is
        // recoverable, and closing it here would make it permanent.
        assert!(rows(board.get_id(), &db).await.is_empty());
        assert!(reread(&board, &db).await.get_closed_at().is_none());
    }

    /// The user's decision in one test: a clear empties the canvas and loses
    /// nothing. If this ever passes with the pre-clear rows gone, the feature
    /// is wrong.
    #[tokio::test]
    async fn a_clear_keeps_every_pre_clear_stroke() {
        let db = a_db().await;
        let board = a_board(&db).await;
        for _ in 0..3 {
            draw(&board, &db).await.unwrap();
        }
        let before: Vec<String> = rows(board.get_id(), &db)
            .await
            .iter()
            .map(|row| row.get_id().key().to_string())
            .collect();

        BoardStroke::clear(board.get_id(), &user("c"), &db)
            .await
            .unwrap();
        let board = reread(&board, &db).await;
        assert_eq!(board.get_epoch(), 1);
        draw(&board, &db).await.unwrap();

        // The three original rows are still in the table, by direct read.
        let after: Vec<String> = rows(board.get_id(), &db)
            .await
            .iter()
            .map(|row| row.get_id().key().to_string())
            .collect();
        for key in &before {
            assert!(after.contains(key), "a clear deleted stroke {key}");
        }
        // 3 old + 1 marker + 1 new, and the live canvas is the new one alone.
        assert_eq!(after.len(), 5);
        let live = BoardStroke::replay_current(board.get_id(), 1, None, &db)
            .await
            .unwrap();
        assert_eq!(live.len(), 1);
        // The old epoch is still replayable in full.
        assert_eq!(
            BoardStroke::replay_current(board.get_id(), 0, None, &db)
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
            draw(&board, &db).await.unwrap();
        }
        let marker = BoardStroke::clear(board.get_id(), &user("c"), &db)
            .await
            .unwrap();
        assert_eq!(marker.get_count(), Some(4));
        assert_eq!(marker.get_epoch(), 0);
        assert!(marker.is_clear());

        // And the epoch counter reset, so drawing resumes against a fresh cap.
        let board = reread(&board, &db).await;
        draw(&board, &db).await.unwrap();
        let epochs = BoardStroke::epochs(board.get_id(), &db).await.unwrap();
        assert_eq!(epochs.len(), 1);
        assert_eq!(epochs[0].get_count(), Some(4));
        // `count` lives on clear markers only — this module is that guard.
        for row in rows(board.get_id(), &db).await {
            assert_eq!(row.get_count().is_some(), row.is_clear());
            assert_eq!(row.get_payload().is_some(), !row.is_clear());
        }
    }

    #[tokio::test]
    async fn the_lifetime_cap_closes_the_board_exactly_once() {
        let db = a_db().await;
        let board = a_board(&db).await;
        set_counters(&board, 0, MAX_BOARD_STROKES, &db).await;
        let refused = draw(&board, &db).await;
        assert!(matches!(refused, Err(AppError::Conflict(msg)) if msg.contains("read-only")));
        let stamp = reread(&board, &db).await.get_closed_at();
        assert!(stamp.is_some());

        // A whole millisecond apart, so a re-stamp could not go unnoticed.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        assert!(draw(&board, &db).await.is_err());
        assert_eq!(reread(&board, &db).await.get_closed_at(), stamp);
        assert!(rows(board.get_id(), &db).await.is_empty());
    }

    /// A locked board must be told it is locked. Both refusals arrive as the
    /// same `FullHard`, so a re-read that got this wrong would tell a paused
    /// room its canvas is full — and send it clearing a board it need not clear.
    #[tokio::test]
    async fn a_locked_board_is_refused_as_locked() {
        let db = a_db().await;
        let board = a_board(&db).await;
        let board = board.set_locked(true, &user("c"), &db).await.unwrap();
        let refused = draw(&board, &db).await;
        assert!(matches!(refused, Err(AppError::Conflict(msg)) if msg.contains("locked")));
        assert!(rows(board.get_id(), &db).await.is_empty());
        // A lock is not a close: unlocking restores drawing.
        assert!(reread(&board, &db).await.get_closed_at().is_none());
        let board = board.set_locked(false, &user("c"), &db).await.unwrap();
        assert!(draw(&board, &db).await.is_ok());
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
        let board = board.set_locked(true, &user("c"), &db).await.unwrap();
        // The claim loses the open-guard while the board is locked...
        assert!(matches!(
            draw(&board, &db).await,
            Err(AppError::Conflict(_))
        ));
        // ...and the creator unlocks before the refusal picks its message.
        let board = board.set_locked(false, &user("c"), &db).await.unwrap();
        let refused = BoardStroke::why_refused(board.get_id(), &db).await.unwrap();
        assert!(matches!(refused, AppError::Conflict(msg) if !msg.contains("read-only")));
        // Stored state, not the return value: the board is still open, and
        // still drawable.
        assert!(reread(&board, &db).await.get_closed_at().is_none());
        assert!(draw(&reread(&board, &db).await, &db).await.is_ok());
    }

    /// The epoch index must never drift, in either direction. A stroke carrying
    /// a pre-clear epoch used to land under the closed epoch while its
    /// increment counted against the new one — the closed marker short by a
    /// stroke, the next marker claiming one that is not there, both permanent.
    #[tokio::test]
    async fn a_stale_epoch_append_leaves_no_marker_drifted() {
        let db = a_db().await;
        let board = a_board(&db).await;
        draw(&board, &db).await.unwrap();
        BoardStroke::clear(board.get_id(), &user("c"), &db)
            .await
            .unwrap();

        // Exactly what `live_board` hands `append` when a clear lands between
        // the board read and the write (src/web/board_ws.rs:431).
        let stale = BoardStroke::append(board.get_id(), &user("p"), "{\"p\":[9]}", 0, &db).await;
        assert!(matches!(stale, Err(AppError::Conflict(_))));

        let board = reread(&board, &db).await;
        draw(&board, &db).await.unwrap();
        BoardStroke::clear(board.get_id(), &user("c"), &db)
            .await
            .unwrap();

        // Every marker's count is the rows the store actually holds at the
        // epoch it closed.
        let stored = rows(board.get_id(), &db).await;
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
        assert_eq!(
            BoardStroke::total_strokes(board.get_id(), &db)
                .await
                .unwrap(),
            4
        );
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
        draw(&board, &db).await.unwrap();
        let board = board.set_locked(true, &user("c"), &db).await.unwrap();

        let refused = BoardStroke::clear(board.get_id(), &user("c"), &db).await;
        assert!(matches!(refused, Err(AppError::Conflict(msg)) if msg == BOARD_LOCKED));
        assert_eq!(rows(board.get_id(), &db).await.len(), 1);
        assert_eq!(reread(&board, &db).await.get_epoch(), 0);
        assert!(
            BoardStroke::epochs(board.get_id(), &db)
                .await
                .unwrap()
                .is_empty()
        );

        // Unlock, and the same clear lands: unlock → clear → relock is the
        // recovery for a locked board sitting at the epoch cap.
        let board = board.set_locked(false, &user("c"), &db).await.unwrap();
        let marker = BoardStroke::clear(board.get_id(), &user("c"), &db)
            .await
            .unwrap();
        assert_eq!(marker.get_count(), Some(1));
        assert_eq!(reread(&board, &db).await.get_epoch(), 1);
    }

    /// Both classifiers, one board: the words a stroke is refused with and the
    /// words the *creator's* clear is refused with. They must match — the two
    /// used to be independent chains and drifted apart on the state below.
    async fn both_refusals(board: &Board, db: &Database) -> (&'static str, &'static str) {
        let stroke = match draw(board, db).await {
            Err(AppError::Conflict(msg)) => msg,
            other => panic!("the stroke was not refused: {other:?}"),
        };
        let clear = match BoardStroke::clear(board.get_id(), &user("c"), db).await {
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
            draw(&board, &db).await.unwrap();

            let board = if closed_first {
                let board = board.close(&db).await.unwrap();
                board.set_locked(true, &user("c"), &db).await.unwrap()
            } else {
                let board = board.set_locked(true, &user("c"), &db).await.unwrap();
                board.close(&db).await.unwrap()
            };
            // Stored state: both flags really are up, and the close stamp stood.
            let stored = reread(&board, &db).await;
            assert!(stored.is_locked() && stored.get_closed_at().is_some());

            assert_eq!(
                both_refusals(&board, &db).await,
                (BOARD_CLOSED, BOARD_CLOSED),
                "closed_first = {closed_first}"
            );
            // And the refusals wrote nothing: one mark, no marker, epoch 0.
            assert_eq!(rows(board.get_id(), &db).await.len(), 1);
            assert_eq!(reread(&board, &db).await.get_epoch(), 0);
        }
    }

    /// The other half of the same contract: a board holding *one* of the flags
    /// keeps its own answer on both paths. A lock is recoverable, a close is
    /// not, and collapsing either into the other is the mislabel above.
    #[tokio::test]
    async fn one_flag_alone_keeps_its_own_answer_on_both_paths() {
        let db = a_db().await;
        let locked = a_board(&db).await;
        draw(&locked, &db).await.unwrap();
        let locked = locked.set_locked(true, &user("c"), &db).await.unwrap();
        assert_eq!(
            both_refusals(&locked, &db).await,
            (BOARD_LOCKED, BOARD_LOCKED)
        );

        let closed = a_board(&db).await;
        draw(&closed, &db).await.unwrap();
        let closed = closed.close(&db).await.unwrap();
        assert_eq!(
            both_refusals(&closed, &db).await,
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
            minted.push(draw(&board, &db).await.unwrap().get_id().key().to_string());
        }
        let replayed: Vec<String> = BoardStroke::replay_current(board.get_id(), 0, None, &db)
            .await
            .unwrap()
            .iter()
            .map(|row| row.get_id().key().to_string())
            .collect();
        assert_eq!(replayed, minted);

        // The cursor resumes exactly where the last chunk stopped.
        let rest: Vec<String> =
            BoardStroke::replay_current(board.get_id(), 0, Some(&minted[9]), &db)
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
        draw(&board, &db).await.unwrap();
        draw(&board, &db).await.unwrap();
        BoardStroke::clear(board.get_id(), &user("c"), &db)
            .await
            .unwrap();
        draw(&other, &db).await.unwrap();

        let id = board.get_id().clone();
        board.delete(&db).await.unwrap();
        assert!(rows(&id, &db).await.is_empty());
        // The cascade is scoped to its own board.
        assert_eq!(rows(other.get_id(), &db).await.len(), 1);
    }

    #[tokio::test]
    async fn an_oversized_payload_is_refused_before_the_write() {
        let db = a_db().await;
        let board = a_board(&db).await;
        let big = "x".repeat(MAX_STROKE_PAYLOAD_LEN + 1);
        assert!(
            BoardStroke::append(board.get_id(), &user("p"), &big, 0, &db)
                .await
                .is_err()
        );
        assert!(
            BoardStroke::append(board.get_id(), &user("p"), "   ", 0, &db)
                .await
                .is_err()
        );
        assert!(rows(board.get_id(), &db).await.is_empty());
        // Nothing was claimed either — the counters must not have moved.
        assert_eq!(reread(&board, &db).await.get_epoch(), 0);
    }

    /// History spans epochs; a named epoch is that epoch alone.
    #[tokio::test]
    async fn history_spans_every_epoch_unless_one_is_named() {
        let db = a_db().await;
        let board = a_board(&db).await;
        draw(&board, &db).await.unwrap();
        BoardStroke::clear(board.get_id(), &user("c"), &db)
            .await
            .unwrap();
        let board = reread(&board, &db).await;
        draw(&board, &db).await.unwrap();

        let (all, total) = BoardStroke::history(board.get_id(), None, false, None, 0, &db)
            .await
            .unwrap();
        assert_eq!((all.len(), total), (3, 3));
        let (first, total) = BoardStroke::history(board.get_id(), Some(0), false, None, 0, &db)
            .await
            .unwrap();
        // Epoch 0: its stroke and the marker that closed it.
        assert_eq!((first.len(), total), (2, 2));
        // `marks_only` drops the marker from that same epoch — and its `total`
        // with it, so the envelope's count stays the count of what is served.
        let (marks, total) = BoardStroke::history(board.get_id(), Some(0), true, None, 0, &db)
            .await
            .unwrap();
        assert_eq!((marks.len(), total), (1, 1));
        assert!(marks.iter().all(|row| !row.is_clear()));
    }
}
