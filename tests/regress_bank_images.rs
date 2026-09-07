//! A bank template's images must never outlive the template, and every blob
//! must have a row pointing at it.
//!
//! `bank_question_image` was the one child in this domain whose write did not
//! touch its parent, and `BankQuestion::delete` the one cascade here that was
//! not a single transaction. Between them an upload that started before a
//! delete and landed after it left a row *and* a blob nothing could ever read,
//! delete, or sweep: every image route goes through the template, and no boot
//! sweep visits this table. The third hole was one round trip wide — the slot's
//! old blob name was read *before* the write, so two uploads to one slot both
//! retired the same old blob and one of the two fresh ones was left on disk
//! with nothing pointing at it.
//!
//! Stored state is the whole verdict here, on both sides: the row (is the
//! orphan there?) and the disk (is the blob still in `files_path`?). Each test
//! gets a blob directory of its own so a file count means something.
//!
//! These two need no server, so they run in CI by default. The *raced* halves
//! of the same three holes live beside the code they test, in
//! `bank_question_image`'s own test module, where `init_test_server` and
//! `RACE_LOCK` are in reach: the store's conflict detection is their subject,
//! and re-spelling the real bootstrap out here would be a hand-kept copy that
//! drifts silently the first time migration or router construction changes.

mod common;

use axum::Router;
use axum::http::StatusCode;
use common::{create_course, create_subject, id_of, login_as, send};
use hezarfen_backend::build_router;
use hezarfen_backend::database::Database;
use hezarfen_backend::domain::bank_question::BankQuestionId;
use hezarfen_backend::domain::bank_question_image::BankQuestionImage;
use hezarfen_backend::domain::note_file::FileContentType;
use hezarfen_backend::error::AppError;
use hezarfen_backend::rate_limit::RateLimitConfig;
use hezarfen_backend::state::AppState;
use serde_json::json;
use tempfile::TempDir;

const BOUNDARY: &str = "multipart/form-data; boundary=hezarfen-test-boundary";

/// A router over a fresh in-memory database, with a blob directory nothing else
/// writes to — so `blobs()` counts this test's files and no one else's.
async fn mem_app() -> (Router, Database, TempDir) {
    let (tenants, db) = common::mem_deployment().await;
    let files = tempfile::tempdir().expect("files tempdir");
    let app = build_router(AppState {
        db: tenants.control().clone(),
        tenants,
        files_path: files.path().to_path_buf(),
        cookie_secure: false,
        rate_limit: RateLimitConfig::unlimited(),
        chatbot_limit: Default::default(),
        exam_presence: Default::default(),
        board_hub: Default::default(),
        db_up: Default::default(),
        ai: None,
        metrics: hezarfen_backend::telemetry::Metrics::noop(),
    });
    (app, db, files)
}

/// Every blob file currently on disk, by name.
fn blobs(files: &TempDir) -> Vec<String> {
    // Blobs live under the school's own subdirectory of `FILES_PATH`.
    let dir = files.path().join(hezarfen_backend::tenant::DEMO_SLUG);
    if !dir.exists() {
        return Vec::new();
    }
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("read the blob dir")
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A template owned by `who`, under a fresh course + subject; returns its id.
async fn template(app: &Router, who: &str, name: &str) -> String {
    let course = create_course(app, who, &format!("Ders {name}")).await;
    let subject = create_subject(app, who, &course, name).await;
    let res = send(
        app,
        "POST",
        "/bank-questions",
        Some(who),
        Some(json!({ "subject_id": subject, "text": "soru", "kind": "text", "points": 3 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    id_of(&res.body)
}

/// Upload an illustration onto the template; returns the status.
async fn upload(app: &Router, who: &str, bid: &str, bytes: &[u8]) -> StatusCode {
    let (status, _, _) = common::send_raw(
        app,
        "POST",
        &format!("/bank-questions/{bid}/image"),
        Some(who),
        Some(BOUNDARY),
        common::multipart_file("pic.png", "image/png", bytes),
    )
    .await;
    status
}

fn png() -> FileContentType {
    FileContentType::try_new("image/png").unwrap()
}

/// The template's existence is part of the image write itself, not a read in
/// front of it: writing a slot under a template that is gone is refused, and
/// stores nothing. Before the fix this was a bare `UPSERT` that cheerfully
/// created the row — a row no route could reach afterwards, since every image
/// endpoint resolves the template first, and no boot sweep touches this table.
///
/// Deterministic, no race needed: the parent is already gone. The *raced*
/// version of the same hole is
/// `an_upload_inside_a_delete_window_leaves_no_orphan_row_or_blob` below.
#[tokio::test]
async fn an_image_write_under_a_missing_template_is_refused() {
    let (app, db, _files) = mem_app().await;
    let teacher = login_as(&app, &db, "ogretmen_banka", "teacher").await;
    let bid = template(&app, &teacher, "optik").await;

    let dropped = send(
        &app,
        "DELETE",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(dropped.status, StatusCode::NO_CONTENT);

    let question = BankQuestionId::from_key(&bid);
    let refused = BankQuestionImage::new(&question, None, png(), 3)
        .upsert(&db)
        .await;
    assert!(
        matches!(refused, Err(AppError::NotFound)),
        "a slot write under a deleted template must be refused: {refused:?}"
    );
    // Stored state is the verdict — a refusal that still wrote the row would be
    // the very defect this pins.
    assert!(
        BankQuestionImage::list_for_question(&question, &db)
            .await
            .unwrap()
            .is_empty(),
        "the refused write left an orphan row"
    );
}

/// The disk contract, end to end: a replace retires exactly the blob it
/// replaced (never the fresh one), and deleting the template takes the blobs of
/// the rows its cascade *swept* off disk with it.
#[tokio::test]
async fn a_replace_retires_one_blob_and_a_delete_takes_the_rest() {
    let (app, db, files) = mem_app().await;
    let teacher = login_as(&app, &db, "ogretmen_disk", "teacher").await;
    let bid = template(&app, &teacher, "akustik").await;

    assert_eq!(
        upload(&app, &teacher, &bid, b"first").await,
        StatusCode::CREATED
    );
    assert_eq!(blobs(&files).len(), 1, "the first upload is on disk");

    assert_eq!(
        upload(&app, &teacher, &bid, b"second").await,
        StatusCode::CREATED
    );
    let after = blobs(&files);
    assert_eq!(
        after.len(),
        1,
        "a replace retires the blob it replaced: {after:?}"
    );
    let question = BankQuestionId::from_key(&bid);
    let rows = BankQuestionImage::list_for_question(&question, &db)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "one row per slot");
    assert_eq!(
        after,
        vec![rows[0].get_file().to_string()],
        "the surviving blob is the one the row points at"
    );

    let dropped = send(
        &app,
        "DELETE",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(dropped.status, StatusCode::NO_CONTENT);
    assert!(
        blobs(&files).is_empty(),
        "the delete left a blob behind: {:?}",
        blobs(&files)
    );
    assert!(
        BankQuestionImage::list_for_question(&question, &db)
            .await
            .unwrap()
            .is_empty()
    );
}
