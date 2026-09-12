//! The `enrollment` table: the roster read and listing, the seat-returning
//! delete, and the enroll claim — one transaction that checks the pair free,
//! the member still a student, and the roster under capacity while it
//! spends the seat. The target-user gates the HTTP handlers pay live in
//! [`crate::service::enrollment`].

use crate::database::{Database, tx_with_retry, unique_violation};
use crate::db::cap;
use crate::db::page::PagedList;
use crate::domain::course::CourseId;
use crate::domain::enrollment::Enrollment;
use crate::domain::role::Role;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// What [`enroll`]'s transaction settled on — the three old `THROW` markers
/// (`enroll_held`, `enroll_not_student`, `enroll_full`) as Rust verdicts.
enum Verdict {
    /// This pair already holds a row.
    Held,
    /// The person being enrolled is no longer a student.
    Unfit,
    /// The roster is full — or the course is gone; the caller reads which.
    Full,
    /// The seat is spent and the row is written.
    Made(Enrollment),
}

/// The primary key the natural composite PK carries — the constraint name
/// that surfaces in SQLSTATE 23505 when a rival lands first.
const PAIR_CONSTRAINT: &str = "enrollment_course_user";

/// Enroll (idempotently) `user` into `course`. The table's primary key *is*
/// the (course, user) pair, so concurrent enrolls of the same pair converge
/// on one row instead of racing into a 500: the loser reads the winner's row
/// back and returns it. When the course carries a capacity, a full roster
/// refuses new members (409) — the seat is taken by a single-record
/// conditional write on the course row whose bound is the row's *own*
/// `capacity` column, read inside the same statement that spends the seat. A
/// snapshot integer taken by a read beforehand would be exactly the number a
/// concurrent capacity PATCH invalidates. An already enrolled user is
/// returned as-is even when the roster is full, and never charged a seat.
/// The same claim is the delete guard's other half: it matches nothing once
/// the course row is gone, and while it holds a seat the course cannot be
/// deleted — so no roster row can outlive its course.
///
/// Gate order is the old THROW order: an existing pair outranks the role
/// check, which outranks a full roster. The role check takes the user row
/// `FOR NO KEY UPDATE`, the same row a demotion cascade updates before its
/// sweep, so grant and demotion serialize on the row lock in both
/// directions — a row landing after the sweep is impossible, and a demotion
/// landing before this check refuses it.
pub async fn enroll(
    db: &Database,
    course: &CourseId,
    user: &UserId,
    enrolled_by: &UserId,
) -> Result<Enrollment, AppError> {
    if let Some(existing) = read_for_user(db, course, user).await? {
        return disown_if_pumped(db, existing).await;
    }
    let verdict = tx_with_retry(db, false, async |tx| {
        // The duplicate gate rides ahead of the role check, so "you are
        // already in" still outranks everything.
        let held = sqlx::query!(
            r#"SELECT 1 AS "one" FROM enrollment WHERE course = $1 AND app_user = $2"#,
            course,
            user,
        )
        .fetch_optional(&mut *tx)
        .await?;
        if held.is_some() {
            return Ok(Verdict::Held);
        }
        // The role handshake: the live role decides, and the row lock makes
        // the check and the insert one decision against a concurrent
        // demotion (see the doc above).
        let role = sqlx::query!(
            r#"SELECT role FROM app_user WHERE id = $1 FOR NO KEY UPDATE"#,
            user,
        )
        .fetch_optional(&mut *tx)
        .await?;
        if role.map(|row| row.role).as_deref() != Some(Role::Student.as_str()) {
            return Ok(Verdict::Unfit);
        }
        // The seat and the row in one statement (the cap recipe): a refused
        // insert takes its own seat bump back; zero rows means the roster is
        // full or the course is gone — only the caller pays for the read
        // that tells those apart.
        let row = sqlx::query_as!(
            Enrollment,
            r#"WITH seat AS (
                 UPDATE course
                    SET enrollment_count = enrollment_count + 1
                  WHERE id = $1
                    AND enrollment_count < COALESCE(capacity, $2)
                  RETURNING 1)
               INSERT INTO enrollment (course, app_user, enrolled_by, source)
               SELECT $1, $3, $4, NULL WHERE EXISTS (SELECT 1 FROM seat)
               RETURNING course, app_user AS "user", enrolled_by, source"#,
            course,
            cap::UNLIMITED,
            user,
            enrolled_by,
        )
        .fetch_optional(&mut *tx)
        .await?;
        match row {
            Some(row) => Ok(Verdict::Made(row)),
            None => Ok(Verdict::Full),
        }
    })
    .await;
    // The 23505 backstop folds into the Held verdict before the mapping:
    // someone placed this pair between the gate and the insert, and their
    // row is the answer — no seat was spent finding that out.
    let verdict = match verdict {
        Err(AppError::Db(err)) if unique_violation(&err) == Some(PAIR_CONSTRAINT) => {
            Ok(Verdict::Held)
        }
        other => other,
    };
    match verdict {
        Ok(Verdict::Made(row)) => Ok(row),
        Ok(Verdict::Held) => match read_for_user(db, course, user).await? {
            Some(existing) => disown_if_pumped(db, existing).await,
            None => Err(AppError::Internal("failed to enroll user".into())),
        },
        // Demoted while this ran: the same refusal the service's own gate
        // makes a moment earlier, and the only one that can arrive after it.
        Ok(Verdict::Unfit) => Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "only students can be enrolled in a course",
        })),
        // Full, or the course is gone — the conditional write matches
        // nothing either way, and only this path pays for the read that
        // tells them apart.
        Ok(Verdict::Full) => match crate::db::course::read(db, course).await? {
            Some(_) => Err(AppError::Conflict("the course is full")),
            None => Err(AppError::NotFound),
        },
        Err(err) => Err(err),
    }
}

/// A hand enroll landing on a row a class pumped takes the row *off* the
/// class: `source` goes, so no later class sweep can unenroll a student an
/// operator placed on purpose. The mirror of the rule the sweep already
/// holds in the other direction — a manual unenroll wins, permanently — and
/// without it "hand-placed" was a state only a first enroll could reach.
///
/// `NULL`, not absence: the column is a nullable uuid whose NULL *is* the
/// meaning a missing key used to carry (see the struct doc), and it is what
/// a hand enroll writes on the create path too.
async fn disown_if_pumped(db: &Database, existing: Enrollment) -> Result<Enrollment, AppError> {
    if existing.get_source().is_none() {
        return Ok(existing);
    }
    let disowned = sqlx::query_as!(
        Enrollment,
        r#"UPDATE enrollment SET source = NULL
           WHERE course = $1 AND app_user = $2
           RETURNING course, app_user AS "user", enrolled_by, source"#,
        existing.get_course(),
        existing.get_user(),
    )
    .fetch_optional(db)
    .await?;
    // A row that is no longer there is not an internal error: an unenroll
    // (or the class sweep a role change runs) landing between the read
    // above and this write matches nothing, and the caller asked to be
    // enrolled — which they were, by the row this call was handed. The
    // disown is all that is lost, and it is lost to a delete that took the
    // whole row with it.
    Ok(disowned.unwrap_or(existing))
}

/// Some(_) iff `user` is enrolled in `course` — the grading gate.
pub async fn read_for_user(
    db: &Database,
    course: &CourseId,
    user: &UserId,
) -> Result<Option<Enrollment>, AppError> {
    let row = sqlx::query_as!(
        Enrollment,
        r#"SELECT course, app_user AS "user", enrolled_by, source
           FROM enrollment WHERE course = $1 AND app_user = $2"#,
        course,
        user,
    )
    .fetch_optional(db)
    .await?;
    Ok(row)
}

pub async fn list_for_course(
    db: &Database,
    course: &CourseId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Enrollment>, i64), AppError> {
    // The old composite key `{course}_{user}` sorted by user within a
    // course — its course prefix was constant — so the natural-PK columns
    // sort the same way.
    PagedList::new(
        "enrollment WHERE course = $1",
        "ORDER BY course DESC, app_user DESC",
    )
    .bind(course.uuid())
    .run::<Enrollment>(limit, offset, db)
    .await
}

/// Take the row and give its seat back in one statement, so nothing but a
/// lost transaction can drift the counter. `None` when the pair held no row.
pub async fn remove(
    db: &Database,
    course: &CourseId,
    user: &UserId,
) -> Result<Option<Enrollment>, AppError> {
    let gone = sqlx::query_as!(
        Enrollment,
        r#"WITH gone AS (
             DELETE FROM enrollment
              WHERE course = $1 AND app_user = $2
              RETURNING course, app_user, enrolled_by, source),
           seated AS (
             UPDATE course
                SET enrollment_count = GREATEST(
                    enrollment_count - (SELECT count(*) FROM gone), 0)
              WHERE id = $1
              RETURNING 1)
           SELECT course, app_user AS "user", enrolled_by, source FROM gone"#,
        course,
        user,
    )
    .fetch_optional(db)
    .await?;
    Ok(gone)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The boot repair of `enrollment_count` **against a real server**, and
    /// `#[ignore]`d for it: the pass counts rows off `enrollment`'s indexed
    /// `course` field, and an aggregate over an indexed field is exactly where
    /// the embedded engine and the server are known to differ (`count()` comes
    /// back `{count: N}` from one and a bare int from the other, which
    /// `option<int>` would refuse). `array::len` over ids is the spelling that
    /// dodges it — this is what proves it on the store that bites.
    ///
    /// Both halves in one boot: `staff` is the row the sweep deletes (their
    /// seat must come back), and `empty` is the course a *past* sweep already
    /// stranded — count above zero with no rows left, so it forms no group and
    /// only a per-course pass ever visits it. The second `migrate` pins that the
    /// repair converges rather than overwriting: it must write nothing.
    #[tokio::test]
    #[ignore = "needs a real SurrealDB server: podman start hezarfen-surrealdb && cargo test -- --ignored"]
    async fn the_count_repair_recounts_on_a_real_server() {
        let (db, _serialized) = crate::database::init_test_server("enrollment_repair").await;
        db.query(
            "CREATE user:t SET username = 't', password_hash = 'x', role = 'teacher';
             CREATE user:a SET username = 'a', password_hash = 'x', role = 'student';
             CREATE user:b SET username = 'b', password_hash = 'x', role = 'student';
             CREATE course:live SET creator = user:t, title = 'l', description = '',
                 enrollment_count = 9;
             CREATE course:empty SET creator = user:t, title = 'e', description = '',
                 enrollment_count = 4;
             CREATE enrollment:live_a SET course = course:live, user = user:a, enrolled_by = user:t;
             CREATE enrollment:live_b SET course = course:live, user = user:b, enrolled_by = user:t;
             CREATE enrollment:live_t SET course = course:live, user = user:t, enrolled_by = user:t;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

        async fn counts(db: &Database) -> Vec<i64> {
            let mut result = db
                .query("SELECT VALUE enrollment_count FROM course ORDER BY id")
                .await
                .unwrap()
                .check()
                .unwrap();
            result.take::<Vec<i64>>(0).unwrap()
        }

        crate::database::migrate(db.as_ref()).await.unwrap();
        // `course:empty` sorts before `course:live`: stranded count cleared, and
        // the live course recounted to its two students — the teacher's row was
        // swept and handed its seat back in the same boot.
        assert_eq!(counts(&db).await, vec![0, 2]);

        crate::database::migrate(db.as_ref()).await.unwrap();
        assert_eq!(counts(&db).await, vec![0, 2], "the repair converges");
    }

    /// A pumped row that vanishes under the disown — a concurrent unenroll, or
    /// the sweep a role change runs — must answer the row the caller was
    /// handed, not a 500. The `UPDATE` matches nothing and used to fall through
    /// to `AppError::Internal`, turning a race the old code answered 200 into a
    /// server error on an ordinary `POST /courses/{id}/enrollments`.
    #[tokio::test]
    async fn a_vanished_row_is_not_an_internal_error() {
        let db = crate::database::init_mem().await.unwrap();
        let class = crate::domain::class_group::ClassGroupId::from_key("9a");
        let course = CourseId::from_key("01J8XZ0K3Q8G7X2M4N5P6R7S8T");
        let student = UserId::from_key("01J8XZ0K3Q8G7X2M4N5P6R7S8U");
        // Never written: the same store state a delete in the window leaves.
        let ghost = Enrollment {
            id: EnrollmentId::composite(&course, &student),
            course: course.clone(),
            user: student.clone(),
            enrolled_by: UserId::from_key("mgr"),
            source: Some(class),
        };

        let answered = disown_if_pumped(&db, ghost).await.unwrap();
        assert_eq!(answered.get_user(), &student);
        assert_eq!(answered.get_course(), &course);
    }

    /// And the other half of "absence is the meaning": a hand-placed enrollment
    /// must store *no* source key, or a class sweep would take back a row it
    /// never wrote. Asserted on the stored row, not the returned one.
    #[tokio::test]
    async fn a_hand_placed_enrollment_stores_no_source_key() {
        use crate::domain::course::{CourseDescription, CourseKind, CourseTitle};

        let db = crate::database::init_mem().await.unwrap();
        let teacher = UserId::from_key("teacher");
        let course = crate::db::course::create(
            &db,
            &teacher,
            CourseTitle::try_new("algebra").unwrap(),
            CourseDescription::try_new("").unwrap(),
            CourseKind::course(),
            None,
            None,
        )
        .await
        .unwrap();
        enroll(&db, course.get_id(), &UserId::from_key("student"), &teacher)
            .await
            .unwrap();

        let mut result = db
            .query("SELECT VALUE 'source' IN object::keys($this) FROM enrollment")
            .await
            .unwrap()
            .check()
            .unwrap();
        assert_eq!(
            result.take::<Vec<bool>>(0).unwrap(),
            vec![false],
            "a hand-placed row may carry no source key"
        );
    }
}
