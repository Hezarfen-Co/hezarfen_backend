//! The badge families this pass added, driven over the routes that earn them:
//! `marks_given`, `high_mark`, `pool_approved` / `pool_published`,
//! `lessons_held` and `lessons_attended`.
//!
//! Each counter is unit-tested inside its own domain module; what lives here is
//! the whole path — a real request moves the counter, the write site syncs the
//! award, and the badge comes back on a profile a client can actually read.
//! Two rules the suite exists to defend:
//!
//! * a badge is **permanent** — a counter falling back below its threshold
//!   never takes the award away, and a later sync never moves `earned_at`;
//! * a counter moves only for the person who did the work, once per piece of
//!   work: a regrade, a re-mark, and a self-approval all move nothing.
//!
//! Every test that reads a counter drives a real registered user through the
//! API, never a fabricated id: `UPDATE` on a record that does not exist writes
//! nothing at all, so a counter assertion against a made-up key passes as
//! `0 == 0` whatever the implementation does.

mod common;

use axum::Router;
use axum::http::StatusCode;
use common::{
    app_and_db, create_course, create_exam, create_exam_with, create_session, enroll, id_of, login,
    login_as, me_id, send, set_role,
};
use hezarfen_backend::constant::{
    BADGES, HIGH_MARK_MIN, MIN_COUNTED_POMODORO_MS, STUDY_STREAK_LAST_DAY_FIELD,
};
use hezarfen_backend::database::Database;
use hezarfen_backend::domain::timestamp::Timestamp;
use serde_json::{Value, json};

/// One profile body as `cookie` may see it (asserts the `200`).
async fn profile_of(app: &Router, cookie: &str, id: &str) -> Value {
    let res = send(
        app,
        "GET",
        &format!("/users/{id}/profile"),
        Some(cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    res.body
}

/// The caller's own profile body (asserts the `200`).
async fn my_profile(app: &Router, cookie: &str) -> Value {
    let res = send(app, "GET", "/users/me/profile", Some(cookie), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    res.body
}

/// One `stats` counter off a profile body. Panics rather than defaulting: a
/// missing key is a renamed counter, not a zero.
fn stat(profile: &Value, key: &str) -> i64 {
    profile["stats"][key]
        .as_i64()
        .unwrap_or_else(|| panic!("stats.{key} is not a number: {profile}"))
}

/// The badge ids on a profile, in the order served.
fn badge_ids(profile: &Value) -> Vec<&str> {
    profile["badges"]
        .as_array()
        .expect("badges array")
        .iter()
        .map(|badge| badge["id"].as_str().expect("badge id"))
        .collect()
}

/// When `id` was earned, off a profile body — `None` if the shelf lacks it.
fn stamp_of(profile: &Value, id: &str) -> Option<i64> {
    profile["badges"]
        .as_array()
        .expect("badges array")
        .iter()
        .find(|badge| badge["id"] == id)
        .map(|badge| badge["earned_at"].as_i64().expect("earned_at millis"))
}

// --- grading ---------------------------------------------------------------

/// A teacher, one enrolled student, and `n` exams in one course.
///
/// `marks_given` counts one mark per *sitting*, so ten first grades need ten
/// exams or ten students — and an exam is a single cheap request while a
/// student costs two argon2 hashes to register and log in.
struct Marking {
    app: Router,
    teacher: String,
    student: String,
    student_id: String,
    exams: Vec<String>,
}

async fn marking(exams: usize) -> Marking {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teach", "teacher").await;
    let student = login(&app, "stu").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "Algebra").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let mut ids = Vec::with_capacity(exams);
    for n in 0..exams {
        ids.push(create_exam(&app, &teacher, &course, &format!("Quiz {n}"), "quiz").await);
    }
    Marking {
        app,
        teacher,
        student,
        student_id,
        exams: ids,
    }
}

/// Record `mark` for `student` on `exam` as `teacher` (asserts the `200`).
async fn grade(app: &Router, teacher: &str, exam: &str, student: &str, mark: i64) {
    let res = send(
        app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(teacher),
        Some(json!({ "user_id": student, "mark": mark })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "grade {mark}: {}", res.body);
}

/// The grader's side, end to end: ten marks recorded through the exam route
/// put `marks_given_10` on the teacher's profile — and on the *public* one a
/// student reads, which is the only surface a client has.
#[tokio::test]
async fn ten_marks_put_the_graders_badge_on_a_profile() {
    let m = marking(10).await;
    let teacher_id = me_id(&m.app, &m.teacher).await;

    for exam in &m.exams {
        grade(&m.app, &m.teacher, exam, &m.student_id, 70).await;
    }

    let mine = my_profile(&m.app, &m.teacher).await;
    assert_eq!(stat(&mine, "marks_given_total"), 10, "{mine}");
    assert_eq!(badge_ids(&mine), ["marks_given_10"], "{mine}");
    // Badges are public: the student whose work was marked sees the same shelf.
    let seen = profile_of(&m.app, &m.student, &teacher_id).await;
    assert_eq!(badge_ids(&seen), ["marks_given_10"], "{seen}");
    // Grading is the teacher's work, not the student's — 70 is under the line,
    // so the student's own counters never moved.
    let theirs = my_profile(&m.app, &m.student).await;
    assert_eq!(stat(&theirs, "high_mark_total"), 0, "{theirs}");
    assert_eq!(stat(&theirs, "marks_given_total"), 0, "{theirs}");
}

/// The *grader's* mark is credited at the first grade of a sitting, so
/// correcting one is free: the counter stays, and — the permanence rule at the
/// stamp — the badge it already earned keeps the `earned_at` it was first
/// given. The *student's* high-mark counter is the one thing a regrade does
/// move, and has to: it counts stored marks at or above the line, not first
/// gradings, and the delete that refunds it reads the stored mark. Pinned the
/// other way round, the pair drifted — the credit was decided by a mark the
/// refund could no longer find.
#[tokio::test]
async fn a_regrade_leaves_the_grader_alone_but_walks_the_high_mark_with_it() {
    let m = marking(10).await;
    for exam in &m.exams {
        grade(&m.app, &m.teacher, exam, &m.student_id, 70).await;
    }
    let earned = stamp_of(&my_profile(&m.app, &m.teacher).await, "marks_given_10")
        .expect("the tenth mark earned the badge");

    // Any overwrite from here on would be strictly later than this instant.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    assert!(
        earned < Timestamp::now().as_millis(),
        "the clock never moved — the stamp assertion below is vacuous"
    );

    grade(&m.app, &m.teacher, &m.exams[0], &m.student_id, 100).await;

    let after = my_profile(&m.app, &m.teacher).await;
    assert_eq!(
        stat(&after, "marks_given_total"),
        10,
        "a regrade is not an eleventh mark: {after}"
    );
    assert_eq!(
        stamp_of(&after, "marks_given_10"),
        Some(earned),
        "a re-sync moved a permanent stamp: {after}"
    );
    // The sitting now *stores* a 100, so the student holds one high mark —
    // exactly what deleting the result would refund, and nothing else.
    let theirs = my_profile(&m.app, &m.student).await;
    assert_eq!(
        stat(&theirs, "high_mark_total"),
        1,
        "the regrade crossed the line and the counter stayed behind: {theirs}"
    );
    assert_eq!(badge_ids(&theirs), ["high_mark_1"], "{theirs}");
}

/// The high-mark cut, from both sides of the line in one student's history:
/// the mark at `HIGH_MARK_MIN` earns `high_mark_1`, and the one below it earns
/// nothing — the counter that stays put is what proves the cut is a `>=` and
/// not "every mark counts".
#[tokio::test]
async fn a_mark_at_the_cut_earns_the_badge_and_one_below_it_earns_nothing() {
    let m = marking(2).await;

    grade(
        &m.app,
        &m.teacher,
        &m.exams[0],
        &m.student_id,
        HIGH_MARK_MIN,
    )
    .await;
    let high = my_profile(&m.app, &m.student).await;
    assert_eq!(stat(&high, "high_mark_total"), 1, "{high}");
    assert_eq!(badge_ids(&high), ["high_mark_1"], "{high}");

    grade(
        &m.app,
        &m.teacher,
        &m.exams[1],
        &m.student_id,
        HIGH_MARK_MIN - 1,
    )
    .await;
    let after = my_profile(&m.app, &m.student).await;
    assert_eq!(
        stat(&after, "high_mark_total"),
        1,
        "a mark under the cut was counted as a high one: {after}"
    );
    // The grader's own counter moved twice, which is what proves the second
    // grade landed at all rather than being refused.
    assert_eq!(
        stat(&my_profile(&m.app, &m.teacher).await, "marks_given_total"),
        2
    );
}

// --- the question pool -----------------------------------------------------

/// Ask a pool question as `cookie` (asserts the `201`); returns its id.
async fn ask(app: &Router, cookie: &str, title: &str) -> String {
    let res = send(
        app,
        "POST",
        "/questions",
        Some(cookie),
        Some(json!({ "title": title, "body": "Nasıl çözülür?" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    id_of(&res.body)
}

/// Approve `question` as `cookie` (asserts the `200`); returns the body.
async fn approve(app: &Router, cookie: &str, question: &str) -> Value {
    let res = send(
        app,
        "POST",
        &format!("/questions/{question}/approve"),
        Some(cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    res.body
}

/// One approval, two people credited: the approver's `pool_approved` and the
/// asker's `pool_published`, which is a badge on its first one. And the farm
/// this closes — approving a question you asked yourself moves *neither*
/// counter, while the approval itself is untouched: same `200`, same
/// `approved` status, same stamp.
#[tokio::test]
async fn a_two_party_approval_credits_both_and_a_self_approval_credits_neither() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teach", "teacher").await;
    let asker = login(&app, "stu").await;
    // Asking is student-only, so the self-approver asks first and is promoted
    // afterwards — the one route onto a self-approval the API actually has.
    let selfie = login(&app, "selfie").await;

    let theirs = ask(&app, &asker, "Integral").await;
    let own = ask(&app, &selfie, "Türev").await;

    approve(&app, &teacher, &theirs).await;
    let judge = my_profile(&app, &teacher).await;
    assert_eq!(stat(&judge, "pool_approved_total"), 1, "{judge}");
    // Five approvals earn the first badge, so this one is a counter move only.
    assert_eq!(badge_ids(&judge), Vec::<&str>::new(), "{judge}");
    let author = my_profile(&app, &asker).await;
    assert_eq!(stat(&author, "pool_published_total"), 1, "{author}");
    assert_eq!(badge_ids(&author), ["pool_published_1"], "{author}");

    set_role(&db, "selfie", "teacher").await;
    let approved = approve(&app, &selfie, &own).await;
    assert_eq!(approved["status"], "approved", "{approved}");
    assert_eq!(
        approved["approved_by"]["id"],
        Value::from(me_id(&app, &selfie).await),
        "{approved}"
    );

    let mine = my_profile(&app, &selfie).await;
    assert_eq!(
        (
            stat(&mine, "pool_approved_total"),
            stat(&mine, "pool_published_total")
        ),
        (0, 0),
        "judging your own question credited you: {mine}"
    );
    assert_eq!(badge_ids(&mine), Vec::<&str>::new(), "{mine}");
}

// --- roll call -------------------------------------------------------------

/// Mark `user` `status` for `session` as `cookie` (asserts the `200`).
async fn roll_call(app: &Router, cookie: &str, session: &str, user: &str, status: &str) {
    let res = send(
        app,
        "POST",
        &format!("/sessions/{session}/attendance"),
        Some(cookie),
        Some(json!({ "user_id": user, "status": status })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "mark {status}: {}", res.body);
}

fn soon() -> i64 {
    Timestamp::now().as_millis() + 3_600_000
}

/// Bring a scheduled lesson's start time back into the past, so a roll call
/// taken now is taken *during* the lesson rather than a week ahead of it.
///
/// This manipulates **stored state, never the clock**, the same trick
/// `age_one_day` uses below and for the same reason: no route can do it. Both
/// `POST /courses/{id}/sessions` and the session PATCH refuse a start time in
/// the past — which is what makes the counter's own gate worth having, since a
/// client can only ever schedule *forward* into an unheld lesson.
async fn ring_the_bell(db: &Database, session: &str) {
    sqlx::query("UPDATE course_session SET starts_at = 1 WHERE id = $1")
        .bind(hezarfen_backend::domain::course_session::CourseSessionId::from_key(session))
        .execute(db)
        .await
        .unwrap();
}

/// A lesson is credited to its teacher when the roll call is *taken*, once:
/// the first student marked stamps it, and the second and third credit nothing
/// further however many are on the roster.
///
/// And attending is a student's badge alone — the manager marking the lesson's
/// own teacher present (the one staff roll-call row the API allows) moves no
/// `lessons_attended` at all.
#[tokio::test]
async fn the_first_roll_call_credits_the_lesson_once_and_only_students_attend() {
    let (app, db) = app_and_db().await;
    let boss = login_as(&app, &db, "mudur", "manager").await;
    let teacher = login_as(&app, &db, "teach", "teacher").await;
    let teacher_id = me_id(&app, &teacher).await;
    let course = create_course(&app, &teacher, "Algebra").await;

    let mut students = Vec::new();
    for name in ["ali", "veli", "ayse"] {
        let cookie = login(&app, name).await;
        let id = me_id(&app, &cookie).await;
        enroll(&app, &teacher, &course, &id).await;
        students.push((cookie, id));
    }
    let session = create_session(&app, &teacher, &course, soon()).await;

    // The lesson is still an hour away. Opening the sheet early is allowed and
    // the mark stands, but it holds nothing — ungated, two hundred lessons
    // scheduled for next week would be two hundred held this afternoon.
    roll_call(&app, &teacher, &session, &students[0].1, "present").await;
    let early = my_profile(&app, &teacher).await;
    assert_eq!(
        stat(&early, "lessons_held_total"),
        0,
        "next week's lesson was held today: {early}"
    );

    ring_the_bell(&db, &session).await;
    roll_call(&app, &teacher, &session, &students[0].1, "present").await;
    let held = my_profile(&app, &teacher).await;
    assert_eq!(
        stat(&held, "lessons_held_total"),
        1,
        "taking the roll call during the lesson is what holds it: {held}"
    );

    roll_call(&app, &teacher, &session, &students[1].1, "present").await;
    roll_call(&app, &teacher, &session, &students[2].1, "late").await;
    let after = my_profile(&app, &teacher).await;
    assert_eq!(
        stat(&after, "lessons_held_total"),
        1,
        "the third student marked held the same lesson again: {after}"
    );

    for (cookie, _) in &students {
        let theirs = my_profile(&app, cookie).await;
        // `late` is attendance too — the same rule `GET /attendance/me` uses.
        assert_eq!(stat(&theirs, "lessons_attended_total"), 1, "{theirs}");
    }

    // Management records the teacher's own presence: a roll-call row like any
    // other, and no `lessons_attended` for a person who is not a student.
    roll_call(&app, &boss, &session, &teacher_id, "present").await;
    let staff = my_profile(&app, &teacher).await;
    assert_eq!(
        stat(&staff, "lessons_attended_total"),
        0,
        "the teacher attended their own lesson: {staff}"
    );
    assert_eq!(stat(&staff, "lessons_held_total"), 1, "{staff}");
}

/// **The permanence rule.** A correction takes the counter back down — that is
/// what a roll call being editable means — and the badge it already earned
/// stays on the shelf with the stamp it was first given. Badges are auto-earned
/// and never revoked; nothing in the system removes an award row, and this is
/// the test that would notice if something started to.
///
/// The tenth lesson is what earns `lessons_attended_10`, and it is that very
/// tenth mark the correction withdraws — so the counter ends *below* the
/// threshold the badge was earned at, which is the only state that tells a
/// permanent award apart from one recomputed on every read.
#[tokio::test]
async fn a_correction_lowers_the_counter_but_never_takes_the_badge_back() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teach", "teacher").await;
    let student = login(&app, "stu").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "Algebra").await;
    enroll(&app, &teacher, &course, &student_id).await;

    let mut sessions = Vec::new();
    for lesson in 0..10 {
        let starts_at = soon() + lesson * 3_600_000;
        let session = create_session(&app, &teacher, &course, starts_at).await;
        ring_the_bell(&db, &session).await;
        roll_call(&app, &teacher, &session, &student_id, "present").await;
        sessions.push(session);
    }

    let earned_it = my_profile(&app, &student).await;
    assert_eq!(
        stat(&earned_it, "lessons_attended_total"),
        10,
        "{earned_it}"
    );
    let earned = stamp_of(&earned_it, "lessons_attended_10").expect("the tenth lesson earns it");

    // The teacher corrects the last roll call: the student was not there.
    roll_call(
        &app,
        &teacher,
        sessions.last().unwrap(),
        &student_id,
        "absent",
    )
    .await;

    let after = my_profile(&app, &student).await;
    assert_eq!(
        stat(&after, "lessons_attended_total"),
        9,
        "a correction must give the count back: {after}"
    );
    assert_eq!(
        stamp_of(&after, "lessons_attended_10"),
        Some(earned),
        "a badge was taken back when its counter fell: {after}"
    );
    // Correcting one student's row does not un-hold the lesson either.
    assert_eq!(
        stat(&my_profile(&app, &teacher).await, "lessons_held_total"),
        10
    );
}

// --- the study streak ------------------------------------------------------

/// Start and finish one focus stint as `cookie` that *counts* (asserts both
/// statuses, and the verdict the finish hands back).
///
/// The running stint is aged past `MIN_COUNTED_POMODORO_MS` in between — stored
/// state again, never the clock, and the same reason `age_one_day` exists: a
/// stint that instant is exactly the farm the rule refuses, and no route can
/// make one older. The domain suite pins the rule itself; what this buys is the
/// streak arriving on a real profile.
async fn stint(app: &Router, db: &Database, cookie: &str, user: &str) {
    let started = send(app, "POST", "/pomodoro/start", Some(cookie), None).await;
    assert_eq!(started.status, StatusCode::CREATED, "{}", started.body);
    sqlx::query(
        "UPDATE pomodoro_session SET started_at = started_at - $2
         WHERE app_user = $1 AND finished_at IS NULL",
    )
    .bind(hezarfen_backend::domain::user::UserId::from_key(user))
    .bind(MIN_COUNTED_POMODORO_MS)
    .execute(db)
    .await
    .unwrap();
    let finished = send(app, "POST", "/pomodoro/finish", Some(cookie), None).await;
    assert_eq!(finished.status, StatusCode::OK, "{}", finished.body);
    assert_eq!(
        finished.body["counted"],
        Value::from(true),
        "{}",
        finished.body
    );
}

/// Wind `user`'s streak bookkeeping back one day, so the *next* finish lands on
/// the next calendar day.
///
/// This manipulates **stored state, never the clock**: `Timestamp::now` stays
/// the one clock read in the process and nothing sleeps. It is the same trick
/// the domain tests use (`age_by_days` in `src/domain/pomodoro.rs`), reached
/// here through the harness's own database handle because no route can move a
/// day boundary — the streak's day arithmetic itself is the domain suite's to
/// pin, and what this buys is the *badge* arriving on a real profile.
async fn age_one_day(db: &Database, user: &str) {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE app_user SET {STUDY_STREAK_LAST_DAY_FIELD} = \
         {STUDY_STREAK_LAST_DAY_FIELD} - 1 WHERE id = $1"
    )))
    .bind(hezarfen_backend::domain::user::UserId::from_key(user))
    .execute(db)
    .await
    .unwrap();
}

/// The streak's whole path, in the one shape a client sees it: a student
/// finishes a stint and their profile says `study_streak_total: 1`.
///
/// That single number is worth more than it looks — it is written by the
/// pomodoro transaction into `study_streak_longest`, projected by
/// `BadgeStats::load`, projected *again* by the parallel hand-kept reader in
/// `src/domain/profile.rs` (the two carry doc comments saying they move
/// together, and nothing but a test notices when they stop), and finally
/// served under a key deliberately spelled `study_streak_total` although the
/// counter is a longest and not a sum. Break any one of those and this reads
/// `0` or panics on a missing key.
///
/// Three consecutive days then earn `study_streak_3` off a real profile read.
#[tokio::test]
async fn a_finished_stint_reaches_the_profile_and_three_days_earn_the_badge() {
    let (app, db) = app_and_db().await;
    let student = login(&app, "stu").await;
    let student_id = me_id(&app, &student).await;

    stint(&app, &db, &student, &student_id).await;

    let first = profile_of(&app, &student, &student_id).await;
    assert_eq!(
        stat(&first, "study_streak_total"),
        1,
        "one finished stint is a one-day run: {first}"
    );
    // The stint landed at all — so a zero above would be the streak's own
    // projection, not a pomodoro route that never ran.
    assert_eq!(stat(&first, "pomodoro_finished_total"), 1, "{first}");
    assert_eq!(badge_ids(&first), Vec::<&str>::new(), "three days away");

    for _ in 0..2 {
        age_one_day(&db, &student_id).await;
        stint(&app, &db, &student, &student_id).await;
    }

    let after = profile_of(&app, &student, &student_id).await;
    assert_eq!(
        stat(&after, "study_streak_total"),
        3,
        "three consecutive days are a run of three: {after}"
    );
    // Ten finished stints earn the pomodoro badge, so three days earn exactly
    // one thing.
    assert_eq!(badge_ids(&after), ["study_streak_3"], "{after}");
}

// --- the read side ---------------------------------------------------------

/// A profile is a read endpoint. The write site behind a badge already ran
/// `badge::sync`, so by the time anyone reads the profile there is nothing
/// owed — and the read must therefore touch the award table exactly zero
/// times, however many badges the shelf carries.
///
/// The probe is a `DEFINE EVENT` on the award table itself: it fires inside any
/// write to it — creates and updates alike, checked with a hand-run `UPDATE`
/// while this was written — so a write this read should not make cannot hide
/// behind an unchanged-looking response.
///
/// What this bites on, precisely: a read path that writes an award row at all.
/// Two things stop it today and either one alone is enough — `badges_of` skips
/// the sync while the shelf is complete, and `sync`'s own
/// `WHERE earned_at = NONE` makes a re-write a no-op — so dropping *only* the
/// second leaves this green. The frozen stamp is pinned by
/// `a_regrade_moves_neither_the_counter_nor_the_stamp` instead.
#[tokio::test]
async fn a_profile_read_owes_nothing_the_write_site_already_paid() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teach", "teacher").await;
    let asker = login(&app, "stu").await;
    let asker_id = me_id(&app, &asker).await;
    let bystander = login(&app, "nobody").await;

    // The award table watched through its rows: the read must not add, remove
    // or alter one. (The old engine counted writes with a `DEFINE EVENT`;
    // Postgres has no triggers to hang that on, and the write the guard
    // bites on — an insert, or a re-stamp of `earned_at` — always changes
    // what the rows hold.)
    let awards = || async {
        let rows: Vec<(uuid::Uuid, String, Option<i64>)> =
            sqlx::query_as("SELECT app_user, badge, earned_at FROM badge_award ORDER BY app_user, badge")
                .fetch_all(&db)
                .await
                .unwrap();
        rows
    };
    assert_eq!(
        awards().await.len(),
        0,
        "nothing has been awarded yet"
    );

    // One approval, and `pool_published_1` is earned — by the route, not by
    // any later read.
    let question = ask(&app, &asker, "Integral").await;
    approve(&app, &teacher, &question).await;
    let written = awards().await;
    assert!(
        !written.is_empty(),
        "the approval never wrote the asker's award"
    );

    let mine = my_profile(&app, &asker).await;
    assert_eq!(badge_ids(&mine), ["pool_published_1"], "{mine}");
    let after_read = awards().await;
    assert_eq!(
        after_read, written,
        "a profile read wrote to the award table"
    );

    // Same again from another reader, and from an account that has earned
    // nothing at all — the shelf is empty, so there is not even a statement to
    // send.
    profile_of(&app, &bystander, &asker_id).await;
    let empty = my_profile(&app, &bystander).await;
    assert_eq!(badge_ids(&empty), Vec::<&str>::new(), "{empty}");
    let after_more = awards().await;
    assert_eq!(
        after_more, written,
        "reading a profile that owes nothing still wrote"
    );
}

// --- exams sat -------------------------------------------------------------

/// Sit `exam` as `cookie` and submit it again, `times` over: the loop a student
/// runs with nobody else in it on an `open` exam with `max_attempts: 0`.
async fn sit_and_finish(app: &Router, cookie: &str, exam: &str, times: usize) {
    for round in 0..times {
        let started = send(
            app,
            "POST",
            &format!("/exams/{exam}/attempt"),
            Some(cookie),
            None,
        )
        .await;
        assert_eq!(
            started.status,
            StatusCode::CREATED,
            "round {round} did not start a new sitting: {}",
            started.body
        );
        let finished = send(
            app,
            "POST",
            &format!("/exams/{exam}/attempt/finish"),
            Some(cookie),
            None,
        )
        .await;
        assert_eq!(finished.status, StatusCode::OK, "{}", finished.body);
    }
}

/// The farm: `exam_sat_total` counts **exams sat**, not sittings. An `open`
/// exam with unlimited attempts is a start/finish loop that needs no teacher,
/// so counting retakes minted `exam_sat_10`/`exam_sat_25` — permanent awards —
/// off a single exam in a couple of dozen requests. Ten sittings of one exam
/// must be worth exactly one, and a *second* exam must still be worth another,
/// or the fix would have frozen the counter instead of narrowing it.
#[tokio::test]
async fn a_retake_loop_on_one_exam_counts_one_exam_sat() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "hoca", "teacher").await;
    let student = login(&app, "ogrenci").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "Biology").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let unlimited = json!({ "title": "Cells", "kind": "quiz", "mode": "open", "max_attempts": 0 });
    let res = create_exam_with(&app, &teacher, &course, unlimited.clone()).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let exam = id_of(&res.body);

    sit_and_finish(&app, &student, &exam, 10).await;

    let mine = my_profile(&app, &student).await;
    assert_eq!(
        stat(&mine, "exam_sat_total"),
        1,
        "ten sittings of one exam are one exam sat: {mine}"
    );
    assert_eq!(
        badge_ids(&mine),
        ["exam_sat_1"],
        "the ladder was farmed off one exam: {mine}"
    );

    // A different exam is a different fact about the student, so it still
    // counts — and its retakes still do not.
    let res = create_exam_with(
        &app,
        &teacher,
        &course,
        json!({ "title": "Genes", "kind": "quiz", "mode": "open", "max_attempts": 0 }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    sit_and_finish(&app, &student, &id_of(&res.body), 3).await;

    let mine = my_profile(&app, &student).await;
    assert_eq!(
        stat(&mine, "exam_sat_total"),
        2,
        "a second exam is a second exam sat: {mine}"
    );
}

/// The catalog `GET /limits` publishes is the whole of `BADGES` — a family
/// added to the constant and forgotten at the wire is a badge no client can
/// ever render — and it carries the high-mark cut, which no id spells out.
#[tokio::test]
async fn the_limits_badge_block_serves_the_whole_catalog_and_the_high_mark_cut() {
    let (app, _db) = app_and_db().await;

    let res = send(&app, "GET", "/limits", None, None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let served: Vec<&str> = res.body["badges"]["catalog"]
        .as_array()
        .unwrap_or_else(|| panic!("badges.catalog array: {}", res.body))
        .iter()
        .map(|badge| badge["id"].as_str().expect("badge id"))
        .collect();
    let known: Vec<&str> = BADGES.iter().map(|(id, _, _)| *id).collect();
    assert_eq!(served, known, "the catalog and the wire disagree");

    assert_eq!(
        res.body["badges"]["high_mark_min"],
        Value::from(HIGH_MARK_MIN),
        "the high-mark cut is not published: {}",
        res.body
    );
}
