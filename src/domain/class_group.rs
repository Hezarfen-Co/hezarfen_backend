//! A class section (şube): a named set of students the school manages as one,
//! so a course attach enrolls the whole set at once. The membership and the
//! attachments live in their own tables (`class_member`, `class_course`) and are
//! counted on this row — a class may only be deleted at zero on both, the same
//! stored guard shape courses and terms use.
//!
//! A class links a term exactly like a course does, and claims a reference on it
//! before the link is written. The reference is its *own* column
//! ([`crate::constant::TERM_CLASS_COUNT_FIELD`]) rather than the courses' `course_count`, because
//! boot seeds that one from the course rows alone and would wipe a class's share
//! of it on the next migration.
//!
//! The queries live in [`crate::db::class_group`]; the archived-term guard and
//! the route-facing wrappers in [`crate::service::class_group`] — this file is
//! the row shape, its newtypes and getters.

use crate::constant::{MAX_CLASS_GRADE_LEN, MAX_CLASS_NAME_LEN};
use crate::domain::monotonic_id::next_uuid;
use crate::domain::term::TermId;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::{validate_optional, validate_required};

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct ClassGroupId(uuid::Uuid);

impl ClassGroupId {
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

    pub fn key(&self) -> String {
        self.0.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct ClassName(String);

impl ClassName {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("name", value, MAX_CLASS_NAME_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The school's own label for the year a class sits in ("9", "10-A",
/// "anaokulu"). Free text on purpose — no school's grade ladder is the next
/// one's — and optional: a club-shaped class has no grade at all.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct ClassGrade(String);

impl ClassGrade {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_optional("grade", value, MAX_CLASS_GRADE_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One class. `creator` is who made it; the two refcounts behind the delete
/// guard are database-side columns only, so no whole-row save can clobber one
/// (see [`crate::db::cap`]).
///
/// `teacher` is the section's homeroom teacher (sınıf öğretmeni): optional, not
/// refcounted, and merely a label pointing at a teacher-or-higher account — the
/// web layer holds that bar, and a demotion sweeps the column
/// ([`crate::db::class_group::unassign_everywhere`]).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ClassGroup {
    pub(crate) id: ClassGroupId,
    pub(crate) creator: UserId,
    pub(crate) name: ClassName,
    pub(crate) grade: Option<ClassGrade>,
    pub(crate) term: Option<TermId>,
    pub(crate) teacher: Option<UserId>,
}

impl ClassGroup {
    pub fn get_id(&self) -> &ClassGroupId {
        &self.id
    }

    pub fn get_creator(&self) -> &UserId {
        &self.creator
    }

    pub fn get_name(&self) -> &ClassName {
        &self.name
    }

    pub fn get_grade(&self) -> Option<&ClassGrade> {
        self.grade.as_ref()
    }

    pub fn get_term(&self) -> Option<&TermId> {
        self.term.as_ref()
    }

    /// The homeroom teacher (sınıf öğretmeni), if one is assigned.
    pub fn get_teacher(&self) -> Option<&UserId> {
        self.teacher.as_ref()
    }

    pub fn is_creator(&self, user: &UserId) -> bool {
        &self.creator == user
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_required_and_grade_is_optional() {
        assert!(ClassName::try_new("9-A").is_ok());
        assert!(ClassName::try_new("").is_err());
        assert!(ClassName::try_new("   ").is_err());
        assert!(ClassName::try_new(&"x".repeat(MAX_CLASS_NAME_LEN + 1)).is_err());
        assert!(ClassGrade::try_new("").is_ok());
        assert!(ClassGrade::try_new("anaokulu").is_ok());
        assert!(ClassGrade::try_new(&"x".repeat(MAX_CLASS_GRADE_LEN + 1)).is_err());
    }
}
