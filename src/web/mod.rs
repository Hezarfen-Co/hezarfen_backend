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

use crate::constant::SCHEDULE_PAST_GRACE_MS;
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

/// A schedule instant a request explicitly sets must not lie in the past —
/// nothing can be scheduled to start (or end) before now. Applies only to
/// values the request itself carries: stored values merged into a PATCH are
/// exempt, because a running exam's `starts_at` is legitimately past and a
/// title-only edit must not trip over it. `SCHEDULE_PAST_GRACE_MS` keeps
/// "starts now" from failing by the width of its own round trip.
pub(crate) fn check_not_past(
    field: &'static str,
    value: Option<Timestamp>,
) -> Result<(), AppError> {
    if let Some(value) = value
        && value.as_millis() < Timestamp::now().as_millis() - SCHEDULE_PAST_GRACE_MS
    {
        return Err(AppError::Validation(ValidationError::Invalid {
            field,
            reason: "must not be in the past",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn check_time_range_orders_the_ends() {
        let at = |ms| Some(Timestamp::from_millis(ms));
        assert!(check_time_range(at(1), at(2)).is_ok());
        assert!(check_time_range(at(1), at(1)).is_ok());
        assert!(check_time_range(at(2), at(1)).is_err());
        // A missing end never fails the range check.
        assert!(check_time_range(None, at(1)).is_ok());
        assert!(check_time_range(at(1), None).is_ok());
        assert!(check_time_range(None, None).is_ok());
    }

    #[tokio::test]
    async fn check_not_past_rejects_backdating_but_keeps_the_grace() {
        let now = Timestamp::now().as_millis();
        // Absent and future values pass.
        assert!(check_not_past("starts_at", None).is_ok());
        assert!(check_not_past("starts_at", Some(Timestamp::from_millis(now + 60_000))).is_ok());
        // Inside the grace passes — "starts now" survives its round trip.
        assert!(
            check_not_past(
                "starts_at",
                Some(Timestamp::from_millis(now - SCHEDULE_PAST_GRACE_MS / 2)),
            )
            .is_ok()
        );
        // Beyond the grace is a validation error naming the field.
        let past = Timestamp::from_millis(now - SCHEDULE_PAST_GRACE_MS - 60_000);
        assert!(matches!(
            check_not_past("ends_at", Some(past)),
            Err(AppError::Validation(ValidationError::Invalid {
                field: "ends_at",
                ..
            }))
        ));
    }
}
