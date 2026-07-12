use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::database::{Database, EXAM_ATTEMPT_TABLE};
use crate::domain::exam::Exam;
use crate::domain::exam::ExamId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ExamAttemptId(RecordId);

impl ExamAttemptId {
    /// A deterministic id for the (exam, user) pair. The same pair always maps
    /// to the same record id, so a student gets exactly one attempt per exam by
    /// construction — a second "start" cannot mint a fresh clock. ULID keys are
    /// alphanumeric, so `_` is an unambiguous joiner.
    pub fn composite(exam: &ExamId, user: &UserId) -> Self {
        Self(RecordId::new(
            EXAM_ATTEMPT_TABLE,
            format!("{}_{}", exam.key(), user.key()),
        ))
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

/// One student's sitting of a scheduled exam: starting is the live-attendance
/// signal, `finished_at` the submission. Grading stays a separate
/// `exam_result` row.
#[derive(Debug, Clone, SurrealValue)]
pub struct ExamAttempt {
    id: ExamAttemptId,
    exam: ExamId,
    user: UserId,
    started_at: Timestamp,
    finished_at: Option<Timestamp>,
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

    pub fn get_started_at(&self) -> Timestamp {
        self.started_at
    }

    pub fn get_finished_at(&self) -> Option<Timestamp> {
        self.finished_at
    }

    /// When this attempt closes, computed from the exam's *current* schedule
    /// (never stored): `ends_at` for a sync exam, `min(started_at +
    /// duration_ms, ends_at)` for an async one. Recomputing on every read means
    /// a teacher extending `ends_at` (or an async `duration_ms`) mid-exam moves
    /// every deadline live. `None` only if the exam is unscheduled, which
    /// starting an attempt forbids.
    pub fn deadline(&self, exam: &Exam) -> Option<Timestamp> {
        let ends_at = exam.get_ends_at()?;
        Some(match exam.get_duration_ms() {
            Some(duration) => ends_at.min(Timestamp::from_millis(
                self.started_at
                    .as_millis()
                    .saturating_add(duration.as_millis()),
            )),
            None => ends_at,
        })
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
    /// whether it was newly created: an existing row is returned untouched, so
    /// a reconnecting client gets its original clock back instead of a reset —
    /// re-"starting" can never buy more time. The composite id makes the
    /// create atomic; a concurrent double-start loses to the unique id and
    /// reads the winner's row.
    pub async fn start(
        exam: &ExamId,
        user: &UserId,
        db: &Database,
    ) -> Result<(ExamAttempt, bool), AppError> {
        if let Some(existing) = Self::read_for_user(exam, user, db).await? {
            return Ok((existing, false));
        }
        let attempt = ExamAttempt {
            id: ExamAttemptId::composite(exam, user),
            exam: exam.clone(),
            user: user.clone(),
            started_at: Timestamp::now(),
            finished_at: None,
        };
        let created: Result<Option<ExamAttempt>, surrealdb::Error> =
            db.create(attempt.id.record()).content(attempt).await;
        match created {
            Ok(Some(created)) => Ok((created, true)),
            Ok(None) => Err(AppError::Internal("failed to start exam attempt".into())),
            Err(err) => match Self::read_for_user(exam, user, db).await? {
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

    pub async fn read_for_user(
        exam: &ExamId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<ExamAttempt>, AppError> {
        Ok(db
            .select(ExamAttemptId::composite(exam, user).record())
            .await?)
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
