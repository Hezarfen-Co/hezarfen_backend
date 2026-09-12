// `Value` looks unused but is load-bearing: the `SurrealValue` derive on the
// tagged `EventAudience` enum expands to code that names `Value` unqualified.
use surrealdb::types::{RecordId, RecordIdKey, SurrealValue, Value};

use crate::constant::{EVENT_TABLE, MAX_EVENT_DESCRIPTION_LEN, MAX_EVENT_TITLE_LEN};
use crate::domain::class_group::ClassGroupId;
use crate::domain::course::CourseId;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::role::Role;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_optional, validate_required};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct EventId(RecordId);

impl EventId {
    /// Minted from the process-wide monotonic generator, not `Ulid::generate()`:
    /// events list `id DESC` (newest first, [`crate::db::event::list_all`]),
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

#[derive(Debug, Clone, SurrealValue)]
pub struct Event {
    pub(crate) id: EventId,
    pub(crate) creator: UserId,
    pub(crate) title: EventTitle,
    pub(crate) description: EventDescription,
    pub(crate) audience: EventAudience,
    pub(crate) starts_at: Option<Timestamp>,
    pub(crate) ends_at: Option<Timestamp>,
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
