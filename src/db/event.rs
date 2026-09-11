//! The `event` table: row reads and listings, the cascading delete, and the
//! audience's live membership resolution — the point check behind marking
//! ([`includes`]) and the who-missed report's backbone ([`members`]).

use surrealdb::types::SurrealValue;

use crate::database::{Database, transaction_with_retry};
use crate::db::field_update::FieldUpdate;
use crate::domain::class_member::{ClassMember, ClassMemberId};
use crate::domain::event::{Event, EventAudience, EventDescription, EventId, EventTitle};
use crate::domain::registration::Registration;
use crate::domain::timestamp::{Timestamp, range_error};
use crate::domain::user::{User, UserId};
use crate::error::AppError;

/// Is `user` in `audience`'s roster right now? The point check behind
/// marking — cheaper than resolving the whole roster when the target is
/// known. `event` must be the event this audience came from: the
/// registration kind resolves against that event's signup rows.
pub async fn includes(
    db: &Database,
    audience: &EventAudience,
    event: &EventId,
    user: &User,
) -> Result<bool, AppError> {
    match audience {
        EventAudience::School => Ok(true),
        EventAudience::Role { role } => Ok(user.get_role() == *role),
        EventAudience::Course { course } => {
            Ok(
                crate::db::enrollment::read_for_user(db, course, user.get_id())
                    .await?
                    .is_some(),
            )
        }
        // The (class, user) pair is the membership row's own id, so the
        // point check is a single select — no scan, no index needed.
        EventAudience::Class { class } => {
            let member: Option<ClassMember> = db
                .select(ClassMemberId::composite(class, user.get_id()).record())
                .await?;
            Ok(member.is_some())
        }
        EventAudience::Registration { .. } => {
            Ok(Registration::read_for_user(event, user.get_id(), db)
                .await?
                .is_some())
        }
    }
}

/// `event`'s full roster, as it stands right now — the who-missed
/// report's backbone. Registered ids that no longer resolve to a user row
/// are kept; the caller degrades their display like any stale reference.
pub async fn members(
    db: &Database,
    audience: &EventAudience,
    event: &EventId,
) -> Result<Vec<UserId>, AppError> {
    match audience {
        EventAudience::School => Ok(crate::db::user::list_all(db, None, 0)
            .await?
            .0
            .iter()
            .map(|user| user.get_id().clone())
            .collect()),
        EventAudience::Role { role } => Ok(crate::db::user::list_by_role(db, *role)
            .await?
            .iter()
            .map(|user| user.get_id().clone())
            .collect()),
        EventAudience::Course { course } => {
            Ok(crate::db::enrollment::list_for_course(db, course, None, 0)
                .await?
                .0
                .iter()
                .map(|enrollment| enrollment.get_user().clone())
                .collect())
        }
        EventAudience::Class { class } => Ok(ClassMember::list_for_class(class, None, 0, db)
            .await?
            .0
            .iter()
            .map(|member| member.get_user().clone())
            .collect()),
        EventAudience::Registration { .. } => Ok(Registration::list_for_event(event, db)
            .await?
            .iter()
            .map(|registration| registration.get_user().clone())
            .collect()),
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
    let event = Event {
        id: EventId::generate(),
        creator: creator.clone(),
        title,
        description,
        audience,
        starts_at,
        ends_at,
    };
    let created: Option<Event> = db.create(event.id.record()).content(event).await?;
    created.ok_or_else(|| AppError::Internal("failed to create event".into()))
}

pub async fn read(db: &Database, id: &EventId) -> Result<Option<Event>, AppError> {
    Ok(db.select(id.record()).await?)
}

pub async fn list_all(db: &Database) -> Result<Vec<Event>, AppError> {
    let mut result = db
        .query("SELECT * FROM event ORDER BY id DESC")
        .await?
        .check()?;
    Ok(result.take::<Vec<Event>>(0)?)
}

/// Request-scoped: the handler reads the event, then awaits the clock and a
/// course lookup before saving, holding no lock, so an omitted field
/// (`None`) is not written at all. Passing the snapshot's value back
/// instead would revert a concurrent edit of that field — scoping the `SET`
/// alone does not prevent that, the values have to come from the request.
/// The schedule columns are nullable, so they take the outer/inner
/// `Option<Option<_>>`: `None` = omitted (keep), `Some(None)` = clear.
pub async fn update(
    db: &Database,
    event: Event,
    title: Option<EventTitle>,
    description: Option<EventDescription>,
    audience: Option<EventAudience>,
    starts_at: Option<Option<Timestamp>>,
    ends_at: Option<Option<Timestamp>>,
) -> Result<Event, AppError> {
    FieldUpdate::new(event.id.record())
        .set("title", title)
        .set("description", description)
        .set("audience", audience)
        .set("starts_at", starts_at)
        .set("ends_at", ends_at)
        .ordered("starts_at", "ends_at", range_error())
        .run::<Event>(db)
        .await
}

/// Delete the event and cascade-remove its attendance and signup rows.
///
/// Children first, and all of it in one transaction the way
/// [`crate::db::course::delete`] does it: run as two queries, a
/// registration or a mark that committed in between outlived its event —
/// an orphan no read path can ever reach and no delete can ever reclaim,
/// since every one of them is keyed on the event that is now gone.
///
/// Re-sent while the store answers "conflict, retry", the way
/// [`crate::db::course_session::delete`] is: now that
/// [`crate::domain::attendance::Attendance::mark`] writes the event row to
/// prove it exists, a mark landing in this window really does contend for
/// it — and without the retry the *delete* is the side that loses, turning
/// a race the store resolved correctly into a 500 (measured 4 rounds in 4).
/// Admissible: every statement is a `DELETE`, which can never answer
/// "already exists", and a lost round wrote nothing.
pub async fn delete(db: &Database, event: Event) -> Result<Event, AppError> {
    let (mut result, mut errors) = transaction_with_retry(
        db,
        "BEGIN TRANSACTION;
             DELETE attendance WHERE event = $ev;
             DELETE registration WHERE event = $ev;
             LET $gone = (DELETE $ev RETURN BEFORE);
             RETURN $gone;
             COMMIT TRANSACTION;",
        &[("ev".into(), event.id.record().into_value())],
        // No THROW of its own — an unconditional cascade.
        &[],
    )
    .await?;
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    // Read through the trailing `RETURN`, never a hand-counted slot.
    let slot = result.num_statements().saturating_sub(2);
    result
        .take::<Vec<Event>>(slot)?
        .into_iter()
        .next()
        .ok_or(AppError::NotFound)
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
