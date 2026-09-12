//! The `registration` table: the seat reads, the per-event listing, and
//! the delete that returns the seat in the same transaction. The register
//! workflow itself (the cap claim and its conflict mapping) lives in
//! [`crate::service::registration`].

use crate::database::{Database, tx_with_retry, unique_violation};
use crate::db::cap::Claimed;
use crate::domain::event::EventId;
use crate::domain::registration::Registration;
use crate::domain::user::UserId;
use crate::error::AppError;
use sqlx::query_as;

/// Some(_) iff `user` holds a seat on `event` — the audience point check.
pub async fn read_for_user(
    db: &Database,
    event: &EventId,
    user: &UserId,
) -> Result<Option<Registration>, AppError> {
    let seat = query_as!(
        Registration,
        "SELECT event AS \"event: EventId\", app_user AS \"user: UserId\", \
                registered_by AS \"registered_by: UserId\" \
         FROM registration
         WHERE event = $1 AND app_user = $2
         LIMIT 1",
        event.uuid(),
        user.uuid()
    )
    .fetch_optional(db)
    .await?;
    Ok(seat)
}

pub async fn list_for_event(db: &Database, event: &EventId) -> Result<Vec<Registration>, AppError> {
    let seats = query_as!(
        Registration,
        "SELECT event AS \"event: EventId\", app_user AS \"user: UserId\", \
                registered_by AS \"registered_by: UserId\" \
         FROM registration
         WHERE event = $1
         ORDER BY event DESC, app_user DESC",
        event.uuid()
    )
    .fetch_all(db)
    .await?;
    Ok(seats)
}

/// Take a seat for `user` on `event` — the cap claim of one: the seat bump,
/// the holder's role handshake, and the row insert are a single statement,
/// so neither a racing placement nor a concurrent capacity lowering can
/// over-admit (the capacity is read off the event row as it is decided, not
/// off a snapshot), and a demotion to `parent` cannot strand a seat the
/// sweep would miss: the handshake takes the holder's row
/// `FOR NO KEY UPDATE`, the same key the demotion writes, so either this
/// sees the parent and inserts nothing, or the sweep sees this row.
///
/// The verdict maps exactly like the old guard layer's: `Made` — seated;
/// `Duplicate` — a rival placement of the same pair won (23505 on
/// `registration_event_user`); `Full` — the event is at its capacity, was
/// deleted, or the holder fell to `parent` (all one marker; the caller
/// re-reads to pick the message).
pub async fn claim_seat(
    db: &Database,
    event: &EventId,
    user: &UserId,
    registered_by: &UserId,
) -> Result<Claimed<Registration>, AppError> {
    match query_as!(
        Registration,
        "WITH person AS (
             -- The lock handshake comes FIRST: FOR NO KEY UPDATE waits out a
             -- racing demotion and re-checks the role against the committed
             -- row. A data-modifying CTE executes even when the INSERT below
             -- matches nothing, so the bump must depend on this CTE's result —
             -- the EXISTS reference in `seat` forces that order.
             SELECT 1 FROM app_user WHERE id = $2 AND role IS DISTINCT FROM 'parent'
             FOR NO KEY UPDATE),
         seat AS (
             UPDATE event SET registration_count = registration_count + 1
             WHERE id = $1
               AND (audience_capacity IS NULL OR registration_count < audience_capacity)
               AND EXISTS (SELECT 1 FROM person)
             RETURNING 1)
         INSERT INTO registration (event, app_user, registered_by)
         SELECT $1, $2, $3
         WHERE EXISTS (SELECT 1 FROM seat)
         RETURNING event AS \"event: EventId\", app_user AS \"user: UserId\", registered_by AS \"registered_by: UserId\"",
        event.uuid(),
        user.uuid(),
        registered_by.uuid()
    )
    .fetch_optional(db)
    .await
    {
        Ok(Some(row)) => Ok(Claimed::Made(row)),
        Ok(None) => Ok(Claimed::Full),
        Err(err) if unique_violation(&err) == Some("registration_event_user") => {
            Ok(Claimed::Duplicate)
        }
        Err(err) => Err(err.into()),
    }
}

pub async fn remove(
    db: &Database,
    event: &EventId,
    user: &UserId,
) -> Result<Option<Registration>, AppError> {
    // The seat comes back in the same transaction as the row that held it.
    // Owned captures (`Send` rule of `tx_with_retry` closures).
    let event = event.clone();
    let user = *user;
    tx_with_retry(db, false, async move |tx| {
        let gone = query_as!(
            Registration,
            "DELETE FROM registration WHERE event = $1 AND app_user = $2
             RETURNING event AS \"event: EventId\", app_user AS \"user: UserId\", registered_by AS \"registered_by: UserId\"",
            event.uuid(),
            user.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        if gone.is_some() {
            sqlx::query!(
                "UPDATE event SET registration_count = GREATEST(registration_count - 1, 0)
                 WHERE id = $1",
                event.uuid()
            )
            .execute(&mut *tx)
            .await?;
        }
        Ok(gone)
    })
    .await
}
