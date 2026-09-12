use crate::constant::{MAX_EVENT_DESCRIPTION_LEN, MAX_EVENT_TITLE_LEN};
use crate::domain::class_group::ClassGroupId;
use crate::domain::course::CourseId;
use crate::domain::monotonic_id::next_uuid;
use crate::domain::role::Role;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_optional, validate_required};

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct EventId(uuid::Uuid);

impl EventId {
    /// Minted from the process-wide monotonic generator, not a plain random
    /// UUID: events list `id DESC` (newest first,
    /// [`crate::db::event::list_all`]),
    /// and a random low half scrambles rows minted in the same millisecond.
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    /// The inner uuid, for runtime-checked binds (Param/QueryBuilder) that
    /// cannot take the newtype. Static `query!` binds take `self` directly.
    pub fn uuid(&self) -> Uuid {
        self.0
    }

    /// Parses a wire key. A key that is not a UUID parses as the nil UUID,
    /// which matches no row.
    pub fn from_key(key: &str) -> Self {
        Self(uuid::Uuid::parse_str(key).unwrap_or(uuid::Uuid::nil()))
    }

    pub fn key(&self) -> String {
        self.0.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
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

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
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

/// Which shape of expected-attendee roster the event carries — the `kind`
/// discriminant of the old tagged value, now a flat column. Membership is
/// resolved live against today's users/enrollments/registrations — never
/// snapshotted — so a role change, (un)enrollment, or (un)registration moves
/// people in and out of rosters by itself. The audience does not gate *seeing*
/// the event (the calendar stays school-visible); it defines who counts as
/// expected and who may be marked.
///
/// The payload lives in the sibling columns: `audience_role` for
/// [`EventAudienceKind::Role`], `audience_course` for
/// [`EventAudienceKind::Course`], `audience_class` for
/// [`EventAudienceKind::Class`], `audience_capacity` for
/// [`EventAudienceKind::Registration`]. A `School` event carries none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
pub enum EventAudienceKind {
    /// Everybody — the pre-audience behavior.
    School,
    /// Every user holding exactly this role ("all students", "all teachers").
    Role,
    /// A course's enrolled students. Staff running the course are not implied
    /// members — a mixed gathering wants a registration or role audience.
    Course,
    /// A class section's (şube) students, read live off `class_member` — the
    /// same live resolution `Course` and `Role` get, so adding a student to the
    /// class puts them on every one of its events' rosters at once. The
    /// homeroom teacher is not implied, matching `Course`. A deleted class
    /// leaves the event standing with an empty roster, exactly as a deleted
    /// course does (`Course::delete` never touches `event`).
    Class,
    /// A signup list built one person at a time through the register
    /// endpoints — teachers place students, staff take their own seat. The
    /// list is capped at the `audience_capacity` seats when set (`NULL` =
    /// unlimited) and closes once the event starts. Rows survive an audience
    /// change inertly.
    Registration,
}

/// Who an event is aimed at, as one flat row. The five audience columns
/// (kind + payload) are exactly the `audience_*` columns of the migration;
/// every write goes through one validated shape, so only the payload matching
/// `audience_kind` is ever non-NULL.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Event {
    pub(crate) id: EventId,
    pub(crate) creator: UserId,
    pub(crate) title: EventTitle,
    pub(crate) description: EventDescription,
    pub(crate) audience_kind: EventAudienceKind,
    pub(crate) audience_role: Option<Role>,
    pub(crate) audience_course: Option<CourseId>,
    pub(crate) audience_class: Option<ClassGroupId>,
    pub(crate) audience_capacity: Option<i64>,
    pub(crate) starts_at: Option<Timestamp>,
    pub(crate) ends_at: Option<Timestamp>,
}

impl EventAudienceKind {
    /// The wire/storage spelling. Must stay in lockstep with `rename_all` —
    /// the web layer publishes these as the audience `kind`.
    pub fn as_str(self) -> &'static str {
        match self {
            EventAudienceKind::School => "school",
            EventAudienceKind::Role => "role",
            EventAudienceKind::Course => "course",
            EventAudienceKind::Class => "class",
            EventAudienceKind::Registration => "registration",
        }
    }
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

    /// Which audience shape the event carries; the payload sits in the
    /// matching sibling column.
    pub fn get_audience_kind(&self) -> EventAudienceKind {
        self.audience_kind
    }

    pub fn get_audience_role(&self) -> Option<Role> {
        self.audience_role
    }

    pub fn get_audience_course(&self) -> Option<&CourseId> {
        self.audience_course.as_ref()
    }

    pub fn get_audience_class(&self) -> Option<&ClassGroupId> {
        self.audience_class.as_ref()
    }

    pub fn get_audience_capacity(&self) -> Option<i64> {
        self.audience_capacity
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
        if self.audience_kind != EventAudienceKind::Registration {
            return Err(AppError::Validation(ValidationError::Invalid {
                field: "audience",
                reason: "this event does not take registrations",
            }));
        }
        // ends_at can't precede starts_at, so when both exist starts_at governs.
        if let Some(closes_at) = self.starts_at.or(self.ends_at)
            && Timestamp::now().as_millis() >= closes_at.as_millis()
        {
            return Err(AppError::Conflict(
                "registration closed — the event has started or ended",
            ));
        }
        Ok(self.audience_capacity)
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

    /// The `audience_kind` column is `TEXT` with a CHECK listing exactly
    /// these spellings — audience-shaped queries match on them, so the
    /// storage form may not drift (which `rename_all` mirrors).
    #[test]
    fn audience_kind_spellings_are_frozen() {
        assert_eq!(EventAudienceKind::School.as_str(), "school");
        assert_eq!(EventAudienceKind::Role.as_str(), "role");
        assert_eq!(EventAudienceKind::Course.as_str(), "course");
        assert_eq!(EventAudienceKind::Class.as_str(), "class");
        assert_eq!(EventAudienceKind::Registration.as_str(), "registration");
    }

    /// The audience columns as a row writer would set them.
    fn event_with(
        audience_kind: EventAudienceKind,
        audience_capacity: Option<i64>,
        starts_at: Option<Timestamp>,
        ends_at: Option<Timestamp>,
    ) -> Event {
        Event {
            id: EventId::generate(),
            creator: UserId::from_key("0198f1a2-3b4c-7d5e-8f90-aa2b3c4d5e6f"),
            title: EventTitle::try_new("signup").unwrap(),
            description: EventDescription::try_new("").unwrap(),
            audience_kind,
            audience_role: None,
            audience_course: None,
            audience_class: None,
            audience_capacity,
            starts_at,
            ends_at,
        }
    }

    #[tokio::test]
    async fn registration_capacity_gates_and_echoes_cap() {
        let past = Some(Timestamp::from_millis(Timestamp::now().as_millis() - 1));

        assert!(matches!(
            event_with(EventAudienceKind::School, None, None, None).registration_capacity(),
            Err(AppError::Validation(_))
        ));
        assert_eq!(
            event_with(EventAudienceKind::Registration, None, None, None)
                .registration_capacity()
                .unwrap(),
            None
        );
        assert_eq!(
            event_with(EventAudienceKind::Registration, Some(30), None, None)
                .registration_capacity()
                .unwrap(),
            Some(30)
        );
        // Started events close the list; an ends_at-only deadline in the past
        // does too.
        assert!(matches!(
            event_with(EventAudienceKind::Registration, Some(30), past, None)
                .registration_capacity(),
            Err(AppError::Conflict(_))
        ));
        assert!(matches!(
            event_with(EventAudienceKind::Registration, None, None, past)
                .registration_capacity(),
            Err(AppError::Conflict(_))
        ));
    }
}
