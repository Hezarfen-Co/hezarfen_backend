//! The `event` table: row reads and listings, the cascading delete, and the
//! audience's live membership resolution — the point check behind marking
//! ([`includes`]) and the who-missed report's backbone ([`members`]).

use crate::database::{Database, tx_with_retry};
use crate::db::field_update::FieldUpdate;
use crate::db::page::Param;
use crate::domain::event::{Event, EventAudience, EventDescription, EventId, EventTitle};
use crate::domain::timestamp::{Timestamp, range_error};
use crate::domain::user::{User, UserId};
use crate::error::AppError;
use sqlx::query_as;

/// Is `user` in this event's audience roster right now? The point check
/// behind marking — cheaper than resolving the whole roster when the target
/// is known. The audience comes off the row itself: the registration kind
/// resolves against that event's signup rows.
pub async fn includes(db: &Database, event: &Event, user: &User) -> Result<bool, AppError> {
    match event.get_audience_kind() {
        crate::domain::event::EventAudienceKind::School => Ok(true),
        crate::domain::event::EventAudienceKind::Role => {
            Ok(Some(user.get_role()) == event.get_audience_role())
        }
        crate::domain::event::EventAudienceKind::Course => {
            match event.get_audience_course() {
                Some(course) => Ok(crate::db::enrollment::read_for_user(db, course, user.get_id())
                    .await?
                    .is_some()),
                None => Ok(false),
            }
        }
        // The (class, user) pair is the membership row's own primary key, so
        // the point check is one existence probe — no scan, no index needed.
        crate::domain::event::EventAudienceKind::Class => match event.get_audience_class() {
            Some(class) => {
                let row = sqlx::query!(
                    "SELECT EXISTS(SELECT 1 FROM class_member WHERE class = $1 AND app_user = $2)
                     AS present",
                    class,
                    user.get_id()
                )
                .fetch_one(db)
                .await?;
                Ok(row.present)
            }
            None => Ok(false),
        },
        crate::domain::event::EventAudienceKind::Registration => {
            Ok(crate::db::registration::read_for_user(db, event.get_id(), user.get_id())
                .await?
                .is_some())
        }
    }
}

/// The event's full roster, as it stands right now — the who-missed
/// report's backbone. Registered ids that no longer resolve to a user row
/// are kept; the caller degrades their display like any stale reference.
pub async fn members(db: &Database, event: &Event) -> Result<Vec<UserId>, AppError> {
    match event.get_audience_kind() {
        crate::domain::event::EventAudienceKind::School => Ok(crate::db::user::list_all(db, None, 0)
            .await?
            .0
            .iter()
            .map(|user| user.get_id().clone())
            .collect()),
        crate::domain::event::EventAudienceKind::Role => {
            match event.get_audience_role() {
                Some(role) => Ok(crate::db::user::list_by_role(db, role)
                    .await?
                    .iter()
                    .map(|user| user.get_id().clone())
                    .collect()),
                None => Ok(Vec::new()),
            }
        }
        crate::domain::event::EventAudienceKind::Course => match event.get_audience_course() {
            Some(course) => Ok(crate::db::enrollment::list_for_course(db, course, None, 0)
                .await?
                .0
                .iter()
                .map(|enrollment| enrollment.get_user().clone())
                .collect()),
            None => Ok(Vec::new()),
        },
        crate::domain::event::EventAudienceKind::Class => match event.get_audience_class() {
            Some(class) => Ok(crate::db::class_member::list_for_class(db, class, None, 0)
                .await?
                .0
                .iter()
                .map(|member| member.get_user().clone())
                .collect()),
            None => Ok(Vec::new()),
        },
        crate::domain::event::EventAudienceKind::Registration => {
            Ok(crate::db::registration::list_for_event(db, event.get_id())
                .await?
                .iter()
                .map(|registration| registration.get_user().clone())
                .collect())
        }
    }
}

pub async fn create(
    db: &Database,
    creator: &UserId,
    title: EventTitle,
    description: EventDescription,
    audience: EventAudience,
    starts_at: Option<Timestamp>,
    ends_at: Option<Timestamp>,
) -> Result<Event, AppError> {
    // The audience payload moves as one unit with its kind; the validated
    // bundle guarantees only the matching payload is ever non-NULL.
    let created = query_as!(
        Event,
        "INSERT INTO event (id, creator, title, description, audience_kind, audience_role, \
                            audience_course, audience_class, audience_capacity, starts_at, ends_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         RETURNING id, creator, title, description, audience_kind, audience_role, \
                   audience_course, audience_class, audience_capacity, starts_at, ends_at",
        EventId::generate(),
        creator,
        title,
        description,
        audience.kind,
        audience.role,
        audience.course,
        audience.class,
        audience.capacity,
        starts_at,
        ends_at,
    )
    .fetch_one(db)
    .await?;
    Ok(created)
}

pub async fn read(db: &Database, id: &EventId) -> Result<Option<Event>, AppError> {
    let event = query_as!(
        Event,
        "SELECT id, creator, title, description, audience_kind, audience_role, audience_course, \
                audience_class, audience_capacity, starts_at, ends_at \
         FROM event WHERE id = $1",
        id
    )
    .fetch_optional(db)
    .await?;
    Ok(event)
}

pub async fn list_all(db: &Database) -> Result<Vec<Event>, AppError> {
    let events = query_as!(
        Event,
        "SELECT id, creator, title, description, audience_kind, audience_role, audience_course, \
                audience_class, audience_capacity, starts_at, ends_at \
         FROM event ORDER BY id DESC",
    )
    .fetch_all(db)
    .await?;
    Ok(events)
}

/// Request-scoped: the handler reads the event, then awaits the clock and a
/// course lookup before saving, holding no lock, so an omitted field
/// (`None`) is not written at all. Passing the snapshot's value back
/// instead would revert a concurrent edit of that field — scoping the `SET`
/// alone does not prevent that, the values have to come from the request.
/// The schedule columns are nullable, so they take the outer/inner
/// `Option<Option<_>>`: `None` = omitted (keep), `Some(None)` = clear.
///
/// The audience is five columns moving as one unit: when the request
/// carries an audience, all five are written (the non-matching payload as
/// NULL); when it carries none, none are.
pub async fn update(
    db: &Database,
    event: Event,
    title: Option<EventTitle>,
    description: Option<EventDescription>,
    audience: Option<EventAudience>,
    starts_at: Option<Option<Timestamp>>,
    ends_at: Option<Option<Timestamp>>,
) -> Result<Event, AppError> {
    FieldUpdate::new("event", event.get_id().uuid())
        .set(
            "title",
            title.map(|title| Param::Text(title.as_str().to_string())),
        )
        .set(
            "description",
            description.map(|d| Param::Text(d.as_str().to_string())),
        )
        .set(
            "audience_kind",
            audience
                .as_ref()
                .map(|a| Param::Text(a.kind.as_str().to_string())),
        )
        .set(
            "audience_role",
            audience
                .as_ref()
                .map(|a| Param::OptText(a.role.map(|r| r.as_str().to_string()))),
        )
        .set(
            "audience_course",
            audience.as_ref().map(|a| Param::OptUuid(a.course.map(|c| c.uuid()))),
        )
        .set(
            "audience_class",
            audience.as_ref().map(|a| Param::OptUuid(a.class.map(|c| c.uuid()))),
        )
        .set(
            "audience_capacity",
            audience.as_ref().map(|a| Param::OptI64(a.capacity)),
        )
        .set(
            "starts_at",
            starts_at.map(|at| Param::OptI64(at.map(|at| at.as_millis()))),
        )
        .set(
            "ends_at",
            ends_at.map(|at| Param::OptI64(at.map(|at| at.as_millis()))),
        )
        .ordered("starts_at", "ends_at", range_error())
        .run::<Event>(db)
        .await
}

/// Delete the event and cascade-remove its attendance and signup rows.
///
/// Children first, all of it in one guarded transaction: run as separate
/// statements, a registration or a mark committing in between would outlive
/// its event — an orphan no read path can ever reach and no delete can ever
/// reclaim, since every one of them is keyed on the event that is now gone.
///
/// `cascade` retry: a mark landing while the cascade is in flight contends
/// for the event row (its insert's foreign key proves the event the moment
/// it lands), so a lost round re-sends instead of answering 500.
pub async fn delete(db: &Database, event: Event) -> Result<Event, AppError> {
    tx_with_retry(db, true, async |tx| {
        sqlx::query!("DELETE FROM attendance WHERE event = $1", event.get_id())
            .execute(&mut *tx)
            .await?;
        sqlx::query!(
            "DELETE FROM registration WHERE event = $1",
            event.get_id()
        )
        .execute(&mut *tx)
        .await?;
        let gone = query_as!(
            Event,
            "DELETE FROM event WHERE id = $1 \
             RETURNING id, creator, title, description, audience_kind, audience_role, \
                       audience_course, audience_class, audience_capacity, starts_at, ends_at",
            event.get_id()
        )
        .fetch_optional(&mut *tx)
        .await?;
        gone.ok_or(AppError::NotFound)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::REGISTRATION_FROZEN_GUARD;
    use crate::domain::class_group::ClassGroupId;

    /// The forcing function behind the one rule with two spellings: the freeze
    /// this file decides in Rust ([`Event::registration_capacity`]) and
    /// [`REGISTRATION_FROZEN_GUARD`], its SurrealQL copy, which
    /// the role cascade (`service::user::set_role`) carries because it frees a demoted
    /// parent's seats inside a transaction and cannot call Rust from there.
    ///
    /// Every schedule shape is put to both, including the three the SQL is most
    /// likely to get wrong: the ends_at-only event (`??` must fall through to
    /// it), the boundary itself (closed *at* the instant, not after it — `>=` in
    /// Rust, `<=` in SQL, and the two read opposite ways round; `$now` is bound
    /// to that exact instant so the boundary is really exercised), and an event
    /// that takes no registrations at all, which Rust refuses one arm *earlier*
    /// — so the SQL must not report it frozen, or a stray row on it would be
    /// preserved forever instead of swept.
    #[tokio::test]
    async fn the_sql_freeze_guard_matches_the_rust_one() {
        let db = crate::database::init_mem().await.unwrap();
        let now = Timestamp::now().as_millis();
        let past = Some(Timestamp::from_millis(now - 60_000));
        let future = Some(Timestamp::from_millis(now + 3_600_000));
        let later = Some(Timestamp::from_millis(now + 7_200_000));
        let signup = EventAudience::Registration { capacity: None };
        let cases = [
            ("timeless", signup.clone(), None, None),
            ("deadline ahead", signup.clone(), None, future),
            ("deadline passed", signup.clone(), None, past),
            ("starts later", signup.clone(), future, later),
            ("already started", signup.clone(), past, None),
            ("started, ends later", signup.clone(), past, future),
            (
                "closing this instant",
                signup.clone(),
                Some(Timestamp::from_millis(now)),
                None,
            ),
            // No signup list to freeze: `registration_capacity` refuses these on
            // the audience, before it ever looks at the clock.
            ("started school event", EventAudience::School, past, None),
            (
                "started class event",
                EventAudience::Class {
                    class: ClassGroupId::from_key("g1"),
                },
                past,
                None,
            ),
        ];
        for (name, audience, starts_at, ends_at) in cases {
            let event = create(
                &db,
                &UserId::from_key("teacher"),
                EventTitle::try_new(name).unwrap(),
                EventDescription::try_new("").unwrap(),
                audience,
                starts_at,
                ends_at,
            )
            .await
            .unwrap();
            let rust = matches!(
                event.registration_capacity(),
                Err(AppError::Conflict(_) | AppError::ConflictOwned(_))
            );
            let mut result = db
                .query(format!(
                    "SELECT VALUE id FROM $ev WHERE {REGISTRATION_FROZEN_GUARD}"
                ))
                .bind(("ev", event.get_id().record()))
                .bind(("now", now))
                .await
                .unwrap()
                .check()
                .unwrap();
            let sql = !result
                .take::<Vec<surrealdb::types::RecordId>>(0)
                .unwrap()
                .is_empty();
            assert_eq!(
                sql, rust,
                "the SQL freeze guard and registration_capacity disagree on a \
                 {name} event — the role cascade would rewrite a closed signup \
                 list, or strand a seat on an open one"
            );
        }
    }
}
