//! Exam-kind weights an offering or a class section overrides: one row per
//! (owner, kind) naming a kind that exists in `settings.exam_kinds` and the
//! weight it carries *there*.
//!
//! The rows are the override levels of the resolution chain
//! ([`crate::service::exam_weight::resolve`]); this file is the row shape, its
//! bounds and the constant floor. The school's settings keep the *default*
//! weight of every kind — an override reweights one grade or one section, and
//! the settings vocabulary is exactly what the rows may name.

use crate::constant::{MAX_EXAM_KIND_WEIGHT, MIN_EXAM_KIND_WEIGHT};
use crate::error::ValidationError;

/// The weight a kind resolves to when nothing in the chain names it: a retired
/// kind (settings edits never rewrite history) and an own-set-in-force section
/// with no row for the kind both count an exam of it once.
pub const FALLBACK_WEIGHT: i64 = 1;

/// One weight override: `kind` is the settings kind exactly as spelled there
/// (the service layer refuses anything else before a row is written), `weight`
/// the weight it takes in averages computed under the owning level.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ExamWeight {
    pub(crate) kind: String,
    pub(crate) weight: i64,
}

impl ExamWeight {
    /// Bounds check only — `weight` must sit inside the same 1..=100 window
    /// the settings themselves enforce
    /// ([`crate::constant::MIN_EXAM_KIND_WEIGHT`]/[`MAX_EXAM_KIND_WEIGHT`]).
    /// Whether `kind` names a kind the school runs is the service layer's
    /// settings lookup; this constructor cannot see a store.
    pub fn try_new(kind: impl Into<String>, weight: i64) -> Result<Self, ValidationError> {
        if !(MIN_EXAM_KIND_WEIGHT..=MAX_EXAM_KIND_WEIGHT).contains(&weight) {
            return Err(ValidationError::Invalid {
                field: "weight",
                reason: "must be between 1 and 100",
            });
        }
        Ok(Self {
            kind: kind.into(),
            weight,
        })
    }

    pub fn get_kind(&self) -> &str {
        &self.kind
    }

    pub fn get_weight(&self) -> i64 {
        self.weight
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_reuse_the_settings_window() {
        assert!(ExamWeight::try_new("final", MIN_EXAM_KIND_WEIGHT).is_ok());
        assert!(ExamWeight::try_new("final", MAX_EXAM_KIND_WEIGHT).is_ok());
        let zero = ExamWeight::try_new("final", MIN_EXAM_KIND_WEIGHT - 1).unwrap_err();
        let over = ExamWeight::try_new("final", MAX_EXAM_KIND_WEIGHT + 1).unwrap_err();
        for err in [zero, over] {
            assert!(
                matches!(&err, ValidationError::Invalid { field: "weight", .. }),
                "a weight outside 1..=100 is a `weight` validation error: {err:?}"
            );
        }
    }

    #[test]
    fn the_floor_is_one_and_the_kind_rides_verbatim() {
        assert_eq!(FALLBACK_WEIGHT, 1);
        let row = ExamWeight::try_new("Final", 2).unwrap();
        assert_eq!(row.get_kind(), "Final", "kind case is the caller's to match");
        assert_eq!(row.get_weight(), 2);
    }
}
