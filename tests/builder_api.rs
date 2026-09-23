//! The vendor surface (`web::builder`): the one principal that is not a user of
//! any school. Everything here is about the line between the two — a builder
//! cookie must never act inside a school, a school cookie must never manage
//! schools, and a session minted by `enter` must act in exactly one school.

mod common;

use axum::Router;
use axum::http::StatusCode;
use common::*;
use hezarfen_backend::domain::user::{Password, Username};
use hezarfen_backend::service::builder;
use hezarfen_backend::tenant::{DEMO_SCHOOL_ID, SchoolId, Tenants};
use serde_json::{Value, json};

const BUILDER_USER: &str = "operator";
const BUILDER_PASS: &str = "secret1";

/// A deployment with the demo school, plus a builder account to drive it —
/// production's `BUILDER_USERNAME`/`BUILDER_PASSWORD` seed, by the same path.
async fn deployment() -> (Router, hezarfen_backend::database::Database, Tenants) {
    let (app, db, tenants) = app_and_tenants().await;
    builder::ensure(
        tenants.control(),
        Username::try_new(BUILDER_USER).unwrap(),
        Password::try_new(BUILDER_PASS).unwrap(),
    )
    .await
    .expect("seed the builder");
    (app, db, tenants)
}

async fn builder_login(app: &Router) -> String {
    let res = bsend(
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

/// Create a school as the builder. `label` is only the display-name prefix
/// (`"{label} school"`); the identity comes back as `id`.
async fn create_school(app: &Router, cookie: &str, label: &str, admin_pass: &str) -> Res {
    bsend(
        app,
        "POST",
        "/schools",
        Some(cookie),
        Some(json!({
            "name": format!("{label} school"),
            "admin_username": "admin",
            "admin_password": admin_pass,
        })),
    )
    .await
}

fn created_id(res: &Res) -> String {
    res.body["id"].as_str().expect("created school id").to_string()
}

/// Pick `label` out of a login choice list: a uuid is used as-is, otherwise
/// the school whose name is `{label} school` (the name [`create_school`] writes).
fn school_choice(body: &Value, label: &str) -> String {
    if hezarfen_backend::tenant::SchoolId::try_parse(label).is_ok() {
        return label.to_string();
    }
    let want = format!("{label} school");
    body["schools"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| s["name"].as_str() == Some(want.as_str()) || s["name"].as_str() == Some(label))
        .and_then(|s| s["id"].as_str())
        .unwrap_or(label)
        .to_string()
}

/// Log `username` into the school named `slug`. The login itself names no
/// school any more — the admin fixtures reuse one username across many
/// schools, which makes that admin one *person* with several memberships —
/// so when login answers a choice list, bind the requested school before
/// returning. The result is whatever a plain school login would have said.
async fn school_login(app: &Router, slug: &str, username: &str, password: &str) -> Res {
    let res = bsend(
        app,
        "POST",
        "/auth/login",
        None,
        Some(json!({ "username": username, "password": password })),
    )
    .await;
    if res.body["schools"].is_array() {
        assert_eq!(res.status, StatusCode::OK, "choice login {username}");
        let selected = bsend(
            app,
            "POST",
            "/auth/school",
            res.cookie.as_deref(),
            Some(json!({ "school": school_choice(&res.body, slug) })),
        )
        .await;
        assert_eq!(
            selected.status,
            StatusCode::OK,
            "select {slug} for {username}"
        );
        return selected;
    }
    res
}

fn school_ids(body: &Value) -> Vec<String> {
    items(body)
        .iter()
        .map(|item| item["id"].as_str().expect("id").to_string())
        .collect()
}

fn school_names(body: &Value) -> Vec<&str> {
    items(body)
        .iter()
        .map(|item| item["name"].as_str().expect("name"))
        .collect()
}


/// Rewrite `/schools/{label}` to `/schools/{id}` when `label` is a school this
/// deployment created (`"{label} school"`). A segment that is already a uuid,
/// or that names no school, is left alone so a 404 stays a 404.
async fn bsend(
    app: &Router,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: Option<Value>,
) -> Res {
    let path = rewrite_school_path(app, cookie, path).await;
    common::send(app, method, &path, cookie, body).await
}

async fn rewrite_school_path(app: &Router, cookie: Option<&str>, path: &str) -> String {
    let Some(rest) = path.strip_prefix("/schools/") else {
        return path.to_string();
    };
    if rest.is_empty() || rest.contains('{') {
        return path.to_string();
    }
    let (seg, tail) = match rest.split_once('/') {
        Some((seg, tail)) => (seg, format!("/{tail}")),
        None => (rest, String::new()),
    };
    if hezarfen_backend::tenant::SchoolId::try_parse(seg).is_ok() {
        return path.to_string();
    }
    let Some(cookie) = cookie else {
        return path.to_string();
    };
    let list = common::send(app, "GET", "/schools", Some(cookie), None).await;
    let Some(rows) = list.body.get("items").and_then(|v| v.as_array()) else {
        return path.to_string();
    };
    let want = format!("{seg} school");
    let id = rows.iter().find_map(|row| {
        let name = row["name"].as_str()?;
        if name == want || name == seg {
            row["id"].as_str().map(str::to_string)
        } else {
            None
        }
    });
    match id {
        Some(id) => format!("/schools/{id}{tail}"),
        None => path.to_string(),
    }
}

/// The two cookies share a name and nothing else. This is the whole point of
/// the split: neither principal may ever be accepted where the other belongs.
#[tokio::test]
async fn a_builder_logs_in_and_its_cookie_works_here_and_nowhere_else() {
    let (app, _db, _tenants) = deployment().await;
    let builder = builder_login(&app).await;

    let me = bsend(&app, "GET", "/builder/me", Some(&builder), None).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.body["username"], BUILDER_USER);
    assert!(me.body["id"].as_str().is_some_and(|id| !id.is_empty()));

    // A school user's cookie is not a builder's…
    let student = login(&app, "ali").await;
    assert_eq!(
        bsend(&app, "GET", "/builder/me", Some(&student), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED,
        "a school cookie reached the builder surface"
    );
    assert_eq!(
        bsend(&app, "GET", "/schools", Some(&student), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    // …and a builder's is not a school user's.
    assert_eq!(
        bsend(&app, "GET", "/auth/me", Some(&builder), None)
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
            bsend(&app, "POST", "/builder/login", None, Some(bad))
                .await
                .status,
            StatusCode::UNAUTHORIZED
        );
    }

    let out = bsend(&app, "POST", "/builder/logout", Some(&builder), None).await;
    assert_eq!(out.status, StatusCode::NO_CONTENT);
    assert_eq!(
        bsend(&app, "GET", "/builder/me", Some(&builder), None)
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
    let id = created_id(&created);
    assert_eq!(created.body["name"], "ata-koleji school");
    assert_eq!(created.body["status"], "active");
    assert!(
        hezarfen_backend::tenant::SchoolId::try_parse(&id).is_ok(),
        "the identity is a hyphenated uuid: {id}"
    );

    let login = school_login(&app, &id, "admin", "secret1").await;
    assert_eq!(login.status, StatusCode::OK, "seeded admin logs in");
    let cookie = login.cookie.expect("school cookie");
    assert!(
        cookie.starts_with(&format!("session={id}.")),
        "the cookie names its school: {cookie}"
    );
    let me = bsend(&app, "GET", "/auth/me", Some(&cookie), None).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(
        me.body["role"], "admin",
        "the seed is an admin, not a student"
    );

    // A blank name and a too-short admin password are refused before a
    // database exists. Same name as an existing school is allowed — the id
    // is the identity, not the display name.
    for body in [
        json!({ "name": "  ", "admin_username": "admin", "admin_password": "secret1" }),
        json!({ "name": "Yeni", "admin_username": "admin", "admin_password": "x" }),
    ] {
        assert_eq!(
            bsend(&app, "POST", "/schools", Some(&builder), Some(body))
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
    }
    let again = school_login(&app, &id, "admin", "secret1").await;
    assert_eq!(again.status, StatusCode::OK);
    let cookie = again.cookie.expect("cookie");
    assert!(
        cookie.starts_with(&format!("session={id}.")),
        "a refused create must not have made a school: {cookie:?}"
    );
}

/// Listing, reading and patching the registry — and the suspension that closes
/// a school to its own users while the builder keeps managing it.
#[tokio::test]
async fn a_suspension_closes_the_school_and_a_resume_reopens_it() {
    let (app, _db, _tenants) = deployment().await;
    let builder = builder_login(&app).await;
    let created = create_school(&app, &builder, "beta", "secret1").await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    let beta = created_id(&created);

    let list = bsend(&app, "GET", "/schools", Some(&builder), None).await;
    assert_eq!(list.status, StatusCode::OK);
    let listed = school_ids(&list.body);
    assert!(listed.contains(&DEMO_SCHOOL_ID.to_string()) && listed.contains(&beta));
    assert_eq!(total(&list.body), 2);
    // The envelope pages like every other list.
    let page = bsend(&app, "GET", "/schools?limit=1", Some(&builder), None).await;
    assert_eq!(items(&page.body).len(), 1);
    assert_eq!(total(&page.body), 2);

    assert_eq!(
        bsend(&app, "GET", "/schools/ghost", Some(&builder), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    // A partial patch touches only what it names: the name moves, the status
    // and the created stamp stay.
    let before = bsend(&app, "GET", &format!("/schools/{beta}"), Some(&builder), None).await;
    let renamed = bsend(
        &app,
        "PATCH",
        &format!("/schools/{beta}"),
        Some(&builder),
        Some(json!({ "name": "Beta Koleji" })),
    )
    .await;
    assert_eq!(renamed.status, StatusCode::OK);
    assert_eq!(renamed.body["name"], "Beta Koleji");
    assert_eq!(renamed.body["status"], "active");
    assert_eq!(renamed.body["id"], beta);
    assert_eq!(renamed.body["created_at"], before.body["created_at"]);
    // An empty patch is a no-op read, and a bad status is a 400 — including
    // `provisioning`, which is the boot's own word for a school that is still
    // being made and not a state a vendor can ask for.
    let untouched = bsend(
        &app,
        "PATCH",
        &format!("/schools/{beta}"),
        Some(&builder),
        Some(json!({})),
    )
    .await;
    assert_eq!(untouched.body["name"], "Beta Koleji");
    for refused in ["closed", "provisioning"] {
        assert_eq!(
            bsend(
                &app,
                "PATCH",
                &format!("/schools/{beta}"),
                Some(&builder),
                Some(json!({ "status": refused })),
            )
            .await
            .status,
            StatusCode::BAD_REQUEST,
            "status {refused:?}"
        );
    }

    // A live cookie, taken out before the suspension.
    let live = school_login(&app, &beta, "admin", "secret1")
        .await
        .cookie
        .expect("school cookie");
    let suspended = bsend(
        &app,
        "PATCH",
        &format!("/schools/{beta}"),
        Some(&builder),
        Some(json!({ "status": "suspended" })),
    )
    .await;
    assert_eq!(suspended.status, StatusCode::OK);
    assert_eq!(suspended.body["status"], "suspended");

    assert_eq!(
        school_login(&app, &beta, "admin", "secret1").await.status,
        StatusCode::FORBIDDEN,
        "a suspended school refuses login"
    );
    assert_eq!(
        bsend(&app, "GET", "/auth/me", Some(&live), None)
            .await
            .status,
        StatusCode::FORBIDDEN,
        "…and the cookie it had already issued, on its very next call"
    );
    // The builder still manages it — that is how it gets un-suspended.
    assert_eq!(
        bsend(&app, "GET", &format!("/schools/{beta}"), Some(&builder), None)
            .await
            .status,
        StatusCode::OK
    );
    // …except entering it, the one door a suspension must also close.
    assert_eq!(
        bsend(
            &app,
            "POST",
            &format!("/schools/{beta}/enter"),
            Some(&builder),
            Some(json!({ "username": "admin" })),
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    bsend(
        &app,
        "PATCH",
        &format!("/schools/{beta}"),
        Some(&builder),
        Some(json!({ "status": "active" })),
    )
    .await;
    assert_eq!(
        bsend(&app, "GET", "/auth/me", Some(&live), None)
            .await
            .status,
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
    create_school(&app, &builder, "delta", "secret1").await;
    let old_cookie = school_login(&app, "gamma", "admin", "secret1")
        .await
        .cookie
        .expect("school cookie");
    let person_cookie = bsend(
        &app,
        "POST",
        "/auth/login",
        None,
        Some(json!({ "username": "admin", "password": "secret1" })),
    )
    .await
    .cookie
    .expect("person cookie from a two-school login");

    // An account that is not an admin of that school, and one that is not there
    // at all, are told apart.
    assert_eq!(
        bsend(
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
    let gamma_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM school WHERE name = 'gamma school'")
        .fetch_one(_tenants.control())
        .await
        .expect("gamma row");
    let gamma_db = _tenants
        .get(&hezarfen_backend::tenant::SchoolId::from_uuid(gamma_id))
        .await
        .unwrap();
    hezarfen_backend::db::user::create(&gamma_db, Username::try_new("veli").unwrap(), None)
        .await
        .unwrap();
    assert_eq!(
        bsend(
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

    let reset = bsend(
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
        bsend(&app, "GET", "/auth/me", Some(&old_cookie), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED,
        "a reset that leaves the old cookie working resets nothing"
    );
    assert_eq!(
        bsend(
            &app,
            "POST",
            "/auth/school",
            Some(&person_cookie),
            Some(json!({ "school": gamma_id.to_string() })),
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED,
        "a person cookie minted under the old password must die too"
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

    let entered = bsend(
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
    assert!(alpha.trim_start_matches("session=").split_once('.').unwrap().0.len() == 36, "{alpha}");

    // It is an ordinary school session — and only a school session.
    let me = bsend(&app, "GET", "/auth/me", Some(&alpha), None).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.body["role"], "admin");
    assert_eq!(
        bsend(&app, "GET", "/builder/me", Some(&alpha), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED,
        "entering a school must not hand back builder power"
    );

    // A row written in alpha is invisible from beta — the isolation the whole
    // per-school database exists for.
    let note = bsend(
        &app,
        "POST",
        "/notes",
        Some(&alpha),
        Some(json!({ "title": "alpha only", "content": "mine" })),
    )
    .await;
    assert_eq!(note.status, StatusCode::CREATED);
    let note_id = id_of(&note.body);
    let beta = bsend(
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
        bsend(&app, "GET", &format!("/notes/{note_id}"), Some(&beta), None)
            .await
            .status,
        StatusCode::NOT_FOUND,
        "a note written in alpha was reachable under beta's cookie"
    );

    // Unknown school, unknown account, non-admin account.
    assert_eq!(
        bsend(
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
        bsend(
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
        let note = bsend(
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
        // One directory per school under `FILES_PATH`, named by the school uuid.
        let school = cookie.trim_start_matches("session=").split_once('.').unwrap().0;
        let blob = files_dir().join(school).join(id_of(&up.body));
        assert!(blob.exists(), "{} should exist", blob.display());
        blobs.push(blob);
    }

    let deleted = bsend(&app, "DELETE", "/schools/delta", Some(&builder), None).await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);

    assert_eq!(
        bsend(&app, "GET", "/schools/delta", Some(&builder), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // A deleted school is not a door any more. The person's login has only
    // `epsilon` left to enter — the dropped slug is neither offered nor 403'd.
    let after_delete = school_login(&app, "delta", "admin", "secret1").await;
    assert_eq!(after_delete.status, StatusCode::OK);
    {
        let kept = after_delete.cookie.expect("cookie");
        let prefix = kept.trim_start_matches("session=").split_once('.').unwrap().0;
        assert_eq!(prefix.len(), 36, "remaining school uuid: {kept}");
        assert!(!blobs[0].parent().unwrap().exists(), "deleted school files dir must be gone");
    }
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
        bsend(&app, "DELETE", "/schools/delta", Some(&builder), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

// ---------------------------------------------------------------------------
// REFUTE-mode probes on the irreversible paths (appended by a verifier).
// ---------------------------------------------------------------------------


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
        "%C4%B1demo",    // dotless i, non-ascii
        "d%E2%80%8Bemo", // zero-width space
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
            let res = bsend(&app, method, &uri, Some(&builder), body).await;
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
        let res = bsend(&app, method, "/schools/", Some(&builder), None).await;
        assert!(
            !res.status.is_success() || method == "GET",
            "{method} /schools/ -> {}",
            res.status
        );
    }

    assert!(
        keep_dir.join("keep.bin").exists(),
        "a bystander school lost its bytes"
    );
    assert!(
        root_canary.exists(),
        "the FILES_PATH root canary was deleted"
    );
    assert!(
        outside_file.exists(),
        "a file outside FILES_PATH was deleted"
    );
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
    let (app, _db, tenants) = deployment().await;
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
    let note = bsend(
        &app,
        "POST",
        "/notes",
        Some(&a),
        Some(json!({ "title": "secret", "content": "x" })),
    )
    .await;
    assert_eq!(note.status, StatusCode::CREATED);
    // A second user only p2a has.
    let p2a_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM school WHERE name = 'p2a school'")
        .fetch_one(tenants.control())
        .await
        .expect("p2a");
    let _ = bsend(
        &app,
        "POST",
        "/auth/register",
        None,
        Some(json!({ "school": p2a_id.to_string(), "username": "ali", "password": "secret1" })),
    )
    .await;

    assert_eq!(
        bsend(&app, "DELETE", "/schools/p2a", Some(&builder), None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );

    // The neighbour's already-resolved handle is untouched.
    assert_eq!(
        bsend(&app, "GET", "/auth/me", Some(&b), None).await.status,
        StatusCode::OK,
        "dropping p2a disturbed p2b's cached handle"
    );
    // The dropped school is unknown, not forbidden.
    assert_eq!(
        bsend(&app, "GET", "/auth/me", Some(&a), None).await.status,
        StatusCode::UNAUTHORIZED
    );
    // The person behind `admin` still belongs to p2b: the login has that
    // school left to enter, and the dropped slug is not among the choices.
    let after_drop = school_login(&app, "p2a", "admin", "secret1").await;
    assert_eq!(after_drop.status, StatusCode::OK);
    let kept = after_drop.cookie.expect("cookie");
    assert!(
        kept.trim_start_matches("session=").split_once('.').unwrap().0.len() == 36,
        "login must enter the remaining school by uuid: {kept}"
    );

    // Re-created with the same slug. `admin` is one *person*: the same
    // username under the same password links that person into the new
    // school, and under a *different* password the create itself refuses
    // with a 409 — a builder is authenticated, there is nothing to enumerate.
    let again = create_school(&app, &builder, "p2a", "secret1").await;
    assert_eq!(again.status, StatusCode::CREATED, "{:?}", again.body);
    let fresh = school_login(&app, "p2a", "admin", "secret1")
        .await
        .cookie
        .expect("fresh admin cookie");
    let users = bsend(&app, "GET", "/users", Some(&fresh), None).await;
    assert_eq!(users.status, StatusCode::OK, "{:?}", users.body);
    assert_eq!(
        total(&users.body),
        1,
        "a re-created school carries users from the dropped one: {:?}",
        users.body
    );
    let notes = bsend(&app, "GET", "/notes", Some(&fresh), None).await;
    assert_eq!(notes.status, StatusCode::OK);
    assert_eq!(total(&notes.body), 0, "old rows survived the drop");

    // A *different* password for an existing person is a plain 409, and no
    // school is left behind by the refusal.
    let mismatched = create_school(&app, &builder, "p2c", "secret9").await;
    assert_eq!(
        mismatched.status,
        StatusCode::CONFLICT,
        "{:?}",
        mismatched.body
    );
    assert_eq!(
        bsend(&app, "GET", "/schools/p2c", Some(&builder), None)
            .await
            .status,
        StatusCode::NOT_FOUND,
        "a refused create must not leave a school behind"
    );
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
    let p4a_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM school WHERE name = 'p4a school'")
        .fetch_one(tenants.control()).await.expect("p4a");
    let p4b_id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM school WHERE name = 'p4b school'")
        .fetch_one(tenants.control())
        .await
        .expect("p4b");
    let a_db = tenants.get(&SchoolId::from_uuid(p4a_id)).await.unwrap();

    // Three non-admins in p4a, and a name that only exists in p4b.
    for (name, role) in [("tea", "teacher"), ("stu", "student"), ("par", "parent")] {
        let reg = bsend(
            &app,
            "POST",
            "/auth/register",
            None,
            Some(json!({ "school": p4a_id.to_string(), "username": name, "password": "secret1" })),
        )
        .await;
        assert_eq!(reg.status, StatusCode::CREATED, "register {name}");
        set_role(&a_db, name, role).await;
    }
    let reg = bsend(
        &app,
        "POST",
        "/auth/register",
        None,
        Some(json!({ "school": p4b_id.to_string(), "username": "onlyb", "password": "secret1" })),
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
            let res = bsend(&app, "POST", path, Some(&builder), Some(body)).await;
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
        let res = bsend(&app, "POST", path, Some(&builder), Some(body)).await;
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

    let reset = bsend(
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
            bsend(&app, "GET", "/auth/me", Some(cookie), None)
                .await
                .status,
            StatusCode::UNAUTHORIZED,
            "session {n} survived the password reset"
        );
    }
    assert_eq!(
        bsend(&app, "GET", "/auth/me", Some(&tea), None).await.status,
        StatusCode::OK,
        "the reset revoked a bystander's session"
    );

    // `enter` on a suspended school is 403; `admin-password` still works.
    assert_eq!(
        bsend(
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
    let entered = bsend(
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
        bsend(
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

    let builder_token = builder.trim_start_matches("session=builder.").to_string();
    let student_token = cookie_token(&student).to_string();

    // The builder's own token, worn as a school cookie.
    for uri in ["/auth/me", "/users", "/notes"] {
        let res = bsend(
            &app,
            "GET",
            uri,
            Some(&format!("session={DEMO_SCHOOL_ID}.{builder_token}")),
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
        let res = bsend(
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
        bsend(&app, "POST", "/builder/logout", Some(&builder), None)
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
                "name": "p5ghost school",
                "admin_username": "admin",
                "admin_password": "secret1"
            })),
        ),
        ("DELETE", "/schools/demo", None),
    ] {
        let res = bsend(&app, method, uri, Some(&builder), body).await;
        assert_eq!(
            res.status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} accepted a logged-out builder cookie"
        );
    }
    // …and nothing was created or destroyed by those calls.
    let fresh = builder_login(&app).await;
    let list = bsend(&app, "GET", "/schools", Some(&fresh), None).await;
    assert!(
        !school_names(&list.body).contains(&"p5ghost school"),
        "a logged-out builder created a school"
    );
    assert!(
        school_ids(&list.body).contains(&DEMO_SCHOOL_ID.to_string()),
        "a logged-out builder deleted the demo school"
    );
    // Logout is idempotent and safe without a cookie.
    assert_eq!(
        bsend(&app, "POST", "/builder/logout", None, None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
}

/// Create, enter, and delete a school by its minted uuid. A second create
/// with the same display name is a new school — the name is not the identity.
#[tokio::test]
async fn probe_remote_deployment_creates_and_deletes_a_school() {
    let deployment = common::deployment_with(&[]).await;
    let app = deployment.app;
    builder::ensure(
        deployment.tenants.control(),
        Username::try_new(BUILDER_USER).unwrap(),
        Password::try_new(BUILDER_PASS).unwrap(),
    )
    .await
    .expect("seed the builder");

    let builder = builder_login(&app).await;
    let plain = create_school(&app, &builder, "atakoleji", "secret1").await;
    assert_eq!(plain.status, StatusCode::CREATED, "{:?}", plain.body);
    let plain_id = created_id(&plain);
    assert_eq!(
        school_login(&app, &plain_id, "admin", "secret1")
            .await
            .status,
        StatusCode::OK
    );

    let made = create_school(&app, &builder, "ata-koleji", "secret1").await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);
    let made_id = created_id(&made);
    let listed = |body: &Value, id: &str| school_ids(body).contains(&id.to_string());
    let after_create = bsend(&app, "GET", "/schools", Some(&builder), None).await;
    assert!(listed(&after_create.body, &made_id));
    let login = school_login(&app, &made_id, "admin", "secret1").await;
    assert_eq!(login.status, StatusCode::OK, "the admin logs into the new school");
    let deleted = bsend(
        &app,
        "DELETE",
        &format!("/schools/{made_id}"),
        Some(&builder),
        None,
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{:?}", deleted.body);
    let after_delete = bsend(&app, "GET", "/schools", Some(&builder), None).await;
    assert!(!listed(&after_delete.body, &made_id));
    // The person still has the first school to enter.
    let login_after_delete = school_login(&app, &plain_id, "admin", "secret1").await;
    assert_eq!(login_after_delete.status, StatusCode::OK);
    assert!(
        login_after_delete
            .cookie
            .expect("cookie")
            .contains(&format!("{plain_id}.")),
        "the deleted school must not come back at login"
    );
}



// ---------------------------------------------------------------------------
// Module entitlements: the vendor's shelf. Every assertion here is about the
// same two things — the set a school ends up with, and the fact that a refused
// request leaves it exactly as it was.
// ---------------------------------------------------------------------------

/// The `enabled` list of a modules response, as owned strings.
fn enabled(body: &Value) -> Vec<String> {
    body["enabled"]
        .as_array()
        .expect("enabled list")
        .iter()
        .map(|v| v.as_str().expect("module name").to_string())
        .collect()
}

async fn school_modules(app: &Router, cookie: &str, slug: &str) -> Res {
    bsend(
        app,
        "GET",
        &format!("/schools/{slug}/modules"),
        Some(cookie),
        None,
    )
    .await
}

#[tokio::test]
async fn a_school_starts_with_everything_and_one_module_toggles_back_and_forth() {
    let (app, _db, _tenants) = deployment().await;
    let builder = builder_login(&app).await;

    let listed = school_modules(&app, &builder, DEMO_SCHOOL_ID).await;
    assert_eq!(listed.status, StatusCode::OK, "{:?}", listed.body);
    assert_eq!(enabled(&listed.body).len(), 21, "{:?}", listed.body);
    assert_eq!(
        listed.body["disabled"].as_array().expect("disabled"),
        &[] as &[Value]
    );

    let off = bsend(
        &app,
        "DELETE",
        &format!("/schools/{DEMO_SCHOOL_ID}/modules/notes"),
        Some(&builder),
        None,
    )
    .await;
    assert_eq!(off.status, StatusCode::OK, "{:?}", off.body);
    assert_eq!(off.body["disabled"], json!(["notes"]));
    assert!(!enabled(&off.body).contains(&"notes".to_string()));
    assert_eq!(
        school_modules(&app, &builder, DEMO_SCHOOL_ID).await.body,
        off.body,
        "the GET must show what the DELETE answered"
    );

    // Idempotent both ways: taking back what is already gone changes nothing.
    let again = bsend(
        &app,
        "DELETE",
        &format!("/schools/{DEMO_SCHOOL_ID}/modules/notes"),
        Some(&builder),
        None,
    )
    .await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.body, off.body);

    let on = bsend(
        &app,
        "POST",
        &format!("/schools/{DEMO_SCHOOL_ID}/modules/notes"),
        Some(&builder),
        None,
    )
    .await;
    assert_eq!(on.status, StatusCode::OK, "{:?}", on.body);
    assert_eq!(on.body, listed.body, "back to the full shelf");
    let once_more = bsend(
        &app,
        "POST",
        &format!("/schools/{DEMO_SCHOOL_ID}/modules/notes"),
        Some(&builder),
        None,
    )
    .await;
    assert_eq!(once_more.status, StatusCode::OK);
    assert_eq!(once_more.body, listed.body);

    // A name nobody sells addresses nothing.
    let ghost = bsend(
        &app,
        "POST",
        &format!("/schools/{DEMO_SCHOOL_ID}/modules/kantin"),
        Some(&builder),
        None,
    )
    .await;
    assert_eq!(ghost.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_dependency_is_named_whichever_direction_breaks_it() {
    let (app, _db, _tenants) = deployment().await;
    let builder = builder_login(&app).await;

    // Enabling: a lone school that bought only notes cannot take exams alone.
    let created = bsend(
        &app,
        "POST",
        "/schools",
        Some(&builder),
        Some(json!({
            "slug": "lone",
            "name": "lone school",
            "admin_username": "admin",
            "admin_password": "secret1",
            "modules": ["notes"],
        })),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    let refused = bsend(
        &app,
        "POST",
        "/schools/lone/modules/exams",
        Some(&builder),
        None,
    )
    .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{:?}", refused.body);
    let message = refused.body["error"].as_str().expect("error message");
    assert!(message.contains("courses"), "{message}");
    assert!(message.contains("subjects"), "names every miss: {message}");
    assert_eq!(
        enabled(&school_modules(&app, &builder, "lone").await.body),
        ["notes"]
    );

    // Disabling: what is still needed says so, by name.
    let held = bsend(
        &app,
        "DELETE",
        &format!("/schools/{DEMO_SCHOOL_ID}/modules/exams"),
        Some(&builder),
        None,
    )
    .await;
    assert_eq!(held.status, StatusCode::CONFLICT, "{:?}", held.body);
    assert_eq!(held.body["error"], json!("exams is required by marks"));

    let courses = bsend(
        &app,
        "DELETE",
        &format!("/schools/{DEMO_SCHOOL_ID}/modules/courses"),
        Some(&builder),
        None,
    )
    .await;
    assert_eq!(courses.status, StatusCode::CONFLICT);
    let message = courses.body["error"].as_str().expect("error message");
    for dependent in [
        "course_notes",
        "classes",
        "sessions",
        "exams",
        "subjects",
        "homework",
    ] {
        assert!(
            message.contains(dependent),
            "{dependent} missing from {message}"
        );
    }
    assert_eq!(
        enabled(&school_modules(&app, &builder, DEMO_SCHOOL_ID).await.body).len(),
        21
    );
}

#[tokio::test]
async fn a_batch_is_all_or_nothing_and_packages_round_trip() {
    let (app, _db, _tenants) = deployment().await;
    let builder = builder_login(&app).await;
    let before = school_modules(&app, &builder, DEMO_SCHOOL_ID).await.body.clone();

    // One bad name in a list voids the whole request.
    for (body, field) in [
        (
            json!({ "enable": ["notes"], "disable": ["kantin"] }),
            "module",
        ),
        (json!({ "enable_packages": ["kantin"] }), "package"),
    ] {
        let bad = bsend(
            &app,
            "PATCH",
            &format!("/schools/{DEMO_SCHOOL_ID}/modules"),
            Some(&builder),
            Some(body),
        )
        .await;
        assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{:?}", bad.body);
        let message = bad.body["error"].as_str().expect("error message");
        assert!(
            message.contains("kantin") && message.contains(field),
            "{message}"
        );
        assert_eq!(school_modules(&app, &builder, DEMO_SCHOOL_ID).await.body, before);
    }

    // A name pulled both ways at once has no answer.
    let both = bsend(
        &app,
        "PATCH",
        &format!("/schools/{DEMO_SCHOOL_ID}/modules"),
        Some(&builder),
        Some(json!({ "enable": ["meals"], "disable": ["meals"] })),
    )
    .await;
    assert_eq!(both.status, StatusCode::BAD_REQUEST, "{:?}", both.body);
    assert!(
        both.body["error"]
            .as_str()
            .expect("error")
            .contains("meals"),
        "{:?}",
        both.body
    );
    assert_eq!(school_modules(&app, &builder, DEMO_SCHOOL_ID).await.body, before);

    // A resulting set that breaks a dependency is refused as a whole.
    let orphaned = bsend(
        &app,
        "PATCH",
        &format!("/schools/{DEMO_SCHOOL_ID}/modules"),
        Some(&builder),
        Some(json!({ "disable": ["exams"] })),
    )
    .await;
    assert_eq!(orphaned.status, StatusCode::CONFLICT, "{:?}", orphaned.body);
    assert!(
        orphaned.body["error"]
            .as_str()
            .expect("error")
            .contains("marks requires exams"),
        "{:?}",
        orphaned.body
    );
    assert_eq!(school_modules(&app, &builder, DEMO_SCHOOL_ID).await.body, before);

    // An empty body is a no-op, not an error.
    let empty = bsend(
        &app,
        "PATCH",
        &format!("/schools/{DEMO_SCHOOL_ID}/modules"),
        Some(&builder),
        Some(json!({})),
    )
    .await;
    assert_eq!(empty.status, StatusCode::OK);
    assert_eq!(empty.body, before);

    // A whole package off and back on again, each in one call.
    let sold_back = bsend(
        &app,
        "PATCH",
        &format!("/schools/{DEMO_SCHOOL_ID}/modules"),
        Some(&builder),
        Some(json!({ "disable_packages": ["academics"] })),
    )
    .await;
    assert_eq!(sold_back.status, StatusCode::OK, "{:?}", sold_back.body);
    assert_eq!(
        enabled(&sold_back.body),
        [
            "appointments",
            "boards",
            "chatbot",
            "events",
            "meals",
            "messages",
            "notes",
            "payments",
            "pomodoro",
            "questions",
            "work"
        ]
    );
    assert_eq!(
        school_modules(&app, &builder, DEMO_SCHOOL_ID).await.body,
        sold_back.body
    );

    let resold = bsend(
        &app,
        "PATCH",
        &format!("/schools/{DEMO_SCHOOL_ID}/modules"),
        Some(&builder),
        Some(json!({ "enable_packages": ["academics"] })),
    )
    .await;
    assert_eq!(resold.status, StatusCode::OK, "{:?}", resold.body);
    assert_eq!(resold.body, before, "the shelf came back exactly as it was");
}

/// The entitlement surface is the vendor's alone, and what it writes is read on
/// the school's very next request — no re-login, same cookie.
#[tokio::test]
async fn only_a_builder_sells_modules_and_the_school_feels_it_at_once() {
    let (app, _db, _tenants) = deployment().await;
    let builder = builder_login(&app).await;
    let student = login(&app, "ali").await;

    for (method, uri) in [
        ("GET", format!("/schools/{DEMO_SCHOOL_ID}/modules")),
        ("POST", format!("/schools/{DEMO_SCHOOL_ID}/modules/notes")),
        ("DELETE", format!("/schools/{DEMO_SCHOOL_ID}/modules/notes")),
        ("PATCH", format!("/schools/{DEMO_SCHOOL_ID}/modules")),
    ] {
        let body = (method == "PATCH").then(|| json!({}));
        let res = bsend(&app, method, &uri, Some(&student), body).await;
        assert_eq!(
            res.status,
            StatusCode::UNAUTHORIZED,
            "a school cookie reached {method} {uri}"
        );
    }

    let menus = bsend(&app, "GET", "/meals/menus", Some(&student), None).await;
    assert_eq!(menus.status, StatusCode::OK, "{:?}", menus.body);

    let off = bsend(
        &app,
        "DELETE",
        &format!("/schools/{DEMO_SCHOOL_ID}/modules/meals"),
        Some(&builder),
        None,
    )
    .await;
    assert_eq!(off.status, StatusCode::OK, "{:?}", off.body);

    let refused = bsend(&app, "GET", "/meals/menus", Some(&student), None).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{:?}", refused.body);
    assert_eq!(refused.body["module"], "meals");

    let on = bsend(
        &app,
        "POST",
        &format!("/schools/{DEMO_SCHOOL_ID}/modules/meals"),
        Some(&builder),
        None,
    )
    .await;
    assert_eq!(on.status, StatusCode::OK, "{:?}", on.body);
    assert_eq!(
        bsend(&app, "GET", "/meals/menus", Some(&student), None)
            .await
            .status,
        StatusCode::OK,
        "the same cookie works again the moment the module is back"
    );
}

/// A school is sold its shelf as it is created, and an unsatisfiable order
/// creates nothing at all.
#[tokio::test]
async fn a_school_is_created_with_the_modules_it_bought() {
    let (app, _db, _tenants) = deployment().await;
    let builder = builder_login(&app).await;

    let created = bsend(
        &app,
        "POST",
        "/schools",
        Some(&builder),
        Some(json!({
            "slug": "notes-only",
            "name": "notes-only school",
            "admin_username": "admin",
            "admin_password": "secret1",
            "modules": ["notes"],
        })),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    assert_eq!(created.body["modules"], json!(["notes"]));
    let read = bsend(&app, "GET", "/schools/notes-only", Some(&builder), None).await;
    assert_eq!(read.body["modules"], json!(["notes"]));
    assert_eq!(
        enabled(&school_modules(&app, &builder, "notes-only").await.body),
        ["notes"]
    );

    for (body, status) in [
        (json!({ "modules": ["kantin"] }), StatusCode::BAD_REQUEST),
        (json!({ "modules": ["exams"] }), StatusCode::CONFLICT),
    ] {
        let mut request = json!({
            "slug": "doomed",
            "name": "doomed school",
            "admin_username": "admin",
            "admin_password": "secret1",
        });
        request["modules"] = body["modules"].clone();
        let res = bsend(&app, "POST", "/schools", Some(&builder), Some(request)).await;
        assert_eq!(res.status, status, "{:?}", res.body);
        assert_eq!(
            bsend(&app, "GET", "/schools/doomed", Some(&builder), None)
                .await
                .status,
            StatusCode::NOT_FOUND,
            "a refused order must not leave a school behind"
        );
    }

    // Omitted still means everything.
    let full = create_school(&app, &builder, "full-shelf", "secret1").await;
    assert_eq!(full.status, StatusCode::CREATED, "{:?}", full.body);
    assert_eq!(full.body["modules"].as_array().expect("modules").len(), 21);
}

/// The catalog is the client's map of the product, and `/modules` is where a
/// school user reads its own square of it.
#[tokio::test]
async fn the_catalog_is_public_and_a_school_user_reads_its_own_set() {
    let (app, _db, _tenants) = deployment().await;

    let catalog = bsend(&app, "GET", "/modules/catalog", None, None).await;
    assert_eq!(catalog.status, StatusCode::OK, "{:?}", catalog.body);
    let modules = catalog.body["modules"].as_array().expect("modules").clone();
    assert_eq!(modules.len(), 21);
    let names: Vec<&str> = modules
        .iter()
        .map(|m| m["module"].as_str().expect("module name"))
        .collect();
    assert!(names.windows(2).all(|w| w[0] < w[1]), "sorted: {names:?}");
    let packages = catalog.body["packages"].as_array().expect("packages");
    assert_eq!(packages.len(), 4);
    for module in &modules {
        assert!(
            packages.iter().any(|p| p["package"] == module["package"]),
            "{} is sold in a package nobody lists",
            module["module"]
        );
        for needed in module["requires"].as_array().expect("requires") {
            assert!(
                names.contains(&needed.as_str().expect("requirement name")),
                "{module:?} requires something the catalog does not sell"
            );
        }
    }

    let student = login(&app, "ali").await;
    let mine = bsend(&app, "GET", "/modules", Some(&student), None).await;
    assert_eq!(mine.status, StatusCode::OK, "{:?}", mine.body);
    assert_eq!(enabled(&mine.body), names);
    assert_eq!(
        bsend(&app, "GET", "/modules", None, None).await.status,
        StatusCode::UNAUTHORIZED
    );
}

// ===================== REFUTE probes (verifier, 2026-09-06) =====================
// Appended by an adversarial verification pass. These probe the module
// entitlement package's stated invariants; they touch no src/.

/// P1: a school that bought nothing can still reach every ungated surface.
#[tokio::test]
async fn probe_a_module_less_school_still_works() {
    let (app, _db, _tenants) = deployment().await;
    let builder = builder_login(&app).await;
    let created = bsend(
        &app,
        "POST",
        "/schools",
        Some(&builder),
        Some(json!({
            "slug": "bare",
            "name": "bare school",
            "admin_username": "admin",
            "admin_password": "secret1",
            "modules": [],
        })),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    assert_eq!(created.body["modules"], json!([]));

    let login = school_login(&app, "bare", "admin", "secret1").await;
    assert_eq!(login.status, StatusCode::OK, "login: {:?}", login.body);
    let cookie = login.cookie.expect("session cookie");

    for path in [
        "/auth/me",
        "/limits",
        "/settings",
        "/terms",
        "/users/me/profile",
        "/modules",
        "/modules/catalog",
    ] {
        let res = bsend(&app, "GET", path, Some(&cookie), None).await;
        assert_eq!(
            res.status,
            StatusCode::OK,
            "GET {path} on a module-less school -> {} {:?}",
            res.status,
            res.body
        );
    }
    let mine = bsend(&app, "GET", "/modules", Some(&cookie), None).await;
    assert_eq!(mine.body["enabled"], json!([]));

    let prefs = bsend(
        &app,
        "PATCH",
        "/users/me/preferences",
        Some(&cookie),
        Some(json!({ "theme": "dark" })),
    )
    .await;
    assert_eq!(
        prefs.status,
        StatusCode::OK,
        "PATCH /users/me/preferences -> {:?}",
        prefs.body
    );
}

/// P1b: the same, reached by narrowing a full school with `disable_packages`.
#[tokio::test]
async fn probe_narrowing_every_package_away_leaves_the_school_usable() {
    let (app, db, _tenants) = deployment().await;
    let builder = builder_login(&app).await;
    let cookie = login_as(&app, &db, "boss", "admin").await;

    let res = bsend(
        &app,
        "PATCH",
        &format!("/schools/{DEMO_SCHOOL_ID}/modules"),
        Some(&builder),
        Some(json!({
            "disable_packages": ["academics", "communication", "operations", "ai"],
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{:?}", res.body);
    assert_eq!(res.body["enabled"], json!([]), "{:?}", res.body);

    for path in [
        "/auth/me",
        "/limits",
        "/settings",
        "/terms",
        "/users/me/profile",
        "/modules",
    ] {
        let r = bsend(&app, "GET", path, Some(&cookie), None).await;
        assert_eq!(
            r.status,
            StatusCode::OK,
            "GET {path} -> {} {:?}",
            r.status,
            r.body
        );
    }
    assert_eq!(
        bsend(&app, "GET", "/modules", Some(&cookie), None)
            .await
            .body["enabled"],
        json!([])
    );
}

/// P2: a fully-enabled school must never meet `module disabled` anywhere.
#[tokio::test]
async fn probe_no_route_refuses_a_module_a_school_has() {
    let (app, db, _tenants) = deployment().await;
    let cookie = login_as(&app, &db, "boss", "admin").await;

    let spec = bsend(&app, "GET", "/api-docs/openapi.json", None, None).await;
    assert_eq!(spec.status, StatusCode::OK, "openapi spec");
    let paths = spec.body["paths"].as_object().expect("paths object");
    assert!(paths.len() > 50, "only {} paths in the spec", paths.len());

    let mut refused = Vec::new();
    let mut swept = 0usize;
    for path in paths.keys() {
        if !paths[path].get("get").is_some() {
            continue;
        }
        let concrete = path
            .split('/')
            .map(|seg| {
                if seg.starts_with('{') && seg.ends_with('}') {
                    "x"
                } else {
                    seg
                }
            })
            .collect::<Vec<_>>()
            .join("/");
        let res = bsend(&app, "GET", &concrete, Some(&cookie), None).await;
        swept += 1;
        if res.body.get("error").and_then(|e| e.as_str()) == Some("module disabled") {
            refused.push(format!("{concrete} -> {} {:?}", res.status, res.body));
        }
    }
    assert!(swept > 50, "only {swept} GET routes swept");
    assert!(
        refused.is_empty(),
        "{} of {swept} GET routes answered `module disabled` on a fully enabled school: {:#?}",
        refused.len(),
        refused
    );
}

/// P6: the gate must not answer before authentication — no cookie means no
/// school, so nothing about a school's shelf may leak.
#[tokio::test]
async fn probe_gate_ordering_versus_auth() {
    let (app, _db, _tenants) = deployment().await;
    let builder = builder_login(&app).await;
    let off = bsend(
        &app,
        "DELETE",
        &format!("/schools/{DEMO_SCHOOL_ID}/modules/meals"),
        Some(&builder),
        None,
    )
    .await;
    assert_eq!(off.status, StatusCode::OK, "{:?}", off.body);

    let anon = bsend(&app, "GET", "/meals/menus", None, None).await;
    assert_eq!(
        anon.status,
        StatusCode::UNAUTHORIZED,
        "cookie-less request to a disabled nest -> {} {:?}",
        anon.status,
        anon.body
    );
    assert_ne!(
        anon.body.get("error").and_then(|e| e.as_str()),
        Some("module disabled"),
        "a cookie-less caller learned a school's entitlements: {:?}",
        anon.body
    );

    let as_builder = bsend(&app, "GET", "/meals/menus", Some(&builder), None).await;
    assert_eq!(
        as_builder.status,
        StatusCode::UNAUTHORIZED,
        "builder cookie inside a school nest -> {} {:?}",
        as_builder.status,
        as_builder.body
    );
}

/// P7: one school's disabled module says nothing about another's.
#[tokio::test]
async fn probe_disabling_in_one_school_leaves_the_other_alone() {
    let (app, _db, tenants) = deployment().await;
    let builder = builder_login(&app).await;
    for slug in ["alpha", "beta"] {
        let res = create_school(&app, &builder, slug, "secret1").await;
        assert_eq!(res.status, StatusCode::CREATED, "{slug}: {:?}", res.body);
    }
    async fn named(tenants: &Tenants, label: &str) -> (hezarfen_backend::tenant::SchoolId, hezarfen_backend::database::Database) {
        let id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM school WHERE name = $1")
            .bind(format!("{label} school"))
            .fetch_one(tenants.control())
            .await
            .unwrap_or_else(|err| panic!("{label}: {err}"));
        let id = hezarfen_backend::tenant::SchoolId::from_uuid(id);
        let db = tenants.get(&id).await.unwrap();
        (id, db)
    }
    let (alpha_id, alpha_db) = named(&tenants, "alpha").await;
    let (beta_id, beta_db) = named(&tenants, "beta").await;
    let alpha_wire = alpha_id.as_str();
    let beta_wire = beta_id.as_str();
    let alpha = login_as_school(&app, &alpha_db, &alpha_wire, "ada", "manager").await;
    let beta = login_as_school(&app, &beta_db, &beta_wire, "ada", "manager").await;

    let off = bsend(
        &app,
        "DELETE",
        "/schools/alpha/modules/meals",
        Some(&builder),
        None,
    )
    .await;
    assert_eq!(off.status, StatusCode::OK, "{:?}", off.body);

    let a = bsend(&app, "GET", "/meals/menus", Some(&alpha), None).await;
    assert_eq!(a.status, StatusCode::FORBIDDEN, "{:?}", a.body);
    assert_eq!(a.body["module"], "meals");
    let b = bsend(&app, "GET", "/meals/menus", Some(&beta), None).await;
    assert_eq!(
        b.status,
        StatusCode::OK,
        "school B lost meals with school A: {} {:?}",
        b.status,
        b.body
    );
}

/// P5: a contradictory batch is a refusal that writes nothing — checked on the
/// registry row itself, not through the API's own read.
#[tokio::test]
async fn probe_a_refused_batch_leaves_the_row_byte_identical() {
    let (app, _db, tenants) = deployment().await;
    let builder = builder_login(&app).await;

    async fn raw_row(tenants: &Tenants) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT sm.module FROM school_module sm
             JOIN school s ON s.id = sm.school
             WHERE s.id = $1 ORDER BY sm.module",
        )
        .bind(hezarfen_backend::tenant::SchoolId::try_parse(DEMO_SCHOOL_ID).unwrap().uuid())
        .fetch_all(tenants.control())
        .await
        .expect("raw read")
    }

    let before = raw_row(&tenants).await;
    let res = bsend(
        &app,
        "PATCH",
        &format!("/schools/{DEMO_SCHOOL_ID}/modules"),
        Some(&builder),
        Some(json!({ "enable": ["exams"], "disable": ["courses"] })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CONFLICT,
        "enable exams + disable courses -> {} {:?}",
        res.status,
        res.body
    );
    let after = raw_row(&tenants).await;
    assert_eq!(before, after, "the refused batch moved the stored row");
}

/// P4 (attempt): count the control database's reads for one gated request by
/// counting the SDK's own tracing spans. Prints the figure; asserts only that
/// the instrumentation saw *something*, so a zero is reported as UNMEASURED
/// rather than as a passing measurement.
///
/// The subscriber is **global**, not `set_default`'s thread-local one, because
/// a thread-local subscriber cannot measure this reliably: `tracing` caches a
/// callsite's interest process-wide the first time any thread reaches it, and
/// with tests running in parallel that thread is usually a sibling with no
/// subscriber — the sqlx callsites are then disabled for the whole process
/// before this test ever asks, and the count is 0. Global is safe here because
/// this subscriber only counts: it stores nothing, prints nothing, and its
/// `enabled` refuses every callsite that is not a sqlx one on *this* test's
/// thread, so a sibling test neither pays for it nor lands in the figure.
#[tokio::test]
async fn probe_registry_reads_per_request() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tracing::span;

    struct Counter {
        thread: std::thread::ThreadId,
        count: Arc<AtomicUsize>,
    }
    impl tracing::Subscriber for Counter {
        // Never `always`/`never`: those are cached per callsite, and this
        // subscriber's answer depends on which thread is asking.
        fn register_callsite(&self, _: &tracing::Metadata<'_>) -> tracing::subscriber::Interest {
            tracing::subscriber::Interest::sometimes()
        }
        fn enabled(&self, meta: &tracing::Metadata<'_>) -> bool {
            meta.target().starts_with("sqlx") && std::thread::current().id() == self.thread
        }
        fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
            self.count.fetch_add(1, Ordering::Relaxed);
            span::Id::from_u64(1)
        }
        fn event(&self, _: &tracing::Event<'_>) {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
        fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
        fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
        fn enter(&self, _: &span::Id) {}
        fn exit(&self, _: &span::Id) {}
    }

    let count = Arc::new(AtomicUsize::new(0));
    tracing::subscriber::set_global_default(Counter {
        thread: std::thread::current().id(),
        count: count.clone(),
    })
    .expect("no other test in this binary installs a global subscriber");

    let (app, db, _tenants) = deployment().await;
    let cookie = login_as(&app, &db, "boss", "admin").await;
    count.store(0, Ordering::Relaxed);
    let res = bsend(&app, "GET", "/notes", Some(&cookie), None).await;
    assert_eq!(res.status, StatusCode::OK, "{:?}", res.body);
    let seen = count.load(Ordering::Relaxed);
    println!("PROBE sqlx spans/events for GET /notes = {seen}");
    assert!(
        seen > 0,
        "UNMEASURED: sqlx emitted no tracing spans, so per-request \
         registry reads cannot be counted this way"
    );
}

/// P8: the foreign route pairs nested under another module's nest carry both
/// gates, and a disabled nest still `404`s an unmatched path inside it.
#[tokio::test]
async fn probe_child_gates_under_courses_and_the_404_inside_a_disabled_nest() {
    let (app, db, _tenants) = deployment().await;
    let builder = builder_login(&app).await;
    let cookie = login_as(&app, &db, "boss", "admin").await;

    // Exams off, courses still on: the child pair must refuse as `exams`.
    for module in ["marks", "exams"] {
        let off = bsend(
            &app,
            "DELETE",
            &format!("/schools/{DEMO_SCHOOL_ID}/modules/{module}"),
            Some(&builder),
            None,
        )
        .await;
        assert_eq!(off.status, StatusCode::OK, "{module}: {:?}", off.body);
    }
    let child = bsend(&app, "GET", "/instances/x/exams", Some(&cookie), None).await;
    assert_eq!(
        child.status,
        StatusCode::FORBIDDEN,
        "/instances/x/exams with exams off -> {} {:?}",
        child.status,
        child.body
    );
    assert_eq!(child.body["module"], "exams", "{:?}", child.body);
    // …while the course routes themselves keep answering.
    let parent = bsend(&app, "GET", "/courses", Some(&cookie), None).await;
    assert_eq!(parent.status, StatusCode::OK, "{:?}", parent.body);
    // A sibling child pair whose own module is still on is untouched.
    let sibling = bsend(&app, "GET", "/courses/x/subjects", Some(&cookie), None).await;
    assert_ne!(
        sibling.status,
        StatusCode::FORBIDDEN,
        "/courses/x/subjects lost its own gate: {:?}",
        sibling.body
    );

    // A disabled nest is a refusal on the routes that exist, not a wall.
    let off = bsend(
        &app,
        "DELETE",
        &format!("/schools/{DEMO_SCHOOL_ID}/modules/meals"),
        Some(&builder),
        None,
    )
    .await;
    assert_eq!(off.status, StatusCode::OK, "{:?}", off.body);
    let ghost = bsend(&app, "GET", "/meals/no-such-route", Some(&cookie), None).await;
    assert_eq!(
        ghost.status,
        StatusCode::NOT_FOUND,
        "an unmatched path inside a disabled nest -> {} {:?}",
        ghost.status,
        ghost.body
    );
    let real = bsend(&app, "GET", "/meals/menus", Some(&cookie), None).await;
    assert_eq!(real.status, StatusCode::FORBIDDEN, "{:?}", real.body);
}
