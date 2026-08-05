//! A child of a course must not survive that course's deletion.
//!
//! `Course::delete` cascades with `DELETE <child> WHERE course = $course`, and
//! that statement's write set is a *snapshot* of the children that existed when
//! it ran. SurrealDB 3.2.3 conflict-checks write sets, not read sets, so a
//! create whose only tie to the course was *reading* it committed happily
//! alongside the sweep — and the row it left is unreachable for good, because
//! every route to it goes through the course:
//!
//! - an orphan `exam` 500s through `course_of` ("exam references a missing
//!   course") on `GET`/`PATCH`/`DELETE /exams/{id}` while `GET /exams` still
//!   lists it to every manager+ — undeletable,
//! - an orphan `course_session` 404s forever through `session_with_course`,
//! - an orphan `subject` 404s through `subject_with_course`, but
//!   `subject_must_exist` still accepts its id, so a bank template can be
//!   tagged with a subject nobody can reach.
//!
//! All three creates now go through `cap::touch_and_create`, which *moves* the
//! course's roster counter (bumped, gated, put back by captured value) inside
//! the create's own transaction: the write lands on the very record the delete
//! removes, so the store refuses one of the two. `SET x = x` would not do —
//! SurrealDB elides an `UPDATE` that leaves the document unchanged, and an
//! elided write sits in no write set at all.
//!
//! Two halves, and they are not interchangeable. **This file is the sequential
//! one**: it runs anywhere, on the in-memory engine, and pins the existence
//! contract (a create against a course that is already gone must be a 404, not
//! an orphan) plus the restore (the borrowed counter comes back exactly as it
//! was, or the course is undeletable forever — a worse bug than the one being
//! fixed).
//!
//! The **race** half drives the real interleaving and lives in-crate, one pin
//! per child beside the create it pins —
//! `domain::exam::tests::an_exam_never_outlives_its_course`,
//! `domain::course_session::tests::a_session_never_outlives_its_course`,
//! `domain::subject::tests::a_subject_never_outlives_its_course` — over the
//! shared harness `domain::course::assert_no_child_outlives_a_course_delete`.
//! It is `#[ignore]`d and needs a real server, because the subject there *is*
//! the store's conflict detection, which the embedded engine does not have: it
//! commits both sides and answers `Ok` to each, so a race test on `memory`
//! passes over the hole (same reason `domain::class_course`'s twin is ignored).
//! Only in-crate tests can reach `database::init_test_server` and the
//! `RACE_LOCK` that serializes them, and a hand-copied bootstrap here would
//! drift out of the real one silently.

use hezarfen_backend::database::{self, Database};
use hezarfen_backend::domain::course::{
    Course, CourseDescription, CourseId, CourseKind, CourseTitle,
};
use hezarfen_backend::domain::course_session::{CourseSession, SessionTopic};
use hezarfen_backend::domain::exam::{
    Exam, ExamAttemptLimit, ExamDescription, ExamKind, ExamSchedule, ExamTitle,
};
use hezarfen_backend::domain::settings::Settings;
use hezarfen_backend::domain::subject::{Subject, SubjectDescription, SubjectName};
use hezarfen_backend::domain::timestamp::Timestamp;
use hezarfen_backend::domain::user::UserId;
use hezarfen_backend::error::AppError;
use tokio::task::JoinHandle;

fn teacher() -> UserId {
    UserId::from_key("01TESTTEACHERAAAAAAAAAAAAA")
}

async fn a_course(db: &Database) -> CourseId {
    Course::create(
        &teacher(),
        CourseTitle::try_new("Fizik").unwrap(),
        CourseDescription::try_new("").unwrap(),
        CourseKind::course(),
        None,
        None,
        db,
    )
    .await
    .unwrap()
    .get_id()
    .clone()
}

async fn drop_course(course: &CourseId, db: &Database) -> Result<bool, AppError> {
    Course::read(course, db)
        .await
        .unwrap()
        .expect("the course is there")
        .delete(db)
        .await
}

// --- the three creates, each as one spawnable unit -------------------------
// Fn pointers rather than a generic closure: the three take different argument
// types and the race harness only ever needs "start it, tell me if it 500s".

fn make_exam(course: CourseId, db: Database) -> JoinHandle<Result<(), AppError>> {
    tokio::spawn(async move {
        let kinds = Settings::defaults().get_exam_kinds().to_vec();
        Exam::create(
            &teacher(),
            &course,
            ExamTitle::try_new("quiz").unwrap(),
            ExamDescription::try_new("").unwrap(),
            ExamKind::try_new("quiz", &kinds).unwrap(),
            ExamSchedule::try_new(None, None, None, None).unwrap(),
            ExamAttemptLimit::try_new(1).unwrap(),
            true,
            false,
            false,
            &db,
        )
        .await
        .map(|_| ())
    })
}

fn make_session(course: CourseId, db: Database) -> JoinHandle<Result<(), AppError>> {
    tokio::spawn(async move {
        CourseSession::create(
            &course,
            &teacher(),
            SessionTopic::try_new("limits").unwrap(),
            Timestamp::from_millis(1),
            None,
            &db,
        )
        .await
        .map(|_| ())
    })
}

fn make_subject(course: CourseId, db: Database) -> JoinHandle<Result<(), AppError>> {
    tokio::spawn(async move {
        Subject::create(
            &course,
            SubjectName::try_new("Limits").unwrap(),
            SubjectDescription::try_new("").unwrap(),
            &db,
        )
        .await
        .map(|_| ())
    })
}

/// How many rows of `table` name `course`. Stored state, never a return value:
/// the whole point is what the store kept.
async fn children(table: &str, course: &CourseId, db: &Database) -> usize {
    let mut result = db
        .query(format!(
            "SELECT VALUE id FROM {table} WHERE course = $course"
        ))
        .bind(("course", course.record()))
        .await
        .unwrap()
        .check()
        .unwrap();
    result
        .take::<Vec<surrealdb::types::RecordId>>(0)
        .unwrap()
        .len()
}

// --- the sequential half: the existence contract ---------------------------

/// A create against a course that is *already* gone must refuse, having written
/// nothing. This is the half of the fix a race cannot show: with the bare
/// `db.create` these creates shipped with, every one of them happily wrote a
/// row naming a course that does not exist.
async fn a_create_against_a_deleted_course_refuses(
    table: &str,
    make: fn(CourseId, Database) -> JoinHandle<Result<(), AppError>>,
) {
    let db = database::init_mem().await.unwrap();
    let course = a_course(&db).await;
    assert!(drop_course(&course, &db).await.unwrap(), "the course goes");

    let answer = make(course.clone(), db.clone()).await.unwrap();
    assert!(
        matches!(answer, Err(AppError::NotFound)),
        "{table}: a create under a deleted course must be a 404, not {answer:?}"
    );
    assert_eq!(
        children(table, &course, &db).await,
        0,
        "{table}: a refused create left a row naming a course that is gone"
    );
}

#[tokio::test]
async fn an_exam_under_a_deleted_course_is_refused() {
    a_create_against_a_deleted_course_refuses("exam", make_exam).await;
}

#[tokio::test]
async fn a_session_under_a_deleted_course_is_refused() {
    a_create_against_a_deleted_course_refuses("course_session", make_session).await;
}

#[tokio::test]
async fn a_subject_under_a_deleted_course_is_refused() {
    a_create_against_a_deleted_course_refuses("subject", make_subject).await;
}

/// The counter the three creates borrow is *given back* — absent stays absent.
/// A bump left behind is not a smaller bug than the orphan it prevents: the
/// course's delete guard is `WHERE (enrollment_count ?? 0) = 0`, so a course
/// that once had an exam created under it would be undeletable forever, and the
/// boot backfill keys on the column being `NONE`.
#[tokio::test]
async fn the_creates_give_the_courses_roster_counter_back_untouched() {
    let db = database::init_mem().await.unwrap();
    let course = a_course(&db).await;
    let absent = "SELECT VALUE id FROM course WHERE enrollment_count = NONE";

    for make in [make_exam, make_session, make_subject] {
        make(course.clone(), db.clone()).await.unwrap().unwrap();
    }
    let mut result = db.query(absent).await.unwrap().check().unwrap();
    assert_eq!(
        result
            .take::<Vec<surrealdb::types::RecordId>>(0)
            .unwrap()
            .len(),
        1,
        "the borrowed counter must be restored to absent, not to 0"
    );
    // The proof that matters to a user: the course is still deletable.
    assert!(
        drop_course(&course, &db).await.unwrap(),
        "a course whose children moved its roster counter can never be deleted"
    );
}
