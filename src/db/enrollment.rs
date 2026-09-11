//! The `enrollment` table: the roster read and listing, the seat-returning
//! delete, and the enroll claim — the one transaction that checks the pair
//! free, the member still a student, and the roster under capacity while it
//! spends the seat. The target-user gates the HTTP handlers pay live in
//! [`crate::service::enrollment`].

use surrealdb::types::SurrealValue;

use crate::constant::ENROLLMENT_COUNT_FIELD;
use crate::database::{Database, transaction_with_retry};
use crate::db::cap;
use crate::db::page::PagedList;
use crate::domain::course::CourseId;
use crate::domain::enrollment::{Enrollment, EnrollmentId};
use crate::domain::role::Role;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// The `THROW` markers [`enroll`]'s claim aborts with: this pair
/// already holds a row, the roster is full, and the person being enrolled is no
/// longer a student.
const HELD_MARK: &str = "enroll_held";
const FULL_MARK: &str = "enroll_full";
const UNFIT_MARK: &str = "enroll_not_student";

/// Enroll (idempotently) `user` into `course`. One row per (course, user),
/// keyed by a deterministic composite id, so concurrent enrolls of the same
/// pair converge on one row instead of racing the unique index into a 500:
/// the loser of the `CREATE` reads the winner's row back and returns it.
/// When the course carries a capacity, a full roster refuses new members
/// (409) — the seat is taken by a single-record conditional write on the
/// course row whose bound is the row's *own* `capacity` column, read inside
/// the same statement that spends the seat. A snapshot integer taken by a
/// read beforehand would be exactly the number a concurrent capacity PATCH
/// invalidates, which is what this promised and did not do. An already
/// enrolled user is returned as-is even when the roster is full, and never
/// charged a seat. The same claim is the delete guard's other half: it
/// matches nothing once the course row is gone, and while it holds a seat
/// the course cannot be deleted — so no roster row can outlive its course.
///
/// On admissibility ([`crate::database::transaction_with_retry`]): the
/// `CREATE` here *can* answer "already exists", but only to a rival that
/// landed inside this very window — and the re-send then sees that row at
/// the `$held` gate and takes the other branch, so the loop converges
/// instead of re-asking a settled question. Same shape, same reason, as
/// [`crate::domain::class_pump::attach`].
pub async fn enroll(
    db: &Database,
    course: &CourseId,
    user: &UserId,
    enrolled_by: &UserId,
) -> Result<Enrollment, AppError> {
    if let Some(existing) = read_for_user(db, course, user).await? {
        return disown_if_pumped(db, existing).await;
    }
    let enrollment = Enrollment {
        id: EnrollmentId::composite(course, user),
        course: course.clone(),
        user: user.clone(),
        enrolled_by: enrolled_by.clone(),
        // Hand-placed: no source key at all is written, which is what makes
        // a class sweep unable to take this row back.
        source: None,
    };
    // CREATE, not UPSERT, and in the seat's own transaction: a duplicate has
    // to be *seen*, or the pair's second writer would keep the seat it
    // claimed for a row that already existed and the counter would drift
    // above the roster forever.
    //
    // Parenthesized `??` throughout: `n ?? 0 < cap` parses as
    // `n ?? (0 < cap)`, which is truthy for every row and would enroll past
    // the capacity.
    //
    // The seat is claimed on the *course*, so the student's own row is
    // claimed beside it ([`cap::role_claim`]): a demotion's sweep runs on a
    // snapshot, and a row landing after it would count a seat for someone
    // who may not hold one, with nothing left to re-sweep. It rides between
    // the duplicate gate and the seat, so "you are already in" still
    // outranks it and it outranks a full roster.
    let held_by = cap::role_claim(
        "usr",
        &format!("!= '{}'", Role::Student.as_str()),
        UNFIT_MARK,
    );
    let hold = held_by.join(";\n             ");
    let sql = format!(
        "BEGIN TRANSACTION;
         LET $held = (SELECT VALUE id FROM $id);
         IF array::len($held) > 0 {{ THROW '{HELD_MARK}' }};
         {hold};
         LET $seat = (UPDATE $course SET {ENROLLMENT_COUNT_FIELD} = \
             ({ENROLLMENT_COUNT_FIELD} ?? 0) + 1 \
             WHERE ({ENROLLMENT_COUNT_FIELD} ?? 0) < (capacity ?? $unlimited) \
             RETURN VALUE id);
         IF array::len($seat) = 0 {{ THROW '{FULL_MARK}' }};
         CREATE $id CONTENT $row;
         COMMIT TRANSACTION;"
    );
    // The process still sends its counter writes one at a time (see
    // [`cap::counter_lock`]); this statement moves the same counter every
    // cap claim does.
    let _guard = cap::counter_lock().await;
    let (mut result, mut errors) = transaction_with_retry(
        db,
        &sql,
        &[
            ("id".into(), enrollment.id.record().into_value()),
            ("row".into(), enrollment.clone().into_value()),
            ("course".into(), course.record().into_value()),
            ("usr".into(), user.record().into_value()),
            ("unlimited".into(), cap::UNLIMITED.into_value()),
        ],
        &[HELD_MARK, FULL_MARK, UNFIT_MARK],
    )
    .await?;
    // Someone placed this pair first; their row is the answer, and no seat
    // was spent finding that out. It is read first: it outranks the refusal
    // below, and the gate aborts before the seat is touched.
    if errors
        .values()
        .any(|error| error.to_string().contains(HELD_MARK))
    {
        return match read_for_user(db, course, user).await? {
            Some(existing) => disown_if_pumped(db, existing).await,
            None => Err(AppError::Internal("failed to enroll user".into())),
        };
    }
    // Demoted while this ran: the same refusal the service's own gate makes
    // a moment earlier, and the only one that can arrive after it.
    if errors
        .values()
        .any(|error| error.to_string().contains(UNFIT_MARK))
    {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "only students can be enrolled in a course",
        }));
    }
    // Full, or the course is gone — the conditional write matches nothing
    // either way, and only this path pays for the read that tells them
    // apart.
    if errors
        .values()
        .any(|error| error.to_string().contains(FULL_MARK))
    {
        return match crate::db::course::read(db, course).await? {
            Some(_) => Err(AppError::Conflict("the course is full")),
            None => Err(AppError::NotFound),
        };
    }
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    // Slots count BEGIN, two LETs and two IFs, plus the holder claim's own
    // statements — counted, not tallied by hand, so a statement added there
    // cannot read back the wrong result.
    result
        .take::<Vec<Enrollment>>(5 + held_by.len())?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::Internal("failed to enroll user".into()))
}

/// A hand enroll landing on a row a class pumped takes the row *off* the
/// class: `source` goes, so no later class sweep can unenroll a student an
/// operator placed on purpose. The mirror of the rule the sweep already
/// holds in the other direction — a manual unenroll wins, permanently — and
/// without it "hand-placed" was a state only a first enroll could reach.
///
/// `UNSET`, not `= NONE`: absence *is* the meaning of this column (see the
/// struct doc), and it is what a hand enroll writes on the create path.
async fn disown_if_pumped(db: &Database, existing: Enrollment) -> Result<Enrollment, AppError> {
    if existing.source.is_none() {
        return Ok(existing);
    }
    let mut result = db
        .query("UPDATE $id UNSET source RETURN AFTER")
        .bind(("id", existing.id.record()))
        .await?
        .check()?;
    // A row that is no longer there is not an internal error: an unenroll
    // (or the class sweep a role change runs) landing between the read
    // above and this write matches nothing, and the caller asked to be
    // enrolled — which they were, by the row this call was handed. The
    // disown is all that is lost, and it is lost to a delete that took the
    // whole row with it.
    Ok(result
        .take::<Vec<Enrollment>>(0)?
        .into_iter()
        .next()
        .unwrap_or(existing))
}

/// Some(_) iff `user` is enrolled in `course` — the grading gate.
pub async fn read_for_user(
    db: &Database,
    course: &CourseId,
    user: &UserId,
) -> Result<Option<Enrollment>, AppError> {
    let mut result = db
        .query("SELECT * FROM enrollment WHERE course = $course AND user = $usr LIMIT 1")
        .bind(("course", course.record()))
        .bind(("usr", user.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<Enrollment>>(0)?.into_iter().next())
}

pub async fn list_for_course(
    db: &Database,
    course: &CourseId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Enrollment>, i64), AppError> {
    PagedList::new("enrollment WHERE course = $course", "ORDER BY id DESC")
        .bind("course", course.record())
        .run(limit, offset, db)
        .await
}

pub async fn remove(
    db: &Database,
    course: &CourseId,
    user: &UserId,
) -> Result<Option<Enrollment>, AppError> {
    // The seat comes back in the same transaction as the row that held it,
    // so nothing but a lost transaction can drift the counter.
    let mut result = db
        .query(
            "BEGIN TRANSACTION;
             LET $gone = (DELETE enrollment WHERE course = $course AND user = $usr RETURN BEFORE);
             UPDATE $course SET enrollment_count = math::max([(enrollment_count ?? 0) - array::len($gone), 0]);
             RETURN $gone;
             COMMIT TRANSACTION;",
        )
        .bind(("course", course.record()))
        .bind(("usr", user.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<Enrollment>>(3)?.into_iter().next())
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
