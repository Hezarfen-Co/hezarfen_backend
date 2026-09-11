use std::collections::HashMap;

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{EXAM_RESULT_TABLE, KIND_REF_TABLE};
use crate::domain::exam::ExamId;
use crate::domain::key;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_mark;

/// The one spelling of "this exam is hidden", shared by the grading
/// pre-flight gate ([`crate::service::exam_result::grade`]) and the
/// in-transaction guard on the mark write.
pub(crate) fn draft_error() -> AppError {
    AppError::Conflict("this exam is a draft — publish it before grading")
}

/// The reference counter for one exam kind — how many marks are written under
/// that name, and whether the school has retired it (see
/// [`crate::db::cap`]). Keyed by the name itself: the kind is snapshotted
/// text on the exam, and this row is what makes "a kind nothing is graded under
/// may be removed" a decision the database takes, not a count a concurrent mark
/// can invalidate.
pub(crate) fn kind_ref(kind: &str) -> RecordId {
    RecordId::new(KIND_REF_TABLE, kind)
}

/// The refusal a mark meets once its kind has left the school's list. The
/// mirror of the settings-side 409: whichever of the two writes reaches the
/// counter first, the other is told the name is no longer usable.
pub(crate) fn retired_kind_error(kind: &str) -> AppError {
    AppError::ConflictOwned(format!(
        "the '{kind}' exam kind has been removed from the school's settings — \
         add it back before grading this exam"
    ))
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ExamResultId(RecordId);

impl ExamResultId {
    /// A deterministic id for the (exam, user, seq) triple — one mark row per
    /// sitting, so grading is a single atomic UPSERT with no find-then-insert
    /// race. See [`key::sitting`] for the key shape and why the first sitting
    /// stays bare.
    pub fn composite(exam: &ExamId, user: &UserId, seq: i64) -> Self {
        let key = key::sitting(exam.key(), user.key(), seq);
        Self(RecordId::new(EXAM_RESULT_TABLE, key))
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

/// A validated exam mark. Stored as an `int`, held to `[MIN_MARK, MAX_MARK]`.
/// Kept in its own row — an exam result is never a course note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, SurrealValue)]
pub struct Mark(i64);

impl Mark {
    pub fn try_new(value: i64) -> Result<Self, ValidationError> {
        validate_mark(value)?;
        Ok(Self(value))
    }

    pub fn as_i64(&self) -> i64 {
        self.0
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct ExamResult {
    pub(crate) id: ExamResultId,
    pub(crate) exam: ExamId,
    pub(crate) user: UserId,
    pub(crate) seq: i64,
    pub(crate) mark: Mark,
    pub(crate) graded_by: UserId,
}

impl ExamResult {
    pub fn get_id(&self) -> &ExamResultId {
        &self.id
    }

    pub fn get_exam(&self) -> &ExamId {
        &self.exam
    }

    pub fn get_user(&self) -> &UserId {
        &self.user
    }

    /// Which sitting this mark grades — 1 for the first attempt, counting up.
    pub fn get_seq(&self) -> i64 {
        self.seq
    }

    pub fn get_mark(&self) -> Mark {
        self.mark
    }

    pub fn get_graded_by(&self) -> &UserId {
        &self.graded_by
    }
}

/// The latest-seq mark per (exam, user) pair, preserving the outer list's
/// row order (first appearance of each pair). Grade-of-record is the latest
/// attempt's mark, so a pair with retakes collapses to its highest seq. Done
/// in Rust rather than SQL: SurrealDB's `GROUP BY` can't return the whole
/// row that carries the max, and id-order can't stand in for seq-order once
/// seq reaches two digits.
pub(crate) fn latest_per_pair(rows: Vec<ExamResult>) -> Vec<ExamResult> {
    let mut order: Vec<(String, String)> = Vec::new();
    let mut best: HashMap<(String, String), ExamResult> = HashMap::new();
    for row in rows {
        let key = (row.exam.key().to_string(), row.user.key().to_string());
        match best.get(&key) {
            Some(existing) if existing.seq >= row.seq => {}
            Some(_) => {
                best.insert(key, row);
            }
            None => {
                order.push(key.clone());
                best.insert(key, row);
            }
        }
    }
    order
        .into_iter()
        .map(|key| best.remove(&key).expect("key was recorded on insert"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mark_range_is_enforced() {
        assert_eq!(Mark::try_new(0).unwrap().as_i64(), 0);
        assert_eq!(Mark::try_new(100).unwrap().as_i64(), 100);
        assert!(Mark::try_new(-1).is_err());
        assert!(Mark::try_new(101).is_err());
    }
}
