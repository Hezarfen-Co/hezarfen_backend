//! Response DTOs shared across more than one handler module.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use utoipa::ToSchema;

use crate::database::Database;
use crate::domain::course::Course;
use crate::domain::course_session::CourseSession;
use crate::domain::exam::Exam;
use crate::domain::homework::Homework;
use crate::domain::role::Role as DomainRole;
use crate::domain::subject::Subject;
use crate::domain::user::{User, UserId};
use crate::error::AppError;

/// The access roles a response can carry, lowest to highest privilege (`parent`
/// is a read-only observer of its linked students). `ai` is an internal service
/// principal — the identity an out-of-process AI service carries over the QUIC
/// bridge. It is never assignable and never stored on a user row, so it only
/// ever appears in a response; requests take the five human roles
/// (`AssignableRole`), which is also what `GET /limits` lists.
///
/// The web-facing mirror of [`crate::domain::role::Role`] — it carries the
/// serde + OpenAPI derives (which the domain type deliberately omits), so it
/// renders as a proper `enum` in the docs. Serializes to the same lowercase
/// strings the domain stores.
#[derive(Serialize, Clone, Copy, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Ai,
    Parent,
    Student,
    Teacher,
    Manager,
    Admin,
}

/// The roles a request may ask for: the five human ones, exactly
/// [`crate::domain::role::ROLES`]. Documentation only — the handlers still take the
/// role as a string and funnel it through
/// [`crate::domain::role::Role::try_from_str`], so an unknown value is a uniform
/// `400` rather than a deserialization error. It exists because [`Role`] (the
/// response schema) also names `ai`, and a request schema pointed at it offered
/// callers a value the server has always refused.
#[derive(ToSchema)]
#[schema(rename_all = "lowercase")]
#[allow(dead_code)] // Nothing constructs it: the variants exist to be emitted.
pub enum AssignableRole {
    Parent,
    Student,
    Teacher,
    Manager,
    Admin,
}

impl From<DomainRole> for Role {
    fn from(role: DomainRole) -> Self {
        match role {
            DomainRole::Ai => Role::Ai,
            DomainRole::Parent => Role::Parent,
            DomainRole::Student => Role::Student,
            DomainRole::Teacher => Role::Teacher,
            DomainRole::Manager => Role::Manager,
            DomainRole::Admin => Role::Admin,
        }
    }
}

/// How another person appears inside a response: enough to say *who* without
/// leaking contact details. Rows that reference users (enrollments, attendance,
/// exam results) embed this instead of a bare id, so no client ever has to
/// show a raw ULID to a human.
#[derive(Serialize, Clone, ToSchema)]
pub struct PersonRef {
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    pub id: String,
    #[schema(example = "ada")]
    pub username: String,
    /// The name to show this person under, resolved in the same three steps
    /// `GET /users/{id}/profile` uses: the stored `display_name`, else the
    /// `"Name Surname"` join, else `null`.
    #[schema(example = "Ada Lovelace")]
    pub display_name: Option<String>,
    /// The school-issued student number, `null` for every account that holds
    /// none (every staff account included). Part of "who this is" the way a
    /// username is, which is why it rides this ref rather than a route of its
    /// own — a roster that names a student can name their number.
    #[schema(example = "1234")]
    pub student_number: Option<String>,
}

impl PersonRef {
    /// The one spelling of the display-name resolve — `ProfileResponse` calls
    /// this too. It was two hand-kept copies once, and the copy here forgot the
    /// stored name, so a person who chose one was still shown their legal name
    /// by every list that embeds a person.
    pub fn new(user: &User) -> Self {
        let full: Vec<&str> = [user.get_name(), user.get_surname()]
            .into_iter()
            .flatten()
            .map(|part| part.as_str())
            .collect();
        Self {
            id: user.get_id().key().to_string(),
            username: user.get_username().as_str().to_string(),
            display_name: user
                .get_display_name()
                .map(|chosen| chosen.as_str().to_string())
                .or_else(|| (!full.is_empty()).then(|| full.join(" "))),
            student_number: user
                .get_student_number()
                .map(|number| number.as_str().to_string()),
        }
    }

    /// Key already-loaded users by id — for handlers that hold the rows anyway.
    pub fn map_of(users: &[&User]) -> HashMap<String, PersonRef> {
        users
            .iter()
            .map(|user| (user.get_id().key().to_string(), PersonRef::new(user)))
            .collect()
    }

    /// Look `id` up in `people`, degrading to the bare id when the row is gone
    /// — a stale reference renders oddly instead of failing the request.
    pub fn resolve(people: &HashMap<String, PersonRef>, id: &UserId) -> Self {
        people
            .get(id.key().as_str())
            .cloned()
            .unwrap_or_else(|| Self {
                id: id.key().to_string(),
                username: id.key().to_string(),
                display_name: None,
                student_number: None,
            })
    }
}

/// Load every user behind `ids` (duplicates collapsed, one query) and key
/// their `PersonRef`s by id — the join half of list endpoints that embed people.
pub async fn person_map(
    ids: impl IntoIterator<Item = UserId>,
    db: &Database,
) -> Result<HashMap<String, PersonRef>, AppError> {
    let mut seen = HashSet::new();
    let unique: Vec<UserId> = ids
        .into_iter()
        .filter(|id| seen.insert(id.key().to_string()))
        .collect();
    let users = crate::service::user::list_by_ids(db, &unique).await?;
    Ok(users
        .iter()
        .map(|user| (user.get_id().key().to_string(), PersonRef::new(user)))
        .collect())
}

/// Public shape of a user: id, username, role, and the optional personal info
/// (`null` until filled in). Never carries the password hash. Shared by `auth`
/// (register/login/me) and `users` (listing, role and profile changes).
#[derive(Serialize, ToSchema)]
pub struct UserResponse {
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    pub id: String,
    #[schema(example = "ada")]
    pub username: String,
    pub role: Role,
    /// The school-issued student number, unique inside the school. `null`
    /// when the account holds none — every staff account, and any student the
    /// office has not numbered yet. Only ever a value on a `student` row: a
    /// role change away from `student` clears it.
    #[schema(example = "1234")]
    pub student_number: Option<String>,
    #[schema(example = "Ada")]
    pub name: Option<String>,
    #[schema(example = "Lovelace")]
    pub surname: Option<String>,
    #[schema(example = "ada@example.com")]
    pub email: Option<String>,
    #[schema(example = "+90 555 123 45 67")]
    pub phone: Option<String>,
    #[schema(example = "1990-01-02")]
    pub birth_date: Option<String>,
    /// UI color scheme: `light` or `dark`. `null` = never chosen — the client
    /// should fall back to the device preference.
    #[schema(example = "dark")]
    pub theme: Option<String>,
    /// UI language (ISO 639-1): `tr` or `en`. `null` = never chosen — the
    /// client should fall back to the device language.
    #[schema(example = "tr")]
    pub language: Option<String>,
    /// UI accent color as a 6-digit hex with a leading `#`, lowercase. `null` =
    /// never chosen — the client should fall back to its default accent.
    #[schema(example = "#fefae0")]
    pub palette_color: Option<String>,
    /// The name the public profile is shown under, instead of the legal one.
    /// `null` = never chosen; unlike [`PersonRef::display_name`] this is the
    /// stored column, never a fallback to `"Name Surname"`.
    #[schema(example = "Ada")]
    pub display_name: Option<String>,
    /// Free text under the profile's name; `null` when unset.
    #[schema(example = "Sınıfın en hızlı pomodorocusu.")]
    pub bio: Option<String>,
}

impl UserResponse {
    pub fn new(user: &User) -> Self {
        Self {
            id: user.get_id().key().to_string(),
            username: user.get_username().as_str().to_string(),
            role: user.get_role().into(),
            student_number: user
                .get_student_number()
                .map(|number| number.as_str().to_string()),
            name: user.get_name().map(|v| v.as_str().to_string()),
            surname: user.get_surname().map(|v| v.as_str().to_string()),
            email: user.get_email().map(|v| v.as_str().to_string()),
            phone: user.get_phone().map(|v| v.as_str().to_string()),
            birth_date: user.get_birth_date().map(|v| v.as_str().to_string()),
            theme: user.get_theme().map(|v| v.as_str().to_string()),
            language: user.get_language().map(|v| v.as_str().to_string()),
            palette_color: user.get_palette_color().map(|v| v.as_str().to_string()),
            display_name: user.get_display_name().map(|v| v.as_str().to_string()),
            bio: user.get_bio().map(|v| v.as_str().to_string()),
        }
    }
}

/// Public shape of a course — the **catalog** row, the school's template for a
/// taught course. Shared by `courses` (CRUD) and the reports that embed the
/// course an instance teaches. The taught thing is the *instance*
/// ([`super::instances::InstanceResponse`]): teachers, weekly hours and the
/// report-card weight live there, per class section.
#[derive(Serialize, ToSchema)]
pub struct CourseResponse {
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    pub id: String,
    /// Who created (and owns) the catalog row. Only they and managers/admins
    /// may edit or delete it.
    pub creator: PersonRef,
    #[schema(example = "Algebra")]
    pub title: String,
    pub description: String,
    /// `course` (a regular class), `study` (a supervised study session), or
    /// `club` (a student club). Only a `course` is taught through class-section
    /// instances and carries exams; a `study`/`club` is joined school-wide.
    #[schema(example = "course")]
    pub kind: String,
    /// How many class×course instances teach this course right now — the count
    /// the delete refusal watches (`0` is what lets the row go).
    #[schema(example = 3)]
    pub class_course_count: i64,
    /// How many individual club/study memberships the course carries.
    #[schema(example = 12)]
    pub course_membership_count: i64,
}

/// Every person a [`CourseResponse`] names: its creator. Feed this into
/// `person_map` so the response can resolve them — a name the map is missing
/// renders as an unknown person.
pub fn course_people(course: &Course) -> impl Iterator<Item = UserId> + '_ {
    std::iter::once(*course.get_creator())
}

impl CourseResponse {
    pub fn new(course: &Course, people: &HashMap<String, PersonRef>) -> Self {
        Self {
            id: course.get_id().key().to_string(),
            creator: PersonRef::resolve(people, course.get_creator()),
            title: course.get_title().as_str().to_string(),
            description: course.get_description().as_str().to_string(),
            kind: course.get_kind().as_str().to_string(),
            class_course_count: course.get_class_course_count(),
            course_membership_count: course.get_course_membership_count(),
        }
    }
}

/// Public shape of a subject (one topic of a course's curriculum). Shared by
/// `courses` (in-course creation and listing) and `subjects` (lookup, edit,
/// delete).
#[derive(Serialize, ToSchema)]
pub struct SubjectResponse {
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    pub id: String,
    /// The course whose curriculum this subject belongs to.
    pub course: String,
    #[schema(example = "Limits and continuity")]
    pub name: String,
    pub description: String,
}

impl SubjectResponse {
    pub fn new(subject: &Subject) -> Self {
        Self {
            id: subject.get_id().key().to_string(),
            course: subject.get_course().key().to_string(),
            name: subject.get_name().as_str().to_string(),
            description: subject.get_description().as_str().to_string(),
        }
    }
}

/// Public shape of a homework assignment. Shared by the instance routes
/// (creation and listing) and `homework` (cross-instance list, lookup, edit).
/// `assigned` is the student subset — `null` means the whole enrolled roster
/// (whoever is enrolled at submit time); a subset lists the named students' ids.
/// `due_at`/`created_at` are UTC unix-milliseconds; lateness is judged per
/// submission (against `due_at`), never stored on the homework itself.
#[derive(Serialize, ToSchema)]
pub struct HomeworkResponse {
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    pub id: String,
    /// The class×course instance this homework belongs to
    /// (`GET /instances/{id}`) — the class section's own assignment, not a
    /// school-wide course's.
    pub class_course: String,
    /// The course subject this homework is tagged with.
    pub subject: String,
    #[schema(example = "Read chapter 3")]
    pub title: String,
    pub description: Option<String>,
    /// When the homework is due, UTC unix-milliseconds.
    #[schema(example = 1_900_000_000_000_i64)]
    pub due_at: i64,
    /// The assigned student ids, or `null` for the whole enrolled course. Only
    /// a viewer who manages the course sees the whole subset; to anyone else it
    /// is narrowed to their own id (the course roster is teacher+-only, so a
    /// subset assignment must not hand out the names it lists).
    pub assigned: Option<Vec<String>>,
    /// Who assigned the homework.
    pub created_by: String,
    /// When it was assigned, UTC unix-milliseconds.
    pub created_at: i64,
}

impl HomeworkResponse {
    pub fn new(homework: &Homework) -> Self {
        Self {
            id: homework.get_id().key().to_string(),
            class_course: homework.get_class_course().key().to_string(),
            subject: homework.get_subject().key().to_string(),
            title: homework.get_title().as_str().to_string(),
            description: homework.get_description().map(|d| d.as_str().to_string()),
            due_at: homework.get_due_at().as_millis(),
            assigned: homework
                .get_assigned()
                .map(|users| users.iter().map(|user| user.key().to_string()).collect()),
            created_by: homework.get_created_by().key().to_string(),
            created_at: homework.get_created_at().as_millis(),
        }
    }

    /// The same homework as seen by a viewer who does *not* manage its course.
    /// A subset roster is narrowed to `viewer` alone: they learn that they are
    /// named — which is why the homework reached them at all — and nothing
    /// about who else is. Whole-course (`null`) stays `null`; every read path
    /// that uses this has already refused a viewer the subset does not name
    /// (404), so a one-element list is never a lie.
    pub fn for_viewer(homework: &Homework, viewer: &UserId) -> Self {
        let mut response = Self::new(homework);
        response.assigned = response.assigned.map(|_| vec![viewer.key().to_string()]);
        response
    }
}

/// Public shape of a course session (one lesson). Shared by the instance
/// routes (creation and listing) and `sessions` (CRUD + roll call).
#[derive(Serialize, ToSchema)]
pub struct SessionResponse {
    pub id: String,
    /// The class×course instance this lesson belongs to
    /// (`GET /instances/{id}`).
    pub class_course: String,
    /// Who teaches this session.
    pub teacher: PersonRef,
    pub topic: String,
    /// Lesson start, UTC unix-milliseconds.
    pub starts_at: i64,
    /// Lesson end, UTC unix-milliseconds; `null` when open-ended.
    pub ends_at: Option<i64>,
}

impl SessionResponse {
    pub fn new(session: &CourseSession, people: &HashMap<String, PersonRef>) -> Self {
        Self {
            id: session.get_id().key().to_string(),
            class_course: session.get_class_course().key().to_string(),
            teacher: PersonRef::resolve(people, session.get_teacher()),
            topic: session.get_topic().as_str().to_string(),
            starts_at: session.get_starts_at().as_millis(),
            ends_at: session.get_ends_at().map(|t| t.as_millis()),
        }
    }
}

/// Public shape of an exam. Shared by `exams` (CRUD/results) and the instance
/// routes (creation and listing). The schedule fields are all `null` for an
/// offline-graded exam (no mode); see the create/update endpoints for the
/// rules tying them together.
#[derive(Serialize, ToSchema)]
pub struct ExamResponse {
    pub id: String,
    pub creator: String,
    /// The class×course instance the exam belongs to (`GET /instances/{id}`) —
    /// two class sections teaching the same course sit their own exams.
    pub class_course: String,
    /// The term the exam is sat in (`GET /terms`); its marks count into that
    /// term's report card.
    pub term: String,
    pub title: String,
    pub description: String,
    /// The assessment form. Its weight in the course average is school policy:
    /// `GET /settings` maps each kind to a weight.
    pub kind: String,
    /// `sync`, `async`, or `open`; `null` for an offline-graded exam
    /// (not sittable).
    #[schema(example = "sync")]
    pub mode: Option<String>,
    /// Window open, UTC unix-milliseconds (`sync`/`async`).
    pub starts_at: Option<i64>,
    /// Window close, UTC unix-milliseconds (`sync`/`async`).
    pub ends_at: Option<i64>,
    /// Per-attempt time budget in milliseconds — required for `async`,
    /// optional for `open` (`null` = unlimited time).
    pub duration_ms: Option<i64>,
    /// How many attempts each student gets; `0` means unlimited.
    #[schema(example = 1)]
    pub max_attempts: i64,
    /// Whether a student who left the exam room may come back in and keep
    /// answering. Teachers can flip this live.
    pub allow_rejoin: bool,
    /// Whether students may review their graded attempt once results are out.
    /// Teachers can flip this live.
    pub allow_review: bool,
    /// Still being prepared: visible only to the course's managers, not
    /// sittable, not gradable. Publish by `PATCH`ing `draft: false`.
    pub draft: bool,
}

impl ExamResponse {
    pub fn new(exam: &Exam) -> Self {
        Self {
            id: exam.get_id().key().to_string(),
            creator: exam.get_creator().key().to_string(),
            class_course: exam.get_class_course().key().to_string(),
            term: exam.get_term().key().to_string(),
            title: exam.get_title().as_str().to_string(),
            description: exam.get_description().as_str().to_string(),
            kind: exam.get_kind().as_str().to_string(),
            mode: exam.get_mode().map(|m| m.as_str().to_string()),
            starts_at: exam.get_starts_at().map(|t| t.as_millis()),
            ends_at: exam.get_ends_at().map(|t| t.as_millis()),
            duration_ms: exam.get_duration_ms().map(|d| d.as_millis()),
            max_attempts: exam.get_max_attempts().as_i64(),
            allow_rejoin: exam.get_allow_rejoin(),
            allow_review: exam.get_allow_review(),
            draft: exam.is_draft(),
        }
    }
}
