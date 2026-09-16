//! A student's stint in a class, and the enrollments that place implies.
//!
//! A history table: `left_at` NULL is the live stint, and the partial unique
//! index `(class, app_user) WHERE left_at IS NULL` keeps one live stint per
//! pair while preserving rejoin history (a student who leaves and comes back
//! has two rows). Both writes are one call into [`crate::db::class_pump`]:
//! adding a member and attaching a course to the class are the same pump seen
//! from its two ends. The refusals-to-errors policy and the detach live in
//! [`crate::service::class_member`]; the roster reads in
//! [`crate::db::class_member`] — this file is the row shape and its id.

use crate::domain::class_group::ClassGroupId;
use crate::domain::monotonic_id::next_uuid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;

/// The identity of one class-member stunt. A UUIDv7 minted by the
/// process-wide monotonic generator, so a class's roster lists in join order.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct ClassMemberId(uuid::Uuid);

impl ClassMemberId {
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    /// The inner uuid, for runtime-checked binds (Param/QueryBuilder) that
    /// cannot take the newtype. Static `query!` binds take `self` directly.
    pub fn uuid(&self) -> uuid::Uuid {
        self.0
    }

    /// Parses a wire key. A key that is not a UUID parses as the nil UUID,
    /// which matches no row.
    pub fn from_key(key: &str) -> Self {
        Self(uuid::Uuid::parse_str(key).unwrap_or(uuid::Uuid::nil()))
    }

    /// The bare uuid wire form.
    pub fn key(&self) -> String {
        self.0.to_string()
    }
}

/// One student's stint in one class. `added_by` is who put them there;
/// `source_class_group` names the şube a rollover copied them from (absent for
/// a hand-added member).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ClassMember {
    pub(crate) id: ClassMemberId,
    pub(crate) class: ClassGroupId,
    /// The column is `app_user` (the reserved-word rename); the Rust name is
    /// the domain's.
    #[sqlx(rename = "app_user")]
    pub(crate) user: UserId,
    pub(crate) added_by: UserId,
    pub(crate) joined_at: Timestamp,
    /// When the student left; `None` while the stint is live.
    pub(crate) left_at: Option<Timestamp>,
    pub(crate) source_class_group: Option<ClassGroupId>,
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

    pub fn get_joined_at(&self) -> Timestamp {
        self.joined_at
    }

    pub fn get_left_at(&self) -> Option<Timestamp> {
        self.left_at
    }

    /// Whether this stint is live (the student is currently in the class).
    pub fn is_live(&self) -> bool {
        self.left_at.is_none()
    }

    pub fn get_source_class_group(&self) -> Option<&ClassGroupId> {
        self.source_class_group.as_ref()
    }
}
