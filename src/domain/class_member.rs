//! A student's place in a class, and the enrollments that place implies.
//!
//! Both writes are one call into [`crate::db::class_pump`]: adding a member
//! and attaching a course to the class are the same pump seen from its two
//! ends. The refusals-to-errors policy and the detach live in
//! [`crate::service::class_member`]; the roster reads in
//! [`crate::db::class_member`] — this file is the row shape and its
//! composite id.

use crate::domain::class_group::ClassGroupId;
use crate::domain::user::UserId;

/// The identity of one (class, user) pair. Not a row column: the table's
/// primary key *is* the pair, and this struct's job is the underscore-joined
/// wire form at the HTTP edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassMemberId {
    pub(crate) class: ClassGroupId,
    pub(crate) user: UserId,
}

impl ClassMemberId {
    /// The one id a (class, user) pair can have.
    pub fn composite(class: &ClassGroupId, user: &UserId) -> Self {
        Self {
            class: class.clone(),
            user: *user,
        }
    }

    /// The underscore-joined wire form (`{class}_{user}`).
    pub fn key(&self) -> String {
        format!("{}_{}", self.class.key(), self.user.key())
    }
}

/// One student in one class. `added_by` is who put them there.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ClassMember {
    pub(crate) class: ClassGroupId,
    /// The column is `app_user` (the reserved-word rename); the Rust name is
    /// the domain's.
    #[sqlx(rename = "app_user")]
    pub(crate) user: UserId,
    pub(crate) added_by: UserId,
}

impl ClassMember {
    /// The row's identity, built back from its primary-key columns.
    pub fn get_id(&self) -> ClassMemberId {
        ClassMemberId::composite(&self.class, &self.user)
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
