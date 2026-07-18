//! File-backed engine tests: exercise the real surrealkv storage path (not the
//! in-memory engine used elsewhere) against a throwaway `tempfile` directory,
//! including surviving a full close + reopen.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::{create_course, create_exam, enroll, me_id, send, set_role};
use hezarfen_backend::config::Config;
use hezarfen_backend::database::Database;
use hezarfen_backend::rate_limit::RateLimitConfig;
use hezarfen_backend::state::AppState;
use hezarfen_backend::{build_router, database};
use serde_json::json;
use tempfile::TempDir;

/// Re-open the on-disk database. surrealkv releases its file lock asynchronously
/// once the previous handle drops, so we retry briefly — a real restart is a new
/// process, which never contends.
async fn reopen(cfg: &Config) -> Database {
    for _ in 0..100 {
        match database::init(cfg).await {
            Ok(db) => return db,
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    database::init(cfg).await.expect("reopen after retries")
}

fn config_at(dir: &TempDir) -> Config {
    Config {
        host: "127.0.0.1".into(),
        port: 0,
        db_path: dir.path().join("hezarfen.db").to_str().unwrap().to_string(),
        db_ns: "hezarfen".into(),
        db_name: "hezarfen".into(),
        files_path: dir.path().join("files").to_str().unwrap().to_string(),
        cookie_secure: false,
        rate_limit: RateLimitConfig::unlimited(),
        admin_username: None,
        admin_password: None,
    }
}

/// Mirror `main`: the blob directory is created at boot, beside the database.
fn state(db: Database, cfg: &Config) -> AppState {
    std::fs::create_dir_all(&cfg.files_path).expect("files dir");
    AppState {
        db,
        files_path: cfg.files_path.clone().into(),
        cookie_secure: false,
        rate_limit: RateLimitConfig::unlimited(),
        exam_presence: Default::default(),
    }
}

/// The full flow works against the on-disk engine, same as in-memory.
#[tokio::test]
async fn file_engine_runs_full_flow() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config_at(&dir);
    let db = database::init(&cfg).await.expect("open file db");
    let app = build_router(state(db, &cfg));

    let creds = json!({ "username": "ali", "password": "secret1" });
    assert_eq!(
        send(&app, "POST", "/auth/register", None, Some(creds.clone()))
            .await
            .status,
        StatusCode::CREATED
    );
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds))
        .await
        .cookie
        .unwrap();

    let note = send(
        &app,
        "POST",
        "/notes",
        Some(&cookie),
        Some(json!({ "title": "disk" })),
    )
    .await;
    assert_eq!(note.status, StatusCode::CREATED);
    assert_eq!(
        common::items(&send(&app, "GET", "/notes", Some(&cookie), None).await.body,).len(),
        1
    );
}

/// Data written in one process lifetime is still readable after closing and
/// re-opening the database at the same path (list endpoints do table scans).
#[tokio::test]
async fn data_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config_at(&dir);
    let creds = json!({ "username": "ali", "password": "secret1" });

    // First boot: create a user and a note, then drop the handle.
    {
        let db = database::init(&cfg).await.expect("first open");
        let app = build_router(state(db, &cfg));
        assert_eq!(
            send(&app, "POST", "/auth/register", None, Some(creds.clone()))
                .await
                .status,
            StatusCode::CREATED
        );
        let cookie = send(&app, "POST", "/auth/login", None, Some(creds.clone()))
            .await
            .cookie
            .unwrap();
        assert_eq!(
            send(
                &app,
                "POST",
                "/notes",
                Some(&cookie),
                Some(json!({ "title": "persisted" }))
            )
            .await
            .status,
            StatusCode::CREATED
        );
    }

    // Second boot from the same path: the user and note are still there.
    {
        let db = reopen(&cfg).await;
        let app = build_router(state(db, &cfg));
        let login = send(&app, "POST", "/auth/login", None, Some(creds)).await;
        assert_eq!(login.status, StatusCode::OK, "user survived reopen");
        let cookie = login.cookie.unwrap();
        let notes = send(&app, "GET", "/notes", Some(&cookie), None).await;
        assert_eq!(
            notes.status,
            StatusCode::OK,
            "notes list body: {}",
            notes.body
        );
        let rows = common::items(&notes.body);
        assert_eq!(rows.len(), 1, "note survived reopen");
        assert_eq!(rows[0]["title"], "persisted");
    }
}

/// A note's uploaded file — its metadata row in the database and its blob on
/// disk — comes back after a close + reopen, byte for byte.
#[tokio::test]
async fn note_files_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config_at(&dir);
    let creds = json!({ "username": "ali", "password": "secret1" });
    let bytes = b"%PDF-1.4 persisted".to_vec();
    let file_uri;

    // First boot: create a note and upload a file onto it.
    {
        let db = database::init(&cfg).await.expect("first open");
        let app = build_router(state(db, &cfg));
        send(&app, "POST", "/auth/register", None, Some(creds.clone())).await;
        let cookie = send(&app, "POST", "/auth/login", None, Some(creds.clone()))
            .await
            .cookie
            .unwrap();
        let note = send(
            &app,
            "POST",
            "/notes",
            Some(&cookie),
            Some(json!({ "title": "with attachment" })),
        )
        .await;
        let note_id = note.body["id"].as_str().unwrap().to_string();
        let up = common::upload_file(
            &app,
            &cookie,
            &note_id,
            "plan.pdf",
            "application/pdf",
            &bytes,
        )
        .await;
        assert_eq!(up.status, StatusCode::CREATED, "{}", up.body);
        file_uri = format!("/notes/{note_id}/files/{}", up.body["id"].as_str().unwrap());
    }

    // Second boot: metadata and bytes are both still there.
    {
        let db = reopen(&cfg).await;
        let app = build_router(state(db, &cfg));
        let cookie = send(&app, "POST", "/auth/login", None, Some(creds))
            .await
            .cookie
            .unwrap();
        let (status, headers, body) =
            common::send_raw(&app, "GET", &file_uri, Some(&cookie), None, Vec::new()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, bytes, "blob bytes survived reopen");
        assert_eq!(headers["content-type"], "application/pdf");
    }
}

/// Personal info written through `PATCH /users/me` is still on the account
/// after a close + reopen.
#[tokio::test]
async fn profile_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config_at(&dir);
    let creds = json!({ "username": "ali", "password": "secret1" });

    // First boot: register and fill in the profile.
    {
        let db = database::init(&cfg).await.expect("first open");
        let app = build_router(state(db, &cfg));
        assert_eq!(
            send(&app, "POST", "/auth/register", None, Some(creds.clone()))
                .await
                .status,
            StatusCode::CREATED
        );
        let cookie = send(&app, "POST", "/auth/login", None, Some(creds.clone()))
            .await
            .cookie
            .unwrap();
        let res = send(
            &app,
            "PATCH",
            "/users/me",
            Some(&cookie),
            Some(json!({
                "name": "Ali",
                "surname": "Gümüş",
                "email": "ali@example.com",
                "phone": "+90 555 123 45 67",
                "birth_date": "1990-01-02",
            })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK);
    }

    // Second boot: the info comes back from disk.
    {
        let db = reopen(&cfg).await;
        let app = build_router(state(db, &cfg));
        let cookie = send(&app, "POST", "/auth/login", None, Some(creds))
            .await
            .cookie
            .unwrap();
        let me = send(&app, "GET", "/auth/me", Some(&cookie), None).await;
        assert_eq!(me.status, StatusCode::OK);
        assert_eq!(me.body["name"], "Ali");
        assert_eq!(me.body["surname"], "Gümüş");
        assert_eq!(me.body["email"], "ali@example.com");
        assert_eq!(me.body["phone"], "+90 555 123 45 67");
        assert_eq!(me.body["birth_date"], "1990-01-02");
    }
}

/// UI preferences written through `PATCH /users/me/preferences` are still on
/// the account after a close + reopen.
#[tokio::test]
async fn preferences_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config_at(&dir);
    let creds = json!({ "username": "ali", "password": "secret1" });

    // First boot: register and choose a theme + language.
    {
        let db = database::init(&cfg).await.expect("first open");
        let app = build_router(state(db, &cfg));
        assert_eq!(
            send(&app, "POST", "/auth/register", None, Some(creds.clone()))
                .await
                .status,
            StatusCode::CREATED
        );
        let cookie = send(&app, "POST", "/auth/login", None, Some(creds.clone()))
            .await
            .cookie
            .unwrap();
        let res = send(
            &app,
            "PATCH",
            "/users/me/preferences",
            Some(&cookie),
            Some(json!({ "theme": "dark", "language": "tr" })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK);
    }

    // Second boot: the choices come back from disk.
    {
        let db = reopen(&cfg).await;
        let app = build_router(state(db, &cfg));
        let cookie = send(&app, "POST", "/auth/login", None, Some(creds))
            .await
            .cookie
            .unwrap();
        let me = send(&app, "GET", "/auth/me", Some(&cookie), None).await;
        assert_eq!(me.status, StatusCode::OK);
        assert_eq!(me.body["theme"], "dark");
        assert_eq!(me.body["language"], "tr");
    }
}

/// A course with an enrollment, an exam whose kind carries a weight, and a
/// graded mark survives a close + reopen — the weighted report (settings
/// included) is rebuilt from disk.
#[tokio::test]
async fn course_marks_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config_at(&dir);
    let teacher_creds = json!({ "username": "hoca", "password": "secret1" });
    let student_creds = json!({ "username": "ali", "password": "secret1" });

    // First boot: the manager-teacher weighs midterms double, sets up a
    // course, enrolls the student, grades 80.
    {
        let db = database::init(&cfg).await.expect("first open");
        let app = build_router(state(db.clone(), &cfg));
        for creds in [&teacher_creds, &student_creds] {
            assert_eq!(
                send(&app, "POST", "/auth/register", None, Some((*creds).clone()))
                    .await
                    .status,
                StatusCode::CREATED
            );
        }
        set_role(&db, "hoca", "manager").await;
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

        assert_eq!(
            send(
                &app,
                "PATCH",
                "/settings",
                Some(&teacher),
                Some(json!({ "exam_kinds": [
                    {"name": "midterm", "weight": 2},
                    {"name": "quiz", "weight": 1},
                ]})),
            )
            .await
            .status,
            StatusCode::OK
        );
        let course_id = create_course(&app, &teacher, "algebra").await;
        enroll(&app, &teacher, &course_id, &student_id).await;
        let exam_id = create_exam(&app, &teacher, &course_id, "midterm", "midterm").await;
        assert_eq!(
            send(
                &app,
                "POST",
                &format!("/exams/{exam_id}/results"),
                Some(&teacher),
                Some(json!({ "mark": 80, "user_id": student_id }))
            )
            .await
            .status,
            StatusCode::OK
        );
    }

    // Second boot: the report, roster, and kind weight (via the settings row)
    // are all rebuilt from disk.
    {
        let db = reopen(&cfg).await;
        let app = build_router(state(db, &cfg));
        let student = send(&app, "POST", "/auth/login", None, Some(student_creds))
            .await
            .cookie
            .unwrap();
        let report = send(&app, "GET", "/marks/me", Some(&student), None).await;
        assert_eq!(report.status, StatusCode::OK);
        let course = &report.body["courses"][0];
        assert_eq!(course["course"]["title"], "algebra");
        assert_eq!(course["average"], 80.0);
        assert_eq!(course["results"][0]["weight"], 2);
        assert_eq!(course["results"][0]["mark"], 80);
        assert_eq!(report.body["overall_average"], 80.0);

        let teacher = send(&app, "POST", "/auth/login", None, Some(teacher_creds))
            .await
            .cookie
            .unwrap();
        let course_id = course["course"]["id"].as_str().unwrap();
        let roster = send(
            &app,
            "GET",
            &format!("/courses/{course_id}/enrollments"),
            Some(&teacher),
            None,
        )
        .await;
        assert_eq!(common::items(&roster.body).len(), 1, "roster intact");
    }
}

/// School policy and terms survive a close + reopen — the settings singleton
/// (nested band objects included) and the course→term link both come back,
/// and the second boot's idempotent migration doesn't disturb them.
#[tokio::test]
async fn settings_and_terms_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config_at(&dir);

    let term;
    {
        let db = database::init(&cfg).await.expect("open file db");
        let app = build_router(state(db.clone(), &cfg));

        let creds = json!({ "username": "boss", "password": "secret1" });
        send(&app, "POST", "/auth/register", None, Some(creds.clone())).await;
        set_role(&db, "boss", "manager").await;
        let cookie = send(&app, "POST", "/auth/login", None, Some(creds))
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
        term = res.body["id"].as_str().unwrap().to_string();

        let res = send(
            &app,
            "POST",
            "/courses",
            Some(&cookie),
            Some(json!({ "title": "History", "term_id": term })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED);
    }

    // New handle at the same path — a fresh boot, migration re-applied.
    let db = reopen(&cfg).await;
    let app = build_router(state(db, &cfg));
    let creds = json!({ "username": "boss", "password": "secret1" });
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
    let dir = tempfile::tempdir().unwrap();
    let cfg = config_at(&dir);
    let creds = json!({ "username": "ali", "password": "secret1" });
    let event_id;

    // First boot: a normal event — then strip its audience the way an old
    // binary's schema would have left it: drop the column definitions so
    // SCHEMAFULL stops enforcing them, and unset the field on the row.
    {
        let db = database::init(&cfg).await.expect("first open");
        let app = build_router(state(db.clone(), &cfg));
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
        event_id = common::id_of(&res.body);

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
    }

    // Second boot re-runs the migration: the field definitions come back and
    // the backfill stamps the legacy row school-wide — it reads, lists, and
    // rosters like any new event.
    let db = reopen(&cfg).await;
    let app = build_router(state(db, &cfg));
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
    let dir = tempfile::tempdir().unwrap();
    let cfg = config_at(&dir);
    let creds = json!({ "username": "ali", "password": "secret1" });
    let event_id;
    let student_id;

    // First boot: a registration-audience event — then rewrite it into the
    // shape an old binary left behind: restore the `audience.users` column
    // definition and store a hand-picked list on the row.
    {
        let db = database::init(&cfg).await.expect("first open");
        let app = build_router(state(db.clone(), &cfg));
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
        student_id = common::me_id(&app, &student_cookie).await;

        let res = send(
            &app,
            "POST",
            "/events",
            Some(&cookie),
            Some(json!({ "title": "trip", "audience": { "kind": "registration" } })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED);
        event_id = common::id_of(&res.body);

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
    }

    // Second boot runs the conversion: the audience reads back as an uncapped
    // registration list, the listed student holds a seat credited to the
    // creator, and the roster (and marking gate) see them.
    let db = reopen(&cfg).await;
    let app = build_router(state(db, &cfg));
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
    let dir = tempfile::tempdir().unwrap();
    let cfg = config_at(&dir);
    let creds = json!({ "username": "ali", "password": "secret1" });
    let exam_id;
    let subject_id;

    // First boot: a full exam — course, subject, open exam, one answered
    // question — then strip the question's subject the way an old binary's
    // schema would have left it.
    {
        let db = database::init(&cfg).await.expect("first open");
        let app = build_router(state(db.clone(), &cfg));
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
        subject_id = common::id_of(&res.body);
        let res = send(
            &app,
            "POST",
            &format!("/courses/{course}/exams"),
            Some(&cookie),
            Some(json!({ "title": "drill", "kind": "quiz", "mode": "open" })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED);
        exam_id = common::id_of(&res.body);
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
    }

    // Second boot re-runs the migration and the destroy-backfill: the
    // subjectless question and its answer are gone, the exam and subject
    // still stand.
    let db = reopen(&cfg).await;
    let app = build_router(state(db.clone(), &cfg));
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
    let dir = tempfile::tempdir().unwrap();
    let cfg = config_at(&dir);
    let creds = json!({ "username": "ali", "password": "secret1" });
    let course;
    let keeper_id;

    // First boot: a course with three enrolled students, then age two rows the
    // way a pre-fix binary could have — flip one student to teacher directly
    // in the DB, and delete the other's user record entirely.
    {
        let db = database::init(&cfg).await.expect("first open");
        let app = build_router(state(db.clone(), &cfg));
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
        course = create_course(&app, &cookie, "algebra").await;

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
        keeper_id = ids[1].clone();
        set_role(&db, "veli", "teacher").await;
        db.query("DELETE user WHERE username = 'can'")
            .await
            .expect("delete user")
            .check()
            .expect("delete user check");
    }

    // Second boot: the backfill swept the promoted user's row and the deleted
    // user's dangling row; the remaining student's enrollment is intact.
    let db = reopen(&cfg).await;
    let app = build_router(state(db, &cfg));
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
