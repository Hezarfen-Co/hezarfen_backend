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

/// An archived term freezes the exams hanging off its courses: all nineteen
/// write routes under `/exams/{id}` answer `409 term_archived` — authoring,
/// grading, the images on both sides, and the sitting itself — while every
/// read stays open. Past years are a read-only archive, not a hidden one.
///
/// The sitting is deliberately *live* when the archive lands: a student mid-exam
/// is the case where a freeze can do real damage, and every one of their write
/// routes has to refuse with the coded 409 rather than a 500 or a silent write.
/// The guards sit after each handler's authz, so a caller who may not write
/// still gets its `403` (asserted below) — the archive never leaks exam
/// structure.
///
/// Not covered here: the exam-room WebSocket. Its door (`GET
/// /exams/{id}/attempt/ws`) and its `finish` frame carry their own guards, but
/// this binary drives the router through `oneshot` and cannot upgrade — the
/// socket needs the real-TCP harness in `tests/e2e.rs`. The REST
/// `POST /exams/{id}/attempt/answers` below does cover the shared
/// `save_answer_in` funnel that every WS `answer` frame also goes through.
#[tokio::test]
async fn an_archived_terms_exams_take_no_writes_but_still_read() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "arsiv_sinav_mudur", "manager").await;
    let student = login_as(&app, &db, "arsiv_sinav_ogrenci", "student").await;
    let student_id = me_id(&app, &student).await;

    let res = send(
        &app,
        "POST",
        "/terms",
        Some(&manager),
        Some(json!({
            "name": "2018 bahar",
            "starts_at": 1_780_000_000_000_i64,
            "ends_at": 1_790_000_000_000_i64,
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let term = id_of(&res.body);

    let res = send(
        &app,
        "POST",
        "/courses",
        Some(&manager),
        Some(json!({ "title": "Tarih", "term_id": term })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let course = id_of(&res.body);
    enroll(&app, &manager, &course, &student_id).await;
    let subject = common::create_subject(&app, &manager, &course, "Kronoloji").await;

    // `open` mode so the sitting is real: the attempt below has to be running
    // when the archive lands.
    let res = create_exam_with(
        &app,
        &manager,
        &course,
        json!({ "title": "Vize", "kind": "midterm", "mode": "open" }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let exam = id_of(&res.body);

    // One choice question — it supplies the qid every question route needs and
    // the choice id the option-picture routes need.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions"),
        Some(&manager),
        Some(json!({ "subject_id": subject, "text": "Ne zaman?", "kind": "choice", "points": 10,
                     "choices": [{"id": "c0", "text": "1453"}, {"id": "c1", "text": "1071"}], "correct": "c0" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let question = id_of(&res.body);
    let choice = res.body["choices"][0]["id"]
        .as_str()
        .expect("choice id")
        .to_string();

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    let res = send(
        &app,
        "POST",
        &format!("/terms/{term}/archive"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // The thirteen JSON write routes: exam, results, and the question authoring
    // set. `from-bank` takes an id that need not exist — the freeze is judged
    // before the bank lookup, and a 404 here would be the bug.
    let staff_writes: Vec<(&str, String, Option<serde_json::Value>)> = vec![
        (
            "PATCH",
            format!("/exams/{exam}"),
            Some(json!({ "title": "Final" })),
        ),
        ("DELETE", format!("/exams/{exam}"), None),
        (
            "POST",
            format!("/exams/{exam}/results"),
            Some(json!({ "user_id": student_id, "mark": 80 })),
        ),
        (
            "DELETE",
            format!("/exams/{exam}/results/{student_id}"),
            None,
        ),
        (
            "POST",
            format!("/exams/{exam}/questions"),
            Some(json!({ "subject_id": subject, "text": "Kim?", "kind": "text", "points": 5 })),
        ),
        (
            "PATCH",
            format!("/exams/{exam}/questions/{question}"),
            Some(json!({ "text": "Nerede?" })),
        ),
        (
            "DELETE",
            format!("/exams/{exam}/questions/{question}"),
            None,
        ),
        (
            "POST",
            format!("/exams/{exam}/questions/from-bank/yok"),
            Some(json!({ "subject_id": subject })),
        ),
        (
            "POST",
            format!("/exams/{exam}/questions/{question}/refresh-from-bank"),
            None,
        ),
        (
            "POST",
            format!("/exams/{exam}/questions/{question}/to-bank"),
            None,
        ),
        (
            "DELETE",
            format!("/exams/{exam}/questions/{question}/image"),
            None,
        ),
        (
            "DELETE",
            format!("/exams/{exam}/questions/{question}/choices/{choice}/image"),
            None,
        ),
    ];
    for (method, uri, body) in &staff_writes {
        let res = send(&app, method, uri, Some(&manager), body.clone()).await;
        assert_eq!(
            (res.status, res.body["code"].clone()),
            (StatusCode::CONFLICT, json!("term_archived")),
            "{method} {uri} must be frozen by the archive: {}",
            res.body
        );
    }

    // The student's own writes — the live sitting. `save_answer` is the REST
    // face of `save_answer_in`, the funnel the WebSocket's `answer` shares.
    let sit_writes: Vec<(&str, String, Option<serde_json::Value>)> = vec![
        ("POST", format!("/exams/{exam}/attempt"), None),
        (
            "POST",
            format!("/exams/{exam}/attempt/answers"),
            Some(json!({ "question_id": question, "selected": choice })),
        ),
        (
            "DELETE",
            format!("/exams/{exam}/attempt/answers/{question}/image"),
            None,
        ),
        ("POST", format!("/exams/{exam}/attempt/finish"), None),
    ];
    for (method, uri, body) in &sit_writes {
        let res = send(&app, method, uri, Some(&student), body.clone()).await;
        assert_eq!(
            (res.status, res.body["code"].clone()),
            (StatusCode::CONFLICT, json!("term_archived")),
            "{method} {uri} must be frozen by the archive: {}",
            res.body
        );
    }

    // The three multipart uploads. The guard runs *before* the body is read, so
    // these never touch the blob store.
    let uploads = [
        (
            format!("/exams/{exam}/questions/{question}/image"),
            manager.clone(),
        ),
        (
            format!("/exams/{exam}/questions/{question}/choices/{choice}/image"),
            manager.clone(),
        ),
        (
            format!("/exams/{exam}/attempt/answers/{question}/image"),
            student.clone(),
        ),
    ];
    for (uri, cookie) in &uploads {
        let res = common::upload_file_at(&app, cookie, uri, "a.png", "image/png", b"png").await;
        assert_eq!(
            (res.status, res.body["code"].clone()),
            (StatusCode::CONFLICT, json!("term_archived")),
            "POST {uri} must be frozen by the archive: {}",
            res.body
        );
    }

    // Reads stay open on both sides of the desk.
    for (uri, cookie) in [
        (format!("/exams/{exam}"), &manager),
        (format!("/exams/{exam}/questions"), &manager),
        (format!("/exams/{exam}/results"), &manager),
        (
            format!("/exams/{exam}/attempts/{student_id}/answers"),
            &manager,
        ),
        (format!("/exams/{exam}/attempt"), &student),
        (format!("/exams/{exam}/attempt/questions"), &student),
    ] {
        let res = send(&app, "GET", &uri, Some(cookie), None).await;
        assert_eq!(
            res.status,
            StatusCode::OK,
            "GET {uri} must stay readable: {}",
            res.body
        );
    }

    // A caller who may not write is still told *that* first: the 403 precedes
    // the 409, or the archive leaks the exam's structure to outsiders.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&student),
        Some(json!({ "title": "Sızıntı" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);

    // Unarchiving thaws them again.
    let res = send(
        &app,
        "POST",
        &format!("/terms/{term}/unarchive"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&manager),
        Some(json!({ "title": "Final" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}
