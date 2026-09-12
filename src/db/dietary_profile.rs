//! The `dietary_profile` table: one row per student (the primary key *is*
//! the student), created on first write and afterwards patched
//! field-by-field — both by one `INSERT … ON CONFLICT DO UPDATE`. The
//! tag/note newtypes and the pure tag intersection live in
//! [`crate::domain::dietary_profile`]; the web layer reads through
//! [`crate::service::dietary_profile`].

use crate::database::Database;
use crate::domain::dietary_profile::{DietaryNote, DietaryProfile, DietaryTags};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// One student's profile, or `None` while the school never recorded one.
/// A single `SELECT` by key — this is what a menu read joins on.
pub async fn read(db: &Database, student: &UserId) -> Result<Option<DietaryProfile>, AppError> {
    let row = sqlx::query_as!(
        DietaryProfile,
        "SELECT student AS \"student: UserId\", tags AS \"tags: DietaryTags\", note AS \"note: DietaryNote\", updated_by AS \"updated_by: UserId\", updated_at AS \"updated_at: Timestamp\"
         FROM dietary_profile WHERE student = $1",
        student.uuid(),
    )
    .fetch_optional(db)
    .await?;
    Ok(row)
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
/// afterwards only the named fields move, because a whole-row save would
/// also revert a concurrent edit of the other field.
///
/// Both paths are one statement: an absent `tags` keeps the stored list
/// (`COALESCE`), an absent `note` keeps the stored one, a `Some(None)` note
/// clears it and a `Some(Some)` overwrites it — the same three-way shape,
/// spelled with a "was the field on the request?" flag plus its nullable
/// value. A rival `PATCH` that created the row a moment ago turns this
/// call's `INSERT` into the `DO UPDATE` arm: a plain field write after all,
/// exactly as it would have been a moment later — without the error the old
/// store's create-then-patch pair had to catch.
pub async fn save(
    db: &Database,
    student: &UserId,
    tags: Option<DietaryTags>,
    note: Option<Option<DietaryNote>>,
    by: &UserId,
) -> Result<DietaryProfile, AppError> {
    let row = sqlx::query_as!(
        DietaryProfile,
        "INSERT INTO dietary_profile (student, tags, note, updated_by, updated_at)
         VALUES ($1, COALESCE($2, '{}'::text[]), $3, $4, $5)
         ON CONFLICT (student) DO UPDATE SET
             tags = COALESCE($2, dietary_profile.tags),
             note = CASE WHEN $6 THEN $3 ELSE dietary_profile.note END,
             updated_by = $4,
             updated_at = $5
         RETURNING student AS \"student: UserId\", tags AS \"tags: DietaryTags\", note AS \"note: DietaryNote\", updated_by AS \"updated_by: UserId\", updated_at AS \"updated_at: Timestamp\"",
        student.uuid(),
        tags.as_ref().map(|t| t.as_slice()),
        note.as_ref().and_then(|n| n.as_ref()).map(|n| n.as_str()),
        by.uuid(),
        Timestamp::now().as_millis(),
        note.is_some(),
    )
    .fetch_one(db)
    .await?;
    Ok(row)
}
