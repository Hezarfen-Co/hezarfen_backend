use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{EXAM_ATTEMPT_TABLE, EXAM_RESULT_COUNT_FIELD, EXAM_SAT_TOTAL_FIELD};
use crate::database::{Database, transaction_with_retry};
use crate::domain::exam::Exam;
use crate::domain::exam::ExamId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::db::cap;
use crate::domain::{badge, key};
use crate::error::AppError;

/// The `THROW` marker the freeze gate aborts with, and the one 409 both it and
/// the handler's pre-flight check answer with — a client cannot tell which of
/// the two refused.
const FROZEN_MARK: &str = "questions_frozen";

/// The `THROW` marker the exam-existence touch aborts with — the exam row this
/// write hangs off is gone, so the write is a 404 and nothing lands.
const GONE_MARK: &str = "no_exam";

pub(crate) fn frozen_error() -> AppError {
    AppError::Conflict("cannot change questions after attempts have started")
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ExamAttemptId(RecordId);

impl ExamAttemptId {
    /// A deterministic id for the (exam, user, seq) triple, so sitting `seq`
    /// exists at most once by construction — a concurrent double "start" races
    /// on the same id and exactly one create wins. See [`key::sitting`] for the
    /// key shape and why the first sitting stays bare.
    pub fn composite(exam: &ExamId, user: &UserId, seq: i64) -> Self {
        let key = key::sitting(exam.key(), user.key(), seq);
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
    ///   conflict otherwise. A retake preserves every prior sitting: the new
    ///   row is created at the new `seq` and the student's earlier answers,
    ///   drawings, and marks stay put at their own seq — the fresh sitting
    ///   simply writes into an empty higher seq.
    ///
    /// The composite id makes each create atomic; a concurrent double-start
    /// races on the same seq, loses to the unique id, and reads the winner's
    /// row.
    ///
    /// A created row rides [`cap::create_counting`] so the student's
    /// `exam_sat_total` moves in the same transaction — but only for `seq == 1`,
    /// because that counter is *exams sat*, not sittings. A retake is the same
    /// exam again, and counting it made a badge a student could mint alone: an
    /// open exam with unlimited attempts is a start/finish loop nobody else has
    /// to touch. `seq == 1` is the whole condition and needs no extra read —
    /// the first sitting's id is one deterministic key, so of every writer
    /// aiming at it exactly one `CREATE` commits and the losers' increments
    /// abort with their duplicates, while a retake computes its seq from a row
    /// that already exists. A `SELECT` inside the transaction would be strictly
    /// worse: SurrealDB 3.2.3 conflict-checks write sets, not read sets.
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
        let id = attempt.id.record();
        let first = next_seq == 1;
        match cap::create_counting(
            &user.record(),
            EXAM_SAT_TOTAL_FIELD,
            first,
            &id,
            &attempt,
            db,
        )
        .await?
        {
            cap::Claimed::Made(created) => {
                // Only a moved counter can have crossed a threshold.
                if first && let Err(err) = badge::sync(user, db).await {
                    tracing::warn!("failed to sync badges for {}: {err}", user.key());
                }
                Ok((created, true))
            }
            // Only a still-running row proves the loss was a double-start
            // collision (the winner's fresh sitting); a terminal or missing
            // latest means the create genuinely failed — surface that instead
            // of passing a finished sitting off as a resume. The duplicate
            // aborted the transaction, so the loser's increment went with it.
            cap::Claimed::Duplicate => {
                match Self::read_latest_for_user(exam.get_id(), user, db).await? {
                    Some(existing)
                        if existing.status(exam, Timestamp::now()) == AttemptStatus::InProgress =>
                    {
                        Ok((existing, false))
                    }
                    _ => Err(AppError::Internal("failed to start exam attempt".into())),
                }
            }
            // Nothing caps sittings, so the conditional write can only miss by
            // finding no user row — not a state a live session can reach, and
            // passing it off as a resume would hand out a sitting the counter
            // never learned about.
            cap::Claimed::Full => Err(AppError::Internal(
                "cannot start an exam attempt: the student's account row is missing".into(),
            )),
        }
    }

    /// Stamp the submission time. The caller has already checked the deadline
    /// and that the attempt isn't finished.
    ///
    /// Writes *only* `finished_at`, for the mirror image of
    /// [`ExamAttempt::set_left`]'s reason: the submit path reads the attempt,
    /// then awaits its deadline and already-finished checks before writing, and
    /// the exam room stamps or clears `left_at` on the same row from a socket.
    /// A whole-row write from the pre-read snapshot would carry its stale
    /// `left_at` back over that stamp, erasing the recorded walk-out.
    pub async fn finish(self, db: &Database) -> Result<ExamAttempt, AppError> {
        let mut result = db
            .query("UPDATE $id SET finished_at = $at RETURN AFTER")
            .bind(("id", self.id.record()))
            .bind(("at", Some(Timestamp::now())))
            .await?
            .check()?;
        result
            .take::<Vec<ExamAttempt>>(0)?
            .into_iter()
            .next()
            .ok_or(AppError::NotFound)
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

    /// Every sitting of `user` nobody has submitted yet, across all exams. Only
    /// the *candidates* for "in progress": a deadline comes off the exam's live
    /// schedule, so the caller still judges [`Self::status`] per exam rather
    /// than re-spelling that rule in SurrealQL.
    pub async fn list_unfinished_for_user(
        user: &UserId,
        db: &Database,
    ) -> Result<Vec<ExamAttempt>, AppError> {
        let mut result = db
            .query("SELECT * FROM exam_attempt WHERE user = $usr AND finished_at = NONE")
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

    /// Send `statements` wrapped in a transaction that refuses to run them at
    /// all once `exam` has an attempt — the freeze gate, made atomic with the
    /// write it guards instead of merely preceding it. `freeze_exam` is bound
    /// here; the caller binds the rest and reads its own results from
    /// [`Self::FROZEN_SLOT`] onwards.
    ///
    /// The same transaction *writes* the exam row — bumping its mark counter
    /// and putting it straight back — which is what ties every child written
    /// through here to its exam. The freeze gate only *reads* `exam_attempt`,
    /// and a read does not survive [`Exam::delete`](crate::domain::exam::Exam::delete)'s
    /// window: a question or picture landing after that delete removed the exam
    /// but before it committed reads a row that is still there, while the
    /// cascade's `DELETE exam_question WHERE exam = $ex` ran on a snapshot
    /// predating this insert — so both commit and the child outlives the exam,
    /// with neither caller told anything. Writing a key the delete also writes
    /// makes the two collide and the store refuses one side. It is the shape
    /// [`crate::domain::exam_answer::ExamAnswer::save`] and
    /// [`crate::domain::menu::bump_menu_and_write`] already use.
    ///
    /// An orphan here is not merely untidy: an `exam_question` that outlives
    /// its exam keeps the reference it claimed on its subject (the cascade's
    /// per-subject decrement counted the rows it could see), and
    /// [`crate::domain::subject::Subject::delete`] is gated on that count
    /// reading zero — a subject nobody can ever delete again.
    ///
    /// The bump is restored *by captured value*, `NONE` included, so the row is
    /// byte-identical afterwards: the boot backfill still finds the rows it
    /// keys on (`WHERE result_count = NONE`) and a teacher's PATCH, which pins
    /// that counter, is not refused because somebody added a question. Writing
    /// the same value back would not do — an `UPDATE` that leaves the document
    /// unchanged is elided and never reaches the store's write set, so it
    /// collides with nothing.
    ///
    /// This replaces a process-wide `EXAM_LOCK.write()` held across the check
    /// and the write. That lock ordered the two requests but still read the
    /// attempt table a round trip before it wrote; the gate checks inside the
    /// writing statement, so a question edit can no longer sail past an attempt
    /// that started in that gap.
    ///
    /// The send lives here rather than at the five call sites because a lost
    /// round has to be re-sent, and only whoever owns the send can re-send: the
    /// gate reads the very table its rival writes, so the two contend by design
    /// and a raced edit used to answer 500. The refusal outranks the conflict —
    /// `FROZEN_MARK` is a decision and stays the 409 the pre-flight check
    /// answers with, and only exhausting the tries becomes a 500. See
    /// [`transaction_with_retry`] for why the whole error map is scanned: an
    /// aborted transaction errors *every* slot and all but one say a generic
    /// "not executed".
    ///
    /// Returning only on an empty error map is what keeps [`Self::FROZEN_SLOT`]
    /// (and any slot counted off `num_statements`) correct — `take_errors`
    /// `swap_remove`s errored slots, so a fixed slot read is meaningless once
    /// anything failed.
    //
    // corner-cut: the count and a concurrent `CREATE exam_attempt` are still not
    // serialized against each other — SurrealDB does not conflict-check a
    // cross-record count (the write skew `db::cap` exists for), so an
    // attempt landing in the same instant as an edit can still interleave
    // either way. The mutex this replaces closed that inside one process only,
    // and there are two, so nothing is lost. Closing it properly means the
    // cap.rs shape: an attempt counter on the exam row, incremented by the
    // attempt create, and the write conditioned on `count ?? 0 = 0`.
    pub(crate) async fn write_unfrozen(
        exam: &ExamId,
        statements: &str,
        bindings: Vec<(String, surrealdb::types::Value)>,
        db: &Database,
    ) -> Result<surrealdb::IndexedResults, AppError> {
        Self::write_unfrozen_with(exam, statements, bindings, Vec::new(), db).await
    }

    /// [`Self::write_unfrozen`] for statements that carry gates of their own:
    /// each `(marker, refusal)` names a `THROW` the caller's SQL aborts with and
    /// the error it means. The markers are handed to [`transaction_with_retry`]
    /// too, so an abort on one is a refusal rather than a round to re-send.
    ///
    /// The freeze still outranks every one of them: a caller folding a counter
    /// claim in here is refused as frozen even when its own gate would also have
    /// fired, which is the answer the pre-flight check has always given.
    pub(crate) async fn write_unfrozen_with(
        exam: &ExamId,
        statements: &str,
        bindings: Vec<(String, surrealdb::types::Value)>,
        refusals: Vec<(&str, AppError)>,
        db: &Database,
    ) -> Result<surrealdb::IndexedResults, AppError> {
        let sql = format!(
            "BEGIN TRANSACTION;
             IF array::len((SELECT VALUE id FROM exam_attempt \
             WHERE exam = $freeze_exam LIMIT 1)) > 0 {{ THROW '{FROZEN_MARK}' }};
             LET $was_results = \
                 (SELECT VALUE {EXAM_RESULT_COUNT_FIELD} FROM ONLY $freeze_exam);
             LET $touched = (UPDATE $freeze_exam SET {EXAM_RESULT_COUNT_FIELD} = \
                 ({EXAM_RESULT_COUNT_FIELD} ?? 0) + 1 RETURN VALUE id);
             IF array::len($touched) = 0 {{ THROW '{GONE_MARK}' }};
             UPDATE $freeze_exam SET {EXAM_RESULT_COUNT_FIELD} = $was_results;
             {statements}
             COMMIT TRANSACTION;"
        );
        let mut bound = vec![("freeze_exam".into(), exam.record().into_value())];
        bound.extend(bindings);
        let mut marks = vec![FROZEN_MARK, GONE_MARK];
        marks.extend(refusals.iter().map(|(marker, _)| *marker));
        let (result, mut errors) = transaction_with_retry(db, &sql, &bound, &marks).await?;
        let thrown = |marker: &str| {
            errors
                .values()
                .any(|error| error.to_string().contains(marker))
        };
        if thrown(FROZEN_MARK) {
            return Err(frozen_error());
        }
        // The exam is gone: the same 404 every one of these writes' handlers
        // answers a missing exam with, and it outranks the callers' own gates
        // for the freeze's reason — there is nothing left to refuse *about*.
        if thrown(GONE_MARK) {
            return Err(AppError::NotFound);
        }
        for (marker, refusal) in refusals {
            if thrown(marker) {
                return Err(refusal);
            }
        }
        match errors.drain().map(|(_, error)| error).next() {
            Some(error) => Err(error.into()),
            None => Ok(result),
        }
    }

    /// The first slot a [`Self::write_unfrozen`] caller's own statements land
    /// in: `BEGIN`, the freeze `IF`, and the exam touch's two `LET`s, `IF` and
    /// restoring `UPDATE` take one each.
    pub(crate) const FROZEN_SLOT: usize = 6;

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
    use crate::domain::exam::{
        ExamAttemptLimit, ExamDescription, ExamKind, ExamMode, ExamSchedule, ExamTitle,
    };
    use crate::domain::exam_answer::ExamAnswer;
    use crate::domain::exam_question::{
        ChoiceInput, ExamQuestion, QuestionKind, QuestionPoints, QuestionSpec,
    };

    /// The id of the question's second option — what these tests used to write
    /// as the index `1`.
    fn second_choice(question: &ExamQuestion) -> String {
        question.get_choices().unwrap()[1]
            .get_id()
            .as_str()
            .to_string()
    }
    use crate::domain::settings::Settings;

    /// An open exam with retakes allowed, plus one choice question — enough
    /// rows to exercise the attempt lifecycle without the HTTP layer.
    async fn open_exam_with_question(db: &Database, max_attempts: i64) -> (Exam, ExamQuestion) {
        let creator = UserId::from_key("01TESTTEACHERAAAAAAAAAAAAA");
        let course = crate::db::course::a_test_course(db).await;
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
            false,
            db,
        )
        .await
        .unwrap();
        let spec = QuestionSpec::try_new(
            QuestionKind::try_new("choice").unwrap(),
            Some(vec![
                ChoiceInput {
                    id: Some("a".into()),
                    text: "5".into(),
                },
                ChoiceInput {
                    id: Some("b".into()),
                    text: "6".into(),
                },
            ]),
            Some("b".into()),
            &[],
        )
        .unwrap();
        // A real subject row, not a minted id: a question claims a reference on
        // its subject and is refused if that subject does not exist.
        let subject = crate::domain::subject::Subject::create(
            &crate::db::course::a_test_course(db).await,
            crate::domain::subject::SubjectName::try_new("topic").unwrap(),
            crate::domain::subject::SubjectDescription::try_new("").unwrap(),
            db,
        )
        .await
        .unwrap();
        let question = ExamQuestion::create(
            exam.get_id(),
            subject.get_id().clone(),
            crate::domain::exam_question::QuestionText::try_new("3 + 3?").unwrap(),
            QuestionPoints::try_new(5).unwrap(),
            spec,
            db,
        )
        .await
        .unwrap();
        (exam, question)
    }

    /// A real user row: the sitting counter lives on it, and the claim that
    /// rides the attempt's create has nothing to write to without one.
    async fn student(db: &Database) -> UserId {
        let hash = crate::domain::user::Password::try_new("secret1")
            .unwrap()
            .hash_async()
            .await
            .unwrap();
        crate::domain::user::User::create(
            crate::domain::user::Username::try_new("ogrenci").unwrap(),
            hash,
            db,
        )
        .await
        .unwrap()
        .get_id()
        .clone()
    }

    /// The student's lifetime sitting count, absent reading as zero.
    async fn sat_total(user: &UserId, db: &Database) -> i64 {
        let mut result = db
            .query(format!(
                "SELECT VALUE ({EXAM_SAT_TOTAL_FIELD} ?? 0) FROM $usr"
            ))
            .bind(("usr", user.record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        result
            .take::<Vec<i64>>(0)
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
    }

    /// The badge counter counts *exams sat*, one per first sitting — so a start
    /// moves it by exactly one and a resume, which creates nothing, leaves it
    /// alone. Anything else and a seeded account and a fresh one would mean
    /// different things by the same number.
    #[tokio::test]
    async fn starting_counts_one_sitting_and_resuming_counts_none() {
        let db = init_mem().await.unwrap();
        let (exam, _question) = open_exam_with_question(&db, 2).await;
        let user = student(&db).await;
        assert_eq!(sat_total(&user, &db).await, 0, "a fresh row reads as zero");

        let (_, created) = ExamAttempt::start(&exam, &user, &db).await.unwrap();
        assert!(created);
        assert_eq!(sat_total(&user, &db).await, 1);

        // Still in progress: this start returns the running sitting untouched.
        let (_, created) = ExamAttempt::start(&exam, &user, &db).await.unwrap();
        assert!(!created);
        assert_eq!(
            sat_total(&user, &db).await,
            1,
            "a resume writes no row, so it counts nothing"
        );
    }

    /// A retake is the same exam again: the row lands at the next `seq` and the
    /// counter stays where it is. Counting it is the farm — an open exam with
    /// unlimited attempts would mint `exam_sat_25` off one exam and no teacher.
    #[tokio::test]
    async fn a_retake_writes_its_row_and_counts_nothing() {
        let db = init_mem().await.unwrap();
        let (exam, _question) = open_exam_with_question(&db, 3).await;
        let user = student(&db).await;

        let (first, _) = ExamAttempt::start(&exam, &user, &db).await.unwrap();
        first.finish(&db).await.unwrap();
        let (second, created) = ExamAttempt::start(&exam, &user, &db).await.unwrap();
        assert!(created);
        assert_eq!(second.get_seq(), 2);
        assert_eq!(sat_total(&user, &db).await, 1);
    }

    /// The lost half of a double-start race. `start` can only reach that branch
    /// by interleaving with a rival, so the claim is put to the counter
    /// directly — the same call `start` makes, aimed at a row that already
    /// exists. The duplicate aborts the transaction and takes its increment
    /// with it: a sitting is never counted twice.
    #[tokio::test]
    async fn a_lost_start_race_never_counts_the_sitting_twice() {
        let db = init_mem().await.unwrap();
        let (exam, _question) = open_exam_with_question(&db, 1).await;
        let user = student(&db).await;

        let (winner, _) = ExamAttempt::start(&exam, &user, &db).await.unwrap();
        assert_eq!(sat_total(&user, &db).await, 1);

        let claimed = cap::claim_and_create(
            &user.record(),
            EXAM_SAT_TOTAL_FIELD,
            cap::UNLIMITED,
            &winner.id.record(),
            &winner,
            &db,
        )
        .await
        .unwrap();
        assert!(matches!(claimed, cap::Claimed::Duplicate));
        assert_eq!(
            sat_total(&user, &db).await,
            1,
            "the loser's increment rolled back with its duplicate create"
        );
    }

    #[tokio::test]
    async fn a_retake_preserves_the_prior_sittings_sheet() {
        let db = init_mem().await.unwrap();
        let (exam, question) = open_exam_with_question(&db, 2).await;
        let user = student(&db).await;

        let (first, created) = ExamAttempt::start(&exam, &user, &db).await.unwrap();
        assert!(created);
        assert_eq!(first.get_seq(), 1);
        ExamAnswer::save(
            &question,
            &user,
            1,
            Some(second_choice(&question)),
            None,
            &db,
        )
        .await
        .unwrap();
        first.finish(&db).await.unwrap();

        // The retake lands as sitting #2 without touching sitting #1's answers.
        let (second, created) = ExamAttempt::start(&exam, &user, &db).await.unwrap();
        assert!(created);
        assert_eq!(second.get_seq(), 2);

        // Sitting #1's answer is still there, read at its own seq.
        let prior = ExamAnswer::read(question.get_id(), &user, 1, &db)
            .await
            .unwrap();
        assert!(prior.is_some(), "the retake must preserve seq 1's answer");
        // The new sitting starts blank at its own seq.
        let fresh = ExamAnswer::list_for_exam_user(exam.get_id(), &user, 2, &db)
            .await
            .unwrap();
        assert!(fresh.is_empty(), "seq 2 starts blank");
    }

    #[tokio::test]
    async fn a_lost_retake_race_cannot_touch_the_winners_sheet() {
        let db = init_mem().await.unwrap();
        let (exam, question) = open_exam_with_question(&db, 3).await;
        let user = student(&db).await;

        // Sitting #1 ends; the winner starts sitting #2 and saves an answer.
        let (first, _) = ExamAttempt::start(&exam, &user, &db).await.unwrap();
        first.finish(&db).await.unwrap();
        let (winner, created) = ExamAttempt::start(&exam, &user, &db).await.unwrap();
        assert!(created);
        assert_eq!(winner.get_seq(), 2);
        ExamAnswer::save(
            &question,
            &user,
            2,
            Some(second_choice(&question)),
            None,
            &db,
        )
        .await
        .unwrap();

        // A stale double-start races on the same seq and loses to the
        // composite id: the duplicate create is rejected, and with no wipe in
        // the path the winner's fresh answer is untouched either way.
        let loser = ExamAttempt {
            id: ExamAttemptId::composite(exam.get_id(), &user, 2),
            exam: exam.get_id().clone(),
            user: user.clone(),
            seq: 2,
            started_at: Timestamp::now(),
            finished_at: None,
            left_at: None,
        };
        let lost: Result<Option<ExamAttempt>, surrealdb::Error> =
            db.create(loser.id.record()).content(loser).await;
        assert!(lost.is_err(), "the duplicate-seq create must be rejected");
        let answers = ExamAnswer::list_for_exam_user(exam.get_id(), &user, 2, &db)
            .await
            .unwrap();
        assert_eq!(
            answers.len(),
            1,
            "the lost race must not disturb the winner's saved answer"
        );

        // The public path shrugs the race off: a re-start resumes the winner.
        let (resumed, created) = ExamAttempt::start(&exam, &user, &db).await.unwrap();
        assert!(!created);
        assert_eq!(resumed.get_seq(), 2);
    }

    #[tokio::test]
    async fn submitting_never_reverts_a_walk_out_that_raced_it() {
        let db = init_mem().await.unwrap();
        let (exam, _question) = open_exam_with_question(&db, 1).await;
        let user = student(&db).await;

        let (attempt, _) = ExamAttempt::start(&exam, &user, &db).await.unwrap();

        // The REST submit path reads the attempt, then checks the deadline and
        // the already-finished guard — several awaits before it writes.
        let stale = ExamAttempt::read(attempt.get_id(), &db)
            .await
            .unwrap()
            .expect("attempt exists");
        assert!(stale.get_left_at().is_none());

        // In that gap the exam room's teardown stamps the walk-out (or a join
        // clears it) — a field-scoped write to the same row.
        ExamAttempt::read(attempt.get_id(), &db)
            .await
            .unwrap()
            .expect("attempt exists")
            .set_left(Some(Timestamp::now()), &db)
            .await
            .unwrap();

        // The submit now writes from its stale snapshot. It owns `finished_at`
        // and nothing else: carrying the snapshot's blank `left_at` back over
        // the fresh stamp erases the recorded walk-out.
        let finished = stale.finish(&db).await.unwrap();
        assert!(finished.get_finished_at().is_some());

        let after = ExamAttempt::read(attempt.get_id(), &db)
            .await
            .unwrap()
            .expect("attempt still exists");
        assert!(
            after.get_left_at().is_some(),
            "submitting must not revert a walk-out stamp that raced it"
        );
        assert!(after.get_finished_at().is_some(), "the submission stands");
    }

    #[tokio::test]
    async fn stamping_left_never_reverts_a_submission() {
        let db = init_mem().await.unwrap();
        let (exam, _question) = open_exam_with_question(&db, 1).await;
        let user = student(&db).await;

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
