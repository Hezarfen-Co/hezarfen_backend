//! HTTP layer: axum routers, request/response DTOs (serde), and the auth
//! extractors. DTOs are plain strings on the wire; handlers parse them into
//! validated domain newtypes before doing anything. Role-gated endpoints take a
//! `RequireTeacher` / `RequireAdmin` extractor instead of `CurrentUser`.

pub mod auth;
pub mod courses;
pub mod events;
pub mod exams;
pub mod marks;
pub mod notes;
pub mod users;

mod dto;
mod extractor;

pub use dto::{CourseResponse, ExamResponse, UserResponse};
pub use extractor::{CurrentUser, RequireAdmin, RequireTeacher};
