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

// ---------------------------------------------------------------------------
// REFUTE-mode probes on the irreversible paths (appended by a verifier).
// ---------------------------------------------------------------------------

use hezarfen_backend::tenant::Slug;

/// Claim 1: no `/schools/{slug}` route can be aimed outside `FILES_PATH/<slug>`.
/// Every hostile segment on every method, with canaries inside and outside the
/// deployment's files root.
#[tokio::test]
async fn probe_hostile_slug_segments_never_delete_anything() {
    let (app, _db, _tenants) = deployment().await;
    let builder = builder_login(&app).await;

    // A school that must survive all of it, with real bytes on disk.
    create_school(&app, &builder, "p1keep", "secret1").await;
    let keep_dir = files_dir().join("p1keep");
    std::fs::create_dir_all(&keep_dir).unwrap();
    std::fs::write(keep_dir.join("keep.bin"), b"keep").unwrap();
    // A canary at the files root itself: an escape by one `..` takes this.
    let root_canary = files_dir().join("p1_root_canary");
    std::fs::write(&root_canary, b"canary").unwrap();
    // …and one fully outside the deployment root.
    let outside = tempfile::tempdir().unwrap();
    let outside_file = outside.path().join("outside.bin");
    std::fs::write(&outside_file, b"outside").unwrap();

    let hostile = [
        "..",
        "%2e%2e",
        "%2E%2E%2F%2E%2E",
        "..%2f..%2fetc",
        "%2f",
        "p1keep%2f..",
        "p1keep..",
        ".",
        "%00",
        "DEMO",
        "Demo",
        "control",
        "builder",
        "%C4%B1demo",              // dotless i, non-ascii
        "d%E2%80%8Bemo",           // zero-width space
        "de%20mo",
        "a",
        "-lead",
        "demo%2e%2e%2f%2e%2e%2fp1keep",
        // 200 characters
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ];

    for raw in hostile {
        for (method, suffix, body) in [
            ("GET", "", None),
            ("DELETE", "", None),
            ("PATCH", "", Some(json!({ "name": "x" }))),
            (
                "POST",
                "/admin-password",
                Some(json!({ "username": "admin", "password": "secret2" })),
            ),
            ("POST", "/enter", Some(json!({ "username": "admin" }))),
        ] {
            let uri = format!("/schools/{raw}{suffix}");
            let res = send(&app, method, &uri, Some(&builder), body).await;
            assert!(
                res.status == StatusCode::NOT_FOUND
                    || res.status == StatusCode::METHOD_NOT_ALLOWED
                    || res.status == StatusCode::BAD_REQUEST
                    || res.status == StatusCode::UNAUTHORIZED,
                "{method} {uri} answered {} — a hostile slug must be refused",
                res.status
            );
            assert!(
                !res.status.is_success(),
                "{method} {uri} SUCCEEDED ({})",
                res.status
            );
        }
    }

    // Also the empty segment, which is a different route shape entirely.
    for method in ["GET", "DELETE", "PATCH"] {
        let res = send(&app, method, "/schools/", Some(&builder), None).await;
        assert!(!res.status.is_success() || method == "GET", "{method} /schools/ -> {}", res.status);
    }

    assert!(keep_dir.join("keep.bin").exists(), "a bystander school lost its bytes");
    assert!(root_canary.exists(), "the FILES_PATH root canary was deleted");
    assert!(outside_file.exists(), "a file outside FILES_PATH was deleted");
    assert!(
        files_dir().exists() && outside.path().exists(),
        "a directory root was removed"
    );
    std::fs::remove_file(&root_canary).ok();
}

/// Claim 2: `drop` takes exactly the named school; a neighbour's cached handle
/// keeps working; a re-created slug is an *empty* school.
#[tokio::test]
async fn probe_drop_is_scoped_and_recreation_is_empty() {
    let (app, _db, _tenants) = deployment().await;
    let builder = builder_login(&app).await;
    create_school(&app, &builder, "p2a", "secret1").await;
    create_school(&app, &builder, "p2b", "secret1").await;

    let a = school_login(&app, "p2a", "admin", "secret1")
        .await
        .cookie
        .expect("p2a cookie");
    let b = school_login(&app, "p2b", "admin", "secret1")
        .await
        .cookie
        .expect("p2b cookie");
    // A row only p2a has.
    let note = send(
        &app,
        "POST",
        "/notes",
        Some(&a),
        Some(json!({ "title": "secret", "content": "x" })),
    )
    .await;
    assert_eq!(note.status, StatusCode::CREATED);
    // A second user only p2a has.
    let _ = send(
        &app,
        "POST",
        "/auth/register",
        None,
        Some(json!({ "school": "p2a", "username": "ali", "password": "secret1" })),
    )
    .await;

    assert_eq!(
        send(&app, "DELETE", "/schools/p2a", Some(&builder), None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );

    // The neighbour's already-resolved handle is untouched.
    assert_eq!(
        send(&app, "GET", "/auth/me", Some(&b), None).await.status,
        StatusCode::OK,
        "dropping p2a disturbed p2b's cached handle"
    );
    // The dropped school is unknown, not forbidden.
    assert_eq!(
        send(&app, "GET", "/auth/me", Some(&a), None).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        school_login(&app, "p2a", "admin", "secret1").await.status,
        StatusCode::UNAUTHORIZED
    );

    // Re-created with the same slug: nothing of the old school comes back.
    let again = create_school(&app, &builder, "p2a", "secret2").await;
    assert_eq!(again.status, StatusCode::CREATED, "{:?}", again.body);
    assert_eq!(
        school_login(&app, "p2a", "admin", "secret1").await.status,
        StatusCode::UNAUTHORIZED,
        "the OLD admin credential still opens the re-created school"
    );
    let fresh = school_login(&app, "p2a", "admin", "secret2")
        .await
        .cookie
        .expect("fresh admin cookie");
    let users = send(&app, "GET", "/users", Some(&fresh), None).await;
    assert_eq!(users.status, StatusCode::OK, "{:?}", users.body);
    assert_eq!(
        total(&users.body),
        1,
        "a re-created school carries users from the dropped one: {:?}",
        users.body
    );
    let notes = send(&app, "GET", "/notes", Some(&fresh), None).await;
    assert_eq!(notes.status, StatusCode::OK);
    assert_eq!(total(&notes.body), 0, "old rows survived the drop");
}

/// Claim 2, remote mode: `Tenants::create`/`drop` interpolate the slug into
/// `DEFINE DATABASE` / `REMOVE DATABASE`. Not injectable — but the statements
/// must at least *execute* for every slug `Slug::try_new` accepts. Run the exact
/// production statements against a live SurrealDB parser.
#[tokio::test]
async fn probe_remote_mode_database_statements_execute_for_every_accepted_slug() {
    let tenants = Tenants::new_mem().await.expect("control db");
    let control = tenants.control();
    let mut broken: Vec<String> = Vec::new();

    for raw in [
        "demo", "abc", "ata-koleji", "x-y", "trail-", "2024school", "12345", "a1",
    ] {
        let Ok(slug) = Slug::try_new(raw) else {
            continue;
        };
        // The exact statements `Tenants::create` / `drop` send, via the same
        // identifier quoting they use.
        let ident = hezarfen_backend::tenant::quoted_ident(&slug);
        let define = control
            .query(format!("DEFINE DATABASE IF NOT EXISTS {ident}"))
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r.check().map(|_| ()).map_err(|e| e.to_string()));
        let remove = control
            .query(format!("REMOVE DATABASE IF EXISTS {ident}"))
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r.check().map(|_| ()).map_err(|e| e.to_string()));
        if let Err(err) = define {
            broken.push(format!("DEFINE DATABASE {raw} -> {err}"));
        }
        if let Err(err) = remove {
            broken.push(format!("REMOVE DATABASE {raw} -> {err}"));
        }
    }
    assert!(
        broken.is_empty(),
        "slugs accepted by Slug::try_new whose remote-mode statements do not execute:\n{}",
        broken.join("\n")
    );
}

/// Claim 3: the accept set, and the reserved list `GET /limits` publishes.
#[tokio::test]
async fn probe_slug_accept_set_and_published_reserved_list() {
    for bad in [
        "control", "builder", "Demo", "DEMO", "..", "", "a", "-lead", "de mo", "de.mo", "de_mo",
        "demo/", "demo\\", "٢٣demo", "démo", "demo\u{200b}", "demo\n",
        // 33 characters
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ] {
        assert!(
            Slug::try_new(bad).is_err(),
            "Slug::try_new accepted {bad:?}"
        );
    }
    for good in ["ab", "demo", "a1", "2024", "ata-koleji"] {
        assert!(Slug::try_new(good).is_ok(), "Slug::try_new rejected {good:?}");
    }
    // Documented boundary: a trailing hyphen IS accepted today.
    assert!(
        Slug::try_new("trail-").is_ok(),
        "trailing hyphen behaviour changed"
    );

    // The published list must be the enforced list.
    let (app, _db, _tenants) = deployment().await;
    let student = login(&app, "ali").await;
    let limits = send(&app, "GET", "/limits", Some(&student), None).await;
    assert_eq!(limits.status, StatusCode::OK, "{:?}", limits.body);
    let published: Vec<String> = limits.body["school"]["reserved_slugs"]
        .as_array()
        .or_else(|| {
            limits
                .body
                .as_object()
                .and_then(|o| o.values().find_map(|v| v["reserved_slugs"].as_array()))
        })
        .unwrap_or_else(|| panic!("no reserved_slugs in /limits: {}", limits.body))
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let mut enforced: Vec<String> = hezarfen_backend::tenant::RESERVED_SLUGS
        .iter()
        .map(|s| s.to_string())
        .collect();
    enforced.sort();
    let mut published_sorted = published.clone();
    published_sorted.sort();
    assert_eq!(
        published_sorted, enforced,
        "GET /limits publishes a different reserved list than Slug::try_new enforces"
    );
    for name in &published {
        assert!(
            Slug::try_new(name).is_err(),
            "{name} is published as reserved but Slug::try_new accepts it"
        );
    }
}

/// Claim 4: `admin-password` and `enter` cannot be aimed at a non-admin, at a
/// stranger's account, or (for `enter`) at a suspended school; and a reset kills
/// **every** live session of that account.
#[tokio::test]
async fn probe_admin_targets_and_reset_revokes_all_sessions() {
    let (app, _db, tenants) = deployment().await;
    let builder = builder_login(&app).await;
    create_school(&app, &builder, "p4a", "secret1").await;
    create_school(&app, &builder, "p4b", "secret1").await;
    let a_db = tenants.get(&Slug::try_new("p4a").unwrap()).await.unwrap();

    // Three non-admins in p4a, and a name that only exists in p4b.
    for (name, role) in [("tea", "teacher"), ("stu", "student"), ("par", "parent")] {
        let reg = send(
            &app,
            "POST",
            "/auth/register",
            None,
            Some(json!({ "school": "p4a", "username": name, "password": "secret1" })),
        )
        .await;
        assert_eq!(reg.status, StatusCode::CREATED, "register {name}");
        set_role(&a_db, name, role).await;
    }
    let reg = send(
        &app,
        "POST",
        "/auth/register",
        None,
        Some(json!({ "school": "p4b", "username": "onlyb", "password": "secret1" })),
    )
    .await;
    assert_eq!(reg.status, StatusCode::CREATED);

    for target in ["tea", "stu", "par"] {
        for (path, body) in [
            (
                "/schools/p4a/admin-password",
                json!({ "username": target, "password": "secret2" }),
            ),
            ("/schools/p4a/enter", json!({ "username": target })),
        ] {
            let res = send(&app, "POST", path, Some(&builder), Some(body)).await;
            assert_eq!(
                res.status,
                StatusCode::CONFLICT,
                "{path} accepted the non-admin {target}: {} {:?}",
                res.status,
                res.body
            );
        }
    }
    // A user of the school next door is not reachable from this school.
    for (path, body) in [
        (
            "/schools/p4a/admin-password",
            json!({ "username": "onlyb", "password": "secret2" }),
        ),
        ("/schools/p4a/enter", json!({ "username": "onlyb" })),
    ] {
        let res = send(&app, "POST", path, Some(&builder), Some(body)).await;
        assert_eq!(
            res.status,
            StatusCode::NOT_FOUND,
            "{path} reached across schools: {:?}",
            res.body
        );
    }

    // Two live sessions for the same admin; a reset must kill both.
    let s1 = school_login(&app, "p4a", "admin", "secret1")
        .await
        .cookie
        .expect("session 1");
    let s2 = school_login(&app, "p4a", "admin", "secret1")
        .await
        .cookie
        .expect("session 2");
    assert_ne!(s1, s2, "the harness reused one session");
    // A bystander session in the same school must survive.
    let tea = school_login(&app, "p4a", "tea", "secret1")
        .await
        .cookie
        .expect("teacher session");

    let reset = send(
        &app,
        "POST",
        "/schools/p4a/admin-password",
        Some(&builder),
        Some(json!({ "username": "admin", "password": "secret2" })),
    )
    .await;
    assert_eq!(reset.status, StatusCode::NO_CONTENT);
    for (n, cookie) in [(1, &s1), (2, &s2)] {
        assert_eq!(
            send(&app, "GET", "/auth/me", Some(cookie), None).await.status,
            StatusCode::UNAUTHORIZED,
            "session {n} survived the password reset"
        );
    }
    assert_eq!(
        send(&app, "GET", "/auth/me", Some(&tea), None).await.status,
        StatusCode::OK,
        "the reset revoked a bystander's session"
    );

    // `enter` on a suspended school is 403; `admin-password` still works.
    assert_eq!(
        send(
            &app,
            "PATCH",
            "/schools/p4a",
            Some(&builder),
            Some(json!({ "status": "suspended" })),
        )
        .await
        .status,
        StatusCode::OK
    );
    let entered = send(
        &app,
        "POST",
        "/schools/p4a/enter",
        Some(&builder),
        Some(json!({ "username": "admin" })),
    )
    .await;
    assert_eq!(
        entered.status,
        StatusCode::FORBIDDEN,
        "enter opened a suspended school: {:?}",
        entered.body
    );
    assert!(
        entered.cookie.is_none(),
        "a refused enter still set a school cookie"
    );
    assert_eq!(
        send(
            &app,
            "POST",
            "/schools/p4a/admin-password",
            Some(&builder),
            Some(json!({ "username": "admin", "password": "secret3" })),
        )
        .await
        .status,
        StatusCode::NO_CONTENT,
        "a lockout fix must still work on a suspended school"
    );
}

/// Claim 5: builder session hygiene — logout is a revocation, and neither
/// principal's token works under the other's prefix.
#[tokio::test]
async fn probe_builder_session_hygiene() {
    let (app, _db, _tenants) = deployment().await;
    let builder = builder_login(&app).await;
    let student = login(&app, "veli").await;

    let builder_token = builder
        .trim_start_matches("session=builder.")
        .to_string();
    let student_token = cookie_token(&student).to_string();

    // The builder's own token, worn as a school cookie.
    for uri in ["/auth/me", "/users", "/notes"] {
        let res = send(
            &app,
            "GET",
            uri,
            Some(&format!("session={DEMO_SLUG}.{builder_token}")),
            None,
        )
        .await;
        assert_eq!(
            res.status,
            StatusCode::UNAUTHORIZED,
            "{uri} accepted a builder token under a school prefix"
        );
    }
    // And a school token worn as a builder cookie.
    for uri in ["/builder/me", "/schools"] {
        let res = send(
            &app,
            "GET",
            uri,
            Some(&format!("session=builder.{student_token}")),
            None,
        )
        .await;
        assert_eq!(
            res.status,
            StatusCode::UNAUTHORIZED,
            "{uri} accepted a school token under the builder prefix"
        );
    }

    // Logout is a server-side revocation: the same cookie is dead everywhere.
    assert_eq!(
        send(&app, "POST", "/builder/logout", Some(&builder), None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    for (method, uri, body) in [
        ("GET", "/builder/me", None),
        ("GET", "/schools", None),
        (
            "POST",
            "/schools",
            Some(json!({
                "slug": "p5ghost",
                "name": "ghost",
                "admin_username": "admin",
                "admin_password": "secret1"
            })),
        ),
        ("DELETE", "/schools/demo", None),
    ] {
        let res = send(&app, method, uri, Some(&builder), body).await;
        assert_eq!(
            res.status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} accepted a logged-out builder cookie"
        );
    }
    // …and nothing was created or destroyed by those calls.
    let fresh = builder_login(&app).await;
    let list = send(&app, "GET", "/schools", Some(&fresh), None).await;
    assert!(
        !slugs(&list.body).contains(&"p5ghost".to_string()),
        "a logged-out builder created a school"
    );
    assert!(
        slugs(&list.body).contains(&DEMO_SLUG.to_string()),
        "a logged-out builder deleted the demo school"
    );
    // Logout is idempotent and safe without a cookie.
    assert_eq!(
        send(&app, "POST", "/builder/logout", None, None).await.status,
        StatusCode::NO_CONTENT
    );
}

/// Claim 6: two creates of one slug, concurrently, leave exactly one school.
#[tokio::test]
async fn probe_concurrent_creates_of_one_slug_leave_one_school() {
    let (app, _db, _tenants) = deployment().await;
    let builder = builder_login(&app).await;

    let (first, second) = tokio::join!(
        create_school(&app, &builder, "p6race", "secret1"),
        create_school(&app, &builder, "p6race", "secret2"),
    );
    let statuses = [first.status, second.status];
    let created = statuses
        .iter()
        .filter(|s| **s == StatusCode::CREATED)
        .count();
    assert_eq!(
        created, 1,
        "concurrent creates of one slug: {statuses:?} ({:?} / {:?})",
        first.body, second.body
    );

    let list = send(&app, "GET", "/schools", Some(&builder), None).await;
    let count = slugs(&list.body)
        .iter()
        .filter(|s| s.as_str() == "p6race")
        .count();
    assert_eq!(count, 1, "the registry holds {count} rows for p6race");

    // Exactly one of the two admin passwords opens it.
    let mut opens = 0;
    for pass in ["secret1", "secret2"] {
        if school_login(&app, "p6race", "admin", pass).await.status == StatusCode::OK {
            opens += 1;
        }
    }
    assert_eq!(opens, 1, "{opens} of the two admin passwords open p6race");
}

/// A `surreal start` child that dies with the test.
struct Server(std::process::Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Claim 1/2 driven against a **real remote deployment** — the mode every test
/// above skips, and the only one where `DEFINE DATABASE` / `REMOVE DATABASE`
/// actually run. `ata-koleji` is the slug the OpenAPI schema advertises.
#[tokio::test]
async fn probe_remote_deployment_creates_and_deletes_a_hyphenated_school() {
    let bin = std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".surrealdb/surreal");
    if !bin.exists() {
        eprintln!("skipped: no surreal binary at {}", bin.display());
        return;
    }
    let port = 18441u16;
    let _server = Server(
        std::process::Command::new(&bin)
            .args([
                "start",
                "--user",
                "root",
                "--pass",
                "root",
                "--bind",
                &format!("127.0.0.1:{port}"),
                "memory",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn surreal"),
    );
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let files = tempfile::tempdir().unwrap();
    let cfg = hezarfen_backend::config::Config {
        host: "127.0.0.1".into(),
        port: 0,
        db_url: format!("ws://127.0.0.1:{port}"),
        db_user: "root".into(),
        db_pass: "root".into(),
        db_ns: "hezarfen".into(),
        db_name: "control".into(),
        files_path: files.path().display().to_string(),
        cookie_secure: false,
        rate_limit: hezarfen_backend::rate_limit::RateLimitConfig::unlimited(),
        chatbot_per_minute: 0,
        builder_username: Some(BUILDER_USER.into()),
        builder_password: Some(BUILDER_PASS.into()),
        ai_quic_addr: None,
        ai_shared_token: None,
        ai_tls_cert: None,
        ai_tls_key: None,
        ai_request_timeout_secs: 30,
    };
    let tenants = hezarfen_backend::database::init(&cfg)
        .await
        .expect("remote deployment");
    let app = hezarfen_backend::build_router(hezarfen_backend::state::AppState {
        db: tenants.control().clone(),
        tenants: tenants.clone(),
        files_path: files.path().to_path_buf(),
        cookie_secure: false,
        rate_limit: hezarfen_backend::rate_limit::RateLimitConfig::unlimited(),
        chatbot_limit: Default::default(),
        exam_presence: Default::default(),
        board_hub: Default::default(),
        db_up: Default::default(),
        ai: None,
    });

    let builder = builder_login(&app).await;
    // A plain slug proves the harness itself works.
    let plain = create_school(&app, &builder, "atakoleji", "secret1").await;
    assert_eq!(
        plain.status,
        StatusCode::CREATED,
        "plain slug: {:?}",
        plain.body
    );
    assert_eq!(
        school_login(&app, "atakoleji", "admin", "secret1")
            .await
            .status,
        StatusCode::OK
    );

    // The advertised, hyphenated slug. Collect the whole chain before asserting,
    // so a failure reports the state the deployment is left in.
    let made = create_school(&app, &builder, "ata-koleji", "secret1").await;
    let listed = |body: &Value| slugs(body).contains(&"ata-koleji".to_string());
    let after_create = send(&app, "GET", "/schools", Some(&builder), None).await;
    let retry = create_school(&app, &builder, "ata-koleji", "secret1").await;
    let login = school_login(&app, "ata-koleji", "admin", "secret1").await;
    let deleted = send(&app, "DELETE", "/schools/ata-koleji", Some(&builder), None).await;
    let after_delete = send(&app, "GET", "/schools", Some(&builder), None).await;
    let login_after_delete = school_login(&app, "ata-koleji", "admin", "secret1").await;

    assert_eq!(
        made.status,
        StatusCode::CREATED,
        "POST /schools ata-koleji (the slug the OpenAPI example advertises) -> {} {:?}; \
         registry lists it: {}; retry -> {}; DELETE -> {} {:?}; still listed after DELETE: {}; \
         admin login -> {}",
        made.status,
        made.body,
        listed(&after_create.body),
        retry.status,
        deleted.status,
        deleted.body,
        listed(&after_delete.body),
        login.status,
    );
    assert_eq!(retry.status, StatusCode::CONFLICT, "a made school is taken");
    assert_eq!(login.status, StatusCode::OK, "the admin logs into the hyphenated school");
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert!(!listed(&after_delete.body));
    assert_eq!(
        login_after_delete.status,
        StatusCode::UNAUTHORIZED,
        "a deleted school is unknown at login"
    );
}
