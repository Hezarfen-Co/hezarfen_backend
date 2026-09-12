//! The `exam_attempt` table: sitting reads, the two field-scoped writes
//! (`finished_at`, `left_at`), the guarded sitting create, and the freeze
//! gate — the in-transaction check every exam-child write runs before it
//! touches anything.

use sqlx::PgConnection;

use crate::database::{Database, tx_with_retry, unique_violation};
use crate::db::cap::Claimed;
use crate::domain::exam::{ExamId, ExamMode};
use crate::domain::exam_attempt::{ExamAttempt, ExamAttemptId};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// The 409 the freeze gate answers — the same one the handlers' pre-flight
/// check ([`crate::service::exam_question::ensure_questions_editable`])
/// answers, so a client cannot tell which of the two refused.
fn frozen_error() -> AppError {
    AppError::Conflict("cannot change questions after attempts have started")
}

/// The freeze gate, run inside a caller's transaction on that caller's own
/// connection: take the exam row `FOR UPDATE`, then refuse unless the exam
/// still exists and nobody has started an attempt.
///
/// This is the whole of the old `write_unfrozen` machinery — the
/// `questions_frozen` THROW and the bump-and-restore existence touch — with
/// Postgres doing the work the tricks stood in for. The row lock makes the
/// check-and-write one unit against [`crate::db::exam::delete`] (which locks
/// the same row before its cascade) and against a sitting create (whose
/// guard below locks it too): a question edit can no longer sail past an
/// attempt that started in the gate's gap, and a child whose exam died
/// mid-flight finds no row here and is the same 404 the old `no_exam` THROW
/// answered with. Real foreign keys retire the bump-and-restore: the child
/// inserts themselves refuse a parent that is gone.
pub(crate) async fn freeze_gate(conn: &mut PgConnection, exam: &ExamId) -> Result<(), AppError> {
    let row = sqlx::query!(
        r#"SELECT id AS "id: ExamId" FROM exam WHERE id = $1 FOR UPDATE"#,
        exam.uuid(),
    )
    .fetch_optional(&mut *conn)
    .await?;
    if row.is_none() {
        return Err(AppError::NotFound);
    }
    let sat = sqlx::query!(
        r#"SELECT EXISTS(SELECT 1 FROM exam_attempt WHERE exam = $1) AS sat"#,
        exam.uuid(),
    )
    .fetch_one(&mut *conn)
    .await?;
    if sat.sat.unwrap_or(false) {
        return Err(frozen_error());
    }
    Ok(())
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
    let id = attempt.get_id();
    let updated = sqlx::query_as!(
        ExamAttempt,
        r#"UPDATE exam_attempt SET finished_at = $1
           WHERE exam = $2 AND app_user = $3 AND seq = $4
           RETURNING exam AS "exam: ExamId", app_user AS "user: UserId", seq,
                     started_at AS "started_at: Timestamp",
                     finished_at AS "finished_at: Timestamp",
                     left_at AS "left_at: Timestamp""#,
        Timestamp::now().as_millis(),
        id.exam.uuid(),
        id.user.uuid(),
        id.seq,
    )
    .fetch_optional(db)
    .await?;
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
    db: &Database,
    attempt: ExamAttempt,
    left_at: Option<Timestamp>,
) -> Result<ExamAttempt, AppError> {
    let id = attempt.get_id();
    let updated = sqlx::query_as!(
        ExamAttempt,
        r#"UPDATE exam_attempt SET left_at = $1
           WHERE exam = $2 AND app_user = $3 AND seq = $4
           RETURNING exam AS "exam: ExamId", app_user AS "user: UserId", seq,
                     started_at AS "started_at: Timestamp",
                     finished_at AS "finished_at: Timestamp",
                     left_at AS "left_at: Timestamp""#,
        left_at.map(|at| at.as_millis()),
        id.exam.uuid(),
        id.user.uuid(),
        id.seq,
    )
    .fetch_optional(db)
    .await?;
    updated.ok_or(AppError::NotFound)
}

/// Stamp `left_at` on one sitting, but only while it is genuinely in
/// progress: nothing stamped yet, nothing submitted, and the exam's *live*
/// schedule has not run past the sitting's deadline. One conditional
/// statement — the exam-room teardown's whole stamp, with the re-derive
/// [`crate::domain::exam_attempt::ExamAttempt::status`] used to do folded
/// into the `WHERE` (joined to `exam` so a deleted exam's sitting is simply
/// not stamped, and concurrent last-outs are harmless: only the first finds
/// `left_at IS NULL`).
pub async fn stamp_left_if_running(
    db: &Database,
    attempt_id: &ExamAttemptId,
    now: Timestamp,
) -> Result<(), AppError> {
    sqlx::query!(
        r#"UPDATE exam_attempt a SET left_at = $4
           FROM exam e
           WHERE e.id = a.exam
             AND a.exam = $1 AND a.app_user = $2 AND a.seq = $3
             AND a.left_at IS NULL AND a.finished_at IS NULL
             AND (e.duration_ms IS NULL OR $4 < a.started_at + e.duration_ms)
             AND (e.duration_ms IS NOT NULL OR e.ends_at IS NULL OR $4 < e.ends_at)"#,
        attempt_id.exam.uuid(),
        attempt_id.user.uuid(),
        attempt_id.seq,
        now.as_millis(),
    )
    .execute(db)
    .await?;
    Ok(())
}

/// One sitting by id — the exam room re-reads its own attempt this way,
/// so a retake started elsewhere can never be mistaken for it.
pub async fn read(db: &Database, id: &ExamAttemptId) -> Result<Option<ExamAttempt>, AppError> {
    Ok(sqlx::query_as!(
        ExamAttempt,
        r#"SELECT exam AS "exam: ExamId", app_user AS "user: UserId", seq,
                  started_at AS "started_at: Timestamp",
                  finished_at AS "finished_at: Timestamp",
                  left_at AS "left_at: Timestamp"
           FROM exam_attempt
           WHERE exam = $1 AND app_user = $2 AND seq = $3"#,
        id.exam.uuid(),
        id.user.uuid(),
        id.seq,
    )
    .fetch_optional(db)
    .await?)
}

/// The student's current sitting — the highest `seq` for the pair. All
/// reads that used to mean "the attempt" mean this now.
pub async fn read_latest_for_user(
    db: &Database,
    exam: &ExamId,
    user: &UserId,
) -> Result<Option<ExamAttempt>, AppError> {
    Ok(sqlx::query_as!(
        ExamAttempt,
        r#"SELECT exam AS "exam: ExamId", app_user AS "user: UserId", seq,
                  started_at AS "started_at: Timestamp",
                  finished_at AS "finished_at: Timestamp",
                  left_at AS "left_at: Timestamp"
           FROM exam_attempt
           WHERE exam = $1 AND app_user = $2
           ORDER BY seq DESC
           LIMIT 1"#,
        exam.uuid(),
        user.uuid(),
    )
    .fetch_optional(db)
    .await?)
}

/// Every sitting of `user` at `exam`, newest first.
pub async fn list_for_user(
    db: &Database,
    exam: &ExamId,
    user: &UserId,
) -> Result<Vec<ExamAttempt>, AppError> {
    Ok(sqlx::query_as!(
        ExamAttempt,
        r#"SELECT exam AS "exam: ExamId", app_user AS "user: UserId", seq,
                  started_at AS "started_at: Timestamp",
                  finished_at AS "finished_at: Timestamp",
                  left_at AS "left_at: Timestamp"
           FROM exam_attempt
           WHERE exam = $1 AND app_user = $2
           ORDER BY seq DESC"#,
        exam.uuid(),
        user.uuid(),
    )
    .fetch_all(db)
    .await?)
}

/// Every sitting of `user` nobody has submitted yet, across all exams. Only
/// the *candidates* for "in progress": a deadline comes off the exam's live
/// schedule, so the caller still judges [`ExamAttempt::status`] per exam rather
/// than re-spelling that rule in SQL.
pub async fn list_unfinished_for_user(
    db: &Database,
    user: &UserId,
) -> Result<Vec<ExamAttempt>, AppError> {
    Ok(sqlx::query_as!(
        ExamAttempt,
        r#"SELECT exam AS "exam: ExamId", app_user AS "user: UserId", seq,
                  started_at AS "started_at: Timestamp",
                  finished_at AS "finished_at: Timestamp",
                  left_at AS "left_at: Timestamp"
           FROM exam_attempt
           WHERE app_user = $1 AND finished_at IS NULL
           ORDER BY started_at, exam"#,
        user.uuid(),
    )
    .fetch_all(db)
    .await?)
}

pub async fn list_for_exam(db: &Database, exam: &ExamId) -> Result<Vec<ExamAttempt>, AppError> {
    Ok(sqlx::query_as!(
        ExamAttempt,
        r#"SELECT exam AS "exam: ExamId", app_user AS "user: UserId", seq,
                  started_at AS "started_at: Timestamp",
                  finished_at AS "finished_at: Timestamp",
                  left_at AS "left_at: Timestamp"
           FROM exam_attempt
           WHERE exam = $1
           ORDER BY started_at DESC, seq DESC"#,
        exam.uuid(),
    )
    .fetch_all(db)
    .await?)
}

/// Whether anyone has started this exam — the gate that freezes `mode`
/// edits once an attempt exists.
pub async fn any_for_exam(db: &Database, exam: &ExamId) -> Result<bool, AppError> {
    let row = sqlx::query!(
        r#"SELECT EXISTS(SELECT 1 FROM exam_attempt WHERE exam = $1) AS sat"#,
        exam.uuid(),
    )
    .fetch_one(db)
    .await?;
    Ok(row.sat.unwrap_or(false))
}

/// The sitting-create guard: the exam row locked `FOR UPDATE` and
/// re-judged *inside the inserting transaction* — draft (a 404, a draft is
/// invisible to students), no mode (a 409: visible, nothing to sit), the
/// window not yet open, the window already closed. This is what the old
/// writer lease `EXAM_LOCK` used to buy the sittable/window gates: they are
/// now judged against the locked row the attempt lands under, so a mode
/// change or re-draft cannot slip between a gate and the insert. The
/// refusal texts are [`crate::service::exam_attempt`]'s own — the same
/// words that pre-flight answers, so a client cannot tell which fired.
pub(crate) async fn guard_start(
    conn: &mut PgConnection,
    exam: &ExamId,
    now: Timestamp,
) -> Result<(), AppError> {
    let row = sqlx::query!(
        r#"SELECT draft AS "draft: bool", mode AS "mode: ExamMode",
                  starts_at AS "starts_at: Timestamp",
                  ends_at AS "ends_at: Timestamp"
           FROM exam WHERE id = $1 FOR UPDATE"#,
        exam.uuid(),
    )
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return Err(AppError::NotFound);
    };
    if row.draft {
        return Err(AppError::NotFound);
    }
    if row.mode.is_none() {
        return Err(AppError::Conflict(
            "this exam is not scheduled — there is nothing to sit (give it a mode: sync, async, or open)",
        ));
    }
    if row.starts_at.is_some_and(|starts_at| now < starts_at) {
        return Err(AppError::Conflict("the exam has not started yet"));
    }
    if row.ends_at.is_some_and(|ends_at| now >= ends_at) {
        return Err(AppError::Conflict("the exam has already ended"));
    }
    Ok(())
}

/// Insert the sitting row, claiming the student's `exam_sat_total` in the
/// same statement when — and only when — this is a first sitting (`seq ==
/// 1`): that counter is *exams sat*, not sittings, and a retake counts
/// nothing. The claim-CTE recipe ([`cap`]): bump the counter row, insert
/// the child only if the bump landed, and let the sitting's natural
/// composite primary key (`exam_attempt_exam_user_seq`) answer "another
/// writer started this very sitting first" as `Claimed::Duplicate`. The
/// bump rides the insert's own transaction, so a refused insert takes its
/// increment straight back.
///
/// `Claimed::Full` here can only mean the student's account row is missing
/// — nothing caps sittings — which the caller surfaces as its own internal.
pub(crate) async fn create_in(
    conn: &mut PgConnection,
    attempt: &ExamAttempt,
    count_sitting: bool,
) -> Result<Claimed<ExamAttempt>, AppError> {
    let bump = i64::from(count_sitting);
    let created = sqlx::query_as!(
        ExamAttempt,
        r#"WITH seat AS (
               UPDATE app_user SET exam_sat_total = app_user.exam_sat_total + $4
               WHERE id = $2
               RETURNING 1)
           INSERT INTO exam_attempt (exam, app_user, seq, started_at, finished_at, left_at)
           SELECT $1, $2, $3, $5, NULL, NULL WHERE EXISTS (SELECT 1 FROM seat)
           RETURNING exam AS "exam: ExamId", app_user AS "user: UserId", seq,
                     started_at AS "started_at: Timestamp",
                     finished_at AS "finished_at: Timestamp",
                     left_at AS "left_at: Timestamp""#,
        attempt.exam.uuid(),
        attempt.user.uuid(),
        attempt.seq,
        bump,
        attempt.started_at.as_millis(),
    )
    .fetch_optional(&mut *conn)
    .await;
    match created {
        Err(err) if unique_violation(&err) == Some("exam_attempt_exam_user_seq") => {
            Ok(Claimed::Duplicate)
        }
        Err(err) => Err(err.into()),
        Ok(None) => Ok(Claimed::Full),
        Ok(Some(attempt)) => Ok(Claimed::Made(attempt)),
    }
}

/// The sitting create as one transaction: [`guard_start`], then
/// [`create_in`]. Retried while Postgres answers "contended"; a duplicate
/// or a missing student row is a verdict, not a conflict, and is never
/// re-sent.
pub async fn create(
    db: &Database,
    attempt: &ExamAttempt,
    count_sitting: bool,
    now: Timestamp,
) -> Result<Claimed<ExamAttempt>, AppError> {
    tx_with_retry(db, false, async |conn| {
        guard_start(conn, &attempt.exam, now).await?;
        create_in(conn, attempt, count_sitting).await
    })
    .await
}

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
        let course = crate::db::course::a_test_course(db).await;
        let kinds = Settings::defaults().get_exam_kinds().to_vec();
        let exam = crate::db::exam::create(
            db,
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
        let subject = crate::db::subject::create(
            db,
            &crate::db::course::a_test_course(db).await,
            crate::domain::subject::SubjectName::try_new("topic").unwrap(),
            crate::domain::subject::SubjectDescription::try_new("").unwrap(),
        )
        .await
        .unwrap();
        let question = crate::db::exam_question::create(
            db,
            exam.get_id(),
            subject.get_id().clone(),
            crate::domain::exam_question::QuestionText::try_new("3 + 3?").unwrap(),
            QuestionPoints::try_new(5).unwrap(),
            spec,
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
        crate::db::user::create(
            db,
            crate::domain::user::Username::try_new("ogrenci").unwrap(),
            hash,
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
