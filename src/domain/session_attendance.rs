use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{
    LESSON_COUNTED_AT_FIELD, LESSONS_ATTENDED_TOTAL_FIELD, LESSONS_HELD_TOTAL_FIELD,
    SESSION_ATTENDANCE_TABLE,
};
use crate::database::{Database, transaction_with_retry};
use crate::db::page::PagedList;
use crate::domain::attendance::AttendanceStatus;
use crate::domain::course::CourseId;
use crate::domain::course_session::{CourseSession, CourseSessionId};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct SessionAttendanceId(RecordId);

impl SessionAttendanceId {
    /// A deterministic id for the (session, user) pair. Because the same pair
    /// always maps to the same record id, marking is a single atomic UPSERT with
    /// no find-then-insert race, and one-row-per-pair holds by construction.
    /// ULID keys are alphanumeric, so `_` is an unambiguous joiner.
    pub fn composite(session: &CourseSessionId, user: &UserId) -> Self {
        Self(RecordId::new(
            SESSION_ATTENDANCE_TABLE,
            format!("{}_{}", session.key(), user.key()),
        ))
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

/// Whether a status means the person was *there*, for the `lessons_attended`
/// badge counter. Exactly the rule `GET /attendance/me` already publishes —
/// `StatusCounts::tally` in `src/web/attendance.rs` puts `present` and `late`
/// over the line and leaves `excused` and every school-added status neutral —
/// so a student's badge and their attendance rate never disagree about what
/// attending is.
///
/// Hardcoding the two literals is safe by construction: statuses are the
/// school's to extend, but [`crate::domain::settings`] refuses any write that
/// drops one of the core four, so `present` and `late` can never be renamed
/// away. The SQL below repeats them — they are one rule in two languages, and
/// the transition test is what pins them together.
fn counts_as_attended(status: &AttendanceStatus) -> bool {
    matches!(status.as_str(), "present" | "late")
}

/// One person's roll-call state for one lesson. `course` is denormalized from
/// the session so the per-course attendance report is a single indexed query
/// (`WHERE user = $u`) with no join.
#[derive(Debug, Clone, SurrealValue)]
pub struct SessionAttendance {
    id: SessionAttendanceId,
    session: CourseSessionId,
    course: CourseId,
    user: UserId,
    status: AttendanceStatus,
    marked_by: UserId,
}

impl SessionAttendance {
    pub fn get_id(&self) -> &SessionAttendanceId {
        &self.id
    }

    pub fn get_session(&self) -> &CourseSessionId {
        &self.session
    }

    pub fn get_course(&self) -> &CourseId {
        &self.course
    }

    pub fn get_user(&self) -> &UserId {
        &self.user
    }

    pub fn get_status(&self) -> &AttendanceStatus {
        &self.status
    }

    pub fn get_marked_by(&self) -> &UserId {
        &self.marked_by
    }

    /// Record (or overwrite) `user`'s status for the session. One row per
    /// (session, user), keyed by a deterministic composite id so this is a
    /// single atomic UPSERT — concurrent marks converge on the one row.
    ///
    /// The "session still exists" gate rides in the same transaction as the
    /// mark, the mirror of the cascade in [`crate::db::course_session::delete`]: the
    /// caller's pre-flight read sits four round trips in front of this write,
    /// so a delete landing in that gap used to leave a roll-call row on a
    /// session that was gone — and that row was *unremovable*, its only delete
    /// route 404ing on the vanished session while the attendance report went
    /// on counting it. A deleted session reads NONE, which is falsy, so the
    /// gate is written as an explicit `IS NONE` (see
    /// [`crate::db::exam_result::grade`]). It reads the
    /// session's `teacher` — required on every row, so NONE means "no row" just
    /// as `id` did — because the counter below needs that teacher anyway, and
    /// one read serves both.
    ///
    /// A read is not a claim, though: SurrealDB 3.2.3 conflict-checks write
    /// sets, not read sets, so that gate alone only closes the *sequential*
    /// order (delete committed, then the mark arrives). To make the two really
    /// collide, this transaction also *writes* the session row, unconditionally
    /// — the bump-and-restore of [`crate::db::exam_answer::save`]
    /// on the one column this write already owns, [`LESSON_COUNTED_AT_FIELD`].
    /// Unconditional is the whole point: the credit branch below writes that
    /// column already, but it fires only for the first roll call of a lesson
    /// that has begun, so a future-dated sheet and every re-mark of a counted
    /// one touched nothing at all and committed happily beside
    /// `DELETE /sessions/{id}`. Re-stating the value verbatim would not do
    /// either — an `UPDATE` that leaves the document unchanged is elided and
    /// never enters the write set — hence bump first, then either stamp
    /// (credit branch) or put back exactly what was found, `NONE` included, so
    /// the row is byte-identical and the once-per-lesson rule is untouched.
    ///
    /// Sound to re-send while the store answers "conflict, retry": the gate
    /// reads the record a delete writes, so the two contend by design, and the
    /// UPSERT cannot legitimately answer "already exists" — its composite id is
    /// bijective with the `session_attendance_session_user` unique tuple, so
    /// the index entry can only point at the row the id already names.
    ///
    /// Two badge counters move in this same transaction, both of them read
    /// from the store rather than taken on the caller's word:
    ///
    /// - `lessons_attended_total`, on the person marked, as a *delta* rather
    ///   than an increment, because this is an upsert: a re-mark that changes
    ///   nothing must change nothing, and a teacher's correction must move it
    ///   back down. `$was` is read before the upsert overwrites it (`NONE` when
    ///   the pair has no row yet), so it writes only on a real crossing of the
    ///   attended line, and only for a **student** — attending lessons is a
    ///   student's badge, the same student-only rule enrolling and sitting an
    ///   exam already carry, so a teacher marked present in their own lesson
    ///   moves it in neither direction. The live `role` decides that, not the
    ///   role someone held when the row was written.
    /// - `lessons_held_total`, on the *session's* teacher, exactly once per
    ///   session: the first roll call taken stamps
    ///   [`LESSON_COUNTED_AT_FIELD`] on the lesson and every later mark sees
    ///   the stamp and credits nothing. It counts lessons that actually
    ///   happened — scheduling one and cancelling it earns nothing, which is
    ///   why the credit does not live in `crate::db::course_session::create`. Two
    ///   simultaneous first marks both write the lesson row, so the store's own
    ///   conflict detection (and `transaction_with_retry` behind it) is what
    ///   keeps the stamp from being set twice.
    ///
    ///   "Actually happened" is a clock reading, not a request count: the
    ///   credit waits for the lesson's own `starts_at` to arrive. Roll call is
    ///   deliberately *not* time-gated — a teacher may open the sheet early and
    ///   is never refused — so without this a teacher could schedule two
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
    ///
    /// Both are floored/guarded rather than trusting the column to exist: a row
    /// written before these columns carries none of them.
    //
    // corner-cut: that asymmetry leaves a teacher able to inflate a *student's*
    // attendance with future-dated lessons. Closing it needs the credit stamped
    // on the roll-call row itself and refunded off that stamp — the same column
    // the live-role note on `remove` below wants, and the shape
    // `counted_on_time` already uses for homework. One column closes both;
    // neither is worth it until a real complaint names one.
    pub async fn mark(
        session: &CourseSession,
        user: &UserId,
        status: AttendanceStatus,
        marked_by: &UserId,
        db: &Database,
    ) -> Result<SessionAttendance, AppError> {
        let attended = counts_as_attended(&status);
        let delta: i64 = if attended { 1 } else { -1 };
        let attendance = SessionAttendance {
            id: SessionAttendanceId::composite(session.get_id(), user),
            session: session.get_id().clone(),
            course: session.get_course().clone(),
            user: user.clone(),
            status,
            marked_by: marked_by.clone(),
        };
        let (mut written, mut errors) = transaction_with_retry(
            db,
            &format!(
                "BEGIN TRANSACTION;
                 LET $teacher = (SELECT VALUE teacher FROM ONLY $sess);
                 IF $teacher IS NONE {{ THROW 'session_missing' }};
                 LET $counted = (SELECT VALUE {LESSON_COUNTED_AT_FIELD} FROM ONLY $sess);
                 LET $begun = ((SELECT VALUE starts_at FROM ONLY $sess) <= $stamp);
                 LET $student = ((SELECT VALUE role FROM ONLY $usr) = 'student');
                 LET $was = ((SELECT VALUE status FROM ONLY $id) IN ['present', 'late']);
                 LET $after = (UPSERT $id CONTENT $row RETURN AFTER);
                 IF $student AND $was != $now {{
                     UPDATE $usr SET {LESSONS_ATTENDED_TOTAL_FIELD} =
                         math::max([({LESSONS_ATTENDED_TOTAL_FIELD} ?? 0) + $delta, 0])
                 }};
                 UPDATE $sess SET {LESSON_COUNTED_AT_FIELD} = ({LESSON_COUNTED_AT_FIELD} ?? 0) + 1;
                 IF ($counted IS NONE) AND $begun {{
                     UPDATE $sess SET {LESSON_COUNTED_AT_FIELD} = $stamp;
                     UPDATE $teacher SET {LESSONS_HELD_TOTAL_FIELD} =
                         ({LESSONS_HELD_TOTAL_FIELD} ?? 0) + 1
                 }} ELSE {{
                     UPDATE $sess SET {LESSON_COUNTED_AT_FIELD} = $counted
                 }};
                 RETURN $after;
                 COMMIT TRANSACTION;"
            ),
            &[
                // `$session` is SurrealDB's own protected variable (the auth
                // session): binding that name errors the whole query.
                ("sess".into(), session.get_id().record().into_value()),
                ("id".into(), attendance.id.record().into_value()),
                ("usr".into(), user.record().into_value()),
                ("now".into(), attended.into_value()),
                ("delta".into(), delta.into_value()),
                ("stamp".into(), Timestamp::now().into_value()),
                ("row".into(), attendance.into_value()),
            ],
            &["session_missing"],
        )
        .await?;
        // An aborted transaction errors every slot and only the THROW's own
        // slot names the marker, so a refusal is read by marker while a lost
        // round was already re-sent — never reported as a 500.
        if errors
            .values()
            .any(|error| error.to_string().contains("session_missing"))
        {
            return Err(AppError::NotFound);
        }
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        // The trailing `RETURN` is always the last statement before `COMMIT`,
        // so its slot follows the statement count instead of a hand-kept one.
        let slot = written.num_statements().saturating_sub(2);
        written
            .take::<Vec<SessionAttendance>>(slot)?
            .into_iter()
            .next()
            .ok_or_else(|| AppError::Internal("failed to mark session attendance".into()))
    }

    pub async fn list_for_session(
        session: &CourseSessionId,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<SessionAttendance>, i64), AppError> {
        PagedList::new("session_attendance WHERE session = $s", "ORDER BY id DESC")
            .bind("s", session.record())
            .run(limit, offset, db)
            .await
    }

    /// Every roll-call row ever recorded for `user` — the session half of the
    /// attendance report. Deliberately not filtered by current enrollment:
    /// attendance is a historical record, so unenrolling hides marks (report
    /// semantics) but never absences.
    pub async fn list_for_user(
        user: &UserId,
        db: &Database,
    ) -> Result<Vec<SessionAttendance>, AppError> {
        let mut result = db
            .query("SELECT * FROM session_attendance WHERE user = $usr ORDER BY id DESC")
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<SessionAttendance>>(0)?)
    }

    /// Clearing a row the student was counted for gives the count back, in the
    /// delete's own transaction — the other direction of the delta in
    /// [`SessionAttendance::mark`], so a row that never existed and a row that
    /// was withdrawn leave the same number behind. Only *this* route decrements:
    /// [`crate::db::course_session::delete`]'s cascade sweeps the rows with a `DELETE` of
    /// its own and never comes through here, which is the ruling
    /// `exam_sat_total` already carries — deleting the lesson does not un-attend
    /// it. `lessons_held_total` is never given back either: the lesson was
    /// taken, and clearing one student's row does not un-take it.
    ///
    /// Student-only on the same terms as the mark, and read from the store for
    /// the same reason.
    ///
    /// Sound to re-send: neither statement can answer "already exists".
    //
    // corner-cut: both ends read the *live* role, so a student promoted between
    // being marked present and having that row corrected leaves the counter one
    // high (the credit landed as a student, the refund is refused as staff).
    // Upgrade path is stamping the credit on the roll-call row itself and
    // refunding off that stamp, the way `counted_on_time` works for homework —
    // not worth a column until a promotion mid-term is a real complaint.
    pub async fn remove(
        session: &CourseSessionId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<SessionAttendance>, AppError> {
        let (mut result, mut errors) = transaction_with_retry(
            db,
            &format!(
                "BEGIN TRANSACTION;
                 LET $student = ((SELECT VALUE role FROM ONLY $usr) = 'student');
                 LET $gone = (DELETE session_attendance
                     WHERE session = $s AND user = $usr RETURN BEFORE);
                 IF $student AND array::len($gone) > 0 AND $gone[0].status IN ['present', 'late'] {{
                     UPDATE $usr SET {LESSONS_ATTENDED_TOTAL_FIELD} =
                         math::max([({LESSONS_ATTENDED_TOTAL_FIELD} ?? 0) - 1, 0])
                 }};
                 RETURN $gone;
                 COMMIT TRANSACTION;"
            ),
            &[
                ("s".into(), session.record().into_value()),
                ("usr".into(), user.record().into_value()),
            ],
            &[],
        )
        .await?;
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        // The trailing `RETURN` is always the last statement before `COMMIT`.
        let slot = result.num_statements().saturating_sub(2);
        Ok(result
            .take::<Vec<SessionAttendance>>(slot)?
            .into_iter()
            .next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::DEFAULT_ATTENDANCE_STATUSES;
    use crate::database::init_mem;
    use crate::domain::badge::BadgeStats;
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
        BadgeStats::load(user, db)
            .await
            .unwrap()
            .get_lessons_attended()
    }

    async fn held(user: &UserId, db: &Database) -> i64 {
        BadgeStats::load(user, db).await.unwrap().get_lessons_held()
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
            SessionAttendance::mark(&session, &student, status(value), &teacher, db)
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
            SessionAttendance::mark(&session, &student, status("present"), &teacher, &db)
                .await
                .unwrap();
            assert_eq!(held(&teacher, &db).await, 1, "{key} counted it again");
        }
        assert_eq!(held(&other, &db).await, 0, "credited the wrong teacher");

        // A second lesson is a second count — the stamp is per session.
        let second = a_session(&teacher, &db).await;
        let student = a_student("s4", &db).await;
        SessionAttendance::mark(&second, &student, status("present"), &teacher, &db)
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

        SessionAttendance::mark(&next_week, &student, status("present"), &teacher, &db)
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
            SessionAttendance::mark(&next_week, &student, status(status_value), &teacher, &db)
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

        SessionAttendance::mark(&session, &teacher, status("present"), &teacher, &db)
            .await
            .unwrap();
        assert_eq!(attended(&teacher, &db).await, 0, "staff attended a lesson");
        assert_eq!(held(&teacher, &db).await, 1, "the roll call was taken");

        // And the correction direction is just as closed: nothing was taken, so
        // nothing may be given back.
        SessionAttendance::mark(&session, &teacher, status("absent"), &teacher, &db)
            .await
            .unwrap();
        assert_eq!(attended(&teacher, &db).await, 0);
        SessionAttendance::remove(session.get_id(), &teacher, &db)
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
            SessionAttendance::mark(&session, &student, status("present"), &teacher, &db)
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
        SessionAttendance::mark(&counted, &student, status("present"), &teacher, &db)
            .await
            .unwrap();
        let neutral = a_session(&teacher, &db).await;
        SessionAttendance::mark(&neutral, &student, status("absent"), &teacher, &db)
            .await
            .unwrap();
        assert_eq!(attended(&student, &db).await, 1);

        SessionAttendance::remove(neutral.get_id(), &student, &db)
            .await
            .unwrap()
            .expect("the absent row was there");
        assert_eq!(attended(&student, &db).await, 1, "absent took nothing");
        SessionAttendance::remove(counted.get_id(), &student, &db)
            .await
            .unwrap()
            .expect("the present row was there");
        assert_eq!(attended(&student, &db).await, 0);
        assert!(
            SessionAttendance::remove(counted.get_id(), &student, &db)
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
            SessionAttendance::mark(&session, &student, status("present"), &teacher, &db)
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
            SessionAttendance::mark(session, &student, status("absent"), &teacher, &db)
                .await
                .unwrap();
            SessionAttendance::remove(session.get_id(), &student, &db)
                .await
                .unwrap();
            assert!(attended(&student, &db).await >= 0);
        }
        assert_eq!(attended(&student, &db).await, 0, "floored, never negative");
    }

    /// Deleting the lesson sweeps its roll-call rows with a `DELETE` of its own
    /// — it never reaches [`SessionAttendance::remove`] — so the count stands,
    /// the ruling every other counter here carries.
    #[tokio::test]
    async fn deleting_the_session_leaves_the_counters_alone() {
        let db = init_mem().await.unwrap();
        let teacher = a_teacher("t", &db).await;
        let student = a_student("s", &db).await;
        let session = a_session(&teacher, &db).await;
        SessionAttendance::mark(&session, &student, status("present"), &teacher, &db)
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
            SessionAttendance::mark(&ghost, &student, status("present"), &teacher, &db).await,
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
                SessionAttendance::mark(&session, &first, status("present"), &teacher, &db)
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
                    SessionAttendance::mark(&ghost, &student, status("present"), &teacher, &db)
                        .await
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
                orphans += SessionAttendance::list_for_session(&id, None, 0, &db)
                    .await
                    .unwrap()
                    .0
                    .len();
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
