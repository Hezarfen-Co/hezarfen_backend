//! The grade ladder: the validated position a class section — and, one layer
//! up, its course blueprint — occupies on the school's single axis of years.
//!
//! It replaces the old free-text `ClassGrade`: "9", "10-A" and "anaokulu" were
//! three spellings no query could order, group or blueprint against without a
//! school-wide convention nobody enforced. The ladder is an integer
//! `0..=12` — `0` is anaokulu, `1..=12` the school years — so "every section
//! at one grade" is one equality and the blueprint is keyed by the same value
//! the class carries.
//!
//! The wire format is the **integer** itself: every DTO carries
//! `grade_level: 0..=12`, and the only strings are [`GradeLevel::label`]'s
//! display/AI-scope labels (`"Anaokulu"`, `"1"`..`"12"`), which are rendered,
//! never parsed back.
//!
//! The bounds live in [`crate::constant`] beside every other published limit
//! (`GET /limits` serves them); this module owns the type.

use crate::constant::{MAX_GRADE_LEVEL, MIN_GRADE_LEVEL};
use crate::error::ValidationError;

/// One rung of the grade ladder: `0` = anaokulu, `1..=12` = school years.
///
/// `sqlx(transparent)` over the `i16` is what makes the column a plain
/// `SMALLINT` — the driver checks the type, the `CHECK
/// (grade_level BETWEEN 0 AND 12)` in the DDL is the stored mirror of
/// [`GradeLevel::new`], and nothing out of range can be constructed in Rust.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, sqlx::Type, serde::Serialize, serde::Deserialize,
)]
#[sqlx(transparent)]
#[serde(transparent)]
pub struct GradeLevel(i16);

impl GradeLevel {
    /// The rung `raw` names, or a validation error when it is off the ladder —
    /// the `400` both the class and the blueprint write paths answer with.
    pub fn new(raw: i16) -> Result<Self, ValidationError> {
        if !(MIN_GRADE_LEVEL..=MAX_GRADE_LEVEL).contains(&raw) {
            return Err(ValidationError::Invalid {
                field: "grade_level",
                reason: "must be 0 (anaokulu) through 12",
            });
        }
        Ok(Self(raw))
    }

    /// Kindergarten, the ladder's floor.
    pub fn anaokulu() -> Self {
        Self(MIN_GRADE_LEVEL)
    }

    /// The integer the wire and the `grade_level` column carry.
    pub fn get(self) -> i16 {
        self.0
    }

    /// The display label: `"Anaokulu"` at the floor, the year number
    /// everywhere else. The one spelling the AI scope (`sinif`) and every
    /// human-facing surface see — an index is safe because the tuple field is
    /// private and only [`GradeLevel::new`]/[`GradeLevel::anaokulu`] construct.
    pub fn label(self) -> &'static str {
        const LABELS: [&str; 13] = [
            "Anaokulu", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12",
        ];
        LABELS[self.0 as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ladder_is_bounded() {
        assert!(GradeLevel::new(MIN_GRADE_LEVEL).is_ok());
        assert!(GradeLevel::new(MAX_GRADE_LEVEL).is_ok());
        assert!(GradeLevel::new(-1).is_err());
        assert!(GradeLevel::new(MAX_GRADE_LEVEL + 1).is_err());
    }

    #[test]
    fn anaokulu_is_the_floor_and_labels_are_stable() {
        assert_eq!(GradeLevel::anaokulu().get(), 0);
        assert_eq!(GradeLevel::anaokulu().label(), "Anaokulu");
        assert_eq!(GradeLevel::new(1).unwrap().label(), "1");
        assert_eq!(GradeLevel::new(12).unwrap().label(), "12");
    }
}
