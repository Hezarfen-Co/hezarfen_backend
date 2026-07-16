// `Value` looks unused but is load-bearing: the `SurrealValue` derive on the
// tagged `EventAudience` enum expands to code that names `Value` unqualified.
use surrealdb::types::{RecordId, RecordIdKey, SurrealValue, Value};
use ulid::Ulid;

use crate::constant::{MAX_EVENT_DESCRIPTION_LEN, MAX_EVENT_TITLE_LEN};
use crate::database::{Database, EVENT_TABLE};
use crate::domain::course::CourseId;
use crate::domain::enrollment::Enrollment;
use crate::domain::role::Role;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_optional, validate_required};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct EventId(RecordId);

impl EventId {
    pub fn generate() -> Self {
        Self(RecordId::new(EVENT_TABLE, Ulid::new().to_string()))
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
/// resolved live against today's users/enrollments — never snapshotted — so a
/// role change or (un)enrollment moves people in and out of rosters by itself.
/// The audience does not gate *seeing* the event (the calendar stays
/// school-visible); it defines who counts as expected and who may be marked.
///
/// Stored internally tagged: `{kind: 'school'}`, `{kind: 'role', role: 'student'}`,
/// `{kind: 'course', course: course:…}`, `{kind: 'users', users: [user:…]}`.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
#[surreal(tag = "kind", rename_all = "lowercase")]
pub enum EventAudience {
    /// Everybody — the pre-audience behavior and the backfill for old rows.
    School,
    /// Every user holding exactly this role ("all students", "all teachers").
    Role { role: Role },
    /// A course's enrolled students. Staff running the course are not implied
    /// members — a mixed gathering wants `Users` or a role audience.
    Course { course: CourseId },
    /// A hand-picked list. Bounded by `MAX_EVENT_AUDIENCE_USERS` at the web
    /// layer; a member row deleted later simply stops resolving.
    Users { users: Vec<UserId> },
}

impl EventAudience {
    /// Is `user` in the roster right now? The point check behind marking —
    /// cheaper than resolving the whole roster when the target is known.
    pub async fn includes(&self, user: &User, db: &Database) -> Result<bool, AppError> {
        match self {
            EventAudience::School => Ok(true),
            EventAudience::Role { role } => Ok(user.get_role() == *role),
            EventAudience::Course { course } => {
                Ok(Enrollment::read_for_user(course, user.get_id(), db)
                    .await?
                    .is_some())
            }
            EventAudience::Users { users } => Ok(users.contains(user.get_id())),
        }
    }

    /// The full roster, as it stands right now — the who-missed report's
    /// backbone. `Users` ids that no longer resolve to a row are kept; the
    /// caller degrades their display like any stale reference.
    pub async fn members(&self, db: &Database) -> Result<Vec<UserId>, AppError> {
        match self {
            EventAudience::School => Ok(User::list_all(db)
                .await?
                .iter()
                .map(|user| user.get_id().clone())
                .collect()),
            EventAudience::Role { role } => Ok(User::list_by_role(*role, db)
                .await?
                .iter()
                .map(|user| user.get_id().clone())
                .collect()),
            EventAudience::Course { course } => Ok(Enrollment::list_for_course(course, db)
                .await?
                .iter()
                .map(|enrollment| enrollment.get_user().clone())
                .collect()),
            EventAudience::Users { users } => Ok(users.clone()),
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

    pub async fn update(
        mut self,
        title: EventTitle,
        description: EventDescription,
        audience: EventAudience,
        starts_at: Option<Timestamp>,
        ends_at: Option<Timestamp>,
        db: &Database,
    ) -> Result<Event, AppError> {
        self.title = title;
        self.description = description;
        self.audience = audience;
        self.starts_at = starts_at;
        self.ends_at = ends_at;
        let updated: Option<Event> = db.update(self.id.record()).content(self).await?;
        updated.ok_or(AppError::NotFound)
    }

    /// Delete the event and cascade-remove its attendance rows.
    pub async fn delete(self, db: &Database) -> Result<Event, AppError> {
        db.query("DELETE attendance WHERE event = $ev")
            .bind(("ev", self.id.record()))
            .await?
            .check()?;
        let deleted: Option<Event> = db.delete(self.id.record()).await?;
        deleted.ok_or(AppError::NotFound)
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
                EventAudience::Users {
                    users: vec![UserId::from_key("u1"), UserId::from_key("u2")],
                },
                "users",
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
}
