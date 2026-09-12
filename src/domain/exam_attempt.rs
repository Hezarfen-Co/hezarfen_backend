use crate::domain::exam::Exam;
use crate::domain::exam::ExamId;
use crate::domain::key;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;

/// The identity of one (exam, user, seq) triple, so sitting `seq` exists at
/// most once by construction — a concurrent double "start" races on the same
/// primary key and exactly one insert wins. See [`key::sitting`] for the wire
/// shape and why the first sitting stays bare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExamAttemptId {
    pub(crate) exam: ExamId,
    pub(crate) user: UserId,
    pub(crate) seq: i64,
}

impl ExamAttemptId {
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

/// Where an attempt stands right now, judged against the server clock. Derived
/// on read — never stored, so it can't go stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptStatus {
    /// Started, not submitted, deadline not reached.
    InProgress,
    /// The student declared themselves done before the deadline.
    Submitted,
    /// The deadline passed without a submission — a valid terminal state
    /// (the student used their full time), not an error.
    Expired,
}

impl AttemptStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AttemptStatus::InProgress => "in_progress",
            AttemptStatus::Submitted => "submitted",
            AttemptStatus::Expired => "expired",
        }
    }
}

/// One student's sitting of an exam: starting is the live-attendance signal,
/// `finished_at` the submission. `seq` numbers the sittings (1, 2, …) when the
/// exam allows retakes; `left_at` marks a student who walked out of the exam
/// room mid-attempt. Grading stays a separate `exam_result` row.
///
/// Fields are crate-visible: [`crate::db::exam_attempt`] targets its
/// field-scoped writes by the primary-key triple and
/// [`crate::service::exam_attempt::start`] mints the rows on create.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExamAttempt {
    pub(crate) exam: ExamId,
    #[sqlx(rename = "app_user")]
    pub(crate) user: UserId,
    pub(crate) seq: i64,
    pub(crate) started_at: Timestamp,
    pub(crate) finished_at: Option<Timestamp>,
    pub(crate) left_at: Option<Timestamp>,
}

impl ExamAttempt {
    /// The row's identity, built back from its primary-key columns.
    pub fn get_id(&self) -> ExamAttemptId {
        ExamAttemptId::composite(&self.exam, &self.user, self.seq)
    }

    pub fn get_exam(&self) -> &ExamId {
        &self.exam
    }

    pub fn get_user(&self) -> &UserId {
        &self.user
    }

    /// Which sitting this is — 1 for the first attempt, counting up.
    pub fn get_seq(&self) -> i64 {
        self.seq
    }

    pub fn get_started_at(&self) -> Timestamp {
        self.started_at
    }

    pub fn get_finished_at(&self) -> Option<Timestamp> {
        self.finished_at
    }

    /// When the student left the exam room (WebSocket closed mid-attempt);
    /// `None` while they're in it, or if they never used the room. Gates
    /// nothing by itself — the exam's `allow_rejoin` decides what it means.
    pub fn get_left_at(&self) -> Option<Timestamp> {
        self.left_at
    }

    /// When this attempt closes, computed from the exam's *current* schedule
    /// (never stored): the earlier of the window's `ends_at` and `started_at +
    /// duration_ms`, whichever of the two exists. Recomputing on every read
    /// means a teacher extending `ends_at` (or `duration_ms`) mid-exam moves
    /// every deadline live. `None` for an open exam without a duration — such
    /// an attempt only ends by submission.
    pub fn deadline(&self, exam: &Exam) -> Option<Timestamp> {
        let by_duration = exam.get_duration_ms().map(|duration| {
            Timestamp::from_millis(
                self.started_at
                    .as_millis()
                    .saturating_add(duration.as_millis()),
            )
        });
        match (exam.get_ends_at(), by_duration) {
            (Some(ends_at), Some(cap)) => Some(ends_at.min(cap)),
            (Some(ends_at), None) => Some(ends_at),
            (None, Some(cap)) => Some(cap),
            (None, None) => None,
        }
    }

    /// Status judged at `now` (one clock read per snapshot, shared across rows).
    pub fn status(&self, exam: &Exam, now: Timestamp) -> AttemptStatus {
        if self.finished_at.is_some() {
            return AttemptStatus::Submitted;
        }
        match self.deadline(exam) {
            Some(deadline) if now >= deadline => AttemptStatus::Expired,
            _ => AttemptStatus::InProgress,
        }
    }
}
