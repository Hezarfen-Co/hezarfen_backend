use std::collections::HashMap;

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

/// The refusal a mark meets once its kind has left the school's list. The
/// mirror of the settings-side 409: whichever of the two writes reaches the
/// counter row (`kind_ref`) first, the other is told the name is no longer
/// usable. The ref-row key is the kind's own name — a `TEXT` primary key,
/// like the settings singleton.
pub(crate) fn retired_kind_error(kind: &str) -> AppError {
    AppError::ConflictOwned(format!(
        "the '{kind}' exam kind has been removed from the school's settings — \
         add it back before grading this exam"
    ))
}

/// The identity of one (exam, user, seq) triple — one mark row per sitting,
/// so grading is a single atomic UPSERT on the composite primary key with no
/// find-then-insert race. See [`key::sitting`] for the wire shape and why the
/// first sitting stays bare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExamResultId {
    pub(crate) exam: ExamId,
    pub(crate) user: UserId,
    pub(crate) seq: i64,
}

impl ExamResultId {
    pub fn composite(exam: &ExamId, user: &UserId, seq: i64) -> Self {
        Self {
            exam: exam.clone(),
            user: *user,
            seq,
        }
    }

    /// The underscore-joined wire form (`{exam}_{user}[_{seq}]`).
    pub fn key(&self) -> String {
        key::sitting(self.exam.key().as_str(), self.user.key().as_str(), self.seq)
    }
}

/// A validated exam mark. Stored as a `BIGINT`, held to
/// `[MIN_MARK, MAX_MARK]`.
/// Kept in its own row — an exam result is never a course note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
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

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExamResult {
    pub(crate) exam: ExamId,
    #[sqlx(rename = "app_user")]
    pub(crate) user: UserId,
    pub(crate) seq: i64,
    pub(crate) mark: Mark,
    pub(crate) graded_by: UserId,
}

impl ExamResult {
    /// The row's identity, built back from its primary-key columns.
    pub fn get_id(&self) -> ExamResultId {
        ExamResultId::composite(&self.exam, &self.user, self.seq)
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
/// in Rust rather than SQL: it keeps the outer ordering stable without a
/// window-function dance, and id order can't stand in for seq order once seq
/// reaches two digits.
pub(crate) fn latest_per_pair(rows: Vec<ExamResult>) -> Vec<ExamResult> {
    let mut order: Vec<(String, String)> = Vec::new();
    let mut best: HashMap<(String, String), ExamResult> = HashMap::new();
    for row in rows {
        let key = (row.exam.key(), row.user.key());
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

    /// Grade-of-record: a pair with retakes collapses to its highest seq, and
    /// the outer list keeps its row order.
    #[tokio::test]
    async fn latest_per_pair_keeps_the_highest_seq_in_first_seen_order() {
        let exam_of = |n: u16| ExamId::from_key(&format!("0198f1a2-0000-7000-8000-{n:012x}"));
        let user_of = |n: u16| UserId::from_key(&format!("0198f1a2-1111-7000-8000-{n:012x}"));
        let (exam, user, other) = (exam_of(1), user_of(1), user_of(2));
        let row = |user: UserId, seq: i64, mark: i64| ExamResult {
            exam: exam.clone(),
            user,
            seq,
            mark: Mark::try_new(mark).unwrap(),
            graded_by: user_of(9),
        };

        let got = latest_per_pair(vec![
            row(user, 1, 60),
            row(other, 1, 70),
            row(user, 2, 85),  // retake outranks the seq-1 mark
            row(other, 3, 40), // third sitting of the second pair
        ]);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].get_user(), &user);
        assert_eq!(got[0].get_seq(), 2);
        assert_eq!(got[0].get_mark().as_i64(), 85);
        assert_eq!(got[1].get_user(), &other);
        assert_eq!(got[1].get_seq(), 3);
        assert_eq!(got[1].get_mark().as_i64(), 40);
    }
}
