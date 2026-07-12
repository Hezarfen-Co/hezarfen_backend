//! HTTP layer: axum routers, request/response DTOs (serde), and the auth
//! extractors. DTOs are plain strings on the wire; handlers parse them into
//! validated domain newtypes before doing anything. Role-gated endpoints take a
//! `RequireTeacher` / `RequireAdmin` extractor instead of `CurrentUser`.

pub mod auth;
pub mod courses;
pub mod events;
pub mod exam_ws;
pub mod exams;
pub mod marks;
pub mod notes;
pub mod users;

mod dto;
mod extractor;

pub use dto::{CourseResponse, ExamResponse, PersonRef, UserResponse, person_map};
pub use extractor::{CurrentUser, RequireAdmin, RequireTeacher};

use serde::{Deserialize, Deserializer};

/// Distinguishes an *absent* PATCH field from an explicit `null`: absent never
/// reaches the deserializer and stays `None` via `#[serde(default)]` (keep the
/// stored value); `null` or a value lands here as `Some(inner)` (clear or set).
pub(crate) fn set_or_clear<'de, D, T>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(de).map(Some)
}
