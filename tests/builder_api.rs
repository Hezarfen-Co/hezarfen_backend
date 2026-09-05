//! The vendor surface (`web::builder`): the one principal that is not a user of
//! any school. Everything here is about the line between the two — a builder
//! cookie must never act inside a school, a school cookie must never manage
//! schools, and a session minted by `enter` must act in exactly one school.

mod common;

use axum::Router;
use axum::http::StatusCode;
use common::*;
use hezarfen_backend::domain::builder::Builder;
use hezarfen_backend::domain::user::{Password, Username};
use hezarfen_backend::tenant::{DEMO_SLUG, Tenants};
use serde_json::{Value, json};

const BUILDER_USER: &str = "operator";
const BUILDER_PASS: &str = "secret1";

/// A deployment with the demo school, plus a builder account to drive it —
/// production's `BUILDER_USERNAME`/`BUILDER_PASSWORD` seed, by the same path.
async fn deployment() -> (Router, hezarfen_backend::database::Database, Tenants) {
    let (app, db, tenants) = app_and_tenants().await;
    Builder::ensure(
        Username::try_new(BUILDER_USER).unwrap(),
        Password::try_new(BUILDER_PASS).unwrap(),
        tenants.control(),
    )
    .await
    .expect("seed the builder");
    (app, db, tenants)
}

async fn builder_login(app: &Router) -> String {
    let res = send(
        app,
        "POST",
        "/builder/login",
        None,
        Some(json!({ "username": BUILDER_USER, "password": BUILDER_PASS })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "builder login: {:?}", res.body);
    res.cookie.expect("builder cookie set on login")
}

/// Create a school as the builder (no assertion) — slug, name, first admin.
async fn create_school(app: &Router, cookie: &str, slug: &str, admin_pass: &str) -> Res {
    send(
        app,
        "POST",
        "/schools",
        Some(cookie),
        Some(json!({
            "slug": slug,
            "name": format!("{slug} school"),
            "admin_username": "admin",
            "admin_password": admin_pass,
        })),
    )
    .await
}

async fn school_login(app: &Router, slug: &str, username: &str, password: &str) -> Res {
    send(
        app,
        "POST",
        "/auth/login",
        None,
        Some(json!({ "school": slug, "username": username, "password": password })),
    )
    .await
}

fn slugs(body: &Value) -> Vec<String> {
    items(body)
        .iter()
        .map(|item| item["slug"].as_str().expect("slug").to_string())
        .collect()
}

/// The two cookies share a name and nothing else. This is the whole point of
/// the split: neither principal may ever be accepted where the other belongs.
#[tokio::test]
async fn a_builder_logs_in_and_its_cookie_works_here_and_nowhere_else() {
    let (app, _db, _tenants) = deployment().await;
    let builder = builder_login(&app).await;

    let me = send(&app, "GET", "/builder/me", Some(&builder), None).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.body["username"], BUILDER_USER);
    assert!(me.body["id"].as_str().is_some_and(|id| !id.is_empty()));

    // A school user's cookie is not a builder's…
    let student = login(&app, "ali").await;
    assert_eq!(
        send(&app, "GET", "/builder/me", Some(&student), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED,
        "a school cookie reached the builder surface"
    );
    assert_eq!(
        send(&app, "GET", "/schools", Some(&student), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    // …and a builder's is not a school user's.
    assert_eq!(
        send(&app, "GET", "/auth/me", Some(&builder), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED,
        "a builder cookie reached a school surface"
    );

    // Bad credentials, both shapes.
    for bad in [
        json!({ "username": BUILDER_USER, "password": "wrongpass" }),
        json!({ "username": "ghost", "password": BUILDER_PASS }),
    ] {
        assert_eq!(
            send(&app, "POST", "/builder/login", None, Some(bad))
                .await
                .status,
            StatusCode::UNAUTHORIZED
        );
    }

    let out = send(&app, "POST", "/builder/logout", Some(&builder), None).await;
    assert_eq!(out.status, StatusCode::NO_CONTENT);
    assert_eq!(
        send(&app, "GET", "/builder/me", Some(&builder), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED,
        "logout must revoke the session, not just clear the cookie"
    );
}

/// One call brings a whole school into being: registry row, database, schema,
/// and an admin who can actually log into it.
#[tokio::test]
async fn creating_a_school_seeds_an_admin_who_can_log_in() {
    let (app, _db, _tenants) = deployment().await;
    let builder = builder_login(&app).await;

    let created = create_school(&app, &builder, "ata-koleji", "secret1").await;
    assert_eq!(
        created.status,
        StatusCode::CREATED,
        "create school: {:?}",
        created.body
    );
    assert_eq!(created.body["slug"], "ata-koleji");
    assert_eq!(created.body["status"], "active");

    let login = school_login(&app, "ata-koleji", "admin", "secret1").await;
    assert_eq!(login.status, StatusCode::OK, "seeded admin logs in");
    let cookie = login.cookie.expect("school cookie");
    assert!(
        cookie.starts_with("session=ata-koleji."),
        "the cookie names its school: {cookie}"
    );
    let me = send(&app, "GET", "/auth/me", Some(&cookie), None).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.body["role"], "admin", "the seed is an admin, not a student");

    // The slug is the identity: a second school may not take it.
    assert_eq!(
        create_school(&app, &builder, "ata-koleji", "secret1")
            .await
            .status,
        StatusCode::CONFLICT
    );

    // Anything that is not a slug is refused before a database exists — the
    // reserved words most of all, since they name the builder cookie prefix and
    // the control database itself.
    for bad in ["control", "builder", "Ab", "a", &"x".repeat(33), "../x"] {
        let res = create_school(&app, &builder, bad, "secret1").await;
        assert_eq!(
            res.status,
            StatusCode::BAD_REQUEST,
            "slug {bad:?} was not refused: {:?}",
            res.body
        );
        // …and nothing was left behind by the refusal.
        assert_eq!(
            send(&app, "GET", &format!("/schools/{bad}"), Some(&builder), None)
                .await
                .status,
            StatusCode::NOT_FOUND,
            "slug {bad:?} left a school behind"
        );
    }
    // A blank name and a too-short admin password are refused the same way.
    for body in [
        json!({ "slug": "yeni", "name": "  ", "admin_username": "admin", "admin_password": "secret1" }),
        json!({ "slug": "yeni", "name": "Yeni", "admin_username": "admin", "admin_password": "x" }),
    ] {
        assert_eq!(
            send(&app, "POST", "/schools", Some(&builder), Some(body))
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        school_login(&app, "yeni", "admin", "secret1").await.status,
        StatusCode::UNAUTHORIZED,
        "a refused create must not have made a school"
    );
}

/// Listing, reading and patching the registry — and the suspension that closes
/// a school to its own users while the builder keeps managing it.
#[tokio::test]
async fn a_suspension_closes_the_school_and_a_resume_reopens_it() {
    let (app, _db, _tenants) = deployment().await;
    let builder = builder_login(&app).await;
    assert_eq!(
        create_school(&app, &builder, "beta", "secret1").await.status,
        StatusCode::CREATED
    );

    let list = send(&app, "GET", "/schools", Some(&builder), None).await;
    assert_eq!(list.status, StatusCode::OK);
    let listed = slugs(&list.body);
    assert!(listed.contains(&DEMO_SLUG.to_string()) && listed.contains(&"beta".to_string()));
    assert_eq!(total(&list.body), 2);
    // The envelope pages like every other list.
    let page = send(&app, "GET", "/schools?limit=1", Some(&builder), None).await;
    assert_eq!(items(&page.body).len(), 1);
    assert_eq!(total(&page.body), 2);

    assert_eq!(
        send(&app, "GET", "/schools/ghost", Some(&builder), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    // A partial patch touches only what it names: the name moves, the status
    // and the created stamp stay.
    let before = send(&app, "GET", "/schools/beta", Some(&builder), None).await;
    let renamed = send(
        &app,
        "PATCH",
        "/schools/beta",
        Some(&builder),
        Some(json!({ "name": "Beta Koleji" })),
    )
    .await;
    assert_eq!(renamed.status, StatusCode::OK);
    assert_eq!(renamed.body["name"], "Beta Koleji");
    assert_eq!(renamed.body["status"], "active");
    assert_eq!(renamed.body["slug"], "beta");
    assert_eq!(renamed.body["created_at"], before.body["created_at"]);
    // An empty patch is a no-op read, and a bad status is a 400.
    let untouched = send(
        &app,
        "PATCH",
        "/schools/beta",
        Some(&builder),
        Some(json!({})),
    )
    .await;
    assert_eq!(untouched.body["name"], "Beta Koleji");
    assert_eq!(
        send(
            &app,
            "PATCH",
            "/schools/beta",
            Some(&builder),
            Some(json!({ "status": "closed" })),
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );

    // A live cookie, taken out before the suspension.
    let live = school_login(&app, "beta", "admin", "secret1")
        .await
        .cookie
        .expect("school cookie");
    let suspended = send(
        &app,
        "PATCH",
        "/schools/beta",
        Some(&builder),
        Some(json!({ "status": "suspended" })),
    )
    .await;
    assert_eq!(suspended.status, StatusCode::OK);
    assert_eq!(suspended.body["status"], "suspended");

    assert_eq!(
        school_login(&app, "beta", "admin", "secret1").await.status,
        StatusCode::FORBIDDEN,
        "a suspended school refuses login"
    );
    assert_eq!(
        send(&app, "GET", "/auth/me", Some(&live), None).await.status,
        StatusCode::FORBIDDEN,
        "…and the cookie it had already issued, on its very next call"
    );
    // The builder still manages it — that is how it gets un-suspended.
    assert_eq!(
        send(&app, "GET", "/schools/beta", Some(&builder), None)
            .await
            .status,
        StatusCode::OK
    );
    // …except entering it, the one door a suspension must also close.
    assert_eq!(
        send(
            &app,
            "POST",
            "/schools/beta/enter",
            Some(&builder),
            Some(json!({ "username": "admin" })),
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    send(
        &app,
        "PATCH",
        "/schools/beta",
        Some(&builder),
        Some(json!({ "status": "active" })),
    )
    .await;
    assert_eq!(
        send(&app, "GET", "/auth/me", Some(&live), None).await.status,
        StatusCode::OK,
        "resuming restores the very same session"
    );
}

/// The lockout fix: re-key an admin, and take every session minted under the
/// old password with it.
#[tokio::test]
async fn resetting_an_admin_password_revokes_the_old_credential_and_its_sessions() {
    let (app, _db, _tenants) = deployment().await;
    let builder = builder_login(&app).await;
    create_school(&app, &builder, "gamma", "secret1").await;
    let old_cookie = school_login(&app, "gamma", "admin", "secret1")
        .await
        .cookie
        .expect("school cookie");

    // An account that is not an admin of that school, and one that is not there
    // at all, are told apart.
    assert_eq!(
        send(
            &app,
            "POST",
            "/schools/gamma/admin-password",
            Some(&builder),
            Some(json!({ "username": "ghost", "password": "secret2" })),
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    let gamma_db = _tenants
        .get(&hezarfen_backend::tenant::Slug::try_new("gamma").unwrap())
        .await
        .unwrap();
    hezarfen_backend::domain::user::User::create(
        Username::try_new("veli").unwrap(),
        Password::try_new("secret1").unwrap().hash_async().await.unwrap(),
        &gamma_db,
    )
    .await
    .unwrap();
    assert_eq!(
        send(
            &app,
            "POST",
            "/schools/gamma/admin-password",
            Some(&builder),
            Some(json!({ "username": "veli", "password": "secret2" })),
        )
        .await
        .status,
        StatusCode::CONFLICT,
        "a student is not the school's admin"
    );

    let reset = send(
        &app,
        "POST",
        "/schools/gamma/admin-password",
        Some(&builder),
        Some(json!({ "username": "admin", "password": "secret2" })),
    )
    .await;
    assert_eq!(reset.status, StatusCode::NO_CONTENT);

    assert_eq!(
        school_login(&app, "gamma", "admin", "secret1").await.status,
        StatusCode::UNAUTHORIZED,
        "the old password must be dead"
    );
    assert_eq!(
        school_login(&app, "gamma", "admin", "secret2").await.status,
        StatusCode::OK
    );
    assert_eq!(
        send(&app, "GET", "/auth/me", Some(&old_cookie), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED,
        "a reset that leaves the old cookie working resets nothing"
    );
}

/// `enter` mints an ordinary school session: full admin power inside that
/// school, none at all in the school next door.
#[tokio::test]
async fn entering_a_school_acts_in_that_school_and_no_other() {
    let (app, _db, _tenants) = deployment().await;
    let builder = builder_login(&app).await;
    create_school(&app, &builder, "alpha", "secret1").await;
    create_school(&app, &builder, "beta", "secret1").await;

    let entered = send(
        &app,
        "POST",
        "/schools/alpha/enter",
        Some(&builder),
        Some(json!({ "username": "admin" })),
    )
    .await;
    assert_eq!(entered.status, StatusCode::OK, "enter: {:?}", entered.body);
    assert_eq!(entered.body["username"], "admin");
    assert_eq!(entered.body["role"], "admin");
    let alpha = entered.cookie.expect("a school cookie");
    assert!(alpha.starts_with("session=alpha."), "{alpha}");

    // It is an ordinary school session — and only a school session.
    let me = send(&app, "GET", "/auth/me", Some(&alpha), None).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.body["role"], "admin");
    assert_eq!(
        send(&app, "GET", "/builder/me", Some(&alpha), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED,
        "entering a school must not hand back builder power"
    );

    // A row written in alpha is invisible from beta — the isolation the whole
    // per-school database exists for.
    let note = send(
        &app,
        "POST",
        "/notes",
        Some(&alpha),
        Some(json!({ "title": "alpha only", "content": "mine" })),
    )
    .await;
    assert_eq!(note.status, StatusCode::CREATED);
    let note_id = id_of(&note.body);
    let beta = send(
        &app,
        "POST",
        "/schools/beta/enter",
        Some(&builder),
        Some(json!({ "username": "admin" })),
    )
    .await
    .cookie
    .expect("beta cookie");
    assert_eq!(
        send(&app, "GET", &format!("/notes/{note_id}"), Some(&beta), None)
            .await
            .status,
        StatusCode::NOT_FOUND,
        "a note written in alpha was reachable under beta's cookie"
    );

    // Unknown school, unknown account, non-admin account.
    assert_eq!(
        send(
            &app,
            "POST",
            "/schools/ghost/enter",
            Some(&builder),
            Some(json!({ "username": "admin" })),
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            "POST",
            "/schools/alpha/enter",
            Some(&builder),
            Some(json!({ "username": "nobody" })),
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

/// Deleting a school takes its rows *and* its bytes — and only its own.
#[tokio::test]
async fn deleting_a_school_takes_its_data_and_its_files() {
    let (app, _db, _tenants) = deployment().await;
    let builder = builder_login(&app).await;
    create_school(&app, &builder, "delta", "secret1").await;
    create_school(&app, &builder, "epsilon", "secret1").await;

    // A note with a file in each school, so the delete has bytes to take.
    let mut blobs = Vec::new();
    for slug in ["delta", "epsilon"] {
        let cookie = school_login(&app, slug, "admin", "secret1")
            .await
            .cookie
            .expect("school cookie");
        let note = send(
            &app,
            "POST",
            "/notes",
            Some(&cookie),
            Some(json!({ "title": "with a file", "content": "x" })),
        )
        .await;
        assert_eq!(note.status, StatusCode::CREATED);
        let up = upload_file(
            &app,
            &cookie,
            &id_of(&note.body),
            "plan.pdf",
            "application/pdf",
            b"bytes",
        )
        .await;
        assert_eq!(up.status, StatusCode::CREATED, "upload: {:?}", up.body);
        // One directory per school under `FILES_PATH`, named by the slug.
        let blob = files_dir().join(slug).join(id_of(&up.body));
        assert!(blob.exists(), "{} should exist", blob.display());
        blobs.push(blob);
    }

    let deleted = send(&app, "DELETE", "/schools/delta", Some(&builder), None).await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);

    assert_eq!(
        send(&app, "GET", "/schools/delta", Some(&builder), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        school_login(&app, "delta", "admin", "secret1").await.status,
        StatusCode::UNAUTHORIZED,
        "a deleted school is an unknown school, never a 403"
    );
    assert!(
        !files_dir().join("delta").exists(),
        "the school's files subtree must be gone"
    );
    assert!(
        blobs[1].exists(),
        "the school next door must keep every byte it owns"
    );
    // Its neighbour is untouched all the way through the API.
    assert_eq!(
        school_login(&app, "epsilon", "admin", "secret1")
            .await
            .status,
        StatusCode::OK
    );
    // Deleting it again is a 404, not a second destruction.
    assert_eq!(
        send(&app, "DELETE", "/schools/delta", Some(&builder), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}
