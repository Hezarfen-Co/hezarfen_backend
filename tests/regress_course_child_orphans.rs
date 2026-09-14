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
//!   `must_exist` still accepts its id, so a bank template can be
//!   tagged with a subject nobody can reach.
//!
//! All three creates now go through a claim that locks the course row inside
//! the create's own transaction: the write lands on the very row the delete
//! removes, so the store refuses one of the two.
//!
//! Two halves, and they are not interchangeable. **This file is the sequential
//! one**: it pins the existence contract (a create against a course that is
//! already gone must be a 404, not an orphan) plus the restore (the borrowed
//! counter comes back exactly as it was, or the course is undeletable forever —
//! a worse bug than the one being fixed).
//!
//! The **race** half drives the real interleaving and lives in-crate, one pin
//! per child beside the create it pins —
//! `db::exam::tests::an_exam_never_outlives_its_course`,
//! `db::course_session::tests::a_session_never_outlives_its_course`,
//! `db::subject::tests::a_subject_never_outlives_its_course` — over the
//! shared harness `db::course::assert_no_child_outlives_a_course_delete`,
//! against the same per-test Postgres this file runs on.

mod common;

use axum::http::StatusCode;
use common::{app_and_db, id_of, login_as, send, upload_course_note_file};
use hezarfen_backend::database::{self, Database};
use hezarfen_backend::db::course;
use hezarfen_backend::db::course_session as db_course_session;
use hezarfen_backend::domain::course::{CourseDescription, CourseId, CourseKind, CourseTitle};
use hezarfen_backend::domain::course_note::{CourseNoteContent, CourseNoteTitle};
use hezarfen_backend::domain::course_note_file::{CourseNoteFile, FileContentType, FileName};
use hezarfen_backend::domain::course_session::SessionTopic;
use hezarfen_backend::domain::exam::{
    ExamAttemptLimit, ExamDescription, ExamKind, ExamSchedule, ExamTitle,
};
use hezarfen_backend::domain::settings::Settings;
use hezarfen_backend::domain::subject::{SubjectDescription, SubjectName};
use hezarfen_backend::domain::timestamp::Timestamp;
use hezarfen_backend::domain::user::UserId;
use hezarfen_backend::error::AppError;
use serde_json::json;
use sqlx::Row as _;
use tokio::task::JoinHandle;

async fn teacher(db: &Database) -> UserId {
    // Creator columns are foreign keys now, so the fixture teacher is a row,
    // not a fabricated id. It never logs in, so the hash is a stub.
    sqlx::query(
        "INSERT INTO app_user (id, username, created_at, role) \
         VALUES ($1, 'doktor', 0, 'teacher') ON CONFLICT DO NOTHING",
    )
    .bind(UserId::generate().uuid())
    .execute(db)
    .await
    .unwrap();
    let id: uuid::Uuid = sqlx::query("SELECT id FROM app_user WHERE username = 'doktor'")
        .fetch_one(db)
        .await
        .unwrap()
        .try_get(0)
        .unwrap();
    UserId::from_key(&id.to_string())
}

async fn a_course(db: &Database) -> CourseId {
    let teacher = teacher(db).await;
    course::create(
        db,
        &teacher,
        CourseTitle::try_new("Fizik").unwrap(),
        CourseDescription::try_new("").unwrap(),
        CourseKind::course(),
        None,
        None,
    )
    .await
    .unwrap()
    .get_id()
    .clone()
}

async fn drop_course(course: &CourseId, db: &Database) -> Result<bool, AppError> {
    course::delete(
        db,
        course::read(db, course)
            .await
            .unwrap()
            .expect("the course is there"),
    )
    .await
}

// --- the three creates, each as one spawnable unit -------------------------
// Fn pointers rather than a generic closure: the three take different argument
// types and the race harness only ever needs "start it, tell me if it 500s".

fn make_exam(course: CourseId, db: Database) -> JoinHandle<Result<(), AppError>> {
    tokio::spawn(async move {
        let teacher = teacher(&db).await;
        let kinds = Settings::defaults().get_exam_kinds().to_vec();
        hezarfen_backend::db::exam::create(
            &db,
            &teacher,
            &course,
            ExamTitle::try_new("quiz").unwrap(),
            ExamDescription::try_new("").unwrap(),
            ExamKind::try_new("quiz", &kinds).unwrap(),
            ExamSchedule::try_new(None, None, None, None).unwrap(),
            ExamAttemptLimit::try_new(1).unwrap(),
            true,
            false,
            false,
        )
        .await
        .map(|_| ())
    })
}

fn make_session(course: CourseId, db: Database) -> JoinHandle<Result<(), AppError>> {
    tokio::spawn(async move {
        let teacher = teacher(&db).await;
        db_course_session::create(
            &db,
            &course,
            &teacher,
            SessionTopic::try_new("limits").unwrap(),
            Timestamp::from_millis(1),
            None,
        )
        .await
        .map(|_| ())
    })
}

fn make_subject(course: CourseId, db: Database) -> JoinHandle<Result<(), AppError>> {
    tokio::spawn(async move {
        hezarfen_backend::db::subject::create(
            &db,
            &course,
            SubjectName::try_new("Limits").unwrap(),
            SubjectDescription::try_new("").unwrap(),
        )
        .await
        .map(|_| ())
    })
}

fn make_note(course: CourseId, db: Database) -> JoinHandle<Result<(), AppError>> {
    tokio::spawn(async move {
        let teacher = teacher(&db).await;
        hezarfen_backend::db::course_note::create(
            &db,
            &course,
            &teacher,
            CourseNoteTitle::try_new("plan").unwrap(),
            CourseNoteContent::try_new("").unwrap(),
        )
        .await
        .map(|_| ())
    })
}

/// How many rows of `table` name `course`. Stored state, never a return value:
/// the whole point is what the store kept.
async fn children(table: &str, course: &CourseId, db: &Database) -> usize {
    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM {table} WHERE course = $1"
    )))
    .bind(course.clone())
    .fetch_one(db)
    .await
    .unwrap() as usize
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
    let (db, _dbs) = database::init_test_db().await;
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

#[tokio::test]
async fn a_course_note_under_a_deleted_course_is_refused() {
    a_create_against_a_deleted_course_refuses("course_note", make_note).await;
}

/// The counter the three creates borrow is *given back*. A bump left behind is
/// not a smaller bug than the orphan it prevents: the course's delete guard
/// reads `enrollment_count`, so a course that once had an exam created under it
/// would be undeletable forever.
#[tokio::test]
async fn the_creates_give_the_courses_roster_counter_back_untouched() {
    let (db, _dbs) = database::init_test_db().await;
    let course = a_course(&db).await;

    for make in [make_exam, make_session, make_subject, make_note] {
        make(course.clone(), db.clone()).await.unwrap().unwrap();
    }
    let bumped = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM course WHERE enrollment_count <> 0",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(
        bumped, 0,
        "the borrowed counter must be restored to 0, not left bumped"
    );
    // The proof that matters to a user: the course is still deletable.
    assert!(
        drop_course(&course, &db).await.unwrap(),
        "a course whose children moved its roster counter can never be deleted"
    );
}

/// The cascade half of the same contract, for a child class with two tiers: a
/// `course_note` deleted through the course cascade must take its own
/// `course_note_file` children with it too, not just itself.
#[tokio::test]
async fn a_course_note_and_its_files_never_outlive_a_course_delete() {
    let (db, _dbs) = database::init_test_db().await;
    let course = a_course(&db).await;
    let teacher = teacher(&db).await;
    let note = hezarfen_backend::db::course_note::create(
        &db,
        &course,
        &teacher,
        CourseNoteTitle::try_new("plan").unwrap(),
        CourseNoteContent::try_new("body").unwrap(),
    )
    .await
    .unwrap();
    hezarfen_backend::db::course_note_file::insert(
        &db,
        CourseNoteFile::new(
            note.get_id(),
            FileName::try_new("plan.pdf").unwrap(),
            FileContentType::try_new("application/pdf").unwrap(),
            3,
        ),
    )
    .await
    .unwrap();

    assert!(drop_course(&course, &db).await.unwrap(), "the course goes");

    assert_eq!(
        children("course_note", &course, &db).await,
        0,
        "a course_note survived its course's delete"
    );
    let files = sqlx::query_as::<_, (uuid::Uuid,)>(
        "SELECT id FROM course_note_file WHERE course_note = $1",
    )
    .bind(note.get_id().clone())
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(
        files.len(),
        0,
        "a course_note_file survived its note's course's delete"
    );
}

/// `CourseNoteFile::insert` already goes through `cap::claim_and_create` on
/// the note row (unlike the bare `db.create` `CourseNote::create` shipped
/// with) — this pins that a file upload against an already-deleted note is
/// refused rather than left as an orphan, the same existence contract as the
/// three creates above.
#[tokio::test]
async fn a_course_note_file_under_a_deleted_note_is_refused() {
    let (db, _dbs) = database::init_test_db().await;
    let course = a_course(&db).await;
    let teacher = teacher(&db).await;
    let note = hezarfen_backend::db::course_note::create(
        &db,
        &course,
        &teacher,
        CourseNoteTitle::try_new("plan").unwrap(),
        CourseNoteContent::try_new("").unwrap(),
    )
    .await
    .unwrap();
    assert!(
        course::delete(&db, course::read(&db, &course).await.unwrap().unwrap())
            .await
            .unwrap(),
        "the course, and its note with it, goes"
    );

    let file = hezarfen_backend::db::course_note_file::insert(
        &db,
        CourseNoteFile::new(
            note.get_id(),
            FileName::try_new("plan.pdf").unwrap(),
            FileContentType::try_new("application/pdf").unwrap(),
            3,
        ),
    )
    .await;
    assert!(
        matches!(file, Err(AppError::Conflict(_))),
        "a file upload under a deleted note must refuse, not {file:?}"
    );
    let files = sqlx::query_as::<_, (uuid::Uuid,)>(
        "SELECT id FROM course_note_file WHERE course_note = $1",
    )
    .bind(note.get_id().clone())
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(
        files.len(),
        0,
        "a refused upload left a row naming a note that is gone"
    );
}
#[tokio::test]
async fn an_archived_term_freezes_a_course_s_subjects_and_notes() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "arch_child_manager", "manager").await;

    let term = send(
        &app,
        "POST",
        "/terms",
        Some(&manager),
        Some(json!({
            "name": "2022",
            "starts_at": 1_500_000_000_000_i64,
            "ends_at": 1_510_000_000_000_i64,
        })),
    )
    .await;
    assert_eq!(term.status, StatusCode::CREATED, "{}", term.body);
    let term_id = id_of(&term.body);

    let course = send(
        &app,
        "POST",
        "/courses",
        Some(&manager),
        Some(json!({ "title": "Fizik", "term_id": term_id })),
    )
    .await;
    assert_eq!(course.status, StatusCode::CREATED, "{}", course.body);
    let course_id = id_of(&course.body);

    let subject = send(
        &app,
        "POST",
        &format!("/courses/{course_id}/subjects"),
        Some(&manager),
        Some(json!({ "name": "Optik" })),
    )
    .await;
    assert_eq!(subject.status, StatusCode::CREATED, "{}", subject.body);
    let subject_id = id_of(&subject.body);

    let note = send(
        &app,
        "POST",
        "/course-notes",
        Some(&manager),
        Some(json!({ "course": course_id, "title": "Ders 1" })),
    )
    .await;
    assert_eq!(note.status, StatusCode::CREATED, "{}", note.body);
    let note_id = id_of(&note.body);

    let file = upload_course_note_file(
        &app,
        &manager,
        &note_id,
        "plan.pdf",
        "application/pdf",
        b"pdf bytes",
    )
    .await;
    assert_eq!(file.status, StatusCode::CREATED, "{}", file.body);
    let file_id = id_of(&file.body);

    let archived = send(
        &app,
        "POST",
        &format!("/terms/{term_id}/archive"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(archived.status, StatusCode::OK, "{}", archived.body);

    let writes: [(&str, String, Option<serde_json::Value>); 5] = [
        (
            "PATCH",
            format!("/subjects/{subject_id}"),
            Some(json!({ "name": "Akustik" })),
        ),
        ("DELETE", format!("/subjects/{subject_id}"), None),
        (
            "POST",
            "/course-notes".to_string(),
            Some(json!({ "course": course_id, "title": "Ders 2" })),
        ),
        (
            "PATCH",
            format!("/course-notes/{note_id}"),
            Some(json!({ "title": "Ders 1a" })),
        ),
        (
            "DELETE",
            format!("/course-notes/{note_id}/files/{file_id}"),
            None,
        ),
    ];
    for (method, uri, body) in writes {
        let res = send(&app, method, &uri, Some(&manager), body).await;
        assert_eq!(
            res.status,
            StatusCode::CONFLICT,
            "{method} {uri}: {}",
            res.body
        );
        assert_eq!(res.body["code"], "term_archived", "{method} {uri}");
    }
    // The multipart upload refuses too, and before its body is buffered.
    let refused = upload_course_note_file(
        &app,
        &manager,
        &note_id,
        "more.pdf",
        "application/pdf",
        b"more bytes",
    )
    .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(refused.body["code"], "term_archived");
    // Deleting the note itself is a write as well (it cascades its files).
    let res = send(
        &app,
        "DELETE",
        &format!("/course-notes/{note_id}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert_eq!(res.body["code"], "term_archived");

    // Reads all stay open.
    for uri in [
        format!("/subjects/{subject_id}"),
        format!("/course-notes/{note_id}"),
        format!("/course-notes/{note_id}/files"),
        format!("/course-notes/{note_id}/files/{file_id}"),
    ] {
        let res = send(&app, "GET", &uri, Some(&manager), None).await;
        assert_eq!(res.status, StatusCode::OK, "GET {uri}: {}", res.body);
    }

    // Re-opening the year thaws them.
    let reopened = send(
        &app,
        "POST",
        &format!("/terms/{term_id}/unarchive"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(reopened.status, StatusCode::OK, "{}", reopened.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/subjects/{subject_id}"),
        Some(&manager),
        Some(json!({ "name": "Akustik" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}
