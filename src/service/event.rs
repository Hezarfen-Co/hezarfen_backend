//! Event surfacing: the calendar's create/read/update/delete and the
//! audience's live membership resolution. The queries live in
//! [`crate::db::event`]; there is no workflow of this domain's own — every
//! write is a single atomic statement or transaction, and the freeze rule
//! the signup flow leans on is the pure
//! [`crate::domain::event::Event::registration_capacity`].

use crate::database::Database;
use crate::db::event;
use crate::domain::event::{Event, EventAudience, EventDescription, EventId, EventTitle};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::AppError;

pub async fn create(
    db: &Database,
    creator: &UserId,
    title: EventTitle,
    description: EventDescription,
    audience: EventAudience,
    starts_at: Option<Timestamp>,
    ends_at: Option<Timestamp>,
) -> Result<Event, AppError> {
    event::create(
        db,
        creator,
        title,
        description,
        audience,
        starts_at,
        ends_at,
    )
    .await
}

pub async fn read(db: &Database, id: &EventId) -> Result<Option<Event>, AppError> {
    event::read(db, id).await
}

/// Every event, newest first.
pub async fn list_all(db: &Database) -> Result<Vec<Event>, AppError> {
    event::list_all(db).await
}

/// Only what the request carried is written: an omitted field (`None`) is
/// not stored at all. The schedule columns take the set-or-clear spelling.
pub async fn update(
    db: &Database,
    event: Event,
    title: Option<EventTitle>,
    description: Option<EventDescription>,
    audience: Option<EventAudience>,
    starts_at: Option<Option<Timestamp>>,
    ends_at: Option<Option<Timestamp>>,
) -> Result<Event, AppError> {
    event::update(db, event, title, description, audience, starts_at, ends_at).await
}

/// Delete the event and cascade its attendance and signup rows.
pub async fn delete(db: &Database, event: Event) -> Result<Event, AppError> {
    event::delete(db, event).await
}

/// Is `user` in the event's audience roster right now — the point check
/// behind marking.
pub async fn includes(db: &Database, event: &Event, user: &User) -> Result<bool, AppError> {
    event::includes(db, event, user).await
}

/// The event's full expected-attendee roster, resolved live.
pub async fn members(db: &Database, event: &Event) -> Result<Vec<UserId>, AppError> {
    event::members(db, event).await
}
