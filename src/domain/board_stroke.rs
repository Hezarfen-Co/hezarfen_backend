//! One board's marks, appended and never deleted.
//!
//! A clear does NOT remove strokes — it bumps the board's `epoch`, so the live
//! canvas empties while every mark the board ever carried stays on the table
//! and stays replayable. That is the whole storage model: the only legitimate
//! delete of a stroke row in the feature is
//! [`crate::db::board::delete`]'s cascade, which runs inside the same
//! transaction as the board's own delete.
//!
//! The `clear` marker row IS the epoch index: it carries the epoch it closed,
//! that epoch's final stroke count, who cleared and when — so replaying a
//! whole session needs no second table and no `GROUP BY` over the history.
//!
//! two stroke counters. Nothing else may write either, because the invariant
//! the schema cannot state depends on it: `count` is populated *only* on a
//! `clear` row, and the two counters must move together with the row they
//! count (the stroke claim's dual-counter CTE is what makes that one
//! transaction).

use crate::constant::BOARD_STROKE_KINDS;
use crate::domain::board::Board;
use crate::domain::monotonic_id::next_uuid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;

/// A drawn mark, and the marker that ends an epoch.
pub(crate) const KIND_STROKE: &str = BOARD_STROKE_KINDS[0];
pub(crate) const KIND_CLEAR: &str = BOARD_STROKE_KINDS[1];

/// The refusal the clear transaction refuses with: a blank canvas, not the
/// creator, or the board is locked or already closed. A decision, so it
/// outranks a retryable failure.
pub(crate) const CLEAR_REFUSED: &str = "board_clear_refused";

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

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct BoardStrokeId(uuid::Uuid);

impl BoardStrokeId {
    /// Monotonic, not a plain random UUID: this id *is* the board's total
    /// order, and a random low half scrambles every stroke drawn in the same
    /// millisecond — which is what a burst of drawing looks like.
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    /// Parses a wire key. A key that is not a UUID parses as the nil UUID,
    /// which matches no row.
    pub fn from_key(key: &str) -> Self {
        Self(uuid::Uuid::parse_str(key).unwrap_or(uuid::Uuid::nil()))
    }

    pub fn key(&self) -> String {
        self.0.to_string()
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BoardStroke {
    pub(crate) id: BoardStrokeId,
    pub(crate) board: crate::domain::board::BoardId,
    pub(crate) author: UserId,
    pub(crate) kind: String,
    pub(crate) payload: Option<String>,
    pub(crate) count: Option<i64>,
    pub(crate) epoch: i64,
    pub(crate) created_at: Timestamp,
}

impl BoardStroke {
    pub fn get_id(&self) -> &BoardStrokeId {
        &self.id
    }

    pub fn get_board(&self) -> &crate::domain::board::BoardId {
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
pub(crate) fn state_refusal(board: &Board) -> Option<&'static str> {
    if board.get_closed_at().is_some() {
        Some(BOARD_CLOSED)
    } else if board.is_locked() {
        Some(BOARD_LOCKED)
    } else {
        None
    }
}
