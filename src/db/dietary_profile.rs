//! The `dietary_profile` table: one row per student (the primary key *is*
//! the student), created on first write and afterwards patched
//! field-by-field — the row by one `INSERT … ON CONFLICT DO UPDATE`, the
//! tags by child rows in `dietary_profile_tag` replaced under the same
//! transaction. The tag/note newtypes and the pure tag intersection live in
//! [`crate::domain::dietary_profile`]; the web layer reads through
//! [`crate::service::dietary_profile`].

use crate::database::{Database, tx_with_retry};
use crate::domain::dietary_profile::{DietaryNote, DietaryProfile, DietaryTags};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// One student's profile, or `None` while the school never recorded one.
/// A single `SELECT` by key — this is what a menu read joins on.
pub async fn read(db: &Database, student: &UserId) -> Result<Option<DietaryProfile>, AppError> {
    let row = sqlx::query_as!(
        DietaryProfile,
        "SELECT student AS \"student: UserId\",
             COALESCE((SELECT array_agg(t.tag ORDER BY t.ord) FROM dietary_profile_tag t WHERE t.student = dietary_profile.student), '{}'::text[]) AS \"tags!: DietaryTags\",
             note AS \"note: DietaryNote\", updated_by AS \"updated_by: UserId\", updated_at AS \"updated_at: Timestamp\"
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
/// The base row is one upsert — an absent `note` keeps the stored one, a
/// `Some(None)` clears it and a `Some(Some)` overwrites it (the same
/// three-way shape, spelled with a "was the field on the request?" flag
/// plus its nullable value) — and an absent `tags` leaves the stored tag
/// rows alone while a carried `tags` replaces the whole set, all under one
/// transaction. A rival `PATCH` that created the row a moment ago turns
/// this call's `INSERT` into the `DO UPDATE` arm: a plain field write
/// after all, exactly as it would have been a moment later — without the
/// error the old store's create-then-patch pair had to catch.
pub async fn save(
    db: &Database,
    student: &UserId,
    tags: Option<DietaryTags>,
    note: Option<Option<DietaryNote>>,
    by: &UserId,
) -> Result<DietaryProfile, AppError> {
    let student_key = student.uuid();
    let by_key = by.uuid();
    let now = Timestamp::now().as_millis();
    tx_with_retry(db, false, async move |tx| {
        sqlx::query!(
            "INSERT INTO dietary_profile (student, note, updated_by, updated_at)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (student) DO UPDATE SET
                 note = CASE WHEN $5 THEN $2 ELSE dietary_profile.note END,
                 updated_by = $3,
                 updated_at = $4",
            student_key,
            note.as_ref().and_then(|n| n.as_ref()).map(|n| n.as_str()),
            by_key,
            now,
            note.is_some(),
        )
        .execute(&mut *tx)
        .await?;
        if let Some(tags) = &tags {
            sqlx::query!(
                "DELETE FROM dietary_profile_tag WHERE student = $1",
                student_key,
            )
            .execute(&mut *tx)
            .await?;
            sqlx::query!(
                "INSERT INTO dietary_profile_tag (student, tag, ord)
                 SELECT $1, tag, ord FROM unnest($2::text[]) WITH ORDINALITY AS t(tag, ord)",
                student_key,
                tags.as_slice(),
            )
            .execute(&mut *tx)
            .await?;
        }
        let row = sqlx::query_as!(
            DietaryProfile,
            "SELECT student AS \"student: UserId\",
                 COALESCE((SELECT array_agg(t.tag ORDER BY t.ord) FROM dietary_profile_tag t WHERE t.student = dietary_profile.student), '{}'::text[]) AS \"tags!: DietaryTags\",
                 note AS \"note: DietaryNote\", updated_by AS \"updated_by: UserId\", updated_at AS \"updated_at: Timestamp\"
             FROM dietary_profile WHERE student = $1",
            student_key,
        )
        .fetch_one(&mut *tx)
        .await?;
        Ok(row)
    })
    .await
}
