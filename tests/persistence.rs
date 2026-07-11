//! File-backed engine tests: exercise the real surrealkv storage path (not the
//! in-memory engine used elsewhere) against a throwaway `tempfile` directory,
//! including surviving a full close + reopen.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::send;
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
        cookie_secure: false,
        rate_limit: RateLimitConfig::unlimited(),
    }
}

fn state(db: Database) -> AppState {
    AppState {
        db,
        cookie_secure: false,
        rate_limit: RateLimitConfig::unlimited(),
    }
}

/// The full flow works against the on-disk engine, same as in-memory.
#[tokio::test]
async fn file_engine_runs_full_flow() {
    let dir = tempfile::tempdir().unwrap();
    let db = database::init(&config_at(&dir))
        .await
        .expect("open file db");
    let app = build_router(state(db));

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
        send(&app, "GET", "/notes", Some(&cookie), None)
            .await
            .body
            .as_array()
            .unwrap()
            .len(),
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
        let app = build_router(state(db));
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
        let app = build_router(state(db));
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
        let rows = notes.body.as_array().unwrap();
        assert_eq!(rows.len(), 1, "note survived reopen");
        assert_eq!(rows[0]["title"], "persisted");
    }
}
