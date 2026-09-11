//! The exam-sitting workflows: starting, resuming, and finishing sittings,
//! saving answers into them, and the gates (sittable, student, enrollment,
//! rejoin, archived term) every sitting path shares. The queries live in
//! [`crate::db::exam_attempt`]; the HTTP shaping stays in the web layer.

use crate::constant::EXAM_SAT_TOTAL_FIELD;
use crate::database::Database;
use crate::db::cap;
use crate::domain::badge;
use crate::domain::course::Course;
use crate::domain::enrollment::Enrollment;
use crate::domain::exam::{Exam, ExamId};
use crate::domain::exam_answer::ExamAnswer;
use crate::domain::exam_attempt::{AttemptStatus, ExamAttempt, ExamAttemptId};
use crate::domain::exam_question::{ExamQuestion, ExamQuestionId};
use crate::domain::role::Role;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::AppError;

/// Serializes the exam subsystem's cross-record check-then-writes, which
/// `BEGIN…COMMIT` cannot (write skew) — the reasoning in [`crate::db::cap`].
/// It orders requests, but only around what it wraps — every invariant that
/// could be moved into the database itself has been, so a gap between a read
/// and its write is decided by the store. What is left here needs a
/// cross-record read and a write held together, which no single statement
/// expresses:
///
/// Read side — the answer saves (REST and the exam room), from the
/// writable-attempt gate through the upsert; the grade write (draft gate
/// through the result upsert); and the exam PATCH, whose mode/re-draft gates
/// count attempts and results. These stay concurrent with each other.
///
/// Write side — attempt starts alone (the max-attempts count and the retake's
/// answer wipe). So a save can never land on a sheet a retake just wiped, and a
/// mark can never land on an exam mid-flight into hiding.
///
/// Two rules have left this list. The question freeze gate rides inside each
/// question/image write's own transaction
/// ([`crate::db::exam_attempt::write_unfrozen`]), and the exam PATCH
/// no longer needs the writer lease because its save is a compare-and-set. The
/// subject delete's cascade — the only writer outside attempt starts, paired
/// with the question writes' subject check — is now a conditional statement on
/// the subject's own reference counter
/// ([`crate::domain::subject::Subject::delete`]), which every question create,
/// re-tag and delete moves.
// corner-cut: global RwLock, shard per-exam if save latency ever matters.
pub(crate) static EXAM_LOCK: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

/// Rejects sitting an exam that can't be sat. A draft is a `404`, not a
/// `409` — sitting is a student act, drafts are invisible to students, and a
/// state-specific error would leak the existence this feature hides. A
/// modeless (offline-graded) exam is a `409`: visible, just nothing to sit.
/// Enrollment and window checks for the caller are the caller's own state —
/// also `Conflict`, not validation.
pub fn ensure_sittable(exam: &Exam) -> Result<(), AppError> {
    if exam.is_draft() {
        return Err(AppError::NotFound);
    }
    if exam.get_mode().is_none() {
        return Err(AppError::Conflict(
            "this exam is not scheduled — there is nothing to sit (give it a mode: sync, async, or open)",
        ));
    }
    Ok(())
}

/// A 403 unless `user` is a student. Sitting an exam is a student action —
/// teachers and above run exams, they never take them — so the sit paths
/// (start, room, save) enforce it on the *current* role. Checking the live
/// role, not just enrollment, closes the gap a mid-exam promotion would open
/// and neutralizes any stale non-student enrollment. Reading one's own attempt
/// and finishing stay ungated: a non-student has no attempt to read, and
/// finishing only submits work already saved.
pub fn ensure_student(user: &User) -> Result<(), AppError> {
    if user.get_role() != Role::Student {
        return Err(AppError::Forbidden("only students can sit exams"));
    }
    Ok(())
}

/// The answer path's live edition of [`ensure_student`]: re-read the row and
/// judge the *current* role, exactly like the enrollment wall beside it. Both
/// save paths run through here (REST per request, the exam room per message),
/// so a promotion out of `student` mid-exam closes the sheet on the very next
/// save — the room's door check is not the last word for a socket that
/// outlives the role.
pub async fn ensure_student_now(user: &UserId, db: &Database) -> Result<(), AppError> {
    let user = crate::domain::user::User::read(user, db)
        .await?
        .ok_or(AppError::Unauthorized)?;
    ensure_student(&user)
}

/// A 403 unless `user` is enrolled in the exam's course — the same wall the
/// exam room checks at its door, re-applied to the sitting's content paths so
/// an unenrollment mid-exam cuts them too. Finishing stays exempt: like the
/// rejoin lock, submitting what's already saved writes nothing new.
pub async fn ensure_enrolled(exam: &Exam, user: &UserId, db: &Database) -> Result<(), AppError> {
    if Enrollment::read_for_user(exam.get_course(), user, db)
        .await?
        .is_none()
    {
        return Err(AppError::Forbidden(
            "you are not enrolled in this exam's course",
        ));
    }
    Ok(())
}

/// A 409 when the student has walked out of the exam room and the exam's
/// rejoin door is closed: no more answering (from anywhere) until the teacher
/// flips `allow_rejoin` back on. Finishing is deliberately exempt — see
/// the finish workflow.
pub fn check_rejoin(exam: &Exam, attempt: &ExamAttempt) -> Result<(), AppError> {
    if attempt.get_left_at().is_some() && !exam.get_allow_rejoin() {
        return Err(AppError::Conflict(
            "you left the exam and rejoin is closed — ask your teacher to reopen it",
        ));
    }
    Ok(())
}

/// The caller's latest attempt provided it is still writable, or the error
/// that says why not: no attempt yet (404 — start it first), already
/// submitted (409), deadline passed (409). One gate shared by REST saves and
/// the WebSocket room.
pub async fn writable_attempt(
    exam: &Exam,
    user: &UserId,
    db: &Database,
) -> Result<ExamAttempt, AppError> {
    let attempt = crate::db::exam_attempt::read_latest_for_user(db, exam.get_id(), user)
        .await?
        .ok_or(AppError::NotFound)?;
    match attempt.status(exam, Timestamp::now()) {
        AttemptStatus::Submitted => Err(AppError::Conflict("the attempt is already submitted")),
        AttemptStatus::Expired => Err(AppError::Conflict("time is up — the attempt has expired")),
        AttemptStatus::InProgress => Ok(attempt),
    }
}

/// The course an exam belongs to. A dangling reference means the course-delete
/// cascade was violated — surface it loudly as a 500, not a user-facing 404.
pub async fn course_of(exam: &Exam, db: &Database) -> Result<Course, AppError> {
    crate::domain::course::Course::read(exam.get_course(), db)
        .await?
        .ok_or_else(|| AppError::Internal("exam references a missing course".into()))
}

/// The question, provided it belongs to `exam` — a qid under someone else's
/// exam is a plain 404, not a leak.
pub async fn question_of_exam(
    exam: &ExamId,
    qid: &str,
    db: &Database,
) -> Result<ExamQuestion, AppError> {
    let question = ExamQuestion::read(&ExamQuestionId::from_key(qid), db)
        .await?
        .ok_or(AppError::NotFound)?;
    if question.get_exam() != exam {
        return Err(AppError::NotFound);
    }
    Ok(question)
}

/// Start, resume, or retake `user`'s attempt at `exam`. Returns the attempt
/// plus whether it was newly created.
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
    db: &Database,
    exam: &Exam,
    user: &UserId,
) -> Result<(ExamAttempt, bool), AppError> {
    let attempts = crate::db::exam_attempt::list_for_user(db, exam.get_id(), user).await?;
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
            match crate::db::exam_attempt::read_latest_for_user(db, exam.get_id(), user).await? {
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

/// Start, resume, or retake the caller's attempt at `exam_id`. Requires the
/// student role (staff run exams, they don't sit them), enrollment in the
/// exam's course, a sittable exam (`sync`/`async`/`open` mode), and — when a
/// window exists — the window to be open. Returns the exam the gates judged
/// (the response describes that row) plus the attempt and whether it was
/// newly created.
///
/// Writer lease of [`EXAM_LOCK`] from the exam read through the start: the
/// sittable/window gates must be judged against the same exam row the
/// attempt lands under (the mirror of `update_exam`'s re-derive — without
/// it, a mode change or re-draft at legally-zero attempts could slip
/// between this gate and the insert, leaving an attempt on an unsittable
/// exam). The lease
/// also keeps the max-attempts count and the retake's wipe-and-create
/// from interleaving with an in-flight answer save (a reader). The caller
/// drops nothing: the lease lives and dies inside this call, before the
/// response reads — they only describe the row.
///
/// A still-running attempt comes back as-is (the handler answers `200`
/// instead of `201`), so a reconnecting client gets its original clock
/// back — re-starting never resets the time. Once the latest attempt is
/// submitted or expired, re-posting starts the next sitting while the
/// exam's `max_attempts` (0 = unlimited) allows it.
///
/// A retake no longer wipes the prior sitting — each attempt's answers,
/// drawings, and marks stay put at their own seq (per-attempt history), so
/// there are no orphaned blobs to GC here. The exam-delete cascade still
/// cleans every sitting's blobs.
pub async fn start_attempt(
    db: &Database,
    exam_id: &ExamId,
    user: &User,
) -> Result<(Exam, ExamAttempt, bool), AppError> {
    let _guard = EXAM_LOCK.write().await;
    let exam = crate::domain::exam::Exam::read(exam_id, db)
        .await?
        .ok_or(AppError::NotFound)?;
    ensure_sittable(&exam)?;
    ensure_student(user)?;
    ensure_enrolled(&exam, user.get_id(), db).await?;
    course_of(&exam, db).await?.require_open(db).await?;
    let now = Timestamp::now();
    if let Some(starts_at) = exam.get_starts_at()
        && now < starts_at
    {
        return Err(AppError::Conflict("the exam has not started yet"));
    }
    if let Some(ends_at) = exam.get_ends_at()
        && now >= ends_at
    {
        return Err(AppError::Conflict("the exam has already ended"));
    }
    let (attempt, created) = start(db, &exam, user.get_id()).await?;
    Ok((exam, attempt, created))
}

/// Submit `user`'s latest attempt at `exam`. Allowed while the deadline
/// hasn't passed; after it, the attempt is already `expired` (a valid
/// terminal state — the student used their full time) and submitting is a
/// `409`.
///
/// Deliberately no rejoin check: a student locked out of the room may
/// still submit what they saved — finishing answers nothing new.
pub async fn finish_attempt(
    db: &Database,
    exam: &Exam,
    user: &UserId,
) -> Result<ExamAttempt, AppError> {
    let attempt = crate::db::exam_attempt::read_latest_for_user(db, exam.get_id(), user)
        .await?
        .ok_or(AppError::NotFound)?;
    if attempt.get_finished_at().is_some() {
        return Err(AppError::Conflict("the attempt is already submitted"));
    }
    let now = Timestamp::now();
    if let Some(deadline) = attempt.deadline(exam)
        && now >= deadline
    {
        return Err(AppError::Conflict("time is up — the attempt has expired"));
    }
    course_of(exam, db).await?.require_open(db).await?;
    crate::db::exam_attempt::finish(db, attempt).await
}

/// Save one answer inside the caller's in-progress attempt — the whole write
/// path (student gate, attempt gate, rejoin gate via [`save_answer_in`],
/// question lookup, kind check, upsert). The REST handler saves into the
/// latest sitting via this; the WebSocket room
/// resolves its own sitting first and shares [`save_answer_in`], so the two
/// can never drift.
pub async fn save_answer_checked(
    db: &Database,
    exam: &Exam,
    user: &User,
    question_id: &str,
    selected: Option<String>,
    text: Option<String>,
) -> Result<ExamAnswer, AppError> {
    ensure_student(user)?;
    // Reader lease of [`EXAM_LOCK`]: the writable gate and the upsert are
    // one unit, or a retake's wipe-and-create (a writer) slips in between
    // and this stale save lands on the fresh blank sheet.
    //
    // The lease covers the gate *and* the upsert here, so a retake cannot
    // interleave with this path at all. The accepted late-save race lives in
    // the exam-room socket instead, which writes into the sitting it joined
    // with — a value chosen before any lease is taken. See
    // [`crate::web::exam_ws`].
    let _guard = EXAM_LOCK.read().await;
    let attempt = writable_attempt(exam, user.get_id(), db).await?;
    save_answer_in(db, exam, &attempt, question_id, selected, text).await
}

/// The tail of the answer write path, given the sitting to write in: the
/// student wall and the enrollment wall (a promotion out of `student` or an
/// unenrollment closes the sheet, mid-exam included), the rejoin gate, the
/// archived-term gate, the question lookup, and the upsert.
///
/// The archived-term refusal sits here because this is the single funnel every
/// answer write passes: REST `POST /exams/{id}/attempt/answers` (via
/// [`save_answer_checked`]) *and* every `answer` frame of the exam-room
/// WebSocket ([`crate::web::exam_ws`]). The answer-image writes are the only
/// answer-side writes outside it, and carry their own guard.
pub async fn save_answer_in(
    db: &Database,
    exam: &Exam,
    attempt: &ExamAttempt,
    question_id: &str,
    selected: Option<String>,
    text: Option<String>,
) -> Result<ExamAnswer, AppError> {
    ensure_student_now(attempt.get_user(), db).await?;
    ensure_enrolled(exam, attempt.get_user(), db).await?;
    check_rejoin(exam, attempt)?;
    course_of(exam, db).await?.require_open(db).await?;
    let question = question_of_exam(exam.get_id(), question_id, db).await?;
    crate::domain::exam_answer::ExamAnswer::save(
        &question,
        attempt.get_user(),
        attempt.get_seq(),
        selected,
        text,
        db,
    )
    .await
}

/// One sitting by id — the exam room re-reads its own attempt this way.
pub async fn read(db: &Database, id: &ExamAttemptId) -> Result<Option<ExamAttempt>, AppError> {
    crate::db::exam_attempt::read(db, id).await
}

/// The student's current sitting — the highest `seq` for the pair.
pub async fn read_latest_for_user(
    db: &Database,
    exam: &ExamId,
    user: &UserId,
) -> Result<Option<ExamAttempt>, AppError> {
    crate::db::exam_attempt::read_latest_for_user(db, exam, user).await
}

/// Every sitting of `user` at `exam`, newest first.
pub async fn list_for_user(
    db: &Database,
    exam: &ExamId,
    user: &UserId,
) -> Result<Vec<ExamAttempt>, AppError> {
    crate::db::exam_attempt::list_for_user(db, exam, user).await
}

/// Every sitting of `user` nobody has submitted yet, across all exams.
pub async fn list_unfinished_for_user(
    db: &Database,
    user: &UserId,
) -> Result<Vec<ExamAttempt>, AppError> {
    crate::db::exam_attempt::list_unfinished_for_user(db, user).await
}

/// Every sitting at `exam`, newest id first.
pub async fn list_for_exam(db: &Database, exam: &ExamId) -> Result<Vec<ExamAttempt>, AppError> {
    crate::db::exam_attempt::list_for_exam(db, exam).await
}

/// Whether anyone has started this exam — the gate that freezes `mode`
/// edits once an attempt exists.
pub async fn any_for_exam(db: &Database, exam: &ExamId) -> Result<bool, AppError> {
    crate::db::exam_attempt::any_for_exam(db, exam).await
}

/// How many sittings `user` has used at `exam`.
pub async fn attempts_used(db: &Database, exam: &ExamId, user: &UserId) -> Result<u64, AppError> {
    Ok(crate::db::exam_attempt::list_for_user(db, exam, user)
        .await?
        .len() as u64)
}

/// Stamp the submission time. The caller has already checked the deadline
/// and that the attempt isn't finished.
pub async fn finish(db: &Database, attempt: ExamAttempt) -> Result<ExamAttempt, AppError> {
    crate::db::exam_attempt::finish(db, attempt).await
}

/// Stamp (or clear) the walked-out marker. The exam room sets it when the
/// student's socket closes mid-attempt and clears it when they come back.
pub async fn set_left(
    db: &Database,
    attempt: ExamAttempt,
    left_at: Option<Timestamp>,
) -> Result<ExamAttempt, AppError> {
    crate::db::exam_attempt::set_left(db, attempt, left_at).await
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
    use crate::domain::settings::Settings;

    /// The id of the question's second option — what these tests used to write
    /// as the index `1`.
    fn second_choice(question: &ExamQuestion) -> String {
        question.get_choices().unwrap()[1]
            .get_id()
            .as_str()
            .to_string()
    }

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

        let (_, created) = start(&db, &exam, &user).await.unwrap();
        assert!(created);
        assert_eq!(sat_total(&user, &db).await, 1);

        // Still in progress: this start returns the running sitting untouched.
        let (_, created) = start(&db, &exam, &user).await.unwrap();
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

        let (first, _) = start(&db, &exam, &user).await.unwrap();
        finish(&db, first).await.unwrap();
        let (second, created) = start(&db, &exam, &user).await.unwrap();
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

        let (winner, _) = start(&db, &exam, &user).await.unwrap();
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

        let (first, created) = start(&db, &exam, &user).await.unwrap();
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
        finish(&db, first).await.unwrap();

        // The retake lands as sitting #2 without touching sitting #1's answers.
        let (second, created) = start(&db, &exam, &user).await.unwrap();
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
        let (first, _) = start(&db, &exam, &user).await.unwrap();
        finish(&db, first).await.unwrap();
        let (winner, created) = start(&db, &exam, &user).await.unwrap();
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
        let (resumed, created) = start(&db, &exam, &user).await.unwrap();
        assert!(!created);
        assert_eq!(resumed.get_seq(), 2);
    }
}
