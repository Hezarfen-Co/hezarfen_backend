//! What one student may not eat: the school's record of their dietary tags
//! plus a free-text note for the kitchen.
//!
//! One row per student, so the record key **is** the student's key — no
//! generated id and no composite. Tags are validated against the same
//! `dietary_tags` vocabulary a dish's tags come from ([`Settings::get_dietary_tags`]);
//! that single shared list is what makes "this dish conflicts with this
//! student" answerable as a set intersection, without the backend knowing any
//! nutrition.
//!
//! [`Settings::get_dietary_tags`]: crate::domain::settings::Settings::get_dietary_tags

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{DIETARY_PROFILE_TABLE, MAX_DIETARY_NOTE_LEN, MAX_DIETARY_TAGS};
use crate::database::Database;
use crate::db::field_update::FieldUpdate;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_optional;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct DietaryProfileId(RecordId);

impl DietaryProfileId {
    /// The profile of one student — deterministic, so "read the caller's
    /// profile" is a single `SELECT` by id and never a scan.
    pub fn of(student: &UserId) -> Self {
        Self(RecordId::new(DIETARY_PROFILE_TABLE, student.key()))
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

/// The tags a student's diet carries, drawn from the school's `dietary_tags`
/// list — the same vocabulary a dish is tagged from, deduplicated and in the
/// order given.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
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
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct DietaryNote(String);

impl DietaryNote {
    /// Blank (or whitespace-only) means "no note" — `None`, not an empty
    /// string, so the column is absent rather than falsely present.
    pub fn try_new(value: &str) -> Result<Option<Self>, ValidationError> {
        validate_optional("note", value, MAX_DIETARY_NOTE_LEN)?;
        let value = value.trim();
        Ok((!value.is_empty()).then(|| Self(value.to_string())))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct DietaryProfile {
    id: DietaryProfileId,
    student: UserId,
    tags: DietaryTags,
    note: Option<DietaryNote>,
    updated_by: UserId,
    updated_at: Timestamp,
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

    /// One student's profile, or `None` while the school never recorded one.
    /// A single `SELECT` by record id — this is what a menu read joins on.
    pub async fn read(student: &UserId, db: &Database) -> Result<Option<Self>, AppError> {
        Ok(db.select(DietaryProfileId::of(student).record()).await?)
    }

    /// The tags a menu read matches its dishes against: the caller's, or an
    /// empty list when they have no profile (every manager, every teacher).
    pub async fn tags_of(student: &UserId, db: &Database) -> Result<Vec<String>, AppError> {
        Ok(Self::read(student, db)
            .await?
            .map(|profile| profile.tags.as_slice().to_vec())
            .unwrap_or_default())
    }

    /// Write the fields the PATCH carried. Creates the row on first write;
    /// afterwards only the named fields move, because `student` is `READONLY`
    /// and a whole-row save would also revert a concurrent edit of the other
    /// field (see [`FieldUpdate`]).
    pub async fn save(
        student: &UserId,
        tags: Option<DietaryTags>,
        note: Option<Option<DietaryNote>>,
        by: &UserId,
        db: &Database,
    ) -> Result<Self, AppError> {
        if Self::read(student, db).await?.is_none() {
            let profile = Self {
                id: DietaryProfileId::of(student),
                student: student.clone(),
                tags: tags.clone().unwrap_or(DietaryTags(Vec::new())),
                note: note.clone().flatten(),
                updated_by: by.clone(),
                updated_at: Timestamp::now(),
            };
            match db.create(profile.id.record()).content(profile).await {
                Ok(Some(created)) => return Ok(created),
                Ok(None) => {
                    return Err(AppError::Internal("failed to save the profile".into()));
                }
                // A rival first `PATCH` created the row in the round trip since
                // the read above (the `dietary_profile_student` UNIQUE index
                // catches the same collision), and `CREATE` leaves that row
                // untouched — so this call is a plain field write after all,
                // exactly as it would have been a moment later. Propagated, it
                // was a 500 on a legitimate request. Same shape as
                // [`MealLedger::append`](crate::domain::meal_ledger), which
                // reads the winner back rather than failing.
                Err(err) if err.is_already_exists() => {}
                Err(err) => return Err(err.into()),
            }
        }
        FieldUpdate::new(DietaryProfileId::of(student).record())
            .set("tags", tags)
            .set("note", note)
            .set("updated_by", Some(by.clone()))
            .set("updated_at", Some(Timestamp::now()))
            .run::<Self>(db)
            .await
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
