//! The `exam_result` table: the mark's claim-riding upsert, the per-sitting
//! history reads, and the refunding delete. The grade pre-flight (draft, kind,
//! grader/target walls) and the `EXAM_LOCK` lease live in
//! [`crate::service::exam_result`]; the mark newtype and the latest-per-pair
//! fold live in [`crate::domain::exam_result`].

use surrealdb::types::SurrealValue;

use crate::constant::{
    EXAM_RESULT_COUNT_FIELD, HIGH_MARK_MIN, HIGH_MARK_TOTAL_FIELD, MARKS_GIVEN_TOTAL_FIELD,
    REF_COUNT_FIELD, REF_RETIRED_FIELD,
};
use crate::database::{Database, transaction_with_retry};
use crate::db::cap;
use crate::domain::course::CourseId;
use crate::domain::exam::ExamId;
use crate::domain::exam_result::{
    ExamResult, ExamResultId, Mark, draft_error, kind_ref, latest_per_pair, retired_kind_error,
};
use crate::domain::user::UserId;
use crate::error::AppError;

/// The `THROW` marker the in-transaction kind claim aborts with — the mark's
/// own transaction refusing a name the school has retired.
const RETIRED_MARK: &str = "kind_retired";

/// A single user's result for an exam, if graded — the latest sitting's mark.
async fn find(db: &Database, exam: &ExamId, user: &UserId) -> Result<Option<ExamResult>, AppError> {
    // Grade-of-record is the latest sitting's mark: the highest seq wins.
    let mut result = db
        .query(
            "SELECT * FROM exam_result WHERE exam = $ex AND user = $usr
             ORDER BY seq DESC LIMIT 1",
        )
        .bind(("ex", exam.record()))
        .bind(("usr", user.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<ExamResult>>(0)?.into_iter().next())
}

/// Record (or overwrite) `user`'s mark for the `seq`th sitting of `exam`.
/// One row per (exam, user, seq), keyed by a deterministic composite id so
/// this is a single atomic UPSERT — concurrent grades for the same sitting
/// converge on one row instead of racing the unique index into a 500.
/// Grading a retake writes a fresh mark at the current seq and never touches
/// prior sittings' marks; the latest seq is the grade-of-record.
///
/// `kind` is the exam's kind, and this is where a mark takes its reference
/// on it — claimed *inside the mark's own transaction*, and only on the
/// branch that is about to add a row. A kind the school has retired refuses
/// the claim, which is the same invariant the settings guard enforces from
/// the other side. Claimed before the write and released after, the pair had
/// two crash windows either side of the write that leaked a reference (a
/// kind frozen out of the settings for good) and an exam's `result_count`;
/// riding the transaction, both counters land exactly when the row does.
///
/// The exam's own `result_count` is claimed in the same breath, and it is
/// what a kind change is refused against: the exam PATCH pins that counter,
/// so a mark landing while it decides cannot slip past its gate and leave
/// itself counted under a kind its exam no longer carries.
///
/// An overwrite (a regrade of a sitting already marked) claims *nothing*:
/// one mark, one reference, so the branch that finds a row simply leaves
/// both counters where they are — there is no claim to give back, and so no
/// window in which a crash could fail to give it.
///
/// The grader's `marks_given_total` rides that same first-time branch: a
/// regrade of a sitting moves it however often it is run, while a retake,
/// being a distinct `seq` and so a distinct row, counts on its own.
///
/// The student's `high_mark_total` cannot ride it, because it is not a
/// count of rows but a count of rows *at or above* [`HIGH_MARK_MIN`], and a
/// regrade moves a mark across that line without adding or removing a row.
/// It moves by the difference between the mark about to be stored and the
/// one already there — `+1` crossing up, `-1` crossing down, nothing at all
/// when both sides sit on the same side of the line, so an overwrite still
/// costs the student's row no write unless the answer actually changed.
/// Decided on the first branch alone it drifted upward without bound: 95
/// credited, regraded to 40 (no row added, so no move), then deleted, whose
/// refund reads the *stored* mark and so found nothing to give back — a
/// counter of one over zero stored high marks, looped into a permanent
/// badge. Both counters are written field-scoped: a user row also carries
/// admin-owned data (`role`) that a whole-row save would revert.
pub async fn grade(
    db: &Database,
    exam: &ExamId,
    user: &UserId,
    seq: i64,
    mark: Mark,
    graded_by: &UserId,
    kind: &str,
) -> Result<ExamResult, AppError> {
    let result = ExamResult {
        id: ExamResultId::composite(exam, user, seq),
        exam: exam.clone(),
        user: user.clone(),
        seq,
        mark,
        graded_by: graded_by.clone(),
    };
    // The "exists" and "not a draft" gates ride in the same transaction as
    // the mark, the mirror of the re-draft gate on
    // `crate::db::exam::update_if_unchanged`: between them, a mark and a
    // re-draft racing each other can only ever leave one of the two applied,
    // whichever process either ran in. The existence gate is not
    // redundant with the draft one — a deleted exam reads NONE, which is
    // *falsy*, so the draft gate alone waved a mark onto an exam that no
    // longer existed. The caller's pre-flight check answers the same 409
    // one round trip earlier. It is also what makes a separate "did the
    // exam survive my claim?" check moot: the claim now sits behind this
    // gate instead of in front of it.
    //
    // `$before` is the sitting's *previous mark*, and it answers both
    // questions off one read: `mark` is a mandatory column on a schemafull
    // row, so `NONE` there means no row at all — the claim branch — while a
    // number is what the student's counter takes its difference against.
    // Read one statement before the counters inside the same transaction:
    // read anywhere else it would be a guess about a row two graders may be
    // writing at once, and the difference would be taken against a mark
    // some other round had already replaced.
    //
    // Re-sent while the store answers "conflict, retry": the gates read a
    // column an exam PATCH writes, and the counters are the ones a
    // concurrent delete gives back, so this contends on three records by
    // design. Sound to re-send because no statement in it can legitimately
    // answer "already exists": the counter writes are `UPDATE`/`UPSERT` on
    // ids no rival can collide *into*, and the mark's own UPSERT is on a
    // deterministic id bijective with the `exam_result_exam_user_seq`
    // unique tuple — `key::sitting(exam, user, seq)` *is* that tuple, so
    // the index entry can only ever point at the row the id already names
    // and the write resolves onto it. A lost round aborts having written
    // nothing, counters included.
    let _guard = cap::counter_lock().await;
    let (mut written, mut errors) = transaction_with_retry(
        db,
        &format!(
            "BEGIN TRANSACTION;
             IF (SELECT VALUE id FROM ONLY $exam) IS NONE {{ THROW 'exam_missing' }};
             IF (SELECT VALUE draft FROM ONLY $exam) {{ THROW 'exam_draft' }};
             LET $before = (SELECT VALUE mark FROM ONLY $id);
             IF $before = NONE {{
                 LET $kind = (UPSERT $kref SET {REF_COUNT_FIELD} = ({REF_COUNT_FIELD} ?? 0) + 1
                     WHERE {REF_RETIRED_FIELD} != true RETURN VALUE id);
                 IF array::len($kind) = 0 {{ THROW '{RETIRED_MARK}' }};
                 UPDATE $exam SET {EXAM_RESULT_COUNT_FIELD} =
                     ({EXAM_RESULT_COUNT_FIELD} ?? 0) + 1;
                 UPDATE $grader SET {MARKS_GIVEN_TOTAL_FIELD} =
                     ({MARKS_GIVEN_TOTAL_FIELD} ?? 0) + 1;
             }};
             LET $high = (IF $result.mark >= {HIGH_MARK_MIN} {{ 1 }} ELSE {{ 0 }})
                 - (IF ($before ?? -1) >= {HIGH_MARK_MIN} {{ 1 }} ELSE {{ 0 }});
             IF $high != 0 {{
                 UPDATE $student SET {HIGH_MARK_TOTAL_FIELD} =
                     math::max([({HIGH_MARK_TOTAL_FIELD} ?? 0) + $high, 0])
             }};
             LET $after = (UPSERT $id CONTENT $result RETURN AFTER);
             RETURN $after[0];
             COMMIT TRANSACTION;"
        ),
        &[
            ("exam".into(), exam.record().into_value()),
            ("id".into(), result.id.record().into_value()),
            ("kref".into(), kind_ref(kind).into_value()),
            ("grader".into(), graded_by.record().into_value()),
            ("student".into(), user.record().into_value()),
            ("result".into(), result.into_value()),
        ],
        &["exam_missing", "exam_draft", RETIRED_MARK],
    )
    .await?;
    // An aborted transaction errors every slot and only the THROW's own
    // slot names the marker, so a refusal is read by marker while a lost
    // round was already re-sent — never reported as a 500.
    let thrown = |marker: &str| {
        errors
            .values()
            .any(|error| error.to_string().contains(marker))
    };
    if thrown("exam_missing") {
        return Err(AppError::NotFound);
    }
    if thrown("exam_draft") {
        return Err(draft_error());
    }
    if thrown(RETIRED_MARK) {
        return Err(retired_kind_error(kind));
    }
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    // The trailing `RETURN` is always the last statement before `COMMIT`,
    // so its slot follows the statement count instead of a hand-kept
    // number — see `db::exam::delete` for the bug the hand-kept one caused.
    // Probed on crate 3.2.3: an `IF { … }` block occupies exactly one slot
    // however many statements it holds, and one whether or not it is taken.
    let slot = written.num_statements().saturating_sub(2);
    written
        .take::<Vec<ExamResult>>(slot)?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::Internal("failed to record exam result".into()))
}

/// The user's graded results restricted to one course's exams — the raw
/// rows behind the per-course block of the marks report.
pub async fn list_for_user_in_course(
    db: &Database,
    course: &CourseId,
    user: &UserId,
) -> Result<Vec<ExamResult>, AppError> {
    let mut result = db
        .query(
            "SELECT * FROM exam_result WHERE user = $usr
             AND exam IN (SELECT VALUE id FROM exam WHERE course = $course)
             ORDER BY id DESC",
        )
        .bind(("usr", user.record()))
        .bind(("course", course.record()))
        .await?
        .check()?;
    Ok(latest_per_pair(result.take::<Vec<ExamResult>>(0)?))
}

pub async fn list_for_exam(db: &Database, exam: &ExamId) -> Result<Vec<ExamResult>, AppError> {
    let mut result = db
        .query("SELECT * FROM exam_result WHERE exam = $ex ORDER BY id DESC")
        .bind(("ex", exam.record()))
        .await?
        .check()?;
    Ok(latest_per_pair(result.take::<Vec<ExamResult>>(0)?))
}

/// Every sitting's mark for one (exam, user) pair, oldest first — the
/// per-attempt grade history behind the FE's attempt-by-attempt view.
pub async fn list_all_for_exam_user(
    db: &Database,
    exam: &ExamId,
    user: &UserId,
) -> Result<Vec<ExamResult>, AppError> {
    let mut result = db
        .query(
            "SELECT * FROM exam_result WHERE exam = $ex AND user = $usr
             ORDER BY seq ASC",
        )
        .bind(("ex", exam.record()))
        .bind(("usr", user.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<ExamResult>>(0)?)
}

/// A single user's result for an exam, if graded.
pub async fn read_for_user(
    db: &Database,
    exam: &ExamId,
    user: &UserId,
) -> Result<Option<ExamResult>, AppError> {
    find(db, exam, user).await
}

/// Delete every sitting's mark for one (exam, user) pair. `kind` is the
/// exam's: the marks give their references back, so a kind nothing is
/// graded under any more can leave the settings again.
///
/// The two badge counters come back the same way, and for a reason the
/// other counters do not need: [`grade`] credits them on the
/// branch that finds no row, and a deleted row *is* no row. Left standing,
/// ungrade-then-regrade credited a second time off one stored mark, and
/// looped it minted a badge from a single exam — permanently, since an
/// award is never revoked. Each is given back off the rows this statement
/// actually deleted: `marks_given_total` to each row's *own* grader (two
/// teachers can hold two sittings of the same pair), and `high_mark_total`
/// once per removed row that cleared [`HIGH_MARK_MIN`], read off the
/// `BEFORE` image rather than re-derived. That last one is why
/// [`grade`] has to move the student's counter on a *regrade*
/// too: this end reads the stored mark, so the other end must be decided by
/// the stored mark as well, or a mark walked across the line between the
/// two leaves a credit no delete can find to give back.
pub async fn remove(
    db: &Database,
    exam: &ExamId,
    user: &UserId,
    kind: &str,
) -> Result<Option<ExamResult>, AppError> {
    // Both counters go back *inside* the delete's own transaction, driven
    // off the rows this statement actually deleted. Counted outside it, a
    // cascade (exam or course delete) taking the same rows in the gap would
    // release them a second time, and on a kind another exam still grades
    // under, one release too many reads as one mark too few — a kind
    // wrongly free to leave the settings.
    //
    // Re-sent while the store answers "conflict, retry": the counters this
    // touches are the ones a concurrent grade claims, so a lost round is
    // routine and aborts having written nothing. `check()` cannot be used
    // to read the outcome — it takes the *lowest*-slot error, and an
    // aborted transaction's generic "not executed" sibling masked the real
    // conflict, the exact defect deleted from `db::exam::delete`. No THROW
    // of its own, and only `DELETE`/`UPDATE` inside, so nothing here can
    // answer "already exists" and make a retry unsound.
    let (mut result, mut errors) = transaction_with_retry(
        db,
        &format!(
            "BEGIN TRANSACTION;
             LET $gone = (DELETE exam_result WHERE exam = $ex AND user = $usr RETURN BEFORE);
             IF array::len($gone) > 0 {{
                 UPDATE type::record('kind_ref', $kind) SET {REF_COUNT_FIELD} =
                     math::max([({REF_COUNT_FIELD} ?? 0) - array::len($gone), 0]);
                 UPDATE $ex SET {EXAM_RESULT_COUNT_FIELD} =
                     math::max([({EXAM_RESULT_COUNT_FIELD} ?? 0) - array::len($gone), 0]);
                 LET $high = array::len($gone[WHERE mark >= {HIGH_MARK_MIN}]);
                 IF $high > 0 {{
                     UPDATE $usr SET {HIGH_MARK_TOTAL_FIELD} =
                         math::max([({HIGH_MARK_TOTAL_FIELD} ?? 0) - $high, 0])
                 }};
                 FOR $row IN $gone {{
                     LET $grader = $row.graded_by;
                     UPDATE $grader SET {MARKS_GIVEN_TOTAL_FIELD} =
                         math::max([({MARKS_GIVEN_TOTAL_FIELD} ?? 0) - 1, 0])
                 }};
             }};
             RETURN $gone;
             COMMIT TRANSACTION;"
        ),
        &[
            ("ex".into(), exam.record().into_value()),
            ("usr".into(), user.record().into_value()),
            ("kind".into(), kind.to_string().into_value()),
        ],
        &[],
    )
    .await?;
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    // `RETURN` is the last statement before `COMMIT`; its slot follows the
    // statement count, as in `db::exam::delete`.
    let slot = result.num_statements().saturating_sub(2);
    let removed = result.take::<Vec<ExamResult>>(slot)?;
    Ok(removed.into_iter().next())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_mem;

    #[tokio::test]
    async fn retakes_keep_a_mark_per_sitting_with_the_latest_as_grade_of_record() {
        let db = init_mem().await.unwrap();
        let exam = ExamId::from_key("01TESTEXAMAAAAAAAAAAAAAAAA");
        let user = UserId::from_key("01TESTSTUDENTAAAAAAAAAAAAA");
        let teacher = UserId::from_key("01TESTTEACHERAAAAAAAAAAAAA");
        // The exam has to be real: a mark is refused on one that isn't.
        db.query(
            "CREATE $ex SET creator = $t, course = course:c, title = 't',
             description = '', kind = 'midterm'",
        )
        .bind(("ex", exam.record()))
        .bind(("t", teacher.record()))
        .await
        .unwrap()
        .check()
        .unwrap();

        // Grade sitting #1, then a retake as sitting #2 — two rows, not one.
        super::grade(
            &db,
            &exam,
            &user,
            1,
            Mark::try_new(40).unwrap(),
            &teacher,
            "midterm",
        )
        .await
        .unwrap();
        let second = super::grade(
            &db,
            &exam,
            &user,
            2,
            Mark::try_new(90).unwrap(),
            &teacher,
            "midterm",
        )
        .await
        .unwrap();
        assert_eq!(second.get_seq(), 2);

        // Grade-of-record is the latest sitting's mark.
        let latest = read_for_user(&db, &exam, &user)
            .await
            .unwrap()
            .expect("graded");
        assert_eq!(latest.get_mark().as_i64(), 90);
        assert_eq!(latest.get_seq(), 2);

        // The roster yields exactly one row per pair — the latest.
        let roster = list_for_exam(&db, &exam).await.unwrap();
        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].get_mark().as_i64(), 90);

        // History keeps both sittings, oldest first.
        let history = list_all_for_exam_user(&db, &exam, &user).await.unwrap();
        assert_eq!(
            history
                .iter()
                .map(|r| r.get_mark().as_i64())
                .collect::<Vec<_>>(),
            vec![40, 90]
        );

        // Deleting the pair removes every sitting.
        remove(&db, &exam, &user, "midterm").await.unwrap();
        assert!(
            list_all_for_exam_user(&db, &exam, &user)
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A deleted exam reads NONE, and NONE is falsy — the draft gate alone let
    /// a mark land on an exam that no longer existed.
    #[tokio::test]
    async fn a_mark_is_refused_on_an_exam_that_no_longer_exists() {
        let db = init_mem().await.unwrap();
        let exam = ExamId::from_key("01TESTGONEEXAMAAAAAAAAAAAA");
        let user = UserId::from_key("01TESTSTUDENTAAAAAAAAAAAAA");
        let teacher = UserId::from_key("01TESTTEACHERAAAAAAAAAAAAA");

        let refused = super::grade(
            &db,
            &exam,
            &user,
            1,
            Mark::try_new(40).unwrap(),
            &teacher,
            "midterm",
        )
        .await;
        assert!(matches!(refused, Err(AppError::NotFound)), "{refused:?}");
        assert!(
            list_all_for_exam_user(&db, &exam, &user)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(counters(&db, "midterm").await, (0, 0));
    }

    /// The two counters, re-read out of the store — never off a return value,
    /// which the in-memory engine forges wins on (see `cap::CLAIM_LOCK`). A
    /// missing `kind_ref` row is a zero, which is exactly how the backfill
    /// treats it (`tests/persistence.rs` — no zero pass, deliberately).
    async fn counters(db: &Database, kind: &str) -> (i64, i64) {
        let mut result = db
            .query(
                "SELECT VALUE count ?? 0 FROM kind_ref WHERE record::id(id) = $kind;
                 SELECT VALUE result_count ?? 0 FROM exam;",
            )
            .bind(("kind", kind.to_string()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let kinds: Vec<i64> = result.take(0).unwrap();
        let exams: Vec<i64> = result.take(1).unwrap();
        (
            kinds.into_iter().next().unwrap_or(0),
            exams.into_iter().next().unwrap_or(0),
        )
    }

    async fn an_exam(db: &Database, exam: &ExamId, kind: &str) {
        db.query(
            "CREATE $ex SET creator = user:t, course = course:c, title = 't',
             description = '', kind = $kind",
        )
        .bind(("ex", exam.record()))
        .bind(("kind", kind.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
    }

    const STUDENT: &str = "01TESTSTUDENTAAAAAAAAAAAAA";
    const TEACHER: &str = "01TESTTEACHERAAAAAAAAAAAAA";

    /// The tests' handle on [`super::grade`] with the grader and target
    /// pinned to the two test people.
    async fn grade(
        db: &Database,
        exam: &ExamId,
        seq: i64,
        mark: i64,
        kind: &str,
    ) -> Result<ExamResult, AppError> {
        super::grade(
            db,
            exam,
            &UserId::from_key(STUDENT),
            seq,
            Mark::try_new(mark).unwrap(),
            &UserId::from_key(TEACHER),
            kind,
        )
        .await
    }

    /// Both counters ride the mark's own transaction: a new mark claims each
    /// exactly once, and a *regrade* of the same sitting claims neither — one
    /// mark, one reference. Claimed-then-released around the write, a regrade
    /// drifted them upward and froze the kind out of the settings for good.
    #[tokio::test]
    async fn a_new_mark_moves_both_counters_and_a_regrade_moves_neither() {
        let db = init_mem().await.unwrap();
        let exam = ExamId::from_key("01TESTEXAMCOUNTAAAAAAAAAAA");
        an_exam(&db, &exam, "midterm").await;

        grade(&db, &exam, 1, 40, "midterm").await.unwrap();
        assert_eq!(counters(&db, "midterm").await, (1, 1));

        // Same sitting, new mark: an overwrite, so neither counter moves.
        let regraded = grade(&db, &exam, 1, 90, "midterm").await.unwrap();
        assert_eq!(regraded.get_mark().as_i64(), 90);
        assert_eq!(counters(&db, "midterm").await, (1, 1));

        // A retake is a second row, so it does claim.
        grade(&db, &exam, 2, 70, "midterm").await.unwrap();
        assert_eq!(counters(&db, "midterm").await, (2, 2));
    }

    /// The two badge counters, re-read out of the store: the grader's marks
    /// given and the student's high marks.
    async fn badge_counters(db: &Database) -> (i64, i64) {
        let mut result = db
            .query(format!(
                "SELECT VALUE ({MARKS_GIVEN_TOTAL_FIELD} ?? 0) FROM $grader;
                 SELECT VALUE ({HIGH_MARK_TOTAL_FIELD} ?? 0) FROM $student;"
            ))
            .bind(("grader", UserId::from_key(TEACHER).record()))
            .bind(("student", UserId::from_key(STUDENT).record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let given: Vec<i64> = result.take(0).unwrap();
        let high: Vec<i64> = result.take(1).unwrap();
        (
            given.into_iter().next().unwrap_or(0),
            high.into_iter().next().unwrap_or(0),
        )
    }

    /// The rows a counter needs to land on — `UPDATE` writes nothing to a user
    /// that does not exist, so the badge tests must make both real.
    async fn the_two_people(db: &Database) {
        for key in [TEACHER, STUDENT] {
            db.query("CREATE $usr SET username = $name, password_hash = 'x'")
                .bind(("usr", UserId::from_key(key).record()))
                .bind(("name", key.to_string()))
                .await
                .unwrap()
                .check()
                .unwrap();
        }
    }

    /// A first grade credits the grader once and the student only when the mark
    /// clears the line; a regrade of the same sitting credits neither again,
    /// while a retake — a distinct seq, so a distinct row — counts on its own.
    #[tokio::test]
    async fn a_grade_credits_the_grader_once_and_a_high_mark_the_student() {
        let db = init_mem().await.unwrap();
        let exam = ExamId::from_key("01TESTEXAMBADGEAAAAAAAAAAA");
        an_exam(&db, &exam, "midterm").await;
        the_two_people(&db).await;

        grade(&db, &exam, 1, 90, "midterm").await.unwrap();
        assert_eq!(badge_counters(&db).await, (1, 1), "at the line earns it");

        // Same sitting graded again: neither counter may move, however high.
        grade(&db, &exam, 1, 100, "midterm").await.unwrap();
        assert_eq!(badge_counters(&db).await, (1, 1), "a regrade moves nothing");

        // A retake below the line: the grader is credited, the student is not.
        grade(&db, &exam, 2, 89, "midterm").await.unwrap();
        assert_eq!(badge_counters(&db).await, (2, 1), "89 is under the line");

        // …and a high-scoring retake counts as its own sitting.
        grade(&db, &exam, 3, 95, "midterm").await.unwrap();
        assert_eq!(badge_counters(&db).await, (3, 2));
    }

    /// Ungrading gives both badge counters back, so grade → ungrade → regrade
    /// nets to one credit however often it is run. Crediting the grade without
    /// refunding the delete let that loop mint a badge off a single exam — and
    /// an award, once earned, is never taken back.
    #[tokio::test]
    async fn ungrading_gives_the_badge_counters_back() {
        let db = init_mem().await.unwrap();
        let exam = ExamId::from_key("01TESTEXAMUNGRADEAAAAAAAAA");
        an_exam(&db, &exam, "midterm").await;
        the_two_people(&db).await;
        let student = UserId::from_key(STUDENT);

        for _ in 0..3 {
            grade(&db, &exam, 1, 95, "midterm").await.unwrap();
            assert_eq!(badge_counters(&db).await, (1, 1));
            remove(&db, &exam, &student, "midterm")
                .await
                .unwrap()
                .expect("the mark was there");
            assert_eq!(badge_counters(&db).await, (0, 0), "the loop kept a credit");
            // The kind's reference and the exam's count come back as before.
            assert_eq!(counters(&db, "midterm").await, (0, 0));
        }

        // Every removed sitting is refunded, and only the ones that took: the
        // grader gets both back, the student only the mark that cleared the line.
        grade(&db, &exam, 1, 40, "midterm").await.unwrap();
        grade(&db, &exam, 2, 95, "midterm").await.unwrap();
        assert_eq!(badge_counters(&db).await, (2, 1));
        remove(&db, &exam, &student, "midterm")
            .await
            .unwrap()
            .expect("both sittings were there");
        assert_eq!(badge_counters(&db).await, (0, 0));

        // Nothing to remove is nothing to give back — the floor holds.
        assert!(
            remove(&db, &exam, &student, "midterm")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(badge_counters(&db).await, (0, 0));
    }

    /// A regrade walks the *same* sitting across [`HIGH_MARK_MIN`], and the
    /// student's counter has to walk with it — the delete refunds off the mark
    /// it finds stored, so a credit decided by a mark that is no longer there
    /// is a credit nothing can give back. Left on the first-write branch alone,
    /// grade-high → regrade-low → delete kept a phantom high mark every round
    /// and looped it into a permanent badge.
    #[tokio::test]
    async fn a_regrade_across_the_line_moves_the_student_counter_with_it() {
        let db = init_mem().await.unwrap();
        let exam = ExamId::from_key("01TESTEXAMREGRADEAAAAAAAAA");
        an_exam(&db, &exam, "midterm").await;
        the_two_people(&db).await;
        let student = UserId::from_key(STUDENT);

        // Up across the line: no row is added, but a high mark now exists.
        grade(&db, &exam, 1, 40, "midterm").await.unwrap();
        assert_eq!(badge_counters(&db).await, (1, 0), "40 is under the line");
        grade(&db, &exam, 1, 95, "midterm").await.unwrap();
        assert_eq!(badge_counters(&db).await, (1, 1), "the regrade crossed up");
        remove(&db, &exam, &student, "midterm")
            .await
            .unwrap()
            .expect("the mark was there");
        assert_eq!(badge_counters(&db).await, (0, 0));

        // Down across it: the credit goes back the moment the mark does, so
        // the delete has nothing left to refund and the counter still lands
        // on zero rather than under it.
        grade(&db, &exam, 1, 95, "midterm").await.unwrap();
        assert_eq!(badge_counters(&db).await, (1, 1));
        grade(&db, &exam, 1, 40, "midterm").await.unwrap();
        assert_eq!(
            badge_counters(&db).await,
            (1, 0),
            "the regrade crossed down"
        );
        remove(&db, &exam, &student, "midterm")
            .await
            .unwrap()
            .expect("the mark was there");
        assert_eq!(badge_counters(&db).await, (0, 0));

        // Both sides of a regrade above the line: nothing crosses, nothing moves.
        grade(&db, &exam, 1, 90, "midterm").await.unwrap();
        grade(&db, &exam, 1, 100, "midterm").await.unwrap();
        assert_eq!(badge_counters(&db).await, (1, 1), "90 → 100 stays one");

        // The loop that minted the badge: three rounds of high → low → delete
        // must leave the counter exactly where it started.
        for _ in 0..3 {
            grade(&db, &exam, 1, 95, "midterm").await.unwrap();
            grade(&db, &exam, 1, 40, "midterm").await.unwrap();
            remove(&db, &exam, &student, "midterm")
                .await
                .unwrap()
                .expect("the mark was there");
        }
        assert_eq!(
            badge_counters(&db).await,
            (0, 0),
            "the loop kept a high mark with no high mark stored"
        );
    }

    /// The other half of the same asymmetry: a refund read off a stored mark
    /// that was never credited eats a credit earned somewhere else. One exam
    /// holds a genuine high mark while a second is graded low, regraded high
    /// and deleted — the first exam's credit must still be standing.
    #[tokio::test]
    async fn one_exams_regrade_never_eats_another_exams_high_mark() {
        let db = init_mem().await.unwrap();
        let first = ExamId::from_key("01TESTEXAMSTEALONEAAAAAAAA");
        let second = ExamId::from_key("01TESTEXAMSTEALTWOAAAAAAAA");
        an_exam(&db, &first, "midterm").await;
        an_exam(&db, &second, "midterm").await;
        the_two_people(&db).await;
        let student = UserId::from_key(STUDENT);

        grade(&db, &first, 1, 95, "midterm").await.unwrap();
        assert_eq!(
            badge_counters(&db).await,
            (1, 1),
            "earned on the first exam"
        );

        grade(&db, &second, 1, 40, "midterm").await.unwrap();
        grade(&db, &second, 1, 95, "midterm").await.unwrap();
        assert_eq!(badge_counters(&db).await, (2, 2));
        remove(&db, &second, &student, "midterm")
            .await
            .unwrap()
            .expect("the mark was there");
        assert_eq!(
            badge_counters(&db).await,
            (1, 1),
            "the first exam's high mark was eaten"
        );
    }

    /// A refused mark leaves the badge counters where the other two are left.
    #[tokio::test]
    async fn a_refused_mark_credits_nobody() {
        let db = init_mem().await.unwrap();
        let exam = ExamId::from_key("01TESTEXAMBADGEDRAFTAAAAAA");
        an_exam(&db, &exam, "midterm").await;
        the_two_people(&db).await;
        db.query("UPDATE $ex SET draft = true")
            .bind(("ex", exam.record()))
            .await
            .unwrap()
            .check()
            .unwrap();

        assert!(grade(&db, &exam, 1, 100, "midterm").await.is_err());
        assert_eq!(badge_counters(&db).await, (0, 0));
    }

    /// A retired kind refuses the claim from inside the transaction, which
    /// takes the mark and the exam's counter down with it — the settings guard
    /// enforcing the same invariant from the other side.
    #[tokio::test]
    async fn a_retired_kind_refuses_the_mark_and_leaves_both_counters_alone() {
        let db = init_mem().await.unwrap();
        let exam = ExamId::from_key("01TESTEXAMRETIREDAAAAAAAAA");
        an_exam(&db, &exam, "midterm").await;
        assert!(cap::retire(&kind_ref("midterm"), &db).await.unwrap());

        let refused = grade(&db, &exam, 1, 40, "midterm").await;
        assert!(
            matches!(&refused, Err(AppError::ConflictOwned(message))
                if message.contains("has been removed from the school's settings")),
            "{refused:?}"
        );
        assert!(
            list_for_exam(&db, &exam).await.unwrap().is_empty(),
            "the mark must not have landed"
        );
        assert_eq!(counters(&db, "midterm").await, (0, 0));
    }

    /// A draft exam refuses the mark, and the counters stay put — the gate
    /// runs before the claim now, so there is nothing to give back.
    #[tokio::test]
    async fn a_draft_exam_refuses_the_mark_and_leaves_both_counters_alone() {
        let db = init_mem().await.unwrap();
        let exam = ExamId::from_key("01TESTEXAMDRAFTAAAAAAAAAAA");
        an_exam(&db, &exam, "midterm").await;
        db.query("UPDATE $ex SET draft = true")
            .bind(("ex", exam.record()))
            .await
            .unwrap()
            .check()
            .unwrap();

        let refused = grade(&db, &exam, 1, 40, "midterm").await;
        assert!(matches!(refused, Err(AppError::Conflict(_))), "{refused:?}");
        assert!(list_for_exam(&db, &exam).await.unwrap().is_empty());
        assert_eq!(counters(&db, "midterm").await, (0, 0));
    }
}
