// `Value` looks unused but is load-bearing: the `SurrealValue` derive on the
// tagged `EventAudience` enum expands to code that names `Value` unqualified.
use surrealdb::types::{RecordId, RecordIdKey, SurrealValue, Value};

use crate::constant::{EVENT_TABLE, MAX_EVENT_DESCRIPTION_LEN, MAX_EVENT_TITLE_LEN};
use crate::database::{Database, transaction_with_retry};
use crate::domain::class_group::ClassGroupId;
use crate::domain::class_member::{ClassMember, ClassMemberId};
use crate::domain::course::CourseId;
use crate::domain::enrollment::Enrollment;
use crate::domain::field_update::FieldUpdate;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::registration::Registration;
use crate::domain::role::Role;
use crate::domain::timestamp::{Timestamp, range_error};
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_optional, validate_required};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct EventId(RecordId);

impl EventId {
    /// Minted from the process-wide monotonic generator, not `Ulid::new()`:
    /// events list `id DESC` (newest first, [`Event::list_all`]),
    /// and a random low half scrambles rows minted in the same millisecond.
    pub fn generate() -> Self {
        Self(RecordId::new(EVENT_TABLE, next_ulid().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(EVENT_TABLE, key))
    }

    pub fn record(&self) -> RecordId {
        self.0.clone()
    }

    pub fn key(&self) -> &str {
        match &self.0.key {
            RecordIdKey::String(key) => key,
            _ => "",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct EventTitle(String);

impl EventTitle {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("title", value, MAX_EVENT_TITLE_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct EventDescription(String);

impl EventDescription {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_optional("description", value, MAX_EVENT_DESCRIPTION_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Who an event is aimed at: the expected-attendee roster. Membership is
/// resolved live against today's users/enrollments/registrations — never
/// snapshotted — so a role change, (un)enrollment, or (un)registration moves
/// people in and out of rosters by itself. The audience does not gate *seeing*
/// the event (the calendar stays school-visible); it defines who counts as
/// expected and who may be marked.
///
/// Stored internally tagged: `{kind: 'school'}`, `{kind: 'role', role: 'student'}`,
/// `{kind: 'course', course: course:…}`, `{kind: 'class', class: class_group:…}`,
/// `{kind: 'registration', capacity: 30}`.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
#[surreal(tag = "kind", rename_all = "lowercase")]
pub enum EventAudience {
    /// Everybody — the pre-audience behavior and the backfill for old rows.
    School,
    /// Every user holding exactly this role ("all students", "all teachers").
    Role { role: Role },
    /// A course's enrolled students. Staff running the course are not implied
    /// members — a mixed gathering wants a registration or role audience.
    Course { course: CourseId },
    /// A class section's (şube) students, read live off `class_member` — the
    /// same live resolution `Course` and `Role` get, so adding a student to the
    /// class puts them on every one of its events' rosters at once. The
    /// homeroom teacher is not implied, matching `Course`. A deleted class
    /// leaves the event standing with an empty roster, exactly as a deleted
    /// course does (`Course::delete` never touches `event`).
    Class { class: ClassGroupId },
    /// A signup list built one person at a time through the register
    /// endpoints — teachers place students, staff take their own seat. The
    /// list is capped at `capacity` seats when set (`None` = unlimited) and
    /// closes once the event starts. Rows survive an audience change inertly.
    Registration { capacity: Option<i64> },
}

impl EventAudience {
    /// Is `user` in `event`'s roster right now? The point check behind
    /// marking — cheaper than resolving the whole roster when the target is
    /// known. `event` must be the event this audience came from: the
    /// registration kind resolves against that event's signup rows.
    pub async fn includes(
        &self,
        event: &EventId,
        user: &User,
        db: &Database,
    ) -> Result<bool, AppError> {
        match self {
            EventAudience::School => Ok(true),
            EventAudience::Role { role } => Ok(user.get_role() == *role),
            EventAudience::Course { course } => {
                Ok(Enrollment::read_for_user(course, user.get_id(), db)
                    .await?
                    .is_some())
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
    pub async fn members(&self, event: &EventId, db: &Database) -> Result<Vec<UserId>, AppError> {
        match self {
            EventAudience::School => Ok(User::list_all(None, 0, db)
                .await?
                .0
                .iter()
                .map(|user| user.get_id().clone())
                .collect()),
            EventAudience::Role { role } => Ok(User::list_by_role(*role, db)
                .await?
                .iter()
                .map(|user| user.get_id().clone())
                .collect()),
            EventAudience::Course { course } => {
                Ok(Enrollment::list_for_course(course, None, 0, db)
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
}

#[derive(Debug, Clone, SurrealValue)]
pub struct Event {
    id: EventId,
    creator: UserId,
    title: EventTitle,
    description: EventDescription,
    audience: EventAudience,
    starts_at: Option<Timestamp>,
    ends_at: Option<Timestamp>,
}

impl Event {
    pub fn get_id(&self) -> &EventId {
        &self.id
    }

    pub fn get_creator(&self) -> &UserId {
        &self.creator
    }

    pub fn get_title(&self) -> &EventTitle {
        &self.title
    }

    pub fn get_description(&self) -> &EventDescription {
        &self.description
    }

    pub fn get_audience(&self) -> &EventAudience {
        &self.audience
    }

    pub fn get_starts_at(&self) -> Option<Timestamp> {
        self.starts_at
    }

    pub fn get_ends_at(&self) -> Option<Timestamp> {
        self.ends_at
    }

    pub fn is_creator(&self, user: &UserId) -> bool {
        &self.creator == user
    }

    /// The shared gate for touching a signup list: the event must carry the
    /// registration audience, and the list must still be open — it closes the
    /// moment the event starts, or, for an ends_at-only event (a pure signup
    /// deadline), the moment that end passes (a truly timeless event never
    /// closes). Returns the seat cap for the register path.
    pub fn registration_capacity(&self) -> Result<Option<i64>, AppError> {
        let EventAudience::Registration { capacity } = &self.audience else {
            return Err(AppError::Validation(ValidationError::Invalid {
                field: "audience",
                reason: "this event does not take registrations",
            }));
        };
        // ends_at can't precede starts_at, so when both exist starts_at governs.
        if let Some(closes_at) = self.starts_at.or(self.ends_at)
            && Timestamp::now().as_millis() >= closes_at.as_millis()
        {
            return Err(AppError::Conflict(
                "registration closed — the event has started or ended",
            ));
        }
        Ok(*capacity)
    }

    pub async fn create(
        creator: &UserId,
        title: EventTitle,
        description: EventDescription,
        audience: EventAudience,
        starts_at: Option<Timestamp>,
        ends_at: Option<Timestamp>,
        db: &Database,
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

    pub async fn read(id: &EventId, db: &Database) -> Result<Option<Event>, AppError> {
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
        self,
        title: Option<EventTitle>,
        description: Option<EventDescription>,
        audience: Option<EventAudience>,
        starts_at: Option<Option<Timestamp>>,
        ends_at: Option<Option<Timestamp>>,
        db: &Database,
    ) -> Result<Event, AppError> {
        FieldUpdate::new(self.id.record())
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
    /// [`crate::domain::course::Course::delete`] does it: run as two queries, a
    /// registration or a mark that committed in between outlived its event —
    /// an orphan no read path can ever reach and no delete can ever reclaim,
    /// since every one of them is keyed on the event that is now gone.
    ///
    /// Re-sent while the store answers "conflict, retry", the way
    /// [`crate::domain::course_session::CourseSession::delete`] is: now that
    /// [`crate::domain::attendance::Attendance::mark`] writes the event row to
    /// prove it exists, a mark landing in this window really does contend for
    /// it — and without the retry the *delete* is the side that loses, turning
    /// a race the store resolved correctly into a 500 (measured 4 rounds in 4).
    /// Admissible: every statement is a `DELETE`, which can never answer
    /// "already exists", and a lost round wrote nothing.
    pub async fn delete(self, db: &Database) -> Result<Event, AppError> {
        let (mut result, mut errors) = transaction_with_retry(
            db,
            "BEGIN TRANSACTION;
                 DELETE attendance WHERE event = $ev;
                 DELETE registration WHERE event = $ev;
                 LET $gone = (DELETE $ev RETURN BEFORE);
                 RETURN $gone;
                 COMMIT TRANSACTION;",
            &[("ev".into(), self.id.record().into_value())],
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The forcing function behind the one rule with two spellings: the freeze
    /// this file decides in Rust ([`Event::registration_capacity`]) and
    /// [`crate::constant::REGISTRATION_FROZEN_GUARD`], its SurrealQL copy, which
    /// the role cascade (`User::set_role`) carries because it frees a demoted
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
        use crate::constant::REGISTRATION_FROZEN_GUARD;

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
            let event = Event::create(
                &UserId::from_key("teacher"),
                EventTitle::try_new(name).unwrap(),
                EventDescription::try_new("").unwrap(),
                audience,
                starts_at,
                ends_at,
                &db,
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

    #[tokio::test]
    async fn title_is_required() {
        assert!(EventTitle::try_new("standup").is_ok());
        assert!(EventTitle::try_new("").is_err());
        assert!(EventTitle::try_new("   ").is_err());
    }

    #[tokio::test]
    async fn description_is_optional() {
        assert!(EventDescription::try_new("").is_ok());
    }

    /// The audience stores as an internally tagged object — `{kind: '…', …}`.
    /// The schema's `audience.kind`/`audience.role`/… field definitions and the
    /// boot backfill (`{kind: 'school'}`) are written against exactly this
    /// encoding; guard that it never drifts.
    #[tokio::test]
    async fn audience_encodes_internally_tagged_and_round_trips() {
        use surrealdb::types::Value;

        let cases = [
            (EventAudience::School, "school"),
            (
                EventAudience::Role {
                    role: Role::Student,
                },
                "role",
            ),
            (
                EventAudience::Course {
                    course: CourseId::from_key("c1"),
                },
                "course",
            ),
            (
                EventAudience::Class {
                    class: ClassGroupId::from_key("g1"),
                },
                "class",
            ),
            (
                EventAudience::Registration { capacity: Some(30) },
                "registration",
            ),
            (
                EventAudience::Registration { capacity: None },
                "registration",
            ),
        ];
        for (audience, kind) in cases {
            let value = audience.clone().into_value();
            let Value::Object(ref object) = value else {
                panic!("audience must encode as an object, got {value:?}");
            };
            assert_eq!(
                object.get("kind"),
                Some(&Value::String(kind.to_string())),
                "tag field must be `kind: '{kind}'`"
            );
            assert_eq!(EventAudience::from_value(value).unwrap(), audience);
        }
    }

    fn event_with(
        audience: EventAudience,
        starts_at: Option<Timestamp>,
        ends_at: Option<Timestamp>,
    ) -> Event {
        Event {
            id: EventId::generate(),
            creator: UserId::from_key("u1"),
            title: EventTitle::try_new("signup").unwrap(),
            description: EventDescription::try_new("").unwrap(),
            audience,
            starts_at,
            ends_at,
        }
    }

    #[tokio::test]
    async fn registration_capacity_gates_and_echoes_cap() {
        let open = |capacity| EventAudience::Registration { capacity };
        let past = Some(Timestamp::from_millis(Timestamp::now().as_millis() - 1));

        assert!(matches!(
            event_with(EventAudience::School, None, None).registration_capacity(),
            Err(AppError::Validation(_))
        ));
        assert_eq!(
            event_with(open(None), None, None)
                .registration_capacity()
                .unwrap(),
            None
        );
        assert_eq!(
            event_with(open(Some(30)), None, None)
                .registration_capacity()
                .unwrap(),
            Some(30)
        );
        // Started events close the list; an ends_at-only deadline in the past
        // does too.
        assert!(matches!(
            event_with(open(Some(30)), past, None).registration_capacity(),
            Err(AppError::Conflict(_))
        ));
        assert!(matches!(
            event_with(open(None), None, past).registration_capacity(),
            Err(AppError::Conflict(_))
        ));
    }

    /// The database strips `NONE`-valued optional columns and the boot
    /// conversion writes a bare `{kind: 'registration'}` — an object with no
    /// `capacity` key at all must decode as an uncapped registration.
    #[tokio::test]
    async fn registration_audience_decodes_without_capacity_key() {
        use surrealdb::types::Value;

        let value = EventAudience::Registration { capacity: None }.into_value();
        let Value::Object(mut object) = value else {
            panic!("audience must encode as an object, got {value:?}");
        };
        object.remove("capacity");
        assert_eq!(
            EventAudience::from_value(Value::Object(object)).unwrap(),
            EventAudience::Registration { capacity: None }
        );
    }
}
