//! Whiteboard defects that must stay fixed: the REST canvas agreeing with the
//! room's socket about `clear` markers, and the lock holding against its own
//! creator's clear.

mod common;

use axum::http::StatusCode;
use common::{app_and_db, items, login, me_id, send};
use hezarfen_backend::domain::board::BoardId;
use hezarfen_backend::domain::user::UserId;
use serde_json::json;

async fn a_board(app: &axum::Router, session: &str) -> String {
    let res = send(
        app,
        "POST",
        "/boards",
        Some(session),
        Some(json!({ "title": "Tahta" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    res.body["id"].as_str().unwrap().to_string()
}

/// `GET /strokes` scopes to the board's current epoch, which by construction
/// holds no `clear` marker — except when a clear commits between the board read
/// and the row read and files its marker under the epoch just named. The room's
/// socket filters markers out and never shows it, so an unfiltered page made
/// the two views disagree in exactly that race. The race is pinned here by
/// leaving a marker in the epoch the board still points at.
#[tokio::test]
async fn a_strokes_page_never_carries_a_clear_marker() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;
    let board = a_board(&app, &ali).await;

    for n in 0..2 {
        hezarfen_backend::db::board_stroke::append(
            &db,
            &BoardId::from_key(&board),
            &UserId::from_key(&ali_id),
            &format!("{{\"p\":[0,{n}]}}"),
            0,
        )
        .await
        .expect("append");
    }
    let res = send(
        &app,
        "POST",
        &format!("/boards/{board}/clear"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    // The clear left a marker on epoch 0 and moved the board to epoch 1. Point
    // the board back at 0: that is precisely what a reader that read the board
    // one instant before the clear committed is looking at.
    sqlx::query("UPDATE board SET epoch = 0 WHERE id = $1")
        .bind(BoardId::from_key(&board))
        .execute(&db)
        .await
        .expect("rewind");

    let res = send(
        &app,
        "GET",
        &format!("/boards/{board}/strokes"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let page = items(&res.body);
    assert_eq!(page.len(), 2, "the two marks, and only them: {}", res.body);
    assert_eq!(
        res.body["total"], 2,
        "the envelope's total counts what is served: {}",
        res.body
    );
    assert!(
        page.iter().all(|row| row["kind"] == "stroke"),
        "a clear marker reached the live canvas: {}",
        res.body
    );

    // The markers are not lost — /history and /epochs are where they live.
    let res = send(
        &app,
        "GET",
        &format!("/boards/{board}/history"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(
        items(&res.body)
            .iter()
            .filter(|row| row["kind"] == "clear")
            .count(),
        1,
        "{}",
        res.body
    );
}

/// The lock pauses the canvas against every write to it, the creator's own
/// clear included: a `409`, not a `403` — a lock is a state the caller can
/// undo, while the `403` on this route means "not your board". The accepted
/// cost is the recovery: unlock, clear, relock.
#[tokio::test]
async fn a_locked_board_refuses_the_creator_s_clear_with_409() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;
    let board = a_board(&app, &ali).await;
    hezarfen_backend::db::board_stroke::append(
        &db,
        &BoardId::from_key(&board),
        &UserId::from_key(&ali_id),
        "{\"p\":[1,2]}",
        0,
    )
    .await
    .expect("append");

    let lock = async |locked: bool| {
        send(
            &app,
            "PATCH",
            &format!("/boards/{board}"),
            Some(&ali),
            Some(json!({ "locked": locked })),
        )
        .await
    };
    let res = lock(true).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let res = send(
        &app,
        "POST",
        &format!("/boards/{board}/clear"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    // Nothing was minted and the epoch did not move.
    let res = send(
        &app,
        "GET",
        &format!("/boards/{board}/epochs"),
        Some(&ali),
        None,
    )
    .await;
    assert!(items(&res.body).is_empty(), "{}", res.body);
    let res = send(&app, "GET", &format!("/boards/{board}"), Some(&ali), None).await;
    assert_eq!(res.body["epoch"], 0, "{}", res.body);

    // Unlocked, the same clear lands.
    let res = lock(false).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/boards/{board}/clear"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["count"], 1, "{}", res.body);
}
