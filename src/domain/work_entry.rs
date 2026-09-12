use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::WORK_ENTRY_TABLE;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// The one spelling of "this stint is inverted", shared by the handler's
/// pre-flight check and the write-time `WHERE` guard that re-makes it against
/// the stored row.
pub(crate) fn out_before_in_error() -> AppError {
    AppError::Validation(ValidationError::Invalid {
        field: "check_out",
        reason: "must be at or after check_in",
    })
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct WorkEntryId(RecordId);

impl WorkEntryId {
    /// Minted from the process-wide monotonic generator, not `Ulid::generate()`:
    /// the log sorts `check_in DESC, id DESC` and the id breaks the tie between
    /// two stints checked in at the same instant. The `open_` key below never
    /// ties with itself (one open stint per user), so it needs no ordering.
    pub fn generate() -> Self {
        Self(RecordId::new(WORK_ENTRY_TABLE, next_ulid().to_string()))
    }

    /// The deterministic id of `user`'s *open* entry. At most one open stint
    /// per user holds by construction: checking in is a single `INSERT IGNORE`
    /// on this id (atomic — a second check-in changes nothing), and checking
    /// out atomically takes the row and re-files it under a ULID id.
    /// `open_` cannot collide with a ULID key (ULIDs are bare alphanumerics).
    pub fn open_for(user: &UserId) -> Self {
        Self(RecordId::new(
            WORK_ENTRY_TABLE,
            format!("open_{}", user.key()),
        ))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(WORK_ENTRY_TABLE, key))
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

/// One work stint of a staff member: server-stamped `check_in`, and
/// `check_out` once closed. The wall clock is read server-side only — a
/// client can never supply its own instants (managers correct closed entries
/// through an explicit endpoint instead).
#[derive(Debug, Clone, SurrealValue)]
pub struct WorkEntry {
    pub(crate) id: WorkEntryId,
    pub(crate) user: UserId,
    pub(crate) check_in: Timestamp,
    pub(crate) check_out: Option<Timestamp>,
}

impl WorkEntry {
    pub fn get_id(&self) -> &WorkEntryId {
        &self.id
    }

    pub fn get_user(&self) -> &UserId {
        &self.user
    }

    pub fn get_check_in(&self) -> Timestamp {
        self.check_in
    }

    pub fn get_check_out(&self) -> Option<Timestamp> {
        self.check_out
    }

    pub fn is_open(&self) -> bool {
        self.check_out.is_none()
    }
}
