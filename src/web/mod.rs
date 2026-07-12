//! HTTP layer: axum routers, request/response DTOs (serde), and the auth
//! extractors. DTOs are plain strings on the wire; handlers parse them into
//! validated domain newtypes before doing anything. Role-gated endpoints take a
//! `RequireTeacher` / `RequireAdmin` extractor instead of `CurrentUser`.

pub mod attendance;
pub mod auth;
pub mod courses;
pub mod events;
pub mod exam_ws;
pub mod exams;
pub mod marks;
pub mod notes;
pub mod sessions;
pub mod users;
pub mod work;

mod dto;
mod extractor;

pub use dto::{CourseResponse, ExamResponse, PersonRef, SessionResponse, UserResponse, person_map};
pub use extractor::{CurrentUser, RequireAdmin, RequireManager, RequireTeacher};

use serde::{Deserialize, Deserializer};

use crate::domain::timestamp::Timestamp;
use crate::error::{AppError, ValidationError};

/// If both ends are present, `ends_at` must not precede `starts_at`. Shared by
/// everything that carries a time range (events, course sessions).
pub(crate) fn check_time_range(
    starts_at: Option<Timestamp>,
    ends_at: Option<Timestamp>,
) -> Result<(), AppError> {
    if let (Some(starts), Some(ends)) = (starts_at, ends_at)
        && ends < starts
    {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "ends_at",
            reason: "must be at or after starts_at",
        }));
    }
    Ok(())
}

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
