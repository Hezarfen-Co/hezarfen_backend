//! Response DTOs shared across more than one handler module.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use utoipa::ToSchema;

use crate::database::Database;
use crate::domain::course::Course;
use crate::domain::exam::Exam;
use crate::domain::role::Role as DomainRole;
use crate::domain::user::{User, UserId};
use crate::error::AppError;

/// The four access roles, lowest to highest privilege. The web-facing mirror of
/// [`crate::domain::role::Role`] — it carries the serde + OpenAPI derives (which
/// the domain type deliberately omits), so it renders as a proper `enum` in the
/// docs. Serializes to the same lowercase strings the domain stores.
#[derive(Serialize, Clone, Copy, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Student,
    Teacher,
    Manager,
    Admin,
}

impl From<DomainRole> for Role {
    fn from(role: DomainRole) -> Self {
        match role {
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
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    pub id: String,
    #[schema(example = "ada")]
    pub username: String,
    /// `"Name Surname"` when the profile carries either; `null` otherwise.
    #[schema(example = "Ada Lovelace")]
    pub display_name: Option<String>,
}

impl PersonRef {
    pub fn new(user: &User) -> Self {
        let full: Vec<&str> = [user.get_name(), user.get_surname()]
            .into_iter()
            .flatten()
            .map(|part| part.as_str())
            .collect();
        Self {
            id: user.get_id().key().to_string(),
            username: user.get_username().as_str().to_string(),
            display_name: (!full.is_empty()).then(|| full.join(" ")),
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
        people.get(id.key()).cloned().unwrap_or_else(|| Self {
            id: id.key().to_string(),
            username: id.key().to_string(),
            display_name: None,
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
    let users = User::list_by_ids(&unique, db).await?;
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
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    pub id: String,
    #[schema(example = "ada")]
    pub username: String,
    pub role: Role,
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
}

impl UserResponse {
    pub fn new(user: &User) -> Self {
        Self {
            id: user.get_id().key().to_string(),
            username: user.get_username().as_str().to_string(),
            role: user.get_role().into(),
            name: user.get_name().map(|v| v.as_str().to_string()),
            surname: user.get_surname().map(|v| v.as_str().to_string()),
            email: user.get_email().map(|v| v.as_str().to_string()),
            phone: user.get_phone().map(|v| v.as_str().to_string()),
            birth_date: user.get_birth_date().map(|v| v.as_str().to_string()),
        }
    }
}

/// Public shape of a course. Shared by `courses` (CRUD) and `marks` (report
/// blocks embed the course they average).
#[derive(Serialize, ToSchema)]
pub struct CourseResponse {
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    pub id: String,
    pub creator: String,
    #[schema(example = "Algebra")]
    pub title: String,
    pub description: String,
}

impl CourseResponse {
    pub fn new(course: &Course) -> Self {
        Self {
            id: course.get_id().key().to_string(),
            creator: course.get_creator().key().to_string(),
            title: course.get_title().as_str().to_string(),
            description: course.get_description().as_str().to_string(),
        }
    }
}

/// Public shape of an exam. Shared by `exams` (CRUD/results) and `courses`
/// (in-course creation and listing). The schedule fields are all `null` for an
/// unscheduled (offline-graded) exam; see the create/update endpoints for the
/// rules tying them together.
#[derive(Serialize, ToSchema)]
pub struct ExamResponse {
    pub id: String,
    pub creator: String,
    pub course: String,
    pub title: String,
    pub description: String,
    pub kind: String,
    pub weight: i64,
    /// `sync` or `async`; `null` for an unscheduled exam.
    #[schema(example = "sync")]
    pub mode: Option<String>,
    /// Window open, UTC unix-milliseconds.
    pub starts_at: Option<i64>,
    /// Window close, UTC unix-milliseconds.
    pub ends_at: Option<i64>,
    /// Per-student time budget in milliseconds (async exams only).
    pub duration_ms: Option<i64>,
}

impl ExamResponse {
    pub fn new(exam: &Exam) -> Self {
        Self {
            id: exam.get_id().key().to_string(),
            creator: exam.get_creator().key().to_string(),
            course: exam.get_course().key().to_string(),
            title: exam.get_title().as_str().to_string(),
            description: exam.get_description().as_str().to_string(),
            kind: exam.get_kind().as_str().to_string(),
            weight: exam.get_weight().as_i64(),
            mode: exam.get_mode().map(|m| m.as_str().to_string()),
            starts_at: exam.get_starts_at().map(|t| t.as_millis()),
            ends_at: exam.get_ends_at().map(|t| t.as_millis()),
            duration_ms: exam.get_duration_ms().map(|d| d.as_millis()),
        }
    }
}
