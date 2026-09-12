//! The `homework_result` table: one grade per (homework, user) — the
//! freeze-stamping UPSERT, the reads the roster and report join on, and the
//! un-grade that gives the grader's counter back. Grading's policy gates
//! (who may grade whom) live in [`crate::service::homework_result`]; the
//! validated status and mark types in
//! [`crate::domain::homework_result`].

use crate::database::{Database, tx_with_retry};
use crate::domain::course::CourseId;
use crate::domain::exam_result::Mark;
use crate::domain::homework::HomeworkId;
use crate::domain::homework_result::{HomeworkResult, HomeworkResultId, HomeworkStatus};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Record (or overwrite) the grade for (homework, user). One row per pair by
/// the pair's UNIQUE constraint, so grading is one atomic UPSERT —
/// concurrent grades for the same pair converge on one row instead of
/// racing a duplicate into a 500. Grading before the due date, or before any
/// submission exists, is allowed (the caller's policy call).
///
/// In the same transaction the grade *stamps* the student's submission
/// (`graded_by_result`), which is what freezes it: every student-side write
/// to that row then fails its own `graded_by_result IS NULL` condition, with
/// no cross-table read for a concurrent write to slip past. A submission
/// that does not exist yet is left alone — grading absent work must not
/// conjure a hand-in (the report reads `submitted`/`missing`/`late` straight
/// off that row).
///
/// That one case is where this statement stops defending itself: a student's
/// *first* hand-in committing between this grade and nothing-to-stamp would
/// land unstamped, leaving a grade beside an editable submission. What
/// forbids it here is the homework row's lock: every student-side write
/// locks the homework row across its write, and so does this transaction, so
/// the interleaving is ordered away instead of leased away — either the
/// grade saw the row to stamp, or the submission saw no grade and created
/// under a lock this transaction already held.
///
/// Whether the pair was already graded is read one statement ahead of the
/// write, under the same lock — it is what keeps a regrade from crediting
/// the grader a second time. No mark counter here: a homework mark is
/// optional and most grades are status-only, so `high_mark` is an exam-only
/// family.
///
/// The homework row's `FOR UPDATE` is also the parent gate: the id is minted
/// fresh, so a grade landing inside a homework (or course) delete's window
/// would otherwise *re-create* a result row under a homework that is gone,
/// readable ever after at `GET /homework/{id}/result` (no existence check
/// there) with a `marks_given_total` no ungrade can reach, because every
/// route to the grade goes through the homework. A vanished row answers
/// `Err(NotFound)` — the 404 the web layer's own lookup would have answered.
pub async fn grade(
    db: &Database,
    homework: &HomeworkId,
    user: &UserId,
    status: HomeworkStatus,
    mark: Option<Mark>,
    graded_by: &UserId,
) -> Result<HomeworkResult, AppError> {
    let id = HomeworkResultId::generate();
    let now = Timestamp::now();
    // Owned captures (`Send` rule of `tx_with_retry` closures).
    let homework = homework.clone();
    let user = *user;
    let graded_by = *graded_by;
    tx_with_retry(db, false, async move |tx| {
        let alive = sqlx::query!(
            r#"SELECT 1 AS "row: i32" FROM homework WHERE id = $1 FOR UPDATE"#,
            homework.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        if alive.is_none() {
            return Err(AppError::NotFound);
        }
        let before = sqlx::query!(
            r#"SELECT 1 AS "row: i32" FROM homework_result
               WHERE homework = $1 AND app_user = $2 FOR UPDATE"#,
            homework.uuid(),
            user.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        // A regrade overwrites status, mark, grader and stamp — `id` and
        // `homework`/`app_user` are the row's identity and stay.
        let graded = sqlx::query_as!(
            HomeworkResult,
            r#"INSERT INTO homework_result (id, homework, app_user, status, mark, graded_by, created_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7)
               ON CONFLICT (homework, app_user) DO UPDATE
                   SET status = EXCLUDED.status, mark = EXCLUDED.mark,
                       graded_by = EXCLUDED.graded_by, created_at = EXCLUDED.created_at
               RETURNING id AS "id: HomeworkResultId",
                         homework AS "homework: HomeworkId",
                         app_user AS "user: UserId",
                         status AS "status: HomeworkStatus",
                         mark AS "mark: Mark",
                         graded_by AS "graded_by: UserId",
                         created_at AS "created_at: Timestamp""#,
            id.uuid(),
            homework.uuid(),
            user.uuid(),
            status.as_str(),
            mark.map(|mark| mark.as_i64()),
            graded_by.uuid(),
            now.as_millis(),
        )
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query!(
            r#"UPDATE homework_submission SET graded_by_result = $3
               WHERE homework = $1 AND app_user = $2 AND graded_by_result IS NULL"#,
            homework.uuid(),
            user.uuid(),
            graded.id.uuid()
        )
        .execute(&mut *tx)
        .await?;
        if !before {
            sqlx::query!(
                "UPDATE app_user SET marks_given_total = marks_given_total + 1 WHERE id = $1",
                graded_by.uuid()
            )
            .execute(&mut *tx)
            .await?;
        }
        Ok(graded)
    })
    .await
}

/// `user`'s grade for `homework`, if graded.
pub async fn read_for(
    db: &Database,
    homework: &HomeworkId,
    user: &UserId,
) -> Result<Option<HomeworkResult>, AppError> {
    Ok(sqlx::query_as!(
        HomeworkResult,
        r#"SELECT id AS "id: HomeworkResultId",
                  homework AS "homework: HomeworkId",
                  app_user AS "user: UserId",
                  status AS "status: HomeworkStatus",
                  mark AS "mark: Mark",
                  graded_by AS "graded_by: UserId",
                  created_at AS "created_at: Timestamp"
           FROM homework_result WHERE homework = $1 AND app_user = $2"#,
        homework.uuid(),
        user.uuid()
    )
    .fetch_optional(db)
    .await?)
}

/// Every grade for `homework` — the roster joins these onto the submissions.
pub async fn list_for_homework(
    db: &Database,
    homework: &HomeworkId,
) -> Result<Vec<HomeworkResult>, AppError> {
    Ok(sqlx::query_as!(
        HomeworkResult,
        r#"SELECT id AS "id: HomeworkResultId",
                  homework AS "homework: HomeworkId",
                  app_user AS "user: UserId",
                  status AS "status: HomeworkStatus",
                  mark AS "mark: Mark",
                  graded_by AS "graded_by: UserId",
                  created_at AS "created_at: Timestamp"
           FROM homework_result WHERE homework = $1 ORDER BY id DESC"#,
        homework.uuid()
    )
    .fetch_all(db)
    .await?)
}

/// `user`'s homework grades across one course — the raw rows behind the
/// per-course block of a homework report. Mirrors
/// `ExamResult::list_for_user_in_course`.
pub async fn list_for_user_in_course(
    db: &Database,
    course: &CourseId,
    user: &UserId,
) -> Result<Vec<HomeworkResult>, AppError> {
    Ok(sqlx::query_as!(
        HomeworkResult,
        r#"SELECT id AS "id: HomeworkResultId",
                  homework AS "homework: HomeworkId",
                  app_user AS "user: UserId",
                  status AS "status: HomeworkStatus",
                  mark AS "mark: Mark",
                  graded_by AS "graded_by: UserId",
                  created_at AS "created_at: Timestamp"
           FROM homework_result
           WHERE app_user = $1
             AND homework IN (SELECT id FROM homework WHERE course = $2)
           ORDER BY id DESC"#,
        user.uuid(),
        course.uuid()
    )
    .fetch_all(db)
    .await?)
}

/// Un-grade (homework, user), returning the removed row (`None` if there
/// was none). Removing the grade unfreezes the student's submission, so the
/// stamp [`grade`] left on it is cleared in the same
/// transaction — scoped to *this* grade's id, so it can never wipe a stamp a
/// concurrent re-grade has just written.
///
/// The grader's `marks_given_total` comes back with it, in that same
/// transaction and only when a row was really deleted. [`grade`]
/// credits on the branch that finds no row, and a deleted row *is* no row:
/// left standing, ungrade-then-regrade credited a second time off one
/// stored grade, and looped it minted a badge with no exam, no mark and no
/// submission behind it — permanently, since an award is never revoked.
/// Given back to the *removed row's own* grader, not the caller: a manager
/// may un-grade what a teacher graded, and the credit is the teacher's.
pub async fn remove(
    db: &Database,
    homework: &HomeworkId,
    user: &UserId,
) -> Result<Option<HomeworkResult>, AppError> {
    // Owned captures (`Send` rule of `tx_with_retry` closures).
    let homework = homework.clone();
    let user = *user;
    tx_with_retry(db, false, async move |tx| {
        // The homework row's lock orders this against a concurrent grade
        // (which locks the same row before inserting): either this delete
        // wins and the re-grade lands after as a fresh grade, or the grade
        // lands first and this removes *it*. A homework already cascaded
        // away left no result rows behind — `None`, the 404 the web layer
        // answers.
        let alive = sqlx::query!(
            r#"SELECT 1 AS "row: i32" FROM homework WHERE id = $1 FOR UPDATE"#,
            homework.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        if alive.is_none() {
            return Ok(None);
        }
        // The stamp clears *before* the delete: the submission's
        // `graded_by_result` FK points at the grade row (`NO ACTION`), so the
        // delete must not run while it stands. The subselect scopes the clear
        // to *this* pair's grade id — it can never wipe a stamp a concurrent
        // re-grade has just written (none can interleave: the homework row is
        // locked above).
        sqlx::query!(
            r#"UPDATE homework_submission SET graded_by_result = NULL
               WHERE homework = $1 AND app_user = $2 AND graded_by_result IN (
                   SELECT id FROM homework_result WHERE homework = $1 AND app_user = $2)"#,
            homework.uuid(),
            user.uuid()
        )
        .execute(&mut *tx)
        .await?;
        let gone = sqlx::query!(
            r#"DELETE FROM homework_result WHERE homework = $1 AND app_user = $2
               RETURNING id AS "id: HomeworkResultId",
                         homework AS "homework: HomeworkId",
                         app_user AS "user: UserId",
                         status AS "status: HomeworkStatus",
                         mark AS "mark: Mark",
                         graded_by AS "graded_by: UserId",
                         created_at AS "created_at: Timestamp""#,
            homework.uuid(),
            user.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(removed) = gone else {
            return Ok(None);
        };
        sqlx::query!(
            "UPDATE app_user SET marks_given_total = GREATEST(marks_given_total - 1, 0) WHERE id = $1",
            removed.graded_by.uuid()
        )
        .execute(&mut *tx)
        .await?;
        Ok(Some(HomeworkResult {
            id: removed.id,
            homework: removed.homework,
            user: removed.user,
            status: removed.status,
            mark: removed.mark,
            graded_by: removed.graded_by,
            created_at: removed.created_at,
        }))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real homework row (with the course and subject it needs): the grade now
    /// carries a parent gate, so a minted id nothing wrote is a `404` here, the
    /// same way it already is for a submission.
    async fn a_homework(db: &Database) -> HomeworkId {
        use crate::db::homework;
        use crate::domain::homework::HomeworkTitle;
        use crate::domain::subject::{SubjectDescription, SubjectName};

        let course = crate::db::course::a_test_course(db).await;
        let subject = crate::db::subject::create(
            db,
            &course,
            SubjectName::try_new("topic").unwrap(),
            SubjectDescription::try_new("").unwrap(),
        )
        .await
        .unwrap();
        homework::create(
            db,
            &course,
            subject.get_id(),
            HomeworkTitle::try_new("essay").unwrap(),
            None,
            Timestamp::from_millis(1),
            None,
            &a_teacher(db).await,
        )
        .await
        .unwrap()
        .get_id()
        .clone()
    }

    /// A real `app_user` teacher row: graders are foreign keys too.
    async fn a_teacher(db: &Database) -> UserId {
        let user = UserId::generate();
        sqlx::query(
            "INSERT INTO app_user (id, username, password_hash, role) \
             VALUES ($1, $2, 'x', 'teacher')",
        )
        .bind(user.uuid())
        .bind(format!("t-{}", &user.key()[30..]))
        .execute(db)
        .await
        .unwrap();
        user
    }

    /// A real `app_user` student row: graded users are foreign keys too.
    async fn a_student(db: &Database) -> UserId {
        let user = UserId::generate();
        sqlx::query(
            "INSERT INTO app_user (id, username, password_hash, role) \
             VALUES ($1, $2, 'x', 'student')",
        )
        .bind(user.uuid())
        .bind(format!("s-{}", &user.key()[30..]))
        .execute(db)
        .await
        .unwrap();
        user
    }

    #[tokio::test]
    async fn grade_upserts_one_row_per_pair_and_remove_unfreezes() {
        let (db, _leases) = crate::database::init_test_db().await;
        let homework = a_homework(&db).await;
        let user = a_student(&db).await;
        let teacher = a_teacher(&db).await;

        let first = grade(
            &db,
            &homework,
            &user,
            HomeworkStatus::try_new("incomplete").unwrap(),
            None,
            &teacher,
        )
        .await
        .unwrap();
        // A regrade lands on the same row (composite id), overwriting the grade.
        let second = grade(
            &db,
            &homework,
            &user,
            HomeworkStatus::try_new("done").unwrap(),
            Some(Mark::try_new(80).unwrap()),
            &teacher,
        )
        .await
        .unwrap();
        assert_eq!(first.get_id(), second.get_id());
        assert_eq!(second.get_status().as_str(), "done");
        assert_eq!(second.get_mark().unwrap().as_i64(), 80);
        assert_eq!(list_for_homework(&db, &homework).await.unwrap().len(), 1);

        assert!(read_for(&db, &homework, &user).await.unwrap().is_some());
        assert!(remove(&db, &homework, &user).await.unwrap().is_some());
        assert!(read_for(&db, &homework, &user).await.unwrap().is_none());
    }

    /// The grader is credited once per graded pair: a regrade lands on the same
    /// row, so it is not a mark given twice. The student's counters are not
    /// touched at all — a homework grade is not a homework submission.
    #[tokio::test]
    async fn a_first_grade_credits_the_grader_and_a_regrade_does_not() {
        let (db, _leases) = crate::database::init_test_db().await;
        let homework = a_homework(&db).await;
        let user = a_student(&db).await;
        let teacher = a_teacher(&db).await;

        for status in ["incomplete", "done"] {
            grade(
                &db,
                &homework,
                &user,
                HomeworkStatus::try_new(status).unwrap(),
                None,
                &teacher,
            )
            .await
            .unwrap();
        }
        assert_eq!(
            marks_given(&teacher, &db).await,
            1,
            "one graded pair, one mark given"
        );
    }

    /// Un-grading gives the credit back, so grade → ungrade → regrade nets to
    /// one however often it is run. Crediting the grade without refunding the
    /// delete was the cheapest counter farm in the codebase — two requests per
    /// mark given, with no exam, no mark and no submission behind them.
    #[tokio::test]
    async fn ungrading_gives_the_grader_credit_back() {
        let (db, _leases) = crate::database::init_test_db().await;
        let homework = a_homework(&db).await;
        let user = a_student(&db).await;
        let teacher = a_teacher(&db).await;

        for _ in 0..3 {
            grade(
                &db,
                &homework,
                &user,
                HomeworkStatus::try_new("missing").unwrap(),
                None,
                &teacher,
            )
            .await
            .unwrap();
            assert_eq!(marks_given(&teacher, &db).await, 1);
            assert!(
                remove(&db, &homework, &user).await.unwrap().is_some(),
                "the grade was there"
            );
            assert_eq!(marks_given(&teacher, &db).await, 0, "the loop kept one");
        }

        // Nothing to remove is nothing to give back — the floor holds.
        assert!(remove(&db, &homework, &user).await.unwrap().is_none());
        assert_eq!(marks_given(&teacher, &db).await, 0);
    }
    /// The grader's badge counter, re-read out of the store.
    async fn marks_given(user: &UserId, db: &Database) -> i64 {
        use sqlx::Row as _;

        sqlx::query("SELECT marks_given_total FROM app_user WHERE id = $1")
            .bind(user.uuid())
            .fetch_one(db)
            .await
            .unwrap()
            .try_get::<i64, _>(0)
            .unwrap()
    }
}
