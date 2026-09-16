//! What one student may not eat: the school's record of their dietary tags
//! plus a free-text note for the kitchen.
//!
//! One row per student, so the primary key **is** the student — no generated
//! id and no composite. Tags are validated against the same
//! `dietary_tags` vocabulary a dish's tags come from ([`Settings::get_dietary_tags`]);
//! that single shared list is what makes "this dish conflicts with this
//! student" answerable as a set intersection, without the backend knowing any
//! nutrition.
//!
//! The queries live in [`crate::db::dietary_profile`]; the web layer reads
//! through [`crate::service::dietary_profile`].
//!
//! [`Settings::get_dietary_tags`]: crate::domain::settings::Settings::get_dietary_tags

use crate::constant::{MAX_DIETARY_NOTE_LEN, MAX_DIETARY_TAGS};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::validate_optional;

/// The profile's identity: the student it belongs to. Not a row column of its
/// own — the table's primary key *is* `student`, so "read the caller's
/// profile" is a single `SELECT` by key and never a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DietaryProfileId {
    pub(crate) student: UserId,
}

impl DietaryProfileId {
    pub fn of(student: &UserId) -> Self {
        Self { student: *student }
    }

    /// The student's wire key, which is the profile's.
    pub fn key(&self) -> String {
        self.student.key()
    }
}

/// The tags a student's diet carries, drawn from the school's `dietary_tags`
/// list — the same vocabulary a dish is tagged from, deduplicated and in the
/// order given. Stored one row per tag in `dietary_profile_tag` (an `ord`
/// column keeps the given order). `no_pg_array` skips the derive's
/// element-array assertion (which a `Vec` inner cannot satisfy); the impls
/// still delegate to `Vec<String>`.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent, no_pg_array)]
pub struct DietaryTags(Vec<String>);

impl DietaryTags {
    pub fn try_new(values: &[String], allowed: &[String]) -> Result<Self, ValidationError> {
        let mut tags: Vec<String> = Vec::new();
        for value in values {
            let value = value.trim();
            if !allowed.iter().any(|tag| tag == value) {
                return Err(ValidationError::Invalid {
                    field: "tags",
                    reason: "not one of the school's dietary tags (see GET /settings)",
                });
            }
            if !tags.iter().any(|tag| tag == value) {
                tags.push(value.to_string());
            }
        }
        if tags.len() > MAX_DIETARY_TAGS {
            return Err(ValidationError::Invalid {
                field: "tags",
                reason: "at most 10 tags per student",
            });
        }
        Ok(Self(tags))
    }

    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
}

/// Anything the tag list cannot say — "carries an EpiPen", "no pork".
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct DietaryNote(String);

impl DietaryNote {
    /// Blank (or whitespace-only) means "no note" — `None`, not an empty
    /// string, so the column is NULL rather than falsely present.
    pub fn try_new(value: &str) -> Result<Option<Self>, ValidationError> {
        validate_optional("note", value, MAX_DIETARY_NOTE_LEN)?;
        let value = value.trim();
        Ok((!value.is_empty()).then(|| Self(value.to_string())))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DietaryProfile {
    pub(crate) student: UserId,
    pub(crate) tags: DietaryTags,
    pub(crate) note: Option<DietaryNote>,
    pub(crate) updated_by: UserId,
    pub(crate) updated_at: Timestamp,
}

impl DietaryProfile {
    pub fn get_student(&self) -> &UserId {
        &self.student
    }

    pub fn get_tags(&self) -> &DietaryTags {
        &self.tags
    }

    pub fn get_note(&self) -> Option<&DietaryNote> {
        self.note.as_ref()
    }

    pub fn get_updated_by(&self) -> &UserId {
        &self.updated_by
    }

    pub fn get_updated_at(&self) -> Timestamp {
        self.updated_at
    }
}

/// Which of a dish's tags the reader's own profile flags. Empty when there is
/// no overlap, and empty for every reader without a profile.
pub fn conflicts(dish_tags: &[String], profile_tags: &[String]) -> Vec<String> {
    dish_tags
        .iter()
        .filter(|tag| profile_tags.iter().any(|held| held == *tag))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_must_come_from_the_school_list_and_dedupe() {
        let allowed = vec!["vegan".to_string(), "nut_allergy".to_string()];
        let tags =
            DietaryTags::try_new(&["vegan".to_string(), "vegan".to_string()], &allowed).unwrap();
        assert_eq!(tags.as_slice(), ["vegan"]);
        assert!(DietaryTags::try_new(&["halal".to_string()], &allowed).is_err());
    }

    #[test]
    fn conflicts_are_the_intersection_in_dish_order() {
        let held = vec!["nut_allergy".to_string(), "vegan".to_string()];
        assert_eq!(
            conflicts(&["vegan".to_string(), "nut_allergy".to_string()], &held),
            ["vegan", "nut_allergy"]
        );
        assert!(conflicts(&["gluten_free".to_string()], &held).is_empty());
        // No profile is no conflict, never "everything conflicts".
        assert!(conflicts(&["vegan".to_string()], &[]).is_empty());
    }
}
