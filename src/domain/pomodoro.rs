use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::POMODORO_SESSION_TABLE;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct PomodoroSessionId(RecordId);

impl PomodoroSessionId {
    /// Minted from the process-wide monotonic generator, not `Ulid::generate()`:
    /// the log sorts `started_at DESC, id DESC` and the id breaks the tie between
    /// two sessions started at the same instant. The `open_` key below never ties
    /// with itself (one running session per user), so it needs no ordering.
    pub fn generate() -> Self {
        Self(RecordId::new(
            POMODORO_SESSION_TABLE,
            next_ulid().to_string(),
        ))
    }

    /// The deterministic id of `user`'s *running* session. At most one runs
    /// per user by construction: starting is a single `UPSERT` on this id
    /// (atomic — a restart replaces the row in place), and finishing
    /// atomically takes the row and re-files it under a ULID id. `open_`
    /// cannot collide with a ULID key (ULIDs are bare alphanumerics).
    pub fn open_for(user: &UserId) -> Self {
        Self(RecordId::new(
            POMODORO_SESSION_TABLE,
            format!("open_{}", user.key()),
        ))
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

/// One pomodoro focus session of a student: server-stamped `started_at`, and
/// `finished_at` once closed. The wall clock is read server-side only — a
/// client can never supply its own instants, so the recorded focus time is
/// honest. Breaks are not stored; the frontend owns the work/break rhythm and
/// the backend records only the focus stint.
#[derive(Debug, Clone, SurrealValue)]
pub struct PomodoroSession {
    id: PomodoroSessionId,
    user: UserId,
    started_at: Timestamp,
    finished_at: Option<Timestamp>,
    /// The student's own name for what this stint is for ("math", "TYT
    /// denemesi") — free text given at start, not a subject reference.
    /// `None` on an unnamed stint and on any row from before the field
    /// existed, which reads exactly like an unnamed one.
    label: Option<String>,
    counted: Option<bool>,
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
    /// they started it unnamed, and on rows that predate the field.
    pub fn get_label(&self) -> Option<&str> {
        self.label.as_deref()
    }
}
