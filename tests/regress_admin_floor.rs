//! Regressions for the two holes the 2026-08-05 hunt left standing on the
//! role/parent-link surface: two admins demoting each other could empty the
//! admin set for good, and a parent's student list named accounts that had
//! since been promoted out of `student`.
//!
//! Every assertion re-reads the *store*. The in-memory engine forges wins under
//! concurrency (src/domain/cap.rs), so the race below is judged on how many
//! admins survive, never on which request answered what.

mod common;

use axum::http::StatusCode;
use common::{app_and_db, login_as, me_id, send};
use hezarfen_backend::database::Database;
use hezarfen_backend::domain::role::Role;
use hezarfen_backend::domain::user::UserId;
use serde_json::json;

/// How many accounts hold `admin` right now, out of the store.
async fn admin_count(db: &Database) -> usize {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM app_user WHERE role = 'admin'")
        .fetch_one(db)
        .await
        .unwrap() as usize
}

/// Two admins demote each other at the same instant. Both `RequireAdmin`
/// extractors resolve against the live rows before either write lands, so
/// before the floor guard both writes committed and the school was left with
/// **zero** admins — unrecoverable, since the boot seed refuses to promote an
/// existing non-admin row.
#[tokio::test]
async fn two_admins_demoting_each_other_leave_one() {
    let (app, db) = app_and_db().await;
    let ada = login_as(&app, &db, "ada", "admin").await;
    let bora = login_as(&app, &db, "bora", "admin").await;
    let ada_id = me_id(&app, &ada).await;
    let bora_id = me_id(&app, &bora).await;
    assert_eq!(admin_count(&db).await, 2);

    let (one, two) = {
        let (app_a, app_b) = (app.clone(), app.clone());
        let demote = |app: axum::Router, cookie: String, target: String| async move {
            send(
                &app,
                "PATCH",
                &format!("/users/{target}/role"),
                Some(&cookie),
                Some(json!({ "role": "student" })),
            )
            .await
            .status
        };
        tokio::join!(
            tokio::spawn(demote(app_a, ada.clone(), bora_id)),
            tokio::spawn(demote(app_b, bora.clone(), ada_id)),
        )
    };
    let (one, two) = (one.unwrap(), two.unwrap());

    // The stored state is the invariant — not who was told they won.
    assert!(
        admin_count(&db).await >= 1,
        "both demotions landed and the school has no admin left ({one}, {two})"
    );
}

/// The floor itself, driven where it is deterministic. Over HTTP the `409` is
/// reachable only through the race above (demoting an admin needs a *second*
/// admin logged in, so sequentially the target is never the last one), which is
/// exactly why the guard has to live in the writer rather than the handler.
#[tokio::test]
async fn set_role_refuses_the_last_admin() {
    let (app, db) = app_and_db().await;
    let ada = login_as(&app, &db, "ada", "admin").await;
    let ada_id = UserId::from_key(&me_id(&app, &ada).await);

    hezarfen_backend::db::user::read(&db, &ada_id).await.unwrap().unwrap();
    let refused = hezarfen_backend::service::user::set_role(&db, &ada_id, Role::Student).await;
    assert!(refused.is_err(), "the sole admin was demoted");
    assert_eq!(admin_count(&db).await, 1, "the admin row was still lowered");

    // Promotion is never refused, and the floor lifts once a second admin is in
    // place: the guard is "keep one", not "freeze the set".
    let bora = login_as(&app, &db, "bora", "student").await;
    let bora_id = me_id(&app, &bora).await;
    let res = send(
        &app,
        "PATCH",
        &format!("/users/{bora_id}/role"),
        Some(&ada),
        Some(json!({ "role": "admin" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    hezarfen_backend::db::user::read(&db, &ada_id).await.unwrap().unwrap();
    assert!(
        hezarfen_backend::service::user::set_role(&db, &ada_id, Role::Student)
            .await
            .is_ok()
    );
    assert_eq!(admin_count(&db).await, 1);
}

/// The self-demotion door, which the floor does not replace: an admin never
/// changes their own role, whether or not others exist.
#[tokio::test]
async fn admin_cannot_demote_themselves() {
    let (app, db) = app_and_db().await;
    let ada = login_as(&app, &db, "ada", "admin").await;
    login_as(&app, &db, "bora", "admin").await;
    let ada_id = me_id(&app, &ada).await;

    let res = send(
        &app,
        "PATCH",
        &format!("/users/{ada_id}/role"),
        Some(&ada),
        Some(json!({ "role": "student" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    assert_eq!(admin_count(&db).await, 2);
}

/// A `parent_link` whose student was promoted out — the row a lost sweep race
/// leaves behind, and the row an older build may already have on disk. Written
/// straight into the store (the role change is applied without `set_role`'s
/// sweep, which is precisely the state the race produces) and then read back
/// through both list handlers.
#[tokio::test]
async fn a_promoted_student_leaves_the_parent_list() {
    let (app, db) = app_and_db().await;
    let ada = login_as(&app, &db, "ada", "admin").await;
    let veli = login_as(&app, &db, "veli", "parent").await;
    let can = login_as(&app, &db, "can", "student").await;
    let veli_id = me_id(&app, &veli).await;
    let can_id = me_id(&app, &can).await;

    let res = send(
        &app,
        "POST",
        &format!("/users/{veli_id}/students"),
        Some(&ada),
        Some(json!({ "user_id": can_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let listed = send(
        &app,
        "GET",
        &format!("/users/{veli_id}/students"),
        Some(&ada),
        None,
    )
    .await;
    assert_eq!(listed.body["total"], 1, "{}", listed.body);

    common::set_role(&db, "can", "teacher").await;
    assert_eq!(
        rows("SELECT parent FROM parent_link", &db).await,
        1,
        "the stale link is the premise of this test"
    );

    for (uri, cookie) in [
        (format!("/users/{veli_id}/students"), &ada),
        ("/users/me/students".to_string(), &veli),
    ] {
        let res = send(&app, "GET", &uri, Some(cookie), None).await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        assert_eq!(res.body["total"], 0, "{uri}: {}", res.body);
        assert_eq!(
            res.body["items"].as_array().unwrap().len(),
            0,
            "{uri}: {}",
            res.body
        );
    }
}

/// Ids `sql` selects.
async fn rows(sql: &'static str, db: &Database) -> usize {
    sqlx::query_as::<_, (uuid::Uuid,)>(sql)
        .fetch_all(db)
        .await
        .unwrap()
        .len()
}
