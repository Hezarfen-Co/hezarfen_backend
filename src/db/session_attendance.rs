//! The `session_attendance` table: the lesson roll call and the two badge
//! counters its mark and remove move. Every write is a single transaction
//! whose gates ride inside it; the target-eligibility rules (staff rows are
//! management's, only students attend, enrollment) are
//! [`crate::service::session_attendance`]'s to judge before calling here.

use crate::database::{Database, tx_with_retry};
use crate::db::page::PagedList;
use crate::domain::attendance::AttendanceStatus;
use crate::domain::course::CourseId;
use crate::domain::course_session::{CourseSession, CourseSessionId};
use crate::domain::session_attendance::SessionAttendance;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Record (or overwrite) `user`'s status for the session. The table's
/// primary key *is* the (session, user) pair, so the mark is a single
/// atomic upsert — concurrent marks converge on the one row.
///
/// The "session still exists" gate is a row lock: the transaction takes the
/// session row `FOR NO KEY UPDATE` before anything else, so a session (or
/// course) delete cascading beneath this mark simply waits, and a delete
/// that got there first leaves this gate matching nothing — `404`, the
/// mirror of [`crate::db::course_session::delete`]. The caller's
/// pre-flight read sits several round trips in front of this write, which
/// is exactly the gap the old code had to fake with a bump-and-restore
/// write on the session row; the lock is the same serialization without
/// the trick. It also freezes `starts_at` and `held_counted_at` for the
/// transaction, so two simultaneous first marks take turns and the
/// lesson-held credit below is stamped exactly once.
///
/// Two badge counters move in this same transaction, both of them read
/// from the store rather than taken on the caller's word:
///
/// - `lessons_attended_total`, on the person marked, as a *delta* rather
///   than an increment, because this is an upsert: a re-mark that changes
///   nothing must change nothing, and a teacher's correction must move it
///   back down. The pair's stored status is read before the upsert
///   overwrites it (`None` when the pair has no row yet), so it writes
///   only on a real crossing of the attended line, and only for a
///   **student** — attending lessons is a student's badge, the same
///   student-only rule enrolling and sitting an exam already carry, so a
///   teacher marked present in their own lesson moves it in neither
///   direction. The live `role` decides that, not the role someone held
///   when the row was written.
/// - `lessons_held_total`, on the *session's* teacher, exactly once per
///   session: the first roll call taken stamps `held_counted_at` on the
///   lesson and every later mark sees the stamp and credits nothing. It
///   counts lessons that actually happened — scheduling one and
///   cancelling it earns nothing, which is why the credit does not live
///   in `crate::db::course_session::create`. Two simultaneous first
///   marks serialize on the session row's lock, so the stamp cannot be
///   set twice.
///
///   "Actually happened" is a clock reading, not a request count: the
///   credit waits for the lesson's own `starts_at` to arrive. Roll call is
///   deliberately *not* time-gated — a teacher may open the sheet early
///   and is never refused — so without this a teacher could schedule two
///   hundred lessons for next week, mark one student in each, and hold two
///   hundred lessons this afternoon. Because the stamp is written only on
///   the branch that credits, a sheet opened early and touched again after
///   the bell still credits exactly once, then; a sheet never touched
///   again credits never, which is the deliberate cost of not storing a
///   promise the passage of time would have to redeem on its own.
///
///   `lessons_attended_total` is *not* gated the same way, and the
///   asymmetry is on purpose: it is a delta off the row's stored status,
///   which carries no record of whether the credit was ever taken, so a
///   gate would refund at the correction what the early mark never
///   credited — a student's real lessons eaten by someone else's farm. It
///   is also nobody's self-service: only a teacher can mark a student, so
///   the counter cannot be moved by the person it decorates.
//
// corner-cut: that asymmetry leaves a teacher able to inflate a *student's*
// attendance with future-dated lessons. Closing it needs the credit stamped
// on the roll-call row itself and refunded off that stamp — the same column
// the live-role note on `remove` below wants, and the shape
// `counted_on_time` already uses for homework. One column closes both;
// neither is worth it until a real complaint names one.
pub async fn mark(
    db: &Database,
    session: &CourseSession,
    user: &UserId,
    status: AttendanceStatus,
    marked_by: &UserId,
) -> Result<SessionAttendance, AppError> {
    let attended = crate::domain::session_attendance::counts_as_attended(&status);
    let delta: i64 = if attended { 1 } else { -1 };
    let now = Timestamp::now();
    tx_with_retry(db, false, async |tx| {
        // The gate and the serialization point: a live session row, locked
        // against the delete cascade and every rival mark.
        let sess = sqlx::query!(
            r#"SELECT teacher, starts_at, held_counted_at
               FROM course_session WHERE id = $1 FOR NO KEY UPDATE"#,
            session.id.uuid(),
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(sess) = sess else {
            return Err(AppError::NotFound);
        };
        // The pair's stored status — the pre-image the attended delta is
        // computed off.
        let was = sqlx::query!(
            r#"SELECT status FROM session_attendance
               WHERE session = $1 AND app_user = $2"#,
            session.id.uuid(),
            user.uuid(),
        )
        .fetch_optional(&mut *tx)
        .await?
        .map(|row| row.status);
        // The live role of the person marked (a gone user row marks as
        // non-student — no badge to move; the service gate has already
        // refused a vanished target before this write ran).
        let is_student = sqlx::query!(
            r#"SELECT role = 'student' AS "is_student!: bool" FROM app_user WHERE id = $1"#,
            user.uuid(),
        )
        .fetch_optional(&mut *tx)
        .await?
        .map(|row| row.is_student)
        .unwrap_or(false);
        let row = sqlx::query_as!(
            SessionAttendance,
            r#"INSERT INTO session_attendance (session, app_user, course, status, marked_by)
               VALUES ($1, $2, $3, $4, $5)
               ON CONFLICT (session, app_user) DO UPDATE
                 SET status = EXCLUDED.status, marked_by = EXCLUDED.marked_by
               RETURNING session AS "session: CourseSessionId", app_user AS "user: UserId",
                         course AS "course: CourseId", status AS "status: AttendanceStatus",
                         marked_by AS "marked_by: UserId""#,
            session.id.uuid(),
            user.uuid(),
            session.course.uuid(),
            status.as_str(),
            marked_by.uuid(),
        )
        .fetch_one(&mut *tx)
        .await?;
        // The attended badge: student-only, and only on a real crossing.
        let was_attended = was
            .as_deref()
            .is_some_and(|was| was == "present" || was == "late");
        if is_student && was_attended != attended {
            sqlx::query!(
                r#"UPDATE app_user
                     SET lessons_attended_total = GREATEST(lessons_attended_total + $2, 0)
                   WHERE id = $1"#,
                user.uuid(),
                delta,
            )
            .execute(&mut *tx)
            .await?;
        }
        // The held badge: once per lesson, and only once the lesson began.
        if sess.held_counted_at.is_none() && sess.starts_at <= now.as_millis() {
            sqlx::query!(
                r#"UPDATE course_session SET held_counted_at = $2 WHERE id = $1"#,
                session.id.uuid(),
                now.as_millis(),
            )
            .execute(&mut *tx)
            .await?;
            sqlx::query!(
                r#"UPDATE app_user SET lessons_held_total = lessons_held_total + 1
                   WHERE id = $1"#,
                sess.teacher,
            )
            .execute(&mut *tx)
            .await?;
        }
        Ok(row)
    })
    .await
}

pub async fn list_for_session(
    db: &Database,
    session: &CourseSessionId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<SessionAttendance>, i64), AppError> {
    // The old composite key `{session}_{user}` sorted by user within a
    // session — its session prefix was constant — so the natural-PK
    // columns sort the same way.
    PagedList::new(
        "session_attendance WHERE session = $1",
        "ORDER BY session DESC, app_user DESC",
    )
    .bind(session.uuid())
    .run::<SessionAttendance>(limit, offset, db)
    .await
}

/// Every roll-call row ever recorded for `user` — the session half of the
/// attendance report. Deliberately not filtered by current enrollment:
/// attendance is a historical record, so unenrolling hides marks (report
/// semantics) but never absences.
pub async fn list_for_user(
    db: &Database,
    user: &UserId,
) -> Result<Vec<SessionAttendance>, AppError> {
    let rows = sqlx::query_as!(
        SessionAttendance,
        r#"SELECT session AS "session: CourseSessionId", app_user AS "user: UserId",
                  course AS "course: CourseId", status AS "status: AttendanceStatus",
                  marked_by AS "marked_by: UserId"
           FROM session_attendance WHERE app_user = $1
           ORDER BY session DESC, app_user DESC"#,
        user.uuid(),
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// Clearing a row the student was counted for gives the count back, in the
/// delete's own transaction — the other direction of the delta in
/// [`mark`], so a row that never existed and a row that
/// was withdrawn leave the same number behind. Only *this* route decrements:
/// [`crate::db::course_session::delete`]'s cascade sweeps the rows with a `DELETE` of
/// its own and never comes through here, which is the ruling
/// `exam_sat_total` already carries — deleting the lesson does not un-attend
/// it. `lessons_held_total` is never given back either: the lesson was
/// taken, and clearing one student's row does not un-take it.
///
/// Student-only on the same terms as the mark, and read from the store for
/// the same reason.
pub async fn remove(
    db: &Database,
    session: &CourseSessionId,
    user: &UserId,
) -> Result<Option<SessionAttendance>, AppError> {
    tx_with_retry(db, false, async |tx| {
        // The pre-image and the row's right to exist in one locked
        // statement: the row cannot vanish (or change status) under this
        // transaction, and `None` here is simply "nothing to remove".
        let Some(gone) = sqlx::query_as!(
            SessionAttendance,
            r#"SELECT session AS "session: CourseSessionId", app_user AS "user: UserId",
               course AS "course: CourseId", status AS "status: AttendanceStatus",
               marked_by AS "marked_by: UserId"
               FROM session_attendance
               WHERE session = $1 AND app_user = $2
               FOR UPDATE"#,
            session.uuid(),
            user.uuid(),
        )
        .fetch_optional(&mut *tx)
        .await?
        else {
            return Ok(None);
        };
        if crate::domain::session_attendance::counts_as_attended(&gone.status) {
            // Student-only, live role: the same rule the mark pays.
            let student = sqlx::query!(
                r#"SELECT role = 'student' AS "is_student!: bool" FROM app_user WHERE id = $1"#,
                user.uuid(),
            )
            .fetch_optional(&mut *tx)
            .await?
            .map(|row| row.is_student)
            .unwrap_or(false);
            if student {
                sqlx::query!(
                    r#"UPDATE app_user
                         SET lessons_attended_total = GREATEST(lessons_attended_total - 1, 0)
                       WHERE id = $1"#,
                    user.uuid(),
                )
                .execute(&mut *tx)
                .await?;
            }
        }
        sqlx::query!(
            r#"DELETE FROM session_attendance WHERE session = $1 AND app_user = $2"#,
            session.uuid(),
            user.uuid(),
        )
        .execute(&mut *tx)
        .await?;
        Ok(Some(gone))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::DEFAULT_ATTENDANCE_STATUSES;
    use crate::database::init_mem;
    use crate::domain::course_session::SessionTopic;
    use crate::domain::role::Role;

    /// A user row carrying its role — the counters are `UPDATE`s on one (which
    /// writes nothing at all to a record that does not exist), and the live
    /// role is what decides whether the attended counter moves.
    async fn a_user(key: &str, role: Role, db: &Database) -> UserId {
        let user = UserId::from_key(key);
        db.query("CREATE $usr SET username = $name, password_hash = 'x', role = $role")
            .bind(("usr", user.record()))
            .bind(("name", key.to_string()))
            .bind(("role", role))
            .await
            .unwrap()
            .check()
            .unwrap();
        user
    }

    async fn a_student(key: &str, db: &Database) -> UserId {
        a_user(key, Role::Student, db).await
    }

    async fn a_teacher(key: &str, db: &Database) -> UserId {
        a_user(key, Role::Teacher, db).await
    }

    async fn a_session_at(teacher: &UserId, starts_at: i64, db: &Database) -> CourseSession {
        crate::db::course_session::create(
            db,
            &crate::db::course::a_test_course(db).await,
            teacher,
            SessionTopic::try_new("limits").unwrap(),
            Timestamp::from_millis(starts_at),
            None,
        )
        .await
        .unwrap()
    }

    /// A lesson long since begun — every test but the gate's own wants one.
    async fn a_session(teacher: &UserId, db: &Database) -> CourseSession {
        a_session_at(teacher, 1, db).await
    }

    /// A status this school allows — the four core ones plus a school-added
    /// `online`, which is exactly the open half of the set.
    fn status(value: &str) -> AttendanceStatus {
        let allowed: Vec<String> = DEFAULT_ATTENDANCE_STATUSES
            .iter()
            .map(|s| s.to_string())
            .chain(["online".to_string()])
            .collect();
        AttendanceStatus::try_new(value, &allowed).unwrap()
    }

    /// The two counters as the badge rules read them, off the stored row.
    async fn attended(user: &UserId, db: &Database) -> i64 {
        crate::db::badge::load(db, user)
            .await
            .unwrap()
            .get_lessons_attended()
    }

    async fn held(user: &UserId, db: &Database) -> i64 {
        crate::db::badge::load(db, user)
            .await
            .unwrap()
            .get_lessons_held()
    }

    /// The whole transition table in one pass: an upsert may only move the
    /// counter when the mark crosses the attended line, in either direction, so
    /// a re-mark and a swap within the same class are both no-ops.
    #[tokio::test]
    async fn the_counter_follows_every_crossing_and_no_other_move() {
        let db = init_mem().await.unwrap();
        let teacher = a_teacher("t", &db).await;
        let student = a_student("s", &db).await;
        let session = a_session(&teacher, &db).await;
        let mark = async |value: &str, db: &Database| {
            super::mark(db, &session, &student, status(value), &teacher)
                .await
                .unwrap();
        };

        for (value, expected, why) in [
            ("present", 1, "first mark counts"),
            ("present", 1, "re-marking the same status changes nothing"),
            ("late", 1, "late is attending too, so no crossing"),
            ("absent", 0, "a correction gives the count back"),
            ("absent", 0, "and does not keep giving it back"),
            ("late", 1, "absent -> late crosses back up"),
            ("excused", 0, "excused is neutral: a crossing down"),
            ("online", 0, "a school-added status stays neutral"),
            ("present", 1, "…and crossing up from one still counts"),
            ("online", 0, "…as does crossing down to one"),
        ] {
            mark(value, &db).await;
            assert_eq!(attended(&student, &db).await, expected, "{why}");
        }
        assert_eq!(attended(&teacher, &db).await, 0, "credited the marker");
        // Ten marks, one lesson: correcting a roll call is not holding another.
        assert_eq!(held(&teacher, &db).await, 1, "the lesson was counted twice");
    }

    /// The lesson is credited to its teacher when the roll call is *taken*, and
    /// once: the whole roster, and every later correction, ride the same stamp.
    #[tokio::test]
    async fn the_first_roll_call_counts_the_lesson_once() {
        let db = init_mem().await.unwrap();
        let teacher = a_teacher("t", &db).await;
        let other = a_teacher("o", &db).await;
        let session = a_session(&teacher, &db).await;
        assert_eq!(held(&teacher, &db).await, 0, "scheduling counts nothing");

        for key in ["s1", "s2", "s3"] {
            let student = a_student(key, &db).await;
            mark(&db, &session, &student, status("present"), &teacher)
                .await
                .unwrap();
            assert_eq!(held(&teacher, &db).await, 1, "{key} counted it again");
        }
        assert_eq!(held(&other, &db).await, 0, "credited the wrong teacher");

        // A second lesson is a second count — the stamp is per session.
        let second = a_session(&teacher, &db).await;
        let student = a_student("s4", &db).await;
        mark(&db, &second, &student, status("present"), &teacher)
            .await
            .unwrap();
        assert_eq!(held(&teacher, &db).await, 2);
    }

    /// A lesson is held when its own time comes, not when the sheet is opened.
    /// Roll call is never refused early, so ungated a teacher could schedule
    /// two hundred lessons for next week and hold all of them this afternoon.
    #[tokio::test]
    async fn a_lesson_that_has_not_started_holds_nothing_yet() {
        let db = init_mem().await.unwrap();
        let teacher = a_teacher("t", &db).await;
        let student = a_student("s", &db).await;
        let next_week =
            a_session_at(&teacher, Timestamp::now().as_millis() + 604_800_000, &db).await;

        mark(&db, &next_week, &student, status("present"), &teacher)
            .await
            .unwrap();
        assert_eq!(held(&teacher, &db).await, 0, "next week's lesson, today");
        // The mark itself stands — opening the sheet early is not an error.
        assert_eq!(attended(&student, &db).await, 1);

        // The bell rings. The same sheet, touched again, credits now — and
        // only once: nothing backfills, so the stamp is the whole guard.
        db.query("UPDATE $sess SET starts_at = 1")
            .bind(("sess", next_week.get_id().record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        for (status_value, why) in [("late", "the bell rang"), ("present", "held twice")] {
            mark(&db, &next_week, &student, status(status_value), &teacher)
                .await
                .unwrap();
            assert_eq!(held(&teacher, &db).await, 1, "{why}");
        }
    }

    /// Attending lessons is a student's badge. A teacher marked present in
    /// their own lesson (which management does, not the teacher) moves nothing
    /// for them as an attendee — while the roll call itself still counts as a
    /// lesson held.
    #[tokio::test]
    async fn a_teacher_marked_present_earns_nothing_for_attending() {
        let db = init_mem().await.unwrap();
        let teacher = a_teacher("t", &db).await;
        let session = a_session(&teacher, &db).await;

        mark(&db, &session, &teacher, status("present"), &teacher)
            .await
            .unwrap();
        assert_eq!(attended(&teacher, &db).await, 0, "staff attended a lesson");
        assert_eq!(held(&teacher, &db).await, 1, "the roll call was taken");

        // And the correction direction is just as closed: nothing was taken, so
        // nothing may be given back.
        mark(&db, &session, &teacher, status("absent"), &teacher)
            .await
            .unwrap();
        assert_eq!(attended(&teacher, &db).await, 0);
        remove(&db, session.get_id(), &teacher)
            .await
            .unwrap()
            .expect("the row was there");
        assert_eq!(attended(&teacher, &db).await, 0);
        assert_eq!(held(&teacher, &db).await, 1, "the lesson still happened");
    }

    /// Two lessons are two counts — the counter is per row, not per student.
    #[tokio::test]
    async fn each_lesson_counts_once_for_the_student_marked() {
        let db = init_mem().await.unwrap();
        let teacher = a_teacher("t", &db).await;
        let student = a_student("s", &db).await;
        let other = a_student("o", &db).await;
        for _ in 0..2 {
            let session = a_session(&teacher, &db).await;
            mark(&db, &session, &student, status("present"), &teacher)
                .await
                .unwrap();
        }
        assert_eq!(attended(&student, &db).await, 2);
        assert_eq!(attended(&other, &db).await, 0, "credited the wrong student");
    }

    /// Removing the row is the other direction of the delta, and only for a row
    /// that was counted.
    #[tokio::test]
    async fn removing_a_row_gives_back_only_what_it_took() {
        let db = init_mem().await.unwrap();
        let teacher = a_teacher("t", &db).await;
        let student = a_student("s", &db).await;

        let counted = a_session(&teacher, &db).await;
        mark(&db, &counted, &student, status("present"), &teacher)
            .await
            .unwrap();
        let neutral = a_session(&teacher, &db).await;
        mark(&db, &neutral, &student, status("absent"), &teacher)
            .await
            .unwrap();
        assert_eq!(attended(&student, &db).await, 1);

        remove(&db, neutral.get_id(), &student)
            .await
            .unwrap()
            .expect("the absent row was there");
        assert_eq!(attended(&student, &db).await, 1, "absent took nothing");
        remove(&db, counted.get_id(), &student)
            .await
            .unwrap()
            .expect("the present row was there");
        assert_eq!(attended(&student, &db).await, 0);
        assert!(
            remove(&db, counted.get_id(), &student)
                .await
                .unwrap()
                .is_none(),
            "nothing left to remove"
        );
        assert_eq!(attended(&student, &db).await, 0, "and nothing to give back");
        assert_eq!(
            held(&teacher, &db).await,
            2,
            "clearing a row un-held a lesson"
        );
    }

    /// The floor: however the corrections and removals interleave, and whatever
    /// a stale row starts from, the counter never goes negative.
    #[tokio::test]
    async fn the_counter_never_goes_below_zero() {
        let db = init_mem().await.unwrap();
        let teacher = a_teacher("t", &db).await;
        let student = a_student("s", &db).await;

        // Ten counted lessons whose counter was never credited — the stale-data
        // case, an account that predates the column. Every one of them is now
        // corrected away, which is ten decrements against a zero.
        let mut sessions = Vec::new();
        for _ in 0..10 {
            let session = a_session(&teacher, &db).await;
            mark(&db, &session, &student, status("present"), &teacher)
                .await
                .unwrap();
            sessions.push(session);
        }
        db.query(format!(
            "UPDATE $usr SET {LESSONS_ATTENDED_TOTAL_FIELD} = 0"
        ))
        .bind(("usr", student.record()))
        .await
        .unwrap()
        .check()
        .unwrap();

        for session in &sessions {
            mark(&db, session, &student, status("absent"), &teacher)
                .await
                .unwrap();
            remove(&db, session.get_id(), &student).await.unwrap();
            assert!(attended(&student, &db).await >= 0);
        }
        assert_eq!(attended(&student, &db).await, 0, "floored, never negative");
    }

    /// Deleting the lesson sweeps its roll-call rows with a `DELETE` of its own
    /// — it never reaches [`remove`] — so the count stands,
    /// the ruling every other counter here carries.
    #[tokio::test]
    async fn deleting_the_session_leaves_the_counters_alone() {
        let db = init_mem().await.unwrap();
        let teacher = a_teacher("t", &db).await;
        let student = a_student("s", &db).await;
        let session = a_session(&teacher, &db).await;
        mark(&db, &session, &student, status("present"), &teacher)
            .await
            .unwrap();

        crate::db::course_session::delete(&db, session)
            .await
            .unwrap();
        assert_eq!(attended(&student, &db).await, 1, "the cascade decremented");
        assert_eq!(held(&teacher, &db).await, 1, "the cascade decremented");
    }

    /// A mark on a session that is already gone is refused, so it cannot leave
    /// a count behind either.
    #[tokio::test]
    async fn a_refused_mark_moves_nothing() {
        let db = init_mem().await.unwrap();
        let teacher = a_teacher("t", &db).await;
        let student = a_student("s", &db).await;
        let session = a_session(&teacher, &db).await;
        let ghost = session.clone();
        crate::db::course_session::delete(&db, session)
            .await
            .unwrap();

        assert!(matches!(
            mark(&db, &ghost, &student, status("present"), &teacher).await,
            Err(AppError::NotFound)
        ));
        assert_eq!(attended(&student, &db).await, 0);
        assert_eq!(held(&teacher, &db).await, 0, "a refused mark held a lesson");
    }

    /// The claim the existence gate cannot make by *reading*. A `DEFINE EVENT`
    /// on `course_session` fires inside the delete's own transaction, and
    /// [`crate::db::course_session::delete`] sweeps its children before removing the row,
    /// so the `SLEEP` opens exactly the window a mark has to lose: the sheet is
    /// written after the sweep has run, and used to commit straight past it.
    ///
    /// The two flavors raced are the ones the credit branch never wrote for — a
    /// lesson that has not begun, and a student with no row yet on a lesson
    /// already counted. A re-mark of a row that *exists* was never in danger:
    /// the cascade and the upsert write that child's own key and collide there.
    ///
    /// Real server, and `#[ignore]`d for it: the subject is the store's
    /// conflict detection, which [`init_mem`]'s embedded engine does not have —
    /// it commits both writes and answers `Ok` to each, so this passes there on
    /// broken code. Mutation-tested: dropping the unconditional
    /// `UPDATE $sess` from `mark` turns it red.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs a real SurrealDB server: podman start hezarfen-surrealdb && cargo test -- --ignored"]
    async fn a_mark_written_inside_a_delete_never_outlives_the_session() {
        let (db, _serialized) = crate::database::init_test_server("session_attendance_race").await;
        db.query(
            "DEFINE EVENT hold_the_window ON TABLE course_session WHEN $event = 'DELETE' \
             THEN { SLEEP 1s; };",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

        let teacher = a_teacher("t", &db).await;
        let (mut swept, mut orphans) = (0, 0);
        for (round, already_counted) in [false, true, false, true].into_iter().enumerate() {
            let session = if already_counted {
                // Counted by somebody else's mark, so this one credits nothing.
                let session = a_session(&teacher, &db).await;
                let first = a_student(&format!("f{round}"), &db).await;
                mark(&db, &session, &first, status("present"), &teacher)
                    .await
                    .unwrap();
                session
            } else {
                a_session_at(&teacher, Timestamp::now().as_millis() + 604_800_000, &db).await
            };
            let id = session.get_id().clone();
            let ghost = session.clone();
            let drop_it = {
                let db = db.clone();
                tokio::spawn(async move { crate::db::course_session::delete(&db, session).await })
            };
            // The mark starts inside the held window: the sweep has run and the
            // session row is gone but uncommitted — which is exactly what a
            // read of that session still believes.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let marked = {
                let (db, teacher) = (db.clone(), teacher.clone());
                let student = a_student(&format!("s{round}"), &db).await;
                tokio::spawn(async move {
                    mark(&db, &ghost, &student, status("present"), &teacher).await
                })
            };
            let (drop_it, marked) = (drop_it.await.unwrap(), marked.await.unwrap());
            assert!(
                !matches!(marked, Err(AppError::Db(_))),
                "round {round}: a raced mark must be answered, not 500: {marked:?}"
            );

            // Stored state is the whole verdict; a return value is not evidence.
            if crate::db::course_session::read(&db, &id)
                .await
                .unwrap()
                .is_none()
            {
                swept += 1;
                orphans += list_for_session(&db, &id, None, 0).await.unwrap().0.len();
            } else if drop_it.is_ok() {
                panic!("round {round}: the delete reported success but the session is still there");
            }
        }
        eprintln!(
            "CourseSession::delete raced by a roll call: {swept}/4 rounds deleted the session"
        );
        assert!(
            swept > 0,
            "no round ever deleted the session, so the window was never reached"
        );
        assert_eq!(orphans, 0, "a roll call outlived its session");
    }
}
