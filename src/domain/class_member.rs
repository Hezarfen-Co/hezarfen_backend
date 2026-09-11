//! A student's place in a class, and the enrollments that place implies.
//!
//! Both writes are one call into [`crate::db::class_pump`]: adding a member
//! and attaching a course to the class are the same pump seen from its two
//! ends. The refusals-to-errors policy and the detach live in
//! [`crate::service::class_member`]; the roster reads in
//! [`crate::db::class_member`] — this file is the row shape and its
//! composite id.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::CLASS_MEMBER_TABLE;
use crate::db::class_pump::link_id;
use crate::domain::class_group::ClassGroupId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ClassMemberId(RecordId);

impl ClassMemberId {
    /// The record one (class, user) pair always maps to.
    pub fn composite(class: &ClassGroupId, user: &UserId) -> Self {
        Self(link_id(CLASS_MEMBER_TABLE, class, user.key()))
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

/// One student in one class. `added_by` is who put them there.
#[derive(Debug, Clone, SurrealValue)]
pub struct ClassMember {
    pub(crate) id: ClassMemberId,
    pub(crate) class: ClassGroupId,
    pub(crate) user: UserId,
    pub(crate) added_by: UserId,
    /// When they were added, and the *only* thing "newest first" can mean here:
    /// the row's id is the (class, user) pair, so ordering by it sorts the
    /// roster by the student's account ULID. Optional because rows written
    /// before this column carry no stamp — see the migration note.
    pub(crate) added_at: Option<Timestamp>,
}

impl ClassMember {
    pub fn get_id(&self) -> &ClassMemberId {
        &self.id
    }

    pub fn get_class(&self) -> &ClassGroupId {
        &self.class
    }

    pub fn get_user(&self) -> &UserId {
        &self.user
    }

    pub fn get_added_by(&self) -> &UserId {
        &self.added_by
    }
}
