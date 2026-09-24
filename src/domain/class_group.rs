//! A class section: a named set of students the school manages as one, so a
//! course attach enrolls the whole set at once. The membership and the
//! attachments live in their own tables (`class_member`, `class_course`) and are
//! counted on this row — a class may only be deleted at zero on both, the same
//! stored guard shape courses and terms use.
//!
//! A class belongs to an academic year ([`crate::domain::academic_year`]), not
//! to a term: the section keeps the same roster all year and the term is a
//! grading slice inside it. It claims a reference on its year before the link
//! is written; the reference is its *own* column
//! ([`crate::constant::ACADEMIC_YEAR_CLASS_COUNT_FIELD`]) rather than the
//! courses' own count, so a class's share is never confused with another
//! roster's.
//!
//! The queries live in [`crate::db::class_group`]; the archived-year guard and
//! the route-facing wrappers in [`crate::service::class_group`] — this file is
//! the row shape, its newtypes and getters.

use crate::constant::MAX_CLASS_NAME_LEN;
use crate::domain::academic_year::AcademicYearId;
use crate::domain::grade::GradeLevel;
use crate::domain::monotonic_id::next_uuid;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::validate_required;

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

/// One class. `creator` is who made it; the two refcounts behind the delete
/// guard are database-side columns only, so no whole-row save can clobber one
/// (see [`crate::db::cap`]).
///
/// `teacher` is the section's homeroom teacher: optional, not
/// refcounted, and merely a label pointing at a teacher-or-higher account — the
/// web layer holds that bar, and a demotion sweeps the column
/// ([`crate::db::class_group::unassign_everywhere`]).
///
/// `grade_level` is the section's rung on the shared grade ladder
/// ([`GradeLevel`]) — required, not a label the school makes up: a club-shaped
/// *class* still sits at a grade, and it is the school-wide club/study
/// *courses* that belong to no section at all.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ClassGroup {
    pub(crate) id: ClassGroupId,
    pub(crate) creator: UserId,
    pub(crate) name: ClassName,
    pub(crate) grade_level: GradeLevel,
    pub(crate) year: Option<AcademicYearId>,
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

    pub fn get_grade_level(&self) -> GradeLevel {
        self.grade_level
    }

    pub fn get_year(&self) -> Option<&AcademicYearId> {
        self.year.as_ref()
    }

    /// The homeroom teacher, if one is assigned.
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
    fn name_is_required_and_the_grade_is_a_ladder_rung() {
        assert!(ClassName::try_new("9-A").is_ok());
        assert!(ClassName::try_new("").is_err());
        assert!(ClassName::try_new("   ").is_err());
        assert!(ClassName::try_new(&"x".repeat(MAX_CLASS_NAME_LEN + 1)).is_err());
        assert!(GradeLevel::new(0).is_ok());
        assert!(GradeLevel::new(12).is_ok());
        assert!(GradeLevel::new(13).is_err());
        assert!(GradeLevel::new(-1).is_err());
    }
}
