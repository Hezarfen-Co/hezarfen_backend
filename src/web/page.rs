//! Shared pagination: the `?limit=&offset=` query every list endpoint accepts,
//! and the `{items, total, limit, offset}` envelope they return.
//!
//! Paging is opt-in. Omit `limit` and the response still carries every
//! (remaining) row — so a client that ignores the parameters keeps seeing full
//! lists, and callers that genuinely need the whole set are never silently
//! truncated. `total` is always the full, unpaged row count, so a frontend can
//! render "showing 100 of 256" and drive a pager without a second request.
//!
//! The windowing is a slice over the already-ordered domain list (the domain
//! layer still returns the full, sorted `Vec`): endpoints that join people onto
//! their rows do that join over the page alone, so the expensive lookup shrinks
//! with the page even though the base scan does not.

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::constant::MAX_PAGE_LIMIT;
use crate::error::{AppError, ValidationError};

/// The `?limit=&offset=` pair on a list endpoint. Both optional: omit `limit`
/// for every remaining row, omit `offset` to start at the top.
#[derive(Debug, Deserialize, IntoParams)]
pub struct PageParams {
    /// Maximum rows to return. Omit for every remaining row; when supplied it
    /// must be between `1` and `500`.
    #[param(minimum = 1, maximum = 500, example = 100)]
    pub limit: Option<i64>,
    /// Rows to skip from the start of the list. Defaults to `0`; an offset past
    /// the end simply yields an empty page.
    #[param(minimum = 0, example = 0)]
    pub offset: Option<i64>,
}

impl PageParams {
    /// Validate and normalize into `(limit, offset)`. A `limit` outside
    /// `1..=MAX_PAGE_LIMIT` or a negative `offset` is a `400` naming the field.
    pub fn resolve(&self) -> Result<(Option<i64>, i64), AppError> {
        if let Some(limit) = self.limit
            && !(1..=MAX_PAGE_LIMIT).contains(&limit)
        {
            return Err(AppError::Validation(ValidationError::Invalid {
                field: "limit",
                reason: "must be between 1 and 500",
            }));
        }
        let offset = self.offset.unwrap_or(0);
        if offset < 0 {
            return Err(AppError::Validation(ValidationError::Invalid {
                field: "offset",
                reason: "must not be negative",
            }));
        }
        Ok((self.limit, offset))
    }
}

/// One page of a list: the `items`, the full `total` (row count before the
/// window), and the `limit`/`offset` that produced it. `limit` is `null` when
/// the caller asked for every row.
#[derive(Debug, Serialize, ToSchema)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Total rows in the full list, before `limit`/`offset` are applied.
    #[schema(example = 256)]
    pub total: i64,
    /// Echo of the applied `limit`; `null` when the response is unbounded.
    #[schema(example = 100)]
    pub limit: Option<i64>,
    /// Echo of the applied `offset`.
    #[schema(example = 0)]
    pub offset: i64,
}

impl<T> Page<T> {
    pub fn new(items: Vec<T>, total: i64, limit: Option<i64>, offset: i64) -> Self {
        Self {
            items,
            total,
            limit,
            offset,
        }
    }
}

/// The window `[offset, offset + limit)` of `items`, clamped to the ends. An
/// offset at or past the end yields an empty slice — a client paging off the
/// tail gets `[]`, not an error — and a `None` limit runs to the end. `total`
/// stays the caller's job: take `items.len()` before slicing.
pub fn paginate<T>(items: &[T], limit: Option<i64>, offset: i64) -> &[T] {
    let start = (offset.max(0) as usize).min(items.len());
    let end = match limit {
        Some(limit) => start.saturating_add(limit.max(0) as usize).min(items.len()),
        None => items.len(),
    };
    &items[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(limit: Option<i64>, offset: Option<i64>) -> PageParams {
        PageParams { limit, offset }
    }

    #[test]
    fn resolve_accepts_bounds_and_defaults_offset() {
        assert_eq!(params(None, None).resolve().unwrap(), (None, 0));
        assert_eq!(params(Some(1), None).resolve().unwrap(), (Some(1), 0));
        assert_eq!(
            params(Some(MAX_PAGE_LIMIT), Some(40)).resolve().unwrap(),
            (Some(MAX_PAGE_LIMIT), 40)
        );
    }

    #[test]
    fn resolve_rejects_out_of_range_limit_and_negative_offset() {
        assert!(params(Some(0), None).resolve().is_err());
        assert!(params(Some(MAX_PAGE_LIMIT + 1), None).resolve().is_err());
        assert!(params(Some(-5), None).resolve().is_err());
        assert!(params(None, Some(-1)).resolve().is_err());
    }

    #[test]
    fn paginate_windows_and_clamps() {
        let items: Vec<i64> = (0..10).collect();
        // A bounded window in the middle.
        assert_eq!(paginate(&items, Some(3), 2), &[2, 3, 4]);
        // No limit → everything from the offset to the end.
        assert_eq!(paginate(&items, None, 7), &[7, 8, 9]);
        // Limit overruns the end → clamped, not padded.
        assert_eq!(paginate(&items, Some(100), 8), &[8, 9]);
        // Offset at or past the end → empty page, not a panic.
        assert!(paginate(&items, Some(5), 10).is_empty());
        assert!(paginate(&items, Some(5), 999).is_empty());
        // No limit, zero offset → the whole list.
        assert_eq!(paginate(&items, None, 0), items.as_slice());
    }
}
