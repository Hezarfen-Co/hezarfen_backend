//! The `enrollment` table: the roster read and listing, the counter-returning
//! delete, and the enroll claim — one transaction that checks the pair free
//! and the member still a student. The target-user gates the HTTP handlers pay
//! live in [`crate::service::enrollment`].
//!
//! Enrollment keys on the *instance* now (D4): a student is enrolled in the
//! Matematik that 5-A teaches, not in a school-wide Matematik row. The seat
//! gate is gone with the catalog's `capacity` (D5) — the roster counter is a
//! count, not a ceiling — so the only refusals left are "already enrolled"
//! (which is an idempotent hit, not an error) and "that user is not a
//! student".

use crate::database::{Database, tx_with_retry, unique_violation};
use crate::db::page::PagedList;
use crate::domain::class_course::ClassCourseId;
use crate::domain::class_group::ClassGroupId;
use crate::domain::course::CourseId;
use crate::domain::enrollment::Enrollment;
use crate::domain::role::Role;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// What [`enroll`]'s transaction settled on.
enum Verdict {
    /// This pair already holds a row.
    Held,
    /// The person being enrolled is no longer a student.
    Unfit,
    /// The row is written.
    Made(Enrollment),
}

/// Enroll (idempotently) `user` into one instance. The table's primary key
/// *is* the (instance, user) pair, so concurrent enrolls of the same pair
/// converge on one row instead of racing into a 500: the loser reads the
/// winner's row back and returns it. An already enrolled user is returned
/// as-is — the counter moves once, and never twice for one row — except that a
/// row a class pumped is *disowned* on the way out (see
/// [`disown_if_pumped`]): the operator's hand enroll is what the row now
/// records.
///
/// `source` is the class the roster pump wrote this row on behalf of, or
/// `None` for a hand-placed (seçmeli) enrollment. The two halves of that
/// rule: only a row carrying the key is a class's to take back
/// ([`crate::db::class_pump`]), and a hand unenroll wins permanently.
///
/// The `enrollment_count` counter moves in the same statement as the row (a
/// refused insert takes its own bump back), so the instance's roster size
/// cannot drift from the rows behind it.
///
/// Gate order is the old THROW order: an existing pair outranks the role
/// check. The role check takes the user row `FOR NO KEY UPDATE`, the same row
/// a demotion cascade updates before its sweep, so grant and demotion
/// serialize on the row lock in both directions — a row landing after the
/// sweep is impossible, and a demotion landing before this check refuses it.
pub async fn enroll(
    db: &Database,
    class_course: &ClassCourseId,
    user: &UserId,
    enrolled_by: &UserId,
    source: Option<&ClassGroupId>,
) -> Result<Enrollment, AppError> {
    if let Some(existing) = read_for_user(db, class_course, user).await? {
        return disown_if_pumped(db, existing).await;
    }
    let instance = class_course.clone();
    let (user_id, enrolled_by_id) = (*user, *enrolled_by);
    let source_id = source.map(ClassGroupId::uuid);
    let created_at = Timestamp::now();
    let verdict = tx_with_retry(db, false, async move |tx| {
        // The duplicate gate rides ahead of the role check, so "you are
        // already in" still outranks everything.
        let held = sqlx::query!(
            r#"SELECT 1 AS "one" FROM enrollment WHERE class_course = $1 AND app_user = $2"#,
            instance.uuid(),
            user_id.uuid(),
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
            user_id.uuid(),
        )
        .fetch_optional(&mut *tx)
        .await?;
        if role.map(|row| row.role).as_deref() != Some(Role::Student.as_str()) {
            return Ok(Verdict::Unfit);
        }
        // The counter and the row in one statement: zero rows means the
        // instance is gone — the `404` the caller's own read gives an
        // instant earlier.
        let row = sqlx::query_as!(
            Enrollment,
            r#"WITH seat AS (
                 UPDATE class_course
                    SET enrollment_count = enrollment_count + 1
                  WHERE id = $1
                  RETURNING 1)
               INSERT INTO enrollment (class_course, app_user, enrolled_by, source, created_at)
               SELECT $1, $2, $3, $4, $5 WHERE EXISTS (SELECT 1 FROM seat)
               RETURNING class_course AS "class_course: ClassCourseId",
                         app_user AS "user: UserId", enrolled_by AS "enrolled_by: UserId",
                         source AS "source: ClassGroupId",
                         created_at AS "created_at: Timestamp""#,
            instance.uuid(),
            user_id.uuid(),
            enrolled_by_id.uuid(),
            source_id,
            created_at.as_millis(),
        )
        .fetch_optional(&mut *tx)
        .await?;
        match row {
            Some(row) => Ok(Verdict::Made(row)),
            None => {
                // A same-student racer whose held-check predated the winner's
                // commit queues on the role lock and wakes to its own row
                // already there: that is the replay, not a refusal. Fresh
                // snapshot per statement, so this read sees the committed
                // winner.
                let now_held = sqlx::query!(
                    r#"SELECT 1 AS "one" FROM enrollment
                       WHERE class_course = $1 AND app_user = $2"#,
                    instance.uuid(),
                    user_id.uuid(),
                )
                .fetch_optional(&mut *tx)
                .await?;
                if now_held.is_some() {
                    return Ok(Verdict::Held);
                }
                // No row and no winner: the instance itself is gone.
                Err(AppError::NotFound)
            }
        }
    })
    .await;
    // The 23505 backstop folds into the Held verdict before the mapping:
    // someone placed this pair between the gate and the insert, and their
    // row is the answer — no counter move was spent finding that out.
    let verdict = match verdict {
        Err(AppError::Db(err))
            if unique_violation(&err) == Some("enrollment_class_course_user") =>
        {
            Ok(Verdict::Held)
        }
        other => other,
    };
    match verdict {
        Ok(Verdict::Made(row)) => Ok(row),
        Ok(Verdict::Held) => match read_for_user(db, class_course, user).await? {
            Some(existing) => Ok(existing),
            None => Err(AppError::Internal("failed to enroll user".into())),
        },
        // Demoted while this ran: the same refusal the service's own gate
        // makes a moment earlier, and the only one that can arrive after it.
        Ok(Verdict::Unfit) => Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "only students can be enrolled in a course",
        })),
        Err(err) => Err(err),
    }
}

/// A hand enroll landing on a row a class pumped takes the row *off* the
/// class: `source` goes, so no later class sweep can unenroll a student an
/// operator placed on purpose. The mirror of the rule the sweep already holds
/// in the other direction — a manual unenroll wins, permanently — and without
/// it "hand-placed" was a state only a first enroll could reach.
///
/// `NULL`, not absence: the column is a nullable uuid whose NULL *is* the
/// meaning a missing key used to carry, and it is what a hand enroll writes on
/// the create path too.
async fn disown_if_pumped(db: &Database, existing: Enrollment) -> Result<Enrollment, AppError> {
    if existing.get_source().is_none() {
        return Ok(existing);
    }
    let disowned = sqlx::query_as!(
        Enrollment,
        r#"UPDATE enrollment SET source = NULL
           WHERE class_course = $1 AND app_user = $2
           RETURNING class_course AS "class_course: ClassCourseId",
                     app_user AS "user: UserId", enrolled_by AS "enrolled_by: UserId",
                     source AS "source: ClassGroupId",
                     created_at AS "created_at: Timestamp""#,
        existing.get_class_course().uuid(),
        existing.get_user().uuid(),
    )
    .fetch_optional(db)
    .await?;
    // A row that is no longer there is not an internal error: an unenroll (or
    // the class sweep a role change runs) landing between the read above and
    // this write matches nothing, and the caller asked to be enrolled — which
    // they were, by the row this call was handed. The disown is all that is
    // lost, and it is lost to a delete that took the whole row with it.
    Ok(disowned.unwrap_or(existing))
}

/// Some(_) iff `user` is enrolled in that instance — the grading gate.
pub async fn read_for_user(
    db: &Database,
    class_course: &ClassCourseId,
    user: &UserId,
) -> Result<Option<Enrollment>, AppError> {
    let row = sqlx::query_as!(
        Enrollment,
        r#"SELECT class_course AS "class_course: ClassCourseId", app_user AS "user: UserId",
               enrolled_by AS "enrolled_by: UserId", source AS "source: ClassGroupId",
               created_at AS "created_at: Timestamp"
           FROM enrollment WHERE class_course = $1 AND app_user = $2"#,
        class_course.uuid(),
        user.uuid(),
    )
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// Whether `user` is one of `course`'s students right now — the point probe
/// behind an event addressed at a course.
///
/// The two membership tiers again: an enrollment in any of the course's
/// instances, or an individual club/etüt membership. Both are existence
/// probes on their own indexes; one entity's audience must not fold a whole
/// roster to answer yes.
pub async fn user_is_in_course(
    db: &Database,
    course: &CourseId,
    user: &UserId,
) -> Result<bool, AppError> {
    let row = sqlx::query!(
        r#"SELECT 1 AS "one" FROM (
               SELECT e.app_user FROM enrollment e
                WHERE e.app_user = $2
                  AND e.class_course IN (SELECT id FROM class_course WHERE course = $1)
               UNION ALL
               SELECT m.app_user FROM course_membership m
                WHERE m.course = $1 AND m.app_user = $2
           ) people LIMIT 1"#,
        course.uuid(),
        user.uuid(),
    )
    .fetch_optional(db)
    .await?;
    Ok(row.is_some())
}

/// Every student the catalog course reaches right now — the union of its
/// instances' rosters and its own individual memberships.
///
/// One course's "students" is two populations since D9 split the two tiers: a
/// ders reaches them through the instance a şube teaches (enrollment), and a
/// club/etüt through the school-scoped membership. A reader that wants the
/// course's audience — an event addressed at the course, a who-missed report —
/// means both, and deduping in SQL keeps a student who is in two of its
/// instances on the list once.
pub async fn list_users_for_course(
    db: &Database,
    course: &CourseId,
) -> Result<Vec<UserId>, AppError> {
    let rows = sqlx::query!(
        r#"SELECT DISTINCT app_user AS "user!: UserId" FROM (
               SELECT e.app_user FROM enrollment e
               JOIN class_course cc ON cc.id = e.class_course
               WHERE cc.course = $1
               UNION ALL
               SELECT m.app_user FROM course_membership m WHERE m.course = $1
           ) people ORDER BY app_user"#,
        course.uuid(),
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|row| row.user).collect())
}

/// The instance's roster, newest first — the natural-PK columns sort the way
/// the old composite key did, with its instance prefix constant.
pub async fn list_for_class_course(
    db: &Database,
    class_course: &ClassCourseId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Enrollment>, i64), AppError> {
    PagedList::new(
        "enrollment WHERE class_course = $1",
        "ORDER BY class_course DESC, app_user DESC",
    )
    .bind(class_course.uuid())
    .run::<Enrollment>(limit, offset, db)
    .await
}

/// Take the row and give the instance's roster counter back in one statement,
/// so nothing but a lost transaction can drift the counter. `None` when the
/// pair held no row.
pub async fn remove(
    db: &Database,
    class_course: &ClassCourseId,
    user: &UserId,
) -> Result<Option<Enrollment>, AppError> {
    let gone = sqlx::query_as!(
        Enrollment,
        r#"WITH gone AS (
             DELETE FROM enrollment
              WHERE class_course = $1 AND app_user = $2
              RETURNING class_course, app_user, enrolled_by, source, created_at),
           seated AS (
             UPDATE class_course
                SET enrollment_count = GREATEST(
                    enrollment_count - (SELECT count(*) FROM gone), 0)
              WHERE id = $1
              RETURNING 1)
           SELECT class_course AS "class_course: ClassCourseId",
               app_user AS "user: UserId", enrolled_by AS "enrolled_by: UserId",
               source AS "source: ClassGroupId", created_at AS "created_at: Timestamp"
           FROM gone"#,
        class_course.uuid(),
        user.uuid(),
    )
    .fetch_optional(db)
    .await?;
    Ok(gone)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real `app_user` row: students, teachers and managers are foreign
    /// keys now. The label names the row's username; the id is minted, so
    /// repeated calls are new people, not the same row.
    async fn a_person(db: &Database, label: &str, role: &str) -> UserId {
        let user = UserId::generate();
        sqlx::query(
            "INSERT INTO app_user (id, username, created_at, role) \
             VALUES ($1, $2, 0, $3)",
        )
        .bind(user.uuid())
        .bind(format!("{label}-{}", &user.key()[30..]))
        .bind(role)
        .execute(db)
        .await
        .unwrap();
        user
    }

    /// A real instance — class, course and the link between them — because
    /// the enrollment's own key is a foreign key now.
    async fn an_instance(db: &Database) -> (ClassCourseId, UserId) {
        use crate::db::class_member::tests::{a_class, a_course};
        use crate::domain::class_course::ClassCourseId;

        let manager = crate::db::class_member::tests::fixture_user(db, "manager").await;
        let class = a_class("9-A", db).await;
        let course = a_course("algebra", db).await;
        crate::service::class_course::attach(db, &class, &course, &manager)
            .await
            .unwrap();
        let id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM class_course WHERE class = $1")
            .bind(class.uuid())
            .fetch_one(db)
            .await
            .unwrap();
        (
            ClassCourseId::from_key(&id.to_string()),
            a_person(db, "student", "student").await,
        )
    }

    /// Enrollment is idempotent and the roster counter moves once: a repeat
    /// enroll answers the row already there and leaves the counter as it
    /// stands.
    #[tokio::test]
    async fn a_repeat_enroll_moves_the_counter_once() {
        let (db, _leases) = crate::database::init_test_db().await;
        let (instance, student) = an_instance(&db).await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;

        for _ in 0..2 {
            enroll(&db, &instance, &student, &manager, None)
                .await
                .unwrap();
        }
        assert_eq!(
            crate::db::class_member::tests::counter("enrollment_count", instance.uuid(), &db).await,
            1,
            "one row, one count"
        );
    }

    /// A hand-placed enrollment carries no source key, and removing it gives
    /// the counter back in the same statement.
    #[tokio::test]
    async fn a_hand_placed_row_carries_no_source_and_gives_its_count_back() {
        let (db, _leases) = crate::database::init_test_db().await;
        let (instance, student) = an_instance(&db).await;
        let manager = crate::db::class_member::tests::fixture_user(&db, "manager").await;

        let row = enroll(&db, &instance, &student, &manager, None)
            .await
            .unwrap();
        assert!(
            row.get_source().is_none(),
            "a hand-placed row may carry no source key"
        );

        let removed = remove(&db, &instance, &student).await.unwrap();
        assert!(removed.is_some(), "the row was there to take back");
        assert_eq!(
            crate::db::class_member::tests::counter("enrollment_count", instance.uuid(), &db).await,
            0,
            "the counter goes back with the row"
        );
    }
}
