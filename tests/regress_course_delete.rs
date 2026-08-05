//! A course delete must take every attempt under it with it — the same hole
//! `regress_exam_delete` pins one level down, and the same lease.
//!
//! `delete_course` cascades the course's exams *and their attempts*, and an
//! attempt is still the one exam child whose write cannot collide with that
//! sweep: its create rides `cap::claim_and_create` against the *student's* row,
//! which this cascade never touches. So the sweep and a start in flight can
//! both commit, and only `EXAM_LOCK.write()` keeps them apart.
//!
//! **Its own test binary on purpose.** `EXAM_LOCK` is process-wide, so this
//! test and `regress_exam_delete`'s share it across two unrelated databases
//! when they run in one binary: the neighbour holds the writer lease through
//! its own one-second window, the start here queues behind it, and the timeline
//! this test is built on stops holding. It was mutation-proven red alone and
//! green — on the *broken* code — beside that neighbour. Anything else added
//! here must not take `EXAM_LOCK` either.

mod common;

use axum::http::StatusCode;
use common::{app_and_db, create_exam_with, enroll, id_of, login_as, me_id, send, unenroll};
use hezarfen_backend::domain::exam::ExamId;
use hezarfen_backend::domain::exam_attempt::ExamAttempt;
use serde_json::json;

/// Reaching the window takes one move `regress_exam_delete` does not need,
/// because a course is refused while anyone is enrolled: the sitting has to
/// pass its enrollment gate and then lose that enrollment while it is still
/// being written.
///
/// That also decides which side the window is opened on. It cannot be the
/// delete's, the way the exam test does it — by the time a course delete is
/// admitted the roster is empty, so a start fired into its window is answered
/// `403` by the enrollment gate and the test would pass on a missing lease. So
/// the *start* is held open instead: a `DEFINE EVENT` on `exam_attempt` fires
/// inside the create's own transaction, after every gate has been cleared, and
/// holds the start — and with it the writer lease — across the unenroll and the
/// delete that follow. That is exactly the pairing the lease exists to
/// serialize; without it the cascade sweeps attempts on a snapshot taken before
/// this one commits, and the sitting outlives both its exam and its course,
/// unreachable (every route to an attempt goes through its exam) with the
/// student's lifetime sitting counter up for good.
///
/// The in-memory engine is enough, as it is next door: this asserts a *lock*,
/// not the store's conflict detection, and a mutex behaves the same on either
/// engine.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attempt_started_inside_a_course_delete_never_outlives_it() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ogretmen_ders_sil", "teacher").await;
    let student = login_as(&app, &db, "ogrenci_ders_sil", "student").await;
    let student_id = me_id(&app, &student).await;

    // Hold the sitting's own write open for a full second, gates already
    // passed, so the unenroll and the delete both land inside it.
    db.query(
        "DEFINE EVENT hold_the_start ON TABLE exam_attempt WHEN $event = 'CREATE' \
         THEN { SLEEP 1s; };",
    )
    .await
    .expect("define the window event")
    .check()
    .expect("check the window event");

    let course = common::create_course(&app, &teacher, "Kimya").await;
    enroll(&app, &teacher, &course, &student_id).await;
    // `open` mode, as next door: an unscheduled exam answers the start with a
    // 409 before it ever writes, and there would be no attempt to race.
    let res = create_exam_with(
        &app,
        &teacher,
        &course,
        json!({ "title": "quiz", "kind": "midterm", "mode": "open" }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "create exam");
    let exam = id_of(&res.body);

    let sit = {
        let (app, student, exam) = (app.clone(), student.clone(), exam.clone());
        tokio::spawn(async move {
            send(
                &app,
                "POST",
                &format!("/exams/{exam}/attempt"),
                Some(&student),
                None,
            )
            .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    // The enrollment goes while the sitting is mid-write — the only staging in
    // which the delete is admitted at all. Without it this is a 409 and the
    // race never happens.
    unenroll(&app, &teacher, &course, &student_id).await;
    let dropped = send(
        &app,
        "DELETE",
        &format!("/courses/{course}"),
        Some(&teacher),
        None,
    )
    .await;
    let sat = sit.await.unwrap();

    assert_eq!(
        dropped.status,
        StatusCode::NO_CONTENT,
        "the delete must succeed: {:?}",
        dropped.body
    );
    // A 404 for the start is a correct answer too; the only defect is stored
    // state. Nothing may 500 either way.
    assert!(
        sat.status == StatusCode::NOT_FOUND || sat.status.is_success(),
        "a raced start must be answered, not {}: {:?}",
        sat.status,
        sat.body
    );
    // Stored state is the whole verdict; a response code is not evidence.
    assert_eq!(
        ExamAttempt::list_for_exam(&ExamId::from_key(&exam), &db)
            .await
            .unwrap()
            .len(),
        0,
        "a sitting outlived the course it was sat under"
    );
}
