//! The `dietary_profile` table: one row per student (the record key *is*
//! the student's key), created on first write and afterwards patched
//! field-by-field. The tag/note newtypes and the pure tag intersection live
//! in [`crate::domain::dietary_profile`]; the web layer reads through
//! [`crate::service::dietary_profile`].

use crate::database::Database;
use crate::db::field_update::FieldUpdate;
use crate::domain::dietary_profile::{DietaryNote, DietaryProfile, DietaryProfileId, DietaryTags};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// One student's profile, or `None` while the school never recorded one.
/// A single `SELECT` by record id — this is what a menu read joins on.
pub async fn read(db: &Database, student: &UserId) -> Result<Option<DietaryProfile>, AppError> {
    Ok(db.select(DietaryProfileId::of(student).record()).await?)
}

/// The tags a menu read matches its dishes against: the caller's, or an
/// empty list when they have no profile (every manager, every teacher).
pub async fn tags_of(db: &Database, student: &UserId) -> Result<Vec<String>, AppError> {
    Ok(read(db, student)
        .await?
        .map(|profile| profile.get_tags().as_slice().to_vec())
        .unwrap_or_default())
}

/// Write the fields the PATCH carried. Creates the row on first write;
/// afterwards only the named fields move, because `student` is `READONLY`
/// and a whole-row save would also revert a concurrent edit of the other
/// field (see [`FieldUpdate`]).
pub async fn save(
    db: &Database,
    student: &UserId,
    tags: Option<DietaryTags>,
    note: Option<Option<DietaryNote>>,
    by: &UserId,
) -> Result<DietaryProfile, AppError> {
    if read(db, student).await?.is_none() {
        let profile = DietaryProfile {
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
            // [`crate::db::meal_ledger::append`], which
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
        .run::<DietaryProfile>(db)
        .await
}
