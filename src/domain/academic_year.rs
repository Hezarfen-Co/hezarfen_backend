//! An academic year (eğitim yılı): the container a şube and its dönemler
//! belong to. Above [`crate::domain::term::Term`], which is a grading slice
//! inside it.
//!
//! The year carries the sınıf geçme policy (`grade_promotions`): a mapping
//! from one grade label to the next, applied by the explicit, idempotent
//! rollover command ([`crate::service::academic_year::rollover`]). A grade
//! absent from the list is not rolled over — that is how graduation is
//! expressed.
//!
//! The queries live in [`crate::db::academic_year`]; this file is the row
//! shape, its newtypes and the archive guard.

use serde::{Deserialize, Serialize};
use sqlx::types::Json;

use crate::constant::{MAX_ACADEMIC_YEAR_NAME_LEN, MAX_CLASS_GRADE_LEN};
use crate::domain::monotonic_id::next_uuid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_required;

/// Typed academic-year row id. A UUIDv7 minted by the process-wide monotonic
/// generator, so `id` order is mint order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct AcademicYearId(uuid::Uuid);

impl AcademicYearId {
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

/// The refusal every write against an archived year gets, coded so a client
/// can tell it from the other 409s on the same route.
pub fn archived_error() -> AppError {
    AppError::ConflictCoded {
        code: "academic_year_archived",
        message: "this academic year is archived — past years are read-only".into(),
    }
}

/// The refusal a rollover into an already-populated target year gets.
pub fn rollover_target_not_empty() -> AppError {
    AppError::ConflictCoded {
        code: "rollover_target_not_empty",
        message: "the target year already has classes — roll into an empty year".into(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct AcademicYearName(String);

impl AcademicYearName {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("name", value, MAX_ACADEMIC_YEAR_NAME_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One sınıf-geçme mapping: a student who finished `from_grade` moves to
/// `to_grade` at rollover.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GradePromotion {
    from_grade: String,
    to_grade: String,
}

impl GradePromotion {
    pub fn try_new(from_grade: &str, to_grade: &str) -> Result<Self, ValidationError> {
        let bound = |field: &'static str, value: &str| {
            if value.is_empty() || value.chars().count() > MAX_CLASS_GRADE_LEN {
                Err(ValidationError::Invalid {
                    field,
                    reason: "grade labels must be 1 to 20 characters",
                })
            } else {
                Ok(())
            }
        };
        bound("from_grade", from_grade)?;
        bound("to_grade", to_grade)?;
        Ok(Self {
            from_grade: from_grade.to_string(),
            to_grade: to_grade.to_string(),
        })
    }

    pub fn get_from_grade(&self) -> &str {
        &self.from_grade
    }

    pub fn get_to_grade(&self) -> &str {
        &self.to_grade
    }
}

/// One academic year. Both ends are required; `archived_at` NULL = open.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AcademicYear {
    pub(crate) id: AcademicYearId,
    pub(crate) name: AcademicYearName,
    pub(crate) starts_at: Timestamp,
    pub(crate) ends_at: Timestamp,
    pub(crate) archived_at: Option<Timestamp>,
    pub(crate) creator: UserId,
    pub(crate) grade_promotions: Json<Vec<GradePromotion>>,
    pub(crate) class_count: i64,
    pub(crate) term_count: i64,
}

impl AcademicYear {
    pub fn try_new(
        name: &str,
        starts_at: Timestamp,
        ends_at: Timestamp,
        creator: UserId,
        grade_promotions: Vec<GradePromotion>,
    ) -> Result<Self, ValidationError> {
        if ends_at <= starts_at {
            return Err(ValidationError::Invalid {
                field: "ends_at",
                reason: "must be after starts_at",
            });
        }
        Ok(Self {
            id: AcademicYearId::generate(),
            name: AcademicYearName::try_new(name)?,
            starts_at,
            ends_at,
            archived_at: None,
            creator,
            grade_promotions: Json(grade_promotions),
            class_count: 0,
            term_count: 0,
        })
    }

    pub fn get_id(&self) -> &AcademicYearId {
        &self.id
    }

    pub fn get_name(&self) -> &AcademicYearName {
        &self.name
    }

    pub fn get_starts_at(&self) -> Timestamp {
        self.starts_at
    }

    pub fn get_ends_at(&self) -> Timestamp {
        self.ends_at
    }

    pub fn get_archived_at(&self) -> Option<Timestamp> {
        self.archived_at
    }

    pub fn get_creator(&self) -> &UserId {
        &self.creator
    }

    pub fn get_grade_promotions(&self) -> &[GradePromotion] {
        &self.grade_promotions.0
    }

    /// The grade a şube of `grade` rolls into, or `None` when the grade has no
    /// promotion entry (graduation).
    pub fn promotion_for(&self, grade: &str) -> Option<&str> {
        self.grade_promotions
            .0
            .iter()
            .find(|promo| promo.from_grade == grade)
            .map(|promo| promo.to_grade.as_str())
    }

    pub fn get_class_count(&self) -> i64 {
        self.class_count
    }

    pub fn get_term_count(&self) -> i64 {
        self.term_count
    }

    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(ms: i64) -> Timestamp {
        Timestamp::from_millis(ms)
    }

    #[tokio::test]
    async fn name_is_required_and_bounded() {
        assert!(AcademicYearName::try_new("2026-2027").is_ok());
        assert!(AcademicYearName::try_new("").is_err());
        assert!(AcademicYearName::try_new("   ").is_err());
        assert!(AcademicYearName::try_new(&"x".repeat(MAX_ACADEMIC_YEAR_NAME_LEN + 1)).is_err());
    }

    #[tokio::test]
    async fn promotions_validate_both_labels() {
        assert!(GradePromotion::try_new("5", "6").is_ok());
        assert!(GradePromotion::try_new("", "6").is_err());
        assert!(GradePromotion::try_new("5", &"x".repeat(MAX_CLASS_GRADE_LEN + 1)).is_err());
    }

    #[tokio::test]
    async fn a_year_needs_a_forward_range() {
        let creator = UserId::from_key("0198f1a2-3b4c-7d5e-8f90-aaaa2b3c4d5e");
        assert!(AcademicYear::try_new("2026-2027", ts(0), ts(1000), creator, vec![]).is_ok());
        assert!(AcademicYear::try_new("2026-2027", ts(1000), ts(1000), creator, vec![]).is_err());
    }

    #[tokio::test]
    async fn promotion_for_maps_known_grades_only() {
        let creator = UserId::from_key("0198f1a2-3b4c-7d5e-8f90-aaaa2b3c4d5e");
        let year = AcademicYear::try_new(
            "2026-2027",
            ts(0),
            ts(1000),
            creator,
            vec![GradePromotion::try_new("5", "6").unwrap()],
        )
        .unwrap();
        assert_eq!(year.promotion_for("5"), Some("6"));
        assert_eq!(year.promotion_for("12"), None);
    }
}
