//! The `karne_snapshot` table: the frozen per-student, per-term report card
//! written when a term is archived.
//!
//! A report card is a *computed* read (D8) — per-instance weighted average,
//! graded against the school's bands, then a `ders_saati`-weighted year
//! average. The snapshot exists for the moment that computation must stop
//! moving: once a term is archived, the report is a record of what the school
//! issued, and a later correction to a mark must not silently rewrite a report
//! card a family already holds. [`crate::service::karne::freeze`] writes the
//! rows; [`crate::service::karne::build`] serves the snapshot back for an
//! archived term and computes live for an open one.

use sqlx::types::Json;

use crate::database::Database;
use crate::domain::term::TermId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Write (or refresh) `user`'s frozen report for `term`.
///
/// An upsert rather than an insert: freezing is idempotent and converges —
/// archiving an already-archived term writes nothing
/// ([`crate::db::term::archive`]), and a re-run recomputes the same payload
/// from the same rows, so overwriting it is a no-op in substance and one
/// statement instead of an existence dance.
pub async fn snapshot(
    db: &Database,
    term: &TermId,
    user: &UserId,
    payload: &serde_json::Value,
) -> Result<(), AppError> {
    let payload = Json(payload.clone());
    sqlx::query!(
        r#"INSERT INTO karne_snapshot (term, app_user, payload, created_at)
           VALUES ($1, $2, $3, $4)
           ON CONFLICT (term, app_user) DO UPDATE
             SET payload = EXCLUDED.payload, created_at = EXCLUDED.created_at"#,
        term.uuid(),
        user.uuid(),
        payload as _,
        Timestamp::now().as_millis(),
    )
    .execute(db)
    .await?;
    Ok(())
}

/// `user`'s frozen report for `term`, or `None` when that term was never
/// frozen (or they had no roster row to freeze).
pub async fn read_snapshot(
    db: &Database,
    term: &TermId,
    user: &UserId,
) -> Result<Option<serde_json::Value>, AppError> {
    let row = sqlx::query!(
        r#"SELECT payload AS "payload: Json<serde_json::Value>"
           FROM karne_snapshot WHERE term = $1 AND app_user = $2"#,
        term.uuid(),
        user.uuid(),
    )
    .fetch_optional(db)
    .await?;
    Ok(row.map(|row| row.payload.0))
}
