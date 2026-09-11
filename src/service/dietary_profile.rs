//! The dietary-profile reads and saves the meal surfaces drive. The row is
//! one per student, created on first write and patched field-by-field; the
//! queries live in [`crate::db::dietary_profile`], the tag/note newtypes
//! and the pure tag intersection in [`crate::domain::dietary_profile`].

use crate::database::Database;
use crate::db::dietary_profile;
use crate::domain::dietary_profile::{DietaryNote, DietaryProfile, DietaryTags};
use crate::domain::user::UserId;
use crate::error::AppError;

/// One student's profile, or `None` while the school never recorded one.
pub async fn read(db: &Database, student: &UserId) -> Result<Option<DietaryProfile>, AppError> {
    dietary_profile::read(db, student).await
}

/// The tags a menu read matches its dishes against: the caller's, or an
/// empty list when they have no profile.
pub async fn tags_of(db: &Database, student: &UserId) -> Result<Vec<String>, AppError> {
    dietary_profile::tags_of(db, student).await
}

/// Write the fields the PATCH carried; creates the row on first write.
pub async fn save(
    db: &Database,
    student: &UserId,
    tags: Option<DietaryTags>,
    note: Option<Option<DietaryNote>>,
    by: &UserId,
) -> Result<DietaryProfile, AppError> {
    dietary_profile::save(db, student, tags, note, by).await
}
