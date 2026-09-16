//! A catalog course's delete must leave the bank where deleting its content by
//! hand would have left it — and the sweep that takes that content is the
//! instance's, not the course's.
//!
//! The K12 remodel split this cascade in two, and the split decides the staging
//! of every test below. The academic work — exams with their attempts,
//! homework, lesson sessions, the roster, the assigned teachers — hangs off the
//! class×course **instance**, and the sweep that takes it is the **detach**
//! (`DELETE /classes/{class}/instances/{instance}`). A catalog course is
//! refused while any instance still teaches it (`class_course_count`, beside
//! `course_membership_count`, is the guard), so reaching [`course::delete`]
//! means the instances went first; what is left for it to sweep is the
//! catalog's own content — subjects, course notes with their files, the
//! instance rows themselves, the memberships, the blueprint links, the derived
//! AI rows, and the bank links its exams and subjects owed.
//!
//! The attempt is the one child a sweep cannot collide with on a key: its
//! create rides `cap::claim_and_create` against the *student's* row, which no
//! cascade here touches. The exam row lock (`FOR UPDATE` on start, the sweep's
//! delete waiting behind it) is what keeps a sweep and a start from both
//! committing — the race test below fires the detach into that window.

mod common;

use axum::http::StatusCode;
use common::{
    Res, app_and_db, create_exam, create_exam_with, create_subject, enroll, id_of, login, login_as,
    me_id, send, taught, unenroll,
};
use hezarfen_backend::db::course;
use hezarfen_backend::db::exam_attempt::list_for_exam;
use hezarfen_backend::domain::course::CourseId;
use hezarfen_backend::domain::exam::ExamId;
use serde_json::json;

/// Delete `course` at the store, bypassing HTTP.
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

/// Detach `t`'s instance: `DELETE /classes/{class}/instances/{instance}`, the
/// shipped sweep that takes an instance and everything taught under it — and
/// the move a catalog course's delete requires first. No assertion: each caller
/// below checks the answer it means to.
async fn detach_instance(app: &axum::Router, staff: &str, t: &common::Taught) -> Res {
    send(
        app,
        "DELETE",
        &format!("/classes/{}/instances/{}", t.class, t.instance),
        Some(staff),
        None,
    )
    .await
}

/// A bank template outlives the course it was saved out of — it is a separate,
/// school-wide library — so the sweeps owe it the same cleanup its children's
/// own deletes perform: the exam sweep clears `source_exam`, the subject sweep
/// clears `subject`. The cascade deleted both rows and neither link, leaving a
/// template pointing at two ids that no longer exist — the `source_exam` one
/// forever (nothing else ever visits that column) and the `subject` one until a
/// `PATCH` omitting `subject_id` writes it back.
///
/// Two sweeps are in the flow since the remodel, and both owe their half: the
/// exam goes with the instance's detach, the subject with the course's delete —
/// which the detach is what makes reachable.
#[tokio::test]
async fn a_course_delete_clears_the_bank_links_its_exams_and_subjects_owed() {
    let (app, db) = app_and_db().await;
    let mudur = login_as(&app, &db, "banka_ders_sil", "manager").await;
    let t = taught(&app, &mudur, "Fizik").await;
    let subject = create_subject(&app, &mudur, &t.course, "Optik").await;
    let exam = create_exam(&app, &mudur, &t.instance, &t.term, "Vize", "yazili").await;

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions"),
        Some(&mudur),
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
        Some(&mudur),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let template = id_of(&res.body);
    // Both links are set by the save — the test is worthless if they are not.
    assert_eq!(res.body["source_exam"], exam, "{}", res.body);
    assert_eq!(res.body["subject"], subject, "{}", res.body);

    // The instance first: a catalog course is refused while one still teaches
    // it, and this detach is what takes the exam.
    let detached = detach_instance(&app, &mudur, &t).await;
    assert_eq!(
        detached.status,
        StatusCode::NO_CONTENT,
        "the instance detaches: {}",
        detached.body
    );
    drop_course(&db, &t.course).await;

    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{template}"),
        Some(&mudur),
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

/// The marks a sweep takes hold a reference on their exam *kind*, and that
/// counter is the only thing standing between a manager and removing a kind the
/// school is already graded under. Nothing covered the release on this path —
/// only on `Exam::delete`'s and `remove_result`'s — and the `GROUP BY` doing it
/// is the shape this store has answered with no rows before.
///
/// The sweep is the instance's now: a catalog course is refused while it still
/// carries an instance, so the detach is what takes the marks (and is the only
/// path that can), after which the course delete follows.
#[tokio::test]
async fn the_sweep_that_takes_a_courses_marks_gives_their_kind_references_back() {
    let (app, db) = app_and_db().await;
    let mudur = login_as(&app, &db, "kind_ref_ders_sil", "manager").await;
    let student = login(&app, "kind_ref_ogrenci").await;
    let student_id = me_id(&app, &student).await;

    let t = taught(&app, &mudur, "Tarih").await;
    enroll(&app, &mudur, &t.instance, &student_id).await;
    let exam = create_exam(&app, &mudur, &t.instance, &t.term, "Vize", "yazili").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&mudur),
        Some(json!({ "user_id": student_id, "mark": 70 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let counted = |db: hezarfen_backend::database::Database| async move {
        sqlx::query_as::<_, (i64,)>("SELECT count FROM kind_ref WHERE name = 'yazili'")
            .fetch_optional(&db)
            .await
            .expect("kind_ref read")
            .map(|(count,)| count)
            .unwrap_or(0)
    };
    assert_eq!(counted(db.clone()).await, 1, "the mark must be counted");

    // The detach is what takes the mark — the course delete cannot even be
    // admitted while the instance stands — so it is the path that owes the
    // release; the course delete follows it onto an empty catalog.
    let detached = detach_instance(&app, &mudur, &t).await;
    assert_eq!(
        detached.status,
        StatusCode::NO_CONTENT,
        "the instance detaches: {}",
        detached.body
    );
    drop_course(&db, &t.course).await;

    assert_eq!(
        counted(db.clone()).await,
        0,
        "the swept mark's kind reference was never given back — the kind can \
         never leave the school's settings"
    );
}

/// The sitting's own write is held open, every gate already cleared, and the
/// sweep that takes its exam fires into that window: an `AFTER INSERT` trigger
/// on `exam_attempt` sleeps *inside* the create's own transaction, after the
/// enrollment gate has passed, and the detach that follows lands inside it.
///
/// That is exactly the pairing the exam row lock exists to serialize. The
/// sitting's transaction holds the exam row's write lock across the sleeping
/// trigger (the start takes `FOR UPDATE` and keeps it through the insert), so
/// the sweep cannot even reach its attempt delete until the sitting has
/// committed — and the sitting finds its exam gone if it goes second. Without
/// the lock the sweep runs on a snapshot taken before the start commits and the
/// sitting outlives both its exam and its instance, unreachable (every route to
/// an attempt goes through its exam) with the student's lifetime sitting
/// counter up for good.
///
/// The course's own delete needs one move more, and that move is the shipped
/// shape of this test now: the instance has to go before the catalog row can
/// (the guard reads `class_course_count`), so the detach is where the race is
/// staged and the course delete closes the flow.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attempt_started_inside_a_detach_never_outlives_it() {
    let (app, db) = app_and_db().await;
    let mudur = login_as(&app, &db, "ogretmen_ders_sil", "manager").await;
    let student = login_as(&app, &db, "ogrenci_ders_sil", "student").await;
    let student_id = me_id(&app, &student).await;

    // Hold the sitting's own write open for a full second, gates already
    // passed, so the roster change and the detach both land inside it. An AFTER
    // INSERT trigger sleeping inside the create's own transaction is the
    // Postgres shape of the old window event.
    let mut conn = db.acquire().await.expect("acquire for the trigger");
    sqlx::raw_sql(
        "CREATE FUNCTION heztest_hold_start() RETURNS trigger AS $$
         BEGIN PERFORM pg_sleep(1.0); RETURN NULL; END;
         $$ LANGUAGE plpgsql;
         CREATE TRIGGER heztest_hold_start AFTER INSERT ON exam_attempt
         FOR EACH ROW EXECUTE FUNCTION heztest_hold_start();",
    )
    .execute(&mut *conn)
    .await
    .expect("define the window trigger");

    let t = taught(&app, &mudur, "Kimya").await;
    enroll(&app, &mudur, &t.instance, &student_id).await;
    // `open` mode, as next door: an unscheduled exam answers the start with a
    // 409 before it ever writes, and there would be no attempt to race.
    let res = create_exam_with(
        &app,
        &mudur,
        &t.instance,
        json!({ "title": "Vize", "kind": "yazili", "mode": "open", "term": t.term.clone() }),
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
    // The enrollment goes while the sitting is mid-write — the roster change
    // the old staging needed to admit the delete, kept because a sitting that
    // loses its enrollment mid-write is the state worth racing.
    unenroll(&app, &mudur, &t.instance, &student_id).await;
    let detached = detach_instance(&app, &mudur, &t).await;
    let dropped = send(
        &app,
        "DELETE",
        &format!("/courses/{}", t.course),
        Some(&mudur),
        None,
    )
    .await;
    let sat = sit.await.unwrap();

    assert_eq!(
        detached.status,
        StatusCode::NO_CONTENT,
        "the detach must succeed: {:?}",
        detached.body
    );
    assert_eq!(
        dropped.status,
        StatusCode::NO_CONTENT,
        "the course delete must succeed once the instance is gone: {:?}",
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
        "a sitting outlived the exam (and instance) it was sat under"
    );
}
