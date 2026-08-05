use std::collections::HashMap;

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{
    EXAM_RESULT_COUNT_FIELD, EXAM_RESULT_TABLE, HIGH_MARK_MIN, HIGH_MARK_TOTAL_FIELD,
    KIND_REF_TABLE, MARKS_GIVEN_TOTAL_FIELD, REF_COUNT_FIELD, REF_RETIRED_FIELD,
};
use crate::database::{Database, transaction_with_retry};
use crate::domain::cap;
use crate::domain::course::CourseId;
use crate::domain::exam::ExamId;
use crate::domain::key;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_mark;

/// The one spelling of "this exam is hidden", shared by the grade handler's
/// pre-flight gate and the in-transaction guard on the mark write.
pub(crate) fn draft_error() -> AppError {
    AppError::Conflict("this exam is a draft — publish it before grading")
}

/// The reference counter for one exam kind — how many marks are written under
/// that name, and whether the school has retired it (see
/// [`crate::domain::cap`]). Keyed by the name itself: the kind is snapshotted
/// text on the exam, and this row is what makes "a kind nothing is graded under
/// may be removed" a decision the database takes, not a count a concurrent mark
/// can invalidate.
pub(crate) fn kind_ref(kind: &str) -> RecordId {
    RecordId::new(KIND_REF_TABLE, kind)
}

/// The refusal a mark meets once its kind has left the school's list. The
/// mirror of the settings-side 409: whichever of the two writes reaches the
/// counter first, the other is told the name is no longer usable.
pub(crate) fn retired_kind_error(kind: &str) -> AppError {
    AppError::ConflictOwned(format!(
        "the '{kind}' exam kind has been removed from the school's settings — \
         add it back before grading this exam"
    ))
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ExamResultId(RecordId);

impl ExamResultId {
    /// A deterministic id for the (exam, user, seq) triple — one mark row per
    /// sitting, so grading is a single atomic UPSERT with no find-then-insert
    /// race. See [`key::sitting`] for the key shape and why the first sitting
    /// stays bare.
    pub fn composite(exam: &ExamId, user: &UserId, seq: i64) -> Self {
        let key = key::sitting(exam.key(), user.key(), seq);
        Self(RecordId::new(EXAM_RESULT_TABLE, key))
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

/// A validated exam mark. Stored as an `int`, held to `[MIN_MARK, MAX_MARK]`.
/// Kept in its own row — an exam result is never a course note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, SurrealValue)]
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

/// The `THROW` marker the in-transaction kind claim aborts with — the mark's
/// own transaction refusing a name the school has retired.
const RETIRED_MARK: &str = "kind_retired";

#[derive(Debug, Clone, SurrealValue)]
pub struct ExamResult {
    id: ExamResultId,
    exam: ExamId,
    user: UserId,
    seq: i64,
    mark: Mark,
    graded_by: UserId,
}

impl ExamResult {
    pub fn get_id(&self) -> &ExamResultId {
        &self.id
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

    async fn find(
        exam: &ExamId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<ExamResult>, AppError> {
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

    /// The latest-seq mark per (exam, user) pair, preserving the outer list's
    /// row order (first appearance of each pair). Grade-of-record is the latest
    /// attempt's mark, so a pair with retakes collapses to its highest seq. Done
    /// in Rust rather than SQL: SurrealDB's `GROUP BY` can't return the whole
    /// row that carries the max, and id-order can't stand in for seq-order once
    /// seq reaches two digits.
    fn latest_per_pair(rows: Vec<ExamResult>) -> Vec<ExamResult> {
        let mut order: Vec<(String, String)> = Vec::new();
        let mut best: HashMap<(String, String), ExamResult> = HashMap::new();
        for row in rows {
            let key = (row.exam.key().to_string(), row.user.key().to_string());
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

    /// A single user's result for an exam, if graded.
    pub async fn read_for_user(
        exam: &ExamId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<ExamResult>, AppError> {
        Self::find(exam, user, db).await
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
    /// The two badge counters ride that same first-time branch: the grader's
    /// `marks_given_total`, and — only when the mark clears
    /// [`HIGH_MARK_MIN`] — the student's `high_mark_total`. So a regrade of a
    /// sitting moves neither however often it is run, while a retake, being a
    /// distinct `seq` and so a distinct row, counts on its own. They are
    /// written field-scoped: a user row also carries admin-owned data (`role`)
    /// that a whole-row save would revert.
    pub async fn grade(
        exam: &ExamId,
        user: &UserId,
        seq: i64,
        mark: Mark,
        graded_by: &UserId,
        kind: &str,
        db: &Database,
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
        // `Exam::update_if_unchanged`: between them, a mark and a re-draft
        // racing each other can only ever leave one of the two applied,
        // whichever process either ran in. The existence gate is not
        // redundant with the draft one — a deleted exam reads NONE, which is
        // *falsy*, so the draft gate alone waved a mark onto an exam that no
        // longer existed. The caller's pre-flight check answers the same 409
        // one round trip earlier. It is also what makes a separate "did the
        // exam survive my claim?" check moot: the claim now sits behind this
        // gate instead of in front of it.
        //
        // Whether the row was already there decides both counters, and it is
        // read one statement before them inside the same transaction — read
        // anywhere else it would be a guess about a row two graders may be
        // writing at once.
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
        // Below the line the student's counter is left out of the statement
        // list entirely, rather than added to by a zero: a low mark is most
        // marks, and it must not write the student's row at all.
        let high_mark = if mark.as_i64() >= HIGH_MARK_MIN {
            format!(
                "UPDATE $student SET {HIGH_MARK_TOTAL_FIELD} =
                         ({HIGH_MARK_TOTAL_FIELD} ?? 0) + 1;"
            )
        } else {
            String::new()
        };
        let _guard = cap::counter_lock().await;
        let (mut written, mut errors) = transaction_with_retry(
            db,
            &format!(
                "BEGIN TRANSACTION;
                 IF (SELECT VALUE id FROM ONLY $exam) IS NONE {{ THROW 'exam_missing' }};
                 IF (SELECT VALUE draft FROM ONLY $exam) {{ THROW 'exam_draft' }};
                 LET $before = (SELECT VALUE id FROM ONLY $id);
                 IF $before = NONE {{
                     LET $kind = (UPSERT $kref SET {REF_COUNT_FIELD} = ({REF_COUNT_FIELD} ?? 0) + 1
                         WHERE {REF_RETIRED_FIELD} != true RETURN VALUE id);
                     IF array::len($kind) = 0 {{ THROW '{RETIRED_MARK}' }};
                     UPDATE $exam SET {EXAM_RESULT_COUNT_FIELD} =
                         ({EXAM_RESULT_COUNT_FIELD} ?? 0) + 1;
                     UPDATE $grader SET {MARKS_GIVEN_TOTAL_FIELD} =
                         ({MARKS_GIVEN_TOTAL_FIELD} ?? 0) + 1;
                     {high_mark}
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
        // number — see `Exam::delete` for the bug the hand-kept one caused.
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
        course: &CourseId,
        user: &UserId,
        db: &Database,
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
        Ok(Self::latest_per_pair(result.take::<Vec<ExamResult>>(0)?))
    }

    pub async fn list_for_exam(exam: &ExamId, db: &Database) -> Result<Vec<ExamResult>, AppError> {
        let mut result = db
            .query("SELECT * FROM exam_result WHERE exam = $ex ORDER BY id DESC")
            .bind(("ex", exam.record()))
            .await?
            .check()?;
        Ok(Self::latest_per_pair(result.take::<Vec<ExamResult>>(0)?))
    }

    /// Every sitting's mark for one (exam, user) pair, oldest first — the
    /// per-attempt grade history behind the FE's attempt-by-attempt view.
    pub async fn list_all_for_exam_user(
        exam: &ExamId,
        user: &UserId,
        db: &Database,
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

    /// Delete every sitting's mark for one (exam, user) pair. `kind` is the
    /// exam's: the marks give their references back, so a kind nothing is
    /// graded under any more can leave the settings again.
    ///
    /// The two badge counters come back the same way, and for a reason the
    /// other counters do not need: [`grade`](Self::grade) credits them on the
    /// branch that finds no row, and a deleted row *is* no row. Left standing,
    /// ungrade-then-regrade credited a second time off one stored mark, and
    /// looped it minted a badge from a single exam — permanently, since an
    /// award is never revoked. Each is given back off the rows this statement
    /// actually deleted: `marks_given_total` to each row's *own* grader (two
    /// teachers can hold two sittings of the same pair), and `high_mark_total`
    /// once per removed row that cleared [`HIGH_MARK_MIN`], read off the
    /// `BEFORE` image rather than re-derived — the mark that was credited is
    /// the mark that was stored.
    pub async fn remove(
        exam: &ExamId,
        user: &UserId,
        kind: &str,
        db: &Database,
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
        // conflict, the exact defect deleted from `Exam::delete`. No THROW of
        // its own, and only `DELETE`/`UPDATE` inside, so nothing here can
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
        // statement count, as in `Exam::delete`.
        let slot = result.num_statements().saturating_sub(2);
        let removed = result.take::<Vec<ExamResult>>(slot)?;
        Ok(removed.into_iter().next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_mem;

    #[tokio::test]
    async fn mark_range_is_enforced() {
        assert_eq!(Mark::try_new(0).unwrap().as_i64(), 0);
        assert_eq!(Mark::try_new(100).unwrap().as_i64(), 100);
        assert!(Mark::try_new(-1).is_err());
        assert!(Mark::try_new(101).is_err());
    }

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
        ExamResult::grade(
            &exam,
            &user,
            1,
            Mark::try_new(40).unwrap(),
            &teacher,
            "midterm",
            &db,
        )
        .await
        .unwrap();
        let second = ExamResult::grade(
            &exam,
            &user,
            2,
            Mark::try_new(90).unwrap(),
            &teacher,
            "midterm",
            &db,
        )
        .await
        .unwrap();
        assert_eq!(second.get_seq(), 2);

        // Grade-of-record is the latest sitting's mark.
        let latest = ExamResult::read_for_user(&exam, &user, &db)
            .await
            .unwrap()
            .expect("graded");
        assert_eq!(latest.get_mark().as_i64(), 90);
        assert_eq!(latest.get_seq(), 2);

        // The roster yields exactly one row per pair — the latest.
        let roster = ExamResult::list_for_exam(&exam, &db).await.unwrap();
        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].get_mark().as_i64(), 90);

        // History keeps both sittings, oldest first.
        let history = ExamResult::list_all_for_exam_user(&exam, &user, &db)
            .await
            .unwrap();
        assert_eq!(
            history
                .iter()
                .map(|r| r.get_mark().as_i64())
                .collect::<Vec<_>>(),
            vec![40, 90]
        );

        // Deleting the pair removes every sitting.
        ExamResult::remove(&exam, &user, "midterm", &db)
            .await
            .unwrap();
        assert!(
            ExamResult::list_all_for_exam_user(&exam, &user, &db)
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

        let refused = ExamResult::grade(
            &exam,
            &user,
            1,
            Mark::try_new(40).unwrap(),
            &teacher,
            "midterm",
            &db,
        )
        .await;
        assert!(matches!(refused, Err(AppError::NotFound)), "{refused:?}");
        assert!(
            ExamResult::list_all_for_exam_user(&exam, &user, &db)
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

    async fn grade(
        db: &Database,
        exam: &ExamId,
        seq: i64,
        mark: i64,
        kind: &str,
    ) -> Result<ExamResult, AppError> {
        ExamResult::grade(
            exam,
            &UserId::from_key(STUDENT),
            seq,
            Mark::try_new(mark).unwrap(),
            &UserId::from_key(TEACHER),
            kind,
            db,
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
            ExamResult::remove(&exam, &student, "midterm", &db)
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
        ExamResult::remove(&exam, &student, "midterm", &db)
            .await
            .unwrap()
            .expect("both sittings were there");
        assert_eq!(badge_counters(&db).await, (0, 0));

        // Nothing to remove is nothing to give back — the floor holds.
        assert!(
            ExamResult::remove(&exam, &student, "midterm", &db)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(badge_counters(&db).await, (0, 0));
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
            ExamResult::list_for_exam(&exam, &db)
                .await
                .unwrap()
                .is_empty(),
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
        assert!(
            ExamResult::list_for_exam(&exam, &db)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(counters(&db, "midterm").await, (0, 0));
    }
}
