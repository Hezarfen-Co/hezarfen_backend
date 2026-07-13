use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::database::{Database, EXAM_ATTEMPT_TABLE};
use crate::domain::exam::Exam;
use crate::domain::exam::ExamId;
use crate::domain::exam_answer::ExamAnswer;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ExamAttemptId(RecordId);

impl ExamAttemptId {
    /// A deterministic id for the (exam, user, seq) triple. The same triple
    /// always maps to the same record id, so sitting `seq` exists at most once
    /// by construction — a concurrent double "start" races on the same id and
    /// exactly one create wins. The first sitting keeps the historical
    /// `{exam}_{user}` shape (rows written before retakes existed stay
    /// addressable); later sittings append their number. ULID keys are
    /// alphanumeric, so `_` is an unambiguous joiner.
    pub fn composite(exam: &ExamId, user: &UserId, seq: i64) -> Self {
        let key = if seq == 1 {
            format!("{}_{}", exam.key(), user.key())
        } else {
            format!("{}_{}_{}", exam.key(), user.key(), seq)
        };
        Self(RecordId::new(EXAM_ATTEMPT_TABLE, key))
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
#[derive(Debug, Clone, SurrealValue)]
pub struct ExamAttempt {
    id: ExamAttemptId,
    exam: ExamId,
    user: UserId,
    seq: i64,
    started_at: Timestamp,
    finished_at: Option<Timestamp>,
    left_at: Option<Timestamp>,
}

impl ExamAttempt {
    pub fn get_id(&self) -> &ExamAttemptId {
        &self.id
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

    /// Start (or resume) `user`'s attempt at `exam`. Returns the attempt plus
    /// whether it was newly created.
    ///
    /// - A still-running latest attempt is returned untouched, so a
    ///   reconnecting client gets its original clock back instead of a reset —
    ///   re-"starting" can never buy more time.
    /// - A terminal latest attempt (submitted or expired) starts sitting
    ///   `seq + 1` if the exam's `max_attempts` allows another, and is a
    ///   conflict otherwise. A retake begins from a blank sheet: the student's
    ///   previous answers are wiped first. Wiping *before* creating keeps the
    ///   race benign — until the new row exists, saves still hit the terminal
    ///   attempt and are rejected, so no fresh answer can be lost to the wipe.
    ///
    /// The composite id makes each create atomic; a concurrent double-start
    /// races on the same seq, loses to the unique id, and reads the winner's
    /// row.
    pub async fn start(
        exam: &Exam,
        user: &UserId,
        db: &Database,
    ) -> Result<(ExamAttempt, bool), AppError> {
        let attempts = Self::list_for_user(exam.get_id(), user, db).await?;
        let next_seq = match attempts.first() {
            None => 1,
            Some(latest) if latest.status(exam, Timestamp::now()) == AttemptStatus::InProgress => {
                return Ok((latest.clone(), false));
            }
            Some(latest) => {
                if !exam.get_max_attempts().allows_another(attempts.len()) {
                    return Err(AppError::Conflict(
                        "no attempts remaining — this exam's attempt limit is used up",
                    ));
                }
                latest.seq + 1
            }
        };
        if next_seq > 1 {
            ExamAnswer::delete_for_exam_user(exam.get_id(), user, db).await?;
        }
        let attempt = ExamAttempt {
            id: ExamAttemptId::composite(exam.get_id(), user, next_seq),
            exam: exam.get_id().clone(),
            user: user.clone(),
            seq: next_seq,
            started_at: Timestamp::now(),
            finished_at: None,
            left_at: None,
        };
        let created: Result<Option<ExamAttempt>, surrealdb::Error> =
            db.create(attempt.id.record()).content(attempt).await;
        match created {
            Ok(Some(created)) => Ok((created, true)),
            Ok(None) => Err(AppError::Internal("failed to start exam attempt".into())),
            Err(err) => match Self::read_latest_for_user(exam.get_id(), user, db).await? {
                Some(existing) => Ok((existing, false)),
                None => Err(err.into()),
            },
        }
    }

    /// Stamp the submission time. The caller has already checked the deadline
    /// and that the attempt isn't finished.
    pub async fn finish(mut self, db: &Database) -> Result<ExamAttempt, AppError> {
        self.finished_at = Some(Timestamp::now());
        let updated: Option<ExamAttempt> = db.update(self.id.record()).content(self).await?;
        updated.ok_or(AppError::NotFound)
    }

    /// Stamp (or clear) the walked-out marker. The exam room sets it when the
    /// student's socket closes mid-attempt and clears it when they come back.
    pub async fn set_left(
        mut self,
        left_at: Option<Timestamp>,
        db: &Database,
    ) -> Result<ExamAttempt, AppError> {
        self.left_at = left_at;
        let updated: Option<ExamAttempt> = db.update(self.id.record()).content(self).await?;
        updated.ok_or(AppError::NotFound)
    }

    /// The student's current sitting — the highest `seq` for the pair. All
    /// reads that used to mean "the attempt" mean this now.
    pub async fn read_latest_for_user(
        exam: &ExamId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<ExamAttempt>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM exam_attempt WHERE exam = $ex AND user = $usr
                 ORDER BY seq DESC LIMIT 1",
            )
            .bind(("ex", exam.record()))
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<ExamAttempt>>(0)?.into_iter().next())
    }

    /// Every sitting of `user` at `exam`, newest first.
    pub async fn list_for_user(
        exam: &ExamId,
        user: &UserId,
        db: &Database,
    ) -> Result<Vec<ExamAttempt>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM exam_attempt WHERE exam = $ex AND user = $usr
                 ORDER BY seq DESC",
            )
            .bind(("ex", exam.record()))
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<ExamAttempt>>(0)?)
    }

    pub async fn list_for_exam(exam: &ExamId, db: &Database) -> Result<Vec<ExamAttempt>, AppError> {
        let mut result = db
            .query("SELECT * FROM exam_attempt WHERE exam = $ex ORDER BY id DESC")
            .bind(("ex", exam.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<ExamAttempt>>(0)?)
    }

    /// Whether anyone has started this exam — the gate that freezes `mode`
    /// edits once an attempt exists.
    pub async fn any_for_exam(exam: &ExamId, db: &Database) -> Result<bool, AppError> {
        let mut result = db
            .query("SELECT VALUE id FROM exam_attempt WHERE exam = $ex LIMIT 1")
            .bind(("ex", exam.record()))
            .await?
            .check()?;
        Ok(!result.take::<Vec<RecordId>>(0)?.is_empty())
    }
}
