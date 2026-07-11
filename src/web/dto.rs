//! Response DTOs shared across more than one handler module.

use serde::Serialize;
use utoipa::ToSchema;

use crate::domain::course::Course;
use crate::domain::exam::Exam;
use crate::domain::role::Role as DomainRole;
use crate::domain::user::User;

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
/// (in-course creation and listing).
#[derive(Serialize, ToSchema)]
pub struct ExamResponse {
    pub id: String,
    pub creator: String,
    pub course: String,
    pub title: String,
    pub description: String,
    pub kind: String,
    pub weight: i64,
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
        }
    }
}
