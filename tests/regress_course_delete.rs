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
//!
//! Which is why the two cascade tests below fire the delete at
//! [`Course::delete`] rather than at `DELETE /courses/{id}`: the handler takes
//! the writer lease, the defects they pin live in that transaction's SQL, and
//! neither one needs a race to show.

mod common;

use axum::http::StatusCode;
use common::{
    app_and_db, create_course, create_exam, create_exam_with, create_subject, enroll, id_of, login,
    login_as, me_id, send, unenroll,
};
use hezarfen_backend::db::course;
use hezarfen_backend::db::exam_attempt::list_for_exam;
use hezarfen_backend::domain::course::CourseId;
use hezarfen_backend::domain::exam::ExamId;
use serde_json::json;

/// Delete `course` the way the handler would, minus its `EXAM_LOCK` lease.
async fn drop_course(db: &hezarfen_backend::database::Database, course: &str) {
    let row = course::read(db, &CourseId::from_key(course))
        .await
        .expect("read the course")
        .expect("the course exists");
    assert!(
        course::delete(db, row).await.expect("delete the course"),
        "refused"
    );
}

/// A bank template outlives the course it was saved out of — it is a separate,
/// school-wide library — so the cascade owes it the same cleanup its children's
/// own deletes perform: `Exam::delete` clears `source_exam`, `db::subject::delete`
/// clears `subject`. The course cascade deleted both rows and neither link,
/// leaving a template pointing at two ids that no longer exist — the
/// `source_exam` one forever (nothing else ever visits that column) and the
/// `subject` one until a `PATCH` omitting `subject_id` writes it back.
#[tokio::test]
async fn a_course_delete_clears_the_bank_links_its_exams_and_subjects_owed() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "banka_ders_sil", "teacher").await;
    let course = create_course(&app, &teacher, "Fizik").await;
    let subject = create_subject(&app, &teacher, &course, "Optik").await;
    let exam = create_exam(&app, &teacher, &course, "quiz", "midterm").await;

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        Some(json!({
            "subject_id": subject, "text": "mercek", "kind": "text", "points": 5
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let question = id_of(&res.body);

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{question}/to-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let template = id_of(&res.body);
    // Both links are set by the save — the test is worthless if they are not.
    assert_eq!(res.body["source_exam"], exam, "{}", res.body);
    assert_eq!(res.body["subject"], subject, "{}", res.body);

    drop_course(&db, &course).await;

    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{template}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        (res.body["source_exam"].clone(), res.body["subject"].clone()),
        (json!(null), json!(null)),
        "the bank must be left where deleting the exam and the subject by hand \
         would have left it: {}",
        res.body
    );
}

/// The marks the cascade sweeps hold a reference on their exam *kind*, and that
/// counter is the only thing standing between a manager and removing a kind the
/// school is already graded under. Nothing covered the release on this path —
/// only on `Exam::delete`'s and `remove_result`'s — and the `GROUP BY` doing it
/// is the shape this store has answered with no rows before.
#[tokio::test]
async fn a_course_delete_gives_back_the_kind_references_its_marks_held() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "kind_ref_ders_sil", "teacher").await;
    let student = login(&app, "kind_ref_ogrenci").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "Tarih").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = create_exam(&app, &teacher, &course, "Vize", "midterm").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        Some(json!({ "user_id": student_id, "mark": 70 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let counted = |db: hezarfen_backend::database::Database| async move {
        db.query("SELECT VALUE count ?? 0 FROM kind_ref:midterm")
            .await
            .expect("kind_ref read")
            .check()
            .expect("kind_ref read")
            .take::<Vec<i64>>(0)
            .expect("kind_ref count")
            .first()
            .copied()
            .unwrap_or(0)
    };
    assert_eq!(counted(db.clone()).await, 1, "the mark must be counted");

    // The roster has to empty before the delete is admitted at all; the mark
    // stays behind, which is precisely why the cascade owes the release.
    unenroll(&app, &teacher, &course, &student_id).await;
    drop_course(&db, &course).await;

    assert_eq!(
        counted(db.clone()).await,
        0,
        "the swept mark's kind reference was never given back — the kind can \
         never leave the school's settings"
    );
}

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
    // race-window staging — do not convert to poll
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
        list_for_exam(&db, &ExamId::from_key(&exam))
            .await
            .unwrap()
            .len(),
        0,
        "a sitting outlived the course it was sat under"
    );
}

/// An archived term freezes the courses hanging off it: every write route under
/// `/courses/{id}` answers `409 term_archived`, while every read stays open —
/// past years are a read-only archive, not a hidden one. The guard sits *after*
/// each handler's authz check, so a caller who may not write still gets its
/// `403` rather than being told about the term.
#[tokio::test]
async fn an_archived_terms_courses_take_no_writes_but_still_read() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "arsiv_ders_mudur", "manager").await;
    let student = login_as(&app, &db, "arsiv_ders_ogrenci", "student").await;
    let student_id = me_id(&app, &student).await;
    let teacher = login_as(&app, &db, "arsiv_ders_ogretmen", "teacher").await;
    let teacher_id = me_id(&app, &teacher).await;

    let res = send(
        &app,
        "POST",
        "/terms",
        Some(&manager),
        Some(json!({
            "name": "2019 güz",
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
        Some(json!({ "title": "Coğrafya", "term_id": term })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let course = id_of(&res.body);
    // Both the roster and an assigned teacher have to exist *before* the
    // archive: the routes that remove them are themselves frozen.
    enroll(&app, &manager, &course, &student_id).await;
    let subject = create_subject(&app, &manager, &course, "Iklim").await;
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/teachers"),
        Some(&manager),
        Some(json!({ "user_id": teacher_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let res = send(
        &app,
        "POST",
        &format!("/terms/{term}/archive"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let due = 1_900_000_000_000_i64;
    let writes: Vec<(&str, String, Option<serde_json::Value>)> = vec![
        (
            "PATCH",
            format!("/courses/{course}"),
            Some(json!({ "title": "Yeni" })),
        ),
        ("DELETE", format!("/courses/{course}"), None),
        (
            "POST",
            format!("/courses/{course}/teachers"),
            Some(json!({ "user_id": teacher_id })),
        ),
        (
            "DELETE",
            format!("/courses/{course}/teachers/{teacher_id}"),
            None,
        ),
        (
            "POST",
            format!("/courses/{course}/enrollments"),
            Some(json!({ "user_id": student_id })),
        ),
        (
            "DELETE",
            format!("/courses/{course}/enrollments/{student_id}"),
            None,
        ),
        (
            "POST",
            format!("/courses/{course}/exams"),
            Some(json!({ "title": "Vize", "kind": "midterm" })),
        ),
        (
            "POST",
            format!("/courses/{course}/subjects"),
            Some(json!({ "name": "Erozyon" })),
        ),
        (
            "POST",
            format!("/courses/{course}/homework"),
            Some(json!({ "title": "Ödev", "subject_id": subject, "due_at": due })),
        ),
        (
            "POST",
            format!("/courses/{course}/sessions"),
            Some(json!({ "starts_at": due })),
        ),
    ];
    for (method, uri, body) in &writes {
        let res = send(&app, method, uri, Some(&manager), body.clone()).await;
        assert_eq!(
            (res.status, res.body["code"].clone()),
            (StatusCode::CONFLICT, json!("term_archived")),
            "{method} {uri} must be frozen by the archive: {}",
            res.body
        );
    }

    for uri in [
        format!("/courses/{course}"),
        format!("/courses/{course}/enrollments"),
        format!("/courses/{course}/exams"),
        format!("/courses/{course}/subjects"),
        format!("/courses/{course}/homework"),
        format!("/courses/{course}/sessions"),
    ] {
        let res = send(&app, "GET", &uri, Some(&manager), None).await;
        assert_eq!(
            res.status,
            StatusCode::OK,
            "GET {uri} must stay readable: {}",
            res.body
        );
    }

    // A student who cannot write here is still told *that* first — the 403 has
    // to precede the 409, or the archive leaks course structure to outsiders.
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/subjects"),
        Some(&student),
        Some(json!({ "name": "Sızıntı" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);

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
        "POST",
        &format!("/courses/{course}/subjects"),
        Some(&manager),
        Some(json!({ "name": "Erozyon" })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "the unarchive must give the writes back: {}",
        res.body
    );
}
