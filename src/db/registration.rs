//! The `registration` table: the seat reads, the per-event listing, and
//! the delete that returns the seat in the same transaction. The register
//! workflow itself (the cap claim and its conflict mapping) lives in
//! [`crate::service::registration`].

use crate::database::Database;
use crate::domain::event::EventId;
use crate::domain::registration::Registration;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Some(_) iff `user` holds a seat on `event` — the audience point check.
pub async fn read_for_user(
    db: &Database,
    event: &EventId,
    user: &UserId,
) -> Result<Option<Registration>, AppError> {
    let mut result = db
        .query("SELECT * FROM registration WHERE event = $ev AND user = $usr LIMIT 1")
        .bind(("ev", event.record()))
        .bind(("usr", user.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<Registration>>(0)?.into_iter().next())
}

pub async fn list_for_event(db: &Database, event: &EventId) -> Result<Vec<Registration>, AppError> {
    let mut result = db
        .query("SELECT * FROM registration WHERE event = $ev ORDER BY id DESC")
        .bind(("ev", event.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<Registration>>(0)?)
}

pub async fn remove(
    db: &Database,
    event: &EventId,
    user: &UserId,
) -> Result<Option<Registration>, AppError> {
    // The seat comes back in the same transaction as the row that held it.
    let mut result = db
        .query(
            "BEGIN TRANSACTION;
             LET $gone = (DELETE registration WHERE event = $ev AND user = $usr RETURN BEFORE);
             UPDATE $ev SET registration_count = math::max([(registration_count ?? 0) - array::len($gone), 0]);
             RETURN $gone;
             COMMIT TRANSACTION;",
        )
        .bind(("ev", event.record()))
        .bind(("usr", user.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<Registration>>(3)?.into_iter().next())
}
