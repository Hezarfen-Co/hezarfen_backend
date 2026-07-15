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
