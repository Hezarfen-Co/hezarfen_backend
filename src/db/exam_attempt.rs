//! The `exam_attempt` table: sitting reads, the two field-scoped writes
//! (`finished_at`, `left_at`), and the `write_unfrozen` transaction gateway
//! that ties exam-child writes to their exam row.

use surrealdb::types::{RecordId, SurrealValue};

use crate::constant::EXAM_RESULT_COUNT_FIELD;
use crate::database::{Database, transaction_with_retry};
use crate::domain::exam::ExamId;
use crate::domain::exam_attempt::{ExamAttempt, ExamAttemptId};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// The `THROW` marker the freeze gate aborts with, and the one 409 both it and
/// the handler's pre-flight check answer with — a client cannot tell which of
/// the two refused.
const FROZEN_MARK: &str = "questions_frozen";

/// The `THROW` marker the exam-existence touch aborts with — the exam row this
/// write hangs off is gone, so the write is a 404 and nothing lands.
const GONE_MARK: &str = "no_exam";

/// The 409 the freeze gate's marker means.
fn frozen_error() -> AppError {
    AppError::Conflict("cannot change questions after attempts have started")
}

/// Stamp the submission time. The caller has already checked the deadline
/// and that the attempt isn't finished.
///
/// Writes *only* `finished_at`, for the mirror image of
/// [`set_left`]'s reason: the submit path reads the attempt,
/// then awaits its deadline and already-finished checks before writing, and
/// the exam room stamps or clears `left_at` on the same row from a socket.
/// A whole-row write from the pre-read snapshot would carry its stale
/// `left_at` back over that stamp, erasing the recorded walk-out.
pub async fn finish(db: &Database, attempt: ExamAttempt) -> Result<ExamAttempt, AppError> {
    let mut result = db
        .query("UPDATE $id SET finished_at = $at RETURN AFTER")
        .bind(("id", attempt.id.record()))
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
    db: &Database,
    attempt: ExamAttempt,
    left_at: Option<Timestamp>,
) -> Result<ExamAttempt, AppError> {
    let mut result = db
        .query("UPDATE $id SET left_at = $left RETURN AFTER")
        .bind(("id", attempt.id.record()))
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
pub async fn read(db: &Database, id: &ExamAttemptId) -> Result<Option<ExamAttempt>, AppError> {
    Ok(db.select(id.record()).await?)
}

/// The student's current sitting — the highest `seq` for the pair. All
/// reads that used to mean "the attempt" mean this now.
pub async fn read_latest_for_user(
    db: &Database,
    exam: &ExamId,
    user: &UserId,
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
    db: &Database,
    exam: &ExamId,
    user: &UserId,
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
/// schedule, so the caller still judges [`ExamAttempt::status`] per exam rather
/// than re-spelling that rule in SurrealQL.
pub async fn list_unfinished_for_user(
    db: &Database,
    user: &UserId,
) -> Result<Vec<ExamAttempt>, AppError> {
    let mut result = db
        .query("SELECT * FROM exam_attempt WHERE user = $usr AND finished_at = NONE")
        .bind(("usr", user.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<ExamAttempt>>(0)?)
}

pub async fn list_for_exam(db: &Database, exam: &ExamId) -> Result<Vec<ExamAttempt>, AppError> {
    let mut result = db
        .query("SELECT * FROM exam_attempt WHERE exam = $ex ORDER BY id DESC")
        .bind(("ex", exam.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<ExamAttempt>>(0)?)
}

/// Whether anyone has started this exam — the gate that freezes `mode`
/// edits once an attempt exists.
pub async fn any_for_exam(db: &Database, exam: &ExamId) -> Result<bool, AppError> {
    let mut result = db
        .query("SELECT VALUE id FROM exam_attempt WHERE exam = $ex LIMIT 1")
        .bind(("ex", exam.record()))
        .await?
        .check()?;
    Ok(!result.take::<Vec<RecordId>>(0)?.is_empty())
}

/// Send `statements` wrapped in a transaction that refuses to run them at
/// all once `exam` has an attempt — the freeze gate, made atomic with the
/// write it guards instead of merely preceding it. `freeze_exam` is bound
/// here; the caller binds the rest and reads its own results from
/// [`FROZEN_SLOT`] onwards.
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
/// Returning only on an empty error map is what keeps [`FROZEN_SLOT`]
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
pub async fn write_unfrozen(
    db: &Database,
    exam: &ExamId,
    statements: &str,
    bindings: Vec<(String, surrealdb::types::Value)>,
) -> Result<surrealdb::IndexedResults, AppError> {
    write_unfrozen_with(db, exam, statements, bindings, Vec::new()).await
}

/// [`write_unfrozen`] for statements that carry gates of their own:
/// each `(marker, refusal)` names a `THROW` the caller's SQL aborts with and
/// the error it means. The markers are handed to [`transaction_with_retry`]
/// too, so an abort on one is a refusal rather than a round to re-send.
///
/// The freeze still outranks every one of them: a caller folding a counter
/// claim in here is refused as frozen even when its own gate would also have
/// fired, which is the answer the pre-flight check has always given.
pub async fn write_unfrozen_with(
    db: &Database,
    exam: &ExamId,
    statements: &str,
    bindings: Vec<(String, surrealdb::types::Value)>,
    refusals: Vec<(&str, AppError)>,
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

/// The first slot a [`write_unfrozen`] caller's own statements land
/// in: `BEGIN`, the freeze `IF`, and the exam touch's two `LET`s, `IF` and
/// restoring `UPDATE` take one each.
pub const FROZEN_SLOT: usize = 6;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_mem;
    use crate::domain::exam::{
        Exam, ExamAttemptLimit, ExamDescription, ExamKind, ExamMode, ExamSchedule, ExamTitle,
    };
    use crate::domain::exam_question::{
        ChoiceInput, ExamQuestion, QuestionKind, QuestionPoints, QuestionSpec,
    };
    use crate::domain::settings::Settings;

    /// An open exam with retakes allowed, plus one choice question — enough
    /// rows to exercise the attempt lifecycle without the HTTP layer.
    async fn open_exam_with_question(db: &Database, max_attempts: i64) -> (Exam, ExamQuestion) {
        let creator = UserId::from_key("01TESTTEACHERAAAAAAAAAAAAA");
        let course = crate::domain::course::a_test_course(db).await;
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
            &crate::domain::course::a_test_course(db).await,
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

    #[tokio::test]
    async fn submitting_never_reverts_a_walk_out_that_raced_it() {
        let db = init_mem().await.unwrap();
        let (exam, _question) = open_exam_with_question(&db, 1).await;
        let user = student(&db).await;

        let (attempt, _) = crate::service::exam_attempt::start(&db, &exam, &user)
            .await
            .unwrap();

        // The REST submit path reads the attempt, then checks the deadline and
        // the already-finished guard — several awaits before it writes.
        let stale = read(&db, attempt.get_id())
            .await
            .unwrap()
            .expect("attempt exists");
        assert!(stale.get_left_at().is_none());

        // In that gap the exam room's teardown stamps the walk-out (or a join
        // clears it) — a field-scoped write to the same row.
        set_left(
            &db,
            read(&db, attempt.get_id())
                .await
                .unwrap()
                .expect("attempt exists"),
            Some(Timestamp::now()),
        )
        .await
        .unwrap();

        // The submit now writes from its stale snapshot. It owns `finished_at`
        // and nothing else: carrying the snapshot's blank `left_at` back over
        // the fresh stamp erases the recorded walk-out.
        let finished = finish(&db, stale).await.unwrap();
        assert!(finished.get_finished_at().is_some());

        let after = read(&db, attempt.get_id())
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
        let (attempt, _) = crate::service::exam_attempt::start(&db, &exam, &user)
            .await
            .unwrap();
        assert_eq!(attempt.get_seq(), 1);

        // The exam-room teardown reads the attempt while it is still in
        // progress (`stamp_left`'s read) — a snapshot with `finished_at` unset.
        let stale = read(&db, attempt.get_id())
            .await
            .unwrap()
            .expect("attempt exists");
        assert!(stale.get_finished_at().is_none());
        let finished = finish(
            &db,
            read(&db, attempt.get_id())
                .await
                .unwrap()
                .expect("attempt exists"),
        )
        .await
        .unwrap();
        assert!(finished.get_finished_at().is_some());

        // Now the last socket's teardown stamps `left_at` from its stale
        // snapshot. Stamping the walk-out must touch only `left_at` — it must
        // not carry the snapshot's blank `finished_at` back over the fresh
        // submission, or the student's submitted exam silently reopens (and,
        // with `allow_rejoin` off, locks them out of a room they left after
        // finishing).
        set_left(&db, stale, Some(Timestamp::now())).await.unwrap();

        let after = read(&db, attempt.get_id())
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
            crate::domain::exam_attempt::AttemptStatus::Submitted,
            "the attempt stays submitted"
        );
    }
}
