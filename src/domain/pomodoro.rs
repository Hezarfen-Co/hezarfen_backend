use uuid::Uuid;

use crate::domain::monotonic_id::next_uuid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;

/// Typed pomodoro-session row id. A UUIDv7 minted by the process-wide
/// monotonic generator, so `id` order is mint order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct PomodoroSessionId(Uuid);

impl PomodoroSessionId {
    /// Minted from the process-wide monotonic generator, not a random v4:
    /// the log sorts `started_at DESC, id DESC` and the id breaks the tie
    /// between two sessions started at the same instant.
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    /// Parse a wire key. A key that parses as no UUID — a malformed path
    /// segment — reads as the nil id, which matches no row: exactly the 404 a
    /// dangling record key produced under the old store, without turning a
    /// typo into a panic.
    pub fn from_key(key: &str) -> Self {
        Self(Uuid::parse_str(key).unwrap_or(Uuid::nil()))
    }

    /// The hyphenated wire form.
    pub fn key(&self) -> String {
        self.0.to_string()
    }
}

/// One pomodoro focus session of a student: server-stamped `started_at`, and
/// `finished_at` once closed. The wall clock is read server-side only — a
/// client can never supply its own instants, so the recorded focus time is
/// honest. Breaks are not stored; the frontend owns the work/break rhythm and
/// the backend records only the focus stint.
///
/// "At most one running session per student" is no longer carried by a
/// deterministic key: it is a partial unique index on the table
/// (`pomodoro_session_open_stint`) over rows whose `finished_at` is NULL, so
/// the database itself refuses (or restarts) a second running stint.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PomodoroSession {
    pub(crate) id: PomodoroSessionId,
    #[sqlx(rename = "app_user")]
    pub(crate) user: UserId,
    pub(crate) started_at: Timestamp,
    pub(crate) finished_at: Option<Timestamp>,
    /// The verdict `finish` reached for this stint, stamped at close so a later
    /// reader never re-derives it against thresholds that have since moved.
    /// `NULL`: a running stint has no verdict yet, and a stint closed
    /// before the rule existed carries none and cannot honestly be given one.
    pub(crate) counted: Option<bool>,
    /// The student's own name for what the stint is for ("math", "TYT
    /// denemesi") — free text given at start, not a subject reference.
    /// `NULL` on an unnamed stint.
    pub(crate) label: Option<String>,
}

impl PomodoroSession {
    pub fn get_id(&self) -> &PomodoroSessionId {
        &self.id
    }

    pub fn get_user(&self) -> &UserId {
        &self.user
    }

    pub fn get_started_at(&self) -> Timestamp {
        self.started_at
    }

    pub fn get_finished_at(&self) -> Option<Timestamp> {
        self.finished_at
    }

    /// Whether this stint moved the lifetime counters, as
    /// [`crate::db::pomodoro::finish`] judged it. `None` while it is still
    /// running — and on any stint closed before the rule existed, which
    /// cannot honestly be given a verdict now.
    pub fn get_counted(&self) -> Option<bool> {
        self.counted
    }

    /// What the student called this stint when they started it. `None` when
    /// they started it unnamed.
    pub fn get_label(&self) -> Option<&str> {
        self.label.as_deref()
    }
}
