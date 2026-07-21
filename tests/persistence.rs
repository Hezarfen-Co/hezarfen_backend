//! Boot-migration tests: age rows into the shapes old binaries left behind,
//! then re-run the idempotent migration on the same database — a second boot —
//! and assert the backfills repair (or deliberately destroy) them. The storage
//! engine itself is the SurrealDB server's concern since the embedded engine
//! was retired; these tests run on the in-memory engine like every other suite.

mod common;

use axum::http::StatusCode;
use axum::Router;
use common::{create_course, create_exam, enroll, me_id, send, set_role};
use hezarfen_backend::database::Database;
use hezarfen_backend::rate_limit::RateLimitConfig;
use hezarfen_backend::state::AppState;
use hezarfen_backend::{build_router, database};
use serde_json::json;

/// Simulate a reboot: re-run the migration on the same database and hand back
/// a fresh router over it.
async fn reboot(db: &Database) -> Router {
    database::migrate(db).await.expect("re-migration");
    build_router(AppState {
        db: db.clone(),
        files_path: common::files_dir(),
        cookie_secure: false,
        rate_limit: RateLimitConfig::unlimited(),
        exam_presence: Default::default(),
    })
}

/// School policy and terms survive a second boot — the settings singleton
/// (nested band objects included) and the course→term link both come back,
/// untouched by the re-applied idempotent migration.
#[tokio::test]
async fn settings_and_terms_survive_remigration() {
    let (app, db) = common::app_and_db().await;

    let creds = json!({ "username": "boss", "password": "secret1" });
    send(&app, "POST", "/auth/register", None, Some(creds.clone())).await;
    set_role(&db, "boss", "manager").await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds.clone()))
        .await
        .cookie
        .unwrap();

    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&cookie),
        Some(json!({
            "exam_kinds": [
                { "name": "lab", "weight": 2 },
                { "name": "quiz", "weight": 1 },
            ],
            "grade_bands": [{ "min": 0, "label": "F" }, { "min": 50, "label": "P" }],
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    let res = send(
        &app,
        "POST",
        "/terms",
        Some(&cookie),
        Some(json!({ "name": "2026 Fall", "starts_at": 1, "ends_at": 2 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let term = res.body["id"].as_str().unwrap().to_string();

    let res = send(
        &app,
        "POST",
        "/courses",
        Some(&cookie),
        Some(json!({ "title": "History", "term_id": term })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);

    // Second boot: migration re-applied over live data.
    let app = reboot(&db).await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds))
        .await
        .cookie
        .unwrap();

    let res = send(&app, "GET", "/settings", Some(&cookie), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        res.body["exam_kinds"],
        json!([
            { "name": "lab", "weight": 2 },
            { "name": "quiz", "weight": 1 },
        ])
    );
    assert_eq!(res.body["grade_bands"][0]["label"], "P");

    let res = send(&app, "GET", &format!("/terms/{term}"), Some(&cookie), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["name"], "2026 Fall");

    let res = send(&app, "GET", "/courses", Some(&cookie), None).await;
    let courses = common::items(&res.body);
    assert_eq!(courses.len(), 1);
    assert_eq!(courses[0]["term"].as_str(), Some(term.as_str()));
}

/// Event rows written before the `audience` column existed are backfilled to
/// school-wide on the next boot (`UPDATE event SET audience = { kind: 'school' }
/// WHERE audience = NONE`), so an old volume keeps serving its events.
#[tokio::test]
async fn legacy_events_backfill_to_school_audience() {
    let (app, db) = common::app_and_db().await;
    let creds = json!({ "username": "ali", "password": "secret1" });

    // A normal event — then strip its audience the way an old binary's schema
    // would have left it: drop the column definitions so SCHEMAFULL stops
    // enforcing them, and unset the field on the row.
    assert_eq!(
        send(&app, "POST", "/auth/register", None, Some(creds.clone()))
            .await
            .status,
        StatusCode::CREATED
    );
    set_role(&db, "ali", "teacher").await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds.clone()))
        .await
        .cookie
        .unwrap();
    let res = send(
        &app,
        "POST",
        "/events",
        Some(&cookie),
        Some(json!({ "title": "before audiences" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let event_id = common::id_of(&res.body);

    db.query(
        "REMOVE FIELD IF EXISTS audience.users ON TABLE event;
         REMOVE FIELD IF EXISTS audience.course ON TABLE event;
         REMOVE FIELD IF EXISTS audience.role ON TABLE event;
         REMOVE FIELD IF EXISTS audience.kind ON TABLE event;
         REMOVE FIELD IF EXISTS audience ON TABLE event;
         UPDATE event SET audience = NONE;",
    )
    .await
    .expect("strip audience")
    .check()
    .expect("strip audience check");

    // Second boot re-runs the migration: the field definitions come back and
    // the backfill stamps the legacy row school-wide — it reads, lists, and
    // rosters like any new event.
    let app = reboot(&db).await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds))
        .await
        .cookie
        .unwrap();
    let res = send(
        &app,
        "GET",
        &format!("/events/{event_id}"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["audience"]["kind"], "school");
    let res = send(
        &app,
        "GET",
        &format!("/events/{event_id}/roster"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(common::total(&res.body), 1, "whole school = the one user");
}

/// Event rows carrying the retired hand-picked (`users`) audience convert on
/// the next boot: the audience becomes an uncapped registration list and each
/// listed user gets a signup row credited to the event's creator — the old
/// roster survives the clean break byte for byte.
#[tokio::test]
async fn legacy_users_audiences_convert_to_registrations() {
    let (app, db) = common::app_and_db().await;
    let creds = json!({ "username": "ali", "password": "secret1" });

    // A registration-audience event — then rewrite it into the shape an old
    // binary left behind: restore the `audience.users` column definition and
    // store a hand-picked list on the row.
    assert_eq!(
        send(&app, "POST", "/auth/register", None, Some(creds.clone()))
            .await
            .status,
        StatusCode::CREATED
    );
    set_role(&db, "ali", "teacher").await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds.clone()))
        .await
        .cookie
        .unwrap();
    let student = json!({ "username": "veli", "password": "secret1" });
    assert_eq!(
        send(&app, "POST", "/auth/register", None, Some(student.clone()))
            .await
            .status,
        StatusCode::CREATED
    );
    let student_cookie = send(&app, "POST", "/auth/login", None, Some(student))
        .await
        .cookie
        .unwrap();
    let student_id = common::me_id(&app, &student_cookie).await;

    let res = send(
        &app,
        "POST",
        "/events",
        Some(&cookie),
        Some(json!({ "title": "trip", "audience": { "kind": "registration" } })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let event_id = common::id_of(&res.body);

    db.query(format!(
        "DEFINE FIELD OVERWRITE audience.users ON TABLE event TYPE option<array<record<user>>>;
         UPDATE event SET audience = {{ kind: 'users', users: [type::record('user', $usr)] }}
             WHERE id = type::record('event', '{event_id}');"
    ))
    .bind(("usr", student_id.clone()))
    .await
    .expect("rewrite to legacy users audience")
    .check()
    .expect("rewrite check");

    // Second boot runs the conversion: the audience reads back as an uncapped
    // registration list, the listed student holds a seat credited to the
    // creator, and the roster (and marking gate) see them.
    let app = reboot(&db).await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds))
        .await
        .cookie
        .unwrap();
    let res = send(
        &app,
        "GET",
        &format!("/events/{event_id}"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["audience"]["kind"], "registration");
    assert!(res.body["audience"]["capacity"].is_null());
    let res = send(
        &app,
        "GET",
        &format!("/events/{event_id}/roster"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(common::total(&res.body), 1, "the listed user became a seat");
    assert_eq!(common::items(&res.body)[0]["user"]["id"], student_id);
    let res = send(
        &app,
        "POST",
        &format!("/events/{event_id}/attendance"),
        Some(&cookie),
        Some(json!({ "status": "present", "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "converted seat is markable");
}

/// Exam-question rows written before subjects existed (2026-07) are destroyed
/// on the next boot, answers first — a subject is mandatory and there is
/// nothing truthful to backfill. The exam and the course's subjects survive.
#[tokio::test]
async fn legacy_subjectless_questions_are_destroyed_on_boot() {
    let (app, db) = common::app_and_db().await;
    let creds = json!({ "username": "ali", "password": "secret1" });

    // A full exam — course, subject, open exam, one answered question — then
    // strip the question's subject the way an old binary's schema would have
    // left it.
    assert_eq!(
        send(&app, "POST", "/auth/register", None, Some(creds.clone()))
            .await
            .status,
        StatusCode::CREATED
    );
    set_role(&db, "ali", "teacher").await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds.clone()))
        .await
        .cookie
        .unwrap();
    let student = json!({ "username": "veli", "password": "secret1" });
    assert_eq!(
        send(&app, "POST", "/auth/register", None, Some(student.clone()))
            .await
            .status,
        StatusCode::CREATED
    );
    let student_cookie = send(&app, "POST", "/auth/login", None, Some(student))
        .await
        .cookie
        .unwrap();
    let student_id = common::me_id(&app, &student_cookie).await;

    let course = create_course(&app, &cookie, "algebra").await;
    enroll(&app, &cookie, &course, &student_id).await;
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/subjects"),
        Some(&cookie),
        Some(json!({ "name": "arithmetic" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let subject_id = common::id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/exams"),
        Some(&cookie),
        Some(json!({ "title": "drill", "kind": "quiz", "mode": "open" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let exam_id = common::id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam_id}/questions"),
        Some(&cookie),
        Some(
            json!({ "subject_id": subject_id, "text": "2 + 2?", "kind": "choice",
                     "points": 10, "choices": ["3", "4"], "correct": 1 }),
        ),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let question_id = common::id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam_id}/attempt"),
        Some(&student_cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam_id}/attempt/answers"),
        Some(&student_cookie),
        Some(json!({ "question_id": question_id, "selected": 1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    db.query(
        "REMOVE FIELD IF EXISTS subject ON TABLE exam_question;
         UPDATE exam_question SET subject = NONE;",
    )
    .await
    .expect("strip subject")
    .check()
    .expect("strip subject check");

    // Second boot re-runs the migration and the destroy-backfill: the
    // subjectless question and its answer are gone, the exam and subject
    // still stand.
    let app = reboot(&db).await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds))
        .await
        .cookie
        .unwrap();
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam_id}/questions"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(common::total(&res.body), 0, "legacy questions destroyed");
    let res = send(
        &app,
        "GET",
        &format!("/subjects/{subject_id}"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "the subject itself survives");
    let mut result = db
        .query("SELECT VALUE id FROM exam_answer")
        .await
        .expect("count answers")
        .check()
        .expect("count answers check");
    let answers: Vec<surrealdb::types::RecordId> = result.take(0).expect("answer rows");
    assert!(answers.is_empty(), "orphaned answers destroyed");
}

/// Enrollment rows whose user is no longer a student — promoted before the
/// role endpoint swept enrollments (2026-07-18), or deleted outright — are
/// removed by the boot backfill. A real student's row survives.
#[tokio::test]
async fn stale_staff_enrollments_are_swept_on_boot() {
    let (app, db) = common::app_and_db().await;
    let creds = json!({ "username": "ali", "password": "secret1" });

    // A course with three enrolled students, then age two rows the way a
    // pre-fix binary could have — flip one student to teacher directly in the
    // DB, and delete the other's user record entirely.
    assert_eq!(
        send(&app, "POST", "/auth/register", None, Some(creds.clone()))
            .await
            .status,
        StatusCode::CREATED
    );
    set_role(&db, "ali", "teacher").await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds.clone()))
        .await
        .cookie
        .unwrap();
    let course = create_course(&app, &cookie, "algebra").await;

    let mut ids = Vec::new();
    for name in ["veli", "ayse", "can"] {
        let student = json!({ "username": name, "password": "secret1" });
        assert_eq!(
            send(&app, "POST", "/auth/register", None, Some(student.clone()))
                .await
                .status,
            StatusCode::CREATED
        );
        let student_cookie = send(&app, "POST", "/auth/login", None, Some(student))
            .await
            .cookie
            .unwrap();
        let id = me_id(&app, &student_cookie).await;
        enroll(&app, &cookie, &course, &id).await;
        ids.push(id);
    }
    let keeper_id = ids[1].clone();
    set_role(&db, "veli", "teacher").await;
    db.query("DELETE user WHERE username = 'can'")
        .await
        .expect("delete user")
        .check()
        .expect("delete user check");

    // Second boot: the backfill swept the promoted user's row and the deleted
    // user's dangling row; the remaining student's enrollment is intact.
    let app = reboot(&db).await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds))
        .await
        .cookie
        .unwrap();
    let roster = send(
        &app,
        "GET",
        &format!("/courses/{course}/enrollments"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(roster.status, StatusCode::OK, "{}", roster.body);
    assert_eq!(
        common::total(&roster.body),
        1,
        "stale rows swept: {}",
        roster.body
    );
    assert_eq!(
        common::items(&roster.body)[0]["user"]["id"],
        keeper_id.as_str(),
        "the real student's row survives"
    );
}

/// Exam rows written before the draft flag existed (2026-07-19) backfill to
/// published (`draft = false`) on the next boot — an old volume's exams stay
/// exactly as visible as they were.
#[tokio::test]
async fn legacy_exams_backfill_to_published() {
    let (app, db) = common::app_and_db().await;
    let teacher_creds = json!({ "username": "ali", "password": "secret1" });
    let student_creds = json!({ "username": "ayse", "password": "secret1" });

    // A normal exam — then strip `draft` the way an old binary's schema would
    // have left it.
    for creds in [&teacher_creds, &student_creds] {
        assert_eq!(
            send(&app, "POST", "/auth/register", None, Some((*creds).clone()))
                .await
                .status,
            StatusCode::CREATED
        );
    }
    set_role(&db, "ali", "teacher").await;
    let teacher = send(
        &app,
        "POST",
        "/auth/login",
        None,
        Some(teacher_creds.clone()),
    )
    .await
    .cookie
    .unwrap();
    let student = send(
        &app,
        "POST",
        "/auth/login",
        None,
        Some(student_creds.clone()),
    )
    .await
    .cookie
    .unwrap();
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam_id = create_exam(&app, &teacher, &course, "midterm", "midterm").await;

    db.query(
        "REMOVE FIELD IF EXISTS draft ON TABLE exam;
         UPDATE exam SET draft = NONE;",
    )
    .await
    .expect("strip draft")
    .check()
    .expect("strip draft check");

    // Second boot re-runs the migration: the legacy exam reads back published
    // and the enrolled student still sees it.
    let app = reboot(&db).await;
    let student = send(&app, "POST", "/auth/login", None, Some(student_creds))
        .await
        .cookie
        .unwrap();
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam_id}"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["draft"], false);
    let listed = send(&app, "GET", "/exams", Some(&student), None).await;
    assert_eq!(common::items(&listed.body).len(), 1);
}
