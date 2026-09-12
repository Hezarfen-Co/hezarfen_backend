//! The `exam_result` table: the mark's claim-riding upsert, the per-sitting
//! history reads, and the refunding delete. The grade pre-flight (draft, kind,
//! grader/target walls) lives in
//! [`crate::service::exam_result`]; the mark newtype and the latest-per-pair
//! fold live in [`crate::domain::exam_result`].

use crate::constant::HIGH_MARK_MIN;
use crate::database::{Database, tx_with_retry};
use crate::domain::course::CourseId;
use crate::domain::exam::ExamId;
use crate::domain::exam_result::{
    ExamResult, Mark, draft_error, latest_per_pair, retired_kind_error,
};
use crate::domain::user::UserId;
use crate::error::AppError;

/// A single user's result for an exam, if graded — the latest sitting's mark.
async fn find(db: &Database, exam: &ExamId, user: &UserId) -> Result<Option<ExamResult>, AppError> {
    // Grade-of-record is the latest sitting's mark: the highest seq wins.
    Ok(sqlx::query_as!(
        ExamResult,
        r#"SELECT exam AS "exam: ExamId", app_user AS "user: UserId", seq,
                  mark AS "mark: Mark", graded_by AS "graded_by: UserId"
           FROM exam_result WHERE exam = $1 AND app_user = $2
           ORDER BY seq DESC LIMIT 1"#,
        exam as &ExamId,
        user as &UserId,
    )
    .fetch_optional(db)
    .await?)
}

/// Record (or overwrite) `user`'s mark for the `seq`th sitting of `exam`.
/// One row per (exam, user, seq), keyed by the natural composite primary key
/// `exam_result_exam_user_seq`, so this is a single atomic UPSERT —
/// concurrent grades for the same sitting converge on one row instead of
/// racing the unique index into a 500. Grading a retake writes a fresh mark
/// at the current seq and never touches prior sittings' marks; the latest
/// seq is the grade-of-record.
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
/// itself counted under a kind its exam no longer carries. The exam row is
/// taken `FOR UPDATE` first — which is also what serializes this write with
/// a concurrent re-draft and with the exam-delete cascade, replacing the
/// reader lease grading used to hold.
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
    // The "exists" and "not a draft" gates ride in the same transaction as
    // the mark, the mirror of the re-draft gate on
    // `crate::db::exam::update_if_unchanged`: between them, a mark and a
    // re-draft racing each other can only ever leave one of the two applied,
    // whichever process either ran in. The existence gate is not
    // redundant with the draft one — a missing row answers the same `404`
    // the old `exam_missing` THROW answered with, and `draft` alone could
    // never see a row that is not there. The caller's pre-flight check
    // answers the same 409 one round trip earlier.
    //
    // `$before` — the sitting's previous mark — answers both counter
    // questions off one read: `None` there means no row at all (the claim
    // branch), while a number is what the student's counter takes its
    // difference against. Read one statement before the counters inside
    // the same transaction: read anywhere else it would be a guess about a
    // row two graders may be writing at once, and the difference would be
    // taken against a mark some other round had already replaced. The exam
    // row's `FOR UPDATE` is what makes that read the write's own: a rival
    // grade holds the same lock across its whole transaction, so the two
    // serialize instead of interleaving their counter moves.
    tx_with_retry(db, false, async |conn| {
        let exam_row = sqlx::query!(
            r#"SELECT draft AS "draft: bool" FROM exam WHERE id = $1 FOR UPDATE"#,
            exam as &ExamId,
        )
        .fetch_optional(&mut *conn)
        .await?;
        let Some(exam_row) = exam_row else {
            return Err(AppError::NotFound);
        };
        if exam_row.draft {
            return Err(draft_error());
        }
        let before = sqlx::query!(
            r#"SELECT mark AS "mark: Mark" FROM exam_result
               WHERE exam = $1 AND app_user = $2 AND seq = $3"#,
            exam as &ExamId,
            user as &UserId,
            seq,
        )
        .fetch_optional(&mut *conn)
        .await?
        .map(|row| row.mark);
        if before.is_none() {
            // `kind_retired`: the mark's own transaction refusing a name
            // the school has retired. Zero rows — retired, or the ref row
            // gone with the kind — refuse the claim; a live row takes the
            // count.
            let claimed = sqlx::query!(
                r#"UPDATE kind_ref SET count = kind_ref.count + 1
                   WHERE name = $1 AND retired = false
                   RETURNING 1"#,
                kind,
            )
            .fetch_optional(&mut *conn)
            .await?;
            if claimed.is_none() {
                return Err(retired_kind_error(kind));
            }
            sqlx::query!(
                r#"UPDATE exam SET result_count = exam.result_count + 1 WHERE id = $1"#,
                exam as &ExamId,
            )
            .execute(&mut *conn)
            .await?;
            sqlx::query!(
                r#"UPDATE app_user SET marks_given_total = app_user.marks_given_total + 1
                   WHERE id = $1"#,
                graded_by as &UserId,
            )
            .execute(&mut *conn)
            .await?;
        }
        let stored = before.map_or(-1, |mark| mark.as_i64());
        let high = i64::from(mark.as_i64() >= HIGH_MARK_MIN) - i64::from(stored >= HIGH_MARK_MIN);
        if high != 0 {
            sqlx::query!(
                r#"UPDATE app_user SET high_mark_total =
                       GREATEST(high_mark_total + $2, 0)
                   WHERE id = $1"#,
                user as &UserId,
                high,
            )
            .execute(&mut *conn)
            .await?;
        }
        let written = sqlx::query_as!(
            ExamResult,
            r#"INSERT INTO exam_result (exam, app_user, seq, mark, graded_by)
               VALUES ($1, $2, $3, $4, $5)
               ON CONFLICT (exam, app_user, seq) DO UPDATE
                   SET mark = EXCLUDED.mark, graded_by = EXCLUDED.graded_by
               RETURNING exam AS "exam: ExamId", app_user AS "user: UserId", seq,
                         mark AS "mark: Mark", graded_by AS "graded_by: UserId""#,
            exam as &ExamId,
            user as &UserId,
            seq,
            mark,
            graded_by as &UserId,
        )
        .fetch_one(&mut *conn)
        .await?;
        Ok(written)
    })
    .await
}

/// The user's graded results restricted to one course's exams — the raw
/// rows behind the per-course block of the marks report.
pub async fn list_for_user_in_course(
    db: &Database,
    course: &CourseId,
    user: &UserId,
) -> Result<Vec<ExamResult>, AppError> {
    let rows = sqlx::query_as!(
        ExamResult,
        r#"SELECT r.exam AS "exam: ExamId", r.app_user AS "user: UserId", r.seq,
                  r.mark AS "mark: Mark", r.graded_by AS "graded_by: UserId"
           FROM exam_result r JOIN exam e ON e.id = r.exam
           WHERE r.app_user = $1 AND e.course = $2
           ORDER BY r.exam DESC, r.seq DESC"#,
        user as &UserId,
        course as &CourseId,
    )
    .fetch_all(db)
    .await?;
    Ok(latest_per_pair(rows))
}

pub async fn list_for_exam(db: &Database, exam: &ExamId) -> Result<Vec<ExamResult>, AppError> {
    let rows = sqlx::query_as!(
        ExamResult,
        r#"SELECT exam AS "exam: ExamId", app_user AS "user: UserId", seq,
                  mark AS "mark: Mark", graded_by AS "graded_by: UserId"
           FROM exam_result WHERE exam = $1
           ORDER BY exam DESC, seq DESC"#,
        exam as &ExamId,
    )
    .fetch_all(db)
    .await?;
    Ok(latest_per_pair(rows))
}

/// Every sitting's mark for one (exam, user) pair, oldest first — the
/// per-attempt grade history behind the FE's attempt-by-attempt view.
pub async fn list_all_for_exam_user(
    db: &Database,
    exam: &ExamId,
    user: &UserId,
) -> Result<Vec<ExamResult>, AppError> {
    Ok(sqlx::query_as!(
        ExamResult,
        r#"SELECT exam AS "exam: ExamId", app_user AS "user: UserId", seq,
                  mark AS "mark: Mark", graded_by AS "graded_by: UserId"
           FROM exam_result WHERE exam = $1 AND app_user = $2
           ORDER BY seq ASC"#,
        exam as &ExamId,
        user as &UserId,
    )
    .fetch_all(db)
    .await?)
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
/// deleted image rather than re-derived. That last one is why
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
    tx_with_retry(db, false, async |conn| {
        // The `gone` CTE deletes and reports in one statement: the counters
        // below move by exactly what this delete removed, so a concurrent
        // grade's rows (it holds the exam row's lock, and this statement's
        // sibling below takes the same lock before touching `result_count`)
        // can never be half-counted.
        let gone = sqlx::query!(
            r#"WITH gone AS (
                   DELETE FROM exam_result
                   WHERE exam = $1 AND app_user = $2
                   RETURNING mark AS "mark: Mark", graded_by AS "graded_by: UserId", seq)
               SELECT seq, mark AS "mark: Mark", graded_by AS "graded_by: UserId"
               FROM gone ORDER BY seq ASC"#,
            exam as &ExamId,
            user as &UserId,
        )
        .fetch_all(&mut *conn)
        .await?;
        if gone.is_empty() {
            return Ok(None);
        }
        sqlx::query!(
            r#"UPDATE kind_ref SET count = GREATEST(kind_ref.count - $2, 0)
               WHERE name = $1"#,
            kind,
            gone.len() as i64,
        )
        .execute(&mut *conn)
        .await?;
        sqlx::query!(
            r#"UPDATE exam SET result_count = GREATEST(exam.result_count - $2, 0)
               WHERE id = $1"#,
            exam as &ExamId,
            gone.len() as i64,
        )
        .execute(&mut *conn)
        .await?;
        let high = gone.iter().filter(|row| row.mark.as_i64() >= HIGH_MARK_MIN).count() as i64;
        if high > 0 {
            sqlx::query!(
                r#"UPDATE app_user SET high_mark_total =
                       GREATEST(high_mark_total - $2, 0)
                   WHERE id = $1"#,
                user as &UserId,
                high,
            )
            .execute(&mut *conn)
            .await?;
        }
        // One give-back per distinct grader: two teachers can hold two
        // sittings of the same pair.
        let mut per_grader: Vec<(UserId, i64)> = Vec::new();
        for row in &gone {
            if let Some((_, n)) =
                per_grader.iter_mut().find(|(grader, _)| grader == &row.graded_by)
            {
                *n += 1;
            } else {
                per_grader.push((row.graded_by.clone(), 1));
            }
        }
        for (grader, n) in &per_grader {
            sqlx::query!(
                r#"UPDATE app_user SET marks_given_total =
                       GREATEST(marks_given_total - $2, 0)
                   WHERE id = $1"#,
                grader as &UserId,
                n,
            )
            .execute(&mut *conn)
            .await?;
        }
        // The first sitting's row comes back — the same shape the old
        // `RETURN` handed over, whose caller only asks whether anything was
        // removed and what the latest standing was.
        let first = gone.into_iter().next().map(|row| ExamResult {
            exam: exam.clone(),
            user: user.clone(),
            seq: row.seq,
            mark: row.mark,
            graded_by: row.graded_by,
        });
        Ok(first)
    })
    .await
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
