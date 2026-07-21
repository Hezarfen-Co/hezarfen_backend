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
    ///   previous answers are wiped in the same transaction that creates the
    ///   new row ([`Self::wipe_and_create`]), so a lost race (or a failed
    ///   create) rolls the wipe back — no answer sheet is ever destroyed
    ///   without its retake existing.
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
        let attempt = ExamAttempt {
            id: ExamAttemptId::composite(exam.get_id(), user, next_seq),
            exam: exam.get_id().clone(),
            user: user.clone(),
            seq: next_seq,
            started_at: Timestamp::now(),
            finished_at: None,
            left_at: None,
        };
        let created: Result<Option<ExamAttempt>, surrealdb::Error> = if next_seq == 1 {
            db.create(attempt.id.record()).content(attempt).await
        } else {
            Self::wipe_and_create(attempt, db).await
        };
        match created {
            Ok(Some(created)) => Ok((created, true)),
            Ok(None) => Err(AppError::Internal("failed to start exam attempt".into())),
            // Only a still-running row proves the loss was a double-start
            // collision (the winner's fresh sitting); a terminal or missing
            // latest means the create genuinely failed — surface that instead
            // of passing a finished sitting off as a resume.
            Err(err) => match Self::read_latest_for_user(exam.get_id(), user, db).await? {
                Some(existing)
                    if existing.status(exam, Timestamp::now()) == AttemptStatus::InProgress =>
                {
                    Ok((existing, false))
                }
                _ => Err(err.into()),
            },
        }
    }

    /// Wipe the student's previous answers and create the retake row in one
    /// transaction. Atomicity is the point: a create that fails (a concurrent
    /// double-start lost the race on the composite id, or the database
    /// hiccuped) cancels the whole transaction, wipe included — otherwise a
    /// stale loser could delete answers freshly saved into the winner's
    /// sitting, or destroy a graded sheet without a retake ever existing.
    async fn wipe_and_create(
        attempt: ExamAttempt,
        db: &Database,
    ) -> Result<Option<ExamAttempt>, surrealdb::Error> {
        let mut result = db
            .query(
                "BEGIN TRANSACTION;
                 DELETE exam_answer WHERE exam = $ex AND user = $usr;
                 DELETE answer_image WHERE exam = $ex AND user = $usr;
                 CREATE $id CONTENT $attempt;
                 COMMIT TRANSACTION;",
            )
            .bind(("ex", attempt.exam.record()))
            .bind(("usr", attempt.user.record()))
            .bind(("id", attempt.id.record()))
            .bind(("attempt", attempt))
            .await?
            .check()?;
        // Statement slots count BEGIN and COMMIT too, plus the two child
        // wipes: the CREATE is slot 3. (The answer-image *blobs* are the web
        // layer's to GC — `start_attempt` collects their names before this.)
        Ok(result.take::<Vec<ExamAttempt>>(3)?.into_iter().next())
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
    ///
    /// Writes *only* the `left_at` field (never the whole row): the room's
    /// teardown reads the attempt, sees it in progress, then stamps here — and
    /// a submission that lands in that gap must survive. A whole-row write from
    /// the pre-read snapshot would carry its blank `finished_at` back over the
    /// fresh submission, silently un-submitting the exam (and, with
    /// `allow_rejoin` off, locking the student out).
    pub async fn set_left(
        self,
        left_at: Option<Timestamp>,
        db: &Database,
    ) -> Result<ExamAttempt, AppError> {
        let mut result = db
            .query("UPDATE $id SET left_at = $left RETURN AFTER")
            .bind(("id", self.id.record()))
            .bind(("left", left_at))
            .await?
            .check()?;
        result
            .take::<Vec<ExamAttempt>>(0)?
            .into_iter()
            .next()
            .ok_or(AppError::NotFound)
    }

    /// One sitting by id — the exam room re-reads its own attempt this way,
    /// so a retake started elsewhere can never be mistaken for it.
    pub async fn read(id: &ExamAttemptId, db: &Database) -> Result<Option<ExamAttempt>, AppError> {
        Ok(db.select(id.record()).await?)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_mem;
    use crate::domain::course::CourseId;
    use crate::domain::exam::{
        ExamAttemptLimit, ExamDescription, ExamKind, ExamMode, ExamSchedule, ExamTitle,
    };
    use crate::domain::exam_answer::ExamAnswer;
    use crate::domain::exam_question::{ExamQuestion, QuestionKind, QuestionPoints, QuestionSpec};
    use crate::domain::settings::Settings;

    /// An open exam with retakes allowed, plus one choice question — enough
    /// rows to exercise the attempt lifecycle without the HTTP layer.
    async fn open_exam_with_question(db: &Database, max_attempts: i64) -> (Exam, ExamQuestion) {
        let creator = UserId::from_key("01TESTTEACHERAAAAAAAAAAAAA");
        let course = CourseId::from_key("01TESTCOURSEAAAAAAAAAAAAAA");
        let kinds = Settings::defaults().get_exam_kinds().to_vec();
        let exam = Exam::create(
            &creator,
            &course,
            ExamTitle::try_new("practice").unwrap(),
            ExamDescription::try_new("").unwrap(),
            ExamKind::try_new("quiz", &kinds).unwrap(),
            ExamSchedule::try_new(Some(ExamMode::try_new("open").unwrap()), None, None, None)
                .unwrap(),
            ExamAttemptLimit::try_new(max_attempts).unwrap(),
            true,
            false,
            db,
        )
        .await
        .unwrap();
        let spec = QuestionSpec::try_new(
            QuestionKind::try_new("choice").unwrap(),
            Some(vec!["5".into(), "6".into()]),
            Some(1),
        )
        .unwrap();
        let question = ExamQuestion::create(
            exam.get_id(),
            crate::domain::subject::SubjectId::generate(),
            crate::domain::exam_question::QuestionText::try_new("3 + 3?").unwrap(),
            QuestionPoints::try_new(5).unwrap(),
            spec,
            db,
        )
        .await
        .unwrap();
        (exam, question)
    }

    fn student() -> UserId {
        UserId::from_key("01TESTSTUDENTAAAAAAAAAAAAA")
    }

    #[tokio::test]
    async fn a_retake_starts_from_a_blank_sheet() {
        let db = init_mem().await.unwrap();
        let (exam, question) = open_exam_with_question(&db, 2).await;
        let user = student();

        let (first, created) = ExamAttempt::start(&exam, &user, &db).await.unwrap();
        assert!(created);
        assert_eq!(first.get_seq(), 1);
        ExamAnswer::save(&question, &user, Some(1), None, &db)
            .await
            .unwrap();
        first.finish(&db).await.unwrap();

        // The retake lands as sitting #2 with the sheet wiped in the same
        // transaction that created it.
        let (second, created) = ExamAttempt::start(&exam, &user, &db).await.unwrap();
        assert!(created);
        assert_eq!(second.get_seq(), 2);
        let answers = ExamAnswer::list_for_exam_user(exam.get_id(), &user, &db)
            .await
            .unwrap();
        assert!(answers.is_empty(), "a retake starts blank");
    }

    #[tokio::test]
    async fn a_lost_retake_race_cannot_wipe_the_winners_sheet() {
        let db = init_mem().await.unwrap();
        let (exam, question) = open_exam_with_question(&db, 3).await;
        let user = student();

        // Sitting #1 ends; the winner starts sitting #2 and saves an answer.
        let (first, _) = ExamAttempt::start(&exam, &user, &db).await.unwrap();
        first.finish(&db).await.unwrap();
        let (winner, created) = ExamAttempt::start(&exam, &user, &db).await.unwrap();
        assert!(created);
        assert_eq!(winner.get_seq(), 2);
        ExamAnswer::save(&question, &user, Some(1), None, &db)
            .await
            .unwrap();

        // A stale double-start races on the same seq and loses to the
        // composite id — and the aborted transaction must roll its wipe back,
        // leaving the winner's fresh answer untouched.
        let loser = ExamAttempt {
            id: ExamAttemptId::composite(exam.get_id(), &user, 2),
            exam: exam.get_id().clone(),
            user: user.clone(),
            seq: 2,
            started_at: Timestamp::now(),
            finished_at: None,
            left_at: None,
        };
        let lost = ExamAttempt::wipe_and_create(loser, &db).await;
        assert!(lost.is_err(), "the duplicate create must fail");
        let answers = ExamAnswer::list_for_exam_user(exam.get_id(), &user, &db)
            .await
            .unwrap();
        assert_eq!(
            answers.len(),
            1,
            "the lost race must not wipe the winner's saved answer"
        );

        // The public path shrugs the race off: a re-start resumes the winner.
        let (resumed, created) = ExamAttempt::start(&exam, &user, &db).await.unwrap();
        assert!(!created);
        assert_eq!(resumed.get_seq(), 2);
    }

    #[tokio::test]
    async fn stamping_left_never_reverts_a_submission() {
        let db = init_mem().await.unwrap();
        let (exam, _question) = open_exam_with_question(&db, 1).await;
        let user = student();

        // The student is sitting the exam.
        let (attempt, _) = ExamAttempt::start(&exam, &user, &db).await.unwrap();
        assert_eq!(attempt.get_seq(), 1);

        // The exam-room teardown reads the attempt while it is still in
        // progress (`stamp_left`'s read) — a snapshot with `finished_at` unset.
        let stale = ExamAttempt::read(attempt.get_id(), &db)
            .await
            .unwrap()
            .expect("attempt exists");
        assert!(stale.get_finished_at().is_none());

        // Before the teardown writes, the student submits (a REST finish, or a
        // finish over another socket) — the submission lands.
        let finished = ExamAttempt::read(attempt.get_id(), &db)
            .await
            .unwrap()
            .expect("attempt exists")
            .finish(&db)
            .await
            .unwrap();
        assert!(finished.get_finished_at().is_some());

        // Now the last socket's teardown stamps `left_at` from its stale
        // snapshot. Stamping the walk-out must touch only `left_at` — it must
        // not carry the snapshot's blank `finished_at` back over the fresh
        // submission, or the student's submitted exam silently reopens (and,
        // with `allow_rejoin` off, locks them out of a room they left after
        // finishing).
        stale.set_left(Some(Timestamp::now()), &db).await.unwrap();

        let after = ExamAttempt::read(attempt.get_id(), &db)
            .await
            .unwrap()
            .expect("attempt still exists");
        assert!(
            after.get_finished_at().is_some(),
            "stamping left_at must not revert a submission that raced it"
        );
        assert!(
            after.get_left_at().is_some(),
            "the walk-out is still recorded"
        );
        assert_eq!(
            after.status(&exam, Timestamp::now()),
            AttemptStatus::Submitted,
            "the attempt stays submitted"
        );
    }
}
