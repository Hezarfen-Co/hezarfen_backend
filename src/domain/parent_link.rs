use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::PARENT_LINK_TABLE;
use crate::domain::user::UserId;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ParentLinkId(RecordId);

impl ParentLinkId {
    /// A deterministic id for the (parent, student) pair — same trick as
    /// `EnrollmentId`: the same pair always maps to the same record id, so
    /// linking is a single atomic UPSERT with no find-then-insert race and
    /// one-row-per-pair by construction.
    pub fn composite(parent: &UserId, student: &UserId) -> Self {
        Self(RecordId::new(
            PARENT_LINK_TABLE,
            format!("{}_{}", parent.key(), student.key()),
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

/// A parent account's tie to one student it may observe. Almost entirely a
/// read grant: it gates the parent's access to the student's reports (marks,
/// attendance, pomodoro). The single write it authorizes is the food program —
/// a parent books and cancels a linked child's meals, since paying for lunch
/// is a parent's job and a small child cannot do it themselves.
#[derive(Debug, Clone, SurrealValue)]
pub struct ParentLink {
    pub(crate) id: ParentLinkId,
    pub(crate) parent: UserId,
    pub(crate) student: UserId,
    pub(crate) linked_by: UserId,
}

impl ParentLink {
    pub fn get_id(&self) -> &ParentLinkId {
        &self.id
    }

    pub fn get_parent(&self) -> &UserId {
        &self.parent
    }

    pub fn get_student(&self) -> &UserId {
        &self.student
    }

    pub fn get_linked_by(&self) -> &UserId {
        &self.linked_by
    }

    // Both role-change sweeps (the observed student leaving `student`, the
    // observing parent leaving `parent`) live in
    // [`crate::service::user::set_role`]: they commit with the role write
    // itself, so there is no window where a link outlives the role that
    // justified it.
}
