//! An exam delete must take every child with it — including the one child the
//! store cannot refuse on its own.
//!
//! Every other child of an exam (answers, drawings, questions, pictures) writes
//! the exam row inside its own transaction, so a delete racing one of them
//! contends on a single key and the store aborts a side. An attempt cannot do
//! that: its create rides `cap::claim_and_create` against the *student's* row
//! (`exam_sat_total`), which this delete never touches, so nothing collides and
//! the freeze/existence gate is a plain read. `start_attempt` has always held
//! `EXAM_LOCK.write()` from its exam read through the insert; `delete_exam`
//! held nothing at all, and that asymmetry is what let a sitting start inside
//! the delete window and outlive the exam.
//!
//! This suite drives the real handlers, because the lease being tested lives in
//! the web layer and no domain call can observe it.

mod common;

use axum::http::StatusCode;
use common::{app_and_db, create_exam_with, enroll, id_of, login_as, me_id, send};
use hezarfen_backend::domain::exam::ExamId;
use hezarfen_backend::domain::exam_attempt::ExamAttempt;
use serde_json::json;

/// The delete is held open for a second *after* the exam row is gone but
/// before its cascade runs — a `DEFINE EVENT` on the table fires inside the
/// delete's own transaction, so the window is opened by the database rather
/// than by a lucky interleaving. A start firing into that window used to read
/// an exam that was still there (deleted, uncommitted), create its sitting, and
/// commit past a sweep that had already run on a snapshot without it. That row
/// was unreachable afterwards — every route to an attempt goes through its exam
/// — while the student's lifetime sitting counter stayed up for good, and a
/// badge minted off it is never revoked.
///
/// The in-memory engine is enough here, unusually: this asserts a *lock*, not
/// the store's conflict detection, and a mutex behaves the same on either
/// engine. Mutation-tested — dropping the lease from `delete_exam` turns it
/// red.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attempt_started_inside_a_delete_never_outlives_the_exam() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ogretmen_sil", "teacher").await;
    let student = login_as(&app, &db, "ogrenci_sil", "student").await;
    let student_id = me_id(&app, &student).await;

    // Hold the delete open for a full second once the row is gone, while its
    // cascade still has to run.
    db.query(
        "DEFINE EVENT hold_the_window ON TABLE exam WHEN $event = 'DELETE' \
         THEN { SLEEP 1s; };",
    )
    .await
    .expect("define the window event")
    .check()
    .expect("check the window event");

    let mut sittings = 0;
    for round in 0..3 {
        let course = common::create_course(&app, &teacher, &format!("Fizik {round}")).await;
        enroll(&app, &teacher, &course, &student_id).await;
        // `open` mode, so the exam is actually sittable: an unscheduled one
        // answers the start with a 409 before it ever writes, which would make
        // this test pass on a missing lease.
        let res = create_exam_with(
            &app,
            &teacher,
            &course,
            json!({ "title": "quiz", "kind": "midterm", "mode": "open" }),
        )
        .await;
        assert_eq!(
            res.status,
            StatusCode::CREATED,
            "round {round}: create exam"
        );
        let exam = id_of(&res.body);

        let drop_it = {
            let (app, teacher, exam) = (app.clone(), teacher.clone(), exam.clone());
            tokio::spawn(async move {
                send(
                    &app,
                    "DELETE",
                    &format!("/exams/{exam}"),
                    Some(&teacher),
                    None,
                )
                .await
            })
        };
        // The start fires inside the held window: without the lease it reads an
        // exam row the delete has removed but not committed.
        // race-window staging — do not convert to poll
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
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
        let (dropped, sat) = (drop_it.await.unwrap(), sit.await.unwrap());
        assert_eq!(
            dropped.status,
            StatusCode::NO_CONTENT,
            "round {round}: the delete must succeed"
        );
        // A 404 for the start is the correct answer; the only defect is stored
        // state. Nothing may 500 either way.
        assert!(
            sat.status == StatusCode::NOT_FOUND || sat.status.is_success(),
            "round {round}: a raced start must be answered, not {}: {:?}",
            sat.status,
            sat.body
        );

        // Stored state is the whole verdict; a response code is not evidence.
        let id = ExamId::from_key(&exam);
        sittings += ExamAttempt::list_for_exam(&id, &db).await.unwrap().len();
    }
    assert_eq!(sittings, 0, "a sitting outlived its exam");
}
