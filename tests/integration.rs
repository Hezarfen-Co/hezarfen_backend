//! Integration tests: drive the assembled axum router directly (no network)
//! against a fresh in-memory SurrealDB, via `tower::ServiceExt::oneshot`.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    app_and_db, app_with_ai_health, create_course, create_exam, create_exam_with, create_homework,
    create_session, create_subject, enroll, id_of, login, login_as, me_id, mem_app, send, set_role,
    unenroll, upload_course_note_file,
};
use hezarfen_backend::build_router;
use hezarfen_backend::constant::{
    BANK_VISIBILITY_SCHOOL, MAX_BOARD_STROKES, MAX_BOARDS_PER_CREATOR, MAX_COURSE_NOTE_FILES,
    MAX_EPOCH_STROKES, MAX_FEE_PLAN_ASSIGN_STUDENTS,
};
use hezarfen_backend::db::exam_attempt::any_for_exam;
use hezarfen_backend::db::session;
use hezarfen_backend::domain::board::{Board, BoardId};
use hezarfen_backend::domain::board_stroke::{BoardStroke, BoardStrokeId};
use hezarfen_backend::domain::chatbot_message::ChatbotMessage;
use hezarfen_backend::domain::chatbot_thread::ChatbotThreadId;
use hezarfen_backend::domain::course::CourseId;
use hezarfen_backend::domain::course_note::CourseNoteId;
use hezarfen_backend::domain::course_note_file::CourseNoteFileId;
use hezarfen_backend::domain::exam::ExamId;
use hezarfen_backend::domain::rag_output::RagOutput;
use hezarfen_backend::domain::timestamp::Timestamp;
use hezarfen_backend::domain::user::{Password, User, UserId, Username};
use hezarfen_backend::module::ModuleSet;
use hezarfen_backend::state::{AppState, DbHealth};
use serde_json::json;
use tower::ServiceExt;

// --- health + auth -------------------------------------------------------

#[tokio::test]
async fn health_reports_ok() {
    let app = mem_app().await;
    let res = send(&app, "GET", "/health", None, None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["status"], "ok");
    assert_eq!(res.body["db"], "up");
    // No `AI_QUIC_ADDR` in the suites, so the bridge is off — which is a
    // configuration, not a fault: the status stays `ok`.
    assert_eq!(res.body["ai"]["enabled"], false);
    assert_eq!(res.body["ai"]["workers"], 0);
}

/// The probe must survive the outage it reports. `db_guard` refuses everything
/// else with a generic 503 while the socket is down; `/health` (and `/`) answer
/// their own 503 that names the database, or the probe is useless exactly when
/// it is needed.
#[tokio::test]
async fn health_reports_the_database_down_instead_of_being_refused() {
    let db_up = DbHealth::default();
    let (app, _db) = app_with_ai_health(None, db_up.clone()).await;

    db_up.set(false);
    for path in ["/health", "/"] {
        let res = send(&app, "GET", path, None, None).await;
        assert_eq!(res.status, StatusCode::SERVICE_UNAVAILABLE, "{path}");
        assert_eq!(res.body["status"], "degraded", "{path}");
        assert_eq!(res.body["db"], "down", "{path}");
    }

    db_up.set(true);
    let res = send(&app, "GET", "/health", None, None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["db"], "up");
}

#[tokio::test]
async fn time_serves_server_clock_without_auth() {
    let app = mem_app().await;

    let before = Timestamp::now().as_millis();
    let res = send(&app, "GET", "/time", None, None).await;
    let after = Timestamp::now().as_millis();

    assert_eq!(res.status, StatusCode::OK);
    // Bracketed by two local reads: proves it is the same clock in millis,
    // not seconds/nanos or some other epoch.
    let now = res.body["now"].as_i64().expect("now is an integer");
    assert!(before <= now && now <= after);
}

#[tokio::test]
async fn limits_publishes_the_bounds_the_api_actually_enforces() {
    let app = mem_app().await;

    let res = send(&app, "GET", "/limits", None, None).await;
    assert_eq!(res.status, StatusCode::OK);

    // Every group is present — a frontend reads one document, not thirteen.
    for group in [
        "user",
        "note",
        "file",
        "message",
        "event",
        "course",
        "exam",
        "homework",
        "question_pool",
        "appointment",
        "chatbot",
        "settings",
        "request",
    ] {
        assert!(res.body[group].is_object(), "missing group: {group}");
    }

    // The published value must equal the one the constants carry — the whole
    // point is that the frontend stops keeping its own copy.
    assert_eq!(
        res.body["user"]["max_username_len"],
        json!(hezarfen_backend::constant::MAX_USERNAME_LEN)
    );
    assert_eq!(res.body["exam"]["modes"], json!(["sync", "async", "open"]));
    // The accent color is published as a pattern, not a value list — it is an
    // open set, so a client validates the shape instead of a fixed palette.
    assert_eq!(
        res.body["user"]["palette_color_pattern"],
        json!(hezarfen_backend::constant::PALETTE_COLOR_PATTERN)
    );
    assert_eq!(
        res.body["user"]["palette_color_len"],
        json!(hezarfen_backend::constant::PALETTE_COLOR_LEN)
    );
    assert_eq!(
        res.body["user"]["roles"],
        json!(["parent", "student", "teacher", "manager", "admin"])
    );

    // The request budgets a caller is actually held to, read live off this
    // server rather than from a constant — a client should learn its budget
    // here, not by collecting a 429.
    assert!(res.body["rate"]["auth_per_minute"].is_number());
    assert!(res.body["rate"]["api_per_minute"].is_number());
    assert!(res.body["rate"]["chatbot_per_minute"].is_number());
    assert_eq!(res.body["rate"]["window_secs"], json!(60));

    // Structural sanity over EVERY published pair, which is what a spot-check
    // of a few named fields cannot give: any `min_x` must not exceed its
    // `max_x`. This is what catches a mapping that wired a floor to a ceiling
    // — the one mistake a "does the constant appear?" check cannot see.
    let groups = res.body.as_object().expect("limits is an object");
    let mut checked = 0;
    for (group, fields) in groups {
        let fields = fields.as_object().expect("each group is an object");
        for (key, value) in fields {
            let Some(bare) = key.strip_prefix("min_") else {
                continue;
            };
            let Some(max) = fields.get(&format!("max_{bare}")) else {
                continue;
            };
            let (min, max) = (
                value.as_i64().expect("min is an integer"),
                max.as_i64().expect("max is an integer"),
            );
            assert!(
                min <= max,
                "{group}.{key} ({min}) exceeds its maximum ({max}) — the pair is inverted"
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 10,
        "only {checked} min/max pairs were checked — the pairing broke, not the values"
    );

    // …and the endpoint that enforces it must agree: one over the published
    // maximum is refused. This is what catches a constant moving without the
    // limits response, or the reverse.
    let max = res.body["user"]["max_username_len"]
        .as_u64()
        .expect("max_username_len is an integer") as usize;
    let too_long =
        json!({ "school": "demo", "username": "a".repeat(max + 1), "password": "secret1" });
    let res = send(&app, "POST", "/auth/register", None, Some(too_long)).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn limits_still_answers_while_the_database_is_down() {
    // A frontend booting against a backend whose database is reconnecting is
    // precisely when it needs the validation contract. `/limits` touches no
    // row, so the outage guard must let it through — otherwise the client
    // falls back to the hard-coded copy this endpoint exists to remove.
    let db_up = hezarfen_backend::state::DbHealth::default();
    let (tenants, _db) = common::mem_deployment().await;
    let app = build_router(AppState {
        db: tenants.control().clone(),
        tenants,
        files_path: tempfile::tempdir().expect("files dir").keep(),
        cookie_secure: false,
        rate_limit: hezarfen_backend::rate_limit::RateLimitConfig::unlimited(),
        chatbot_limit: Default::default(),
        exam_presence: Default::default(),
        board_hub: Default::default(),
        db_up: db_up.clone(),
        ai: None,
        metrics: hezarfen_backend::telemetry::Metrics::noop(),
    });
    db_up.set(false);

    let res = send(&app, "GET", "/limits", None, None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body["user"]["max_username_len"].is_number());

    // Everything that does touch the database still refuses, so the exemption
    // is scoped to the one static route and has not disarmed the guard.
    let res = send(&app, "GET", "/settings", None, None).await;
    assert_eq!(res.status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn register_validates_input() {
    let app = mem_app().await;

    // Password too short -> 400.
    let short = json!({ "school": "demo", "username": "bob", "password": "123" });
    let res = send(&app, "POST", "/auth/register", None, Some(short)).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // Blank username -> 400.
    let blank = json!({ "school": "demo", "username": "   ", "password": "secret1" });
    let res = send(&app, "POST", "/auth/register", None, Some(blank)).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // Uppercase anywhere in the username -> 400.
    for bad in ["Bob", "bOb", "BOB"] {
        let upper = json!({ "school": "demo", "username": bad, "password": "secret1" });
        let res = send(&app, "POST", "/auth/register", None, Some(upper)).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{bad} accepted");
    }

    // Username must start and end with a letter or digit -> 400.
    for bad in ["-bob", "bob-", "_bob", "bob_", ".bob"] {
        let edge = json!({ "school": "demo", "username": bad, "password": "secret1" });
        let res = send(&app, "POST", "/auth/register", None, Some(edge)).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{bad} accepted");
    }

    // Interior characters are an allowlist: lowercase, digits, . _ - only.
    for bad in [
        "a b",
        "a\nb",
        "a\u{1b}[31mb",
        "a<b>c",
        "a@b.c",
        "a/b",
        "a\"b",
    ] {
        let ugly = json!({ "school": "demo", "username": bad, "password": "secret1" });
        let res = send(&app, "POST", "/auth/register", None, Some(ugly)).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{bad:?} accepted");
    }

    // Consecutive separators -> 400.
    for bad in ["a--b", "a..b", "a__b", "a.-b"] {
        let doubled = json!({ "school": "demo", "username": bad, "password": "secret1" });
        let res = send(&app, "POST", "/auth/register", None, Some(doubled)).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{bad} accepted");
    }

    // Single separators between alphanumerics are fine -> 201.
    let dotted = json!({ "school": "demo", "username": "ali.k_1-b", "password": "secret1" });
    let res = send(&app, "POST", "/auth/register", None, Some(dotted)).await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["username"], "ali.k_1-b");

    // Valid -> 201, no password echoed back, defaults to the student role.
    let ok = json!({ "school": "demo", "username": "bob", "password": "secret1" });
    let res = send(&app, "POST", "/auth/register", None, Some(ok.clone())).await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["username"], "bob");
    assert_eq!(res.body["role"], "student");
    assert!(res.body.get("password_hash").is_none());

    // A duplicate answers 201 like any other register (the "taken" reply is
    // deliberately indistinguishable — see tests/auth_enumeration.rs), but the
    // write is still rejected: the original password keeps working and the
    // second one never becomes valid.
    let dup = json!({ "school": "demo", "username": "bob", "password": "hijack1" });
    let res = send(&app, "POST", "/auth/register", None, Some(dup.clone())).await;
    assert_eq!(res.status, StatusCode::CREATED);
    let res = send(&app, "POST", "/auth/login", None, Some(ok)).await;
    assert_eq!(
        res.status,
        StatusCode::OK,
        "original password stopped working"
    );
    let res = send(&app, "POST", "/auth/login", None, Some(dup)).await;
    assert_eq!(
        res.status,
        StatusCode::UNAUTHORIZED,
        "the duplicate register overwrote the account"
    );
}

#[tokio::test]
async fn register_rejects_reserved_usernames() {
    let app = mem_app().await;

    // Staff-looking names can't be claimed through public registration.
    for name in [
        "admin",
        "administrator",
        "root",
        "support",
        "system",
        "moderator",
        "staff",
    ] {
        let creds = json!({ "school": "demo", "username": name, "password": "secret1" });
        let res = send(&app, "POST", "/auth/register", None, Some(creds)).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{name} accepted");
        let msg = res.body["error"].as_str().unwrap_or_default();
        assert!(
            msg.contains("reserved"),
            "unexpected error for {name}: {msg}"
        );
    }

    // The reservation is registration-only policy: the seeded bootstrap admin
    // (created through `ensure_admin`, not `/auth/register`) still logs in.
    // Covered by the admin bootstrap tests; here just prove a reserved name
    // is not permanently poisoned for login by the register-level check.
    let creds = json!({ "school": "demo", "username": "admin", "password": "wrong" });
    let res = send(&app, "POST", "/auth/login", None, Some(creds)).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED); // bad credentials, not "reserved"
}

#[tokio::test]
async fn login_rejects_bad_credentials() {
    let app = mem_app().await;
    send(
        &app,
        "POST",
        "/auth/register",
        None,
        Some(json!({ "school": "demo", "username": "kate", "password": "secret1" })),
    )
    .await;

    // Wrong password.
    let res = send(
        &app,
        "POST",
        "/auth/login",
        None,
        Some(json!({ "school": "demo", "username": "kate", "password": "wrong" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);

    // Unknown user.
    let res = send(
        &app,
        "POST",
        "/auth/login",
        None,
        Some(json!({ "school": "demo", "username": "ghost", "password": "secret1" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
}

/// Routes that answer without a session, on purpose. Adding a route here is a
/// deliberate, reviewable decision to leave a door open — everything else in
/// the spec is asserted to reject an anonymous caller with a 401.
const PUBLIC: &[(&str, &str)] = &[
    ("GET", "/health"),
    ("GET", "/time"),
    ("GET", "/limits"),
    // The deployment's product catalog: which modules exist, what each needs,
    // how they are packaged. Deploy-constant and identical for every school —
    // public for the same reason `/limits` is. What one school *bought* is
    // `GET /modules`, which is behind a session.
    ("GET", "/modules/catalog"),
    ("POST", "/auth/register"),
    ("POST", "/auth/login"),
    // Idempotent: revokes the session if there is one, `204` either way.
    ("POST", "/auth/logout"),
    // A server certificate is handed to every peer in the TLS handshake, so
    // publishing it discloses nothing (see the module doc in `web/ai.rs`).
    ("GET", "/ai/certificate"),
    // The vendor's own credential endpoint: the door a builder session comes
    // out of, behind the same strict per-IP tier `/auth/login` sits behind.
    ("POST", "/builder/login"),
    // Idempotent, like `/auth/logout`: revokes the builder session if there is
    // one, `204` either way.
    ("POST", "/builder/logout"),
];

/// Router paths that carry no OpenAPI operation, so the derived sweep below
/// cannot see them: `OpenApiRouter` has no way to describe a WebSocket
/// upgrade, so both rooms are plain `.route()`s. They authenticate before they
/// upgrade, so a rejection is an ordinary HTTP status here.
const UNDOCUMENTED_WS: &[(&str, &str)] = &[("GET", "/exams/x/attempt/ws"), ("GET", "/boards/x/ws")];

/// Every protected operation the server publishes must reject a caller with no
/// cookie — derived from the emitted OpenAPI document, never from a list
/// somebody keeps by hand. A hand-kept list drifts silently, and the drift is
/// auth coverage: ~10 route families were missing from the one this replaced.
///
/// Route and spec entry come from the same `routes!()` macro, so a family that
/// leaves the router leaves the document too; `MIN_PROTECTED` is what makes
/// that visible instead of a quietly shorter sweep.
#[tokio::test]
async fn protected_routes_require_session() {
    /// Floor on the number of protected operations swept. It only ever goes
    /// up: raise it when routes are added. Without it, deleting a route family
    /// would delete its own coverage and still pass.
    const MIN_PROTECTED: usize = 268;

    let app = mem_app().await;
    let spec = send(&app, "GET", "/api-docs/openapi.json", None, None)
        .await
        .body;
    let paths = spec["paths"].as_object().expect("spec has paths");

    let mut checked = 0;
    for (path, item) in paths {
        let item = item.as_object().expect("path item is an object");
        for (method, operation) in item {
            let secured = operation["security"]
                .as_array()
                .is_some_and(|requirements| {
                    requirements
                        .iter()
                        .any(|requirement| requirement.get("session_cookie").is_some())
                });
            let method = method.to_uppercase();
            if !secured {
                assert!(
                    PUBLIC.contains(&(method.as_str(), path.as_str())),
                    "{method} {path} declares no session requirement and is not \
                     in the deliberate PUBLIC list"
                );
                continue;
            }

            // `{id}` → `x`, but an integer-typed parameter (a choice index)
            // has to stay parseable or axum answers 400 before the extractor
            // that would have answered 401.
            let mut uri = path.clone();
            for parameter in operation["parameters"].as_array().unwrap_or(&vec![]) {
                let name = parameter["name"].as_str().expect("parameter name");
                let placeholder = if parameter["schema"]["type"] == "integer" {
                    "0"
                } else {
                    "x"
                };
                uri = uri.replace(&format!("{{{name}}}"), placeholder);
            }
            assert!(!uri.contains('{'), "unsubstituted path parameter: {uri}");

            let res = send(&app, &method, &uri, None, None).await;
            assert_eq!(res.status, StatusCode::UNAUTHORIZED, "{method} {uri}");
            checked += 1;
        }
    }

    for (method, uri) in UNDOCUMENTED_WS {
        let res = send(&app, method, uri, None, None).await;
        assert_eq!(res.status, StatusCode::UNAUTHORIZED, "{method} {uri}");
    }

    assert!(
        checked >= MIN_PROTECTED,
        "swept only {checked} protected operations, expected at least \
         {MIN_PROTECTED} — did a route family leave the router?"
    );
}

/// The AI allowlist is a hand-kept list of router paths, so it can only drift
/// one way: a route is renamed and every entry naming it silently stops
/// matching, leaving the AI services with a scope that quietly shrank. Pinning
/// each entry to a `GET` in the emitted document makes the rename break here
/// instead of out there.
#[tokio::test]
async fn ai_allowlist_entries_are_real_get_routes() {
    let app = mem_app().await;
    let spec = send(&app, "GET", "/api-docs/openapi.json", None, None)
        .await
        .body;
    let paths = spec["paths"].as_object().expect("spec has paths");

    for entry in hezarfen_backend::constant::AI_API_ALLOWLIST {
        let item = paths
            .get(*entry)
            .unwrap_or_else(|| panic!("allowlisted {entry} is not a path in the OpenAPI document"));
        assert!(
            item.get("get").is_some(),
            "allowlisted {entry} exists but serves no GET"
        );
    }
}

#[tokio::test]
async fn garbage_cookie_is_unauthorized() {
    let app = mem_app().await;
    let res = send(&app, "GET", "/auth/me", Some("session=deadbeef"), None).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn me_returns_current_user_without_secrets() {
    let app = mem_app().await;
    let ali = login(&app, "ali").await;
    let res = send(&app, "GET", "/auth/me", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["username"], "ali");
    assert_eq!(res.body["role"], "student");
    assert!(res.body.get("password_hash").is_none());
}

#[tokio::test]
async fn logout_invalidates_the_session_server_side() {
    let app = mem_app().await;
    let ali = login(&app, "ali").await;

    assert_eq!(
        send(&app, "GET", "/auth/me", Some(&ali), None).await.status,
        StatusCode::OK
    );

    let res = send(&app, "POST", "/auth/logout", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);

    // Re-using the same token now fails: the session row is gone.
    let res = send(&app, "GET", "/auth/me", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);
}

// --- notes ---------------------------------------------------------------

#[tokio::test]
async fn notes_crud_is_scoped_to_owner() {
    let app = mem_app().await;
    let ali = login(&app, "ali").await;
    let veli = login(&app, "veli").await;

    let res = send(
        &app,
        "POST",
        "/notes",
        Some(&ali),
        Some(json!({ "title": "first", "content": "hello" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let note_id = id_of(&res.body);

    // Partial update keeps the untouched field.
    let res = send(
        &app,
        "PATCH",
        &format!("/notes/{note_id}"),
        Some(&ali),
        Some(json!({ "content": "edited" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["content"], "edited");
    assert_eq!(res.body["title"], "first");

    // Owner sees exactly one; other user sees none and gets 404 on direct access.
    assert_eq!(
        common::items(&send(&app, "GET", "/notes", Some(&ali), None).await.body).len(),
        1
    );
    assert_eq!(
        send(&app, "GET", &format!("/notes/{note_id}"), Some(&veli), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        common::items(&send(&app, "GET", "/notes", Some(&veli), None).await.body).len(),
        0
    );

    // Other user cannot update or delete it either.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/notes/{note_id}"),
            Some(&veli),
            Some(json!({"title":"x"}))
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/notes/{note_id}"),
            Some(&veli),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );

    // Delete, then it's gone.
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/notes/{note_id}"),
            Some(&ali),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(&app, "GET", &format!("/notes/{note_id}"), Some(&ali), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

/// A demotion to `parent` must not confiscate the account's own notes. It used
/// to: every note route required `student`, so after the demotion the owner
/// could no longer list, read or **delete** their notes — and since nothing
/// else in the crate reads a note and no route cascades one, the rows and their
/// on-disk blobs were unreachable and undeletable by everybody, forever. The
/// role bar is gone; the owner scoping that was always underneath it is now the
/// whole authorization, so a `parent` reaches their own notes and nobody
/// else's.
#[tokio::test]
async fn a_demotion_to_parent_strands_no_note_and_no_blob() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;

    // A student's note with a file on it.
    let res = send(
        &app,
        "POST",
        "/notes",
        Some(&ali),
        Some(json!({ "title": "kept", "content": "mine" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let note_id = id_of(&res.body);
    let up = common::upload_file(
        &app,
        &ali,
        &note_id,
        "plan.pdf",
        "application/pdf",
        b"bytes",
    )
    .await;
    assert_eq!(up.status, StatusCode::CREATED);
    let file_id = id_of(&up.body);
    let blob = common::blob_dir().join(&file_id);
    assert!(blob.exists(), "the upload must have written a blob");

    // Demote through the real route, not a seeded role.
    let res = send(
        &app,
        "PATCH",
        &format!("/users/{ali_id}/role"),
        Some(&admin),
        Some(json!({ "role": "parent" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["role"], "parent");

    // Every own-scoped read still answers the owner.
    let res = send(&app, "GET", "/notes", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(common::items(&res.body).len(), 1);
    assert_eq!(
        send(&app, "GET", &format!("/notes/{note_id}"), Some(&ali), None)
            .await
            .body["title"],
        "kept"
    );
    let res = send(
        &app,
        "PATCH",
        &format!("/notes/{note_id}"),
        Some(&ali),
        Some(json!({ "content": "still mine" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["content"], "still mine");

    // Files too: list, download the bytes, and attach another one.
    let res = send(
        &app,
        "GET",
        &format!("/notes/{note_id}/files"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::items(&res.body).len(), 1);
    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &format!("/notes/{note_id}/files/{file_id}"),
        Some(&ali),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"bytes");
    let extra =
        common::upload_file(&app, &ali, &note_id, "more.pdf", "application/pdf", b"x").await;
    assert_eq!(extra.status, StatusCode::CREATED);

    // A different parent reaches none of it — the owner check is all that
    // stands there now, so prove every route holds. `404`, the crate's answer
    // for someone else's row, never `403`.
    let veli = login_as(&app, &db, "veli", "parent").await;
    for (method, uri) in [
        ("GET", format!("/notes/{note_id}")),
        ("PATCH", format!("/notes/{note_id}")),
        ("GET", format!("/notes/{note_id}/files")),
        ("GET", format!("/notes/{note_id}/files/{file_id}")),
        ("DELETE", format!("/notes/{note_id}/files/{file_id}")),
        ("DELETE", format!("/notes/{note_id}")),
    ] {
        let body = (method == "PATCH").then(|| json!({ "title": "stolen" }));
        assert_eq!(
            send(&app, method, &uri, Some(&veli), body).await.status,
            StatusCode::NOT_FOUND,
            "{method} {uri} must not reach another user's note"
        );
    }
    assert_eq!(
        common::upload_file(&app, &veli, &note_id, "x.pdf", "application/pdf", b"x")
            .await
            .status,
        StatusCode::NOT_FOUND,
        "a stranger must not attach a file to someone else's note"
    );
    assert!(common::items(&send(&app, "GET", "/notes", Some(&veli), None).await.body).is_empty());
    // A parent's own notes work end to end, which is what makes the demoted
    // owner's access ownership and not leftover privilege.
    assert_eq!(
        send(
            &app,
            "POST",
            "/notes",
            Some(&veli),
            Some(json!({ "title": "veli's" }))
        )
        .await
        .status,
        StatusCode::CREATED
    );

    // And the demoted owner can still clear the whole thing out: one file by
    // hand, the rest through the note's cascade — blobs included.
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/notes/{note_id}/files/{file_id}"),
            Some(&ali),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert!(!blob.exists(), "deleting a file must unlink its blob");
    let extra_blob = common::blob_dir().join(id_of(&extra.body));
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/notes/{note_id}"),
            Some(&ali),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert!(
        !extra_blob.exists(),
        "the note delete must take its remaining blobs with it"
    );
    assert!(common::items(&send(&app, "GET", "/notes", Some(&ali), None).await.body).is_empty());
}

#[tokio::test]
async fn note_content_defaults_to_empty() {
    let app = mem_app().await;
    let ali = login(&app, "ali").await;
    let res = send(
        &app,
        "POST",
        "/notes",
        Some(&ali),
        Some(json!({ "title": "solo" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["content"], "");
}

/// Regression: two partial PATCHes touching *different* fields must both
/// survive. The handler used to fill the field a request omitted from the
/// snapshot it had read, so the writer that landed second reverted the other
/// one's field — a field-scoped `SET` does not help while the value bound to
/// it comes from a stale read. Raced repeatedly: a single round only loses the
/// update on the interleaving where both handlers read before either writes.
#[tokio::test]
async fn concurrent_partial_note_patches_keep_both_fields() {
    let app = mem_app().await;
    let ali = login(&app, "ali").await;

    for round in 0..20 {
        let note = create_note(&app, &ali, "old title").await;
        let uri = format!("/notes/{note}");
        let title_patch = json!({ "title": "new title" });
        let content_patch = json!({ "content": "new body" });
        let (a, b) = tokio::join!(
            send(&app, "PATCH", &uri, Some(&ali), Some(title_patch)),
            send(&app, "PATCH", &uri, Some(&ali), Some(content_patch)),
        );
        assert_eq!(a.status, StatusCode::OK, "round {round} title patch");
        assert_eq!(b.status, StatusCode::OK, "round {round} content patch");

        let after = send(&app, "GET", &uri, Some(&ali), None).await.body;
        assert_eq!(after["title"], "new title", "round {round}: title reverted");
        assert_eq!(
            after["content"], "new body",
            "round {round}: content reverted"
        );
    }
}

#[tokio::test]
async fn note_validation_and_missing_ids() {
    let app = mem_app().await;
    let ali = login(&app, "ali").await;

    // Blank title on create -> 400.
    assert_eq!(
        send(
            &app,
            "POST",
            "/notes",
            Some(&ali),
            Some(json!({ "title": "  " }))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );

    // Operations on a non-existent id -> 404.
    for method in ["GET", "PATCH", "DELETE"] {
        let body = (method == "PATCH").then(|| json!({ "title": "x" }));
        let res = send(&app, method, "/notes/does-not-exist", Some(&ali), body).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{method}");
    }

    // Blank title on update -> 400.
    let created = send(
        &app,
        "POST",
        "/notes",
        Some(&ali),
        Some(json!({ "title": "keep" })),
    )
    .await;
    let id = id_of(&created.body);
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/notes/{id}"),
            Some(&ali),
            Some(json!({ "title": "" }))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
}

// --- note files ------------------------------------------------------------

/// Create a note as `cookie`; returns its id.
async fn create_note(app: &axum::Router, cookie: &str, title: &str) -> String {
    let res = send(
        app,
        "POST",
        "/notes",
        Some(cookie),
        Some(json!({ "title": title })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "create note {title}");
    id_of(&res.body)
}

#[tokio::test]
async fn note_files_upload_download_delete_roundtrip() {
    let app = mem_app().await;
    let ali = login(&app, "ali").await;
    let note = create_note(&app, &ali, "with file").await;

    let bytes = b"%PDF-1.4 fake but binary \x00\x01\x02".to_vec();
    let up = common::upload_file(&app, &ali, &note, "plan.pdf", "application/pdf", &bytes).await;
    assert_eq!(up.status, StatusCode::CREATED, "{}", up.body);
    assert_eq!(up.body["name"], "plan.pdf");
    assert_eq!(up.body["content_type"], "application/pdf");
    assert_eq!(up.body["size"], bytes.len() as i64);
    let file_id = id_of(&up.body);

    // Listed under the note (paged envelope).
    let list = send(
        &app,
        "GET",
        &format!("/notes/{note}/files"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(list.status, StatusCode::OK);
    assert_eq!(common::total(&list.body), 1);
    assert_eq!(common::items(&list.body)[0]["name"], "plan.pdf");

    // Download returns the exact bytes with the declared type and filename.
    let (status, headers, body) = common::send_raw(
        &app,
        "GET",
        &format!("/notes/{note}/files/{file_id}"),
        Some(&ali),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, bytes);
    assert_eq!(headers["content-type"], "application/pdf");
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename=\"plan.pdf\"; filename*=UTF-8''plan.pdf"
    );

    // Delete; the file is gone from the list, download 404s, blob removed.
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/notes/{note}/files/{file_id}"),
            Some(&ali),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    let list = send(
        &app,
        "GET",
        &format!("/notes/{note}/files"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::total(&list.body), 0);
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/notes/{note}/files/{file_id}"),
            Some(&ali),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert!(
        !common::blob_dir().join(&file_id).exists(),
        "blob must be unlinked with its row"
    );
}

#[tokio::test]
async fn note_files_are_scoped_to_the_notes_owner() {
    let app = mem_app().await;
    let ali = login(&app, "ali").await;
    let veli = login(&app, "veli").await;
    let note = create_note(&app, &ali, "mine").await;
    let up = common::upload_file(&app, &ali, &note, "a.txt", "text/plain", b"hi").await;
    assert_eq!(up.status, StatusCode::CREATED);
    let file_id = id_of(&up.body);

    // Another user can't upload to, list, download from, or delete on the note.
    let foreign = common::upload_file(&app, &veli, &note, "b.txt", "text/plain", b"x").await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND);
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/notes/{note}/files"),
            Some(&veli),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    for method in ["GET", "DELETE"] {
        assert_eq!(
            send(
                &app,
                method,
                &format!("/notes/{note}/files/{file_id}"),
                Some(&veli),
                None
            )
            .await
            .status,
            StatusCode::NOT_FOUND,
            "{method}"
        );
    }

    // A file id under someone else's note id doesn't resolve either.
    let veli_note = create_note(&app, &veli, "veli's").await;
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/notes/{veli_note}/files/{file_id}"),
            Some(&veli),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn note_file_size_follows_the_school_limit() {
    let (app, db) = app_and_db().await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let ali = login(&app, "ali").await;
    let note = create_note(&app, &ali, "sized").await;

    // Lower the school cap to 1 KiB.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&boss),
        Some(json!({ "max_file_bytes": 1024 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["max_file_bytes"], 1024);

    // Over the cap dies with 413; at the cap passes.
    let big = common::upload_file(&app, &ali, &note, "big.bin", "", &vec![7u8; 1025]).await;
    assert_eq!(big.status, StatusCode::PAYLOAD_TOO_LARGE, "{}", big.body);
    let fits = common::upload_file(&app, &ali, &note, "fits.bin", "", &vec![7u8; 1024]).await;
    assert_eq!(fits.status, StatusCode::CREATED, "{}", fits.body);
    // A blank part content type falls back to octet-stream.
    assert_eq!(fits.body["content_type"], "application/octet-stream");

    // Raising the cap unlocks bigger files at once.
    assert_eq!(
        send(
            &app,
            "PATCH",
            "/settings",
            Some(&boss),
            Some(json!({ "max_file_bytes": 10_000 })),
        )
        .await
        .status,
        StatusCode::OK
    );
    let now_fits = common::upload_file(&app, &ali, &note, "big.bin", "", &vec![7u8; 1025]).await;
    assert_eq!(now_fits.status, StatusCode::CREATED);
}

#[tokio::test]
async fn settings_bound_the_file_limit() {
    let (app, db) = app_and_db().await;
    let boss = login_as(&app, &db, "boss", "manager").await;

    // The default is visible to any authenticated user.
    let res = send(&app, "GET", "/settings", Some(&boss), None).await;
    assert_eq!(res.body["max_file_bytes"], 5 * 1024 * 1024);

    // Outside the hard bounds -> 400 (and the stored value is untouched).
    for bad in [0, 1023, 25 * 1024 * 1024 + 1] {
        let res = send(
            &app,
            "PATCH",
            "/settings",
            Some(&boss),
            Some(json!({ "max_file_bytes": bad })),
        )
        .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "cap {bad}");
    }
    let res = send(&app, "GET", "/settings", Some(&boss), None).await;
    assert_eq!(res.body["max_file_bytes"], 5 * 1024 * 1024);

    // Editing another field leaves the cap alone.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&boss),
        Some(json!({ "max_file_bytes": 2048 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&boss),
        Some(json!({ "grade_bands": [{ "min": 0, "label": "F" }] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["max_file_bytes"], 2048);
}

#[tokio::test]
async fn note_file_count_is_capped() {
    let app = mem_app().await;
    let ali = login(&app, "ali").await;
    let note = create_note(&app, &ali, "full").await;

    for i in 0..10 {
        let up = common::upload_file(&app, &ali, &note, &format!("f{i}.txt"), "", b"x").await;
        assert_eq!(up.status, StatusCode::CREATED, "file {i}");
    }
    let over = common::upload_file(&app, &ali, &note, "f10.txt", "", b"x").await;
    assert_eq!(over.status, StatusCode::CONFLICT);

    // Deleting one frees a slot.
    let list = send(
        &app,
        "GET",
        &format!("/notes/{note}/files"),
        Some(&ali),
        None,
    )
    .await;
    let victim = id_of(&common::items(&list.body)[0]);
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/notes/{note}/files/{victim}"),
            Some(&ali),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    let retry = common::upload_file(&app, &ali, &note, "f10.txt", "", b"x").await;
    assert_eq!(retry.status, StatusCode::CREATED);
}

/// Regression: the 10-file cap was a bare count-then-write — concurrent
/// uploads all read the same pre-count and pushed a note past the cap (the
/// same write-skew as the event-capacity over-admit; SurrealDB transactions
/// don't conflict-check a cross-record count against an insert). The insert
/// now recounts under a lock, so the outcome is race-order independent:
/// exactly enough uploads win to land on the cap, the rest get the same 409
/// as a sequential over-fill.
#[tokio::test]
async fn concurrent_uploads_never_exceed_the_file_cap() {
    let app = mem_app().await;
    let ali = login(&app, "ali").await;
    let note = create_note(&app, &ali, "contested").await;

    for i in 0..8 {
        let up = common::upload_file(&app, &ali, &note, &format!("f{i}.txt"), "", b"x").await;
        assert_eq!(up.status, StatusCode::CREATED, "seed file {i}");
    }

    // Four racers for the two remaining slots.
    let results = tokio::join!(
        common::upload_file(&app, &ali, &note, "r0.txt", "", b"x"),
        common::upload_file(&app, &ali, &note, "r1.txt", "", b"x"),
        common::upload_file(&app, &ali, &note, "r2.txt", "", b"x"),
        common::upload_file(&app, &ali, &note, "r3.txt", "", b"x"),
    );
    let statuses = [
        results.0.status,
        results.1.status,
        results.2.status,
        results.3.status,
    ];
    for status in statuses {
        assert!(
            status == StatusCode::CREATED || status == StatusCode::CONFLICT,
            "a lost cap race must be a 409, got {status}"
        );
    }

    let list = send(
        &app,
        "GET",
        &format!("/notes/{note}/files"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(
        common::total(&list.body),
        10,
        "the cap must hold exactly under concurrency, statuses={statuses:?}"
    );
}

/// The crate-wide `422` is a *behaviour*, not just 74 utoipa annotations.
///
/// Every JSON-bodied operation declares `422` with no `ErrorResponse`, and the
/// only thing making that true is axum's default [`JsonRejection`]: a custom
/// mapping added anywhere (an `AppError` `From`, a `WithRejection` wrapper, a
/// hand-rolled `Json`) would silently turn the whole surface into `400`, or into
/// a `422` carrying an `ErrorResponse` the annotations say it does not, and the
/// spec-drift test in `spec_bounds.rs` — which only reads the document — would
/// still pass.
///
/// Four extractor shapes stand in for all 74 sites, because the rejection is a
/// property of the *extractor*, never of the route:
///   1. plain `Json<T>`, unauthenticated — the bare shape most sites are;
///   2. `Json<T>` behind a role gate (`RequireTeacher`), where the gate runs
///      first: proves the gate passing still leaves the body rejection in
///      charge (a `403`/`400` here would mean the order had changed);
///   3. `deny_unknown_fields` — the only DTO flavour that can reject a body
///      that is otherwise perfectly typed;
///   4. `Multipart`, which has a different rejection entirely and must answer
///      `400`, never `422` — that is why those routes declare no `422`.
/// Both `422` triggers are covered (a type-mismatched field and a missing
/// required one), as is the `400` line either side of them: unparseable JSON
/// and a well-typed value that a domain rule refuses.
#[tokio::test]
async fn malformed_json_bodies_answer_422_across_every_extractor_shape() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;
    let ogretmen = login_as(&app, &db, "ogretmen", "teacher").await;
    let json = Some("application/json");

    // 422: the body reached serde and did not fit the target type.
    for (uri, cookie, body, shape) in [
        (
            "/auth/register",
            None,
            r#"{"school": "demo", "username": 5, "password": "secret1"}"#,
            "plain Json, type mismatch",
        ),
        (
            "/auth/register",
            None,
            r#"{"school": "demo", "username": "veli"}"#,
            "plain Json, missing required field",
        ),
        (
            "/courses",
            Some(ogretmen.as_str()),
            r#"{"title": 42}"#,
            "role-gated Json, type mismatch",
        ),
        (
            "/boards",
            Some(ali.as_str()),
            r#"{"title": "Geometri", "participant_ids": []}"#,
            "deny_unknown_fields, unknown key",
        ),
    ] {
        let (status, headers, bytes) =
            common::send_raw(&app, "POST", uri, cookie, json, body.as_bytes().to_vec()).await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{shape} ({uri}) must be 422"
        );
        // The declared shape is "422, no `ErrorResponse`": axum answers a
        // plain-text explanation, *not* the `{"error": ...}` body every other
        // failure carries. A custom mapping that started emitting one would
        // make the annotations lie, so pin the negative rather than the text.
        assert_eq!(
            headers.get("content-type").and_then(|v| v.to_str().ok()),
            Some("text/plain; charset=utf-8"),
            "{shape} ({uri}) must not answer an ErrorResponse body"
        );
        assert!(
            serde_json::from_slice::<serde_json::Value>(&bytes).is_err(),
            "{shape} ({uri}) body must not be JSON, got {}",
            String::from_utf8_lossy(&bytes)
        );
    }

    // 400, both flavours: never a 422. Syntax that serde cannot even parse, and
    // a value that parsed fine but a domain rule refuses.
    for (body, why) in [
        ("not json at all", "unparseable JSON"),
        (
            r#"{"school": "demo", "username": "a", "password": "secret1"}"#,
            "well-typed value refused by a domain rule",
        ),
    ] {
        let (status, _, _) = common::send_raw(
            &app,
            "POST",
            "/auth/register",
            None,
            json,
            body.as_bytes().to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{why} must be 400");
    }

    // The multipart shape: a different rejection, and it must stay 400 — those
    // routes deliberately declare no 422 at all.
    let note = create_note(&app, &ali, "attachments").await;
    let (status, _, _) = common::send_raw(
        &app,
        "POST",
        &format!("/notes/{note}/files"),
        Some(&ali),
        Some("multipart/form-data; boundary=hezarfen-test-boundary"),
        b"not a multipart body".to_vec(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a multipart route must answer 400, never 422"
    );
}

#[tokio::test]
async fn note_file_upload_validation() {
    let app = mem_app().await;
    let ali = login(&app, "ali").await;
    let note = create_note(&app, &ali, "strict").await;

    // Empty file, filename with a path separator, no file field at all: 400.
    let empty = common::upload_file(&app, &ali, &note, "e.txt", "", b"").await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST, "{}", empty.body);
    let traversal = common::upload_file(&app, &ali, &note, "../../etc/passwd", "", b"data").await;
    assert_eq!(traversal.status, StatusCode::BAD_REQUEST);

    // A body whose only part isn't named "file" has no upload in it.
    let stray = common::multipart_file("y.txt", "", b"data");
    let stray = String::from_utf8(stray)
        .unwrap()
        .replace("name=\"file\"", "name=\"attachment\"");
    let (status, _, _) = common::send_raw(
        &app,
        "POST",
        &format!("/notes/{note}/files"),
        Some(&ali),
        Some("multipart/form-data; boundary=hezarfen-test-boundary"),
        stray.into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Non-ASCII filenames are fine — stored and encoded on download.
    let turkish =
        common::upload_file(&app, &ali, &note, "ödev 1.pdf", "application/pdf", b"pdf").await;
    assert_eq!(turkish.status, StatusCode::CREATED);
    let (status, headers, _) = common::send_raw(
        &app,
        "GET",
        &format!("/notes/{note}/files/{}", id_of(&turkish.body)),
        Some(&ali),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename=\"_dev 1.pdf\"; filename*=UTF-8''%C3%B6dev%201.pdf"
    );
}

#[tokio::test]
async fn deleting_a_note_removes_its_files() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;
    let note = create_note(&app, &ali, "doomed").await;
    let up = common::upload_file(&app, &ali, &note, "gone.txt", "", b"bye").await;
    assert_eq!(up.status, StatusCode::CREATED);
    let file_id = id_of(&up.body);

    assert_eq!(
        send(&app, "DELETE", &format!("/notes/{note}"), Some(&ali), None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );

    // Row and blob are both gone.
    let mut rows = db
        .query("SELECT * FROM note_file")
        .await
        .unwrap()
        .check()
        .unwrap();
    let left: Vec<serde_json::Value> = rows.take(0).unwrap();
    assert!(left.is_empty(), "note_file rows must cascade: {left:?}");
    assert!(
        !common::blob_dir().join(&file_id).exists(),
        "blob must be unlinked when its note dies"
    );
}

// --- events + attendance -------------------------------------------------

#[tokio::test]
async fn events_are_shared_but_creator_guarded() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login_as(&app, &db, "veli", "teacher").await;

    let ev = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "standup", "description": "daily" })),
    )
    .await;
    assert_eq!(ev.status, StatusCode::CREATED);
    let event_id = id_of(&ev.body);

    // Any authenticated user can read the event and its roster.
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/events/{event_id}"),
            Some(&veli),
            None
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        common::items(&send(&app, "GET", "/events", Some(&veli), None).await.body).len(),
        1
    );

    // Non-creator cannot edit or delete.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/events/{event_id}"),
            Some(&veli),
            Some(json!({"title":"hijack"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/events/{event_id}"),
            Some(&veli),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // Creator can edit (partial).
    let res = send(
        &app,
        "PATCH",
        &format!("/events/{event_id}"),
        Some(&ali),
        Some(json!({ "description": "moved" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["description"], "moved");
    assert_eq!(res.body["title"], "standup");

    // Blank title on update -> 400. Missing event -> 404.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/events/{event_id}"),
            Some(&ali),
            Some(json!({"title":" "}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(&app, "GET", "/events/nope", Some(&ali), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn attendance_marking_and_upsert() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let veli_id = id_of(&send(&app, "GET", "/auth/me", Some(&veli), None).await.body);

    let event_id = id_of(
        &send(
            &app,
            "POST",
            "/events",
            Some(&ali),
            Some(json!({ "title": "standup" })),
        )
        .await
        .body,
    );

    // Default target is the caller.
    let res = send(
        &app,
        "POST",
        &format!("/events/{event_id}/attendance"),
        Some(&ali),
        Some(json!({ "status": "present" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["status"], "present");

    // Mark veli, then re-mark: upsert keeps a single row (same id).
    let first = send(
        &app,
        "POST",
        &format!("/events/{event_id}/attendance"),
        Some(&ali),
        Some(json!({ "status": "late", "user_id": veli_id })),
    )
    .await;
    let second = send(
        &app,
        "POST",
        &format!("/events/{event_id}/attendance"),
        Some(&ali),
        Some(json!({ "status": "absent", "user_id": veli_id })),
    )
    .await;
    assert_eq!(
        id_of(&first.body),
        id_of(&second.body),
        "upsert same record"
    );
    assert_eq!(second.body["status"], "absent");

    // Two rows total.
    assert_eq!(
        common::items(
            &send(
                &app,
                "GET",
                &format!("/events/{event_id}/attendance"),
                Some(&ali),
                None
            )
            .await
            .body
        )
        .len(),
        2
    );

    // Remove veli -> one row; removing again -> 404.
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/events/{event_id}/attendance/{veli_id}"),
            Some(&ali),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        common::items(
            &send(
                &app,
                "GET",
                &format!("/events/{event_id}/attendance"),
                Some(&ali),
                None
            )
            .await
            .body
        )
        .len(),
        1
    );
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/events/{event_id}/attendance/{veli_id}"),
            Some(&ali),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn attendance_input_validation() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let event_id = id_of(
        &send(
            &app,
            "POST",
            "/events",
            Some(&ali),
            Some(json!({ "title": "e" })),
        )
        .await
        .body,
    );

    // Invalid status -> 400.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/events/{event_id}/attendance"),
            Some(&ali),
            Some(json!({"status":"maybe"}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );

    // Unknown target user -> 400.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/events/{event_id}/attendance"),
            Some(&ali),
            Some(json!({"status":"present","user_id":"ghost"}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );

    // Attendance on a missing event -> 404.
    assert_eq!(
        send(
            &app,
            "POST",
            "/events/nope/attendance",
            Some(&ali),
            Some(json!({"status":"present"}))
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn deleting_event_cascades_attendance() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let event_id = id_of(
        &send(
            &app,
            "POST",
            "/events",
            Some(&ali),
            Some(json!({ "title": "e" })),
        )
        .await
        .body,
    );
    send(
        &app,
        "POST",
        &format!("/events/{event_id}/attendance"),
        Some(&ali),
        Some(json!({"status":"present"})),
    )
    .await;

    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/events/{event_id}"),
            Some(&ali),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    // Event gone — and its roster endpoint with it (404, not an empty list).
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/events/{event_id}"),
            Some(&ali),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/events/{event_id}/attendance"),
            Some(&ali),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn event_timestamps_echo_and_validate() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let now = Timestamp::now().as_millis();
    let starts = now + 3_600_000;
    let ends = now + 7_200_000;

    // Create with unix-millis times.
    let res = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "mtg", "starts_at": starts, "ends_at": ends })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["starts_at"], starts);
    assert_eq!(res.body["ends_at"], ends);
    let event_id = id_of(&res.body);

    // Omitted times serialize as null.
    let untimed = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "notime" })),
    )
    .await;
    assert!(untimed.body["starts_at"].is_null());
    assert!(untimed.body["ends_at"].is_null());

    // ends_at before starts_at -> 400.
    let bad = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "bad", "starts_at": now + 5_000_000, "ends_at": now + 1_000_000 })),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);

    // Past times -> 400, whichever end carries them.
    for body in [
        json!({ "title": "old", "starts_at": now - 3_600_000 }),
        json!({ "title": "old", "ends_at": now - 3_600_000 }),
    ] {
        let res = send(&app, "POST", "/events", Some(&ali), Some(body)).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    }

    // Updating only ends_at keeps starts_at.
    let updated = send(
        &app,
        "PATCH",
        &format!("/events/{event_id}"),
        Some(&ali),
        Some(json!({ "ends_at": now + 10_800_000 })),
    )
    .await;
    assert_eq!(updated.status, StatusCode::OK);
    assert_eq!(updated.body["starts_at"], starts);
    assert_eq!(updated.body["ends_at"], now + 10_800_000);

    // A PATCH may not move a time into the past either — whichever end.
    for body in [
        json!({ "starts_at": now - 3_600_000 }),
        json!({ "ends_at": now - 3_600_000 }),
    ] {
        let res = send(
            &app,
            "PATCH",
            &format!("/events/{event_id}"),
            Some(&ali),
            Some(body),
        )
        .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    }
}

#[tokio::test]
async fn event_audience_defaults_validates_and_echoes() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;

    // Omitted audience = school-wide, and it echoes on every read.
    let ev = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "assembly" })),
    )
    .await;
    assert_eq!(ev.status, StatusCode::CREATED);
    assert_eq!(ev.body["audience"]["kind"], "school");

    // A role audience round-trips…
    let ev = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "orientation", "audience": { "kind": "role", "role": "student" } })),
    )
    .await;
    assert_eq!(ev.status, StatusCode::CREATED);
    assert_eq!(ev.body["audience"]["role"], "student");
    let event_id = id_of(&ev.body);

    // …a PATCH without `audience` keeps it, and a sent one replaces it.
    let res = send(
        &app,
        "PATCH",
        &format!("/events/{event_id}"),
        Some(&ali),
        Some(json!({ "title": "orientation day" })),
    )
    .await;
    assert_eq!(res.body["audience"]["role"], "student");
    let res = send(
        &app,
        "PATCH",
        &format!("/events/{event_id}"),
        Some(&ali),
        Some(json!({ "audience": { "kind": "registration", "capacity": 5 } })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["audience"]["capacity"], 5);

    // An omitted capacity echoes as null: an uncapped signup list.
    let res = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "trip", "audience": { "kind": "registration" } })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["audience"]["kind"], "registration");
    assert!(res.body["audience"]["capacity"].is_null());

    // Invalid audiences are rejected: unknown role, ghost course, and
    // non-positive capacities.
    for audience in [
        json!({ "kind": "role", "role": "wizard" }),
        json!({ "kind": "course", "course": "nope" }),
        json!({ "kind": "registration", "capacity": 0 }),
        json!({ "kind": "registration", "capacity": -3 }),
    ] {
        let res = send(
            &app,
            "POST",
            "/events",
            Some(&ali),
            Some(json!({ "title": "bad", "audience": audience })),
        )
        .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{audience}");
    }
    // An unknown kind never deserializes — including the retired hand-picked
    // `users` kind, gone in the registration clean break.
    for audience in [
        json!({ "kind": "galaxy" }),
        json!({ "kind": "users", "users": ["ghost"] }),
    ] {
        let res = send(
            &app,
            "POST",
            "/events",
            Some(&ali),
            Some(json!({ "title": "bad", "audience": audience })),
        )
        .await;
        assert!(res.status.is_client_error(), "{audience}: {}", res.status);
    }
}

#[tokio::test]
async fn event_audience_gates_marking() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await; // student
    let mina = login(&app, "mina").await; // student
    let veli_id = me_id(&app, &veli).await;
    let mina_id = me_id(&app, &mina).await;
    let ali_id = me_id(&app, &ali).await;

    let create = |audience: serde_json::Value| {
        let app = &app;
        let ali = &ali;
        async move {
            let res = send(
                app,
                "POST",
                "/events",
                Some(ali),
                Some(json!({ "title": "ev", "audience": audience })),
            )
            .await;
            assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
            id_of(&res.body)
        }
    };
    let mark = |event: String, user: String| {
        let app = &app;
        let ali = &ali;
        async move {
            send(
                app,
                "POST",
                &format!("/events/{event}/attendance"),
                Some(ali),
                Some(json!({ "status": "present", "user_id": user })),
            )
            .await
            .status
        }
    };

    // Role audience: students are markable, the (teacher) marker is not —
    // including implicitly, via the marker-defaults-to-caller path.
    let ev = create(json!({ "kind": "role", "role": "student" })).await;
    assert_eq!(mark(ev.clone(), veli_id.clone()).await, StatusCode::OK);
    assert_eq!(
        mark(ev.clone(), ali_id.clone()).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/events/{ev}/attendance"),
            Some(&ali),
            Some(json!({ "status": "present" })),
        )
        .await
        .status,
        StatusCode::BAD_REQUEST,
        "caller-defaulted target must still clear the audience gate"
    );

    // Course audience: enrollment decides, live.
    let course = create_course(&app, &ali, "algebra").await;
    enroll(&app, &ali, &course, &veli_id).await;
    let ev = create(json!({ "kind": "course", "course": course })).await;
    assert_eq!(mark(ev.clone(), veli_id.clone()).await, StatusCode::OK);
    assert_eq!(
        mark(ev.clone(), mina_id.clone()).await,
        StatusCode::BAD_REQUEST
    );

    // Registration audience: the signup list is the roster.
    let ev = create(json!({ "kind": "registration" })).await;
    let res = send(
        &app,
        "POST",
        &format!("/events/{ev}/register"),
        Some(&ali),
        Some(json!({ "user_id": mina_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(mark(ev.clone(), mina_id.clone()).await, StatusCode::OK);
    assert_eq!(
        mark(ev.clone(), veli_id.clone()).await,
        StatusCode::BAD_REQUEST
    );

    // School audience: everyone is expected — the teacher may mark themselves.
    let ev = create(json!({ "kind": "school" })).await;
    assert_eq!(mark(ev.clone(), ali_id.clone()).await, StatusCode::OK);
    assert_eq!(mark(ev, veli_id.clone()).await, StatusCode::OK);
}

#[tokio::test]
async fn event_roster_joins_expected_with_marks() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let s1 = login(&app, "selin").await;
    let s2 = login(&app, "zeynep").await;
    let s1_id = me_id(&app, &s1).await;
    let course = create_course(&app, &ali, "algebra").await;
    enroll(&app, &ali, &course, &s1_id).await;
    enroll(&app, &ali, &course, &me_id(&app, &s2).await).await;

    let ev = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "field trip",
                     "audience": { "kind": "course", "course": course } })),
    )
    .await;
    let event_id = id_of(&ev.body);
    let roster_uri = format!("/events/{event_id}/roster");

    // Before any marks: every expected attendee, all unmarked.
    let res = send(&app, "GET", &roster_uri, Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(common::total(&res.body), 2);
    assert!(
        common::items(&res.body)
            .iter()
            .all(|entry| entry["status"].is_null() && entry["marked_by"].is_null())
    );

    // Mark one: the roster shows who came and who's still missing.
    send(
        &app,
        "POST",
        &format!("/events/{event_id}/attendance"),
        Some(&ali),
        Some(json!({ "status": "present", "user_id": s1_id })),
    )
    .await;
    let res = send(&app, "GET", &roster_uri, Some(&ali), None).await;
    let items = common::items(&res.body);
    assert_eq!(items.len(), 2);
    let marked: Vec<_> = items
        .iter()
        .filter(|entry| entry["status"] == "present")
        .collect();
    assert_eq!(marked.len(), 1);
    assert_eq!(marked[0]["user"]["id"], s1_id);
    assert_eq!(marked[0]["marked_by"]["username"], "ali");
    assert_eq!(
        items
            .iter()
            .filter(|entry| entry["status"].is_null())
            .count(),
        1
    );

    // The roster pages like any list.
    let res = send(
        &app,
        "GET",
        &format!("{roster_uri}?limit=1&offset=1"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::items(&res.body).len(), 1);
    assert_eq!(common::total(&res.body), 2);

    // Dropping someone from the audience (unenroll) drops them from the
    // roster report — their recorded mark stays in the attendance listing.
    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{course}/enrollments/{s1_id}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(&app, "GET", &roster_uri, Some(&ali), None).await;
    assert_eq!(common::total(&res.body), 1);
    assert!(common::items(&res.body)[0]["status"].is_null());
    let res = send(
        &app,
        "GET",
        &format!("/events/{event_id}/attendance"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 1, "stale mark stays recorded");

    // A missing event has no roster — 404, not an empty page.
    assert_eq!(
        send(&app, "GET", "/events/nope/roster", Some(&ali), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

// --- exams + results -----------------------------------------------------

#[tokio::test]
async fn exams_are_course_scoped_and_course_guarded() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login_as(&app, &db, "veli", "teacher").await;

    let course_id = create_course(&app, &ali, "algebra").await;
    let ex = send(
        &app,
        "POST",
        &format!("/courses/{course_id}/exams"),
        Some(&ali),
        Some(json!({ "title": "ch3", "description": "algebra", "kind": "quiz" })),
    )
    .await;
    assert_eq!(ex.status, StatusCode::CREATED);
    assert_eq!(ex.body["kind"], "quiz");
    assert_eq!(ex.body["course"], course_id);
    // Weight lives on the kind (settings), not the exam.
    assert_eq!(ex.body.get("weight"), None);
    let exam_id = id_of(&ex.body);

    // A teacher outside the course sees none of it: not the exam, not the
    // course's exam list, and an empty catalog.
    assert_eq!(
        send(&app, "GET", &format!("/exams/{exam_id}"), Some(&veli), None)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        common::items(&send(&app, "GET", "/exams", Some(&veli), None).await.body).len(),
        0
    );
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/courses/{course_id}/exams"),
            Some(&veli),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // The course creator reads all three views.
    assert_eq!(
        send(&app, "GET", &format!("/exams/{exam_id}"), Some(&ali), None)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        common::items(&send(&app, "GET", "/exams", Some(&ali), None).await.body).len(),
        1
    );
    assert_eq!(
        common::items(
            &send(
                &app,
                "GET",
                &format!("/courses/{course_id}/exams"),
                Some(&ali),
                None
            )
            .await
            .body
        )
        .len(),
        1
    );

    // A teacher who doesn't manage the course cannot edit or delete its exams.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/exams/{exam_id}"),
            Some(&veli),
            Some(json!({"title":"hijack"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/exams/{exam_id}"),
            Some(&veli),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // Course creator edits (partial) — kind flips homework, title kept.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam_id}"),
        Some(&ali),
        Some(json!({ "kind": "homework" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["kind"], "homework");
    assert_eq!(res.body["title"], "ch3");
}

#[tokio::test]
async fn exam_input_validation() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let course_id = create_course(&app, &ali, "algebra").await;
    let exams_uri = format!("/courses/{course_id}/exams");

    // Missing/blank title -> 400.
    assert_eq!(
        send(
            &app,
            "POST",
            &exams_uri,
            Some(&ali),
            Some(json!({"title":"  ","kind":"quiz"}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    // Unknown kind -> 400 ("final" is valid now; "essay" is not).
    assert_eq!(
        send(
            &app,
            "POST",
            &exams_uri,
            Some(&ali),
            Some(json!({"title":"t","kind":"essay"}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(
            &app,
            "POST",
            &exams_uri,
            Some(&ali),
            Some(json!({"title":"t","kind":"final"}))
        )
        .await
        .status,
        StatusCode::CREATED
    );
    // Missing exam -> 404.
    assert_eq!(
        send(&app, "GET", "/exams/nope", Some(&ali), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn grading_upsert_and_own_result() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await; // student
    let bob = login(&app, "bob").await; // student
    let alice_id = me_id(&app, &alice).await;

    let course_id = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course_id, &alice_id).await;
    let exam_id = create_exam(&app, &teacher, &course_id, "mt", "quiz").await;

    // Before grading, the student's own result is 404.
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/exams/{exam_id}/result"),
            Some(&alice),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );

    // Teacher grades alice, then re-grades: upsert keeps one row (same id).
    let first = send(
        &app,
        "POST",
        &format!("/exams/{exam_id}/results"),
        Some(&teacher),
        Some(json!({ "mark": 70, "user_id": alice_id })),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK);
    let second = send(
        &app,
        "POST",
        &format!("/exams/{exam_id}/results"),
        Some(&teacher),
        Some(json!({ "mark": 88, "user_id": alice_id })),
    )
    .await;
    assert_eq!(
        id_of(&first.body),
        id_of(&second.body),
        "upsert same record"
    );
    assert_eq!(second.body["mark"], 88);

    // Alice reads her own result; bob (ungraded) still 404s and can't see alice's.
    let mine = send(
        &app,
        "GET",
        &format!("/exams/{exam_id}/result"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(mine.status, StatusCode::OK);
    assert_eq!(mine.body["mark"], 88);
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/exams/{exam_id}/result"),
            Some(&bob),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );

    // Teacher sees the full list (one row).
    assert_eq!(
        common::items(
            &send(
                &app,
                "GET",
                &format!("/exams/{exam_id}/results"),
                Some(&teacher),
                None
            )
            .await
            .body
        )
        .len(),
        1
    );

    // Remove alice's result -> her own read 404s again; removing again -> 404.
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/exams/{exam_id}/results/{alice_id}"),
            Some(&teacher),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/exams/{exam_id}/result"),
            Some(&alice),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/exams/{exam_id}/results/{alice_id}"),
            Some(&teacher),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn grading_rbac_and_validation() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await; // student
    let bob = login(&app, "bob").await; // student
    let alice_id = me_id(&app, &alice).await;
    let bob_id = me_id(&app, &bob).await;

    let course_id = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course_id, &alice_id).await;
    let exam_id = create_exam(&app, &teacher, &course_id, "e", "homework").await;

    // Students cannot create exams, grade anyone, list all results, or delete a result.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/courses/{course_id}/exams"),
            Some(&alice),
            Some(json!({"title":"x","kind":"quiz"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/exams/{exam_id}/results"),
            Some(&alice),
            Some(json!({"mark":100,"user_id":alice_id}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/exams/{exam_id}/results"),
            Some(&alice),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/exams/{exam_id}/results/{alice_id}"),
            Some(&bob),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // Mark out of range -> 400.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/exams/{exam_id}/results"),
            Some(&teacher),
            Some(json!({"mark":101,"user_id":alice_id}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    // Unknown target user -> 400.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/exams/{exam_id}/results"),
            Some(&teacher),
            Some(json!({"mark":50,"user_id":"ghost"}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    // Target exists but is not enrolled in the course -> 400.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/exams/{exam_id}/results"),
            Some(&teacher),
            Some(json!({"mark":50,"user_id":bob_id}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    // Grading on a missing exam -> 404.
    assert_eq!(
        send(
            &app,
            "POST",
            "/exams/nope/results",
            Some(&teacher),
            Some(json!({"mark":50,"user_id":alice_id}))
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn deleting_exam_cascades_results() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await;
    let alice_id = me_id(&app, &alice).await;

    let course_id = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course_id, &alice_id).await;
    let exam_id = create_exam(&app, &teacher, &course_id, "e", "quiz").await;
    send(
        &app,
        "POST",
        &format!("/exams/{exam_id}/results"),
        Some(&teacher),
        Some(json!({"mark":60,"user_id":alice_id})),
    )
    .await;

    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/exams/{exam_id}"),
            Some(&teacher),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    // Exam gone; the student's result is gone with it.
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/exams/{exam_id}"),
            Some(&teacher),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/exams/{exam_id}/result"),
            Some(&alice),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

// --- courses + enrollments + marks ----------------------------------------

#[tokio::test]
async fn courses_are_owner_scoped_and_creator_guarded() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login_as(&app, &db, "veli", "teacher").await;
    let boss = login_as(&app, &db, "boss", "manager").await;

    let res = send(
        &app,
        "POST",
        "/courses",
        Some(&ali),
        Some(json!({ "title": "algebra", "description": "numbers" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["title"], "algebra");
    // The creator comes back as an embedded person ref, not a bare id — the
    // catalog renders names without a second users request.
    assert_eq!(res.body["creator"]["username"], "ali");
    let course_id = id_of(&res.body);

    // Another teacher is not enrolled and doesn't manage it: no read, and the
    // catalog shows them nothing.
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/courses/{course_id}"),
            Some(&veli),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        common::items(&send(&app, "GET", "/courses", Some(&veli), None).await.body).len(),
        0
    );

    // The creator and a manager see it — in the catalog too.
    for caller in [&ali, &boss] {
        assert_eq!(
            send(
                &app,
                "GET",
                &format!("/courses/{course_id}"),
                Some(caller),
                None
            )
            .await
            .status,
            StatusCode::OK
        );
        let catalog = send(&app, "GET", "/courses", Some(caller), None).await.body;
        let rows = common::items(&catalog);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0]["creator"]["username"], "ali",
            "the catalog embeds the creator ref on every row"
        );
    }

    // Non-creator teacher cannot edit or delete.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/courses/{course_id}"),
            Some(&veli),
            Some(json!({"title":"hijack"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/courses/{course_id}"),
            Some(&veli),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // Creator edits (partial) — description flips, title kept.
    let res = send(
        &app,
        "PATCH",
        &format!("/courses/{course_id}"),
        Some(&ali),
        Some(json!({ "description": "letters" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["title"], "algebra");
    assert_eq!(res.body["description"], "letters");

    // A manager can edit anyone's course.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/courses/{course_id}"),
            Some(&boss),
            Some(json!({"title":"algebra II"}))
        )
        .await
        .status,
        StatusCode::OK
    );
}

/// A manager can put a second teacher in charge of someone else's course. The
/// assignee then manages everything inside it, but the course stays its
/// creator's to delete — and a demotion takes the assignment away.
#[tokio::test]
async fn assigned_teacher_manages_course_without_owning_it() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login_as(&app, &db, "veli", "teacher").await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let rektor = login_as(&app, &db, "rektor", "admin").await;
    let veli_id = id_of(&send(&app, "GET", "/auth/me", Some(&veli), None).await.body);

    let course_id = id_of(
        &send(
            &app,
            "POST",
            "/courses",
            Some(&ali),
            Some(json!({ "title": "algebra" })),
        )
        .await
        .body,
    );
    let teachers = format!("/courses/{course_id}/teachers");

    // Before assignment veli is just another teacher: no read, no write.
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/courses/{course_id}"),
            Some(&veli),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // Staffing is the office's call — the course's own creator cannot do it.
    assert_eq!(
        send(
            &app,
            "POST",
            &teachers,
            Some(&ali),
            Some(json!({ "user_id": veli_id }))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // Only teacher+ can be assigned.
    let student = login_as(&app, &db, "zeynep", "student").await;
    let student_id = id_of(
        &send(&app, "GET", "/auth/me", Some(&student), None)
            .await
            .body,
    );
    assert_eq!(
        send(
            &app,
            "POST",
            &teachers,
            Some(&boss),
            Some(json!({ "user_id": student_id }))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );

    // The manager assigns veli; the course echoes its staff back.
    let res = send(
        &app,
        "POST",
        &teachers,
        Some(&boss),
        Some(json!({ "user_id": veli_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["creator"]["username"], "ali");
    assert_eq!(res.body["teachers"][0]["username"], "veli");

    // Assigning twice is idempotent — no duplicate row in the list.
    let again = send(
        &app,
        "POST",
        &teachers,
        Some(&boss),
        Some(json!({ "user_id": veli_id })),
    )
    .await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.body["teachers"].as_array().unwrap().len(), 1);

    // Veli now runs the course: it shows in their catalog, and they can edit
    // it and enroll students.
    let catalog = send(&app, "GET", "/courses", Some(&veli), None).await.body;
    assert_eq!(common::items(&catalog).len(), 1);
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/courses/{course_id}"),
            Some(&veli),
            Some(json!({"description":"letters"}))
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/courses/{course_id}/enrollments"),
            Some(&veli),
            Some(json!({ "user_id": student_id }))
        )
        .await
        .status,
        StatusCode::OK
    );

    // But the course is not theirs to delete, nor to re-staff.
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/courses/{course_id}"),
            Some(&veli),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("{teachers}/{veli_id}"),
            Some(&veli),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // Demoting veli below teacher sweeps the assignment: the course drops off
    // their catalog and the rights go with it.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/users/{veli_id}/role"),
            Some(&rektor),
            Some(json!({"role":"student"}))
        )
        .await
        .status,
        StatusCode::OK
    );
    let after = send(
        &app,
        "GET",
        &format!("/courses/{course_id}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(
        after.body["teachers"].as_array().unwrap().len(),
        0,
        "a demoted teacher is swept off the courses they were assigned to"
    );

    // Re-assign, then unassign by hand: the second removal is a 404.
    send(
        &app,
        "PATCH",
        &format!("/users/{veli_id}/role"),
        Some(&rektor),
        Some(json!({"role":"teacher"})),
    )
    .await;
    assert_eq!(
        send(
            &app,
            "POST",
            &teachers,
            Some(&boss),
            Some(json!({ "user_id": veli_id }))
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("{teachers}/{veli_id}"),
            Some(&boss),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("{teachers}/{veli_id}"),
            Some(&boss),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/courses/{course_id}"),
            Some(&veli),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN,
        "unassigning takes the course back out of their reach"
    );
}

#[tokio::test]
async fn course_input_validation() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;

    // Missing/blank title -> 400.
    assert_eq!(
        send(
            &app,
            "POST",
            "/courses",
            Some(&ali),
            Some(json!({"title":"  "}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    // Missing course -> 404.
    assert_eq!(
        send(&app, "GET", "/courses/nope", Some(&ali), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

/// A course is `course` by default, may be born a `study` (etüt), the kind is
/// PATCH-editable, and anything else is a 400.
#[tokio::test]
async fn course_kind_defaults_validates_and_edits() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;

    let plain = send(
        &app,
        "POST",
        "/courses",
        Some(&ali),
        Some(json!({"title":"algebra"})),
    )
    .await;
    assert_eq!(plain.status, StatusCode::CREATED);
    assert_eq!(plain.body["kind"], "course");

    let etut = send(
        &app,
        "POST",
        "/courses",
        Some(&ali),
        Some(json!({"title":"evening etut", "kind":"study"})),
    )
    .await;
    assert_eq!(etut.status, StatusCode::CREATED);
    assert_eq!(etut.body["kind"], "study");

    assert_eq!(
        send(
            &app,
            "POST",
            "/courses",
            Some(&ali),
            Some(json!({"title":"nope", "kind":"etut"}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );

    // PATCH flips the kind; omitting it keeps the current one.
    let id = id_of(&plain.body);
    let flipped = send(
        &app,
        "PATCH",
        &format!("/courses/{id}"),
        Some(&ali),
        Some(json!({"kind":"study"})),
    )
    .await;
    assert_eq!(flipped.status, StatusCode::OK);
    assert_eq!(flipped.body["kind"], "study");
    let renamed = send(
        &app,
        "PATCH",
        &format!("/courses/{id}"),
        Some(&ali),
        Some(json!({"title":"geometry"})),
    )
    .await;
    assert_eq!(renamed.body["kind"], "study", "omitted kind is kept");
}

#[tokio::test]
async fn enrollment_upsert_roster_and_my_courses() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await; // student
    let bob = login(&app, "bob").await; // student
    let alice_id = me_id(&app, &alice).await;
    let course_id = create_course(&app, &teacher, "algebra").await;

    // Enrolling twice is an upsert: same row id both times.
    let first = send(
        &app,
        "POST",
        &format!("/courses/{course_id}/enrollments"),
        Some(&teacher),
        Some(json!({ "user_id": alice_id })),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK);
    let second = send(
        &app,
        "POST",
        &format!("/courses/{course_id}/enrollments"),
        Some(&teacher),
        Some(json!({ "user_id": alice_id })),
    )
    .await;
    assert_eq!(id_of(&first.body), id_of(&second.body), "upsert same row");

    // Roster lists alice once; students can't read the roster.
    let roster = send(
        &app,
        "GET",
        &format!("/courses/{course_id}/enrollments"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::items(&roster.body).len(), 1);
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/courses/{course_id}/enrollments"),
            Some(&alice),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // /courses/me shows the course for alice, stays empty for bob.
    assert_eq!(
        common::items(
            &send(&app, "GET", "/courses/me", Some(&alice), None)
                .await
                .body,
        )
        .len(),
        1
    );
    assert_eq!(
        common::items(
            &send(&app, "GET", "/courses/me", Some(&bob), None)
                .await
                .body,
        )
        .len(),
        0
    );

    // Unenroll -> 204; repeating -> 404.
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/courses/{course_id}/enrollments/{alice_id}"),
            Some(&teacher),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/courses/{course_id}/enrollments/{alice_id}"),
            Some(&teacher),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn capacity_caps_the_roster() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await; // student
    let bob = login(&app, "bob").await; // student
    let alice_id = me_id(&app, &alice).await;
    let bob_id = me_id(&app, &bob).await;

    // A zero-seat club is nonsense.
    assert_eq!(
        send(
            &app,
            "POST",
            "/courses",
            Some(&teacher),
            Some(json!({ "title": "chess", "kind": "club", "capacity": 0 })),
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );

    // A one-seat club.
    let created = send(
        &app,
        "POST",
        "/courses",
        Some(&teacher),
        Some(json!({ "title": "chess", "kind": "club", "capacity": 1 })),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(created.body["kind"], "club");
    assert_eq!(created.body["capacity"], 1);
    let course_id = id_of(&created.body);

    // The seat goes to alice; bob bounces off the full roster; re-enrolling
    // the member stays an idempotent OK even at the cap.
    let enroll = |cookie: String, user_id: String| {
        let app = app.clone();
        let path = format!("/courses/{course_id}/enrollments");
        async move {
            send(
                &app,
                "POST",
                &path,
                Some(&cookie),
                Some(json!({ "user_id": user_id })),
            )
            .await
            .status
        }
    };
    assert_eq!(
        enroll(teacher.clone(), alice_id.clone()).await,
        StatusCode::OK
    );
    assert_eq!(
        enroll(teacher.clone(), bob_id.clone()).await,
        StatusCode::CONFLICT
    );
    assert_eq!(
        enroll(teacher.clone(), alice_id.clone()).await,
        StatusCode::OK
    );

    // `null` lifts the cap and the door reopens.
    let lifted = send(
        &app,
        "PATCH",
        &format!("/courses/{course_id}"),
        Some(&teacher),
        Some(json!({ "capacity": null })),
    )
    .await;
    assert_eq!(lifted.status, StatusCode::OK);
    assert!(lifted.body["capacity"].is_null());
    assert_eq!(enroll(teacher.clone(), bob_id).await, StatusCode::OK);
}

#[tokio::test]
async fn enrollment_requires_course_management() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let veli = login_as(&app, &db, "veli", "teacher").await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let alice = login(&app, "alice").await; // student
    let alice_id = me_id(&app, &alice).await;
    let course_id = create_course(&app, &teacher, "algebra").await;

    // A teacher who doesn't manage the course cannot enroll anyone.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/courses/{course_id}/enrollments"),
            Some(&veli),
            Some(json!({ "user_id": alice_id }))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    // A manager can.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/courses/{course_id}/enrollments"),
            Some(&boss),
            Some(json!({ "user_id": alice_id }))
        )
        .await
        .status,
        StatusCode::OK
    );
    // Unknown target -> 400; missing course -> 404.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/courses/{course_id}/enrollments"),
            Some(&teacher),
            Some(json!({ "user_id": "ghost" }))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(
            &app,
            "POST",
            "/courses/nope/enrollments",
            Some(&teacher),
            Some(json!({ "user_id": alice_id }))
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn exam_creation_lives_under_courses() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let veli = login_as(&app, &db, "veli", "teacher").await;
    let alice = login(&app, "alice").await; // student

    // The old flat creation route is gone: /exams only serves GET now.
    assert_eq!(
        send(
            &app,
            "POST",
            "/exams",
            Some(&teacher),
            Some(json!({"title":"t","kind":"quiz"}))
        )
        .await
        .status,
        StatusCode::METHOD_NOT_ALLOWED
    );

    let course_id = create_course(&app, &teacher, "algebra").await;
    // Creation inside the course echoes the course.
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course_id}/exams"),
        Some(&teacher),
        Some(json!({"title":"mt","kind":"midterm"})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["course"], course_id);

    // A teacher who doesn't manage the course cannot add exams to it.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/courses/{course_id}/exams"),
            Some(&veli),
            Some(json!({"title":"x","kind":"quiz"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // An unenrolled student cannot list the course's exams …
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/courses/{course_id}/exams"),
            Some(&alice),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // … but an enrolled one can.
    let alice_id = me_id(&app, &alice).await;
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/courses/{course_id}/enrollments"),
            Some(&teacher),
            Some(json!({ "user_id": alice_id }))
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        common::items(
            &send(
                &app,
                "GET",
                &format!("/courses/{course_id}/exams"),
                Some(&alice),
                None
            )
            .await
            .body
        )
        .len(),
        1
    );
}

#[tokio::test]
async fn grading_requires_enrollment() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await; // student
    let alice_id = me_id(&app, &alice).await;

    let course_id = create_course(&app, &teacher, "algebra").await;
    let exam_id = create_exam(&app, &teacher, &course_id, "mt", "quiz").await;
    let grade_uri = format!("/exams/{exam_id}/results");
    let grade_body = json!({ "mark": 70, "user_id": alice_id });

    // Not enrolled -> 400.
    assert_eq!(
        send(
            &app,
            "POST",
            &grade_uri,
            Some(&teacher),
            Some(grade_body.clone())
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );

    // Enrolled -> 200.
    enroll(&app, &teacher, &course_id, &alice_id).await;
    assert_eq!(
        send(
            &app,
            "POST",
            &grade_uri,
            Some(&teacher),
            Some(grade_body.clone())
        )
        .await
        .status,
        StatusCode::OK
    );

    // Unenrolling blocks further grading but keeps the recorded result:
    // the row still lists for the teacher and the student still reads it.
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/courses/{course_id}/enrollments/{alice_id}"),
            Some(&teacher),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(&app, "POST", &grade_uri, Some(&teacher), Some(grade_body))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        common::items(
            &send(&app, "GET", &grade_uri, Some(&teacher), None)
                .await
                .body
        )
        .len(),
        1
    );
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/exams/{exam_id}/result"),
            Some(&alice),
            None
        )
        .await
        .body["mark"],
        70
    );

    // ...but the mark vanishes from the report until re-enrollment restores it.
    let report = send(&app, "GET", "/marks/me", Some(&alice), None).await;
    assert_eq!(report.body["courses"].as_array().unwrap().len(), 0);
    enroll(&app, &teacher, &course_id, &alice_id).await;
    let report = send(&app, "GET", "/marks/me", Some(&alice), None).await;
    assert_eq!(report.body["courses"][0]["average"], 70.0);
}

#[tokio::test]
async fn exam_writes_follow_course_management() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let veli = login_as(&app, &db, "veli", "teacher").await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let course_id = create_course(&app, &teacher, "algebra").await;

    // A manager creates an exam in the teacher's course...
    let exam_id = create_exam(&app, &boss, &course_id, "mt", "midterm").await;

    // ...and the course creator (not the exam's creator) can edit and delete it.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/exams/{exam_id}"),
            Some(&teacher),
            Some(json!({"title":"renamed"}))
        )
        .await
        .body["title"],
        "renamed"
    );
    // An unrelated teacher still cannot.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/exams/{exam_id}"),
            Some(&veli),
            Some(json!({"title":"hijack"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/exams/{exam_id}"),
            Some(&teacher),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn exam_statistics_summarize_results() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await;
    let bob = login(&app, "bob").await;
    let alice_id = me_id(&app, &alice).await;
    let bob_id = me_id(&app, &bob).await;

    let course_id = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course_id, &alice_id).await;
    enroll(&app, &teacher, &course_id, &bob_id).await;
    let exam_id = create_exam(&app, &teacher, &course_id, "mt", "quiz").await;
    let stats_uri = format!("/exams/{exam_id}/statistics");

    // Nothing graded yet: zero count, null aggregates.
    let res = send(&app, "GET", &stats_uri, Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["graded"], 0);
    assert_eq!(res.body["average"], serde_json::Value::Null);
    assert_eq!(res.body["min"], serde_json::Value::Null);
    assert_eq!(res.body["max"], serde_json::Value::Null);

    for (id, mark) in [(&alice_id, 60), (&bob_id, 80)] {
        send(
            &app,
            "POST",
            &format!("/exams/{exam_id}/results"),
            Some(&teacher),
            Some(json!({ "mark": mark, "user_id": id })),
        )
        .await;
    }

    let res = send(&app, "GET", &stats_uri, Some(&teacher), None).await;
    assert_eq!(res.body["graded"], 2);
    assert_eq!(res.body["average"], 70.0);
    assert_eq!(res.body["min"], 60);
    assert_eq!(res.body["max"], 80);

    // Students don't see statistics; missing exam is a 404.
    assert_eq!(
        send(&app, "GET", &stats_uri, Some(&alice), None)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn weighted_averages_follow_kind_weights() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let manager = login_as(&app, &db, "boss", "manager").await;
    let alice = login(&app, "alice").await;
    let alice_id = me_id(&app, &alice).await;

    // Weights are school policy, per kind: quizzes count once, midterms three
    // times (the defaults are all 1).
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "exam_kinds": [
            {"name": "quiz", "weight": 1},
            {"name": "midterm", "weight": 3},
            {"name": "final", "weight": 4},
            {"name": "homework", "weight": 1},
        ]})),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    // Course A: quiz (w1, mark 50) + midterm (w3, mark 90) -> (50 + 270) / 4 = 80.
    let algebra = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &algebra, &alice_id).await;
    let quiz = create_exam(&app, &teacher, &algebra, "quiz", "quiz").await;
    let midterm = create_exam(&app, &teacher, &algebra, "midterm", "midterm").await;
    // A third exam stays ungraded and must not drag the average.
    create_exam(&app, &teacher, &algebra, "final", "final").await;
    for (exam, mark) in [(&quiz, 50), (&midterm, 90)] {
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/results"),
            Some(&teacher),
            Some(json!({ "mark": mark, "user_id": alice_id })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK);
    }

    // Course B: enrolled, nothing graded -> null average.
    let physics = create_course(&app, &teacher, "physics").await;
    enroll(&app, &teacher, &physics, &alice_id).await;
    create_exam(&app, &teacher, &physics, "hw", "homework").await;

    let report = send(&app, "GET", "/marks/me", Some(&alice), None).await;
    assert_eq!(report.status, StatusCode::OK);
    let courses = report.body["courses"].as_array().unwrap();
    assert_eq!(courses.len(), 2);
    let algebra_block = courses
        .iter()
        .find(|c| c["course"]["title"] == "algebra")
        .unwrap();
    let physics_block = courses
        .iter()
        .find(|c| c["course"]["title"] == "physics")
        .unwrap();

    assert_eq!(algebra_block["average"], 80.0);
    assert_eq!(algebra_block["results"].as_array().unwrap().len(), 2);
    // Each entry carries its kind's resolved weight.
    let midterm_entry = algebra_block["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "midterm")
        .unwrap();
    assert_eq!(midterm_entry["weight"], 3);
    assert_eq!(physics_block["average"], serde_json::Value::Null);
    assert_eq!(physics_block["results"].as_array().unwrap().len(), 0);
    // Overall skips the null course instead of zeroing it.
    assert_eq!(report.body["overall_average"], 80.0);
    assert_eq!(report.body["user"], alice_id);
}

/// Kind weights resolve at report time: editing a kind's weight re-weights
/// already-graded exams, and a kind removed from settings counts once.
#[tokio::test]
async fn kind_weight_edits_reweight_reports_live() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let manager = login_as(&app, &db, "boss", "manager").await;
    let alice = login(&app, "alice").await;
    let alice_id = me_id(&app, &alice).await;

    let course = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course, &alice_id).await;
    let quiz = create_exam(&app, &teacher, &course, "q", "quiz").await;
    let oral = create_exam(&app, &teacher, &course, "o", "oral").await;
    // Ungraded for now — its kind gets retired below, out from under it.
    let project = create_exam(&app, &teacher, &course, "p", "project").await;
    for (exam, mark) in [(&quiz, 40), (&oral, 80)] {
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/results"),
            Some(&teacher),
            Some(json!({ "mark": mark, "user_id": alice_id })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK);
    }

    // Default weights (all 1): plain mean of 40 and 80.
    let report = send(&app, "GET", "/marks/me", Some(&alice), None).await;
    assert_eq!(report.body["courses"][0]["average"], 60.0);

    // Triple the oral weight — the already-graded exam follows immediately:
    // (40 + 240) / 4 = 70.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "exam_kinds": [
            {"name": "quiz", "weight": 1},
            {"name": "oral", "weight": 3},
        ]})),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let report = send(&app, "GET", "/marks/me", Some(&alice), None).await;
    assert_eq!(report.body["courses"][0]["average"], 70.0);

    // The PATCH above also retired `project` (allowed — no marks of that kind
    // existed yet; a graded kind can't leave at all, see
    // `exam_kind_removal_blocks_while_marks_exist`). Its exam lives on, but a
    // retired kind takes no new marks: that is the other end of the same rule,
    // and it is what keeps "a kind with marks cannot be removed" true however
    // the two writes interleave.
    let grade_project = async || {
        send(
            &app,
            "POST",
            &format!("/exams/{project}/results"),
            Some(&teacher),
            Some(json!({ "mark": 60, "user_id": alice_id })),
        )
        .await
    };
    assert_eq!(grade_project().await.status, StatusCode::CONFLICT);
    // Offered again — at weight 1, so the exam counts exactly as an exam of a
    // since-removed kind would have.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "exam_kinds": [
            {"name": "quiz", "weight": 1},
            {"name": "oral", "weight": 3},
            {"name": "project", "weight": 1},
        ]})),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(grade_project().await.status, StatusCode::OK);
    // (40 + 240 + 60) / 5 = 68.
    let report = send(&app, "GET", "/marks/me", Some(&alice), None).await;
    assert_eq!(report.body["courses"][0]["average"], 68.0);
    let project_entry = report.body["courses"][0]["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "project")
        .unwrap()
        .clone();
    assert_eq!(project_entry["weight"], 1);
}

#[tokio::test]
async fn marks_reports_are_self_or_teacher_scoped() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await;
    let bob = login(&app, "bob").await;
    let alice_id = me_id(&app, &alice).await;

    // Own report works for a student.
    assert_eq!(
        send(&app, "GET", "/marks/me", Some(&alice), None)
            .await
            .status,
        StatusCode::OK
    );
    // Another user's report needs teacher+.
    assert_eq!(
        send(&app, "GET", &format!("/marks/{alice_id}"), Some(&bob), None)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/marks/{alice_id}"),
            Some(&teacher),
            None
        )
        .await
        .status,
        StatusCode::OK
    );
}

/// One teacher's course is a wall: another teacher can't read its roster,
/// results, statistics, live monitor, answer key, or answer sheets, and the
/// per-user reports narrow to the courses the caller manages. Managers see
/// through the wall; an enrolled student sees the course itself.
#[tokio::test]
async fn course_data_is_walled_off_from_other_teachers() {
    let (app, db) = app_and_db().await;
    let owner = login_as(&app, &db, "owner", "teacher").await;
    let rival = login_as(&app, &db, "rival", "teacher").await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let alice = login(&app, "alice").await;
    let alice_id = me_id(&app, &alice).await;

    let course_id = create_course(&app, &owner, "algebra").await;
    enroll(&app, &owner, &course_id, &alice_id).await;
    let exam_id = create_exam(&app, &owner, &course_id, "mt", "quiz").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam_id}/results"),
        Some(&owner),
        Some(json!({ "mark": 70, "user_id": alice_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    // Every course-scoped teacher read is refused for the rival …
    for uri in [
        format!("/courses/{course_id}/enrollments"),
        format!("/exams/{exam_id}/results"),
        format!("/exams/{exam_id}/statistics"),
        format!("/exams/{exam_id}/live"),
        format!("/exams/{exam_id}/questions"),
        format!("/exams/{exam_id}/attempts/{alice_id}/answers"),
    ] {
        let res = send(&app, "GET", &uri, Some(&rival), None).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{uri}");
    }

    // … and open to the owner and a manager. (The answer sheet turns into a
    // 404 once authorization clears: alice never sat the exam.)
    for caller in [&owner, &boss] {
        for uri in [
            format!("/courses/{course_id}/enrollments"),
            format!("/exams/{exam_id}/results"),
            format!("/exams/{exam_id}/statistics"),
            format!("/exams/{exam_id}/live"),
            format!("/exams/{exam_id}/questions"),
        ] {
            let res = send(&app, "GET", &uri, Some(caller), None).await;
            assert_eq!(res.status, StatusCode::OK, "{uri}");
        }
        let res = send(
            &app,
            "GET",
            &format!("/exams/{exam_id}/attempts/{alice_id}/answers"),
            Some(caller),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::NOT_FOUND);
    }

    // The marks report narrows to the caller's courses: the rival gets an
    // empty shell, the owner and the manager get the graded course.
    let res = send(
        &app,
        "GET",
        &format!("/marks/{alice_id}"),
        Some(&rival),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body["courses"].as_array().unwrap().is_empty());
    assert!(res.body["overall_average"].is_null());
    for caller in [&owner, &boss] {
        let res = send(
            &app,
            "GET",
            &format!("/marks/{alice_id}"),
            Some(caller),
            None,
        )
        .await;
        assert_eq!(res.body["courses"].as_array().unwrap().len(), 1);
        assert_eq!(res.body["overall_average"], 70.0);
    }

    // The enrolled student reads the course and its exam, and both catalogs
    // include them; the rival's catalogs stay empty; a manager sees all.
    for uri in [format!("/courses/{course_id}"), format!("/exams/{exam_id}")] {
        let res = send(&app, "GET", &uri, Some(&alice), None).await;
        assert_eq!(res.status, StatusCode::OK, "{uri}");
    }
    for (caller, visible) in [(&alice, 1), (&rival, 0), (&boss, 1)] {
        for uri in ["/courses", "/exams"] {
            let res = send(&app, "GET", uri, Some(caller), None).await;
            assert_eq!(
                common::items(&res.body).len(),
                visible,
                "{uri} as seen by caller"
            );
        }
    }
}

#[tokio::test]
async fn deleting_course_cascades_enrollments_exams_and_results() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await;
    let alice_id = me_id(&app, &alice).await;

    let course_id = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course_id, &alice_id).await;
    let exam_id = create_exam(&app, &teacher, &course_id, "mt", "quiz").await;
    send(
        &app,
        "POST",
        &format!("/exams/{exam_id}/results"),
        Some(&teacher),
        Some(json!({ "mark": 70, "user_id": alice_id })),
    )
    .await;

    unenroll(&app, &teacher, &course_id, &alice_id).await;
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/courses/{course_id}"),
            Some(&teacher),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );

    // Course, exam, result, and enrollment are all gone.
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/courses/{course_id}"),
            Some(&teacher),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/exams/{exam_id}"),
            Some(&teacher),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/exams/{exam_id}/result"),
            Some(&alice),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        common::items(
            &send(&app, "GET", "/courses/me", Some(&alice), None)
                .await
                .body,
        )
        .len(),
        0
    );
    assert_eq!(
        send(&app, "GET", "/marks/me", Some(&alice), None)
            .await
            .body["courses"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

/// Same guarantee as attendance/grading: concurrent enrolls of the same pair
/// converge on one row instead of racing the unique index into a 500.
#[tokio::test]
async fn concurrent_enrollments_never_collide() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await;
    let alice_id = me_id(&app, &alice).await;
    let course_id = create_course(&app, &teacher, "algebra").await;

    let mut handles = Vec::new();
    for _ in 0..24 {
        let app = app.clone();
        let teacher = teacher.clone();
        let uri = format!("/courses/{course_id}/enrollments");
        let body = json!({ "user_id": alice_id });
        handles.push(tokio::spawn(async move {
            send(&app, "POST", &uri, Some(&teacher), Some(body))
                .await
                .status
        }));
    }
    for h in handles {
        assert_eq!(
            h.await.unwrap(),
            StatusCode::OK,
            "concurrent enroll must not fail"
        );
    }

    let roster = send(
        &app,
        "GET",
        &format!("/courses/{course_id}/enrollments"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(
        common::items(&roster.body).len(),
        1,
        "upsert keeps a single row"
    );
}

// --- roles / RBAC --------------------------------------------------------

#[tokio::test]
async fn students_cannot_create_events() {
    let app = mem_app().await;
    let ali = login(&app, "ali").await; // default role: student
    let res = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "class" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn students_never_mark_attendance() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await; // student
    let bob = login(&app, "bob").await; // student
    let bob_id = id_of(&send(&app, "GET", "/auth/me", Some(&bob), None).await.body);

    let ev = id_of(
        &send(
            &app,
            "POST",
            "/events",
            Some(&teacher),
            Some(json!({ "title": "class" })),
        )
        .await
        .body,
    );

    // Taking attendance is teacher+: a student cannot mark even themselves…
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/events/{ev}/attendance"),
            Some(&alice),
            Some(json!({"status":"present"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    // …let alone someone else.
    assert_eq!(
        send(
            &app,
            "POST",
            &format!("/events/{ev}/attendance"),
            Some(&alice),
            Some(json!({"status":"present","user_id":bob_id}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    // And cannot remove attendance rows or read the roster report.
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/events/{ev}/attendance/{bob_id}"),
            Some(&alice),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/events/{ev}/roster"),
            Some(&alice),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
}

/// Registration seats follow the placement rule — teachers seat students and
/// themselves, never other staff; students never touch the endpoints — and a
/// repeat register is a no-op on the same single row. Deleting the event
/// removes its signup rows with it.
#[tokio::test]
async fn event_registration_follows_placement_rules() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let deniz = login_as(&app, &db, "deniz", "teacher").await;
    let veli = login(&app, "veli").await; // student
    let veli_id = me_id(&app, &veli).await;
    let ali_id = me_id(&app, &ali).await;
    let deniz_id = me_id(&app, &deniz).await;

    let res = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "seminar", "audience": { "kind": "registration" } })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let ev = id_of(&res.body);
    let register_uri = format!("/events/{ev}/register");

    // Students can't register — not even themselves.
    let res = send(&app, "POST", &register_uri, Some(&veli), Some(json!({}))).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // A teacher seats a student; the row names who placed whom.
    let res = send(
        &app,
        "POST",
        &register_uri,
        Some(&ali),
        Some(json!({ "user_id": veli_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["user"]["id"], veli_id);
    assert_eq!(res.body["registered_by"]["id"], ali_id);

    // Registering the same student again is a no-op on the same seat.
    let res = send(
        &app,
        "POST",
        &register_uri,
        Some(&deniz),
        Some(json!({ "user_id": veli_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let roster = send(
        &app,
        "GET",
        &format!("/events/{ev}/roster"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::total(&roster.body), 1, "upsert keeps a single seat");

    // A teacher takes their own seat by omitting the target…
    let res = send(&app, "POST", &register_uri, Some(&ali), Some(json!({}))).await;
    assert_eq!(res.status, StatusCode::OK);
    // …but never seats another staff member, and ghosts don't register.
    let res = send(
        &app,
        "POST",
        &register_uri,
        Some(&ali),
        Some(json!({ "user_id": deniz_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "POST",
        &register_uri,
        Some(&ali),
        Some(json!({ "user_id": "ghost" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // Non-registration events refuse the endpoint outright.
    let school = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "assembly" })),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/events/{}/register", id_of(&school.body)),
        Some(&ali),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // Unregistering mirrors the rule: students never, staff seats are their
    // own, students' seats are anyone's (teacher+) to free.
    let res = send(
        &app,
        "DELETE",
        &format!("{register_uri}/{ali_id}"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "DELETE",
        &format!("{register_uri}/{ali_id}"),
        Some(&deniz),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "staff seat is not deniz's"
    );
    let res = send(
        &app,
        "DELETE",
        &format!("{register_uri}/{veli_id}"),
        Some(&deniz),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "DELETE",
        &format!("{register_uri}/{veli_id}"),
        Some(&deniz),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "seat already freed");

    // Deleting the event cascades its signup rows away.
    let res = send(&app, "DELETE", &format!("/events/{ev}"), Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let left: Vec<hezarfen_backend::domain::registration::Registration> =
        db.select("registration").await.unwrap();
    assert!(left.is_empty(), "event delete leaves no seats behind");
}

/// A capacity caps live seats, not history: re-registering never double-counts,
/// freeing a seat reopens it, and shrinking the cap below the current count
/// only blocks new seats.
#[tokio::test]
async fn event_registration_capacity_caps_seats() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let mina = login(&app, "mina").await;
    let veli_id = me_id(&app, &veli).await;
    let mina_id = me_id(&app, &mina).await;

    let res = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "workshop",
                     "audience": { "kind": "registration", "capacity": 1 } })),
    )
    .await;
    let ev = id_of(&res.body);
    let register_uri = format!("/events/{ev}/register");
    let register = |user: String| {
        let app = &app;
        let ali = &ali;
        let uri = &register_uri;
        async move {
            send(
                app,
                "POST",
                uri,
                Some(ali),
                Some(json!({ "user_id": user })),
            )
            .await
            .status
        }
    };

    assert_eq!(register(veli_id.clone()).await, StatusCode::OK);
    assert_eq!(
        register(mina_id.clone()).await,
        StatusCode::CONFLICT,
        "full"
    );
    assert_eq!(
        register(veli_id.clone()).await,
        StatusCode::OK,
        "re-register of a seated user never counts against the cap"
    );

    // Freeing the seat reopens the list.
    let res = send(
        &app,
        "DELETE",
        &format!("{register_uri}/{veli_id}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert_eq!(register(mina_id.clone()).await, StatusCode::OK);

    // Shrinking the cap under the current count keeps the seats and only
    // refuses growth.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/events/{ev}"),
            Some(&ali),
            Some(json!({ "audience": { "kind": "registration", "capacity": 2 } })),
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(register(veli_id.clone()).await, StatusCode::OK);
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/events/{ev}"),
            Some(&ali),
            Some(json!({ "audience": { "kind": "registration", "capacity": 1 } })),
        )
        .await
        .status,
        StatusCode::OK
    );
    let roster = send(
        &app,
        "GET",
        &format!("/events/{ev}/roster"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(
        common::total(&roster.body),
        2,
        "shrink keeps existing seats"
    );
}

/// Regression: re-registering an already-seated student used to be an UPSERT
/// that silently rewrote `registered_by` to the second caller. The documented
/// contract is a no-op — the seat and its audit trail come back unchanged.
#[tokio::test]
async fn re_registering_returns_the_seat_untouched() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let deniz = login_as(&app, &db, "deniz", "teacher").await;
    let veli = login(&app, "veli").await;
    let veli_id = me_id(&app, &veli).await;
    let ali_id = me_id(&app, &ali).await;

    let res = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "seminar", "audience": { "kind": "registration" } })),
    )
    .await;
    let uri = format!("/events/{}/register", id_of(&res.body));

    let first = send(
        &app,
        "POST",
        &uri,
        Some(&ali),
        Some(json!({ "user_id": veli_id })),
    )
    .await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(first.body["registered_by"]["id"], ali_id);

    let second = send(
        &app,
        "POST",
        &uri,
        Some(&deniz),
        Some(json!({ "user_id": veli_id })),
    )
    .await;
    assert_eq!(second.status, StatusCode::OK);
    assert_eq!(
        second.body["registered_by"]["id"], ali_id,
        "re-registering must not rewrite who placed the student"
    );
    // Regression: the response used to resolve people only from {target,
    // caller}, so the original registrar degraded to a bare ULID username.
    assert_eq!(
        second.body["registered_by"]["username"], "ali",
        "the original registrar must resolve to a person, not a bare id"
    );
    assert_eq!(second.body["user"]["username"], "veli");
}

/// Regression: the capacity check used to run inside a `BEGIN…COMMIT` whose
/// cross-record `count()` SurrealDB does not conflict-check against a
/// concurrent insert (write-skew) — two racing placements both saw the last
/// seat free and a capacity-1 event admitted 2. The register path now
/// serializes the check-then-write, so exactly one wins and the loser gets
/// the same 409 as a sequential over-fill.
#[tokio::test]
async fn concurrent_placements_never_exceed_capacity() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let ayse = login(&app, "ayse").await;
    let veli_id = me_id(&app, &veli).await;
    let ayse_id = me_id(&app, &ayse).await;

    let res = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "trip", "audience": { "kind": "registration", "capacity": 1 } })),
    )
    .await;
    let ev = id_of(&res.body);
    let uri = format!("/events/{ev}/register");

    for _round in 0..10 {
        for uid in [&veli_id, &ayse_id] {
            let _ = send(&app, "DELETE", &format!("{uri}/{uid}"), Some(&ali), None).await;
        }
        let (a, b) = tokio::join!(
            send(
                &app,
                "POST",
                &uri,
                Some(&ali),
                Some(json!({ "user_id": veli_id })),
            ),
            send(
                &app,
                "POST",
                &uri,
                Some(&ali),
                Some(json!({ "user_id": ayse_id })),
            ),
        );
        for status in [a.status, b.status] {
            assert!(
                status == StatusCode::OK || status == StatusCode::CONFLICT,
                "a lost capacity race must be a 409, got {status}"
            );
        }
        let roster = send(
            &app,
            "GET",
            &format!("/events/{ev}/roster"),
            Some(&ali),
            None,
        )
        .await;
        assert!(
            common::total(&roster.body) <= 1,
            "capacity 1 must never over-admit, roster={}",
            roster.body
        );
    }
}

/// The course capacity cap is a counter column on the course row now, not a
/// process mutex: `UPDATE ... SET n += 1 WHERE n < capacity` is atomic per
/// record, so it holds however the racing enrolls interleave. Removing
/// that `WHERE` (or the claim entirely) puts a third student on a capacity-2
/// course and fails this. The unenroll each round proves the seat comes back —
/// without the decrement the course is permanently full by round two.
#[tokio::test]
async fn concurrent_enrolls_never_exceed_course_capacity() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let mut students = Vec::new();
    for name in ["ali", "veli", "ayse"] {
        let cookie = login(&app, name).await;
        let id = me_id(&app, &cookie).await;
        students.push(id);
    }
    let res = send(
        &app,
        "POST",
        "/courses",
        Some(&teacher),
        Some(json!({ "title": "small", "description": "", "capacity": 2 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let course = id_of(&res.body);
    let uri = format!("/courses/{course}/enrollments");

    for _round in 0..8 {
        for id in &students {
            let _ = send(&app, "DELETE", &format!("{uri}/{id}"), Some(&teacher), None).await;
        }
        let mut handles = Vec::new();
        for id in &students {
            let (app, teacher, uri) = (app.clone(), teacher.clone(), uri.clone());
            let body = json!({ "user_id": id });
            handles.push(tokio::spawn(async move {
                send(&app, "POST", &uri, Some(&teacher), Some(body))
                    .await
                    .status
            }));
        }
        for handle in handles {
            let status = handle.await.unwrap();
            assert!(
                status == StatusCode::OK || status == StatusCode::CONFLICT,
                "a lost capacity race must be a 409, got {status}"
            );
        }
        let roster = send(&app, "GET", &uri, Some(&teacher), None).await;
        assert!(
            common::total(&roster.body) <= 2,
            "capacity 2 must never over-admit, roster={}",
            roster.body
        );
    }
}

/// The same (user, course) pair enrolled concurrently must cost exactly one
/// seat: the row is idempotent by its composite id, so a second enroll that
/// finds the pair already there returns it *without* claiming, and a `CREATE`
/// that loses the id race releases the seat it claimed before reading the
/// winner's row back. Either way the counter must equal the roster — a drift
/// above it is permanent (nothing recomputes the counter), so the course would
/// be full forever. Stored state is the only witness: the embedded engine can
/// tell two racers they both won, so the HTTP statuses prove nothing.
//
// Nothing serializes these any more: the seat and the row are claimed in one
// transaction (`cap::claim_and_create`), so a racer that loses the id has its
// own increment rolled back with the transaction and reads the winner's row
// instead of being told the course is full. Both the early return and that
// rollback are pinned by the assertion below (counter == roster).
#[tokio::test]
async fn concurrent_enrolls_of_one_pair_claim_one_seat() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let alice = login(&app, "alice").await;
    let alice_id = me_id(&app, &alice).await;
    let res = send(
        &app,
        "POST",
        "/courses",
        Some(&teacher),
        Some(json!({ "title": "seats", "description": "", "capacity": 2 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let course = id_of(&res.body);
    let uri = format!("/courses/{course}/enrollments");

    let mut handles = Vec::new();
    for _ in 0..16 {
        let (app, teacher, uri) = (app.clone(), teacher.clone(), uri.clone());
        let body = json!({ "user_id": alice_id });
        handles.push(tokio::spawn(async move {
            send(&app, "POST", &uri, Some(&teacher), Some(body))
                .await
                .status
        }));
    }
    for handle in handles {
        let status = handle.await.unwrap();
        assert_eq!(status, StatusCode::OK, "a same-pair enroll is idempotent");
    }

    let roster = send(&app, "GET", &uri, Some(&teacher), None).await;
    assert_eq!(common::total(&roster.body), 1, "one pair, one row");
    let mut counted = db
        .query("SELECT VALUE enrollment_count FROM type::record('course', $c)")
        .bind(("c", course.clone()))
        .await
        .expect("counter read")
        .check()
        .expect("counter read");
    assert_eq!(
        counted.take::<Vec<i64>>(0).expect("counter column"),
        vec![1],
        "one row on the roster must have cost exactly one seat"
    );
}

/// The same stampede on a course with exactly one seat. Every racer enrolls the
/// *same* student, so between them they owe one seat — but on a cap this tight
/// the claim is the first thing to fail, and answering that with "the course is
/// full" would refuse a student the enrollment that just succeeded on their
/// behalf. So the pair's own row is looked for inside the claim's transaction,
/// before the seat is blamed. Drop that gate and every loser here is a 409.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_enrolls_of_one_seat_never_refuse_their_own_winner() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "tight_teacher", "teacher").await;
    let student = login(&app, "tight_student").await;
    let student_id = me_id(&app, &student).await;
    let res = send(
        &app,
        "POST",
        "/courses",
        Some(&teacher),
        Some(json!({ "title": "one seat", "description": "", "capacity": 1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let course = id_of(&res.body);
    let uri = format!("/courses/{course}/enrollments");

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let (app, teacher, uri) = (app.clone(), teacher.clone(), uri.clone());
        let body = json!({ "user_id": student_id });
        tasks.push(tokio::spawn(async move {
            send(&app, "POST", &uri, Some(&teacher), Some(body)).await
        }));
    }
    for task in tasks {
        let res = task.await.unwrap();
        assert_eq!(
            res.status,
            StatusCode::OK,
            "one student, one seat, no refusal: {}",
            res.body
        );
    }

    let roster = send(&app, "GET", &uri, Some(&teacher), None).await;
    assert_eq!(common::total(&roster.body), 1, "one seat: {}", roster.body);
    let mut counted = db
        .query("SELECT VALUE enrollment_count FROM type::record('course', $c)")
        .bind(("c", course.clone()))
        .await
        .expect("counter read")
        .check()
        .expect("counter read");
    assert_eq!(
        counted.take::<Vec<i64>>(0).expect("counter column"),
        vec![1],
        "and the seat it cost is the one seat the course has"
    );
}

/// The two reference counters a subject carries — the whole basis of its
/// delete guard — must equal the rows that actually point at it, through every
/// writer: create, re-tag, delete, and the exam cascade. Nothing recomputes
/// them, so a single missed decrement makes a subject undeletable forever.
///
/// This bites on the halves the HTTP status alone cannot see. Drop the
/// release of the *old* subject from either re-tag and the counter drifts up:
/// the last delete below comes back 409 instead of 204.
#[tokio::test]
async fn subject_reference_counts_track_every_writer() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "refc_t", "teacher").await;
    let course = create_course(&app, &teacher, "physics").await;
    let from = create_subject(&app, &teacher, &course, "optics").await;
    let to = create_subject(&app, &teacher, &course, "waves").await;
    let exam = create_exam(&app, &teacher, &course, "midterm", "midterm").await;
    let due = Timestamp::now().as_millis() + 86_400_000;

    let counts = async |subject: &str| -> (i64, i64) {
        let mut result = db
            .query(
                "SELECT VALUE [exam_question_count ?? 0, homework_count ?? 0] \
                 FROM type::record('subject', $s)",
            )
            .bind(("s", subject.to_string()))
            .await
            .expect("counter read")
            .check()
            .expect("counter read");
        let pair = result
            .take::<Vec<Vec<i64>>>(0)
            .expect("counter columns")
            .pop()
            .expect("the subject row");
        (pair[0], pair[1])
    };

    let question = create_question(
        &app,
        &teacher,
        &exam,
        &from,
        json!({ "text": "How fast?", "kind": "text", "points": 10 }),
    )
    .await;
    let cascaded = create_question(
        &app,
        &teacher,
        &exam,
        &from,
        json!({ "text": "How bright?", "kind": "text", "points": 10 }),
    )
    .await;
    let homework = create_homework(&app, &teacher, &course, &from, "lenses", due).await;
    assert_eq!(counts(&from).await, (2, 1), "two questions and a homework");
    assert_eq!(counts(&to).await, (0, 0), "the spare subject holds nothing");

    // A re-tag moves a reference: both ends must move, not just the new one.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{question}"),
        Some(&teacher),
        Some(json!({ "subject_id": to })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/homework/{homework}"),
        Some(&teacher),
        Some(json!({ "subject_id": to })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(counts(&from).await, (1, 0), "both re-tags must have left");
    assert_eq!(counts(&to).await, (1, 1), "and both must have arrived");

    // A re-tag back to where it already is moves nothing.
    let res = send(
        &app,
        "PATCH",
        &format!("/homework/{homework}"),
        Some(&teacher),
        Some(json!({ "subject_id": to })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(counts(&to).await, (1, 1), "a no-op re-tag is a no-op");

    // Deleting the referencing rows gives every reference back — including the
    // question the *exam* cascade takes with it, which the row delete never
    // sees.
    let res = send(
        &app,
        "DELETE",
        &format!("/homework/{homework}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/exams/{exam}/questions/{question}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    assert_eq!(counts(&to).await, (0, 0), "the new subject is free again");
    assert_eq!(
        counts(&from).await,
        (1, 0),
        "the cascaded question still holds"
    );
    assert!(!cascaded.is_empty());

    let res = send(
        &app,
        "DELETE",
        &format!("/exams/{exam}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    assert_eq!(counts(&from).await, (0, 0), "the exam cascade gave it back");

    for subject in [&from, &to] {
        let res = send(
            &app,
            "DELETE",
            &format!("/subjects/{subject}"),
            Some(&teacher),
            None,
        )
        .await;
        assert_eq!(
            res.status,
            StatusCode::NO_CONTENT,
            "an unreferenced subject must delete: {}",
            res.body
        );
    }
}

/// A subject delete racing question creates on the same subject. Exactly one
/// outcome is legal, and it is decided by the delete's own `WHERE`: either the
/// subject is gone and *no* question points at it, or it survived and its
/// counter equals the questions that landed. Stored state is the only witness —
/// the embedded engine can tell two racers they both won, so the statuses prove
/// nothing.
///
/// The count-then-delete this replaced fails here: it reads "no questions",
/// then deletes while a create that already passed its own subject check lands
/// its row on the corpse.
#[tokio::test]
async fn a_subject_delete_racing_question_creates_leaves_no_orphan() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "race_t", "teacher").await;
    let course = create_course(&app, &teacher, "chemistry").await;
    let subject = create_subject(&app, &teacher, &course, "bonds").await;
    let exam = create_exam(&app, &teacher, &course, "quiz", "quiz").await;
    // One question already tagged before the race starts, so the delete is
    // never legal: whichever way the interleaving falls, it must be refused.
    create_question(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "first", "kind": "text", "points": 1 }),
    )
    .await;

    let mut handles = Vec::new();
    for i in 0..8 {
        let (app, teacher, exam, subject) =
            (app.clone(), teacher.clone(), exam.clone(), subject.clone());
        handles.push(tokio::spawn(async move {
            let body = json!({
                "text": format!("q{i}"),
                "kind": "text",
                "points": 1,
                "subject_id": subject,
            });
            send(
                &app,
                "POST",
                &format!("/exams/{exam}/questions"),
                Some(&teacher),
                Some(body),
            )
            .await
            .status
        }));
    }
    let killer = {
        let (app, teacher, subject) = (app.clone(), teacher.clone(), subject.clone());
        tokio::spawn(async move {
            send(
                &app,
                "DELETE",
                &format!("/subjects/{subject}"),
                Some(&teacher),
                None,
            )
            .await
            .status
        })
    };
    for handle in handles {
        let status = handle.await.unwrap();
        assert!(
            status == StatusCode::CREATED || status == StatusCode::BAD_REQUEST,
            "a create either lands or is told the subject is gone, got {status}"
        );
    }
    let killed = killer.await.unwrap();

    let mut result = db
        .query("SELECT VALUE exam_question_count ?? 0 FROM type::record('subject', $s)")
        .bind(("s", subject.clone()))
        .await
        .expect("counter read")
        .check()
        .expect("counter read");
    let counter = result.take::<Vec<i64>>(0).expect("counter column");
    let mut rows = db
        .query("SELECT VALUE id FROM exam_question WHERE subject = type::record('subject', $s)")
        .bind(("s", subject.clone()))
        .await
        .expect("row read")
        .check()
        .expect("row read");
    let landed = rows
        .take::<Vec<surrealdb::types::RecordId>>(0)
        .expect("question rows")
        .len() as i64;
    assert_eq!(
        killed,
        StatusCode::CONFLICT,
        "a subject a question already points at may never be deleted"
    );
    assert_eq!(
        counter.first().copied(),
        Some(landed),
        "the subject must survive with its counter equal to the rows it guards"
    );
}

/// The signup list freezes the moment the event starts — both directions.
/// (The create-time no-past grace lets a just-started event exist, which is
/// exactly a closed list.)
#[tokio::test]
async fn event_registration_closes_at_start() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let veli_id = me_id(&app, &veli).await;
    let now = Timestamp::now().as_millis();

    // Started 30s ago: inside the scheduling grace, so creation passes — but
    // the list is already closed.
    let res = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "underway", "starts_at": now - 30_000,
                     "audience": { "kind": "registration" } })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let ev = id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/events/{ev}/register"),
        Some(&ali),
        Some(json!({ "user_id": veli_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT);
    let res = send(
        &app,
        "DELETE",
        &format!("/events/{ev}/register/{veli_id}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "unregister freezes too");

    // Regression: an ends_at-only event (a pure signup deadline) used to stay
    // open forever — the gate read starts_at alone. Ended 30s ago: inside the
    // creation grace, but the list is closed.
    let res = send(
        &app,
        "POST",
        "/events",
        Some(&ali),
        Some(json!({ "title": "deadline passed", "ends_at": now - 30_000,
                     "audience": { "kind": "registration" } })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let ev = id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/events/{ev}/register"),
        Some(&ali),
        Some(json!({ "user_id": veli_id })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CONFLICT,
        "an event whose end has passed takes no signups"
    );

    // A future or timeless event keeps its list open — a future signup
    // deadline (ends_at only) included.
    for body in [
        json!({ "title": "later", "starts_at": now + 3_600_000,
                "audience": { "kind": "registration" } }),
        json!({ "title": "deadline ahead", "ends_at": now + 3_600_000,
                "audience": { "kind": "registration" } }),
        json!({ "title": "whenever", "audience": { "kind": "registration" } }),
    ] {
        let res = send(&app, "POST", "/events", Some(&ali), Some(body)).await;
        let ev = id_of(&res.body);
        let res = send(
            &app,
            "POST",
            &format!("/events/{ev}/register"),
            Some(&ali),
            Some(json!({ "user_id": veli_id })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    }
}

#[tokio::test]
async fn event_management_follows_hierarchy() {
    let (app, db) = app_and_db().await;
    let owner = login_as(&app, &db, "owner", "teacher").await;
    let other = login_as(&app, &db, "other", "teacher").await;
    let boss = login_as(&app, &db, "boss", "manager").await;

    let ev = id_of(
        &send(
            &app,
            "POST",
            "/events",
            Some(&owner),
            Some(json!({ "title": "orig" })),
        )
        .await
        .body,
    );

    // A different teacher (not the creator) cannot edit it.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/events/{ev}"),
            Some(&other),
            Some(json!({"title":"hijack"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    // A manager can edit anyone's event.
    let res = send(
        &app,
        "PATCH",
        &format!("/events/{ev}"),
        Some(&boss),
        Some(json!({"title":"by mgr"})),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["title"], "by mgr");
    // The creator can still delete their own.
    assert_eq!(
        send(&app, "DELETE", &format!("/events/{ev}"), Some(&owner), None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn admin_manages_roles_with_guards() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let alice = login(&app, "alice").await; // student
    let alice_id = id_of(&send(&app, "GET", "/auth/me", Some(&alice), None).await.body);

    // Non-admins cannot reach the role endpoints.
    assert_eq!(
        send(&app, "GET", "/users", Some(&alice), None).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/users/{alice_id}/role"),
            Some(&alice),
            Some(json!({"role":"teacher"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // Admin lists users (at least admin + alice).
    let list = send(&app, "GET", "/users", Some(&admin), None).await;
    assert_eq!(list.status, StatusCode::OK);
    assert!(common::items(&list.body).len() >= 2);

    // Admin promotes alice to teacher; the change is reflected in the response...
    let promoted = send(
        &app,
        "PATCH",
        &format!("/users/{alice_id}/role"),
        Some(&admin),
        Some(json!({"role":"teacher"})),
    )
    .await;
    assert_eq!(promoted.status, StatusCode::OK);
    assert_eq!(promoted.body["role"], "teacher");
    // ...and takes effect immediately: alice can now create events.
    assert_eq!(
        send(
            &app,
            "POST",
            "/events",
            Some(&alice),
            Some(json!({ "title": "now allowed" }))
        )
        .await
        .status,
        StatusCode::CREATED
    );

    // Unknown role value -> 400.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/users/{alice_id}/role"),
            Some(&admin),
            Some(json!({"role":"wizard"}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    // Missing user -> 404.
    assert_eq!(
        send(
            &app,
            "PATCH",
            "/users/does-not-exist/role",
            Some(&admin),
            Some(json!({"role":"teacher"}))
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    // An admin cannot change their own role (anti-lockout guard).
    let admin_id = id_of(&send(&app, "GET", "/auth/me", Some(&admin), None).await.body);
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/users/{admin_id}/role"),
            Some(&admin),
            Some(json!({"role":"student"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
}

/// The AI service principal must never be mintable, storable, or filterable
/// over HTTP: it is the QUIC bridge's own identity, not an account anybody can
/// hold. `Role` carries no `serde` derive (`src/domain/role.rs:21`), so no
/// handler can bind a role straight out of a body — every request-supplied role
/// passes `Role::try_from_str`, which answers only with a member of
/// `constant::ROLES` (`src/constant.rs:674`, the five human roles).
///
/// Grep-verified 2026-08-12, the complete set of role paths a request can reach:
///   * `src/web/users.rs:453` — `PATCH /users/{id}/role`, the *only* path that
///     writes `user.role` from a caller's string (`Role::try_from_str`).
///   * `src/web/auth.rs:114` — `POST /auth/register`, hardcoded `Role::Student`
///     (`User::create` → `create_with_role`, `src/domain/user.rs:334`).
///   * `src/web/users.rs:231` — the user-search `?role=` filter (a read).
///   * `src/web/events.rs:89` — a role-audience event stores a role string of
///     its own on the event row.
/// The only other `create_with_role` caller is the out-of-band admin bootstrap
/// (`src/domain/user.rs:426`, hardcoded `Role::Admin`, behind no route).
///
/// corner-cut: that funnel list is grep-verified, not machine-enforced — a *new*
/// route writing an arbitrary role string would slip past this test. Upgrade
/// path: derive the surface list from the emitted OpenAPI, the way the auth-gate
/// test derives its protected-path set.
#[tokio::test]
async fn role_ai_is_unreachable_over_http() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let alice = login(&app, "alice").await;
    let alice_id = me_id(&app, &alice).await;

    // 1. Registration mints a student and nothing else — read back off the row,
    //    not off the register echo.
    let me = send(&app, "GET", "/auth/me", Some(&alice), None).await;
    assert_eq!(me.body["role"], "student");

    // 2. The one role-write funnel refuses it, and the stored role stands.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/users/{alice_id}/role"),
            Some(&admin),
            Some(json!({"role":"ai"}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/users/{alice_id}"),
            Some(&admin),
            None
        )
        .await
        .body["role"],
        "student"
    );

    // 3. The search filter refuses it too — an unknown role is a 400, never an
    //    empty page, so a caller can't probe for service accounts.
    assert_eq!(
        send(&app, "GET", "/users/search?q=&role=ai", Some(&admin), None)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );

    // 4. And an event audience, the other surface that stores a role string.
    assert_eq!(
        send(
            &app,
            "POST",
            "/events",
            Some(&admin),
            Some(json!({ "title": "ai only", "audience": { "kind": "role", "role": "ai" } }))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );

    // 5. The published contract never names it. The exact five-role equality is
    //    pinned in `limits_publishes_the_bounds_the_api_actually_enforces`; this
    //    is the `ai`-specific
    //    angle on the same array, so growing `ROLES` fails here as well.
    let roles = send(&app, "GET", "/limits", None, None).await.body["user"]["roles"].clone();
    let roles = roles.as_array().expect("limits user.roles array");
    assert_eq!(roles.len(), 5, "{roles:?}");
    assert!(!roles.iter().any(|r| r == "ai"), "{roles:?}");

    // 6. Neither does the OpenAPI document Swagger UI renders its forms from.
    //    The request schema once `$ref`ed the response-side `Role`, which does
    //    name `ai` — so the try-it-out form offered a value the funnel above has
    //    always refused with a 400. Request schemas carry the five; the response
    //    schema keeps `ai` (a service principal really does read back as one)
    //    and must say what it is, since utoipa emits only the *type*-level doc
    //    comment, never a variant's.
    let spec = send(&app, "GET", "/api-docs/openapi.json", None, None)
        .await
        .body;
    let schemas = &spec["components"]["schemas"];
    let human = json!(["parent", "student", "teacher", "manager", "admin"]);
    assert_eq!(schemas["AssignableRole"]["enum"], human, "{schemas:?}");
    assert_eq!(
        spec["paths"]["/users/{id}/role"]["patch"]["requestBody"]["content"]["application/json"]["schema"]
            ["$ref"],
        "#/components/schemas/SetRole"
    );
    assert_eq!(
        schemas["SetRole"]["properties"]["role"]["$ref"],
        "#/components/schemas/AssignableRole"
    );
    let response_role = &schemas["Role"];
    assert_eq!(
        response_role["enum"],
        json!(["ai", "parent", "student", "teacher", "manager", "admin"]),
        "{response_role:?}"
    );
    let described = response_role["description"]
        .as_str()
        .expect("Role schema carries its type-level doc comment");
    assert!(described.contains("never assignable"), "{described:?}");
}

// --- users: personal info (profile) ---------------------------------------

#[tokio::test]
async fn profile_starts_empty_and_updates_via_users_me() {
    let app = mem_app().await;
    let bob = login(&app, "bob").await;

    // A fresh account carries no personal info.
    let me = send(&app, "GET", "/auth/me", Some(&bob), None).await;
    assert_eq!(me.status, StatusCode::OK);
    for field in ["name", "surname", "email", "phone", "birth_date"] {
        assert!(me.body[field].is_null(), "{field} should start null");
    }

    // Fill everything in via the self-service endpoint.
    let res = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&bob),
        Some(json!({
            "name": "Bob",
            "surname": "Gümüş",
            "email": "bob@example.com",
            "phone": "+90 555 123 45 67",
            "birth_date": "1990-1-2",
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["name"], "Bob");
    assert_eq!(res.body["surname"], "Gümüş"); // unicode names allowed
    assert_eq!(res.body["email"], "bob@example.com");
    assert_eq!(res.body["phone"], "+90 555 123 45 67");
    assert_eq!(res.body["birth_date"], "1990-01-02"); // canonicalized
    assert!(res.body.get("password_hash").is_none());

    // Partial patch: only phone changes, everything else survives.
    let res = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&bob),
        Some(json!({ "phone": "05551112233" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["phone"], "05551112233");
    assert_eq!(res.body["name"], "Bob");
    assert_eq!(res.body["email"], "bob@example.com");

    // Empty string clears a field; the others stay.
    let res = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&bob),
        Some(json!({ "phone": "" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body["phone"].is_null());
    assert_eq!(res.body["name"], "Bob");

    // The merged state is what /auth/me reports afterwards.
    let me = send(&app, "GET", "/auth/me", Some(&bob), None).await;
    assert_eq!(me.body["name"], "Bob");
    assert_eq!(me.body["surname"], "Gümüş");
    assert_eq!(me.body["birth_date"], "1990-01-02");
    assert!(me.body["phone"].is_null());

    // No session -> 401.
    assert_eq!(
        send(&app, "PATCH", "/users/me", None, Some(json!({"name":"x"})))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn profile_rejects_invalid_fields() {
    let app = mem_app().await;
    let bob = login(&app, "bob").await;

    let bad = [
        json!({ "email": "not-an-email" }),
        json!({ "email": "bob@nodot" }),
        json!({ "phone": "123" }),
        json!({ "phone": "letters" }),
        json!({ "birth_date": "02/01/1990" }),
        json!({ "birth_date": "1990-02-30" }),
        json!({ "birth_date": "9999-01-01" }),
        json!({ "name": "   " }),
        json!({ "name": "x".repeat(101) }),
    ];
    for body in bad {
        let res = send(&app, "PATCH", "/users/me", Some(&bob), Some(body.clone())).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "should reject {body}");
    }

    // A rejected patch must not have half-applied: profile is still empty.
    let me = send(&app, "GET", "/auth/me", Some(&bob), None).await;
    for field in ["name", "surname", "email", "phone", "birth_date"] {
        assert!(me.body[field].is_null(), "{field} should still be null");
    }
}

#[tokio::test]
async fn admin_reads_and_edits_any_profile_with_guards() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let alice = login(&app, "alice").await;
    let alice_id = id_of(&send(&app, "GET", "/auth/me", Some(&alice), None).await.body);
    let admin_id = id_of(&send(&app, "GET", "/auth/me", Some(&admin), None).await.body);

    // Students can neither look up nor edit someone else's record.
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/users/{admin_id}"),
            Some(&alice),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/users/{admin_id}/profile"),
            Some(&alice),
            Some(json!({"name":"Mallory"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // Admin maintains alice's record on her behalf.
    let res = send(
        &app,
        "PATCH",
        &format!("/users/{alice_id}/profile"),
        Some(&admin),
        Some(json!({ "name": "Alice", "email": "alice@school.edu" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["name"], "Alice");

    // Admin reads a single user; alice sees the change on /auth/me.
    let one = send(
        &app,
        "GET",
        &format!("/users/{alice_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(one.status, StatusCode::OK);
    assert_eq!(one.body["username"], "alice");
    assert_eq!(one.body["email"], "alice@school.edu");
    let me = send(&app, "GET", "/auth/me", Some(&alice), None).await;
    assert_eq!(me.body["name"], "Alice");

    // The admin listing now carries the info columns too.
    let list = send(&app, "GET", "/users", Some(&admin), None).await;
    let listed = common::items(&list.body)
        .iter()
        .find(|u| u["username"] == "alice")
        .expect("alice listed")
        .clone();
    assert_eq!(listed["email"], "alice@school.edu");

    // Unknown ids -> 404 on both admin endpoints.
    assert_eq!(
        send(&app, "GET", "/users/does-not-exist", Some(&admin), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            "PATCH",
            "/users/does-not-exist/profile",
            Some(&admin),
            Some(json!({"name":"Nobody"}))
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

// --- users: UI preferences (theme, language, palette color) ------------------------------

#[tokio::test]
async fn preferences_start_null_and_update_via_me_preferences() {
    let app = mem_app().await;
    let bob = login(&app, "bob").await;

    // A fresh account has never chosen — both come back null.
    let me = send(&app, "GET", "/auth/me", Some(&bob), None).await;
    assert_eq!(me.status, StatusCode::OK);
    assert!(me.body["theme"].is_null(), "theme should start null");
    assert!(me.body["language"].is_null(), "language should start null");
    assert!(
        me.body["palette_color"].is_null(),
        "palette_color should start null"
    );

    // Set all three through the self-service endpoint. Mixed-case hex is
    // accepted and comes back lowercase.
    let res = send(
        &app,
        "PATCH",
        "/users/me/preferences",
        Some(&bob),
        Some(json!({ "theme": "dark", "language": "tr", "palette_color": "#FEFAE0" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["theme"], "dark");
    assert_eq!(res.body["language"], "tr");
    assert_eq!(res.body["palette_color"], "#fefae0");

    // Partial patch: only the theme changes, the other two survive.
    let res = send(
        &app,
        "PATCH",
        "/users/me/preferences",
        Some(&bob),
        Some(json!({ "theme": "light" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["theme"], "light");
    assert_eq!(res.body["language"], "tr");
    assert_eq!(res.body["palette_color"], "#fefae0");

    // …and only the accent color changes, leaving theme and language alone.
    let res = send(
        &app,
        "PATCH",
        "/users/me/preferences",
        Some(&bob),
        Some(json!({ "palette_color": "#283618" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["palette_color"], "#283618");
    assert_eq!(res.body["theme"], "light");
    assert_eq!(res.body["language"], "tr");

    // Empty string clears back to "never chose"; the others stay.
    let res = send(
        &app,
        "PATCH",
        "/users/me/preferences",
        Some(&bob),
        Some(json!({ "theme": "" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body["theme"].is_null());
    assert_eq!(res.body["language"], "tr");
    assert_eq!(res.body["palette_color"], "#283618");

    let res = send(
        &app,
        "PATCH",
        "/users/me/preferences",
        Some(&bob),
        Some(json!({ "palette_color": "" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body["palette_color"].is_null());
    assert_eq!(res.body["language"], "tr");

    // The merged state is what /auth/me reports afterwards.
    let me = send(&app, "GET", "/auth/me", Some(&bob), None).await;
    assert!(me.body["theme"].is_null());
    assert!(me.body["palette_color"].is_null());
    assert_eq!(me.body["language"], "tr");

    // No session -> 401.
    assert_eq!(
        send(
            &app,
            "PATCH",
            "/users/me/preferences",
            None,
            Some(json!({"theme":"dark"}))
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn preferences_reject_invalid_values() {
    let app = mem_app().await;
    let bob = login(&app, "bob").await;

    let bad = [
        json!({ "theme": "solarized" }),
        json!({ "theme": "Dark" }),
        json!({ "language": "turkish" }),
        json!({ "language": "de" }),
        json!({ "theme": "dark", "language": "nope" }),
        json!({ "palette_color": "fefae0" }),   // no leading #
        json!({ "palette_color": "#fff" }),     // short form
        json!({ "palette_color": "#gggggg" }),  // not hex
        json!({ "palette_color": "#fefae0 " }), // untrimmed
        json!({ "palette_color": "rebeccapurple" }),
        // Mixed: a valid theme must not sneak through beside a bad color.
        json!({ "theme": "dark", "palette_color": "#12345" }),
    ];
    for body in bad {
        let res = send(
            &app,
            "PATCH",
            "/users/me/preferences",
            Some(&bob),
            Some(body.clone()),
        )
        .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "should reject {body}");
    }

    // A rejected patch must not have half-applied: both are still null.
    let me = send(&app, "GET", "/auth/me", Some(&bob), None).await;
    assert!(me.body["theme"].is_null(), "theme should still be null");
    assert!(
        me.body["language"].is_null(),
        "language should still be null"
    );
    assert!(
        me.body["palette_color"].is_null(),
        "palette_color should still be null"
    );
}

#[tokio::test]
async fn admin_edits_any_preferences_with_guards() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let alice = login(&app, "alice").await;
    let alice_id = id_of(&send(&app, "GET", "/auth/me", Some(&alice), None).await.body);
    let admin_id = id_of(&send(&app, "GET", "/auth/me", Some(&admin), None).await.body);

    // A student cannot touch someone else's preferences.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/users/{admin_id}/preferences"),
            Some(&alice),
            Some(json!({"theme":"dark"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // Admin sets alice's preferences on her behalf.
    let res = send(
        &app,
        "PATCH",
        &format!("/users/{alice_id}/preferences"),
        Some(&admin),
        Some(json!({ "theme": "dark", "language": "en", "palette_color": "#FEFAE0" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["theme"], "dark");
    assert_eq!(res.body["language"], "en");
    assert_eq!(
        res.body["palette_color"], "#fefae0",
        "normalized on the admin path too"
    );

    // The admin route patches partially and clears by `""` exactly like the
    // self-service one — one field at a time, the others untouched.
    let res = send(
        &app,
        "PATCH",
        &format!("/users/{alice_id}/preferences"),
        Some(&admin),
        Some(json!({ "palette_color": "#283618" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["palette_color"], "#283618");
    assert_eq!(res.body["theme"], "dark", "theme lost to the color patch");
    assert_eq!(res.body["language"], "en");

    let res = send(
        &app,
        "PATCH",
        &format!("/users/{alice_id}/preferences"),
        Some(&admin),
        Some(json!({ "palette_color": "" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body["palette_color"].is_null());
    assert_eq!(res.body["theme"], "dark", "theme lost to the clear");
    assert_eq!(res.body["language"], "en");

    // Back to a set value, so the reads below cover it too.
    let res = send(
        &app,
        "PATCH",
        &format!("/users/{alice_id}/preferences"),
        Some(&admin),
        Some(json!({ "palette_color": "#fefae0" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    // Admin reads them on the single-user lookup; alice sees them on /auth/me.
    let one = send(
        &app,
        "GET",
        &format!("/users/{alice_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(one.body["theme"], "dark");
    assert_eq!(one.body["language"], "en");
    assert_eq!(one.body["palette_color"], "#fefae0");
    let me = send(&app, "GET", "/auth/me", Some(&alice), None).await;
    assert_eq!(me.body["theme"], "dark");
    assert_eq!(me.body["language"], "en");
    assert_eq!(me.body["palette_color"], "#fefae0");

    // Unknown id -> 404.
    assert_eq!(
        send(
            &app,
            "PATCH",
            "/users/does-not-exist/preferences",
            Some(&admin),
            Some(json!({"theme":"dark"}))
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

// --- regression: race safety, session hygiene, CORS ----------------------

/// #1 — Many concurrent marks for the *same* (event, user). A find-then-insert
/// implementation races the unique index and 500s; the atomic composite-id
/// upsert must let every request succeed and leave exactly one row.
#[tokio::test]
async fn concurrent_attendance_marks_never_collide() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let student = login(&app, "student").await;
    let student_id = id_of(
        &send(&app, "GET", "/auth/me", Some(&student), None)
            .await
            .body,
    );
    let event_id = id_of(
        &send(
            &app,
            "POST",
            "/events",
            Some(&teacher),
            Some(json!({ "title": "e" })),
        )
        .await
        .body,
    );

    let mut handles = Vec::new();
    for i in 0..24 {
        let app = app.clone();
        let teacher = teacher.clone();
        let uri = format!("/events/{event_id}/attendance");
        let status = if i % 2 == 0 { "present" } else { "late" };
        let body = json!({ "status": status, "user_id": student_id });
        handles.push(tokio::spawn(async move {
            send(&app, "POST", &uri, Some(&teacher), Some(body))
                .await
                .status
        }));
    }
    for h in handles {
        assert_eq!(
            h.await.unwrap(),
            StatusCode::OK,
            "concurrent mark must not fail"
        );
    }

    let roster = send(
        &app,
        "GET",
        &format!("/events/{event_id}/attendance"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(
        common::items(&roster.body).len(),
        1,
        "upsert keeps a single row"
    );
}

/// #1 — Same guarantee for exam grading.
#[tokio::test]
async fn concurrent_exam_grades_never_collide() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let student = login(&app, "student").await;
    let student_id = me_id(&app, &student).await;
    let course_id = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course_id, &student_id).await;
    let exam_id = create_exam(&app, &teacher, &course_id, "e", "quiz").await;

    let mut handles = Vec::new();
    for mark in 0..24i64 {
        let app = app.clone();
        let teacher = teacher.clone();
        let uri = format!("/exams/{exam_id}/results");
        let body = json!({ "mark": mark, "user_id": student_id });
        handles.push(tokio::spawn(async move {
            send(&app, "POST", &uri, Some(&teacher), Some(body))
                .await
                .status
        }));
    }
    for h in handles {
        assert_eq!(
            h.await.unwrap(),
            StatusCode::OK,
            "concurrent grade must not fail"
        );
    }

    let results = send(
        &app,
        "GET",
        &format!("/exams/{exam_id}/results"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(
        common::items(&results.body).len(),
        1,
        "upsert keeps a single row"
    );
}

/// #2 — A successful login sweeps expired session rows but leaves live ones.
#[tokio::test]
async fn login_purges_expired_sessions() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await; // one live session

    // Inject a session that has already expired.
    db.query("CREATE session SET user = type::record('user', $u), token = 'expired-token', expires_at = 1")
        .bind(("u", "nobody".to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
    assert!(
        session::find_by_token(&db, "expired-token")
            .await
            .unwrap()
            .is_some()
    );

    // A later successful login sweeps expired rows...
    login(&app, "veli").await;
    assert!(
        session::find_by_token(&db, "expired-token")
            .await
            .unwrap()
            .is_none(),
        "expired session should be purged on login"
    );
    // ...while the still-valid session keeps working.
    assert_eq!(
        send(&app, "GET", "/auth/me", Some(&ali), None).await.status,
        StatusCode::OK
    );
}

/// An expired session row is rejected at auth even while it still exists —
/// expiry must not depend on the purge sweep having run.
#[tokio::test]
async fn expired_session_is_unauthorized_before_any_purge() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;

    // Copy ali's live session into a second row that expired long ago
    // (epoch millis 1), pointing at the same real user.
    db.query(
        "CREATE session SET \
            user = (SELECT VALUE user FROM ONLY session WHERE token = $live LIMIT 1), \
            token = 'stale-token', expires_at = 1",
    )
    .bind(("live", common::cookie_token(&ali).to_string()))
    .await
    .unwrap()
    .check()
    .unwrap();

    // Row exists, but the clock says no.
    assert!(
        session::find_by_token(&db, "stale-token")
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        send(&app, "GET", "/auth/me", Some("session=stale-token"), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

/// Regression: `Username::try_new` used to validate the trimmed value but store
/// the raw input, so `"ali"`, `"ali "` and `" ali"` were three distinct,
/// visually identical accounts — and a user who registered with padding could
/// only log in by reproducing it exactly. The name must be canonicalized
/// (trimmed) at construction and at login lookup.
#[tokio::test]
async fn padded_usernames_are_canonicalized_not_distinct_accounts() {
    let (app, db) = app_and_db().await;

    let res = send(
        &app,
        "POST",
        "/auth/register",
        None,
        Some(
            json!({ "school": "demo", "school": "demo", "username": "ali", "password": "secret1" }),
        ),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);

    // Padding must not mint a lookalike account. Register answers 201 either way
    // (username enumeration is denied, see tests/auth_enumeration.rs), so the
    // collision is observed through the account itself: each spoof asks for a
    // different password, and none of them may take effect.
    for spoof in ["ali ", " ali", "  ali  "] {
        let res = send(
            &app,
            "POST",
            "/auth/register",
            None,
            Some(json!({ "school": "demo", "username": spoof, "password": "spoof1" })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{spoof:?}");
    }

    // One row, not four — the padded forms collided with the canonical name.
    let rows: Vec<User> = db
        .query("SELECT * FROM user WHERE username = 'ali'")
        .await
        .unwrap()
        .check()
        .unwrap()
        .take(0)
        .unwrap();
    assert_eq!(rows.len(), 1, "padding minted a lookalike account");
    let all: Vec<User> = db
        .query("SELECT * FROM user")
        .await
        .unwrap()
        .check()
        .unwrap()
        .take(0)
        .unwrap();
    assert_eq!(all.len(), 1, "unexpected extra user rows");

    // That one account still belongs to the first registration, and every
    // spelling of the name resolves to it.
    for attempt in ["ali", "ali ", " ali", "  ali  "] {
        let res = send(
            &app,
            "POST",
            "/auth/login",
            None,
            Some(json!({ "school": "demo", "username": attempt, "password": "secret1" })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "login as {attempt:?}");
        let res = send(
            &app,
            "POST",
            "/auth/login",
            None,
            Some(json!({ "school": "demo", "username": attempt, "password": "spoof1" })),
        )
        .await;
        assert_eq!(
            res.status,
            StatusCode::UNAUTHORIZED,
            "a padded register overwrote \"ali\" (as {attempt:?})"
        );
    }

    // A name registered with padding is stored trimmed...
    let res = send(
        &app,
        "POST",
        "/auth/register",
        None,
        Some(json!({ "school": "demo", "username": " veli ", "password": "secret1" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["username"], "veli");

    // ...and the login lookup tolerates padding the same way registration does.
    for attempt in ["veli", " veli "] {
        let res = send(
            &app,
            "POST",
            "/auth/login",
            None,
            Some(json!({ "school": "demo", "username": attempt, "password": "secret1" })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "login as {attempt:?}");
    }
}

/// Regression: the grade endpoint verified the exam, the mark, and that the
/// target exists — but never that the grader isn't the target, so any teacher
/// could write their own mark. "Grading never targets oneself" (README). The
/// self-check precedes the target-role and enrollment checks, so it holds even
/// though staff — the only ones who can grade — are never enrolled or
/// gradeable themselves.
#[tokio::test]
async fn graders_cannot_grade_themselves() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let teacher_id = me_id(&app, &teacher).await;
    let boss_id = me_id(&app, &boss).await;
    let student = login(&app, "veli").await;
    let student_id = me_id(&app, &student).await;

    let course_id = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course_id, &student_id).await;
    let exam_id = create_exam(&app, &teacher, &course_id, "t", "quiz").await;

    // Self-grading is forbidden at every privilege level, not just for teachers.
    // (The teacher owns the course; the manager clears the gate by role.)
    for (cookie, own_id) in [(&teacher, &teacher_id), (&boss, &boss_id)] {
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam_id}/results"),
            Some(cookie),
            Some(json!({ "mark": 100, "user_id": own_id })),
        )
        .await;
        assert_eq!(
            res.status,
            StatusCode::FORBIDDEN,
            "self-grade must be rejected"
        );
    }

    // And the rejected attempts wrote nothing.
    assert_eq!(
        common::items(
            &send(
                &app,
                "GET",
                &format!("/exams/{exam_id}/results"),
                Some(&teacher),
                None
            )
            .await
            .body
        )
        .len(),
        0
    );

    // Grading someone *else* still works (boss grades the enrolled student).
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam_id}/results"),
        Some(&boss),
        Some(json!({ "mark": 90, "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
}

/// Enrollment is student membership: staff (teacher/manager/admin) can't be
/// enrolled, so they never gain the seat that gates sitting exams, being
/// graded, and the class roster. A plain student enrolls fine.
#[tokio::test]
async fn only_students_can_be_enrolled() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let colleague = login_as(&app, &db, "colleague", "teacher").await;
    let manager = login_as(&app, &db, "boss", "manager").await;
    let colleague_id = me_id(&app, &colleague).await;
    let manager_id = me_id(&app, &manager).await;

    let course = create_course(&app, &teacher, "algebra").await;

    // A fellow teacher and a manager are both refused — staff don't enroll.
    for staff_id in [&colleague_id, &manager_id] {
        let res = send(
            &app,
            "POST",
            &format!("/courses/{course}/enrollments"),
            Some(&teacher),
            Some(json!({ "user_id": staff_id })),
        )
        .await;
        assert_eq!(
            res.status,
            StatusCode::BAD_REQUEST,
            "enrolling staff must be rejected"
        );
    }

    // A student enrolls without complaint (the helper asserts 200).
    let student = login(&app, "veli").await;
    let student_id = me_id(&app, &student).await;
    enroll(&app, &teacher, &course, &student_id).await;
}

/// The student-only rule is enforced on the *live* role, not merely at enroll
/// time. This flips the role directly in the DB (bypassing the admin endpoint,
/// which also sweeps enrollments — and boot sweeps rows like this one), so the
/// enrollment row is still present — but inert: they can no longer sit the
/// exam, save answers, be graded, or be marked present. The action-time checks
/// alone hold, with no help from roster hygiene.
#[tokio::test]
async fn promotion_out_of_student_freezes_the_seat() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let student = login(&app, "veli").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let future = Timestamp::now().as_millis() + 3_600_000;
    let session = create_session(&app, &teacher, &course, future).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "essay", "kind": "quiz", "mode": "open" }),
    )
    .await;

    // While still a student, the seat works: they can start a sitting.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "student sits: {}",
        res.body
    );

    // Promote the enrolled student to teacher; the enrollment row survives.
    set_role(&db, "veli", "teacher").await;

    // Sitting is now refused — the live role, not the stale seat, decides.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "promoted user can't start a sitting"
    );

    // Nor can they save into the attempt they opened while still a student.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": "nope", "selected": "c0" })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "promoted user can't save answers"
    );

    // Grading the promoted user is refused — only students carry marks.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        Some(json!({ "mark": 80, "user_id": student_id })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::BAD_REQUEST,
        "promoted user can't be graded"
    );

    // Roll call is refused too — only students attend classes.
    let res = send(
        &app,
        "POST",
        &format!("/sessions/{session}/attendance"),
        Some(&teacher),
        Some(json!({ "status": "present", "user_id": student_id })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::BAD_REQUEST,
        "promoted user can't be rolled"
    );
}

/// Promotion through the admin role endpoint doesn't just freeze the seat —
/// it deletes the user's enrollment rows outright, so a promoted user drops
/// off course rosters and live-monitor counts instead of lingering.
#[tokio::test]
async fn promotion_via_role_endpoint_sweeps_enrollments() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let student = login(&app, "veli").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let roster = send(
        &app,
        "GET",
        &format!("/courses/{course}/enrollments"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::total(&roster.body), 1);

    let res = send(
        &app,
        "PATCH",
        &format!("/users/{student_id}/role"),
        Some(&admin),
        Some(json!({ "role": "teacher" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let roster = send(
        &app,
        "GET",
        &format!("/courses/{course}/enrollments"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(
        common::total(&roster.body),
        0,
        "promoted user left the roster: {}",
        roster.body
    );

    // Really deleted, not merely filtered out of the listing.
    let mut result = db
        .query("SELECT VALUE id FROM enrollment")
        .await
        .expect("count enrollments")
        .check()
        .expect("count enrollments check");
    let rows: Vec<surrealdb::types::RecordId> = result.take(0).expect("enrollment rows");
    assert!(rows.is_empty(), "enrollment rows deleted from the DB");
}

/// Regression: `User::create` pre-checks the username and then inserts, so two
/// concurrent registrations of the same name could both pass the check; the
/// loser then hit the unique index and surfaced as a raw 500. A lost race must
/// answer the same uniform 201 as the winner (the "taken" reply is deliberately
/// indistinguishable), exactly one account may exist afterwards, and the
/// winner's password must be the one that works.
#[tokio::test]
async fn concurrent_duplicate_registrations_conflict_not_500() {
    let (app, db) = app_and_db().await;

    let mut handles = Vec::new();
    for _ in 0..24 {
        let app = app.clone();
        handles.push(tokio::spawn(async move {
            send(
                &app,
                "POST",
                "/auth/register",
                None,
                Some(json!({ "school": "demo", "username": "dup", "password": "secret1" })),
            )
            .await
            .status
        }));
    }

    for h in handles {
        let status = h.await.unwrap();
        assert_eq!(
            status,
            StatusCode::CREATED,
            "a lost race must answer 201 like the winner, not a 500"
        );
    }

    // Exactly one row exists for the name.
    let users: Vec<hezarfen_backend::domain::user::User> = db
        .query("SELECT * FROM user WHERE username = 'dup'")
        .await
        .unwrap()
        .check()
        .unwrap()
        .take(0)
        .unwrap();
    assert_eq!(
        users.len(),
        1,
        "exactly one user row for the duplicated name"
    );

    // And the winner's credentials survived the pile-up: a further duplicate
    // asking for a different password does not replace them.
    let res = send(
        &app,
        "POST",
        "/auth/register",
        None,
        Some(json!({ "school": "demo", "username": "dup", "password": "hijack1" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let res = send(
        &app,
        "POST",
        "/auth/login",
        None,
        Some(json!({ "school": "demo", "username": "dup", "password": "secret1" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "the winning password broke");
    let res = send(
        &app,
        "POST",
        "/auth/login",
        None,
        Some(json!({ "school": "demo", "username": "dup", "password": "hijack1" })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::UNAUTHORIZED,
        "a duplicate register overwrote the account"
    );
}

/// Regression: listing a child collection of a missing parent used to return an
/// empty 200 list — indistinguishable from "exists but empty". A missing parent
/// is a 404, matching the write endpoints on the same paths.
#[tokio::test]
async fn child_lists_of_missing_parents_are_404() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;

    assert_eq!(
        send(&app, "GET", "/events/nope/attendance", Some(&teacher), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&app, "GET", "/exams/nope/results", Some(&teacher), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &app,
            "GET",
            "/courses/nope/enrollments",
            Some(&teacher),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&app, "GET", "/courses/nope/exams", Some(&teacher), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&app, "GET", "/exams/nope/statistics", Some(&teacher), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&app, "GET", "/marks/ghost", Some(&teacher), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

/// Regression: the session cookie never carried the `Secure` attribute, so a
/// TLS deployment would let browsers replay it over plain HTTP. The attribute
/// now follows `COOKIE_SECURE` (`Config::cookie_secure`); the default stays off
/// so plain-HTTP local dev keeps working.
#[tokio::test]
async fn session_cookie_secure_attribute_follows_config() {
    async fn login_set_cookie(app: &axum::Router) -> String {
        let creds = json!({ "school": "demo", "username": "ada", "password": "secret1" });
        assert_eq!(
            send(app, "POST", "/auth/register", None, Some(creds.clone()))
                .await
                .status,
            StatusCode::CREATED
        );
        let req = Request::builder()
            .method("POST")
            .uri("/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(creds.to_string()))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        res.headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    }

    // Default (dev): HttpOnly but not Secure.
    let plain = login_set_cookie(&mem_app().await).await;
    assert!(
        plain.contains("HttpOnly"),
        "cookie must stay HttpOnly: {plain}"
    );
    assert!(
        !plain.contains("Secure"),
        "dev cookie must not be Secure: {plain}"
    );
    // SameSite=Lax is the cookie-side half of the CORS defense: dev mirror
    // mode (see `cors_layer`) is only safe as long as this cookie never rides
    // cross-site requests.
    assert!(
        plain.contains("SameSite=Lax"),
        "cookie must stay SameSite=Lax: {plain}"
    );

    // With the flag on: Secure present.
    let (tenants, _db) = common::mem_deployment().await;
    let app = build_router(AppState {
        db: tenants.control().clone(),
        tenants,
        files_path: common::files_dir(),
        cookie_secure: true,
        rate_limit: hezarfen_backend::rate_limit::RateLimitConfig::unlimited(),
        chatbot_limit: Default::default(),
        exam_presence: Default::default(),
        board_hub: Default::default(),
        db_up: Default::default(),
        ai: None,
        metrics: hezarfen_backend::telemetry::Metrics::noop(),
    });
    let secure = login_set_cookie(&app).await;
    assert!(
        secure.contains("Secure"),
        "cookie must be Secure when configured: {secure}"
    );
    assert!(
        secure.contains("HttpOnly"),
        "Secure must not displace HttpOnly: {secure}"
    );
}

/// A query issued while the database socket is down does not fail — the SDK
/// parks it until the connection returns and *then* runs it, so a handler that
/// reaches the database mid-outage waits out the whole outage and any write it
/// carries lands late. The guard layer refuses at the edge instead, before the
/// request can touch the database, which is what makes the 503 safe to retry.
#[tokio::test]
async fn db_down_refuses_before_touching_the_database() {
    async fn count_rows(db: &hezarfen_backend::database::Database, table: &str) -> usize {
        let mut rows = db
            .query(format!("SELECT id FROM {table}"))
            .await
            .expect("count query");
        let ids: Vec<surrealdb::types::RecordId> = rows.take(0).expect("ids");
        ids.len()
    }

    let (tenants, db) = common::mem_deployment().await;
    let db_up = hezarfen_backend::state::DbHealth::default();
    let app = build_router(AppState {
        db: tenants.control().clone(),
        tenants,
        files_path: common::files_dir(),
        cookie_secure: false,
        rate_limit: hezarfen_backend::rate_limit::RateLimitConfig::unlimited(),
        chatbot_limit: Default::default(),
        exam_presence: Default::default(),
        board_hub: Default::default(),
        db_up: db_up.clone(),
        ai: None,
        metrics: hezarfen_backend::telemetry::Metrics::noop(),
    });

    // Healthy: a login against the seeded user reaches the handler as usual.
    let before: usize = count_rows(&db, "user").await;
    let ok = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/register")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"school": "demo", "username": "gulsah", "password": "sifre12345"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::CREATED);
    assert_eq!(
        count_rows(&db, "user").await,
        before + 1,
        "the write landed"
    );

    // Socket reported down: refused with a retryable 503, and — the point of
    // refusing at the edge rather than timing out — nothing was written.
    db_up.set(false);
    let refused = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/register")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"school": "demo", "username": "kerem", "password": "sifre12345"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        refused.headers().get("retry-after").unwrap(),
        "1",
        "a pre-execution refusal must advertise that retrying is safe"
    );
    assert_eq!(
        count_rows(&db, "user").await,
        before + 1,
        "a refused request must not have reached the database"
    );

    // Recovery flips back without a restart.
    db_up.set(true);
    let healed = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/register")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"school": "demo", "username": "kerem", "password": "sifre12345"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(healed.status(), StatusCode::CREATED);
    assert_eq!(count_rows(&db, "user").await, before + 2);
}

/// Regression: PATCH could set an event's times but never clear them — a JSON
/// `null` was indistinguishable from an omitted field and silently kept the old
/// value. An explicit `null` now clears the field; omitted still keeps it.
#[tokio::test]
async fn patch_null_clears_event_times() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let now = Timestamp::now().as_millis();
    let starts = now + 3_600_000;
    let ends = now + 7_200_000;
    let event_id = id_of(
        &send(
            &app,
            "POST",
            "/events",
            Some(&ali),
            Some(json!({"title":"mtg","starts_at": starts,"ends_at": ends})),
        )
        .await
        .body,
    );

    // Omitted fields still keep their values.
    let res = send(
        &app,
        "PATCH",
        &format!("/events/{event_id}"),
        Some(&ali),
        Some(json!({"title":"mtg2"})),
    )
    .await;
    assert_eq!(res.body["starts_at"], starts);
    assert_eq!(res.body["ends_at"], ends);

    // An explicit null clears just that field.
    let res = send(
        &app,
        "PATCH",
        &format!("/events/{event_id}"),
        Some(&ali),
        Some(json!({"ends_at": null})),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["starts_at"], starts);
    assert!(res.body["ends_at"].is_null());

    // With ends_at cleared, moving starts_at past the old end is legal...
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/events/{event_id}"),
            Some(&ali),
            Some(json!({"starts_at": now + 9_000_000}))
        )
        .await
        .status,
        StatusCode::OK
    );
    // ...and a new ends_at before starts_at is still rejected.
    assert_eq!(
        send(
            &app,
            "PATCH",
            &format!("/events/{event_id}"),
            Some(&ali),
            Some(json!({"ends_at": now + 1_000_000}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );

    // Clearing both works.
    let res = send(
        &app,
        "PATCH",
        &format!("/events/{event_id}"),
        Some(&ali),
        Some(json!({"starts_at": null, "ends_at": null})),
    )
    .await;
    assert!(res.body["starts_at"].is_null());
    assert!(res.body["ends_at"].is_null());
}

/// #4 — mirror mode (no `CORS_ALLOWED_ORIGINS`) reflects the caller's origin,
/// never `*`, but must NOT advertise credentials: reflecting arbitrary origins
/// with credentials would let any website ride a visitor's session cookie.
#[tokio::test]
async fn cors_mirror_mode_reflects_origin_without_credentials() {
    let app = mem_app().await;
    let req = Request::builder()
        .method("GET")
        .uri("/health")
        .header("origin", "https://evil.example")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    let headers = res.headers();
    assert_eq!(
        headers.get("access-control-allow-origin").unwrap(),
        "https://evil.example",
        "origin must be reflected, not wildcarded"
    );
    // tower-http omits the header entirely when credentials are off.
    assert!(
        headers.get("access-control-allow-credentials").is_none(),
        "mirror mode must never send allow-credentials"
    );
}

/// Allowlist mode: a listed origin gets both the origin echo and
/// `allow-credentials: true`; an unlisted origin gets no origin header at all.
/// `cors_layer` takes the allowlist as a parameter, so this needs no
/// process-global env mutation (which would race other tests).
#[tokio::test]
async fn cors_allowlist_mode_credits_only_listed_origins() {
    async fn probe(origin: &str) -> axum::http::HeaderMap {
        let app = axum::Router::new()
            .route("/ping", axum::routing::get(|| async { "pong" }))
            .layer(hezarfen_backend::cors_layer(vec![
                "https://app.example".parse().unwrap(),
            ]));
        let req = Request::builder()
            .method("GET")
            .uri("/ping")
            .header("origin", origin)
            .body(Body::empty())
            .unwrap();
        app.oneshot(req).await.unwrap().headers().clone()
    }

    let listed = probe("https://app.example").await;
    assert_eq!(
        listed.get("access-control-allow-origin").unwrap(),
        "https://app.example",
    );
    assert_eq!(
        listed.get("access-control-allow-credentials").unwrap(),
        "true",
        "the allowlisted origin must keep credentialed CORS"
    );

    let unlisted = probe("https://evil.example").await;
    assert!(
        unlisted.get("access-control-allow-origin").is_none(),
        "an unlisted origin must not be echoed"
    );
}

/// Both CORS modes share one layer base, so a single probe proves the exposed
/// headers: without them, cross-origin JS reads `null` for the documented
/// `Retry-After` (429s) and `Content-Disposition` (download filename) headers.
#[tokio::test]
async fn cors_exposes_retry_after_and_content_disposition() {
    let app = mem_app().await;
    let req = Request::builder()
        .method("GET")
        .uri("/health")
        .header("origin", "https://app.example")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(
        res.headers().get("access-control-expose-headers").unwrap(),
        "retry-after,content-disposition",
        "contract headers must be readable by cross-origin JS"
    );
}

// --- admin seed (ADMIN_USERNAME / ADMIN_PASSWORD bootstrap) ----------------

/// The seed creates a ready-to-use admin: login works with the configured
/// credentials and the account can reach admin-only endpoints immediately.
#[tokio::test]
async fn admin_seed_creates_working_admin() {
    let (app, db) = app_and_db().await;
    let username = Username::try_new("root").unwrap();
    let password = Password::try_new("secret1").unwrap();
    User::ensure_admin(username, password, &db).await.unwrap();

    let creds = json!({ "school": "demo", "username": "root", "password": "secret1" });
    let res = send(&app, "POST", "/auth/login", None, Some(creds)).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["role"], "admin");

    let cookie = res.cookie.expect("session cookie");
    let res = send(&app, "GET", "/users", Some(&cookie), None).await;
    assert_eq!(res.status, StatusCode::OK, "admin-only listing must open");
}

/// Re-running the seed (every boot does) must not duplicate the account,
/// demote it, or overwrite its password.
#[tokio::test]
async fn admin_seed_is_idempotent() {
    let (app, db) = app_and_db().await;
    for _ in 0..2 {
        let username = Username::try_new("root").unwrap();
        let password = Password::try_new("secret1").unwrap();
        User::ensure_admin(username, password, &db).await.unwrap();
    }

    let cookie = {
        let creds = json!({ "school": "demo", "username": "root", "password": "secret1" });
        let res = send(&app, "POST", "/auth/login", None, Some(creds)).await;
        assert_eq!(res.status, StatusCode::OK);
        res.cookie.expect("session cookie")
    };
    let res = send(&app, "GET", "/users", Some(&cookie), None).await;
    let listed = common::items(&res.body);
    assert_eq!(listed.len(), 1, "seed must not duplicate the account");
    assert_eq!(listed[0]["role"], "admin");
}

/// A username already registered by someone else is never promoted (or its
/// password replaced) by the seed — that would be privilege escalation.
#[tokio::test]
async fn admin_seed_refuses_existing_non_admin() {
    let (app, db) = app_and_db().await;
    let cookie = login(&app, "squatter").await; // registers with password `secret1`

    let username = Username::try_new("squatter").unwrap();
    let password = Password::try_new("attacker-pw").unwrap();
    User::ensure_admin(username, password, &db).await.unwrap();

    // Still a student: the admin-only listing stays closed...
    let res = send(&app, "GET", "/users", Some(&cookie), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "must not be promoted");

    // ...and the original password still logs in (nothing was overwritten).
    let creds = json!({ "school": "demo", "username": "squatter", "password": "secret1" });
    let res = send(&app, "POST", "/auth/login", None, Some(creds)).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["role"], "student");
}

// --- scheduled exams: sync/async attempts + the live monitor ---------------

/// Create a scheduled exam from a full body and return its id (asserts 201).
async fn scheduled_exam(
    app: &axum::Router,
    cookie: &str,
    course: &str,
    body: serde_json::Value,
) -> String {
    let res = create_exam_with(app, cookie, course, body).await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "create scheduled exam: {}",
        res.body
    );
    id_of(&res.body)
}

#[tokio::test]
async fn exam_scheduling_validates_and_echoes() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "sched_t", "teacher").await;
    let course = create_course(&app, &teacher, "algebra").await;
    let now = Timestamp::now().as_millis();

    // Sync: the window is echoed, no duration.
    let res = create_exam_with(
        &app,
        &teacher,
        &course,
        json!({
            "title": "midterm", "kind": "midterm",
            "mode": "sync", "starts_at": now + 60_000, "ends_at": now + 120_000,
        }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["mode"], "sync");
    assert_eq!(res.body["starts_at"].as_i64(), Some(now + 60_000));
    assert_eq!(res.body["ends_at"].as_i64(), Some(now + 120_000));
    assert!(res.body["duration_ms"].is_null());

    // Async: adds the per-student duration.
    let res = create_exam_with(
        &app,
        &teacher,
        &course,
        json!({
            "title": "takehome", "kind": "quiz",
            "mode": "async", "starts_at": now, "ends_at": now + 7_200_000,
            "duration_ms": 5_400_000,
        }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["mode"], "async");
    assert_eq!(res.body["duration_ms"].as_i64(), Some(5_400_000));

    // Async: a duration exactly as long as the window is fine — only
    // strictly exceeding it is rejected (see "duration exceeds window" below).
    scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({
            "title": "exact-fit", "kind": "quiz",
            "mode": "async", "starts_at": now, "ends_at": now + 3_600_000,
            "duration_ms": 3_600_000,
        }),
    )
    .await;

    // Unscheduled: every schedule field stays null (pre-schedule behavior),
    // and the attempt policy shows its defaults.
    let res = create_exam_with(
        &app,
        &teacher,
        &course,
        json!({ "title": "homework", "kind": "homework" }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert!(res.body["mode"].is_null());
    assert!(res.body["starts_at"].is_null());
    assert!(res.body["ends_at"].is_null());
    assert!(res.body["duration_ms"].is_null());
    assert_eq!(res.body["max_attempts"], 1);
    assert_eq!(res.body["allow_rejoin"], true);

    // Open: no window at all; an optional per-attempt duration is fine, and
    // the attempt policy echoes what was asked for.
    let res = create_exam_with(
        &app,
        &teacher,
        &course,
        json!({
            "title": "practice", "kind": "quiz", "mode": "open",
            "duration_ms": 5_400_000, "max_attempts": 0, "allow_rejoin": false,
        }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["mode"], "open");
    assert!(res.body["starts_at"].is_null());
    assert!(res.body["ends_at"].is_null());
    assert_eq!(res.body["duration_ms"].as_i64(), Some(5_400_000));
    assert_eq!(res.body["max_attempts"], 0);
    assert_eq!(res.body["allow_rejoin"], false);

    // Inconsistent schedules are rejected as a unit.
    for (label, body) in [
        (
            "unknown mode",
            json!({ "title": "x", "kind": "quiz", "mode": "weekly",
                    "starts_at": now, "ends_at": now + 1_000 }),
        ),
        (
            "times without mode",
            json!({ "title": "x", "kind": "quiz", "starts_at": now }),
        ),
        (
            "duration without mode",
            json!({ "title": "x", "kind": "quiz", "duration_ms": 60_000 }),
        ),
        (
            "sync missing ends_at",
            json!({ "title": "x", "kind": "quiz", "mode": "sync",
                    "starts_at": now }),
        ),
        (
            "sync with duration",
            json!({ "title": "x", "kind": "quiz", "mode": "sync",
                    "starts_at": now, "ends_at": now + 1_000, "duration_ms": 60_000 }),
        ),
        (
            "async without duration",
            json!({ "title": "x", "kind": "quiz", "mode": "async",
                    "starts_at": now, "ends_at": now + 1_000 }),
        ),
        (
            "backwards window",
            json!({ "title": "x", "kind": "quiz", "mode": "sync",
                    "starts_at": now + 2_000, "ends_at": now + 1_000 }),
        ),
        (
            "empty window",
            json!({ "title": "x", "kind": "quiz", "mode": "sync",
                    "starts_at": now, "ends_at": now }),
        ),
        (
            "duration too short",
            json!({ "title": "x", "kind": "quiz", "mode": "async",
                    "starts_at": now, "ends_at": now + 90_000_000, "duration_ms": 59_999 }),
        ),
        (
            "duration too long",
            json!({ "title": "x", "kind": "quiz", "mode": "async",
                    "starts_at": now, "ends_at": now + 90_000_000, "duration_ms": 86_400_001 }),
        ),
        (
            // Within the global 1min-24h bounds, but longer than its own
            // window: a 90-minute duration inside a 60-minute window.
            "duration exceeds window",
            json!({ "title": "x", "kind": "quiz", "mode": "async",
                    "starts_at": now, "ends_at": now + 3_600_000, "duration_ms": 5_400_000 }),
        ),
        (
            "past starts_at",
            json!({ "title": "x", "kind": "quiz", "mode": "sync",
                    "starts_at": now - 3_600_000, "ends_at": now + 3_600_000 }),
        ),
        (
            // starts_at is future so the rejection can only come from ends_at.
            "past ends_at",
            json!({ "title": "x", "kind": "quiz", "mode": "sync",
                    "starts_at": now + 3_600_000, "ends_at": now - 3_600_000 }),
        ),
        (
            "open with a window",
            json!({ "title": "x", "kind": "quiz", "mode": "open",
                    "starts_at": now + 60_000, "ends_at": now + 120_000 }),
        ),
        (
            "open with only an ends_at",
            json!({ "title": "x", "kind": "quiz", "mode": "open",
                    "ends_at": now + 120_000 }),
        ),
        (
            "negative attempt limit",
            json!({ "title": "x", "kind": "quiz", "max_attempts": -1 }),
        ),
        (
            "attempt limit over the cap",
            json!({ "title": "x", "kind": "quiz", "max_attempts": 101 }),
        ),
    ] {
        let res = create_exam_with(&app, &teacher, &course, body).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{label}: {}", res.body);
    }
}

/// Nothing can be scheduled to start (or end) in the past — on create and on
/// any PATCH that sets a time. Values a PATCH merely keeps are exempt, so a
/// running exam (its `starts_at` naturally behind the clock) stays editable.
#[tokio::test]
async fn schedule_times_cannot_be_backdated() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "past_t", "teacher").await;
    let course = create_course(&app, &teacher, "history").await;
    let now = Timestamp::now().as_millis();

    // A running exam: started seconds ago (inside the clock-skew grace).
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "running", "kind": "quiz",
                "mode": "sync", "starts_at": now - 1_000, "ends_at": now + 600_000 }),
    )
    .await;

    // Setting either time to the past is rejected...
    for body in [
        json!({ "starts_at": now - 3_600_000 }),
        json!({ "ends_at": now - 3_600_000 }),
    ] {
        let res = send(
            &app,
            "PATCH",
            &format!("/exams/{exam}"),
            Some(&teacher),
            Some(body),
        )
        .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    }

    // ...while edits that only keep the stored (already past) start work:
    // a rename, and a deadline extension.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "title": "renamed" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "ends_at": now + 1_200_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["ends_at"].as_i64(), Some(now + 1_200_000));

    // The grace is real: times well inside it (30s < 60s) are accepted, on
    // create and on PATCH alike.
    let res = create_exam_with(
        &app,
        &teacher,
        &course,
        json!({ "title": "graced", "kind": "quiz",
                "mode": "sync", "starts_at": now - 30_000, "ends_at": now + 600_000 }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let graced = id_of(&res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{graced}"),
        Some(&teacher),
        Some(json!({ "starts_at": now - 20_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

#[tokio::test]
async fn sync_attempt_lifecycle_feeds_the_live_monitor() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "live_t", "teacher").await;
    let ayse = login(&app, "ayse").await;
    let veli = login(&app, "veli").await;
    let ayse_id = me_id(&app, &ayse).await;
    let veli_id = me_id(&app, &veli).await;
    let course = create_course(&app, &teacher, "physics").await;
    enroll(&app, &teacher, &course, &ayse_id).await;
    enroll(&app, &teacher, &course, &veli_id).await;

    let now = Timestamp::now().as_millis();
    let ends = now + 600_000;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "final", "kind": "final",
                "mode": "sync", "starts_at": now - 1_000, "ends_at": ends }),
    )
    .await;

    // No attempt yet.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // Start: 201, in progress, and (sync) the deadline is the window close.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["status"], "in_progress");
    assert_eq!(res.body["user"]["username"], "veli");
    assert_eq!(res.body["deadline"].as_i64(), Some(ends));
    assert!(res.body["mark"].is_null());
    let started_at = res.body["started_at"].as_i64().expect("started_at");
    let remaining = res.body["remaining_ms"].as_i64().expect("remaining_ms");
    assert!(
        remaining > 0 && remaining <= 601_000,
        "remaining {remaining}"
    );

    // Re-"start" resumes the same attempt with the original clock: 200.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["started_at"].as_i64(), Some(started_at));

    // The monitor sees one writer and one absentee, sorted by username.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/live"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["counts"]["enrolled"], 2);
    assert_eq!(res.body["counts"]["in_progress"], 1);
    assert_eq!(res.body["counts"]["not_started"], 1);
    assert!(res.body["now"].as_i64().is_some());
    let students = res.body["students"].as_array().expect("students");
    assert_eq!(students[0]["user"]["username"], "ayse");
    assert_eq!(students[0]["status"], "not_started");
    assert!(students[0]["started_at"].is_null());
    assert!(students[0]["deadline"].is_null());
    assert_eq!(students[1]["user"]["username"], "veli");
    assert_eq!(students[1]["status"], "in_progress");
    assert!(students[1]["remaining_ms"].as_i64().expect("remaining") > 0);

    // Submit, then grade mid-session; both land in the next snapshot.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "submitted");
    assert!(res.body["finished_at"].as_i64().is_some());
    assert!(res.body["remaining_ms"].is_null());

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        Some(json!({ "mark": 85, "user_id": veli_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/live"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["counts"]["submitted"], 1);
    assert_eq!(res.body["counts"]["graded"], 1);
    assert_eq!(res.body["students"][1]["mark"], 85);

    // The student's own view carries the mark; the monitor stays teacher+.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.body["status"], "submitted");
    assert_eq!(res.body["mark"], 85);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/live"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
}

/// Once the window closes, an enrolled student who never sat turns from
/// `not_started` into `absent` on the monitor — started-but-unsubmitted stays
/// `expired`, and an open exam (no window) never flags anyone.
#[tokio::test]
async fn closed_window_flags_no_shows_as_absent() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "abs_t", "teacher").await;
    let ayse = login(&app, "ayse").await; // never shows up
    let veli = login(&app, "veli").await; // submits
    let cem = login(&app, "cem").await; // starts, never submits
    let ayse_id = me_id(&app, &ayse).await;
    let veli_id = me_id(&app, &veli).await;
    let cem_id = me_id(&app, &cem).await;
    let course = create_course(&app, &teacher, "history").await;
    enroll(&app, &teacher, &course, &ayse_id).await;
    enroll(&app, &teacher, &course, &veli_id).await;
    enroll(&app, &teacher, &course, &cem_id).await;

    let now = Timestamp::now().as_millis();
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "midterm", "kind": "midterm",
                "mode": "sync", "starts_at": now - 50_000, "ends_at": now + 600_000 }),
    )
    .await;

    for cookie in [&veli, &cem] {
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/attempt"),
            Some(cookie),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    }
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Window still open: the no-show is merely `not_started`.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/live"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["counts"]["not_started"], 1);
    assert_eq!(res.body["counts"]["absent"], 0);
    assert_eq!(res.body["students"][0]["user"]["username"], "ayse");
    assert_eq!(res.body["students"][0]["status"], "not_started");

    // The teacher cuts the window short (inside the backdating grace): the
    // exam is over, and the gap hardens into an absence.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "ends_at": now - 10_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/live"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["counts"]["enrolled"], 3);
    assert_eq!(res.body["counts"]["not_started"], 0);
    assert_eq!(res.body["counts"]["absent"], 1);
    assert_eq!(res.body["counts"]["submitted"], 1);
    assert_eq!(res.body["counts"]["expired"], 1);
    let students = res.body["students"].as_array().expect("students");
    assert_eq!(students[0]["user"]["username"], "ayse");
    assert_eq!(students[0]["status"], "absent");
    assert_eq!(students[1]["user"]["username"], "cem");
    assert_eq!(students[1]["status"], "expired");
    assert_eq!(students[2]["user"]["username"], "veli");
    assert_eq!(students[2]["status"], "submitted");

    // Absence is informational — the mark stays a human call, and the
    // teacher may still record one for the no-show.
    assert!(students[0]["mark"].is_null());
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        Some(json!({ "mark": 0, "user_id": ayse_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // An open exam has no window: never started is never absent.
    let open = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "essay", "kind": "homework", "mode": "open" }),
    )
    .await;
    let res = send(
        &app,
        "GET",
        &format!("/exams/{open}/live"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["counts"]["not_started"], 3);
    assert_eq!(res.body["counts"]["absent"], 0);
}

#[tokio::test]
async fn async_deadline_is_start_plus_duration_clamped_to_window() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "async_t", "teacher").await;
    let student = login(&app, "asli").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "chem").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let now = Timestamp::now().as_millis();

    // Roomy window: the personal duration is the binding constraint.
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "quiz", "kind": "quiz",
                "mode": "async", "starts_at": now - 1_000, "ends_at": now + 6_000_000,
                "duration_ms": 60_000 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let started = res.body["started_at"].as_i64().expect("started_at");
    assert_eq!(res.body["deadline"].as_i64(), Some(started + 60_000));

    // Tight window: starting partway through it still lets `ends_at` clamp
    // the personal deadline (duration_ms can be at most the full window now,
    // so the clamp only bites once a student starts late inside it).
    let starts = now - 40_000;
    let ends = now + 20_000;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "quiz2", "kind": "quiz",
                "mode": "async", "starts_at": starts, "ends_at": ends,
                "duration_ms": 60_000 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["deadline"].as_i64(), Some(ends));
}

#[tokio::test]
async fn attempts_gate_on_schedule_enrollment_and_window() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "gate_t", "teacher").await;
    let student = login(&app, "gita").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "bio").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let now = Timestamp::now().as_millis();

    // Unscheduled exams cannot be sat at all.
    let unscheduled = create_exam(&app, &teacher, &course, "hw", "homework").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{unscheduled}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // Not-yet-open and already-closed windows are conflicts too.
    let future = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "later", "kind": "quiz",
                "mode": "sync", "starts_at": now + 600_000, "ends_at": now + 1_200_000 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{future}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    let past = scheduled_exam(
        &app,
        &teacher,
        &course,
        // Inside the backdating grace, yet already closed by the clock.
        json!({ "title": "gone", "kind": "quiz",
                "mode": "sync", "starts_at": now - 50_000, "ends_at": now - 10_000 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{past}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // Enrollment is required — teachers included (the monitor is their view).
    let open = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "open", "kind": "quiz",
                "mode": "sync", "starts_at": now - 1_000, "ends_at": now + 600_000 }),
    )
    .await;
    let outsider = login(&app, "omer").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{open}/attempt"),
        Some(&outsider),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{open}/attempt"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // A missing exam is a 404, not a conflict.
    let res = send(
        &app,
        "POST",
        "/exams/does-not-exist/attempt",
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn finish_rejects_double_submit_missing_attempt_and_expiry() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "fin_t", "teacher").await;
    let student = login(&app, "fern").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "cs").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let now = Timestamp::now().as_millis();

    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "final", "kind": "final",
                "mode": "sync", "starts_at": now - 1_000, "ends_at": now + 600_000 }),
    )
    .await;

    // Finishing before starting: no attempt row to close.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "double submit");

    // Expiry: a window that closes right after the start. Real time, not a
    // mocked clock — the server judges expiry by `Timestamp::now()`.
    let brief = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "blitz", "kind": "quiz",
                "mode": "sync", "starts_at": now - 1_000,
                "ends_at": Timestamp::now().as_millis() + 1_500 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{brief}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["status"], "in_progress");

    tokio::time::sleep(std::time::Duration::from_millis(1_700)).await;

    let res = send(
        &app,
        "POST",
        &format!("/exams/{brief}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{brief}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.body["status"], "expired");
    assert!(res.body["finished_at"].is_null());
    assert!(res.body["remaining_ms"].is_null());
}

#[tokio::test]
async fn mode_freezes_after_attempts_but_times_extend_live() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ext_t", "teacher").await;
    let student = login(&app, "elif").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "math").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let now = Timestamp::now().as_millis();
    let ends = now + 60_000;

    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "final", "kind": "final",
                "mode": "sync", "starts_at": now - 1_000, "ends_at": ends }),
    )
    .await;

    // Before any attempt the whole schedule is editable — even the mode...
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "mode": "async", "duration_ms": 60_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["mode"], "async");

    // ...but a PATCH may not leave a half-schedule behind.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "mode": null })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // Clearing everything unschedules; restoring re-schedules.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "mode": null, "starts_at": null, "ends_at": null, "duration_ms": null })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body["mode"].is_null());
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "mode": "sync", "starts_at": now - 1_000, "ends_at": ends })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // A student sits down; the mode is now frozen (unscheduling included).
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "mode": "async", "duration_ms": 60_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "mode": null, "starts_at": null, "ends_at": null })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // ...but extending the window moves the running deadline immediately.
    let extended = ends + 300_000;
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "ends_at": extended })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.body["status"], "in_progress");
    assert_eq!(res.body["deadline"].as_i64(), Some(extended));
}

#[tokio::test]
async fn attempts_cascade_with_exam_and_course_deletion() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "casc_t", "teacher").await;
    let student = login(&app, "cansu").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "hist").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let now = Timestamp::now().as_millis();
    let schedule = json!({ "title": "final", "kind": "final",
                           "mode": "sync", "starts_at": now - 1_000, "ends_at": now + 600_000 });

    // Deleting the exam removes its attempts.
    let exam = scheduled_exam(&app, &teacher, &course, schedule.clone()).await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert!(any_for_exam(&db, &ExamId::from_key(&exam)).await.unwrap());
    let res = send(
        &app,
        "DELETE",
        &format!("/exams/{exam}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert!(!any_for_exam(&db, &ExamId::from_key(&exam)).await.unwrap());

    // Deleting the whole course cascades through its exams' attempts too.
    let exam = scheduled_exam(&app, &teacher, &course, schedule).await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    unenroll(&app, &teacher, &course, &student_id).await;
    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{course}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert!(!any_for_exam(&db, &ExamId::from_key(&exam)).await.unwrap());
}

/// An `open` exam is sittable anytime — no window, no deadline — and
/// `max_attempts` meters the retakes: every terminal attempt can be followed
/// by a fresh sitting (blank answer sheet) until the limit is spent, and the
/// limit is live-editable, `0` meaning unlimited.
#[tokio::test]
async fn open_exams_sit_anytime_and_retakes_respect_the_limit() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "open_t", "teacher").await;
    let student = login(&app, "omer").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "practice").await;
    let subject = create_subject(&app, &teacher, &course, "drills").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "drill", "kind": "quiz",
                "mode": "open", "max_attempts": 2 }),
    )
    .await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}], "correct": "c1" }),
    )
    .await;
    let question = id_of(&question_body);

    // First sitting starts right away — no window to wait for — and runs
    // without a deadline: it can only end by submission.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["attempt"], 1);
    assert_eq!(res.body["attempts_used"], 1);
    assert_eq!(res.body["max_attempts"], 2);
    assert_eq!(res.body["status"], "in_progress");
    assert!(res.body["deadline"].is_null());
    assert!(res.body["remaining_ms"].is_null());

    // Answer, then re-"start": the running attempt resumes (200), same clock.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": question, "selected": choice_of(&question_body, 1) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["attempt"], 1);
    assert_eq!(res.body["answered"], 1);

    // Submit, then start again: sitting #2, and the retake wiped the sheet.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["attempt"], 2);
    assert_eq!(res.body["attempts_used"], 2);
    assert_eq!(res.body["answered"], 0, "retake starts from a blank sheet");
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt/questions"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(
        res.body[0]["answer"].is_null(),
        "wiped answer resurfaced: {}",
        res.body
    );

    // The limit is spent after sitting #2 ends: no third start.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // …until the teacher raises it live. `0` = unlimited: sitting #3 opens.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "max_attempts": 0 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["max_attempts"], 0);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["attempt"], 3);
    assert_eq!(res.body["max_attempts"], 0);

    // Lowering the limit below what's used never kills the running attempt —
    // it only blocks the next start once this one is over.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "max_attempts": 1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["status"], "in_progress");
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
}

/// An `open` exam with a `duration_ms` gives each sitting its own countdown
/// (`started_at + duration`), and retakes inside a sync window restart the
/// sheet while the monitor tracks the latest sitting.
#[tokio::test]
async fn open_duration_and_sync_retakes_shape_the_deadline_and_monitor() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "dur_t", "teacher").await;
    let student = login(&app, "duru").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "timing").await;
    enroll(&app, &teacher, &course, &student_id).await;

    // Open + duration: the deadline is exactly start + budget.
    let timed = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "sprint", "kind": "quiz",
                "mode": "open", "duration_ms": 60_000 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{timed}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let started_at = res.body["started_at"].as_i64().expect("started_at");
    assert_eq!(res.body["deadline"].as_i64(), Some(started_at + 60_000));
    let remaining = res.body["remaining_ms"].as_i64().expect("remaining_ms");
    assert!(
        remaining > 0 && remaining <= 60_000,
        "remaining {remaining}"
    );

    // Sync exam with retakes: sitting #2 opens inside the window and the
    // monitor shows the latest sitting plus the usage count.
    let now = Timestamp::now().as_millis();
    let ends = now + 600_000;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "final", "kind": "final",
                "mode": "sync", "starts_at": now - 1_000, "ends_at": ends,
                "max_attempts": 3 }),
    )
    .await;
    for _ in 0..2 {
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/attempt"),
            Some(&student),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
        assert_eq!(res.body["deadline"].as_i64(), Some(ends));
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/attempt/finish"),
            Some(&student),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    }
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/live"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["counts"]["submitted"], 1);
    let row = &res.body["students"][0];
    assert_eq!(row["user"]["username"], "duru");
    assert_eq!(row["attempt"], 2);
    assert_eq!(row["attempts_used"], 2);
    assert!(row["left_at"].is_null());

    // The third sitting is the last: the window still gates retakes, so once
    // it shuts (or the limit is spent) starting conflicts.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["attempt"], 3);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
}

// --- subjects ---------------------------------------------------------------

#[tokio::test]
async fn subject_crud_validation_and_rbac() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "sub_t", "teacher").await;
    let other_teacher = login_as(&app, &db, "sub_t2", "teacher").await;
    let boss = login_as(&app, &db, "sub_m", "manager").await;
    let student = login(&app, "selin").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "calculus").await;
    enroll(&app, &teacher, &course, &student_id).await;

    // Create echoes the full shape; description defaults to empty.
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/subjects"),
        Some(&teacher),
        Some(json!({ "name": "limits", "description": "epsilon-delta" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["course"], course);
    assert_eq!(res.body["name"], "limits");
    assert_eq!(res.body["description"], "epsilon-delta");
    let subject = id_of(&res.body);
    let second = create_subject(&app, &teacher, &course, "derivatives").await;

    // Validation: a blank name is a 400, a missing course a 404.
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/subjects"),
        Some(&teacher),
        Some(json!({ "name": "  " })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = send(
        &app,
        "POST",
        "/courses/missing/subjects",
        Some(&teacher),
        Some(json!({ "name": "orphan" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // Writes are management-gated: students lack the role, a rival teacher
    // the rights; a manager passes everywhere.
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/subjects"),
        Some(&student),
        Some(json!({ "name": "nope" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/subjects"),
        Some(&other_teacher),
        Some(json!({ "name": "nope" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "PATCH",
        &format!("/subjects/{subject}"),
        Some(&other_teacher),
        Some(json!({ "name": "hijack" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "DELETE",
        &format!("/subjects/{second}"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // Reads follow course visibility: the enrolled student and a manager see
    // the list and the subject, the rival teacher sees neither.
    let res = send(
        &app,
        "GET",
        &format!("/courses/{course}/subjects"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let listed = common::items(&res.body);
    assert_eq!(listed.len(), 2, "creation order");
    assert_eq!(id_of(&listed[0]), subject);
    assert_eq!(id_of(&listed[1]), second);
    assert_eq!(common::total(&res.body), 2);
    let res = send(
        &app,
        "GET",
        &format!("/subjects/{subject}"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["name"], "limits");
    let res = send(
        &app,
        "GET",
        &format!("/courses/{course}/subjects"),
        Some(&other_teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "GET",
        &format!("/subjects/{subject}"),
        Some(&other_teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // PATCH keeps omitted fields; a manager may edit anyone's.
    let res = send(
        &app,
        "PATCH",
        &format!("/subjects/{subject}"),
        Some(&boss),
        Some(json!({ "name": "limits & continuity" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["name"], "limits & continuity");
    assert_eq!(res.body["description"], "epsilon-delta", "kept by omission");

    // Unknown ids are 404s.
    for (method, body) in [
        ("GET", None),
        ("PATCH", Some(json!({ "name": "x" }))),
        ("DELETE", None),
    ] {
        let res = send(&app, method, "/subjects/missing", Some(&teacher), body).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{method}");
    }

    // Delete works while nothing references the subject.
    let res = send(
        &app,
        "DELETE",
        &format!("/subjects/{second}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "GET",
        &format!("/subjects/{second}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn subject_delete_blocks_while_questions_reference_it() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "sdb_t", "teacher").await;
    let course = create_course(&app, &teacher, "history").await;
    let tagged = create_subject(&app, &teacher, &course, "antiquity").await;
    let spare = create_subject(&app, &teacher, &course, "middle ages").await;
    let exam = create_exam(&app, &teacher, &course, "final", "final").await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &tagged,
        json!({ "text": "When?", "kind": "text", "points": 10 }),
    )
    .await;
    let question = id_of(&question_body);

    // Referenced: the delete is a conflict, and the subject survives.
    let res = send(
        &app,
        "DELETE",
        &format!("/subjects/{tagged}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/subjects/{tagged}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    // Re-tag the question elsewhere: the old subject frees up, the new one
    // locks.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{question}"),
        Some(&teacher),
        Some(json!({ "subject_id": spare })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/subjects/{tagged}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "DELETE",
        &format!("/subjects/{spare}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT);

    // Deleting the question releases the last reference.
    let res = send(
        &app,
        "DELETE",
        &format!("/exams/{exam}/questions/{question}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "DELETE",
        &format!("/subjects/{spare}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
}

// --- exam questions + answers (taking an exam) -----------------------------

/// Create a question tagged with `subject` via `POST /exams/{exam}/questions`
/// (asserts 201); returns its id.
async fn create_question(
    app: &axum::Router,
    cookie: &str,
    exam: &str,
    subject: &str,
    body: serde_json::Value,
) -> String {
    id_of(&create_question_body(app, cookie, exam, subject, body).await)
}

/// Like [`create_question`], but hands back the whole response: choice ids are
/// minted by the server, so a test that answers a question — or pictures one of
/// its options — has to read them off the response rather than count positions.
async fn create_question_body(
    app: &axum::Router,
    cookie: &str,
    exam: &str,
    subject: &str,
    mut body: serde_json::Value,
) -> serde_json::Value {
    body["subject_id"] = json!(subject);
    let res = send(
        app,
        "POST",
        &format!("/exams/{exam}/questions"),
        Some(cookie),
        Some(body),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "create question: {}",
        res.body
    );
    res.body
}

/// The minted id of the option at `index` of a question (or bank template)
/// response — what these tests used to write as a bare choice index.
fn choice_of(question: &serde_json::Value, index: usize) -> String {
    question["choices"][index]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("choice {index} of {question}"))
        .to_string()
}

/// A course with an open sync window, one subject, and one enrolled student —
/// the spine of the question/answer tests. Returns (course, exam, subject).
async fn open_exam_with_student(
    app: &axum::Router,
    teacher: &str,
    student_id: &str,
    course_title: &str,
) -> (String, String, String) {
    let course = create_course(app, teacher, course_title).await;
    let subject = create_subject(app, teacher, &course, "general").await;
    enroll(app, teacher, &course, student_id).await;
    let now = Timestamp::now().as_millis();
    let exam = scheduled_exam(
        app,
        teacher,
        &course,
        json!({ "title": "final", "kind": "final",
                "mode": "sync", "starts_at": now - 1_000, "ends_at": now + 600_000 }),
    )
    .await;
    (course, exam, subject)
}

#[tokio::test]
async fn question_crud_validation_and_rbac() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "q_t", "teacher").await;
    let other_teacher = login_as(&app, &db, "q_t2", "teacher").await;
    let student = login(&app, "quinn").await;
    let course = create_course(&app, &teacher, "logic").await;
    let subject = create_subject(&app, &teacher, &course, "propositions").await;
    let exam = create_exam(&app, &teacher, &course, "final", "final").await;

    // A choice question echoes its full authoring view, correct included.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        Some(
            json!({ "subject_id": subject, "text": "2 + 2?", "kind": "choice", "points": 10,
                     "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}, {"id": "c2", "text": "5"}], "correct": "c1" }),
        ),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["exam"], exam);
    assert_eq!(res.body["subject"], subject);
    assert_eq!(res.body["kind"], "choice");
    assert_eq!(res.body["points"], 10);
    assert_eq!(res.body["choices"][1]["text"], "4");
    assert_eq!(res.body["correct"], res.body["choices"][1]["id"]);
    let choice_q = id_of(&res.body);

    // A text question carries neither choices nor correct.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        Some(json!({ "subject_id": subject, "text": "Explain.", "kind": "text", "points": 20 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert!(res.body["choices"].is_null());
    assert!(res.body["correct"].is_null());
    let text_q = id_of(&res.body);

    // The teacher list shows both, in creation order.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let questions = common::items(&res.body);
    assert_eq!(questions.len(), 2);
    assert_eq!(id_of(&questions[0]), choice_q);
    assert_eq!(id_of(&questions[1]), text_q);

    // Kind-dependent fields are validated as a unit.
    for (label, body) in [
        (
            "unknown kind",
            json!({ "text": "x", "kind": "essay", "points": 1 }),
        ),
        (
            "zero points",
            json!({ "text": "x", "kind": "text", "points": 0 }),
        ),
        (
            "oversized points",
            json!({ "text": "x", "kind": "text", "points": 101 }),
        ),
        (
            "blank text",
            json!({ "text": "  ", "kind": "text", "points": 1 }),
        ),
        (
            "choice without choices",
            json!({ "text": "x", "kind": "choice", "points": 1, "correct": "c0" }),
        ),
        (
            "choice without correct",
            json!({ "text": "x", "kind": "choice", "points": 1, "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}] }),
        ),
        (
            "correct names no submitted choice",
            json!({ "text": "x", "kind": "choice", "points": 1, "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}], "correct": "c2" }),
        ),
        (
            "correct is blank",
            json!({ "text": "x", "kind": "choice", "points": 1, "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}], "correct": "" }),
        ),
        (
            // Two options under one key: the payload can't say which one
            // `correct` (or a picture) means, so it is refused outright.
            "duplicate choice id in one payload",
            json!({ "text": "x", "kind": "choice", "points": 1, "choices": [{"id": "c0", "text": "a"}, {"id": "c0", "text": "b"}], "correct": "c0" }),
        ),
        (
            // An option with no key cannot be named by `correct`.
            "correct names an unkeyed option",
            json!({ "text": "x", "kind": "choice", "points": 1, "choices": [{"text": "a"}, {"text": "b"}], "correct": "c0" }),
        ),
        (
            "one choice",
            json!({ "text": "x", "kind": "choice", "points": 1, "choices": [{"id": "c0", "text": "a"}], "correct": "c0" }),
        ),
        (
            "eleven choices",
            json!({ "text": "x", "kind": "choice", "points": 1,
                                   "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}, {"id": "c2", "text": "c"}, {"id": "c3", "text": "d"}, {"id": "c4", "text": "e"}, {"id": "c5", "text": "f"}, {"id": "c6", "text": "g"}, {"id": "c7", "text": "h"}, {"id": "c8", "text": "i"}, {"id": "c9", "text": "j"}, {"id": "c10", "text": "k"}], "correct": "c0" }),
        ),
        (
            "blank choice",
            json!({ "text": "x", "kind": "choice", "points": 1, "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": " "}], "correct": "c0" }),
        ),
        (
            "text with choices",
            json!({ "text": "x", "kind": "text", "points": 1, "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}] }),
        ),
        (
            "text with correct",
            json!({ "text": "x", "kind": "text", "points": 1, "correct": "c0" }),
        ),
    ] {
        let mut body = body;
        body["subject_id"] = json!(subject);
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/questions"),
            Some(&teacher),
            Some(body),
        )
        .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{label}: {}", res.body);
    }

    // The subject must exist and belong to the exam's own course.
    let other_course = create_course(&app, &teacher, "rhetoric").await;
    let foreign_subject = create_subject(&app, &teacher, &other_course, "fallacies").await;
    for (label, subject_id) in [
        ("unknown subject", "missing"),
        ("foreign subject", foreign_subject.as_str()),
    ] {
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/questions"),
            Some(&teacher),
            Some(json!({ "subject_id": subject_id, "text": "x", "kind": "text", "points": 1 })),
        )
        .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{label}: {}", res.body);
    }

    // Authoring needs management rights over the course; reading needs teacher+.
    let question_body = json!({ "subject_id": subject, "text": "x", "kind": "text", "points": 1 });
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions"),
        Some(&student),
        Some(question_body.clone()),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions"),
        Some(&other_teacher),
        Some(question_body),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "not the course creator");
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "correct stays teacher-side"
    );

    // PATCH: keep-by-omission, and the kind bundle re-validates as a unit.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{choice_q}"),
        Some(&teacher),
        Some(json!({ "points": 15 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["points"], 15);
    assert_eq!(
        res.body["correct"], res.body["choices"][1]["id"],
        "kind bundle kept"
    );
    assert_eq!(res.body["subject"], subject, "subject kept by omission");

    // Re-tagging stays inside the course: another of its subjects is fine, a
    // foreign course's subject is a 400.
    let second_subject = create_subject(&app, &teacher, &course, "predicates").await;
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{choice_q}"),
        Some(&teacher),
        Some(json!({ "subject_id": second_subject })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["subject"], second_subject);
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{choice_q}"),
        Some(&teacher),
        Some(json!({ "subject_id": foreign_subject })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // Switching to text must clear the bundle explicitly...
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{choice_q}"),
        Some(&teacher),
        Some(json!({ "kind": "text" })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::BAD_REQUEST,
        "half-switch: {}",
        res.body
    );
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{choice_q}"),
        Some(&teacher),
        Some(json!({ "kind": "text", "choices": null, "correct": null })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body["choices"].is_null());

    // ...and back, bringing the bundle along.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{choice_q}"),
        Some(&teacher),
        Some(json!({ "kind": "choice", "choices": [{"id": "c0", "text": "yes"}, {"id": "c1", "text": "no"}], "correct": "c0" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Editing is management-gated too.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{choice_q}"),
        Some(&other_teacher),
        Some(json!({ "points": 1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // Unknown ids are 404s — including a question under the wrong exam.
    let other_exam = create_exam(&app, &teacher, &course, "quiz", "quiz").await;
    for (method, uri) in [
        ("POST", "/exams/missing/questions".to_string()),
        ("GET", "/exams/missing/questions".to_string()),
        ("PATCH", format!("/exams/{exam}/questions/missing")),
        ("DELETE", format!("/exams/{exam}/questions/missing")),
        ("PATCH", format!("/exams/{other_exam}/questions/{choice_q}")),
        (
            "DELETE",
            format!("/exams/{other_exam}/questions/{choice_q}"),
        ),
    ] {
        let body = match method {
            "POST" => {
                Some(json!({ "subject_id": subject, "text": "x", "kind": "text", "points": 1 }))
            }
            "PATCH" => Some(json!({ "points": 2 })),
            _ => None,
        };
        let res = send(&app, method, &uri, Some(&teacher), body).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{method} {uri}");
    }

    // Delete removes from the list; deleting is management-gated.
    let res = send(
        &app,
        "DELETE",
        &format!("/exams/{exam}/questions/{text_q}"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "DELETE",
        &format!("/exams/{exam}/questions/{text_q}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::items(&res.body).len(), 1);
}

#[tokio::test]
async fn questions_freeze_once_attempts_start() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "frz_t", "teacher").await;
    let student = login(&app, "firat").await;
    let student_id = me_id(&app, &student).await;
    let (_course, exam, subject) =
        open_exam_with_student(&app, &teacher, &student_id, "algo").await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}], "correct": "c1" }),
    )
    .await;
    let question = id_of(&question_body);

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);

    // Create, edit, and delete are all conflicts now.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        Some(json!({ "subject_id": subject, "text": "late", "kind": "text", "points": 1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{question}"),
        Some(&teacher),
        Some(json!({ "points": 1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/exams/{exam}/questions/{question}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
}

#[tokio::test]
async fn student_question_view_hides_correct_and_embeds_answers() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "sv_t", "teacher").await;
    let student = login(&app, "sona").await;
    let student_id = me_id(&app, &student).await;
    let (_course, exam, subject) = open_exam_with_student(&app, &teacher, &student_id, "art").await;
    let choice_q_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}], "correct": "c1" }),
    )
    .await;
    let choice_q = id_of(&choice_q_body);
    create_question(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Explain.", "kind": "text", "points": 20 }),
    )
    .await;

    // No early peek: the student view requires an attempt.
    let uri = format!("/exams/{exam}/attempt/questions");
    let res = send(&app, "GET", &uri, Some(&student), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "attempt first");
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);

    // The sitting view: same order, no `correct` key anywhere, answers null.
    let res = send(&app, "GET", &uri, Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let questions = res.body.as_array().expect("questions");
    assert_eq!(questions.len(), 2);
    assert_eq!(questions[0]["kind"], "choice");
    assert_eq!(questions[0]["choices"][1]["text"], "4");
    for question in questions {
        assert!(
            question.as_object().unwrap().get("correct").is_none(),
            "correct must never reach a student: {question}"
        );
        assert!(question["answer"].is_null());
    }

    // A saved answer comes back embedded on the next read.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": choice_q, "selected": choice_of(&choice_q_body, 0) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["question"], choice_q);
    assert_eq!(res.body["selected"], choice_of(&choice_q_body, 0));
    let saved_at = res.body["updated_at"].as_i64().expect("updated_at");

    let res = send(&app, "GET", &uri, Some(&student), None).await;
    let questions = res.body.as_array().expect("questions");
    assert_eq!(
        questions[0]["answer"]["selected"],
        choice_of(&choice_q_body, 0)
    );
    assert_eq!(questions[0]["answer"]["updated_at"], saved_at);
    assert!(questions[1]["answer"].is_null());
}

#[tokio::test]
async fn answer_saves_gate_on_attempt_state_and_kind() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ans_t", "teacher").await;
    let student = login(&app, "arda").await;
    let student_id = me_id(&app, &student).await;
    let (course, exam, subject) = open_exam_with_student(&app, &teacher, &student_id, "phys").await;
    let choice_q_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}, {"id": "c2", "text": "5"}], "correct": "c1" }),
    )
    .await;
    let choice_q = id_of(&choice_q_body);
    let text_q_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Explain.", "kind": "text", "points": 20 }),
    )
    .await;
    let text_q = id_of(&text_q_body);
    let answers_uri = format!("/exams/{exam}/attempt/answers");

    // Saving needs an attempt (404 before start).
    let res = send(
        &app,
        "POST",
        &answers_uri,
        Some(&student),
        Some(json!({ "question_id": choice_q, "selected": choice_of(&choice_q_body, 0) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "attempt first");
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);

    // Upsert: re-answering overwrites the one row and re-stamps its clock.
    let res = send(
        &app,
        "POST",
        &answers_uri,
        Some(&student),
        Some(json!({ "question_id": choice_q, "selected": choice_of(&choice_q_body, 0) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let first_saved_at = res.body["updated_at"].as_i64().expect("updated_at");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let res = send(
        &app,
        "POST",
        &answers_uri,
        Some(&student),
        Some(json!({ "question_id": choice_q, "selected": choice_of(&choice_q_body, 2) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["selected"], choice_of(&choice_q_body, 2));
    assert!(
        res.body["updated_at"].as_i64().expect("updated_at") > first_saved_at,
        "a re-save moves updated_at forward"
    );
    let res = send(
        &app,
        "POST",
        &answers_uri,
        Some(&student),
        Some(json!({ "question_id": text_q, "text": "because" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["text"], "because");

    // The attempt view counts distinct questions answered, not saves.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.body["answered"], 2);
    assert_eq!(res.body["question_count"], 2);

    // Payloads that don't fit the question's kind are 400s.
    for (label, body) in [
        (
            "text on a choice question",
            json!({ "question_id": choice_q, "text": "4" }),
        ),
        (
            "both fields",
            json!({ "question_id": choice_q, "selected": choice_of(&choice_q_body, 1), "text": "4" }),
        ),
        (
            "selected names no choice of this question",
            json!({ "question_id": choice_q, "selected": "01NOTACHOICEOFTHISQUEST" }),
        ),
        ("nothing to save", json!({ "question_id": choice_q })),
        (
            "selected on a text question",
            json!({ "question_id": text_q, "selected": choice_of(&choice_q_body, 0) }),
        ),
        (
            "oversized text",
            json!({ "question_id": text_q, "text": "x".repeat(10_001) }),
        ),
    ] {
        let res = send(&app, "POST", &answers_uri, Some(&student), Some(body)).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{label}: {}", res.body);
    }

    // Unknown questions — and questions of *other* exams — are 404s.
    let res = send(
        &app,
        "POST",
        &answers_uri,
        Some(&student),
        Some(json!({ "question_id": "missing", "selected": "c0" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let foreign_exam = create_exam(&app, &teacher, &course, "other", "quiz").await;
    let foreign_q_body = create_question_body(
        &app,
        &teacher,
        &foreign_exam,
        &subject,
        json!({ "text": "not yours", "kind": "choice", "points": 1,
                "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}], "correct": "c0" }),
    )
    .await;
    let foreign_q = id_of(&foreign_q_body);
    let res = send(
        &app,
        "POST",
        &answers_uri,
        Some(&student),
        Some(json!({ "question_id": foreign_q, "selected": choice_of(&foreign_q_body, 0) })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::NOT_FOUND,
        "a real question under the wrong exam: {}",
        res.body
    );

    // Submitting closes the write path.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(
        &app,
        "POST",
        &answers_uri,
        Some(&student),
        Some(json!({ "question_id": choice_q, "selected": choice_of(&choice_q_body, 1) })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CONFLICT,
        "after submit: {}",
        res.body
    );

    // ...but the sitting view stays readable for review, answers intact.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt/questions"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::OK,
        "review after submit: {}",
        res.body
    );
    assert_eq!(
        res.body[0]["answer"]["selected"],
        choice_of(&choice_q_body, 2)
    );
    assert_eq!(res.body[1]["answer"]["text"], "because");
}

#[tokio::test]
async fn unenrollment_cuts_the_sittings_reads_and_writes() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "cut_t", "teacher").await;
    let student = login(&app, "cansu").await;
    let student_id = me_id(&app, &student).await;
    let (course, exam, subject) = open_exam_with_student(&app, &teacher, &student_id, "chem").await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}], "correct": "c1" }),
    )
    .await;
    let question = id_of(&question_body);

    // Enrolled: start the attempt, read the questions, save an answer.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt/questions"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": question, "selected": choice_of(&question_body, 1) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Unenrolled mid-exam: the course wall closes over the sitting too. The
    // question list is course content (`GET /exams/{id}` is already walled),
    // and answering from outside the course would dodge the same wall the
    // exam room enforces on entry.
    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{course}/enrollments/{student_id}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt/questions"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "question list after unenrollment: {}",
        res.body
    );
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": question, "selected": choice_of(&question_body, 0) })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "answer save after unenrollment: {}",
        res.body
    );

    // Their own status and submission stay theirs: like the rejoin lock,
    // finishing submits what's already saved and writes nothing new.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::OK,
        "finish stays open: {}",
        res.body
    );
    assert_eq!(res.body["status"], "submitted");
    assert_eq!(res.body["answered"], 1, "the enrolled-time answer survives");
}

#[tokio::test]
async fn answer_saves_stop_at_the_deadline() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "dl_t", "teacher").await;
    let student = login(&app, "dila").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "geo").await;
    let subject = create_subject(&app, &teacher, &course, "general").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let now = Timestamp::now().as_millis();

    // A window that closes right after the start — real time, the server
    // judges expiry by `Timestamp::now()`.
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "blitz", "kind": "quiz",
                "mode": "sync", "starts_at": now - 1_000, "ends_at": now + 1_500 }),
    )
    .await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "quick", "kind": "choice", "points": 1,
                "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}], "correct": "c0" }),
    )
    .await;
    let question = id_of(&question_body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": question, "selected": choice_of(&question_body, 0) })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::OK,
        "inside the window: {}",
        res.body
    );

    tokio::time::sleep(std::time::Duration::from_millis(1_700)).await;

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": question, "selected": choice_of(&question_body, 1) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "expired: {}", res.body);

    // The pre-deadline answer survives untouched for grading.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempts/{student_id}/answers"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        res.body["answers"][0]["selected"],
        choice_of(&question_body, 0)
    );
}

#[tokio::test]
async fn teacher_answer_sheet_judges_choices_and_suggests_a_score() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "sc_t", "teacher").await;
    let student = login(&app, "sena").await;
    let student_id = me_id(&app, &student).await;
    let (_course, exam, subject) =
        open_exam_with_student(&app, &teacher, &student_id, "chem").await;
    let q1_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}], "correct": "c1" }),
    )
    .await;
    let q1 = id_of(&q1_body);
    let q2_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "3 + 3?", "kind": "choice", "points": 20,
                "choices": [{"id": "c0", "text": "6"}, {"id": "c1", "text": "7"}], "correct": "c0" }),
    )
    .await;
    let q2 = id_of(&q2_body);
    let q3 = create_question(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Explain.", "kind": "text", "points": 30 }),
    )
    .await;
    let sheet_uri = format!("/exams/{exam}/attempts/{student_id}/answers");

    // No attempt yet: no sheet to read.
    let res = send(&app, "GET", &sheet_uri, Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "no attempt yet");

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    for body in [
        json!({ "question_id": q1, "selected": choice_of(&q1_body, 1) }), // right
        json!({ "question_id": q2, "selected": choice_of(&q2_body, 1) }), // wrong
        json!({ "question_id": q3, "text": "entropy grows" }),
    ] {
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/attempt/answers"),
            Some(&student),
            Some(body),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    }

    // The grader's view: judged rows plus the suggested score.
    let res = send(&app, "GET", &sheet_uri, Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["exam"], exam);
    assert_eq!(res.body["user"]["username"], "sena");
    let answers = res.body["answers"].as_array().expect("answers");
    assert_eq!(answers.len(), 3);
    assert_eq!(answers[0]["question"], q1);
    assert_eq!(answers[0]["is_correct"], true);
    assert_eq!(answers[1]["is_correct"], false);
    assert_eq!(answers[2]["text"], "entropy grows");
    assert!(
        answers[2]["is_correct"].is_null(),
        "text is the grader's call"
    );
    assert_eq!(res.body["auto_score"]["earned"], 10);
    assert_eq!(
        res.body["auto_score"]["possible"], 30,
        "text points never counted"
    );

    // The sheet is teacher-side only; students keep using their own view.
    let res = send(&app, "GET", &sheet_uri, Some(&student), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn live_monitor_tracks_answer_progress() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "prg_t", "teacher").await;
    let ayla = login(&app, "ayla").await;
    let bora = login(&app, "bora").await;
    let ayla_id = me_id(&app, &ayla).await;
    let bora_id = me_id(&app, &bora).await;
    let (course, exam, subject) = open_exam_with_student(&app, &teacher, &ayla_id, "stats").await;
    enroll(&app, &teacher, &course, &bora_id).await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}], "correct": "c1" }),
    )
    .await;
    let question = id_of(&question_body);
    create_question(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Explain.", "kind": "text", "points": 20 }),
    )
    .await;

    // Starting reports zero progress out of two questions.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&ayla),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["answered"], 0);
    assert_eq!(res.body["question_count"], 2);

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&ayla),
        Some(json!({ "question_id": question, "selected": choice_of(&question_body, 1) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let saved_at = res.body["updated_at"].as_i64().expect("updated_at");

    // The monitor: per-student progress and last activity, plus the total.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/live"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["question_count"], 2);
    let students = res.body["students"].as_array().expect("students");
    assert_eq!(students[0]["user"]["username"], "ayla");
    assert_eq!(students[0]["answered"], 1);
    assert_eq!(students[0]["last_activity"].as_i64(), Some(saved_at));
    assert_eq!(students[1]["user"]["username"], "bora");
    assert_eq!(students[1]["answered"], 0);
    assert!(students[1]["last_activity"].is_null());
}

#[tokio::test]
async fn questions_and_answers_cascade_with_deletes() {
    use hezarfen_backend::db::exam_question;
    use hezarfen_backend::domain::exam_answer::ExamAnswer;

    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "qc_t", "teacher").await;
    let student = login(&app, "cem").await;
    let student_id = me_id(&app, &student).await;
    let (course, exam, subject) = open_exam_with_student(&app, &teacher, &student_id, "geo2").await;
    let q1_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}], "correct": "c1" }),
    )
    .await;
    let q1 = id_of(&q1_body);
    let q2_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Explain.", "kind": "text", "points": 20 }),
    )
    .await;
    let q2 = id_of(&q2_body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    for body in [
        json!({ "question_id": q1, "selected": choice_of(&q1_body, 1) }),
        json!({ "question_id": q2, "text": "so" }),
    ] {
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/attempt/answers"),
            Some(&student),
            Some(body),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    }
    let exam_id = ExamId::from_key(&exam);
    assert_eq!(
        exam_question::list_for_exam(&db, &exam_id, None, 0)
            .await
            .unwrap()
            .0
            .len(),
        2
    );
    assert_eq!(
        ExamAnswer::list_for_exam(&exam_id, &db)
            .await
            .unwrap()
            .len(),
        2
    );

    // Deleting one question takes its answers with it. The freeze gate now
    // lives inside `db::exam_question::delete`'s own transaction (not in a lock the
    // handler held), so the attempt has to go first for the cascade to be
    // exercised at all — that refusal is asserted here before it is cleared.
    let frozen = exam_question::list_for_exam(&db, &exam_id, None, 0)
        .await
        .unwrap()
        .0
        .into_iter()
        .next()
        .unwrap();
    assert!(
        matches!(exam_question::delete(&db, frozen).await, Err(err) if err.to_string().contains("after attempts")),
        "the gate must refuse a question delete while an attempt exists"
    );
    db.query("DELETE exam_attempt WHERE exam = $ex")
        .bind(("ex", exam_id.record()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let question = exam_question::list_for_exam(&db, &exam_id, None, 0)
        .await
        .unwrap()
        .0
        .into_iter()
        .next()
        .unwrap();
    exam_question::delete(&db, question).await.unwrap();
    assert_eq!(
        exam_question::list_for_exam(&db, &exam_id, None, 0)
            .await
            .unwrap()
            .0
            .len(),
        1
    );
    assert_eq!(
        ExamAnswer::list_for_exam(&exam_id, &db)
            .await
            .unwrap()
            .len(),
        1,
        "only the deleted question's answer goes"
    );

    // Deleting the exam clears the rest.
    let res = send(
        &app,
        "DELETE",
        &format!("/exams/{exam}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert!(
        exam_question::list_for_exam(&db, &exam_id, None, 0)
            .await
            .unwrap()
            .0
            .is_empty()
    );
    assert!(
        ExamAnswer::list_for_exam(&exam_id, &db)
            .await
            .unwrap()
            .is_empty()
    );

    // And a course delete cascades through its exams' questions and answers.
    let now = Timestamp::now().as_millis();
    let exam2 = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "retake", "kind": "quiz",
                "mode": "sync", "starts_at": now - 1_000, "ends_at": now + 600_000 }),
    )
    .await;
    let q_body = create_question_body(
        &app,
        &teacher,
        &exam2,
        &subject,
        json!({ "text": "2 + 2?", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}], "correct": "c1" }),
    )
    .await;
    let q = id_of(&q_body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam2}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam2}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": q, "selected": choice_of(&q_body, 1) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    unenroll(&app, &teacher, &course, &student_id).await;
    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{course}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let exam2_id = ExamId::from_key(&exam2);
    assert!(
        exam_question::list_for_exam(&db, &exam2_id, None, 0)
            .await
            .unwrap()
            .0
            .is_empty()
    );
    assert!(
        ExamAnswer::list_for_exam(&exam2_id, &db)
            .await
            .unwrap()
            .is_empty()
    );
    // The course's subjects go down with it too.
    let res = send(
        &app,
        "GET",
        &format!("/subjects/{subject}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "subject cascaded");
}

#[tokio::test]
async fn question_patch_revalidates_the_stale_kind_bundle() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "rv_t", "teacher").await;
    let course = create_course(&app, &teacher, "sets").await;
    let subject = create_subject(&app, &teacher, &course, "unions").await;
    let exam = create_exam(&app, &teacher, &course, "final", "final").await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "pick", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}, {"id": "c2", "text": "c"}], "correct": "c2" }),
    )
    .await;
    let question = id_of(&question_body);
    let uri = format!("/exams/{exam}/questions/{question}");

    // Shrinking the options while keeping the old `correct` would leave it
    // dangling past the end — the merge must re-validate the bundle.
    let res = send(
        &app,
        "PATCH",
        &uri,
        Some(&teacher),
        Some(json!({ "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}] })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::BAD_REQUEST,
        "kept correct=2 dangles: {}",
        res.body
    );

    // Same shrink with `correct` brought along is fine.
    let res = send(
        &app,
        "PATCH",
        &uri,
        Some(&teacher),
        Some(json!({ "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}], "correct": "c1" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["correct"], res.body["choices"][1]["id"]);

    // A lone out-of-range `correct` against the kept choices is caught too.
    let res = send(
        &app,
        "PATCH",
        &uri,
        Some(&teacher),
        Some(json!({ "correct": "c5" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // Clearing just one half of the bundle can't leave a half-question.
    let res = send(
        &app,
        "PATCH",
        &uri,
        Some(&teacher),
        Some(json!({ "correct": null })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // Nothing of the failed PATCHes stuck.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(
        common::items(&res.body)[0]["choices"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        common::items(&res.body)[0]["correct"],
        common::items(&res.body)[0]["choices"][1]["id"]
    );
}

#[tokio::test]
async fn question_authoring_follows_course_management() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "own_t", "teacher").await;
    let boss = login_as(&app, &db, "own_m", "manager").await;
    let course = create_course(&app, &teacher, "greek").await;
    let subject = create_subject(&app, &boss, &course, "alphabet").await;
    let exam = create_exam(&app, &teacher, &course, "final", "final").await;

    // A manager+ authors questions in anyone's course, like every other
    // course-management write.
    let question_body = create_question_body(
        &app,
        &boss,
        &exam,
        &subject,
        json!({ "text": "pick", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}], "correct": "c0" }),
    )
    .await;
    let question = id_of(&question_body);
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{question}"),
        Some(&boss),
        Some(json!({ "points": 20 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/exams/{exam}/questions/{question}"),
        Some(&boss),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn concurrent_answer_saves_never_collide() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "cc_t", "teacher").await;
    let student = login(&app, "cana").await;
    let student_id = me_id(&app, &student).await;
    let (_course, exam, subject) =
        open_exam_with_student(&app, &teacher, &student_id, "race").await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "pick", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}, {"id": "c2", "text": "c"}], "correct": "c0" }),
    )
    .await;
    let question = id_of(&question_body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);

    // A burst of autosaves for the same question (fast clicking / reconnect
    // races): the composite-id upsert must converge, never 500.
    let mut handles = Vec::new();
    for i in 0..24i64 {
        let app = app.clone();
        let student = student.clone();
        let uri = format!("/exams/{exam}/attempt/answers");
        let body = json!({ "question_id": question, "selected": choice_of(&question_body, (i % 3) as usize) });
        handles.push(tokio::spawn(async move {
            send(&app, "POST", &uri, Some(&student), Some(body))
                .await
                .status
        }));
    }
    for h in handles {
        assert_eq!(
            h.await.unwrap(),
            StatusCode::OK,
            "concurrent save must not fail"
        );
    }

    // One row survives, whichever save won.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempts/{student_id}/answers"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["answers"].as_array().unwrap().len(), 1);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.body["answered"], 1, "24 saves, one answer");
}

// --- course sessions + roll call ------------------------------------------

#[tokio::test]
async fn session_crud_follows_course_management() {
    let (app, db) = app_and_db().await;
    let owner = login_as(&app, &db, "owner", "teacher").await;
    let rival = login_as(&app, &db, "rival", "teacher").await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let student = login(&app, "ali").await;
    let course = create_course(&app, &owner, "algebra").await;
    let now = Timestamp::now().as_millis();
    let start = now + 3_600_000;

    // Create: student 403, unrelated teacher 403, owner 201, manager+ 201.
    let body = json!({ "topic": "limits", "starts_at": start });
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(&student),
        Some(body.clone()),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(&rival),
        Some(body.clone()),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(&owner),
        Some(body.clone()),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let session = id_of(&res.body);
    // The teacher defaults to the caller, rendered as a PersonRef.
    assert_eq!(res.body["teacher"]["username"], "owner");
    assert_eq!(res.body["topic"], "limits");
    assert_eq!(res.body["starts_at"], start);
    assert!(res.body["ends_at"].is_null());
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(&boss),
        Some(body.clone()),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "manager+ manages any course"
    );

    // Explicit teacher_id: must exist and be teacher+.
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(&owner),
        Some(json!({ "starts_at": now + 60_000, "teacher_id": "01UNKNOWN" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "unknown teacher");
    let ali_id = me_id(&app, &student).await;
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(&owner),
        Some(json!({ "starts_at": now + 60_000, "teacher_id": ali_id })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::BAD_REQUEST,
        "a student cannot teach"
    );
    let rival_id = me_id(&app, &rival).await;
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(&owner),
        Some(json!({ "starts_at": now + 120_000, "teacher_id": rival_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["teacher"]["username"], "rival");
    let rival_session = id_of(&res.body);

    // Time range validated on create and update.
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(&owner),
        Some(json!({ "starts_at": now + 10_000, "ends_at": now + 5_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // Past lessons cannot be scheduled — on create or by a later PATCH,
    // whichever end carries the past value.
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(&owner),
        Some(json!({ "starts_at": now - 3_600_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(&owner),
        Some(json!({ "starts_at": now + 3_600_000, "ends_at": now - 3_600_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "past ends_at");

    // Reads follow course visibility: an unenrolled student is refused …
    let res = send(
        &app,
        "GET",
        &format!("/courses/{course}/sessions"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // … an enrolled one reads; parents must exist.
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/enrollments"),
        Some(&owner),
        Some(json!({ "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(
        &app,
        "GET",
        &format!("/courses/{course}/sessions"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    // owner's, the manager's, and the one taught by `rival` — newest starts_at
    // first, so the two `start` lessons precede the earlier `rival` one.
    assert_eq!(common::items(&res.body).len(), 3);
    assert_eq!(common::items(&res.body)[2]["starts_at"], now + 120_000);
    let res = send(&app, "GET", "/courses/nope/sessions", Some(&student), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = send(
        &app,
        "GET",
        &format!("/sessions/{session}"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    // The session's own teacher reads it without course rights; a session of
    // someone else's course stays out of reach.
    let res = send(
        &app,
        "GET",
        &format!("/sessions/{rival_session}"),
        Some(&rival),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(
        &app,
        "GET",
        &format!("/sessions/{session}"),
        Some(&rival),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(&app, "GET", "/sessions/nope", Some(&student), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // PATCH: management rights; set/keep/clear semantics; range check.
    let res = send(
        &app,
        "PATCH",
        &format!("/sessions/{session}"),
        Some(&rival),
        Some(json!({ "topic": "hijack" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "PATCH",
        &format!("/sessions/{session}"),
        Some(&owner),
        Some(json!({ "topic": "derivatives", "ends_at": start + 100_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["topic"], "derivatives");
    assert_eq!(res.body["ends_at"], start + 100_000);
    assert_eq!(res.body["starts_at"], start, "omitted keeps");
    let res = send(
        &app,
        "PATCH",
        &format!("/sessions/{session}"),
        Some(&owner),
        Some(json!({ "ends_at": null })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body["ends_at"].is_null(), "explicit null clears");
    let res = send(
        &app,
        "PATCH",
        &format!("/sessions/{session}"),
        Some(&owner),
        Some(json!({ "ends_at": now + 1_800_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "ends before starts");
    let res = send(
        &app,
        "PATCH",
        &format!("/sessions/{session}"),
        Some(&owner),
        Some(json!({ "starts_at": now - 3_600_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "backdated start");
    let res = send(
        &app,
        "PATCH",
        &format!("/sessions/{session}"),
        Some(&owner),
        Some(json!({ "ends_at": now - 3_600_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "backdated end");
    // A lesson already underway (start inside the grace) stays editable as
    // long as the past time is only kept, not re-sent.
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(&owner),
        Some(json!({ "topic": "underway", "starts_at": now - 1_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let underway = id_of(&res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/sessions/{underway}"),
        Some(&owner),
        Some(json!({ "topic": "still editable" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    // Reassigning the teacher revalidates the target.
    let res = send(
        &app,
        "PATCH",
        &format!("/sessions/{rival_session}"),
        Some(&boss),
        Some(json!({ "teacher_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // DELETE: management rights.
    let res = send(
        &app,
        "DELETE",
        &format!("/sessions/{session}"),
        Some(&rival),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "DELETE",
        &format!("/sessions/{session}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "GET",
        &format!("/sessions/{session}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn roll_call_rbac_and_upsert() {
    let (app, db) = app_and_db().await;
    let owner = login_as(&app, &db, "owner", "teacher").await;
    let hoca = login_as(&app, &db, "hoca", "teacher").await;
    let rival = login_as(&app, &db, "rival", "teacher").await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let ali = login(&app, "ali").await;
    let veli = login(&app, "veli").await;
    let ali_id = me_id(&app, &ali).await;
    let veli_id = me_id(&app, &veli).await;
    let hoca_id = me_id(&app, &hoca).await;

    let course = create_course(&app, &owner, "algebra").await;
    enroll(&app, &owner, &course, &ali_id).await;
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
        Some(&owner),
        Some(
            json!({ "starts_at": Timestamp::now().as_millis() + 3_600_000, "teacher_id": hoca_id }),
        ),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let session = id_of(&res.body);
    let mark_uri = format!("/sessions/{session}/attendance");

    // The session's teacher takes roll even without course-management rights.
    let res = send(
        &app,
        "POST",
        &mark_uri,
        Some(&hoca),
        Some(json!({ "status": "present", "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["user"]["username"], "ali");
    assert_eq!(res.body["marked_by"]["username"], "hoca");
    assert_eq!(res.body["course"].as_str().unwrap(), course);

    // Re-marking overwrites — one row per session+user (course manager may too).
    let res = send(
        &app,
        "POST",
        &mark_uri,
        Some(&owner),
        Some(json!({ "status": "absent", "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    // The roster follows roll-call rights: the session teacher reads it, a
    // student (even a listed one) and an unrelated teacher do not — students
    // get their own tallies via `/attendance/me`.
    let res = send(&app, "GET", &mark_uri, Some(&hoca), None).await;
    assert_eq!(res.status, StatusCode::OK);
    let roster = common::items(&res.body);
    assert_eq!(roster.len(), 1);
    assert_eq!(roster[0]["status"], "absent");
    let res = send(&app, "GET", &mark_uri, Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(&app, "GET", &mark_uri, Some(&rival), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // Neither an unrelated teacher nor the student themselves may mark.
    let res = send(
        &app,
        "POST",
        &mark_uri,
        Some(&rival),
        Some(json!({ "status": "present", "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "POST",
        &mark_uri,
        Some(&ali),
        Some(json!({ "status": "present", "user_id": ali_id })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "students never self-mark"
    );

    // The roster comes from enrollment; targets must exist; statuses are closed.
    let res = send(
        &app,
        "POST",
        &mark_uri,
        Some(&hoca),
        Some(json!({ "status": "present", "user_id": veli_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "not enrolled");
    let res = send(
        &app,
        "POST",
        &mark_uri,
        Some(&hoca),
        Some(json!({ "status": "present", "user_id": "01UNKNOWN" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = send(
        &app,
        "POST",
        &mark_uri,
        Some(&hoca),
        Some(json!({ "status": "sleeping", "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = send(
        &app,
        "POST",
        "/sessions/nope/attendance",
        Some(&hoca),
        Some(json!({ "status": "present", "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // The session teacher's own row is management's call: even the course
    // creator (teacher role) can't write it; a manager can.
    let res = send(
        &app,
        "POST",
        &mark_uri,
        Some(&hoca),
        Some(json!({ "status": "present", "user_id": hoca_id })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "no self-declared presence"
    );
    let res = send(
        &app,
        "POST",
        &mark_uri,
        Some(&owner),
        Some(json!({ "status": "present", "user_id": hoca_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "POST",
        &mark_uri,
        Some(&boss),
        Some(json!({ "status": "late", "user_id": hoca_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    // Removal mirrors marking rights.
    let res = send(
        &app,
        "DELETE",
        &format!("{mark_uri}/{ali_id}"),
        Some(&rival),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "DELETE",
        &format!("{mark_uri}/{hoca_id}"),
        Some(&hoca),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "teacher row needs manager+"
    );
    let res = send(
        &app,
        "DELETE",
        &format!("{mark_uri}/{hoca_id}"),
        Some(&boss),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "DELETE",
        &format!("{mark_uri}/{ali_id}"),
        Some(&hoca),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "DELETE",
        &format!("{mark_uri}/{ali_id}"),
        Some(&hoca),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "row already gone");
}

#[tokio::test]
async fn deleting_session_or_course_cascades_roll_call() {
    let (app, db) = app_and_db().await;
    let owner = login_as(&app, &db, "owner", "teacher").await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;

    let course = create_course(&app, &owner, "algebra").await;
    enroll(&app, &owner, &course, &ali_id).await;
    let now = Timestamp::now().as_millis();
    let s1 = create_session(&app, &owner, &course, now + 3_600_000).await;
    let s2 = create_session(&app, &owner, &course, now + 7_200_000).await;
    for s in [&s1, &s2] {
        let res = send(
            &app,
            "POST",
            &format!("/sessions/{s}/attendance"),
            Some(&owner),
            Some(json!({ "status": "present", "user_id": ali_id })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK);
    }
    let report = send(&app, "GET", "/attendance/me", Some(&ali), None).await;
    assert_eq!(report.body["sessions"]["total"], 2);

    // Deleting one session removes exactly its rows...
    let res = send(
        &app,
        "DELETE",
        &format!("/sessions/{s1}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let report = send(&app, "GET", "/attendance/me", Some(&ali), None).await;
    assert_eq!(report.body["sessions"]["total"], 1);

    // ...and deleting the course removes the rest, sessions included.
    unenroll(&app, &owner, &course, &ali_id).await;
    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{course}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(&app, "GET", &format!("/sessions/{s2}"), Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let report = send(&app, "GET", "/attendance/me", Some(&ali), None).await;
    assert_eq!(report.body["sessions"]["total"], 0);
    assert!(report.body["courses"].as_array().unwrap().is_empty());
}

// --- work log ---------------------------------------------------------------

#[tokio::test]
async fn work_log_lifecycle_is_server_stamped_and_exclusive() {
    let (app, db) = app_and_db().await;
    let hoca = login_as(&app, &db, "hoca", "teacher").await;
    let ali = login(&app, "ali").await;

    // Staff only.
    let res = send(&app, "POST", "/work/check-in", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(&app, "GET", "/work/me", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // Check out before check-in conflicts.
    let res = send(&app, "POST", "/work/check-out", Some(&hoca), None).await;
    assert_eq!(res.status, StatusCode::CONFLICT);

    let res = send(&app, "POST", "/work/check-in", Some(&hoca), None).await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert!(res.body["check_out"].is_null());
    let first_in = res.body["check_in"].as_i64().unwrap();

    // A second check-in neither opens a second stint nor resets the clock.
    let res = send(&app, "POST", "/work/check-in", Some(&hoca), None).await;
    assert_eq!(res.status, StatusCode::CONFLICT);
    let res = send(&app, "GET", "/work/me", Some(&hoca), None).await;
    let log = common::items(&res.body);
    assert_eq!(log.len(), 1);
    assert_eq!(log[0]["check_in"].as_i64().unwrap(), first_in);

    let res = send(&app, "POST", "/work/check-out", Some(&hoca), None).await;
    assert_eq!(res.status, StatusCode::OK);
    let out = res.body["check_out"].as_i64().unwrap();
    assert!(out >= first_in);
    assert_eq!(res.body["duration_ms"].as_i64().unwrap(), out - first_in);
    let res = send(&app, "POST", "/work/check-out", Some(&hoca), None).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "already checked out");

    // The open slot is free again; the log keeps both stints, newest first.
    let res = send(&app, "POST", "/work/check-in", Some(&hoca), None).await;
    assert_eq!(res.status, StatusCode::CREATED);
    let res = send(&app, "GET", "/work/me", Some(&hoca), None).await;
    let log = common::items(&res.body);
    assert_eq!(log.len(), 2);
    assert!(log[0]["check_out"].is_null(), "open stint sorts newest");
    assert!(!log[1]["check_out"].is_null());
}

// --- pomodoro ----------------------------------------------------------------

#[tokio::test]
async fn pomodoro_restart_replaces_finish_closes_and_teachers_read() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;
    let hoca = login_as(&app, &db, "hoca", "teacher").await;
    let ali_id = me_id(&app, &ali).await;

    // Students only on start; finishing with nothing running conflicts.
    let res = send(&app, "POST", "/pomodoro/start", Some(&hoca), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(&app, "POST", "/pomodoro/finish", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::CONFLICT);

    let res = send(&app, "POST", "/pomodoro/start", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert!(res.body["finished_at"].is_null());
    let first_start = res.body["started_at"].as_i64().unwrap();

    // A restart discards the dangling session: one row, clock reset allowed.
    let res = send(&app, "POST", "/pomodoro/start", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert!(res.body["started_at"].as_i64().unwrap() >= first_start);
    let res = send(&app, "GET", "/pomodoro/me", Some(&ali), None).await;
    assert_eq!(common::items(&res.body).len(), 1);
    assert_eq!(
        res.body["total_focus_ms"], 0,
        "running session counts nothing"
    );

    let res = send(&app, "POST", "/pomodoro/finish", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::OK);
    let finished_at = res.body["finished_at"].as_i64().unwrap();
    let duration = res.body["duration_ms"].as_i64().unwrap();
    assert_eq!(
        duration,
        finished_at - res.body["started_at"].as_i64().unwrap()
    );
    let res = send(&app, "POST", "/pomodoro/finish", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "already finished");

    // The open slot is free again; the log keeps both sessions, newest first,
    // and the focus total sums only the finished one.
    let res = send(&app, "POST", "/pomodoro/start", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::CREATED);
    let res = send(&app, "GET", "/pomodoro/me", Some(&ali), None).await;
    let log = common::items(&res.body);
    assert_eq!(log.len(), 2);
    assert!(log[0]["finished_at"].is_null(), "open session sorts newest");
    assert_eq!(res.body["total_focus_ms"].as_i64().unwrap(), duration);

    // Reading someone's log is teacher+; unknown users are 404.
    let res = send(
        &app,
        "GET",
        &format!("/pomodoro/{ali_id}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "GET",
        &format!("/pomodoro/{ali_id}"),
        Some(&hoca),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(common::items(&res.body).len(), 2);
    let res = send(&app, "GET", "/pomodoro/01UNKNOWN", Some(&hoca), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn work_log_manager_reads_and_corrections() {
    let (app, db) = app_and_db().await;
    let hoca = login_as(&app, &db, "hoca", "teacher").await;
    let other = login_as(&app, &db, "other", "teacher").await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let hoca_id = me_id(&app, &hoca).await;

    // One closed stint and one open stint for hoca.
    send(&app, "POST", "/work/check-in", Some(&hoca), None).await;
    let closed = send(&app, "POST", "/work/check-out", Some(&hoca), None).await;
    let closed_id = id_of(&closed.body);
    let open = send(&app, "POST", "/work/check-in", Some(&hoca), None).await;
    let open_id = id_of(&open.body);

    // Reading someone's log is manager+; unknown users are 404.
    let res = send(&app, "GET", &format!("/work/{hoca_id}"), Some(&other), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(&app, "GET", &format!("/work/{hoca_id}"), Some(&boss), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(common::items(&res.body).len(), 2);
    let res = send(&app, "GET", "/work/01UNKNOWN", Some(&boss), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // Corrections: manager+ only, closed entries only, ordered instants only.
    let res = send(
        &app,
        "PATCH",
        &format!("/work/entries/{closed_id}"),
        Some(&hoca),
        Some(json!({ "check_in": 1000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "PATCH",
        &format!("/work/entries/{closed_id}"),
        Some(&boss),
        Some(json!({ "check_in": 1000, "check_out": 2000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["check_in"], 1000);
    assert_eq!(res.body["check_out"], 2000);
    assert_eq!(res.body["duration_ms"], 1000);
    let res = send(
        &app,
        "PATCH",
        &format!("/work/entries/{closed_id}"),
        Some(&boss),
        Some(json!({ "check_out": 500 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "out precedes in");
    let res = send(
        &app,
        "PATCH",
        &format!("/work/entries/{open_id}"),
        Some(&boss),
        Some(json!({ "check_in": 1 })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CONFLICT,
        "open entries aren't correctable"
    );
    let res = send(
        &app,
        "PATCH",
        "/work/entries/nope",
        Some(&boss),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // Deletion: manager+, any entry (open ones included — the stray-row broom).
    let res = send(
        &app,
        "DELETE",
        &format!("/work/entries/{closed_id}"),
        Some(&hoca),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "DELETE",
        &format!("/work/entries/{open_id}"),
        Some(&boss),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "DELETE",
        &format!("/work/entries/{open_id}"),
        Some(&boss),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    // With the open row swept, hoca can check in again.
    let res = send(&app, "POST", "/work/check-in", Some(&hoca), None).await;
    assert_eq!(res.status, StatusCode::CREATED);
}

#[tokio::test]
async fn work_duration_survives_absurd_corrected_instants() {
    let (app, db) = app_and_db().await;
    let hoca = login_as(&app, &db, "wid_t", "teacher").await;
    let boss = login_as(&app, &db, "wid_m", "manager").await;
    send(&app, "POST", "/work/check-in", Some(&hoca), None).await;
    let closed = send(&app, "POST", "/work/check-out", Some(&hoca), None).await;
    let closed_id = id_of(&closed.body);

    // Corrected instants are arbitrary i64 millis; the widest legal pair must
    // not overflow the `check_out - check_in` duration (a wrapped negative in
    // release, a panicking 500 in debug) — it saturates instead.
    let res = send(
        &app,
        "PATCH",
        &format!("/work/entries/{closed_id}"),
        Some(&boss),
        Some(json!({ "check_in": i64::MIN, "check_out": i64::MAX })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        res.body["duration_ms"].as_i64(),
        Some(i64::MAX),
        "the over-wide stint saturates: {}",
        res.body
    );
    // The stored row must stay readable, not a permanent 500.
    let res = send(&app, "GET", "/work/me", Some(&hoca), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

// --- attendance reports -------------------------------------------------------

#[tokio::test]
async fn attendance_report_tallies_events_and_sessions_per_course() {
    let (app, db) = app_and_db().await;
    let owner = login_as(&app, &db, "owner", "teacher").await;
    let ali = login(&app, "ali").await;
    let veli = login(&app, "veli").await;
    let ali_id = me_id(&app, &ali).await;

    // A fresh user's report is all zeros with null rates.
    let res = send(&app, "GET", "/attendance/me", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["events"]["total"], 0);
    assert!(res.body["events"]["rate"].is_null());
    assert!(res.body["courses"].as_array().unwrap().is_empty());

    // Two event rows, both teacher-marked (students never mark).
    let ev1 = send(
        &app,
        "POST",
        "/events",
        Some(&owner),
        Some(json!({ "title": "assembly" })),
    )
    .await;
    let ev2 = send(
        &app,
        "POST",
        "/events",
        Some(&owner),
        Some(json!({ "title": "trip" })),
    )
    .await;
    let (ev1, ev2) = (id_of(&ev1.body), id_of(&ev2.body));
    send(
        &app,
        "POST",
        &format!("/events/{ev1}/attendance"),
        Some(&owner),
        Some(json!({ "status": "present", "user_id": ali_id })),
    )
    .await;
    send(
        &app,
        "POST",
        &format!("/events/{ev2}/attendance"),
        Some(&owner),
        Some(json!({ "status": "absent", "user_id": ali_id })),
    )
    .await;

    // Roll call across two courses: algebra present+late, physics absent+excused.
    let algebra = create_course(&app, &owner, "algebra").await;
    let physics = create_course(&app, &owner, "physics").await;
    for (course, statuses) in [
        (&algebra, ["present", "late"]),
        (&physics, ["absent", "excused"]),
    ] {
        enroll(&app, &owner, course, &ali_id).await;
        for (n, status) in statuses.iter().enumerate() {
            let session = create_session(
                &app,
                &owner,
                course,
                Timestamp::now().as_millis() + (n as i64 + 1) * 3_600_000,
            )
            .await;
            let res = send(
                &app,
                "POST",
                &format!("/sessions/{session}/attendance"),
                Some(&owner),
                Some(json!({ "status": status, "user_id": ali_id })),
            )
            .await;
            assert_eq!(res.status, StatusCode::OK);
        }
    }

    let res = send(&app, "GET", "/attendance/me", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["user"].as_str().unwrap(), ali_id);
    // Events: 1 present + 1 absent → rate 1/2.
    assert_eq!(res.body["events"]["present"], 1);
    assert_eq!(res.body["events"]["absent"], 1);
    assert_eq!(res.body["events"]["total"], 2);
    assert_eq!(res.body["events"]["rate"].as_f64().unwrap(), 0.5);
    // Sessions overall: present+late+absent+excused → rate (1+1)/3.
    assert_eq!(res.body["sessions"]["total"], 4);
    assert_eq!(res.body["sessions"]["excused"], 1);
    let rate = res.body["sessions"]["rate"].as_f64().unwrap();
    assert!(
        (rate - 2.0 / 3.0).abs() < 1e-12,
        "excused stays out: {rate}"
    );
    // Per-course blocks: algebra 100% attended, physics 0% (excused uncounted).
    let courses = res.body["courses"].as_array().unwrap();
    assert_eq!(courses.len(), 2);
    let block = |title: &str| {
        courses
            .iter()
            .find(|b| b["course"]["title"] == title)
            .unwrap_or_else(|| panic!("missing course block {title}"))
            .clone()
    };
    assert_eq!(block("algebra")["counts"]["rate"].as_f64().unwrap(), 1.0);
    assert_eq!(block("algebra")["counts"]["late"], 1);
    assert_eq!(block("physics")["counts"]["rate"].as_f64().unwrap(), 0.0);
    assert_eq!(block("physics")["counts"]["excused"], 1);

    // Unenrolling hides marks, never absences: the physics block stays.
    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{physics}/enrollments/{ali_id}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(&app, "GET", "/attendance/me", Some(&ali), None).await;
    assert_eq!(res.body["courses"].as_array().unwrap().len(), 2);

    // Someone else's report: teacher+ only; unknown users are 404.
    let res = send(
        &app,
        "GET",
        &format!("/attendance/{ali_id}"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "GET",
        &format!("/attendance/{ali_id}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["sessions"]["total"], 4);

    // A teacher's view narrows to the courses they manage: rival runs only
    // chemistry, so ali's report shows that block alone — the overall session
    // tally follows, while event tallies stay school-wide.
    let rival = login_as(&app, &db, "rival", "teacher").await;
    let chemistry = create_course(&app, &rival, "chemistry").await;
    enroll(&app, &rival, &chemistry, &ali_id).await;
    let session = create_session(
        &app,
        &rival,
        &chemistry,
        Timestamp::now().as_millis() + 10 * 3_600_000,
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/sessions/{session}/attendance"),
        Some(&rival),
        Some(json!({ "status": "present", "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    let res = send(
        &app,
        "GET",
        &format!("/attendance/{ali_id}"),
        Some(&rival),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["events"]["total"], 2, "events stay school-wide");
    assert_eq!(res.body["sessions"]["total"], 1);
    let blocks = res.body["courses"].as_array().unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["course"]["title"], "chemistry");

    // The owner's view still carries only their own two courses …
    let res = send(
        &app,
        "GET",
        &format!("/attendance/{ali_id}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(res.body["courses"].as_array().unwrap().len(), 2);
    assert_eq!(res.body["sessions"]["total"], 4);

    // … and the self view stays complete: all three courses, every row.
    let res = send(&app, "GET", "/attendance/me", Some(&ali), None).await;
    assert_eq!(res.body["courses"].as_array().unwrap().len(), 3);
    assert_eq!(res.body["sessions"]["total"], 5);

    let res = send(&app, "GET", "/attendance/01UNKNOWN", Some(&owner), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

/// Issue #24: a student or parent needs a way to find the teacher they are
/// allowed to write to, so `/users/search` is no longer teacher+ — but below
/// staff it shows staff *only*, in the items and in `total` alike (the
/// restriction is in the query, not a post-filter over one page).
#[tokio::test]
async fn search_below_staff_sees_only_staff() {
    let (app, db) = app_and_db().await;
    // Both match `q=ay`: one student, one teacher.
    let ayse = login(&app, "ayse").await;
    let _ayhan = login_as(&app, &db, "ayhan", "teacher").await;
    let anne = login_as(&app, &db, "anne", "parent").await;
    let mudur = login_as(&app, &db, "mudur", "manager").await;

    for (who, cookie) in [("student", &ayse), ("parent", &anne)] {
        let res = send(&app, "GET", "/users/search?q=ay", Some(cookie), None).await;
        assert_eq!(res.status, StatusCode::OK, "{who}: {}", res.body);
        let items = common::items(&res.body);
        assert_eq!(
            items.len(),
            1,
            "{who} must see only the teacher: {}",
            res.body
        );
        assert_eq!(items[0]["username"], "ayhan", "{who}");
        assert_eq!(
            res.body["total"], 1,
            "{who}: total counts the visible set only"
        );
        // Only the picker fields, as before — never contact details.
        assert!(items[0].get("email").is_none(), "{who}");

        // Naming a role they may not message is a refusal, not an empty page.
        let res = send(
            &app,
            "GET",
            "/users/search?q=ay&role=student",
            Some(cookie),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{who}: {}", res.body);
        // A staff role they may message filters as usual.
        let res = send(
            &app,
            "GET",
            "/users/search?q=ay&role=teacher",
            Some(cookie),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{who}: {}", res.body);
        assert_eq!(common::items(&res.body).len(), 1, "{who}");
    }

    // Staff keep the unrestricted picker: the student is back in the results.
    let res = send(&app, "GET", "/users/search?q=ay", Some(&mudur), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["total"], 2, "{}", res.body);
    let res = send(
        &app,
        "GET",
        "/users/search?q=ay&role=student",
        Some(&mudur),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(common::items(&res.body)[0]["username"], "ayse");
}

// --- users: search matching semantics --------------------------------------

/// The search fragment is matched with literal `CONTAINS` semantics — never
/// SQL-`LIKE`. A regression to a `LIKE '%{q}%'` pattern would turn `%` and `_`
/// in the query into wildcards: `q=%` would dump every named user and `q=_`
/// every username, defeating the "real substring only" intent of the picker.
/// `%` and `_` must match only users who literally carry those characters.
#[tokio::test]
async fn search_matches_are_literal_not_like_wildcards() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;

    // Three students: one plainly named, one with a literal `%` in their name,
    // one with a literal `_` in their username.
    let alice = login(&app, "alice").await;
    let res = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&alice),
        Some(json!({ "name": "Plain", "surname": "Jane" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    let percy = login(&app, "percy").await;
    let res = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&percy),
        Some(json!({ "name": "100% legend" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    login(&app, "under_score").await;

    // `q=%` (url-encoded %25) is a literal percent sign: only percy matches.
    // Under LIKE semantics it would match every user with a name.
    let res = send(&app, "GET", "/users/search?q=%25", Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::OK);
    let hits = common::items(&res.body);
    assert_eq!(hits.len(), 1, "`%` must match literally: {hits:?}");
    assert_eq!(hits[0]["username"], "percy");

    // `q=_` is a literal underscore: only under_score matches. Under LIKE
    // semantics `_` is any-single-character and would match everyone.
    let res = send(&app, "GET", "/users/search?q=_", Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::OK);
    let hits = common::items(&res.body);
    assert_eq!(hits.len(), 1, "`_` must match literally: {hits:?}");
    assert_eq!(hits[0]["username"], "under_score");
}

/// A blank search fragment is refused, not treated as match-everything.
#[tokio::test]
async fn search_rejects_a_blank_query() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    login(&app, "alice").await;

    // Whitespace-only (url-encoded spaces) -> 400, not the full user list.
    let res = send(&app, "GET", "/users/search?q=%20%20", Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // Missing `q` entirely is a deserialization failure, not a wildcard.
    let res = send(&app, "GET", "/users/search", Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
}

/// The app is EN/TR, so the user picker must fold Turkish casing: `İ` (U+0130)
/// lowercases to `i` + U+0307 in both Rust and SurrealQL, which used to make
/// `ilker` and `İLKER` two disjoint searches — a teacher typing lowercase got
/// an empty picker. Same defect, same fix as the bank-question search.
#[tokio::test]
async fn user_search_folds_turkish_casing() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;

    let ilker = login(&app, "ilker_a").await;
    let res = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&ilker),
        Some(json!({ "name": "İLKER", "surname": "ÇAĞLAR" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let ilknur = login(&app, "ilknur_b").await;
    let res = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&ilknur),
        Some(json!({ "name": "ilknur", "surname": "Işık" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Every casing of the shared `ilk` prefix finds both, in both directions:
    // `ilk`, `İLK`, `ılk`, `ILK`.
    for needle in ["ilk", "%C4%B0LK", "%C4%B1lk", "ILK"] {
        let res = send(
            &app,
            "GET",
            &format!("/users/search?q={needle}"),
            Some(&teacher),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        let hits = common::items(&res.body);
        assert_eq!(hits.len(), 2, "needle {needle} missed: {hits:?}");
    }

    // Diacritics fold on the surname too, both ways (`ÇAĞLAR` ↔ `caglar`,
    // `Işık` ↔ `isik`).
    for (needle, username) in [
        ("caglar", "ilker_a"),
        ("%C3%A7a%C4%9Flar", "ilker_a"),
        ("isik", "ilknur_b"),
        ("I%C5%9F%C4%B1k", "ilknur_b"),
    ] {
        let res = send(
            &app,
            "GET",
            &format!("/users/search?q={needle}"),
            Some(&teacher),
            None,
        )
        .await;
        let hits = common::items(&res.body);
        assert_eq!(hits.len(), 1, "needle {needle} missed: {hits:?}");
        assert_eq!(hits[0]["username"], username);
    }

    // Folding widens the match, it does not match everything.
    let res = send(&app, "GET", "/users/search?q=zeynep", Some(&teacher), None).await;
    assert_eq!(common::items(&res.body).len(), 0);
}

// --- courses: catalog dedup -------------------------------------------------

/// A user who both created a course (while staff) and is enrolled in it (after
/// a demotion to student) sees it once in the catalog, not twice — the created
/// and enrolled sources are deduplicated. Enrollment is student-only now, so
/// that overlap can only arise across a role change; the dedup must still hold.
#[tokio::test]
async fn course_catalog_lists_a_creator_enrolled_course_once() {
    let (app, db) = app_and_db().await;
    let creator = login_as(&app, &db, "teacher", "teacher").await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let creator_id = me_id(&app, &creator).await;

    let course = create_course(&app, &creator, "algebra").await;
    // Author the exam while still staff, before the demotion below.
    let exam = create_exam(&app, &creator, &course, "mt", "quiz").await;

    // Demote the creator to student, then have a manager enroll them into their
    // own course: they now show up in both the created and enrolled sources.
    set_role(&db, "teacher", "student").await;
    enroll(&app, &boss, &course, &creator_id).await;

    let res = send(&app, "GET", "/courses", Some(&creator), None).await;
    assert_eq!(res.status, StatusCode::OK);
    let courses = common::items(&res.body);
    assert_eq!(courses.len(), 1, "created+enrolled must dedup: {courses:?}");
    assert_eq!(courses[0]["id"], json!(course));

    // The exam catalog derives from the same visible set and must not double
    // the course's exams either.
    let res = send(&app, "GET", "/exams", Some(&creator), None).await;
    let exams = common::items(&res.body);
    assert_eq!(exams.len(), 1, "one exam listed once: {exams:?}");
    assert_eq!(exams[0]["id"], json!(exam));
}

// --- school settings + terms ----------------------------------------------

#[tokio::test]
async fn settings_serve_defaults_and_gate_edits_to_manager() {
    let (app, db) = app_and_db().await;
    let student = login(&app, "set.student").await;
    let teacher = login_as(&app, &db, "set.teacher", "teacher").await;
    let manager = login_as(&app, &db, "set.manager", "manager").await;

    // Reading requires a session; the policy is not public.
    let res = send(&app, "GET", "/settings", None, None).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);

    // Any authenticated user reads; the defaults mirror the old constants,
    // every kind weighing 1 until the school says otherwise.
    let res = send(&app, "GET", "/settings", Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        res.body["exam_kinds"],
        json!([
            {"name": "homework", "weight": 1},
            {"name": "quiz", "weight": 1},
            {"name": "midterm", "weight": 1},
            {"name": "final", "weight": 1},
            {"name": "project", "weight": 1},
            {"name": "oral", "weight": 1},
        ])
    );
    assert_eq!(
        res.body["attendance_statuses"],
        json!(["present", "absent", "late", "excused"])
    );
    assert_eq!(res.body["grade_bands"], json!([]));

    // Students and teachers cannot edit school policy.
    for cookie in [&student, &teacher] {
        let res = send(
            &app,
            "PATCH",
            "/settings",
            Some(cookie),
            Some(json!({ "exam_kinds": [{"name": "lab", "weight": 1}] })),
        )
        .await;
        assert_eq!(res.status, StatusCode::FORBIDDEN);
    }

    // A manager edits one field; omitted fields keep their value.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "exam_kinds": [
            {"name": "lab", "weight": 2},
            {"name": "Quiz", "weight": 1},
        ]})),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        res.body["exam_kinds"],
        json!([
            {"name": "lab", "weight": 2},
            {"name": "Quiz", "weight": 1},
        ])
    );
    assert_eq!(
        res.body["attendance_statuses"],
        json!(["present", "absent", "late", "excused"])
    );

    // Every reader sees the new policy at once.
    let res = send(&app, "GET", "/settings", Some(&student), None).await;
    assert_eq!(res.body["exam_kinds"][0]["name"], "lab");
    assert_eq!(res.body["exam_kinds"][0]["weight"], 2);
}

#[tokio::test]
async fn settings_validation_rejects_bad_lists_and_bands() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "val.manager", "manager").await;

    let bad = [
        json!({ "exam_kinds": [] }), // a school needs at least one kind
        json!({ "exam_kinds": [{"name": "   ", "weight": 1}] }), // blank name
        // Case-insensitive duplicate names.
        json!({ "exam_kinds": [
            {"name": "Lab", "weight": 1},
            {"name": "lab", "weight": 2},
        ]}),
        // Weights are held to 1–100.
        json!({ "exam_kinds": [{"name": "quiz", "weight": 0}] }),
        json!({ "exam_kinds": [{"name": "quiz", "weight": 101}] }),
        json!({ "exam_kinds": [{"name": "quiz", "weight": -1}] }),
        json!({ "attendance_statuses": ["present", "absent", "late"] }), // core dropped
        json!({ "grade_bands": [{ "min": 50, "label": "CC" }] }),        // no band starts at 0
        json!({ "grade_bands": [{ "min": 0, "label": "F" }, { "min": 0, "label": "E" }] }),
        json!({ "grade_bands": [{ "min": -1, "label": "F" }] }),
        json!({ "grade_bands": [{ "min": 101, "label": "A" }] }),
        json!({ "grade_bands": [{ "min": 0, "label": "  " }] }),
    ];
    for body in bad {
        let res = send(
            &app,
            "PATCH",
            "/settings",
            Some(&manager),
            Some(body.clone()),
        )
        .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "should reject {body}");
    }

    // No failed PATCH half-applied anything.
    let res = send(&app, "GET", "/settings", Some(&manager), None).await;
    assert_eq!(res.body["exam_kinds"].as_array().unwrap().len(), 6);
    assert_eq!(res.body["grade_bands"], json!([]));
}

#[tokio::test]
async fn exam_kinds_follow_settings_for_new_writes_only() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "kind.teacher", "teacher").await;
    let manager = login_as(&app, &db, "kind.manager", "manager").await;

    let course = create_course(&app, &teacher, "Chemistry").await;
    let exam = create_exam(&app, &teacher, &course, "Midterm", "midterm").await;

    // The school swaps its kind list wholesale.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "exam_kinds": [{"name": "lab", "weight": 1}] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    // New exams are held to the new list.
    let res = create_exam_with(
        &app,
        &teacher,
        &course,
        json!({ "title": "Retired kind", "kind": "midterm" }),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = create_exam_with(
        &app,
        &teacher,
        &course,
        json!({ "title": "Lab 1", "kind": "lab" }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);

    // The stored exam keeps its retired kind: a title-only PATCH passes…
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "title": "Midterm A" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["kind"], "midterm");
    // …but a kind this request sets is validated against the current list.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "kind": "midterm" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "kind": "lab" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["kind"], "lab");
}

#[tokio::test]
async fn attendance_statuses_follow_settings_and_bucket_in_reports() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "att.manager", "manager").await;
    let student = login(&app, "att.student").await;
    let student_id = me_id(&app, &student).await;

    // Add a school-specific status on top of the mandatory core.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({
            "attendance_statuses": ["present", "absent", "late", "excused", "online"]
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    let res = send(
        &app,
        "POST",
        "/events",
        Some(&manager),
        Some(json!({ "title": "Assembly" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let event = id_of(&res.body);

    // The custom status is markable; garbage still is not.
    let res = send(
        &app,
        "POST",
        &format!("/events/{event}/attendance"),
        Some(&manager),
        Some(json!({ "status": "online", "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(
        &app,
        "POST",
        &format!("/events/{event}/attendance"),
        Some(&manager),
        Some(json!({ "status": "maybe", "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // The report buckets it under `custom` and keeps it out of the rate.
    let res = send(&app, "GET", "/attendance/me", Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["events"]["custom"]["online"], 1);
    assert_eq!(res.body["events"]["total"], 1);
    assert_eq!(res.body["events"]["rate"], serde_json::Value::Null);

    // Retiring the status blocks new marks; the stored row keeps reporting.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({
            "attendance_statuses": ["present", "absent", "late", "excused"]
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(
        &app,
        "POST",
        &format!("/events/{event}/attendance"),
        Some(&manager),
        Some(json!({ "status": "online", "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = send(&app, "GET", "/attendance/me", Some(&student), None).await;
    assert_eq!(res.body["events"]["custom"]["online"], 1);
}

#[tokio::test]
async fn grade_bands_label_the_marks_report() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "band.teacher", "teacher").await;
    let manager = login_as(&app, &db, "band.manager", "manager").await;
    let student = login(&app, "band.student").await;
    let student_id = me_id(&app, &student).await;

    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({
            "grade_bands": [
                { "min": 0, "label": "FF" },
                { "min": 50, "label": "CC" },
                { "min": 85, "label": "AA" },
            ]
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    // Bands echo highest-first — the canonical order.
    assert_eq!(res.body["grade_bands"][0]["label"], "AA");

    let course = create_course(&app, &teacher, "Physics").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = create_exam(&app, &teacher, &course, "Final", "final").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        Some(json!({ "user_id": student_id, "mark": 90 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    let res = send(&app, "GET", "/marks/me", Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK);
    let block = &res.body["courses"][0];
    assert_eq!(block["results"][0]["mark"], 90);
    assert_eq!(block["results"][0]["grade"], "AA");
    assert_eq!(block["average"], 90.0);
    assert_eq!(block["average_grade"], "AA");
    assert_eq!(res.body["overall_grade"], "AA");

    // Clearing the bands turns labels off without touching the numbers.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "grade_bands": [] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(&app, "GET", "/marks/me", Some(&student), None).await;
    assert_eq!(res.body["courses"][0]["results"][0]["mark"], 90);
    assert_eq!(
        res.body["courses"][0]["results"][0]["grade"],
        serde_json::Value::Null
    );
    assert_eq!(res.body["overall_grade"], serde_json::Value::Null);
}

#[tokio::test]
async fn terms_crud_gates_and_links_to_courses() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "term.manager", "manager").await;
    let teacher = login_as(&app, &db, "term.teacher", "teacher").await;
    let student = login(&app, "term.student").await;

    // Writes are manager+; and a term may lie fully in the past — a school
    // adopting the app mid-year backfills its calendar, unlike the no-past
    // rule on exams/lessons/events.
    let past_term = json!({
        "name": "2025 Fall",
        "starts_at": 1_600_000_000_000_i64,
        "ends_at": 1_610_000_000_000_i64,
    });
    let res = send(
        &app,
        "POST",
        "/terms",
        Some(&teacher),
        Some(past_term.clone()),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(&app, "POST", "/terms", Some(&manager), Some(past_term)).await;
    assert_eq!(res.status, StatusCode::CREATED);
    let term = id_of(&res.body);

    // The range stays ordered, merged PATCHes included.
    let res = send(
        &app,
        "POST",
        "/terms",
        Some(&manager),
        Some(json!({ "name": "Broken", "starts_at": 2, "ends_at": 1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = send(
        &app,
        "PATCH",
        &format!("/terms/{term}"),
        Some(&manager),
        Some(json!({ "ends_at": 1_500_000_000_000_i64 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = send(
        &app,
        "PATCH",
        &format!("/terms/{term}"),
        Some(&manager),
        Some(json!({ "name": "2025/26 Fall" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["name"], "2025/26 Fall");

    // Any authenticated user reads the calendar.
    let res = send(&app, "GET", "/terms", Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(common::items(&res.body).len(), 1);

    // Courses link to a term at creation; a bogus id is a 400, not a silent null.
    let res = send(
        &app,
        "POST",
        "/courses",
        Some(&teacher),
        Some(json!({ "title": "History", "term_id": term })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let course = id_of(&res.body);
    assert_eq!(res.body["term"].as_str(), Some(term.as_str()));
    let res = send(
        &app,
        "POST",
        "/courses",
        Some(&teacher),
        Some(json!({ "title": "Broken", "term_id": "nope" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // PATCH: null unlinks, a value re-links, omitting keeps.
    let res = send(
        &app,
        "PATCH",
        &format!("/courses/{course}"),
        Some(&teacher),
        Some(json!({ "term_id": null })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["term"], serde_json::Value::Null);
    let res = send(
        &app,
        "PATCH",
        &format!("/courses/{course}"),
        Some(&teacher),
        Some(json!({ "term_id": term })),
    )
    .await;
    assert_eq!(res.body["term"].as_str(), Some(term.as_str()));
    let res = send(
        &app,
        "PATCH",
        &format!("/courses/{course}"),
        Some(&teacher),
        Some(json!({ "title": "History II" })),
    )
    .await;
    assert_eq!(res.body["term"].as_str(), Some(term.as_str()));

    // A term a course still points at cannot be deleted; unlinking the course
    // clears the way, and the course itself survives untouched.
    let res = send(
        &app,
        "DELETE",
        &format!("/terms/{term}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT);
    let res = send(
        &app,
        "PATCH",
        &format!("/courses/{course}"),
        Some(&teacher),
        Some(json!({ "term_id": null })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(
        &app,
        "DELETE",
        &format!("/terms/{term}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(&app, "GET", &format!("/terms/{term}"), Some(&student), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = send(
        &app,
        "GET",
        &format!("/courses/{course}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["title"], "History II");
    assert_eq!(res.body["term"], serde_json::Value::Null);
}

#[tokio::test]
async fn roll_call_accepts_school_statuses_and_buckets_session_reports() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "roll.manager", "manager").await;
    let teacher = login_as(&app, &db, "roll.teacher", "teacher").await;
    let student = login(&app, "roll.student").await;
    let student_id = me_id(&app, &student).await;

    // The event path is already covered; this proves the second marking
    // path — lesson roll call — reads the same settings list.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({
            "attendance_statuses": ["present", "absent", "late", "excused", "online"]
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    let course = create_course(&app, &teacher, "Biology").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let future = Timestamp::now().as_millis() + 3_600_000;
    let session = create_session(&app, &teacher, &course, future).await;

    // Custom status marks; garbage still dies.
    let res = send(
        &app,
        "POST",
        &format!("/sessions/{session}/attendance"),
        Some(&teacher),
        Some(json!({ "status": "online", "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["status"], "online");
    let res = send(
        &app,
        "POST",
        &format!("/sessions/{session}/attendance"),
        Some(&teacher),
        Some(json!({ "status": "onsite", "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // Both the overall session tally and the per-course block bucket it
    // under `custom`, rate-neutral.
    let res = send(&app, "GET", "/attendance/me", Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["sessions"]["custom"]["online"], 1);
    assert_eq!(res.body["sessions"]["total"], 1);
    assert_eq!(res.body["sessions"]["rate"], serde_json::Value::Null);
    let block = &res.body["courses"][0];
    assert_eq!(block["counts"]["custom"]["online"], 1);
    assert_eq!(block["counts"]["rate"], serde_json::Value::Null);
}

#[tokio::test]
async fn terms_list_newest_first_require_auth_and_guard_delete_per_term() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "bulk.manager", "manager").await;
    let teacher = login_as(&app, &db, "bulk.teacher", "teacher").await;

    // The calendar is not public.
    let res = send(&app, "GET", "/terms", None, None).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED);

    let older = send(
        &app,
        "POST",
        "/terms",
        Some(&manager),
        Some(json!({ "name": "2025 Fall", "starts_at": 1_000, "ends_at": 2_000 })),
    )
    .await;
    let newer = send(
        &app,
        "POST",
        "/terms",
        Some(&manager),
        Some(json!({ "name": "2026 Spring", "starts_at": 5_000, "ends_at": 6_000 })),
    )
    .await;
    let (older, newer) = (id_of(&older.body), id_of(&newer.body));

    // Newest (by starts_at) first.
    let res = send(&app, "GET", "/terms", Some(&teacher), None).await;
    let names: Vec<&str> = common::items(&res.body)
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["2026 Spring", "2025 Fall"]);

    // The delete guard counts EVERY linked course — unlinking one of two is
    // not enough — and it is per term: a course on another term never blocks.
    let mut linked = Vec::new();
    for title in ["Algebra", "Geometry"] {
        let res = send(
            &app,
            "POST",
            "/courses",
            Some(&teacher),
            Some(json!({ "title": title, "term_id": newer })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED);
        linked.push(id_of(&res.body));
    }
    let res = send(
        &app,
        "POST",
        "/courses",
        Some(&teacher),
        Some(json!({ "title": "History", "term_id": older })),
    )
    .await;
    let unrelated = id_of(&res.body);

    let res = send(
        &app,
        "DELETE",
        &format!("/terms/{newer}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT);

    for (i, course) in linked.iter().enumerate() {
        let res = send(
            &app,
            "PATCH",
            &format!("/courses/{course}"),
            Some(&teacher),
            Some(json!({ "term_id": null })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK);
        // Still blocked until the LAST linked course lets go.
        let res = send(
            &app,
            "DELETE",
            &format!("/terms/{newer}"),
            Some(&manager),
            None,
        )
        .await;
        let expected = if i + 1 == linked.len() {
            StatusCode::NO_CONTENT
        } else {
            StatusCode::CONFLICT
        };
        assert_eq!(res.status, expected, "after unlinking {course}");
    }
    let res = send(
        &app,
        "GET",
        &format!("/courses/{unrelated}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["term"].as_str(), Some(older.as_str()));
}

#[tokio::test]
async fn grade_band_boundary_applies_to_the_weighted_average() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "edge.teacher", "teacher").await;
    let manager = login_as(&app, &db, "edge.manager", "manager").await;
    let student = login(&app, "edge.student").await;
    let student_id = me_id(&app, &student).await;

    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({
            "grade_bands": [{ "min": 0, "label": "FF" }, { "min": 85, "label": "AA" }]
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    // Two equal-weight marks straddling the band edge: 80 and 90 → average
    // exactly 85.0, which is INSIDE the AA band (min is inclusive), while
    // the 80 itself still reads FF.
    let course = create_course(&app, &teacher, "Calculus").await;
    enroll(&app, &teacher, &course, &student_id).await;
    for (title, mark) in [("Quiz A", 80), ("Quiz B", 90)] {
        let exam = create_exam(&app, &teacher, &course, title, "quiz").await;
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/results"),
            Some(&teacher),
            Some(json!({ "user_id": student_id, "mark": mark })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK);
    }

    let res = send(&app, "GET", "/marks/me", Some(&student), None).await;
    let block = &res.body["courses"][0];
    assert_eq!(block["average"], 85.0);
    assert_eq!(block["average_grade"], "AA");
    let grades: Vec<(&str, &str)> = block["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["title"].as_str().unwrap(), r["grade"].as_str().unwrap()))
        .collect();
    assert!(grades.contains(&("Quiz A", "FF")));
    assert!(grades.contains(&("Quiz B", "AA")));
    assert_eq!(res.body["overall_grade"], "AA");
}

/// Two managers patch DIFFERENT fields concurrently. Neither edit may be lost
/// and neither request may 500 — the compare-and-set loop absorbs the
/// conflict and re-merges (the guard `save_if_unchanged` exists for).
#[tokio::test]
async fn concurrent_settings_patches_both_land() {
    for _round in 0..10 {
        let (app, db) = app_and_db().await;
        let mavi = login_as(&app, &db, "mavi", "manager").await;
        let kara = login_as(&app, &db, "kara", "manager").await;

        let (a, b) = tokio::join!(
            send(
                &app,
                "PATCH",
                "/settings",
                Some(&mavi),
                Some(
                    json!({ "grade_bands": [ { "min": 0, "label": "F" }, { "min": 50, "label": "P" } ] })
                ),
            ),
            send(
                &app,
                "PATCH",
                "/settings",
                Some(&kara),
                Some(json!({ "max_file_bytes": 4096 })),
            ),
        );
        assert_eq!(a.status, StatusCode::OK, "bands patch: {}", a.body);
        assert_eq!(b.status, StatusCode::OK, "bytes patch: {}", b.body);

        let after = send(&app, "GET", "/settings", Some(&mavi), None).await;
        assert_eq!(after.body["max_file_bytes"], 4096, "{}", after.body);
        assert_eq!(
            after.body["grade_bands"].as_array().unwrap().len(),
            2,
            "both concurrent edits must survive: {}",
            after.body
        );
    }
}

#[tokio::test]
async fn settings_accept_admin_edits_trim_entries_and_noop_on_empty_patch() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "adm.admin", "admin").await;

    // An empty PATCH is a valid no-op: current policy back, nothing changed.
    let res = send(&app, "PATCH", "/settings", Some(&admin), Some(json!({}))).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["exam_kinds"].as_array().unwrap().len(), 6);

    // Admin clears the manager bar (hierarchy, not equality), and kind names
    // arrive trimmed on the wire.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&admin),
        Some(json!({ "exam_kinds": [
            {"name": "  lab  ", "weight": 2},
            {"name": "quiz", "weight": 1},
        ]})),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        res.body["exam_kinds"],
        json!([
            {"name": "lab", "weight": 2},
            {"name": "quiz", "weight": 1},
        ])
    );
}

/// The food-program knobs are `option<…>` columns, and every settings save
/// rewrites the whole row — so a PATCH that never mentions them must still
/// round-trip them. (A `DEFAULT []` column plus a struct that dropped the field
/// aborts the transaction with a coercion error instead.)
#[tokio::test]
async fn settings_round_trip_the_food_program_knobs() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "meal.manager", "manager").await;

    // Unset columns read as the built-in seeds; an absent cutoff is no cutoff.
    let res = send(&app, "GET", "/settings", Some(&manager), None).await;
    assert_eq!(
        res.body["meal_slots"],
        json!([
            {"name": "breakfast", "serving_minute": null},
            {"name": "lunch", "serving_minute": null},
            {"name": "snack", "serving_minute": null},
        ]),
        "the shipped defaults carry no serving time, so the cutoff keeps its \
         midnight-UTC meaning until a school sets real hours"
    );
    assert_eq!(
        res.body["dietary_tags"],
        json!([
            "vegetarian",
            "vegan",
            "gluten_free",
            "lactose_free",
            "nut_allergy"
        ])
    );
    assert_eq!(res.body["meal_cancel_cutoff_minutes"], json!(null));

    // A PATCH of an unrelated field still writes the whole row.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "max_file_bytes": 4096 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["meal_slots"].as_array().unwrap().len(), 3);

    // All three set at once; slot names arrive trimmed.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({
            "meal_slots": [
                {"name": "lunch", "serving_minute": 720},
                {"name": "  snack "},
            ],
            "dietary_tags": ["vegan"],
            "meal_cancel_cutoff_minutes": 120,
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        res.body["meal_slots"],
        json!([
            {"name": "lunch", "serving_minute": 720},
            {"name": "snack", "serving_minute": null},
        ]),
        "slots round-trip with and without a serving time"
    );
    assert_eq!(res.body["dietary_tags"], json!(["vegan"]));
    assert_eq!(res.body["meal_cancel_cutoff_minutes"], 120);

    // A later PATCH that omits them keeps them — now over a row that really
    // carries the columns.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "chatbot_history_turns": 3 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["meal_slots"].as_array().unwrap().len(), 2);
    assert_eq!(res.body["meal_cancel_cutoff_minutes"], 120);

    // `null` clears the cutoff (no cutoff at all); the lists stay.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "meal_cancel_cutoff_minutes": null })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["meal_cancel_cutoff_minutes"], json!(null));
    assert_eq!(res.body["meal_slots"].as_array().unwrap().len(), 2);

    // Bounds hold on the way in.
    for body in [
        json!({ "meal_cancel_cutoff_minutes": 10_081 }),
        json!({ "meal_cancel_cutoff_minutes": -1 }),
        json!({ "meal_slots": [{"name": "   "}] }),
        json!({ "meal_slots": [{"name": "Lunch"}, {"name": "lunch"}] }),
        json!({ "meal_slots": [{"name": "lunch", "serving_minute": 1440}] }),
        json!({ "meal_slots": [{"name": "lunch", "serving_minute": -1}] }),
        json!({ "dietary_tags": ["vegan", "VEGAN"] }),
    ] {
        let res = send(
            &app,
            "PATCH",
            "/settings",
            Some(&manager),
            Some(body.clone()),
        )
        .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "should reject {body}");
    }

    // A published menu pins the slot it snapshotted: dropping it is a 409.
    let menu = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&manager),
        Some(json!({ "date": "2099-07-27", "slot": "lunch" })),
    )
    .await;
    assert_eq!(menu.status, StatusCode::CREATED, "{}", menu.body);
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "meal_slots": [{"name": "snack"}] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT);
    // Keeping it is fine; an unused slot still leaves freely.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "meal_slots": [{"name": "lunch"}, {"name": "dinner"}] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        res.body["meal_slots"],
        json!([
            {"name": "lunch", "serving_minute": null},
            {"name": "dinner", "serving_minute": null},
        ])
    );
}

// --- question images ---------------------------------------------------------

/// POST `bytes` as a multipart image upload to `path`; returns (status, json).
async fn post_image(
    app: &axum::Router,
    cookie: &str,
    path: &str,
    content_type: &str,
    bytes: &[u8],
) -> (StatusCode, serde_json::Value) {
    let (status, _, body) = common::send_raw(
        app,
        "POST",
        path,
        Some(cookie),
        Some("multipart/form-data; boundary=hezarfen-test-boundary"),
        common::multipart_file("pic.png", content_type, bytes),
    )
    .await;
    let body = if body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null)
    };
    (status, body)
}

/// Every stored question-image blob name, straight from the table.
async fn image_blob_keys(db: &hezarfen_backend::database::Database) -> Vec<String> {
    let mut result = db
        .query("SELECT VALUE file FROM question_image")
        .await
        .unwrap()
        .check()
        .unwrap();
    result.take::<Vec<String>>(0).unwrap()
}

/// Every stored bank-question-image blob name, straight from the table.
async fn bank_image_blob_keys(db: &hezarfen_backend::database::Database) -> Vec<String> {
    let mut result = db
        .query("SELECT VALUE file FROM bank_question_image")
        .await
        .unwrap()
        .check()
        .unwrap();
    result.take::<Vec<String>>(0).unwrap()
}

/// Every stored answer-image (student drawing) blob name, straight from the table.
async fn answer_image_blob_keys(db: &hezarfen_backend::database::Database) -> Vec<String> {
    let mut result = db
        .query("SELECT VALUE file FROM answer_image")
        .await
        .unwrap()
        .check()
        .unwrap();
    result.take::<Vec<String>>(0).unwrap()
}

/// The full life of question images: teacher uploads (question + choice slots,
/// rasters only, choice slots bounded), metadata on both question views,
/// bytes behind the sitting wall, replace swaps the blob, a choices PATCH
/// drops the option pictures, and every delete path takes the blobs with it.
#[tokio::test]
async fn question_images_author_serve_and_cascade() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "img_t", "teacher").await;
    let student = login(&app, "img_s").await;
    let student_id = me_id(&app, &student).await;
    let outsider = login(&app, "img_o").await;

    let course = create_course(&app, &teacher, "geography").await;
    let subject = create_subject(&app, &teacher, &course, "maps").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "map quiz", "kind": "quiz", "mode": "open" }),
    )
    .await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Which city is marked?", "kind": "choice", "points": 10,
                "choices": [{"id": "c0", "text": "Ankara"}, {"id": "c1", "text": "İzmir"}], "correct": "c0" }),
    )
    .await;
    let question = id_of(&question_body);
    let essay = create_question(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Describe the marked region.", "kind": "text", "points": 10 }),
    )
    .await;

    let q_image = format!("/exams/{exam}/questions/{question}/image");
    let png = b"png-bytes-question".as_slice();

    // SVG is out (script risk); unknown types are out; rasters land.
    let (status, body) = post_image(&app, &teacher, &q_image, "image/svg+xml", b"<svg/>").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = post_image(&app, &teacher, &q_image, "application/pdf", b"%PDF").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = post_image(&app, &teacher, &q_image, "image/png", png).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["content_type"], "image/png");
    assert_eq!(body["size"], png.len() as i64);

    // A classical (text) question takes an illustration too — but no option
    // pictures, and no out-of-range slot anywhere.
    let (status, body) = post_image(
        &app,
        &teacher,
        &format!("/exams/{exam}/questions/{essay}/image"),
        "image/jpeg",
        b"jpeg-essay",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!(
            "/exams/{exam}/questions/{essay}/choices/{}/image",
            choice_of(&question_body, 0)
        ),
        "image/png",
        b"x",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "text questions take no option pictures"
    );
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/exams/{exam}/questions/{question}/choices/01NOTACHOICEOFTHISQUEST/image"),
        "image/png",
        b"x",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "slot must name a choice");

    // Option picture on choice 1, and the author view carries all the metas.
    let choice_image = format!(
        "/exams/{exam}/questions/{question}/choices/{}/image",
        choice_of(&question_body, 1)
    );
    let (status, _) = post_image(&app, &teacher, &choice_image, "image/webp", b"webp-choice").await;
    assert_eq!(status, StatusCode::CREATED);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let listed = &common::items(&res.body)[0];
    assert_eq!(listed["image"]["content_type"], "image/png");
    assert!(listed["choice_images"][0].is_null());
    assert_eq!(listed["choice_images"][1]["content_type"], "image/webp");

    // Replace swaps the blob under the same slot: one row, fresh bytes.
    let keys_before = image_blob_keys(&db).await;
    let png2 = b"png-bytes-question-v2".as_slice();
    let (status, body) = post_image(&app, &teacher, &q_image, "image/png", png2).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["size"], png2.len() as i64);
    let keys_after = image_blob_keys(&db).await;
    assert_eq!(keys_before.len(), keys_after.len(), "replace adds no row");
    for stale in keys_before.iter().filter(|k| !keys_after.contains(k)) {
        assert!(
            !common::blob_dir().join(stale).exists(),
            "replaced blob lingers on disk"
        );
    }

    // The bytes sit behind the sitting wall: outsiders 403, enrolled students
    // 404 until an attempt exists, then 200 with the guard headers on.
    let (status, _, _) =
        common::send_raw(&app, "GET", &q_image, Some(&outsider), None, Vec::new()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) =
        common::send_raw(&app, "GET", &q_image, Some(&student), None, Vec::new()).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "no early peek before the attempt"
    );
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let (status, headers, bytes) =
        common::send_raw(&app, "GET", &q_image, Some(&student), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, png2);
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["cache-control"], "private, no-store");
    let (status, _, bytes) =
        common::send_raw(&app, "GET", &choice_image, Some(&student), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"webp-choice");

    // The sitting view embeds the same metadata the author sees (sans key).
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt/questions"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body[0]["image"]["content_type"], "image/png");
    assert_eq!(
        res.body[0]["choice_images"][1]["content_type"],
        "image/webp"
    );
    assert_eq!(res.body[1]["image"]["content_type"], "image/jpeg");
    assert!(res.body[1]["choice_images"].is_null());

    // Images freeze with the rest of the question once attempts exist.
    let (status, body) = post_image(&app, &teacher, &q_image, "image/png", b"late").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let res = send(&app, "DELETE", &q_image, Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::CONFLICT);

    // Deleting the exam takes every image row and blob with it.
    let keys = image_blob_keys(&db).await;
    assert_eq!(keys.len(), 3);
    let res = send(
        &app,
        "DELETE",
        &format!("/exams/{exam}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert!(
        image_blob_keys(&db).await.is_empty(),
        "rows survived the cascade"
    );
    for key in &keys {
        assert!(
            !common::blob_dir().join(key).exists(),
            "blob {key} survived the exam delete"
        );
    }
}

/// The full life of a student's answer drawing: the student is the uploader
/// (rasters only), the bytes sit behind the sitting wall (own GET 404 before an
/// attempt, 200 after with the guard headers), the drawing meta rides both the
/// student's own sitting view and the teacher grading sheet, the teacher reads
/// the bytes via `/attempts/{user}/...` while an outsider cannot, a retake wipes
/// the row *and* its on-disk blob, and deleting the exam takes rows + blobs.
#[tokio::test]
async fn student_answer_images_serve_and_cascade() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ans_t", "teacher").await;
    let student = login(&app, "ans_s").await;
    let student_id = me_id(&app, &student).await;
    let outsider = login(&app, "ans_o").await;

    let course = create_course(&app, &teacher, "science").await;
    let subject = create_subject(&app, &teacher, &course, "biology").await;
    enroll(&app, &teacher, &course, &student_id).await;
    // Retakes allowed (max_attempts: 2) so the wipe path is reachable.
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "cell quiz", "kind": "quiz", "mode": "open", "max_attempts": 2 }),
    )
    .await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Draw a cell.", "kind": "text", "points": 10 }),
    )
    .await;
    let question = id_of(&question_body);

    let own = format!("/exams/{exam}/attempt/answers/{question}/image");
    let png = b"png-drawing".as_slice();

    // Before an attempt exists there is nothing to write into and no early
    // peek: the write path 404s (no attempt), the own-read 404s too.
    let (status, _) = post_image(&app, &student, &own, "image/png", png).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "no attempt yet");
    let (status, _, _) =
        common::send_raw(&app, "GET", &own, Some(&student), None, Vec::new()).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "no early peek before the attempt"
    );

    // The student sits the exam.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    // The raster wall holds on the student's own upload: SVG/PDF out, rasters in.
    let (status, body) = post_image(&app, &student, &own, "image/svg+xml", b"<svg/>").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = post_image(&app, &student, &own, "application/pdf", b"%PDF").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    for ct in ["image/png", "image/jpeg", "image/webp"] {
        let (status, body) = post_image(&app, &student, &own, ct, png).await;
        assert_eq!(status, StatusCode::CREATED, "{ct}: {body}");
        assert_eq!(body["content_type"], ct);
        assert_eq!(body["size"], png.len() as i64);
    }
    // Same (question, user) slot: three uploads collapse to one upserted row.
    assert_eq!(
        answer_image_blob_keys(&db).await.len(),
        1,
        "own slot upserts, adds no row"
    );

    // The student reads their own drawing back: 200, the guard headers, the
    // bytes, and the content type of the last upload (webp).
    let (status, headers, bytes) =
        common::send_raw(&app, "GET", &own, Some(&student), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, png);
    assert_eq!(headers["content-type"], "image/webp");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["cache-control"], "private, no-store");

    // An outsider (not enrolled) can't read the student's own route.
    let (status, _, _) =
        common::send_raw(&app, "GET", &own, Some(&outsider), None, Vec::new()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "outsiders can't peek");

    // The drawing rides the student's own answer state — save the text answer
    // it accompanies, then the sitting view embeds the drawing meta.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": question, "text": "a cell, drawn and described" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt/questions"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        res.body[0]["answer"]["answer_image"]["content_type"],
        "image/webp"
    );

    // The teacher grading sheet carries the drawing meta and serves the bytes
    // via the teacher route; the sheet row also exposes the flag.
    let teacher_bytes = format!("/exams/{exam}/attempts/{student_id}/answers/{question}/image");
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempts/{student_id}/answers"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        res.body["answers"][0]["answer_image"]["content_type"],
        "image/webp"
    );
    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &teacher_bytes,
        Some(&teacher),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, png);
    // An outsider student can't read the teacher grading route (not a teacher).
    let (status, _, _) = common::send_raw(
        &app,
        "GET",
        &teacher_bytes,
        Some(&outsider),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A retake keeps the prior sitting's sheet — per-attempt history — so the
    // seq-1 answer-image row *and* its on-disk blob survive the new sitting.
    let pre = answer_image_blob_keys(&db).await;
    assert_eq!(pre.len(), 1);
    assert!(common::blob_dir().join(&pre[0]).exists());
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "retake: {}", res.body);
    assert_eq!(res.body["attempt"], 2, "the retake is the second sitting");
    assert_eq!(
        answer_image_blob_keys(&db).await.len(),
        1,
        "the retake kept the prior sitting's answer-image row (history)"
    );
    assert!(
        common::blob_dir().join(&pre[0]).exists(),
        "the retake kept the prior sitting's answer-image blob on disk"
    );

    // A fresh drawing in the new sitting adds a second row alongside the kept
    // one; deleting the exam then sweeps every sitting's rows and blobs.
    let (status, _) = post_image(&app, &student, &own, "image/png", png).await;
    assert_eq!(status, StatusCode::CREATED);
    let keys = answer_image_blob_keys(&db).await;
    assert_eq!(keys.len(), 2, "seq-1 and seq-2 drawings coexist");
    let res = send(
        &app,
        "DELETE",
        &format!("/exams/{exam}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert!(
        answer_image_blob_keys(&db).await.is_empty(),
        "answer-image rows survived the exam delete"
    );
    for key in &keys {
        assert!(
            !common::blob_dir().join(key).exists(),
            "answer-image blob {key} survived the exam delete"
        );
    }
}

/// A student who draws but types nothing still has their drawing surface in
/// both the sitting view and the teacher grading sheet — the upload creates the
/// backing (blank-text) answer row, since `answer_image` rides the answer payload.
#[tokio::test]
async fn answer_image_surfaces_for_a_drawing_only_answer() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "draw_t", "teacher").await;
    let student = login(&app, "draw_s").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "art").await;
    let subject = create_subject(&app, &teacher, &course, "drawing").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "sketch", "kind": "quiz", "mode": "open" }),
    )
    .await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Draw a triangle.", "kind": "text", "points": 5 }),
    )
    .await;
    let question = id_of(&question_body);

    // Sit, then upload a drawing WITHOUT ever saving a text answer.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let own = format!("/exams/{exam}/attempt/answers/{question}/image");
    let (status, body) = post_image(&app, &student, &own, "image/png", b"tri").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // The sitting view carries the drawing meta with an empty typed answer.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt/questions"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        res.body[0]["answer"]["answer_image"]["content_type"],
        "image/png"
    );
    assert_eq!(res.body[0]["answer"]["text"], "");

    // The teacher grading sheet shows it too.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempts/{student_id}/answers"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        res.body["answers"][0]["answer_image"]["content_type"],
        "image/png"
    );

    // Removing the drawing drops the blank answer row it created — the question
    // stops showing as answered rather than lingering as an empty text answer.
    let (status, _, _) =
        common::send_raw(&app, "DELETE", &own, Some(&student), None, Vec::new()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/attempt/questions"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(
        res.body[0]["answer"].is_null(),
        "blank answer row dropped with its drawing"
    );
}

/// The authoring edits that reshape a question also groom its images — and
/// **only** the pictures of options that are actually gone. Reordering the list
/// and deleting one option leaves every surviving option's picture attached to
/// *that option*, and leaves `correct` naming the option the author marked.
/// This is the behaviour stable choice ids exist for: the images used to be
/// keyed by position, so any edit that carried a `choices` key wiped all of
/// them.
#[tokio::test]
async fn question_image_edits_follow_the_choices() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "imge_t", "teacher").await;
    let course = create_course(&app, &teacher, "history").await;
    let subject = create_subject(&app, &teacher, &course, "eras").await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "eras", "kind": "quiz", "mode": "open" }),
    )
    .await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Pick the era.", "kind": "choice", "points": 5,
                "choices": [{"id": "c0", "text": "Stone"}, {"id": "c1", "text": "Bronze"},
                            {"id": "c2", "text": "Iron"}], "correct": "c2" }),
    )
    .await;
    let question = id_of(&question_body);
    let (stone, bronze, iron) = (
        choice_of(&question_body, 0),
        choice_of(&question_body, 1),
        choice_of(&question_body, 2),
    );
    let base = format!("/exams/{exam}/questions/{question}");
    for (path, bytes) in [
        (format!("{base}/image"), b"illustration".as_slice()),
        (format!("{base}/choices/{stone}/image"), b"stone".as_slice()),
        (
            format!("{base}/choices/{bronze}/image"),
            b"bronze".as_slice(),
        ),
        (format!("{base}/choices/{iron}/image"), b"iron".as_slice()),
    ] {
        let (status, body) = post_image(&app, &teacher, &path, "image/png", bytes).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    // A points-only PATCH touches no images.
    let res = send(
        &app,
        "PATCH",
        &base,
        Some(&teacher),
        Some(json!({ "points": 7 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body["image"].is_object());
    for slot in 0..3 {
        assert!(res.body["choice_images"][slot].is_object());
    }
    assert_eq!(res.body["correct"], iron, "the answer key must not move");

    // The edit this whole remodel exists for: reorder the list (Iron first),
    // rename one option, and drop Bronze entirely.
    let keys_before = image_blob_keys(&db).await;
    let res = send(
        &app,
        "PATCH",
        &base,
        Some(&teacher),
        Some(json!({ "choices": [
            {"id": iron, "text": "Iron Age"},
            {"id": stone, "text": "Stone"},
        ], "correct": iron })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    // The surviving options kept their identities, in their new order…
    assert_eq!(res.body["choices"][0]["id"], iron);
    assert_eq!(res.body["choices"][0]["text"], "Iron Age");
    assert_eq!(res.body["choices"][1]["id"], stone);
    // …the answer key still names the option the author marked, though it moved
    // from position 2 to position 0…
    assert_eq!(res.body["correct"], iron, "correct must follow its option");
    // …and each survivor kept *its own* picture, illustration included.
    assert!(res.body["image"].is_object(), "illustration must survive");
    assert!(
        res.body["choice_images"][0].is_object(),
        "Iron kept its picture across the reorder"
    );
    assert!(
        res.body["choice_images"][1].is_object(),
        "Stone kept its picture across the reorder"
    );

    // Exactly one blob went: the removed option's.
    let keys_after = image_blob_keys(&db).await;
    assert_eq!(
        keys_after.len(),
        3,
        "illustration + two surviving option pictures"
    );
    let dropped: Vec<_> = keys_before
        .iter()
        .filter(|k| !keys_after.contains(k))
        .collect();
    assert_eq!(dropped.len(), 1, "only the removed option's blob may go");
    for key in dropped {
        assert!(
            !common::blob_dir().join(key).exists(),
            "dropped option blob lingers"
        );
    }

    // The remaining wipe path, now opt-in: a list of all-new options (no id
    // matches a stored one) is a genuinely new set, so every option picture
    // goes — and the illustration still stays.
    let res = send(
        &app,
        "PATCH",
        &base,
        Some(&teacher),
        Some(json!({ "choices": [{"id": "fresh-a", "text": "Iron"}, {"id": "fresh-b", "text": "Stone"}],
                     "correct": "fresh-a" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_ne!(res.body["choices"][0]["id"], iron, "a new option, not Iron");
    assert_eq!(
        res.body["choice_images"],
        json!([null, null]),
        "an all-new option list keeps no pictures"
    );
    assert!(res.body["image"].is_object(), "illustration must survive");
    assert_eq!(image_blob_keys(&db).await.len(), 1);

    // Explicit image DELETE: 204 once, 404 after, blob gone.
    let fresh = choice_of(&res.body, 0);
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("{base}/choices/{fresh}/image"),
        "image/gif",
        b"gif",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let res = send(
        &app,
        "DELETE",
        &format!("{base}/choices/{fresh}/image"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "DELETE",
        &format!("{base}/choices/{fresh}/image"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // Deleting the question sweeps its remaining image rows and blobs.
    let keys = image_blob_keys(&db).await;
    assert_eq!(keys.len(), 1);
    let res = send(&app, "DELETE", &base, Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert!(image_blob_keys(&db).await.is_empty());
    for key in &keys {
        assert!(
            !common::blob_dir().join(key).exists(),
            "blob {key} survived the question delete"
        );
    }
}

// --- exam drafts ---------------------------------------------------------

#[tokio::test]
async fn draft_exams_hide_from_students_until_published() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "draft_t", "teacher").await;
    let student = login(&app, "draft_s").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "history").await;
    enroll(&app, &teacher, &course, &student_id).await;

    // Saved as a draft: an open (sittable-were-it-published) exam.
    let res = create_exam_with(
        &app,
        &teacher,
        &course,
        json!({ "title": "wip final", "kind": "final", "mode": "open", "draft": true }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["draft"], true);
    let exam = id_of(&res.body);

    // The manager sees it everywhere, flagged.
    let mine = send(&app, "GET", "/exams", Some(&teacher), None).await;
    assert_eq!(common::items(&mine.body).len(), 1);
    assert_eq!(common::items(&mine.body)[0]["draft"], true);

    // The enrolled student sees nothing: not in either list, a 404 on direct
    // read, and a 404 (not a telltale 409) on sitting.
    let listed = send(&app, "GET", "/exams", Some(&student), None).await;
    assert_eq!(common::items(&listed.body).len(), 0);
    let in_course = send(
        &app,
        "GET",
        &format!("/courses/{course}/exams"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(common::items(&in_course.body).len(), 0);
    for (method, path) in [
        ("GET", format!("/exams/{exam}")),
        ("POST", format!("/exams/{exam}/attempt")),
    ] {
        let res = send(&app, method, &path, Some(&student), None).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{method} {path}");
    }

    // Grading a draft is refused — a mark must never point at a hidden exam.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        Some(json!({ "mark": 90, "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // Publish. The student now sees and sits it.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "draft": false })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["draft"], false);
    let listed = send(&app, "GET", "/exams", Some(&student), None).await;
    assert_eq!(common::items(&listed.body).len(), 1);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    // Once sat, the exam can't be pulled back into hiding.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "draft": true })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
}

#[tokio::test]
async fn results_alone_also_freeze_re_drafting() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "redraft_t", "teacher").await;
    let student = login(&app, "redraft_s").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "geo").await;
    enroll(&app, &teacher, &course, &student_id).await;

    // A published offline-graded exam (no mode, no attempts — marks by hand).
    let exam = create_exam(&app, &teacher, &course, "field trip report", "project").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        Some(json!({ "mark": 75, "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // No attempts exist, but the graded result freezes re-drafting too.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "draft": true })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // Ungraded siblings still re-draft freely.
    let other = create_exam(&app, &teacher, &course, "map quiz", "quiz").await;
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{other}"),
        Some(&teacher),
        Some(json!({ "draft": true })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["draft"], true);
}

// --- parent role ---------------------------------------------------------

/// The tie lifecycle: admin-only, role-checked on both ends, idempotent, and
/// symmetric between the admin listing and the parent's own `/me/students`.
#[tokio::test]
async fn parent_links_are_admin_managed_and_role_checked() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let parent = login_as(&app, &db, "mom", "parent").await;
    let parent_id = me_id(&app, &parent).await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;
    let teacher_id = me_id(&app, &teacher).await;

    // Only an admin ties.
    let res = send(
        &app,
        "POST",
        &format!("/users/{parent_id}/students"),
        Some(&teacher),
        Some(json!({ "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // Both ends are role-checked: the target must be a parent…
    let res = send(
        &app,
        "POST",
        &format!("/users/{teacher_id}/students"),
        Some(&admin),
        Some(json!({ "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    // …the student must be a student…
    let res = send(
        &app,
        "POST",
        &format!("/users/{parent_id}/students"),
        Some(&admin),
        Some(json!({ "user_id": teacher_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    // …and both must exist.
    let res = send(
        &app,
        "POST",
        &format!("/users/{parent_id}/students"),
        Some(&admin),
        Some(json!({ "user_id": "01ZZZZZZZZZZZZZZZZZZZZZZZZ" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    let res = send(
        &app,
        "POST",
        "/users/01ZZZZZZZZZZZZZZZZZZZZZZZZ/students",
        Some(&admin),
        Some(json!({ "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // A valid tie lands, and linking again is a no-op returning the same tie.
    for _ in 0..2 {
        let res = send(
            &app,
            "POST",
            &format!("/users/{parent_id}/students"),
            Some(&admin),
            Some(json!({ "user_id": ali_id })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        assert_eq!(res.body["student"]["id"], json!(ali_id));
        assert_eq!(res.body["parent"]["id"], json!(parent_id));
    }

    // Admin listing and the parent's own listing agree.
    let res = send(
        &app,
        "GET",
        &format!("/users/{parent_id}/students"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(common::total(&res.body), 1);
    let res = send(&app, "GET", "/users/me/students", Some(&parent), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(common::items(&res.body)[0]["id"], json!(ali_id));
    // /me/students is the parent's view — everyone else is refused.
    let res = send(&app, "GET", "/users/me/students", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // Untie: once, then the tie is gone.
    let res = send(
        &app,
        "DELETE",
        &format!("/users/{parent_id}/students/{ali_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "DELETE",
        &format!("/users/{parent_id}/students/{ali_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

/// The tie is the parent's whole power: linked students' reports open up (in
/// full — no course narrowing), everyone else's stay walled, and every write
/// a parent might try is refused.
#[tokio::test]
async fn parent_observes_linked_students_and_nothing_else() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let parent = login_as(&app, &db, "mom", "parent").await;
    let parent_id = me_id(&app, &parent).await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;
    let bob = login(&app, "bob").await;
    let bob_id = me_id(&app, &bob).await;

    // ali gets a graded exam in a course the parent has nothing to do with.
    let course = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course, &ali_id).await;
    let exam = create_exam(&app, &teacher, &course, "midterm", "quiz").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        Some(json!({ "mark": 70, "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let res = send(
        &app,
        "POST",
        &format!("/users/{parent_id}/students"),
        Some(&admin),
        Some(json!({ "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Linked student: all three reports open, and the mark report is FULL —
    // the parent manages no courses, so narrowing would blank it.
    let res = send(
        &app,
        "GET",
        &format!("/marks/{ali_id}"),
        Some(&parent),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["courses"][0]["results"][0]["mark"], 70);
    let res = send(
        &app,
        "GET",
        &format!("/attendance/{ali_id}"),
        Some(&parent),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/pomodoro/{ali_id}"),
        Some(&parent),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Unlinked student: every report stays walled.
    for uri in [
        format!("/marks/{bob_id}"),
        format!("/attendance/{bob_id}"),
        format!("/pomodoro/{bob_id}"),
    ] {
        let res = send(&app, "GET", &uri, Some(&parent), None).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{uri} must stay walled");
    }

    // A parent writes nothing: not a student (pomodoro, sitting), not staff
    // (courses, grading, search), and the tie grants no write-through.
    let res = send(&app, "POST", "/pomodoro/start", Some(&parent), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "POST",
        "/courses",
        Some(&parent),
        Some(json!({ "title": "sneaky" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    // A sittable exam (the modeless `exam` would 409 as unsittable before any
    // role question arises): the student-only wall answers first.
    let sittable = create_exam_with(
        &app,
        &teacher,
        &course,
        json!({ "title": "anytime", "kind": "quiz", "mode": "open" }),
    )
    .await;
    assert_eq!(sittable.status, StatusCode::CREATED, "{}", sittable.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{}/attempt", id_of(&sittable.body)),
        Some(&parent),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    // Search is open to a parent since #24, but only onto staff: the linked
    // student is a match by name and still must not come back.
    let res = send(&app, "GET", "/users/search?q=ali", Some(&parent), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(common::items(&res.body).len(), 0, "{}", res.body);
    assert_eq!(res.body["total"], 0);
}

/// A role change off either end of a tie sweeps it, exactly like promotion
/// sweeps enrollments — no dead read grants left behind.
#[tokio::test]
async fn role_change_sweeps_parent_links() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let parent = login_as(&app, &db, "mom", "parent").await;
    let parent_id = me_id(&app, &parent).await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;

    let res = send(
        &app,
        "POST",
        &format!("/users/{parent_id}/students"),
        Some(&admin),
        Some(json!({ "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // The student side leaves the student role: the tie dies with it.
    let res = send(
        &app,
        "PATCH",
        &format!("/users/{ali_id}/role"),
        Some(&admin),
        Some(json!({ "role": "teacher" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(&app, "GET", "/users/me/students", Some(&parent), None).await;
    assert_eq!(common::total(&res.body), 0, "tie must die with the role");
    let res = send(
        &app,
        "GET",
        &format!("/marks/{ali_id}"),
        Some(&parent),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // The parent side leaves the parent role: its remaining ties die too.
    let bob = login(&app, "bob").await;
    let bob_id = me_id(&app, &bob).await;
    let res = send(
        &app,
        "POST",
        &format!("/users/{parent_id}/students"),
        Some(&admin),
        Some(json!({ "user_id": bob_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/users/{parent_id}/role"),
        Some(&admin),
        Some(json!({ "role": "student" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Really deleted, not merely hidden by the role checks.
    let mut result = db
        .query("SELECT VALUE id FROM parent_link")
        .await
        .expect("count parent links")
        .check()
        .expect("count parent links check");
    let rows: Vec<surrealdb::types::RecordId> = result.take(0).expect("parent link rows");
    assert!(rows.is_empty(), "parent_link rows deleted from the DB");
}

// --- messages ------------------------------------------------------------

#[tokio::test]
async fn messages_flow_through_folders_per_side() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await; // student
    let hoca = login_as(&app, &db, "hoca", "teacher").await;
    let hoca_id = me_id(&app, &hoca).await;

    // Send lands in the recipient's inbox unread; the sender holds a `sent` copy.
    let res = send(
        &app,
        "POST",
        "/messages",
        Some(&ali),
        Some(json!({ "recipient_id": hoca_id, "subject": "etüt", "body": "10 dk gecikebilirim", "label": " Etüt " })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["folder"], "sent");
    assert_eq!(res.body["read"], false);
    assert_eq!(res.body["label"], "Etüt", "label stored trimmed");
    assert_eq!(res.body["sender"]["username"], "ali");
    assert_eq!(res.body["sender_role"], "student");
    assert_eq!(res.body["recipient_role"], "teacher");
    let msg_id = id_of(&res.body);

    // Recipient's inbox shows it; their copy says `inbox`.
    let inbox = send(&app, "GET", "/messages", Some(&hoca), None).await;
    let items = common::items(&inbox.body);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["folder"], "inbox");
    assert_eq!(items[0]["subject"], "etüt");

    // Only the recipient flips the read flag; the sender sees the receipt.
    let res = send(
        &app,
        "PATCH",
        &format!("/messages/{msg_id}"),
        Some(&ali),
        Some(json!({ "read": true })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    // `?read=` narrows; `total` on the filtered view is the unread badge.
    let unread = send(
        &app,
        "GET",
        "/messages?folder=inbox&read=false&limit=1",
        Some(&hoca),
        None,
    )
    .await;
    assert_eq!(unread.body["total"], 1);
    let res = send(
        &app,
        "PATCH",
        &format!("/messages/{msg_id}"),
        Some(&hoca),
        Some(json!({ "read": true })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let unread = send(
        &app,
        "GET",
        "/messages?folder=inbox&read=false&limit=1",
        Some(&hoca),
        None,
    )
    .await;
    assert_eq!(unread.body["total"], 0);
    let sent = send(&app, "GET", "/messages?folder=sent", Some(&ali), None).await;
    assert_eq!(common::items(&sent.body)[0]["read"], true);

    // Each side only moves through its own folders.
    let res = send(
        &app,
        "PATCH",
        &format!("/messages/{msg_id}"),
        Some(&hoca),
        Some(json!({ "folder": "sent" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = send(
        &app,
        "PATCH",
        &format!("/messages/{msg_id}"),
        Some(&hoca),
        Some(json!({ "folder": "archive" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let inbox = send(&app, "GET", "/messages", Some(&hoca), None).await;
    assert_eq!(common::items(&inbox.body).len(), 0);
    let archive = send(&app, "GET", "/messages?folder=archive", Some(&hoca), None).await;
    assert_eq!(common::items(&archive.body).len(), 1);

    // Permanent delete only from the trash; it never touches the other copy.
    let res = send(
        &app,
        "DELETE",
        &format!("/messages/{msg_id}"),
        Some(&hoca),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT);
    send(
        &app,
        "PATCH",
        &format!("/messages/{msg_id}"),
        Some(&hoca),
        Some(json!({ "folder": "trash" })),
    )
    .await;
    let trash = send(&app, "GET", "/messages?folder=trash", Some(&hoca), None).await;
    assert_eq!(common::items(&trash.body).len(), 1);
    let res = send(
        &app,
        "DELETE",
        &format!("/messages/{msg_id}"),
        Some(&hoca),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let trash = send(&app, "GET", "/messages?folder=trash", Some(&hoca), None).await;
    assert_eq!(common::items(&trash.body).len(), 0);
    // Deleted side is no longer a party at all.
    let res = send(
        &app,
        "PATCH",
        &format!("/messages/{msg_id}"),
        Some(&hoca),
        Some(json!({ "folder": "inbox" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    // Sender still holds their copy…
    let sent = send(&app, "GET", "/messages?folder=sent", Some(&ali), None).await;
    assert_eq!(common::items(&sent.body).len(), 1);

    // …and once the sender trashes + deletes too, the row itself is gone.
    send(
        &app,
        "PATCH",
        &format!("/messages/{msg_id}"),
        Some(&ali),
        Some(json!({ "folder": "trash" })),
    )
    .await;
    let res = send(
        &app,
        "DELETE",
        &format!("/messages/{msg_id}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let mut result = db
        .query("SELECT VALUE id FROM message")
        .await
        .expect("count messages")
        .check()
        .expect("count messages check");
    let rows: Vec<surrealdb::types::RecordId> = result.take(0).expect("message rows");
    assert!(rows.is_empty(), "both-sides-deleted row is removed");
}

/// A filed copy remembers the folder it left, so restoring lands where it
/// came from instead of always in the inbox.
#[tokio::test]
async fn messages_remember_the_folder_a_filed_copy_came_from() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await; // student
    let hoca = login_as(&app, &db, "hoca", "teacher").await;
    let hoca_id = me_id(&app, &hoca).await;

    let res = send(
        &app,
        "POST",
        "/messages",
        Some(&ali),
        Some(json!({ "recipient_id": hoca_id, "subject": "etüt" })),
    )
    .await;
    let msg_id = res.body["id"].as_str().expect("message id").to_string();
    assert!(
        res.body["previous_folder"].is_null(),
        "a home copy has none"
    );

    let file = |actor: &'static str, folder: &'static str| {
        let app = app.clone();
        let (ali, hoca) = (ali.clone(), hoca.clone());
        let msg_id = msg_id.clone();
        async move {
            let session = if actor == "ali" { ali } else { hoca };
            let res = send(
                &app,
                "PATCH",
                &format!("/messages/{msg_id}"),
                Some(&session),
                Some(json!({ "folder": folder })),
            )
            .await;
            assert_eq!(res.status, StatusCode::OK, "move to {folder}");
            res.body["previous_folder"].clone()
        }
    };

    // inbox → archive → trash walks the memory one hop at a time.
    assert_eq!(file("hoca", "archive").await, json!("inbox"));
    assert_eq!(file("hoca", "trash").await, json!("archive"));
    // Restoring to the remembered folder puts the copy back there — with no
    // memory of its own, since it came out of the trash and the trash is
    // never a restore target. It falls back to the home folder.
    assert!(file("hoca", "archive").await.is_null());
    let archive = send(&app, "GET", "/messages?folder=archive", Some(&hoca), None).await;
    assert_eq!(common::items(&archive.body).len(), 1);
    // Back home clears the stamp for good.
    assert!(file("hoca", "inbox").await.is_null());

    // A no-op move keeps the memory it already had.
    assert_eq!(file("hoca", "archive").await, json!("inbox"));
    assert_eq!(file("hoca", "archive").await, json!("inbox"));

    // The sender's side has only one home, so its trash always points at it.
    assert_eq!(file("ali", "trash").await, json!("sent"));
    // The two sides' memories are independent.
    let trash = send(&app, "GET", "/messages?folder=trash", Some(&ali), None).await;
    assert_eq!(common::items(&trash.body)[0]["previous_folder"], "sent");
    let archive = send(&app, "GET", "/messages?folder=archive", Some(&hoca), None).await;
    assert_eq!(common::items(&archive.body)[0]["previous_folder"], "inbox");

    // Deleting drops the stamp with the rest of that side's copy.
    let res = send(
        &app,
        "DELETE",
        &format!("/messages/{msg_id}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let mut result = db
        .query("SELECT VALUE sender_origin FROM message")
        .await
        .expect("read origins")
        .check()
        .expect("read origins check");
    let origins: Vec<Option<String>> = result.take(0).expect("origin rows");
    assert_eq!(origins, vec![None], "the deleted side keeps no origin");
}

/// Issue #23: messaging is upward only below staff. A student or parent writes
/// to teacher+ and to nobody else; staff write in any direction.
#[tokio::test]
async fn messages_from_below_staff_go_upward_only() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await; // student
    let ali_id = me_id(&app, &ali).await;
    let veli = login(&app, "veli").await; // student
    let veli_id = me_id(&app, &veli).await;
    let anne = login_as(&app, &db, "anne", "parent").await;
    let hoca = login_as(&app, &db, "hoca", "teacher").await;
    let hoca_id = me_id(&app, &hoca).await;
    let mudur = login_as(&app, &db, "mudur", "manager").await;

    let write = async |from: &str, to: &str| {
        send(
            &app,
            "POST",
            "/messages",
            Some(from),
            Some(json!({ "recipient_id": to, "subject": "selam" })),
        )
        .await
    };

    // Sideways and downward from below staff: refused, with the rule spelled out.
    let res = write(&ali, &veli_id).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    assert!(
        res.body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("staff"),
        "{}",
        res.body
    );
    assert_eq!(write(&anne, &ali_id).await.status, StatusCode::FORBIDDEN);

    // Upward: allowed for both.
    assert_eq!(write(&ali, &hoca_id).await.status, StatusCode::CREATED);
    assert_eq!(write(&anne, &hoca_id).await.status, StatusCode::CREATED);

    // Staff write anywhere — down to a student, sideways to a colleague.
    assert_eq!(write(&hoca, &ali_id).await.status, StatusCode::CREATED);
    assert_eq!(write(&mudur, &hoca_id).await.status, StatusCode::CREATED);

    // The refused sends left nothing behind.
    let inbox = send(&app, "GET", "/messages", Some(&veli), None).await;
    assert_eq!(common::items(&inbox.body).len(), 0, "{}", inbox.body);
}

#[tokio::test]
async fn messages_guard_parties_recipients_and_folders() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;
    let veli = login(&app, "veli").await;
    let parent = login_as(&app, &db, "anne", "parent").await;
    let hoca = login_as(&app, &db, "hoca", "teacher").await;
    let hoca_id = me_id(&app, &hoca).await;

    // Sending to yourself or to nobody fails.
    let res = send(
        &app,
        "POST",
        "/messages",
        Some(&ali),
        Some(json!({ "recipient_id": ali_id, "subject": "hi" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = send(
        &app,
        "POST",
        "/messages",
        Some(&ali),
        Some(json!({ "recipient_id": "01J8XZ0K3Q8G7X2M4N5P6R7S8T", "subject": "hi" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // A parent writes — messaging is the role's one pen — but upward only: to
    // the teacher, not to the student.
    let res = send(
        &app,
        "POST",
        "/messages",
        Some(&parent),
        Some(json!({ "recipient_id": ali_id, "subject": "görüşme", "body": "uygun mu?" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = send(
        &app,
        "POST",
        "/messages",
        Some(&parent),
        Some(json!({ "recipient_id": hoca_id, "subject": "görüşme", "body": "uygun mu?" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(res.body["sender_role"], "parent");
    assert_eq!(
        res.body["label"],
        serde_json::Value::Null,
        "no label sent, none stored"
    );
    let msg_id = id_of(&res.body);

    // A third user is not a party: sees nothing, touches nothing.
    let inbox = send(&app, "GET", "/messages", Some(&veli), None).await;
    assert_eq!(common::items(&inbox.body).len(), 0);
    let res = send(
        &app,
        "PATCH",
        &format!("/messages/{msg_id}"),
        Some(&veli),
        Some(json!({ "read": true })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = send(
        &app,
        "DELETE",
        &format!("/messages/{msg_id}"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // Unknown folder names and oversized labels are refused.
    let res = send(&app, "GET", "/messages?folder=spam", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let veli_id = me_id(&app, &veli).await;
    let res = send(
        &app,
        "POST",
        "/messages",
        Some(&ali),
        Some(json!({ "recipient_id": veli_id, "subject": "hi", "label": "x".repeat(51) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
}

/// A patch that fails validation must not half-apply: the read flag stays
/// untouched when the folder part of the same body is rejected.
#[tokio::test]
async fn messages_rejected_patch_leaves_no_side_effects() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;
    let hoca = login_as(&app, &db, "hoca", "teacher").await;
    let hoca_id = me_id(&app, &hoca).await;

    let res = send(
        &app,
        "POST",
        "/messages",
        Some(&ali),
        Some(json!({ "recipient_id": hoca_id, "subject": "s", "body": "b" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let msg_id = id_of(&res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/messages/{msg_id}"),
        Some(&hoca),
        Some(json!({ "read": true })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    // Recipient can't move to `sent`; the bundled read flip must not land.
    let res = send(
        &app,
        "PATCH",
        &format!("/messages/{msg_id}"),
        Some(&hoca),
        Some(json!({ "read": false, "folder": "sent" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let unread = send(
        &app,
        "GET",
        "/messages?folder=inbox&read=false",
        Some(&hoca),
        None,
    )
    .await;
    assert_eq!(
        common::items(&unread.body).len(),
        0,
        "rejected patch must not flip the read flag"
    );
}

/// Notes carry no role bar: they are own-scoped personal data with no other
/// reader, so the read-only `parent` keeps theirs like everybody else. The
/// `student` bar this route family used to hold was a defect — see
/// `a_demotion_to_parent_strands_no_note_and_no_blob`.
#[tokio::test]
async fn parent_role_keeps_its_own_notes() {
    let (app, db) = app_and_db().await;
    let parent = login_as(&app, &db, "baba", "parent").await;

    let res = send(
        &app,
        "POST",
        "/notes",
        Some(&parent),
        Some(json!({ "title": "mine" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(&app, "GET", "/notes", Some(&parent), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(common::items(&res.body).len(), 1);

    // Student and up keep the pen.
    let ali = login(&app, "ali").await;
    let res = send(
        &app,
        "POST",
        "/notes",
        Some(&ali),
        Some(json!({ "title": "ok" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
}

/// A parent link whose student side stopped being a student (a role sweep
/// losing a race with link creation) must be inert, exactly like a stale
/// enrollment: the report reads re-check the live role.
#[tokio::test]
async fn stale_parent_link_is_inert_after_role_change() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let parent = login_as(&app, &db, "anne", "parent").await;
    let parent_id = me_id(&app, &parent).await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;

    let res = send(
        &app,
        "POST",
        &format!("/users/{parent_id}/students"),
        Some(&admin),
        Some(json!({ "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/marks/{ali_id}"),
        Some(&parent),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Promote the student straight in the DB — the endpoint's sweep never
    // fires, leaving the link row behind like a lost race would.
    common::set_role(&db, "ali", "teacher").await;

    for path in [
        format!("/marks/{ali_id}"),
        format!("/attendance/{ali_id}"),
        format!("/pomodoro/{ali_id}"),
    ] {
        let res = send(&app, "GET", &path, Some(&parent), None).await;
        assert_eq!(
            res.status,
            StatusCode::FORBIDDEN,
            "stale link must not grant {path}"
        );
    }
}

// --- the question pool ---------------------------------------------------

/// The pool lifecycle end to end: a student asks (pending — invisible to
/// other students, listed for teachers), a teacher approves (one-way, 409 on
/// repeat), the pool opens school-wide, anyone offers solutions, and
/// moderation deletes cascade.
#[tokio::test]
async fn question_pool_lifecycle() {
    let (app, db) = app_and_db().await;
    let asker = login(&app, "ali").await;
    let stranger = login(&app, "veli").await;
    let teacher = login_as(&app, &db, "hoca", "teacher").await;

    // Only students ask: the teacher is refused.
    let ask = json!({ "title": "Integral", "body": "∫x·eˣ dx?" });
    let res = send(
        &app,
        "POST",
        "/questions",
        Some(&teacher),
        Some(ask.clone()),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);

    let res = send(&app, "POST", "/questions", Some(&asker), Some(ask)).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["status"], "pending");
    let qid = id_of(&res.body);

    // Pending: the asker and the teacher see it; another student must not
    // even learn it exists — and it takes no solutions yet.
    for (cookie, visible) in [(&asker, true), (&teacher, true), (&stranger, false)] {
        let res = send(
            &app,
            "GET",
            &format!("/questions/{qid}"),
            Some(cookie),
            None,
        )
        .await;
        let expected = if visible {
            StatusCode::OK
        } else {
            StatusCode::NOT_FOUND
        };
        assert_eq!(res.status, expected, "{}", res.body);
    }
    let offer = json!({ "body": "Kısmi integrasyon." });
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/solutions"),
        Some(&asker),
        Some(offer.clone()),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // The teacher's ?status=pending list is the approval queue; approval is
    // teacher-gated and one-way.
    let res = send(
        &app,
        "GET",
        "/questions?status=pending",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 1);
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/approve"),
        Some(&stranger),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/approve"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "approved");
    assert_eq!(res.body["approved_by"]["username"], "hoca");
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/approve"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // Approved: school-wide — the stranger reads it and answers it.
    let res = send(&app, "GET", "/questions", Some(&stranger), None).await;
    assert_eq!(common::total(&res.body), 1);
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/solutions"),
        Some(&stranger),
        Some(offer),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let sid = id_of(&res.body);

    // Deleting the solution: a third party may not, its author may.
    let res = send(
        &app,
        "DELETE",
        &format!("/questions/{qid}/solutions/{sid}"),
        Some(&asker),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/questions/{qid}/solutions/{sid}"),
        Some(&stranger),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);

    // Question delete (asker withdraws) cascades what's left.
    let res = send(
        &app,
        "DELETE",
        &format!("/questions/{qid}"),
        Some(&asker),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/questions/{qid}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
}

/// Parents are observers: every pool surface (list, ask, answer) is closed to
/// them.
#[tokio::test]
async fn question_pool_locks_parents_out() {
    let (app, db) = app_and_db().await;
    let parent = login_as(&app, &db, "anne", "parent").await;
    let res = send(&app, "GET", "/questions", Some(&parent), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = send(
        &app,
        "POST",
        "/questions",
        Some(&parent),
        Some(json!({ "title": "t", "body": "b" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
}

/// The question photo: asker-only upload while pending, SVG refused, bytes
/// served under the question's visibility, approval freezes the slot.
#[tokio::test]
async fn question_pool_image_upload_and_freeze() {
    let (app, db) = app_and_db().await;
    let asker = login(&app, "ali").await;
    let stranger = login(&app, "veli").await;
    let teacher = login_as(&app, &db, "hoca", "teacher").await;

    let res = send(
        &app,
        "POST",
        "/questions",
        Some(&asker),
        Some(json!({ "title": "Harita", "body": "Hangi şehir?" })),
    )
    .await;
    let qid = id_of(&res.body);
    let upload = |cookie: &str, content_type: &str| {
        let uri = format!("/questions/{qid}/image");
        let body = common::multipart_file("soru.png", content_type, b"png-bytes");
        let cookie = cookie.to_string();
        let app = app.clone();
        async move {
            common::send_raw(
                &app,
                "POST",
                &uri,
                Some(&cookie),
                Some("multipart/form-data; boundary=hezarfen-test-boundary"),
                body,
            )
            .await
        }
    };

    // SVG is out; the teacher isn't the asker; the asker's PNG lands.
    let (status, _, _) = upload(&asker, "image/svg+xml").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _) = upload(&teacher, "image/png").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = upload(&asker, "image/png").await;
    assert_eq!(status, StatusCode::CREATED);

    // Pending bytes follow pending visibility: asker yes, stranger no.
    let uri = format!("/questions/{qid}/image");
    let (status, headers, bytes) =
        common::send_raw(&app, "GET", &uri, Some(&asker), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(bytes, b"png-bytes");
    let (status, _, _) =
        common::send_raw(&app, "GET", &uri, Some(&stranger), None, Vec::new()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Approval freezes the image with the text: replace and remove both 409.
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/approve"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let (status, _, _) = upload(&asker, "image/png").await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _, _) =
        common::send_raw(&app, "DELETE", &uri, Some(&asker), None, Vec::new()).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // But the approved bytes are now school-wide, and ride the question meta.
    let (status, _, _) =
        common::send_raw(&app, "GET", &uri, Some(&stranger), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    let res = send(
        &app,
        "GET",
        &format!("/questions/{qid}"),
        Some(&stranger),
        None,
    )
    .await;
    assert_eq!(res.body["image"]["content_type"], "image/png");
}

/// The solution's photo and body stay the author's alone (no freeze — no
/// moderation state to protect), bytes follow the question's visibility, and
/// question rows tally their solutions.
#[tokio::test]
async fn solution_images_edits_and_counts() {
    let (app, db) = app_and_db().await;
    let asker = login(&app, "ali").await;
    let helper = login(&app, "veli").await;
    let teacher = login_as(&app, &db, "hoca", "teacher").await;

    // An approved question with the helper's solution under it.
    let res = send(
        &app,
        "POST",
        "/questions",
        Some(&asker),
        Some(json!({ "title": "Limit", "body": "x→0 iken sin(x)/x?" })),
    )
    .await;
    let qid = id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/approve"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/solutions"),
        Some(&helper),
        Some(json!({ "body": "Birebir 1'e gider." })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert!(res.body["image"].is_null());
    let sid = id_of(&res.body);

    // Image writes are the author's: the asker is refused, the helper lands.
    let uri = format!("/questions/{qid}/solutions/{sid}/image");
    let upload = |cookie: &str| {
        let uri = uri.clone();
        let body = common::multipart_file("cozum.png", "image/png", b"png-bytes");
        let cookie = cookie.to_string();
        let app = app.clone();
        async move {
            common::send_raw(
                &app,
                "POST",
                &uri,
                Some(&cookie),
                Some("multipart/form-data; boundary=hezarfen-test-boundary"),
                body,
            )
            .await
        }
    };
    let (status, _, _) = upload(&asker).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = upload(&teacher).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = upload(&helper).await;
    assert_eq!(status, StatusCode::CREATED);

    // Bytes follow the question's visibility — approved, so any student
    // reads them — and the meta rides the solution row.
    let (status, headers, bytes) =
        common::send_raw(&app, "GET", &uri, Some(&asker), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(bytes, b"png-bytes");
    let res = send(
        &app,
        "GET",
        &format!("/questions/{qid}/solutions"),
        Some(&asker),
        None,
    )
    .await;
    assert_eq!(res.body["items"][0]["image"]["content_type"], "image/png");

    // Body edits are author-only too — teacher+ moderate by deleting, never
    // by rewriting.
    let edit = json!({ "body": "Standart limit: sin(x)/x → 1." });
    let res = send(
        &app,
        "PATCH",
        &format!("/questions/{qid}/solutions/{sid}"),
        Some(&asker),
        Some(edit.clone()),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/questions/{qid}/solutions/{sid}"),
        Some(&helper),
        Some(edit),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["body"], "Standart limit: sin(x)/x → 1.");
    assert_eq!(res.body["image"]["content_type"], "image/png");

    // Question rows tally their solutions, in the list and the single GET.
    let res = send(&app, "GET", "/questions?limit=5", Some(&asker), None).await;
    assert_eq!(res.body["items"][0]["solution_count"], 1);
    let res = send(
        &app,
        "GET",
        &format!("/questions/{qid}"),
        Some(&asker),
        None,
    )
    .await;
    assert_eq!(res.body["solution_count"], 1);

    // The author drops the photo; a second removal finds nothing.
    let (status, _, _) =
        common::send_raw(&app, "DELETE", &uri, Some(&helper), None, Vec::new()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = common::send_raw(&app, "GET", &uri, Some(&asker), None, Vec::new()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) =
        common::send_raw(&app, "DELETE", &uri, Some(&helper), None, Vec::new()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // And the question delete still sweeps everything under it.
    let res = send(
        &app,
        "DELETE",
        &format!("/questions/{qid}"),
        Some(&asker),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
}

// --- drawing byte-fidelity (stroke-JSON tEXt passthrough) -------------------
// The frontend's canvas saves a drawing as a PNG that also carries its
// editable stroke JSON inside a custom `tEXt` chunk; the backend only ever
// stores and re-serves the uploaded bytes (see `read_upload` / `serve_inline_blob`
// in web/mod.rs). These tests build a real (if minimal) PNG with a `tEXt`
// chunk and assert the round trip is byte-for-byte — if anyone ever adds
// image re-encoding, compression, or thumbnailing, every stored drawing
// silently stops being replayable, and this is what would catch it.

/// CRC-32 (ISO-HDLC — the checksum PNG chunks and zlib both use), bit by bit:
/// no table, since this only ever runs over a few dozen fixture bytes. No new
/// crate for one checksum used to hand-build a PNG below.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Pin the hand-rolled checksum against the standard CRC-32 check value.
/// Nothing downstream ever validates a chunk's CRC (the server never parses
/// PNG structure), so a broken implementation here would silently build
/// fixtures that only *look* like PNGs — this is the one thing that would
/// catch that.
#[tokio::test]
async fn crc32_matches_the_standard_check_value() {
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
}

/// Frame one PNG chunk: 4-byte big-endian length, 4-byte type, the data, then
/// the CRC-32 over type+data.
fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut chunk = Vec::with_capacity(12 + data.len());
    chunk.extend_from_slice(&(data.len() as u32).to_be_bytes());
    chunk.extend_from_slice(kind);
    chunk.extend_from_slice(data);
    let crc_body: Vec<u8> = kind.iter().chain(data.iter()).copied().collect();
    chunk.extend_from_slice(&crc32(&crc_body).to_be_bytes());
    chunk
}

/// A genuinely valid, minimal (1x1 grayscale) PNG carrying a custom `tEXt`
/// chunk whose text is `marker` — the same trick the frontend uses to smuggle
/// its editable stroke JSON inside a drawing's PNG bytes. Built inline (no
/// binary fixture file) so the test commits exactly the bytes it later
/// asserts on.
fn png_with_text_chunk(marker: &str) -> Vec<u8> {
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    // IHDR: 1x1 pixel, 8-bit depth, grayscale, no interlace.
    png.extend(png_chunk(b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0, 0, 0]));
    // tEXt: keyword "Comment", NUL, then the marker — exactly where a real
    // drawing's stroke JSON rides.
    let mut text = b"Comment\0".to_vec();
    text.extend_from_slice(marker.as_bytes());
    png.extend(png_chunk(b"tEXt", &text));
    // IDAT: one stored (uncompressed) deflate block over the single
    // [filter=None, pixel=0] scanline, so this decodes as a real black pixel.
    png.extend(png_chunk(
        b"IDAT",
        &[
            0x78, 0x01, // zlib header
            0x01, 0x02, 0x00, 0xFD, 0xFF, // stored block: BFINAL+BTYPE, LEN, NLEN
            0x00, 0x00, // scanline: filter=None, pixel=0
            0x00, 0x02, 0x00, 0x01, // Adler-32 of the scanline bytes
        ],
    ));
    png.extend(png_chunk(b"IEND", &[]));
    png
}

/// The primary invariant: a student's answer drawing survives its round trip
/// byte-for-byte, `tEXt` chunk included — on both read paths that matter, the
/// student's own (for editing) and the teacher's grading read (for replay).
/// If the backend ever re-encodes or strips metadata from this PNG, the
/// stroke JSON in the `tEXt` chunk is gone and the drawing stops replaying.
#[tokio::test]
async fn answer_image_round_trip_keeps_the_stroke_text_chunk_intact() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "rt_t", "teacher").await;
    let student = login(&app, "rt_s").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "art").await;
    let subject = create_subject(&app, &teacher, &course, "sketching").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "sketch quiz", "kind": "quiz", "mode": "open" }),
    )
    .await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Draw a triangle.", "kind": "text", "points": 10 }),
    )
    .await;
    let question = id_of(&question_body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    let png = png_with_text_chunk(r#"{"strokes":[[1,1],[2,2],[3,3]]}"#);
    let own = format!("/exams/{exam}/attempt/answers/{question}/image");
    let (status, body) = post_image(&app, &student, &own, "image/png", &png).await;
    assert_eq!(status, StatusCode::CREATED, "{}", body);

    // The student replays their own drawing from this route.
    let (status, headers, bytes) =
        common::send_raw(&app, "GET", &own, Some(&student), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(bytes, png, "the student's own read must be byte-identical");

    // The teacher replays it too, via the grading route — same requirement.
    let teacher_uri = format!("/exams/{exam}/attempts/{student_id}/answers/{question}/image");
    let (status, _, bytes) =
        common::send_raw(&app, "GET", &teacher_uri, Some(&teacher), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, png, "the grader read must be byte-identical too");
}

/// The pool question photo is a drawing too (a student can attach a photo or
/// a canvas sketch of the problem) — same byte-for-byte requirement as an
/// answer image, or a teacher replaying a hand-drawn question loses the
/// stroke JSON in its `tEXt` chunk.
#[tokio::test]
async fn question_photo_round_trip_keeps_the_stroke_text_chunk_intact() {
    let app = mem_app().await;
    let asker = login(&app, "ali").await;
    let res = send(
        &app,
        "POST",
        "/questions",
        Some(&asker),
        Some(json!({ "title": "Şekil", "body": "Çizim ekli" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let qid = id_of(&res.body);

    let png = png_with_text_chunk(r#"{"strokes":[[4,4],[5,5]]}"#);
    let uri = format!("/questions/{qid}/image");
    let (status, body) = post_image(&app, &asker, &uri, "image/png", &png).await;
    assert_eq!(status, StatusCode::CREATED, "{}", body);

    let (status, headers, bytes) =
        common::send_raw(&app, "GET", &uri, Some(&asker), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(
        bytes, png,
        "the pool question photo must round-trip byte-for-byte"
    );
}

/// Same story on a solution's worked-steps photo: the backend must never
/// touch these bytes, or a replayed solution drawing loses its stroke JSON.
#[tokio::test]
async fn solution_photo_round_trip_keeps_the_stroke_text_chunk_intact() {
    let (app, db) = app_and_db().await;
    let asker = login(&app, "ali").await;
    let helper = login(&app, "veli").await;
    let teacher = login_as(&app, &db, "hoca", "teacher").await;

    let res = send(
        &app,
        "POST",
        "/questions",
        Some(&asker),
        Some(json!({ "title": "Limit", "body": "Çözüm arıyorum" })),
    )
    .await;
    let qid = id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/approve"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/solutions"),
        Some(&helper),
        Some(json!({ "body": "İşte çözüm." })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let sid = id_of(&res.body);

    let png = png_with_text_chunk(r#"{"strokes":[[6,6],[7,7]]}"#);
    let uri = format!("/questions/{qid}/solutions/{sid}/image");
    let (status, body) = post_image(&app, &helper, &uri, "image/png", &png).await;
    assert_eq!(status, StatusCode::CREATED, "{}", body);

    let (status, headers, bytes) =
        common::send_raw(&app, "GET", &uri, Some(&asker), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(
        bytes, png,
        "the solution photo must round-trip byte-for-byte"
    );
}

/// The contrast to `question_pool_image_upload_and_freeze`'s 409-after-approval:
/// a solution carries no moderation state, so its author replaces the photo
/// freely at any time, even on a long-approved question.
#[tokio::test]
async fn solution_photo_replaces_freely_unlike_the_frozen_question_photo() {
    let (app, db) = app_and_db().await;
    let asker = login(&app, "ali").await;
    let helper = login(&app, "veli").await;
    let teacher = login_as(&app, &db, "hoca", "teacher").await;

    let res = send(
        &app,
        "POST",
        "/questions",
        Some(&asker),
        Some(json!({ "title": "Türev", "body": "Nasıl alınır?" })),
    )
    .await;
    let qid = id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/approve"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/solutions"),
        Some(&helper),
        Some(json!({ "body": "Cevap burada." })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let sid = id_of(&res.body);

    let uri = format!("/questions/{qid}/solutions/{sid}/image");
    let (status, _) = post_image(&app, &helper, &uri, "image/png", b"first-draft").await;
    assert_eq!(status, StatusCode::CREATED);
    // No freeze: a second replace on the same long-approved question still lands.
    let (status, _) = post_image(&app, &helper, &uri, "image/png", b"second-draft").await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "a solution photo must never freeze"
    );
    let (status, _, bytes) =
        common::send_raw(&app, "GET", &uri, Some(&helper), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"second-draft");
}

/// The other half of a pending question's photo visibility (the asker-vs-stranger
/// half is already pinned by `question_pool_image_upload_and_freeze`): teacher+
/// is the approval queue's other reader, so a pending photo must reach them too.
#[tokio::test]
async fn pool_question_pending_photo_reaches_teacher_too() {
    let (app, db) = app_and_db().await;
    let asker = login(&app, "ali").await;
    let teacher = login_as(&app, &db, "hoca", "teacher").await;

    let res = send(
        &app,
        "POST",
        "/questions",
        Some(&asker),
        Some(json!({ "title": "Soru", "body": "Yardım lazım" })),
    )
    .await;
    let qid = id_of(&res.body);
    let uri = format!("/questions/{qid}/image");
    let (status, _) = post_image(&app, &asker, &uri, "image/png", b"png-bytes").await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _, bytes) =
        common::send_raw(&app, "GET", &uri, Some(&teacher), None, Vec::new()).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "teacher+ must see a pending photo too"
    );
    assert_eq!(bytes, b"png-bytes");
}

/// `GET` on a pool photo needs student role or higher; parents are read-only
/// report observers (see `question_pool_locks_parents_out`), so both photo
/// routes must refuse them exactly like the rest of the pool.
#[tokio::test]
async fn pool_photo_reads_require_student_role() {
    let (app, db) = app_and_db().await;
    let asker = login(&app, "ali").await;
    let helper = login(&app, "veli").await;
    let teacher = login_as(&app, &db, "hoca", "teacher").await;
    let parent = login_as(&app, &db, "anne", "parent").await;

    let res = send(
        &app,
        "POST",
        "/questions",
        Some(&asker),
        Some(json!({ "title": "Soru", "body": "Yardım lazım" })),
    )
    .await;
    let qid = id_of(&res.body);
    let q_uri = format!("/questions/{qid}/image");
    let (status, _) = post_image(&app, &asker, &q_uri, "image/png", b"q-bytes").await;
    assert_eq!(status, StatusCode::CREATED);
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/approve"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/questions/{qid}/solutions"),
        Some(&helper),
        Some(json!({ "body": "Cevap burada." })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let sid = id_of(&res.body);
    let s_uri = format!("/questions/{qid}/solutions/{sid}/image");
    let (status, _) = post_image(&app, &helper, &s_uri, "image/png", b"s-bytes").await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _, _) =
        common::send_raw(&app, "GET", &q_uri, Some(&parent), None, Vec::new()).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "parents can't read a question photo"
    );
    let (status, _, _) =
        common::send_raw(&app, "GET", &s_uri, Some(&parent), None, Vec::new()).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "parents can't read a solution photo"
    );
}

// --- homework -------------------------------------------------------------

/// A teacher-run course with a subject and two enrolled students — the stage
/// most homework tests play on.
struct HwWorld {
    teacher: String,
    ali: String,
    ali_id: String,
    veli: String,
    veli_id: String,
    course: String,
    subject: String,
}

async fn hw_world(app: &axum::Router, db: &hezarfen_backend::database::Database) -> HwWorld {
    let teacher = login_as(app, db, "teacher", "teacher").await;
    let ali = login(app, "ali").await;
    let ali_id = me_id(app, &ali).await;
    let veli = login(app, "veli").await;
    let veli_id = me_id(app, &veli).await;
    let course = create_course(app, &teacher, "math").await;
    let subject = create_subject(app, &teacher, &course, "algebra").await;
    enroll(app, &teacher, &course, &ali_id).await;
    enroll(app, &teacher, &course, &veli_id).await;
    HwWorld {
        teacher,
        ali,
        ali_id,
        veli,
        veli_id,
        course,
        subject,
    }
}

/// POST the caller's submission (optional text) to `hw` — no assertion.
async fn submit_hw(app: &axum::Router, cookie: &str, hw: &str, text: Option<&str>) -> common::Res {
    let body = match text {
        Some(text) => json!({ "text": text }),
        None => json!({}),
    };
    send(
        app,
        "POST",
        &format!("/homework/{hw}/submission"),
        Some(cookie),
        Some(body),
    )
    .await
}

/// Upload `bytes` as a file onto the caller's submission to `hw` — no
/// assertion. Parses the JSON body like `send`.
async fn upload_hw_file(
    app: &axum::Router,
    cookie: &str,
    hw: &str,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
) -> common::Res {
    let (status, _, body) = common::send_raw(
        app,
        "POST",
        &format!("/homework/{hw}/submission/files"),
        Some(cookie),
        Some("multipart/form-data; boundary=hezarfen-test-boundary"),
        common::multipart_file(filename, content_type, bytes),
    )
    .await;
    let body = if body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null)
    };
    common::Res {
        status,
        body,
        cookie: None,
    }
}

/// POST a grade for `user` onto `hw` — no assertion.
async fn grade_hw(
    app: &axum::Router,
    cookie: &str,
    hw: &str,
    user: &str,
    status: &str,
    mark: Option<i64>,
) -> common::Res {
    let mut body = json!({ "user": user, "status": status });
    if let Some(mark) = mark {
        body["mark"] = json!(mark);
    }
    send(
        app,
        "POST",
        &format!("/homework/{hw}/results"),
        Some(cookie),
        Some(body),
    )
    .await
}

/// Every stored homework-file blob name, straight from the table.
async fn homework_blob_keys(db: &hezarfen_backend::database::Database) -> Vec<String> {
    let mut result = db
        .query("SELECT VALUE file FROM homework_file")
        .await
        .unwrap()
        .check()
        .unwrap();
    result.take::<Vec<String>>(0).unwrap()
}

/// Row count of `table`, straight from the database — a cascade must really
/// delete, not merely hide behind the role checks.
async fn hw_row_count(db: &hezarfen_backend::database::Database, table: &str) -> usize {
    let mut result = db
        .query(format!("SELECT VALUE id FROM {table}"))
        .await
        .unwrap()
        .check()
        .unwrap();
    result
        .take::<Vec<surrealdb::types::RecordId>>(0)
        .unwrap()
        .len()
}

/// Creation validates the due date (60s grace), the subject's course, the
/// subset's enrollment, and the subset cap — and stays teacher-only.
#[tokio::test]
async fn homework_create_validates_due_subject_and_subset() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();

    // due_at beyond the 60s grace lies in the past — rejected.
    let res = common::create_homework_with(
        &app,
        &w.teacher,
        &w.course,
        json!({ "title": "old", "subject_id": w.subject, "due_at": now - 120_000 }),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // The subject must belong to this very course.
    let other_course = create_course(&app, &w.teacher, "physics").await;
    let foreign = create_subject(&app, &w.teacher, &other_course, "optics").await;
    let res = common::create_homework_with(
        &app,
        &w.teacher,
        &w.course,
        json!({ "title": "x", "subject_id": foreign, "due_at": now + 3_600_000 }),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // Every assigned student must be enrolled…
    let mehmet = login(&app, "mehmet").await;
    let mehmet_id = me_id(&app, &mehmet).await;
    let res = common::create_homework_with(
        &app,
        &w.teacher,
        &w.course,
        json!({ "title": "x", "subject_id": w.subject, "due_at": now + 3_600_000,
                "assigned": [mehmet_id] }),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // …and the subset caps at 200 names (checked before enrollment, so fakes
    // trip it without 201 signups).
    let fakes: Vec<String> = (0..201).map(|i| format!("01FAKE{i:020}")).collect();
    let res = common::create_homework_with(
        &app,
        &w.teacher,
        &w.course,
        json!({ "title": "x", "subject_id": w.subject, "due_at": now + 3_600_000,
                "assigned": fakes }),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // Students don't assign homework.
    let res = common::create_homework_with(
        &app,
        &w.ali,
        &w.course,
        json!({ "title": "x", "subject_id": w.subject, "due_at": now + 3_600_000 }),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // Within the grace counts as "due now" — accepted, like a plain future one.
    let res = common::create_homework_with(
        &app,
        &w.teacher,
        &w.course,
        json!({ "title": "just now", "subject_id": w.subject, "due_at": now - 30_000 }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let hw = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "ch. 3",
        now + 3_600_000,
    )
    .await;
    let res = send(&app, "GET", &format!("/homework/{hw}"), Some(&w.ali), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["title"], "ch. 3");
    assert_eq!(res.body["subject"], w.subject.as_str());
    assert!(
        res.body["assigned"].is_null(),
        "whole-course stores no subset"
    );
}

/// A subset assignment must never leak to the students it leaves out (404s,
/// list omissions), off-course teachers are walled off, the cross-course list
/// narrows per caller, and staff never submit.
#[tokio::test]
async fn homework_subset_hides_from_outsiders() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();

    let res = common::create_homework_with(
        &app,
        &w.teacher,
        &w.course,
        json!({ "title": "secret drill", "subject_id": w.subject, "due_at": now + 3_600_000,
                "assigned": [w.ali_id] }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let hw = id_of(&res.body);

    // ali is named: both lists carry it and the fetch works.
    let uri = format!("/courses/{}/homework", w.course);
    let res = send(&app, "GET", &uri, Some(&w.ali), None).await;
    assert_eq!(common::total(&res.body), 1);
    let res = send(&app, "GET", "/homework", Some(&w.ali), None).await;
    assert_eq!(common::total(&res.body), 1);
    // veli is enrolled but unnamed: the homework must not even seem to exist.
    let res = send(&app, "GET", &uri, Some(&w.veli), None).await;
    assert_eq!(
        common::total(&res.body),
        0,
        "unnamed student sees no subset"
    );
    let res = send(&app, "GET", "/homework", Some(&w.veli), None).await;
    assert_eq!(common::total(&res.body), 0);
    let res = send(&app, "GET", &format!("/homework/{hw}"), Some(&w.veli), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "404, never a 403 leak");
    let res = submit_hw(&app, &w.veli, &hw, Some("hi")).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // A teacher without management rights over the course is walled off.
    let rival = login_as(&app, &db, "rival", "teacher").await;
    let res = send(&app, "GET", &format!("/homework/{hw}"), Some(&rival), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "PATCH",
        &format!("/homework/{hw}"),
        Some(&rival),
        Some(json!({ "title": "hijack" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "GET",
        &format!("/homework/{hw}/submissions"),
        Some(&rival),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = grade_hw(&app, &rival, &hw, &w.ali_id, "done", None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(
        &app,
        "DELETE",
        &format!("/homework/{hw}"),
        Some(&rival),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // The cross-course list narrows per caller: manager+ everything, an
    // off-course teacher nothing, the managing teacher their course's rows.
    let boss = login_as(&app, &db, "boss", "manager").await;
    let res = send(&app, "GET", "/homework", Some(&boss), None).await;
    assert_eq!(common::total(&res.body), 1);
    let res = send(&app, "GET", "/homework", Some(&rival), None).await;
    assert_eq!(common::total(&res.body), 0);
    let res = send(&app, "GET", "/homework", Some(&w.teacher), None).await;
    assert_eq!(common::total(&res.body), 1);

    // Staff never submit — a 403 on the live role, whatever they can see.
    let res = submit_hw(&app, &boss, &hw, Some("i am staff")).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = submit_hw(&app, &w.teacher, &hw, Some("me neither")).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
}

/// PATCH re-checks a newly set due date and subject, refuses a narrowing that
/// would orphan existing work (naming the blockers), and leaves stored past
/// due dates alone on unrelated edits.
#[tokio::test]
async fn homework_patch_rechecks_and_blocks_orphaning() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();
    let res = common::create_homework_with(
        &app,
        &w.teacher,
        &w.course,
        json!({ "title": "drill", "subject_id": w.subject, "due_at": now + 3_600_000,
                "assigned": [w.ali_id, w.veli_id] }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let hw = id_of(&res.body);

    // A newly set due date must not be past…
    let res = send(
        &app,
        "PATCH",
        &format!("/homework/{hw}"),
        Some(&w.teacher),
        Some(json!({ "due_at": now - 120_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // …and a new subject must be the course's own.
    let other_course = create_course(&app, &w.teacher, "physics").await;
    let foreign = create_subject(&app, &w.teacher, &other_course, "optics").await;
    let res = send(
        &app,
        "PATCH",
        &format!("/homework/{hw}"),
        Some(&w.teacher),
        Some(json!({ "subject_id": foreign })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    let algebra2 = create_subject(&app, &w.teacher, &w.course, "algebra II").await;
    let res = send(
        &app,
        "PATCH",
        &format!("/homework/{hw}"),
        Some(&w.teacher),
        Some(json!({ "subject_id": algebra2 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["subject"], algebra2.as_str());

    // ali submits; narrowing to veli alone would strand that work — refused,
    // naming the blocker. Narrowing to exactly ali passes.
    let res = submit_hw(&app, &w.ali, &hw, Some("done")).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/homework/{hw}"),
        Some(&w.teacher),
        Some(json!({ "assigned": [w.veli_id] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert!(
        res.body["error"].as_str().unwrap().contains(&w.ali_id),
        "the 409 names whose work blocks: {}",
        res.body
    );
    let res = send(
        &app,
        "PATCH",
        &format!("/homework/{hw}"),
        Some(&w.teacher),
        Some(json!({ "assigned": [w.ali_id] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Widening back to the whole course always passes.
    let res = send(
        &app,
        "PATCH",
        &format!("/homework/{hw}"),
        Some(&w.teacher),
        Some(json!({ "assigned": [] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body["assigned"].is_null());

    // A title-only edit on a graced, already-due homework keeps its stored
    // due date without tripping the not-past check.
    let old = create_homework(&app, &w.teacher, &w.course, &w.subject, "old", now - 30_000).await;
    let res = send(
        &app,
        "PATCH",
        &format!("/homework/{old}"),
        Some(&w.teacher),
        Some(json!({ "title": "renamed" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

/// Submitting creates once (201) and edits in place after (200): the
/// first-hand-in stamp is pinned, text is replaced whole (omitting clears),
/// and withdrawal removes it all.
#[tokio::test]
async fn homework_submission_lifecycle_and_stamps() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();
    let hw = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "essay",
        now + 3_600_000,
    )
    .await;

    let uri = format!("/homework/{hw}/submission");
    let res = send(&app, "GET", &uri, Some(&w.ali), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "nothing submitted yet");

    let res = submit_hw(&app, &w.ali, &hw, Some("draft one")).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["text"], "draft one");
    assert_eq!(res.body["late"], false);
    let submitted_at = res.body["submitted_at"].as_i64().unwrap();

    let res = submit_hw(&app, &w.ali, &hw, Some("draft two")).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["text"], "draft two");
    assert_eq!(
        res.body["submitted_at"].as_i64().unwrap(),
        submitted_at,
        "the first-submit stamp never moves"
    );
    assert!(res.body["updated_at"].as_i64().unwrap() >= submitted_at);

    // The text rides whole in each submit: omitting it clears.
    let res = submit_hw(&app, &w.ali, &hw, None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body["text"].is_null());

    // Reads are strictly own: veli asking gets veli's (absent) submission.
    let res = send(&app, "GET", &uri, Some(&w.ali), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert!(res.body["result"].is_null());
    let res = send(&app, "GET", &uri, Some(&w.veli), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // Withdrawal, twice: gone, then nothing left to withdraw.
    let res = send(&app, "DELETE", &uri, Some(&w.ali), None).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(&app, "GET", &uri, Some(&w.ali), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = send(&app, "DELETE", &uri, Some(&w.ali), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

/// The late flag is computed against `due_at` on every read, and the roster
/// computes `missing` for unsubmitted-past-due rows — no sleeping: a graced
/// creation plants the due date ~30s in the past.
#[tokio::test]
async fn homework_lateness_is_computed_across_due() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();

    let overdue = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "late one",
        now - 30_000,
    )
    .await;
    let res = submit_hw(&app, &w.ali, &overdue, Some("sorry")).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["late"], true, "touched after due_at");

    let open = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "open one",
        now + 3_600_000,
    )
    .await;
    let res = submit_hw(&app, &w.ali, &open, Some("early")).await;
    assert_eq!(res.body["late"], false);

    // The roster mirrors the flag and derives `missing` for veli, who never
    // submitted — a computation, not a teacher verdict.
    let res = send(
        &app,
        "GET",
        &format!("/homework/{overdue}/submissions"),
        Some(&w.teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(common::total(&res.body), 2);
    let rows = common::items(&res.body);
    let ali_row = rows
        .iter()
        .find(|row| row["user"] == w.ali_id.as_str())
        .unwrap();
    assert_eq!(ali_row["submission"]["late"], true);
    assert_eq!(ali_row["missing"], false);
    assert_eq!(ali_row["unenrolled"], false);
    let veli_row = rows
        .iter()
        .find(|row| row["user"] == w.veli_id.as_str())
        .unwrap();
    assert!(veli_row["submission"].is_null());
    assert_eq!(veli_row["missing"], true, "unsubmitted past due");
}

/// Grading walls: students never grade, nobody grades themselves, and the
/// target must exist, hold the student role, be enrolled, and be in the
/// audience; the verdict set is closed and the mark bounded. A regrade
/// overwrites its one row, and the student reads the verdict back.
#[tokio::test]
async fn homework_grading_gates_and_bounds() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();
    let res = common::create_homework_with(
        &app,
        &w.teacher,
        &w.course,
        json!({ "title": "drill", "subject_id": w.subject, "due_at": now + 3_600_000,
                "assigned": [w.ali_id] }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let hw = id_of(&res.body);
    let teacher_id = me_id(&app, &w.teacher).await;

    let res = grade_hw(&app, &w.ali, &hw, &w.ali_id, "done", None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "students never grade");
    let res = grade_hw(&app, &w.teacher, &hw, &teacher_id, "done", None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "no self-grading");
    let res = grade_hw(
        &app,
        &w.teacher,
        &hw,
        "01ZZZZZZZZZZZZZZZZZZZZZZZZ",
        "done",
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "target must exist");
    let rival = login_as(&app, &db, "rival", "teacher").await;
    let rival_id = me_id(&app, &rival).await;
    let res = grade_hw(&app, &w.teacher, &hw, &rival_id, "done", None).await;
    assert_eq!(
        res.status,
        StatusCode::BAD_REQUEST,
        "only students carry grades"
    );
    let mehmet = login(&app, "mehmet").await;
    let mehmet_id = me_id(&app, &mehmet).await;
    let res = grade_hw(&app, &w.teacher, &hw, &mehmet_id, "done", None).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "must be enrolled");
    let res = grade_hw(&app, &w.teacher, &hw, &w.veli_id, "done", None).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "outside the audience");
    let res = grade_hw(&app, &w.teacher, &hw, &w.ali_id, "late", None).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "junk status");
    let res = grade_hw(&app, &w.teacher, &hw, &w.ali_id, "done", Some(101)).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "mark over 100");
    let res = grade_hw(&app, &w.teacher, &hw, &w.ali_id, "done", Some(-1)).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "negative mark");

    // 200 upsert semantics: the regrade overwrites, never stacks.
    let res = grade_hw(&app, &w.teacher, &hw, &w.ali_id, "incomplete", None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "incomplete");
    assert!(res.body["mark"].is_null());
    let res = grade_hw(&app, &w.teacher, &hw, &w.ali_id, "done", Some(85)).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["mark"], 85);
    assert_eq!(hw_row_count(&db, "homework_result").await, 1);

    // The student reads their own verdict; ungraded means 404, for anyone.
    let uri = format!("/homework/{hw}/result");
    let res = send(&app, "GET", &uri, Some(&w.ali), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["status"], "done");
    assert_eq!(res.body["mark"], 85);
    let res = send(&app, "GET", &uri, Some(&w.veli), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

/// A stored grade freezes the submission — text, files, withdrawal — until
/// The files-per-submission cap is the same counter guard as the note-file one,
/// on the submission row. Nine seeded files plus four racers for the last slot:
/// exactly one may win, and the delete must give the slot back or the next
/// upload is refused forever.
#[tokio::test]
async fn concurrent_homework_uploads_never_exceed_the_file_cap() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();
    let hw = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "essay",
        now + 3_600_000,
    )
    .await;
    let res = submit_hw(&app, &w.ali, &hw, Some("v1")).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    for i in 0..9 {
        let up = upload_hw_file(&app, &w.ali, &hw, &format!("f{i}.txt"), "text/plain", b"x").await;
        assert_eq!(up.status, StatusCode::CREATED, "seed file {i}: {}", up.body);
    }

    let racers = tokio::join!(
        upload_hw_file(&app, &w.ali, &hw, "r0.txt", "text/plain", b"x"),
        upload_hw_file(&app, &w.ali, &hw, "r1.txt", "text/plain", b"x"),
        upload_hw_file(&app, &w.ali, &hw, "r2.txt", "text/plain", b"x"),
        upload_hw_file(&app, &w.ali, &hw, "r3.txt", "text/plain", b"x"),
    );
    let statuses = [
        racers.0.status,
        racers.1.status,
        racers.2.status,
        racers.3.status,
    ];
    for status in statuses {
        assert!(
            status == StatusCode::CREATED || status == StatusCode::CONFLICT,
            "a lost cap race must be a 409, got {status}"
        );
    }
    let list = send(
        &app,
        "GET",
        &format!("/homework/{hw}/submission"),
        Some(&w.ali),
        None,
    )
    .await;
    let files = list.body["files"].as_array().expect("files").clone();
    assert_eq!(
        files.len(),
        10,
        "the cap must hold exactly under concurrency, statuses={statuses:?}"
    );

    // A deleted file frees its slot again — the counter is not a ratchet.
    let fid = files[0]["id"].as_str().expect("file id").to_string();
    let res = send(
        &app,
        "DELETE",
        &format!("/homework/{hw}/submission/files/{fid}"),
        Some(&w.ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    let up = upload_hw_file(&app, &w.ali, &hw, "after.txt", "text/plain", b"x").await;
    assert_eq!(
        up.status,
        StatusCode::CREATED,
        "the freed slot must be reusable: {}",
        up.body
    );
}

/// the teacher removes it; and work never handed in can be graded `missing`,
/// a verdict the student reads from `/result` despite having no submission.
#[tokio::test]
async fn homework_grade_freezes_until_removed() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();
    let hw = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "essay",
        now + 3_600_000,
    )
    .await;

    let res = submit_hw(&app, &w.ali, &hw, Some("v1")).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let up = upload_hw_file(&app, &w.ali, &hw, "draft.txt", "text/plain", b"notes").await;
    assert_eq!(up.status, StatusCode::CREATED, "{}", up.body);
    let fid = id_of(&up.body);

    let res = grade_hw(&app, &w.teacher, &hw, &w.ali_id, "incomplete", None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = submit_hw(&app, &w.ali, &hw, Some("v2")).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "text frozen");
    let res = upload_hw_file(&app, &w.ali, &hw, "late.txt", "text/plain", b"more").await;
    assert_eq!(res.status, StatusCode::CONFLICT, "file add frozen");
    let res = send(
        &app,
        "DELETE",
        &format!("/homework/{hw}/submission/files/{fid}"),
        Some(&w.ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "file delete frozen");
    let res = send(
        &app,
        "DELETE",
        &format!("/homework/{hw}/submission"),
        Some(&w.ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "withdrawal frozen");
    // The grade rides on the student's own submission read.
    let res = send(
        &app,
        "GET",
        &format!("/homework/{hw}/submission"),
        Some(&w.ali),
        None,
    )
    .await;
    assert_eq!(res.body["result"]["status"], "incomplete");

    // Un-grading reopens the work; a second removal finds nothing.
    let uri = format!("/homework/{hw}/results/{}", w.ali_id);
    let res = send(&app, "DELETE", &uri, Some(&w.teacher), None).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = submit_hw(&app, &w.ali, &hw, Some("v2")).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(&app, "DELETE", &uri, Some(&w.teacher), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);

    // Never-submitted work still takes a `missing` verdict, and `/result` is
    // where its student reads it — the submission endpoints have nothing.
    let res = grade_hw(&app, &w.teacher, &hw, &w.veli_id, "missing", None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/homework/{hw}/submission"),
        Some(&w.veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = send(
        &app,
        "GET",
        &format!("/homework/{hw}/result"),
        Some(&w.veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "missing");
}

/// Files round-trip: a photo-first upload auto-creates the submission, the
/// owner and the managing teacher download the exact bytes as a forced
/// attachment, file ids are scoped to their homework, other students see
/// nothing, and a delete unlinks the blob.
#[tokio::test]
async fn homework_files_roundtrip_and_scope() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();
    let hw = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "photo hw",
        now + 3_600_000,
    )
    .await;

    let bytes = b"%PDF-1.4 homework \x00\x01".to_vec();
    let up = upload_hw_file(&app, &w.ali, &hw, "odev.pdf", "application/pdf", &bytes).await;
    assert_eq!(up.status, StatusCode::CREATED, "{}", up.body);
    assert_eq!(up.body["name"], "odev.pdf");
    assert_eq!(up.body["content_type"], "application/pdf");
    assert_eq!(up.body["size"], bytes.len() as i64);
    let fid = id_of(&up.body);
    let res = send(
        &app,
        "GET",
        &format!("/homework/{hw}/submission"),
        Some(&w.ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "photo-first auto-creates");
    assert!(res.body["text"].is_null());
    assert_eq!(res.body["files"].as_array().unwrap().len(), 1);

    // Owner and managing teacher read the same bytes — always an attachment:
    // homework files take any content type, so inline HTML/SVG would be XSS.
    let file_uri = format!("/homework/{hw}/submission/files/{fid}");
    for viewer in [&w.ali, &w.teacher] {
        let (status, headers, body) =
            common::send_raw(&app, "GET", &file_uri, Some(viewer), None, Vec::new()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, bytes);
        assert_eq!(headers["content-type"], "application/pdf");
        assert_eq!(
            headers["content-disposition"],
            "attachment; filename=\"odev.pdf\"; filename*=UTF-8''odev.pdf"
        );
    }
    // The id only resolves under its own homework, and never for a classmate.
    let other = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "other",
        now + 3_600_000,
    )
    .await;
    let (status, _, _) = common::send_raw(
        &app,
        "GET",
        &format!("/homework/{other}/submission/files/{fid}"),
        Some(&w.teacher),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "scoped to its homework");
    let (status, _, _) =
        common::send_raw(&app, "GET", &file_uri, Some(&w.veli), None, Vec::new()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "classmates see nothing");

    // Delete removes the row and unlinks the blob.
    let keys = homework_blob_keys(&db).await;
    assert_eq!(keys.len(), 1);
    let res = send(&app, "DELETE", &file_uri, Some(&w.ali), None).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert!(
        !common::blob_dir().join(&keys[0]).exists(),
        "blob must be unlinked with its row"
    );
    let res = send(
        &app,
        "GET",
        &format!("/homework/{hw}/submission"),
        Some(&w.ali),
        None,
    )
    .await;
    assert_eq!(res.body["files"].as_array().unwrap().len(), 0);
}

/// File limits: empty uploads are rejected, the 11th file trips the cap, and
/// the school's `max_file_bytes` bounds each file (413 over, 201 at).
#[tokio::test]
async fn homework_file_limits_follow_cap_and_settings() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let now = Timestamp::now().as_millis();
    let hw = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "many files",
        now + 3_600_000,
    )
    .await;

    let res = upload_hw_file(&app, &w.ali, &hw, "empty.txt", "text/plain", b"").await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    for i in 0..10 {
        let res = upload_hw_file(&app, &w.ali, &hw, &format!("f{i}.txt"), "text/plain", b"x").await;
        assert_eq!(res.status, StatusCode::CREATED, "file {i}: {}", res.body);
    }
    let res = upload_hw_file(&app, &w.ali, &hw, "f10.txt", "text/plain", b"x").await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert_eq!(
        homework_blob_keys(&db).await.len(),
        10,
        "the losing upload leaves no row"
    );

    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&boss),
        Some(json!({ "max_file_bytes": 1024 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let hw2 = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "sized",
        now + 3_600_000,
    )
    .await;
    let res = upload_hw_file(&app, &w.ali, &hw2, "big.bin", "", &vec![7u8; 1025]).await;
    assert_eq!(res.status, StatusCode::PAYLOAD_TOO_LARGE, "{}", res.body);
    let res = upload_hw_file(&app, &w.ali, &hw2, "fits.bin", "", &vec![7u8; 1024]).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["content_type"], "application/octet-stream");
}

/// The roster carries the whole audience — and keeps a straggler's work
/// visible (flagged `unenrolled`) instead of letting it vanish; a subset
/// roster lists the named students only; the list pages.
#[tokio::test]
async fn homework_roster_covers_audience_and_stragglers() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();

    let hw = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "for all",
        now + 3_600_000,
    )
    .await;
    let roster_uri = format!("/homework/{hw}/submissions");
    let res = send(&app, "GET", &roster_uri, Some(&w.teacher), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(common::total(&res.body), 2, "whole course, both enrolled");

    // ali submits, then leaves the course: the work stays on the roster,
    // flagged — and grading the straggler is refused.
    let res = submit_hw(&app, &w.ali, &hw, Some("mine")).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{}/enrollments/{}", w.course, w.ali_id),
        Some(&w.teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(&app, "GET", &roster_uri, Some(&w.teacher), None).await;
    assert_eq!(common::total(&res.body), 2, "the straggler's work stays");
    let rows = common::items(&res.body);
    let ali_row = rows
        .iter()
        .find(|row| row["user"] == w.ali_id.as_str())
        .unwrap();
    assert_eq!(ali_row["unenrolled"], true);
    assert_eq!(ali_row["submission"]["text"], "mine");
    let res = grade_hw(&app, &w.teacher, &hw, &w.ali_id, "done", None).await;
    assert_eq!(
        res.status,
        StatusCode::BAD_REQUEST,
        "no grading the unenrolled"
    );

    // The roster pages like every list.
    let res = send(
        &app,
        "GET",
        &format!("{roster_uri}?limit=1&offset=1"),
        Some(&w.teacher),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 2);
    assert_eq!(common::items(&res.body).len(), 1);
    assert_eq!(res.body["limit"], 1);
    assert_eq!(res.body["offset"], 1);

    // A subset roster is the named students, nobody else.
    enroll(&app, &w.teacher, &w.course, &w.ali_id).await;
    let res = common::create_homework_with(
        &app,
        &w.teacher,
        &w.course,
        json!({ "title": "subset", "subject_id": w.subject, "due_at": now + 3_600_000,
                "assigned": [w.ali_id] }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let subset_hw = id_of(&res.body);
    let res = send(
        &app,
        "GET",
        &format!("/homework/{subset_hw}/submissions"),
        Some(&w.teacher),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 1);
    assert_eq!(common::items(&res.body)[0]["user"], w.ali_id.as_str());
}

/// The observer report: full for manager+ and a linked parent, narrowed to
/// managed courses for a teacher, walled for everyone else — and it carries
/// statuses, marks, and flags, never file bytes.
#[tokio::test]
async fn homework_report_serves_observers_only() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let now = Timestamp::now().as_millis();

    // A second course under another teacher; ali sits in both.
    let rival = login_as(&app, &db, "rival", "teacher").await;
    let course2 = create_course(&app, &rival, "physics").await;
    let subject2 = create_subject(&app, &rival, &course2, "optics").await;
    enroll(&app, &rival, &course2, &w.ali_id).await;

    // Three states across the two courses: graded, submitted-late, missing.
    let graded = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "graded",
        now + 3_600_000,
    )
    .await;
    let res = submit_hw(&app, &w.ali, &graded, Some("done")).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = grade_hw(&app, &w.teacher, &graded, &w.ali_id, "done", Some(90)).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let overdue = create_homework(&app, &rival, &course2, &subject2, "overdue", now - 30_000).await;
    let res = submit_hw(&app, &w.ali, &overdue, Some("late")).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let missing = create_homework(&app, &rival, &course2, &subject2, "skipped", now - 30_000).await;

    // The admin reads all three rows, each in its computed state.
    let report_uri = format!("/homework/report/{}", w.ali_id);
    let res = send(&app, "GET", &report_uri, Some(&admin), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(common::total(&res.body), 3);
    let rows = common::items(&res.body);
    let graded_row = rows
        .iter()
        .find(|row| row["homework"] == graded.as_str())
        .unwrap();
    assert_eq!(graded_row["submitted"], true);
    assert_eq!(graded_row["late"], false);
    assert_eq!(graded_row["result"]["status"], "done");
    assert_eq!(graded_row["result"]["mark"], 90);
    let late_row = rows
        .iter()
        .find(|row| row["homework"] == overdue.as_str())
        .unwrap();
    assert_eq!(late_row["late"], true);
    assert_eq!(late_row["missing"], false);
    assert!(late_row["result"].is_null());
    let missing_row = rows
        .iter()
        .find(|row| row["homework"] == missing.as_str())
        .unwrap();
    assert_eq!(missing_row["submitted"], false);
    assert_eq!(missing_row["missing"], true);

    // A teacher reads only the slice they manage.
    let res = send(&app, "GET", &report_uri, Some(&w.teacher), None).await;
    assert_eq!(common::total(&res.body), 1);
    assert_eq!(common::items(&res.body)[0]["course"], w.course.as_str());

    // A linked parent reads all of it (paged like every list)…
    let mom = login_as(&app, &db, "mom", "parent").await;
    let mom_id = me_id(&app, &mom).await;
    let res = send(
        &app,
        "POST",
        &format!("/users/{mom_id}/students"),
        Some(&admin),
        Some(json!({ "user_id": w.ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(&app, "GET", &report_uri, Some(&mom), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(common::total(&res.body), 3);
    let res = send(
        &app,
        "GET",
        &format!("{report_uri}?limit=2&offset=2"),
        Some(&mom),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 3);
    assert_eq!(common::items(&res.body).len(), 1);

    // …but never the underlying work: no submission reads, no file bytes.
    let up = upload_hw_file(&app, &w.ali, &overdue, "p.png", "image/png", b"png").await;
    assert_eq!(up.status, StatusCode::CREATED, "{}", up.body);
    let fid = id_of(&up.body);
    let res = send(
        &app,
        "GET",
        &format!("/homework/{overdue}/submission"),
        Some(&mom),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let (status, _, _) = common::send_raw(
        &app,
        "GET",
        &format!("/homework/{overdue}/submission/files/{fid}"),
        Some(&mom),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "reports, never bytes");

    // Unlinked parent, the student themselves, and a ghost target all fail.
    let dad = login_as(&app, &db, "dad", "parent").await;
    let res = send(&app, "GET", &report_uri, Some(&dad), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "unlinked parent");
    let res = send(&app, "GET", &report_uri, Some(&w.ali), None).await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "self-reads use /homework"
    );
    let res = send(
        &app,
        "GET",
        "/homework/report/01ZZZZZZZZZZZZZZZZZZZZZZZZ",
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

/// Every delete path collects its garbage: withdrawing a submission, deleting
/// a homework, and deleting the whole course each remove the database rows
/// *and* the blobs on disk.
#[tokio::test]
async fn homework_gc_removes_rows_and_blobs() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();

    let hw1 = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "one",
        now + 3_600_000,
    )
    .await;
    let hw2 = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "two",
        now + 3_600_000,
    )
    .await;
    for (cookie, hw, name) in [
        (&w.ali, &hw1, "a.txt"),
        (&w.veli, &hw1, "b.txt"),
        (&w.ali, &hw2, "c.txt"),
    ] {
        let res = upload_hw_file(&app, cookie, hw, name, "text/plain", b"data").await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    }
    let res = grade_hw(&app, &w.teacher, &hw1, &w.veli_id, "done", None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let keys = homework_blob_keys(&db).await;
    assert_eq!(keys.len(), 3);
    for key in &keys {
        assert!(common::blob_dir().join(key).exists(), "blob {key} on disk");
    }

    // Withdrawing a submission takes its file rows and blobs.
    let res = send(
        &app,
        "DELETE",
        &format!("/homework/{hw2}/submission"),
        Some(&w.ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let remaining = homework_blob_keys(&db).await;
    assert_eq!(remaining.len(), 2);
    for gone in keys.iter().filter(|key| !remaining.contains(key)) {
        assert!(
            !common::blob_dir().join(gone).exists(),
            "withdrawn blob {gone} must be unlinked"
        );
    }

    // Deleting the homework cascades submissions, files, and grades — blobs
    // included.
    let res = send(
        &app,
        "DELETE",
        &format!("/homework/{hw1}"),
        Some(&w.teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert_eq!(hw_row_count(&db, "homework_submission").await, 0);
    assert_eq!(hw_row_count(&db, "homework_file").await, 0);
    assert_eq!(hw_row_count(&db, "homework_result").await, 0);
    assert_eq!(hw_row_count(&db, "homework").await, 1, "hw2 remains");
    for key in &remaining {
        assert!(
            !common::blob_dir().join(key).exists(),
            "blob {key} must die with the homework"
        );
    }

    // Deleting the course takes everything left.
    let res = upload_hw_file(&app, &w.ali, &hw2, "d.txt", "text/plain", b"data").await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = grade_hw(&app, &w.teacher, &hw2, &w.veli_id, "missing", None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let keys = homework_blob_keys(&db).await;
    assert_eq!(keys.len(), 1);
    unenroll(&app, &w.teacher, &w.course, &w.ali_id).await;
    unenroll(&app, &w.teacher, &w.course, &w.veli_id).await;
    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{}", w.course),
        Some(&w.teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    for table in [
        "homework",
        "homework_submission",
        "homework_file",
        "homework_result",
    ] {
        assert_eq!(
            hw_row_count(&db, table).await,
            0,
            "{table} died with course"
        );
    }
    for key in &keys {
        assert!(
            !common::blob_dir().join(key).exists(),
            "blob {key} must die with the course"
        );
    }
}

/// A promotion sweeps the enrollment but never the homework rows; the gates
/// re-read the live role and deny, and the roster keeps the stale work
/// visible, flagged `unenrolled`.
#[tokio::test]
async fn homework_rows_survive_promotion_but_gates_deny() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let now = Timestamp::now().as_millis();
    let hw = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "before",
        now + 3_600_000,
    )
    .await;
    let res = submit_hw(&app, &w.ali, &hw, Some("as a student")).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = grade_hw(&app, &w.teacher, &hw, &w.ali_id, "done", Some(70)).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let res = send(
        &app,
        "PATCH",
        &format!("/users/{}/role", w.ali_id),
        Some(&admin),
        Some(json!({ "role": "teacher" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(hw_row_count(&db, "homework_submission").await, 1);
    assert_eq!(hw_row_count(&db, "homework_result").await, 1);

    // Live-role gates: no submission access, no fresh grades onto staff.
    let res = send(
        &app,
        "GET",
        &format!("/homework/{hw}/submission"),
        Some(&w.ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = submit_hw(&app, &w.ali, &hw, Some("as staff")).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = grade_hw(&app, &w.teacher, &hw, &w.ali_id, "done", None).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "no longer a student");

    // The roster keeps the stale work, flagged (the promotion swept the
    // enrollment).
    let res = send(
        &app,
        "GET",
        &format!("/homework/{hw}/submissions"),
        Some(&w.teacher),
        None,
    )
    .await;
    let rows = common::items(&res.body);
    let row = rows
        .iter()
        .find(|row| row["user"] == w.ali_id.as_str())
        .unwrap();
    assert_eq!(row["unenrolled"], true);
    assert_eq!(row["result"]["mark"], 70);
}

/// A subject with homework can't be deleted (409 naming homework) until the
/// homework is re-tagged or removed — the guard the exam questions already
/// have.
#[tokio::test]
async fn homework_blocks_subject_delete_until_retagged() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();
    let hw = create_homework(
        &app,
        &w.teacher,
        &w.course,
        &w.subject,
        "tagged",
        now + 3_600_000,
    )
    .await;

    let res = send(
        &app,
        "DELETE",
        &format!("/subjects/{}", w.subject),
        Some(&w.teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert!(
        res.body["error"].as_str().unwrap().contains("homework"),
        "{}",
        res.body
    );

    // Re-tagging frees the old subject and guards the new one.
    let second = create_subject(&app, &w.teacher, &w.course, "geometry").await;
    let res = send(
        &app,
        "PATCH",
        &format!("/homework/{hw}"),
        Some(&w.teacher),
        Some(json!({ "subject_id": second })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/subjects/{}", w.subject),
        Some(&w.teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "DELETE",
        &format!("/subjects/{second}"),
        Some(&w.teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT);
}

/// The two homework lists page through the standard envelope.
#[tokio::test]
async fn homework_lists_paginate() {
    let (app, db) = app_and_db().await;
    let w = hw_world(&app, &db).await;
    let now = Timestamp::now().as_millis();
    for i in 0..3 {
        create_homework(
            &app,
            &w.teacher,
            &w.course,
            &w.subject,
            &format!("hw {i}"),
            now + 3_600_000,
        )
        .await;
    }
    for uri in [
        format!("/courses/{}/homework?limit=2&offset=2", w.course),
        "/homework?limit=2&offset=2".to_string(),
    ] {
        let res = send(&app, "GET", &uri, Some(&w.ali), None).await;
        assert_eq!(res.status, StatusCode::OK, "{uri}");
        assert_eq!(common::total(&res.body), 3, "{uri}");
        assert_eq!(common::items(&res.body).len(), 1, "{uri}");
        assert_eq!(res.body["limit"], 2);
        assert_eq!(res.body["offset"], 2);
    }
}

/// A course carrying students can't be deleted (409) until the roster is
/// emptied — the cascade would otherwise take the enrollments with it.
#[tokio::test]
async fn course_delete_blocks_while_students_are_enrolled() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "cdb_t", "teacher").await;
    let student = login(&app, "cdb_s").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "Chemistry").await;
    enroll(&app, &teacher, &course, &student_id).await;

    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{course}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/courses/{course}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    // Empty the roster: the delete goes through.
    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{course}/enrollments/{student_id}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{course}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
}

/// An exam kind whose exams already carry marks can't leave the settings list
/// (409) — weights are read live, so those marks would silently re-weight.
/// Ungraded kinds still leave freely.
#[tokio::test]
async fn exam_kind_removal_blocks_while_marks_exist() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ekd_t", "teacher").await;
    let manager = login_as(&app, &db, "ekd_m", "manager").await;
    let student = login(&app, "ekd_s").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "Biology").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = create_exam(&app, &teacher, &course, "Final", "final").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        Some(json!({ "user_id": student_id, "mark": 70 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Dropping the graded kind is refused; the list is untouched.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "exam_kinds": [{ "name": "quiz", "weight": 1 }] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let res = send(&app, "GET", "/settings", Some(&manager), None).await;
    let kinds: Vec<String> = res.body["exam_kinds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|kind| kind["name"].as_str().unwrap().to_string())
        .collect();
    assert!(kinds.contains(&"final".to_string()), "{}", res.body);
    assert!(kinds.contains(&"quiz".to_string()), "{}", res.body);

    // Keeping the graded kind, dropping the ungraded ones, is fine.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "exam_kinds": [{ "name": "final", "weight": 5 }] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["exam_kinds"].as_array().unwrap().len(), 1);
    assert_eq!(res.body["exam_kinds"][0]["weight"], 5);
}

// --- chatbot relay -------------------------------------------------------
//
// What the `POST` itself decides: the receipt, the availability gate, the
// limits, and who may see a thread. The bridge round trip and the answer that
// comes back are `ai_bridge.rs`'s job — the fake service dialled in here
// completes the handshake and then stays quiet on purpose, so nothing in this
// file depends on an answer arriving.

use hezarfen_backend::ai::protocol::{Greeting, Hello, read_frame, write_frame};
use hezarfen_backend::ai::{AiBridge, BridgeConfig};
use hezarfen_backend::constant::{AI_ALPN, AI_CHAT_CAPABILITY, AI_PROTOCOL};
use hezarfen_backend::database::Database;

const CHAT_TOKEN: &str = "integration-ai-token";

/// A bridge with one registered — but silent — `chat.reply` service. Holding
/// it keeps the registration alive: the control stream's closure is a goodbye.
struct ChatBridge {
    bridge: AiBridge,
    _endpoint: quinn::Endpoint,
    _conn: quinn::Connection,
    _control: (quinn::SendStream, quinn::RecvStream),
}

async fn chat_bridge() -> ChatBridge {
    let bridge = AiBridge::bind(BridgeConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: CHAT_TOKEN.to_string(),
        cert_path: None,
        key_path: None,
        request_timeout: std::time::Duration::from_secs(5),
    })
    .await
    .expect("bridge binds on an ephemeral port");

    hezarfen_backend::ai::tls::install_crypto_provider();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(bridge.certificate()).expect("pin the leaf");
    let mut tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![AI_ALPN.to_vec()];
    let config = quinn::ClientConfig::new(std::sync::Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).expect("QUIC-usable TLS"),
    ));
    let mut endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).expect("client bind");
    endpoint.set_default_client_config(config);
    let conn = endpoint
        .connect(bridge.local_addr().unwrap(), "localhost")
        .expect("dial")
        .await
        .expect("QUIC handshake");

    let (mut send_stream, mut recv_stream) = conn.open_bi().await.expect("control stream");
    write_frame(
        &mut send_stream,
        &Hello {
            protocol: AI_PROTOCOL.to_string(),
            service: "fake-chat".to_string(),
            capabilities: vec![AI_CHAT_CAPABILITY.to_string()],
            token: CHAT_TOKEN.to_string(),
            max_concurrent: None,
        },
    )
    .await
    .expect("send Hello");
    let greeting: Greeting = read_frame(&mut recv_stream).await.expect("read Greeting");
    assert!(matches!(greeting, Greeting::Welcome { .. }), "{greeting:?}");

    // Registration completes after the welcome is on the wire, so wait for it
    // rather than racing it.
    for _ in 0..300 {
        if bridge.has_capability(AI_CHAT_CAPABILITY) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        bridge.has_capability(AI_CHAT_CAPABILITY),
        "service never registered"
    );

    ChatBridge {
        bridge,
        _endpoint: endpoint,
        _conn: conn,
        _control: (send_stream, recv_stream),
    }
}

/// A router whose AI bridge is `ai`, plus a handle to its database.
async fn chat_app(ai: Option<AiBridge>) -> (axum::Router, Database) {
    chat_app_limited(ai, Default::default()).await
}

/// The same, with an explicit per-user chat tier — what
/// `RATE_LIMIT_CHATBOT_PER_MINUTE` configures in production.
async fn chat_app_limited(
    ai: Option<AiBridge>,
    chatbot_limit: hezarfen_backend::rate_limit::UserRateLimiter,
) -> (axum::Router, Database) {
    let (tenants, db) = common::mem_deployment().await;
    let app = build_router(AppState {
        db: tenants.control().clone(),
        tenants,
        files_path: common::files_dir(),
        cookie_secure: false,
        rate_limit: hezarfen_backend::rate_limit::RateLimitConfig::unlimited(),
        chatbot_limit,
        exam_presence: Default::default(),
        board_hub: Default::default(),
        db_up: Default::default(),
        ai,
        metrics: hezarfen_backend::telemetry::Metrics::noop(),
    });
    (app, db)
}

/// How many `thread` and `chatbot_message` rows exist, anywhere.
async fn chat_rows(db: &Database) -> (usize, usize) {
    let mut res = db
        .query("SELECT VALUE id FROM chatbot_thread; SELECT VALUE id FROM chatbot_message;")
        .await
        .expect("count query")
        .check()
        .expect("count check");
    let threads: Vec<surrealdb::types::RecordId> = res.take(0).expect("thread ids");
    let messages: Vec<surrealdb::types::RecordId> = res.take(1).expect("chatbot_message ids");
    (threads.len(), messages.len())
}

/// Open a thread (asserts 201) and return its id.
async fn new_thread(app: &axum::Router, cookie: &str) -> String {
    let res = send(
        app,
        "POST",
        "/chatbot/threads",
        Some(cookie),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    id_of(&res.body)
}

async fn post_turn(app: &axum::Router, cookie: &str, thread: &str, content: &str) -> common::Res {
    send(
        app,
        "POST",
        &format!("/chatbot/threads/{thread}/messages"),
        Some(cookie),
        Some(json!({ "content": content })),
    )
    .await
}

/// Open the SSE stream without draining it — a `pending` turn's stream stays
/// open, so reading its body here would hang.
async fn open_stream(app: &axum::Router, cookie: &str, thread: &str, mid: &str) -> StatusCode {
    let request = Request::builder()
        .method("GET")
        .uri(format!("/chatbot/threads/{thread}/messages/{mid}/stream"))
        .header("cookie", cookie)
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(request).await.unwrap().status()
}

#[tokio::test]
async fn chat_send_is_accepted_with_a_reserved_assistant_row() {
    let ai = chat_bridge().await;
    let (app, db) = chat_app(Some(ai.bridge.clone())).await;
    let cookie = login(&app, "ali").await;

    let res = send(
        &app,
        "POST",
        "/chatbot/threads",
        Some(&cookie),
        Some(json!({ "title": "Fizik" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["title"], "Fizik");
    assert!(res.body["created_at"].is_i64(), "{}", res.body);
    assert!(res.body["updated_at"].is_i64(), "{}", res.body);
    let thread = id_of(&res.body);

    // The receipt: the reserved row's id and nothing else. The answer is not
    // here — it is being fetched.
    let res = post_turn(&app, &cookie, &thread, "ikinci yasa nedir?").await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.body);
    assert_eq!(res.body["status"], "pending");
    let mid = res.body["message_id"].as_str().expect("message_id");
    assert!(!mid.is_empty());
    assert_eq!(
        res.body.as_object().expect("object body").len(),
        2,
        "{}",
        res.body
    );

    // Both rows are already durable, and the reserved one reads back empty.
    assert_eq!(chat_rows(&db).await, (1, 2));
    let res = send(
        &app,
        "GET",
        &format!("/chatbot/threads/{thread}/messages/{mid}"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["thread_id"], thread);
    assert_eq!(res.body["role"], "assistant");
    assert_eq!(res.body["status"], "pending");
    assert_eq!(res.body["content"], "");
    assert!(res.body["error_code"].is_null(), "{}", res.body);
    assert!(res.body["completed_at"].is_null(), "{}", res.body);
    assert!(res.body["created_at"].is_i64(), "{}", res.body);

    // The thread holds the question and its reserved answer, in that order —
    // the two rows one POST writes share a millisecond, so this is the id
    // tie-break of the route's `ORDER BY` doing its job.
    let res = send(
        &app,
        "GET",
        &format!("/chatbot/threads/{thread}/messages"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let turns = common::items(&res.body);
    assert_eq!(turns.len(), 2, "{}", res.body);
    assert_eq!(turns[0]["role"], "user", "{}", res.body);
    assert_eq!(turns[0]["content"], "ikinci yasa nedir?", "{}", res.body);
    assert_eq!(turns[1]["role"], "assistant", "{}", res.body);
    assert_eq!(turns[1]["id"], mid, "{}", res.body);
}

#[tokio::test]
async fn chat_send_without_a_service_is_503_and_writes_nothing() {
    // Both ways the gate can say no: no bridge on this deployment at all, and
    // a bridge with nothing declaring `chat.reply`. Neither may leave a user
    // turn stranded next to a pending row nothing could ever answer.
    let idle = AiBridge::bind(BridgeConfig {
        addr: "127.0.0.1:0".parse().unwrap(),
        token: CHAT_TOKEN.to_string(),
        cert_path: None,
        key_path: None,
        request_timeout: std::time::Duration::from_secs(5),
    })
    .await
    .expect("bridge binds");

    for ai in [None, Some(idle)] {
        let (app, db) = chat_app(ai).await;
        let cookie = login(&app, "ali").await;
        let thread = new_thread(&app, &cookie).await;

        let res = post_turn(&app, &cookie, &thread, "bir soru").await;
        assert_eq!(res.status, StatusCode::SERVICE_UNAVAILABLE, "{}", res.body);
        assert!(
            res.body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("AI"),
            "{}",
            res.body
        );
        // The thread exists (it was created), but the turn left no trace: no
        // user message, no pending assistant row.
        assert_eq!(chat_rows(&db).await, (1, 0), "a refused turn wrote rows");
    }
}

#[tokio::test]
async fn chat_lists_are_paged() {
    let ai = chat_bridge().await;
    let (app, _db) = chat_app(Some(ai.bridge.clone())).await;
    let cookie = login(&app, "ali").await;

    // Three threads, and three turns (six rows) inside the first.
    let mut threads = Vec::new();
    for _ in 0..3 {
        threads.push(new_thread(&app, &cookie).await);
    }
    for n in 0..3 {
        let res = post_turn(&app, &cookie, &threads[0], &format!("soru {n}")).await;
        assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.body);
    }

    // Both routes answer the same envelope, and the pages partition the whole
    // list. Order is compared element for element: the id tie-break makes it
    // deterministic even for the two rows one POST writes in a millisecond.
    for (uri, expected) in [
        ("/chatbot/threads".to_string(), 3),
        (format!("/chatbot/threads/{}/messages", threads[0]), 6),
    ] {
        let res = send(
            &app,
            "GET",
            &format!("{uri}?limit=2&offset=0"),
            Some(&cookie),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        assert_eq!(common::total(&res.body), expected, "{}", res.body);
        assert_eq!(res.body["limit"], 2, "{}", res.body);
        assert_eq!(res.body["offset"], 0, "{}", res.body);
        let mut ids: Vec<String> = common::items(&res.body).iter().map(id_of).collect();
        assert_eq!(ids.len(), 2);

        let res = send(
            &app,
            "GET",
            &format!("{uri}?limit=99&offset=2"),
            Some(&cookie),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        assert_eq!(common::total(&res.body), expected, "{}", res.body);
        assert_eq!(res.body["offset"], 2, "{}", res.body);
        ids.extend(common::items(&res.body).iter().map(id_of));

        // Paging is a window on one stable order, not a reshuffle: the pages
        // concatenated must reproduce the unpaged list exactly.
        let res = send(&app, "GET", &uri, Some(&cookie), None).await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        let whole: Vec<String> = common::items(&res.body).iter().map(id_of).collect();
        assert_eq!(ids, whole, "the pages must reproduce {uri} in order");

        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len() as i64,
            expected,
            "the pages must partition {uri}, not overlap or skip"
        );

        // Past the end is an empty page, not an error.
        let res = send(
            &app,
            "GET",
            &format!("{uri}?limit=5&offset=999"),
            Some(&cookie),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        assert!(common::items(&res.body).is_empty(), "{}", res.body);
        assert_eq!(common::total(&res.body), expected, "{}", res.body);
    }
}

#[tokio::test]
async fn chat_threads_are_invisible_to_everyone_else() {
    // Nobody reads someone else's thread — not a teacher, not an admin. A
    // foreign id is a 404, never a 403: its existence is not leaked either.
    let ai = chat_bridge().await;
    let (app, db) = chat_app(Some(ai.bridge.clone())).await;
    let ali = login(&app, "ali").await;
    let thread = new_thread(&app, &ali).await;
    let res = post_turn(&app, &ali, &thread, "bir soru").await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.body);
    let mid = res.body["message_id"].as_str().unwrap().to_string();

    for (name, role) in [
        ("veli", "student"),
        ("hoca", "teacher"),
        ("patron", "admin"),
    ] {
        let other = login_as(&app, &db, name, role).await;
        for (method, uri) in [
            ("GET", format!("/chatbot/threads/{thread}/messages")),
            ("GET", format!("/chatbot/threads/{thread}/messages/{mid}")),
            (
                "GET",
                format!("/chatbot/threads/{thread}/messages/{mid}/stream"),
            ),
            ("DELETE", format!("/chatbot/threads/{thread}")),
            ("PATCH", format!("/chatbot/threads/{thread}")),
            ("POST", format!("/chatbot/threads/{thread}/messages")),
        ] {
            let body = match method {
                "POST" => Some(json!({ "content": "sızıntı" })),
                "PATCH" => Some(json!({ "title": "benim artık" })),
                _ => None,
            };
            let res = send(&app, method, &uri, Some(&other), body).await;
            assert_eq!(res.status, StatusCode::NOT_FOUND, "{name} {method} {uri}");
        }
        // Nor does someone else's thread show up in their own list.
        let res = send(&app, "GET", "/chatbot/threads", Some(&other), None).await;
        assert!(common::items(&res.body).is_empty(), "{}", res.body);
    }

    // And the owner still has everything, untouched.
    assert_eq!(chat_rows(&db).await, (1, 2));
    let res = send(&app, "GET", "/chatbot/threads", Some(&ali), None).await;
    assert_eq!(common::items(&res.body).len(), 1, "{}", res.body);
}

#[tokio::test]
async fn chat_thread_count_is_capped_by_the_school() {
    let (app, db) = chat_app(None).await;
    let manager = login_as(&app, &db, "mudur", "manager").await;
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "max_chatbot_threads": 1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let ali = login(&app, "ali").await;
    let first = new_thread(&app, &ali).await;
    let res = send(
        &app,
        "POST",
        "/chatbot/threads",
        Some(&ali),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // The cap is storage protection, per user: another user is unaffected, and
    // deleting a thread frees the slot.
    let veli = login(&app, "veli").await;
    let _ = new_thread(&app, &veli).await;

    let res = send(
        &app,
        "DELETE",
        &format!("/chatbot/threads/{first}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    let _ = new_thread(&app, &ali).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_thread_creation_never_passes_the_cap() {
    // The sequential test above cannot see this: count-then-create is only a
    // cap if something serializes the two, and the database's transactions do
    // not serialize a count against concurrent inserts. Sixteen simultaneous
    // creators against a cap of five must still leave five rows.
    const CAP: usize = 5;
    const RACERS: usize = 16;

    let (app, db) = chat_app(None).await;
    let manager = login_as(&app, &db, "mudur", "manager").await;
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "max_chatbot_threads": CAP })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let ali = login(&app, "ali").await;

    // A barrier, so every request is inside the check-then-write window at
    // once rather than trickling in.
    let gate = std::sync::Arc::new(tokio::sync::Barrier::new(RACERS));
    let racers: Vec<_> = (0..RACERS)
        .map(|_| {
            let (app, cookie, gate) = (app.clone(), ali.clone(), gate.clone());
            tokio::spawn(async move {
                gate.wait().await;
                send(
                    &app,
                    "POST",
                    "/chatbot/threads",
                    Some(&cookie),
                    Some(json!({})),
                )
                .await
                .status
            })
        })
        .collect();

    let mut created = 0;
    for racer in racers {
        match racer.await.expect("no panic in a racer") {
            StatusCode::CREATED => created += 1,
            StatusCode::CONFLICT => {}
            other => panic!("unexpected {other}"),
        }
    }
    assert_eq!(created, CAP, "the cap was over-admitted");
    assert_eq!(
        chat_rows(&db).await.0,
        CAP,
        "rows past the cap were written"
    );
}

#[tokio::test]
async fn chat_turn_stamps_thread_activity() {
    // The `updated_at` bump is best-effort (a failure must never strand the
    // pending row it follows) — but it still has to happen.
    let ai = chat_bridge().await;
    let (app, _db) = chat_app(Some(ai.bridge.clone())).await;
    let ali = login(&app, "ali").await;
    let thread = new_thread(&app, &ali).await;
    let before = send(&app, "GET", "/chatbot/threads", Some(&ali), None).await;
    let opened = common::items(&before.body)[0]["updated_at"]
        .as_i64()
        .unwrap();

    // The stamp is in whole milliseconds; make sure the clock has moved.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let res = post_turn(&app, &ali, &thread, "bir soru").await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.body);

    let after = send(&app, "GET", "/chatbot/threads", Some(&ali), None).await;
    let stamped = common::items(&after.body)[0]["updated_at"]
        .as_i64()
        .unwrap();
    assert!(stamped > opened, "{stamped} !> {opened}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_stream_stops_polling_when_the_client_hangs_up() {
    // A `pending` turn sends nothing, so nothing used to notice a vanished
    // client: the poll task kept re-reading the row every 200ms for the whole
    // 300-second staleness window, and `/stream` is not rate-limited. Dropping
    // the response body is exactly what axum does when a connection dies.
    // No bridge here: the pending row is written straight to the database, so
    // the only task this runtime gains is the stream's own poll loop and the
    // count below is not drowned in the QUIC service's task churn.
    let (app, db) = chat_app(None).await;
    let ali = login(&app, "ali").await;
    let thread = new_thread(&app, &ali).await;
    let me = send(&app, "GET", "/auth/me", Some(&ali), None).await;
    let user = UserId::from_key(me.body["id"].as_str().unwrap());
    let pending =
        ChatbotMessage::append_pending_assistant(&ChatbotThreadId::from_key(&thread), &user, &db)
            .await
            .expect("reserve an assistant row");
    let mid = pending.get_id().key().to_string();

    // The witness is the runtime's task count, so it is read only once the
    // setup's short-lived database tasks have drained — otherwise one of those
    // finishing mid-measurement looks like the stream's own task.
    let metrics = tokio::runtime::Handle::current().metrics();
    let mut before = metrics.num_alive_tasks();
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let now = metrics.num_alive_tasks();
        if now == before {
            break;
        }
        before = now;
    }
    let request = Request::builder()
        .method("GET")
        .uri(format!("/chatbot/threads/{thread}/messages/{mid}/stream"))
        .header("cookie", &ali)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        metrics.num_alive_tasks() > before,
        "the stream's poll task should be running"
    );

    drop(response);
    for _ in 0..20 {
        if metrics.num_alive_tasks() <= before {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the stream kept polling after its client hung up");
}

#[tokio::test]
async fn chat_content_is_required_and_capped_by_the_school() {
    let ai = chat_bridge().await;
    let (app, db) = chat_app(Some(ai.bridge.clone())).await;
    let manager = login_as(&app, &db, "mudur", "manager").await;
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "max_chatbot_message_len": 100 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let ali = login(&app, "ali").await;
    let thread = new_thread(&app, &ali).await;

    for empty in ["", "   ", "\n\t"] {
        let res = post_turn(&app, &ali, &thread, empty).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{empty:?} accepted");
    }
    // Counted in characters, not bytes: 101 multi-byte characters is 101.
    let res = post_turn(&app, &ali, &thread, &"é".repeat(101)).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    // Nothing rejected was written.
    assert_eq!(chat_rows(&db).await, (1, 0));

    let res = post_turn(&app, &ali, &thread, &"é".repeat(100)).await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.body);
}

#[tokio::test]
async fn chat_rate_limit_refuses_before_anything_is_written() {
    // 20 messages a minute per user (the documented tier). The limiter is
    // charged first, so the refused turn leaves no row behind — and no other
    // user inherits the window.
    let ai = chat_bridge().await;
    let (app, db) = chat_app(Some(ai.bridge.clone())).await;
    let ali = login(&app, "ali").await;
    let thread = new_thread(&app, &ali).await;

    for n in 1..=20 {
        let res = post_turn(&app, &ali, &thread, &format!("soru {n}")).await;
        assert_eq!(res.status, StatusCode::ACCEPTED, "turn {n}: {}", res.body);
    }
    let before = chat_rows(&db).await;
    assert_eq!(before, (1, 40));

    let (status, headers, _) = common::send_raw(
        &app,
        "POST",
        &format!("/chatbot/threads/{thread}/messages"),
        Some(&ali),
        Some("application/json"),
        json!({ "content": "yirmi birinci" })
            .to_string()
            .into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let retry_after = headers
        .get("retry-after")
        .expect("Retry-After tells the client when to come back")
        .to_str()
        .unwrap()
        .parse::<u64>()
        .expect("whole seconds");
    assert!((1..=60).contains(&retry_after), "{retry_after}");
    assert_eq!(chat_rows(&db).await, before, "a refused turn wrote rows");

    let veli = login(&app, "veli").await;
    let other = new_thread(&app, &veli).await;
    let res = post_turn(&app, &veli, &other, "benim ilk sorum").await;
    assert_eq!(res.status, StatusCode::ACCEPTED, "{}", res.body);
}

#[tokio::test]
async fn every_role_may_chat_parents_included() {
    // Deliberate, not an oversight: the chatbot is a `CurrentUser` route for
    // every role the school has, parents among them.
    let ai = chat_bridge().await;
    let (app, db) = chat_app(Some(ai.bridge.clone())).await;

    for (name, role) in [
        ("veli", "parent"),
        ("ogrenci", "student"),
        ("hoca", "teacher"),
        ("mudur", "manager"),
        ("patron", "admin"),
    ] {
        let cookie = login_as(&app, &db, name, role).await;
        let thread = new_thread(&app, &cookie).await;

        let res = post_turn(&app, &cookie, &thread, "bir soru").await;
        assert_eq!(res.status, StatusCode::ACCEPTED, "{role}: {}", res.body);
        let mid = res.body["message_id"].as_str().unwrap().to_string();

        let res = send(
            &app,
            "GET",
            &format!("/chatbot/threads/{thread}/messages/{mid}"),
            Some(&cookie),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{role}: {}", res.body);

        let res = send(
            &app,
            "GET",
            &format!("/chatbot/threads/{thread}/messages"),
            Some(&cookie),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{role}: {}", res.body);
        assert_eq!(common::items(&res.body).len(), 2, "{role}: {}", res.body);

        assert_eq!(
            open_stream(&app, &cookie, &thread, &mid).await,
            StatusCode::OK,
            "{role} may open the stream"
        );

        let res = send(&app, "GET", "/chatbot/threads", Some(&cookie), None).await;
        assert_eq!(res.status, StatusCode::OK, "{role}: {}", res.body);
        assert_eq!(common::items(&res.body).len(), 1, "{role}: {}", res.body);

        let res = send(
            &app,
            "DELETE",
            &format!("/chatbot/threads/{thread}"),
            Some(&cookie),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::NO_CONTENT, "{role}: {}", res.body);
    }
    // Every thread deleted, every turn cascaded with it.
    assert_eq!(chat_rows(&db).await, (0, 0));
}

#[tokio::test]
async fn chat_thread_can_be_renamed_and_cleared() {
    // Renaming is how a user files a thread, so it counts as activity and the
    // thread moves to the top of the list. Nothing renames a thread by itself:
    // there is no auto-titling from the first message.
    let (app, _db) = chat_app(None).await;
    let ali = login(&app, "ali").await;
    let first = new_thread(&app, &ali).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let second = new_thread(&app, &ali).await;

    let listed = send(&app, "GET", "/chatbot/threads", Some(&ali), None).await;
    let items = common::items(&listed.body);
    assert_eq!(id_of(&items[0]), second, "newest activity first");
    let before = items[1]["updated_at"].as_i64().expect("updated_at");
    assert!(items[1]["title"].is_null(), "{}", listed.body);

    // The stamp is in whole milliseconds; make sure the clock has moved.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let res = send(
        &app,
        "PATCH",
        &format!("/chatbot/threads/{first}"),
        Some(&ali),
        Some(json!({ "title": "  Fizik ödevi  " })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["title"], "Fizik ödevi", "trimmed, not stored raw");
    assert_eq!(id_of(&res.body), first);
    let stamped = res.body["updated_at"].as_i64().expect("updated_at");
    assert!(stamped > before, "{stamped} !> {before}");

    // The rename is durable and re-sorted the list.
    let listed = send(&app, "GET", "/chatbot/threads", Some(&ali), None).await;
    let items = common::items(&listed.body);
    assert_eq!(id_of(&items[0]), first, "the renamed thread is now first");
    assert_eq!(items[0]["title"], "Fizik ödevi");

    // `null` — and a blank string — clear the name back to untitled.
    for clearing in [json!({ "title": null }), json!({ "title": "   " })] {
        let res = send(
            &app,
            "PATCH",
            &format!("/chatbot/threads/{first}"),
            Some(&ali),
            Some(clearing.clone()),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{clearing}: {}", res.body);
        assert!(res.body["title"].is_null(), "{clearing}: {}", res.body);

        // Restore a name so the next iteration has something to clear.
        send(
            &app,
            "PATCH",
            &format!("/chatbot/threads/{first}"),
            Some(&ali),
            Some(json!({ "title": "geri" })),
        )
        .await;
    }

    // Too long is a 400, and the stored name is untouched.
    let res = send(
        &app,
        "PATCH",
        &format!("/chatbot/threads/{first}"),
        Some(&ali),
        Some(json!({ "title": "é".repeat(201) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/chatbot/threads/{first}"),
        Some(&ali),
        Some(json!({ "title": "é".repeat(200) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // A thread that never existed is the same 404 a foreign one gets.
    let res = send(
        &app,
        "PATCH",
        "/chatbot/threads/nosuchthread",
        Some(&ali),
        Some(json!({ "title": "hayalet" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
    // And `second` was never touched by any of it.
    let res = send(
        &app,
        "GET",
        &format!("/chatbot/threads/{second}/messages"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

#[tokio::test]
async fn chat_rate_limit_tier_is_configurable_and_zero_disables_it() {
    // `RATE_LIMIT_CHATBOT_PER_MINUTE`, the third tier, behaves like the two IP
    // ones: the configured number is honoured exactly and `0` switches it off.
    // No bridge is needed — the limiter is charged before the availability
    // gate, so a metered turn answers 503 and only the refused one answers 429.
    use hezarfen_backend::rate_limit::UserRateLimiter;

    let (app, _db) = chat_app_limited(None, UserRateLimiter::per_user_minute(3)).await;
    let ali = login(&app, "ali").await;
    let thread = new_thread(&app, &ali).await;
    for n in 1..=3 {
        let res = post_turn(&app, &ali, &thread, &format!("soru {n}")).await;
        assert_eq!(
            res.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "turn {n} was inside the budget: {}",
            res.body
        );
    }
    let res = post_turn(&app, &ali, &thread, "dorduncu").await;
    assert_eq!(res.status, StatusCode::TOO_MANY_REQUESTS, "{}", res.body);
    // Per user, still: a second caller starts with a full budget.
    let veli = login(&app, "veli").await;
    let other = new_thread(&app, &veli).await;
    assert_eq!(
        post_turn(&app, &veli, &other, "benim ilk sorum")
            .await
            .status,
        StatusCode::SERVICE_UNAVAILABLE
    );

    // `0` disables the tier: well past the shipped default, nothing is refused.
    let (app, _db) = chat_app_limited(None, UserRateLimiter::per_user_minute(0)).await;
    let ali = login(&app, "ali").await;
    let thread = new_thread(&app, &ali).await;
    for n in 1..=40 {
        let res = post_turn(&app, &ali, &thread, &format!("soru {n}")).await;
        assert_eq!(
            res.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "turn {n} was rate-limited with the tier off: {}",
            res.body
        );
    }
}

// --- appointments ----------------------------------------------------------

const HOUR_MS: i64 = 3_600_000;
const WEEK_MS: i64 = 7 * 24 * HOUR_MS;

/// Publish availability from a full JSON body (no assertion). The response is
/// always an array: one element for a one-off, one per occurrence for a weekly.
async fn publish_slots(app: &axum::Router, cookie: &str, body: serde_json::Value) -> common::Res {
    send(app, "POST", "/appointments/slots", Some(cookie), Some(body)).await
}

/// Publish one slot as `cookie` (asserts 201); returns its id.
async fn publish_slot(app: &axum::Router, cookie: &str, starts_at: i64, ends_at: i64) -> String {
    let res = publish_slots(
        app,
        cookie,
        json!({ "starts_at": starts_at, "ends_at": ends_at }),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "publish slot: {}",
        res.body
    );
    res.body[0]["id"].as_str().expect("slot id").to_string()
}

/// Ask for a meeting on `slot` (no assertion).
async fn book_slot(app: &axum::Router, cookie: &str, slot: &str) -> common::Res {
    send(
        app,
        "POST",
        "/appointments",
        Some(cookie),
        Some(json!({ "slot": slot, "reason": "görüşmek istiyorum" })),
    )
    .await
}

/// Book `slot`, asserting it lands pending; returns the booking id.
async fn book_ok(app: &axum::Router, cookie: &str, slot: &str) -> String {
    let res = book_slot(app, cookie, slot).await;
    assert_eq!(res.status, StatusCode::CREATED, "book slot: {}", res.body);
    assert_eq!(res.body["status"], "pending");
    id_of(&res.body)
}

/// `PATCH /appointments/{id}/{action}` — approve, reject, cancel, or either
/// answer to a counter-proposal (no assertion, no body).
async fn decide(app: &axum::Router, cookie: &str, id: &str, action: &str) -> common::Res {
    send(
        app,
        "PATCH",
        &format!("/appointments/{id}/{action}"),
        Some(cookie),
        None,
    )
    .await
}

/// Counter-propose another window for `id` (no assertion).
async fn propose(
    app: &axum::Router,
    cookie: &str,
    id: &str,
    starts_at: i64,
    ends_at: i64,
) -> common::Res {
    send(
        app,
        "PATCH",
        &format!("/appointments/{id}/reschedule"),
        Some(cookie),
        Some(json!({ "starts_at": starts_at, "ends_at": ends_at })),
    )
    .await
}

/// Accept the standing counter-proposal on `id`, naming the window the
/// requester read — the accept pins *which* proposal it answers, so the body
/// is required (a bodyless PATCH is a `415`, not a `400`).
async fn accept_reschedule(
    app: &axum::Router,
    cookie: &str,
    id: &str,
    starts_at: i64,
    ends_at: i64,
) -> common::Res {
    send(
        app,
        "PATCH",
        &format!("/appointments/{id}/reschedule/accept"),
        Some(cookie),
        Some(json!({ "proposed_starts_at": starts_at, "proposed_ends_at": ends_at })),
    )
    .await
}

#[tokio::test]
async fn appointment_book_and_approve_holds_the_slot() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let ali_id = me_id(&app, &ali).await;
    let veli = login(&app, "veli").await;
    let veli_id = me_id(&app, &veli).await;
    let ayse = login(&app, "ayse").await;
    let now = Timestamp::now().as_millis();

    let res = publish_slots(
        &app,
        &ali,
        json!({ "starts_at": now + HOUR_MS, "ends_at": now + 2 * HOUR_MS,
                "note": "ofis saati" }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body.as_array().expect("array body").len(), 1);
    let slot = res.body[0]["id"].as_str().expect("slot id").to_string();
    assert_eq!(res.body[0]["teacher"]["id"], ali_id.as_str());
    assert_eq!(res.body[0]["note"], "ofis saati");
    assert!(res.body[0]["series"].is_null(), "a one-off has no series");

    // Booking lands pending, echoing the slot's own window.
    let res = book_slot(&app, &veli, &slot).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["status"], "pending");
    assert_eq!(res.body["requester"]["id"], veli_id.as_str());
    assert_eq!(res.body["teacher"]["id"], ali_id.as_str());
    assert_eq!(res.body["starts_at"], now + HOUR_MS);
    assert!(res.body["decided_by"].is_null(), "pending has no decider");
    let booking = id_of(&res.body);

    // The pending request already holds the slot against everyone else.
    let res = book_slot(&app, &ayse, &slot).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    let res = decide(&app, &ali, &booking, "approve").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "approved");
    assert_eq!(res.body["decided_by"]["id"], ali_id.as_str());

    // Both sides read it back from their own list.
    let mine = send(&app, "GET", "/appointments", Some(&veli), None).await;
    assert_eq!(common::items(&mine.body).len(), 1);
    let inbox = send(&app, "GET", "/appointments", Some(&ali), None).await;
    assert_eq!(common::items(&inbox.body)[0]["id"], booking.as_str());
}

#[tokio::test]
async fn rejecting_a_booking_frees_the_slot() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let ali_id = me_id(&app, &ali).await;
    let veli = login(&app, "veli").await;
    let ayse = login(&app, "ayse").await;
    let now = Timestamp::now().as_millis();
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;

    // A bodyless reject records the decider and no reason.
    let booking = book_ok(&app, &veli, &slot).await;
    let res = decide(&app, &ali, &booking, "reject").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "rejected");
    assert_eq!(res.body["decided_by"]["id"], ali_id.as_str());
    assert!(res.body["reject_reason"].is_null(), "no reason was given");

    // A reject carrying a reason records both the decider and the reason, and
    // an over-long reason is refused before the row changes.
    let booking = book_ok(&app, &ayse, &slot).await;
    let res = send(
        &app,
        "PATCH",
        &format!("/appointments/{booking}/reject"),
        Some(&ali),
        Some(json!({ "reason": "x".repeat(2000) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/appointments/{booking}/reject"),
        Some(&ali),
        Some(json!({ "reason": "Bu saatte müsait değilim" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "rejected");
    assert_eq!(res.body["decided_by"]["id"], ali_id.as_str());
    assert_eq!(res.body["reject_reason"], "Bu saatte müsait değilim");

    // Occupancy counts live rows only, so the next person may take it.
    let res = book_slot(&app, &ayse, &slot).await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "a rejected booking must not keep the seat: {}",
        res.body
    );
    // And the rejected one cannot be decided twice.
    let res = decide(&app, &ali, &booking, "approve").await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
}

#[tokio::test]
async fn approval_refuses_a_teacher_double_booking() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let ayse = login(&app, "ayse").await;
    let now = Timestamp::now().as_millis();

    // A teacher can no longer publish two colliding slots, so the collision is
    // reached the only way left: two non-overlapping slots, then a reschedule
    // proposal that drags the second onto the first's just-approved window.
    let first = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let second = publish_slot(&app, &ali, now + 3 * HOUR_MS, now + 4 * HOUR_MS).await;
    let one = book_ok(&app, &veli, &first).await;
    let two = book_ok(&app, &ayse, &second).await;

    assert_eq!(
        decide(&app, &ali, &one, "approve").await.status,
        StatusCode::OK
    );
    // Teacher counter-proposes a window overlapping the just-approved meeting;
    // accepting it is approval at the proposed time, and that collision is what
    // must be refused.
    let res = propose(
        &app,
        &ali,
        &two,
        now + HOUR_MS + 600_000,
        now + 2 * HOUR_MS + 600_000,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = accept_reschedule(
        &app,
        &ayse,
        &two,
        now + HOUR_MS + 600_000,
        now + 2 * HOUR_MS + 600_000,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    // Refused, not half-applied: it is still pending and still decidable.
    let res = decide(&app, &ali, &two, "reject").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

#[tokio::test]
async fn approval_refuses_a_requester_double_booking() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let mert = login_as(&app, &db, "mert", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();

    // One student, two different teachers, overlapping times. Both requests
    // stand (nothing is committed yet); the second approval collides.
    let first = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let second = publish_slot(&app, &mert, now + HOUR_MS + 600_000, now + 3 * HOUR_MS).await;
    let one = book_ok(&app, &veli, &first).await;
    let two = book_ok(&app, &veli, &second).await;

    assert_eq!(
        decide(&app, &ali, &one, "approve").await.status,
        StatusCode::OK
    );
    let res = decide(&app, &mert, &two, "approve").await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // Booking a fresh overlap once one is committed is refused up front. The
    // slot comes from a third teacher: mert's calendar already holds `second`,
    // which a same-teacher slot at this window would now overlap at publish.
    let ahmet = login_as(&app, &db, "ahmet", "teacher").await;
    let third = publish_slot(&app, &ahmet, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let res = book_slot(&app, &veli, &third).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
}

#[tokio::test]
async fn touching_windows_never_collide() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();

    // Half-open windows: 10:00–11:00 and 11:00–12:00 are back-to-back, not a
    // clash — for the teacher and for the student alike.
    let first = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let second = publish_slot(&app, &ali, now + 2 * HOUR_MS, now + 3 * HOUR_MS).await;
    let one = book_ok(&app, &veli, &first).await;
    let two = book_ok(&app, &veli, &second).await;

    for booking in [&one, &two] {
        let res = decide(&app, &ali, booking, "approve").await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        assert_eq!(res.body["status"], "approved");
    }
}

#[tokio::test]
async fn reschedule_returns_to_pending_until_the_requester_accepts() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let ali_id = me_id(&app, &ali).await;
    let veli = login(&app, "veli").await;
    let ayse = login(&app, "ayse").await;
    let now = Timestamp::now().as_millis();
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot).await;
    assert_eq!(
        decide(&app, &ali, &booking, "approve").await.status,
        StatusCode::OK
    );

    // A counter-proposal un-commits the approved meeting rather than moving it
    // silently.
    let res = propose(&app, &ali, &booking, now + 3 * HOUR_MS, now + 4 * HOUR_MS).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "pending");
    assert!(res.body["decided_by"].is_null(), "approval was withdrawn");
    assert_eq!(res.body["proposed_starts_at"], now + 3 * HOUR_MS);
    assert_eq!(res.body["proposed_ends_at"], now + 4 * HOUR_MS);
    assert_eq!(res.body["proposed_by"]["id"], ali_id.as_str());
    // The effective window already reads as the proposed one.
    assert_eq!(res.body["starts_at"], now + 3 * HOUR_MS);

    // Only the requester may answer it. A valid pin still 403s for the wrong
    // caller: `ensure_requester` runs ahead of the domain either way.
    let res = accept_reschedule(&app, &ayse, &booking, now + 3 * HOUR_MS, now + 4 * HOUR_MS).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = accept_reschedule(&app, &ali, &booking, now + 3 * HOUR_MS, now + 4 * HOUR_MS).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);

    let res = accept_reschedule(&app, &veli, &booking, now + 3 * HOUR_MS, now + 4 * HOUR_MS).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "approved");
    assert_eq!(res.body["starts_at"], now + 3 * HOUR_MS);
    assert_eq!(res.body["ends_at"], now + 4 * HOUR_MS);
}

#[tokio::test]
async fn a_started_proposal_is_refused() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot).await;

    // A proposal 30s into the past is inside the publishing grace, so the times
    // themselves validate — but the window is already underway, and a meeting
    // that began is one `cancel` refuses to undo. Refused at the proposal, not
    // left for the requester to agree to.
    let res = propose(&app, &ali, &booking, now - 30_000, now + HOUR_MS).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    // Refused, not half-applied: the teacher can still propose a real time.
    let res = send(&app, "GET", "/appointments", Some(&veli), None).await;
    assert_eq!(common::items(&res.body)[0]["status"], "pending");
    assert!(
        common::items(&res.body)[0]["proposed_starts_at"].is_null(),
        "a refused proposal leaves nothing behind"
    );

    assert_eq!(
        propose(&app, &ali, &booking, now + 3 * HOUR_MS, now + 4 * HOUR_MS)
            .await
            .status,
        StatusCode::OK
    );
    let res = accept_reschedule(&app, &veli, &booking, now + 3 * HOUR_MS, now + 4 * HOUR_MS).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "approved");
}

#[tokio::test]
async fn declining_a_reschedule_cancels_the_booking() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let veli_id = me_id(&app, &veli).await;
    let ayse = login(&app, "ayse").await;
    let now = Timestamp::now().as_millis();
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot).await;

    // Nothing proposed yet: there is nothing to decline.
    let res = decide(&app, &veli, &booking, "reschedule/decline").await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    assert_eq!(
        propose(&app, &ali, &booking, now + 3 * HOUR_MS, now + 4 * HOUR_MS)
            .await
            .status,
        StatusCode::OK
    );
    let res = decide(&app, &veli, &booking, "reschedule/decline").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "cancelled");
    // The decliner is recorded as the canceller, with no reason.
    assert_eq!(res.body["cancelled_by"]["id"], veli_id.as_str());
    assert!(res.body["cancel_reason"].is_null());
    // The refused proposal stays readable on the cancelled row.
    assert_eq!(res.body["proposed_starts_at"], now + 3 * HOUR_MS);

    // And the slot is free again.
    let res = book_slot(&app, &ayse, &slot).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
}

/// Declining cancels, so it answers to the cancel deadline. The guard lives in
/// `service::appointment::cancel` itself — when it sat in the cancel *handler*, decline
/// went straight past it and called off a meeting already underway.
#[tokio::test]
async fn declining_a_started_reschedule_is_refused() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot).await;

    // A proposal that is still ahead when made, and underway a moment later:
    // the only way an effective window opens with the booking still live.
    let opens_at = Timestamp::now().as_millis() + 700;
    assert_eq!(
        propose(&app, &ali, &booking, opens_at, opens_at + HOUR_MS)
            .await
            .status,
        StatusCode::OK
    );
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    let res = decide(&app, &veli, &booking, "reschedule/decline").await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    // The plain cancel door answers the same, and the booking survives both.
    let res = decide(&app, &veli, &booking, "cancel").await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let res = send(&app, "GET", "/appointments", Some(&veli), None).await;
    assert_eq!(common::items(&res.body)[0]["status"], "pending");

    // Moved back into the future, declining cancels as it always did.
    assert_eq!(
        propose(&app, &ali, &booking, now + 3 * HOUR_MS, now + 4 * HOUR_MS)
            .await
            .status,
        StatusCode::OK
    );
    let res = decide(&app, &veli, &booking, "reschedule/decline").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "cancelled");
}

#[tokio::test]
async fn accepting_a_reschedule_re_runs_the_overlap_guard() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let mert = login_as(&app, &db, "mert", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();

    // Committed with ali at +5h.
    let taken = publish_slot(&app, &ali, now + 5 * HOUR_MS, now + 6 * HOUR_MS).await;
    let committed = book_ok(&app, &veli, &taken).await;
    assert_eq!(
        decide(&app, &ali, &committed, "approve").await.status,
        StatusCode::OK
    );

    // Mert counter-proposes straight onto that hour; accepting would commit
    // the student twice, so it is refused.
    let slot = publish_slot(&app, &mert, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot).await;
    assert_eq!(
        propose(&app, &mert, &booking, now + 5 * HOUR_MS, now + 6 * HOUR_MS)
            .await
            .status,
        StatusCode::OK
    );
    let res = accept_reschedule(&app, &veli, &booking, now + 5 * HOUR_MS, now + 6 * HOUR_MS).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert_eq!(
        send(&app, "GET", "/appointments", Some(&veli), None)
            .await
            .body["items"][0]["status"],
        "pending",
        "a refused acceptance leaves the booking pending"
    );
}

/// Only the requester (student/parent) may cancel a booking — not the slot's
/// teacher, not an unrelated student. Deciders reject or reschedule instead.
#[tokio::test]
async fn only_the_requester_cancels() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let veli_id = me_id(&app, &veli).await;
    let ayse = login(&app, "ayse").await;
    let now = Timestamp::now().as_millis();

    // The requester's own call, on an approved meeting. A bodyless cancel
    // records the actor and no reason.
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot).await;
    assert_eq!(
        decide(&app, &ali, &booking, "approve").await.status,
        StatusCode::OK
    );
    let res = decide(&app, &veli, &booking, "cancel").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "cancelled");
    assert_eq!(res.body["cancelled_by"]["id"], veli_id.as_str());
    assert!(res.body["cancel_reason"].is_null(), "no reason was given");

    // A cancel carrying a reason records both the actor and the reason, and an
    // over-long reason is refused before the row changes.
    let booking = book_ok(&app, &veli, &slot).await;
    let res = send(
        &app,
        "PATCH",
        &format!("/appointments/{booking}/cancel"),
        Some(&veli),
        Some(json!({ "reason": "x".repeat(2000) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/appointments/{booking}/cancel"),
        Some(&veli),
        Some(json!({ "reason": "Rahatsızlandım" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "cancelled");
    assert_eq!(res.body["cancelled_by"]["id"], veli_id.as_str());
    assert_eq!(res.body["cancel_reason"], "Rahatsızlandım");

    // The slot's teacher may NOT cancel — cancelling is the requester's lever;
    // a decider ends a booking by rejecting (pending) or rescheduling.
    let booking = book_ok(&app, &ayse, &slot).await;
    let res = decide(&app, &ali, &booking, "cancel").await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "owner-teacher: {}",
        res.body
    );
    // An unrelated student may not either.
    let res = decide(&app, &veli, &booking, "cancel").await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "outsider: {}", res.body);
    // The requester can, and a settled booking cannot be cancelled twice.
    assert_eq!(
        decide(&app, &ayse, &booking, "cancel").await.status,
        StatusCode::OK
    );
    let res = decide(&app, &ayse, &booking, "cancel").await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
}

/// A student is not a decider: rejecting a booking is a teacher/manager call,
/// so the requester calling `reject` on their own booking is refused.
#[tokio::test]
async fn student_cannot_reject_a_booking() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot).await;

    let res = decide(&app, &veli, &booking, "reject").await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
}

/// A slot-owning teacher may not *cancel* a booking — rejecting (pending) or
/// rescheduling is their lever; cancelling belongs to the requester alone.
#[tokio::test]
async fn teacher_cannot_cancel_a_booking() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();

    // Pending booking: owner-teacher cancel is refused (the booking stays live).
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot).await;
    let res = decide(&app, &ali, &booking, "cancel").await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "pending: {}", res.body);

    // Approved booking on a fresh slot: same refusal.
    let slot2 = publish_slot(&app, &ali, now + 3 * HOUR_MS, now + 4 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot2).await;
    assert_eq!(
        decide(&app, &ali, &booking, "approve").await.status,
        StatusCode::OK
    );
    let res = decide(&app, &ali, &booking, "cancel").await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "approved: {}", res.body);
}

/// A manager who is not the requester also cannot cancel — no oversight override
/// exists; only the requester (student/parent) cancels.
#[tokio::test]
async fn manager_cannot_cancel_a_booking() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let mgr = login_as(&app, &db, "mgr", "manager").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot).await;

    let res = decide(&app, &mgr, &booking, "cancel").await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "manager: {}", res.body);
}

#[tokio::test]
async fn a_started_slot_cannot_be_booked() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();

    // A window that opened 30s ago is inside the publishing grace, so the slot
    // is created — but the meeting has begun, and a meeting that began is
    // history, not a plan. Booking it would land a row `cancel` refuses to
    // undo, stuck for good.
    let underway = publish_slot(&app, &ali, now - 30_000, now + HOUR_MS).await;
    let res = book_slot(&app, &veli, &underway).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // A window still ahead is untouched by the guard.
    let upcoming = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let res = book_slot(&app, &veli, &upcoming).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    // ...and the calendar the student browses is bounded the same way: the
    // underway slot is still a live row (the `409` above proves it), but
    // offering it would be an entry that can only be refused.
    let res = send(&app, "GET", "/appointments/slots", Some(&veli), None).await;
    let offered: Vec<&str> = common::items(&res.body)
        .iter()
        .map(|slot| slot["id"].as_str().expect("slot id"))
        .collect();
    assert_eq!(offered, [upcoming.as_str()], "{}", res.body);
    // The teacher's own calendar keeps it: past occurrences are their history.
    let res = send(&app, "GET", "/appointments/slots", Some(&ali), None).await;
    assert_eq!(common::total(&res.body), 2, "{}", res.body);
}

#[tokio::test]
async fn slot_delete_waits_for_the_booking_to_settle() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot).await;

    let uri = format!("/appointments/slots/{slot}");
    let res = send(&app, "DELETE", &uri, Some(&ali), None).await;
    assert_eq!(
        res.status,
        StatusCode::CONFLICT,
        "someone is waiting on this slot: {}",
        res.body
    );
    assert_eq!(
        decide(&app, &veli, &booking, "cancel").await.status,
        StatusCode::OK
    );
    let res = send(&app, "DELETE", &uri, Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    // The settled booking goes with the slot, in the same transaction — it is
    // history of a meeting that no longer exists, and a row left pointing at a
    // deleted slot is a dangling link every read of it has to survive. The
    // `204` alone never proved the cascade ran, so the count is read from the
    // database (`pay_row_count` is this file's plain table count).
    assert_eq!(
        pay_row_count(&db, "appointment").await,
        0,
        "the cancelled booking {booking} outlived its slot"
    );
    assert_eq!(
        send(&app, "DELETE", &uri, Some(&ali), None).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn weekly_slots_share_a_series_and_are_dropped_together() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();
    let starts_at = now + HOUR_MS;

    // `until` is inclusive, so three weeks out is four occurrences.
    let res = publish_slots(
        &app,
        &ali,
        json!({ "starts_at": starts_at, "ends_at": starts_at + HOUR_MS,
                "repeat_weekly": true, "until": starts_at + 3 * WEEK_MS }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let slots = res.body.as_array().expect("array body").clone();
    assert_eq!(slots.len(), 4);
    let series = slots[0]["series"].as_str().expect("series id").to_string();
    for (week, slot) in slots.iter().enumerate() {
        assert_eq!(slot["series"], series.as_str(), "one publish, one series");
        assert_eq!(slot["starts_at"], starts_at + week as i64 * WEEK_MS);
    }

    // `repeat_weekly` without an end, and past the 52-occurrence cap.
    let res = publish_slots(
        &app,
        &ali,
        json!({ "starts_at": starts_at, "ends_at": starts_at + HOUR_MS,
                "repeat_weekly": true }),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    let res = publish_slots(
        &app,
        &ali,
        json!({ "starts_at": starts_at, "ends_at": starts_at + HOUR_MS,
                "repeat_weekly": true, "until": starts_at + 52 * WEEK_MS }),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::BAD_REQUEST,
        "53 occurrences: {}",
        res.body
    );

    // An end at `i64::MAX` clears every gate — future, not inverted, 52
    // occurrences — and then overflows on the first weekly shift. Unchecked it
    // wrapped the end below the start, and an inverted window never overlaps
    // anything, so the double-booking guard would have gone blind.
    let res = publish_slots(
        &app,
        &ali,
        json!({ "starts_at": starts_at, "ends_at": i64::MAX,
                "repeat_weekly": true, "until": starts_at + 51 * WEEK_MS }),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::BAD_REQUEST,
        "a shift that overflows: {}",
        res.body
    );

    // All-or-nothing: one waiting requester anywhere in the series blocks it.
    let booked = slots[1]["id"].as_str().expect("slot id");
    let booking = book_ok(&app, &veli, booked).await;
    let uri = format!("/appointments/slots/series/{series}");
    let res = send(&app, "DELETE", &uri, Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    assert_eq!(
        decide(&app, &ali, &booking, "reject").await.status,
        StatusCode::OK
    );
    let res = send(&app, "DELETE", &uri, Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    let res = send(&app, "GET", "/appointments/slots", Some(&ali), None).await;
    assert_eq!(common::total(&res.body), 0, "the whole series went");
    assert_eq!(
        send(&app, "DELETE", &uri, Some(&ali), None).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn only_students_and_parents_book_appointments() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let mert = login_as(&app, &db, "mert", "teacher").await;
    let mudur = login_as(&app, &db, "mudur", "manager").await;
    let admin = login_as(&app, &db, "yonetici", "admin").await;
    let anne = login_as(&app, &db, "anne", "parent").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;

    // Staff arrange between themselves off this API.
    for (who, label) in [(&mert, "teacher"), (&mudur, "manager"), (&admin, "admin")] {
        let res = book_slot(&app, who, &slot).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{label}: {}", res.body);
    }
    // A parent books for themselves — the parent-teacher conference.
    let res = book_slot(&app, &anne, &slot).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let second = publish_slot(&app, &ali, now + 3 * HOUR_MS, now + 4 * HOUR_MS).await;
    let res = book_slot(&app, &veli, &second).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
}

#[tokio::test]
async fn only_the_slots_teacher_decides_its_bookings() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let mert = login_as(&app, &db, "mert", "teacher").await;
    let mudur = login_as(&app, &db, "mudur", "manager").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot).await;

    // Another teacher's calendar is not theirs to run.
    for action in ["approve", "reject"] {
        let res = decide(&app, &mert, &booking, action).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{action}: {}", res.body);
    }
    let res = propose(&app, &mert, &booking, now + 3 * HOUR_MS, now + 4 * HOUR_MS).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/appointments/slots/{slot}"),
        Some(&mert),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    // The requester cannot approve their own request either.
    assert_eq!(
        decide(&app, &veli, &booking, "approve").await.status,
        StatusCode::FORBIDDEN
    );
    // A manager may decide anyone's.
    let res = decide(&app, &mudur, &booking, "approve").await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "approved");
}

#[tokio::test]
async fn appointment_times_must_be_sane_and_future() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();

    // Past (beyond the 60s grace), empty, and inverted windows.
    for (body, label) in [
        (
            json!({ "starts_at": now - 10 * HOUR_MS, "ends_at": now - 9 * HOUR_MS }),
            "past",
        ),
        (
            json!({ "starts_at": now + HOUR_MS, "ends_at": now + HOUR_MS }),
            "empty",
        ),
        (
            json!({ "starts_at": now + 2 * HOUR_MS, "ends_at": now + HOUR_MS }),
            "inverted",
        ),
    ] {
        let res = publish_slots(&app, &ali, body).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{label}: {}", res.body);
    }

    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;
    let booking = book_ok(&app, &veli, &slot).await;
    // A counter-proposal answers to the same rules.
    for (starts_at, ends_at, label) in [
        (now - 10 * HOUR_MS, now - 9 * HOUR_MS, "past"),
        (now + HOUR_MS, now + HOUR_MS, "empty"),
        (now + 2 * HOUR_MS, now + HOUR_MS, "inverted"),
    ] {
        let res = propose(&app, &ali, &booking, starts_at, ends_at).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{label}: {}", res.body);
    }
    // A blank reason is refused too — the teacher decides on it.
    let res = send(
        &app,
        "POST",
        "/appointments",
        Some(&veli),
        Some(json!({ "slot": slot, "reason": "" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
}

#[tokio::test]
async fn a_demoted_teachers_slots_go_inert() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let veli = login(&app, "veli").await;
    let now = Timestamp::now().as_millis();
    let slot = publish_slot(&app, &ali, now + HOUR_MS, now + 2 * HOUR_MS).await;

    let res = send(&app, "GET", "/appointments/slots", Some(&veli), None).await;
    assert_eq!(common::items(&res.body).len(), 1, "{}", res.body);

    // The row survives the demotion; the offer does not.
    set_role(&db, "ali", "student").await;
    let res = send(&app, "GET", "/appointments/slots", Some(&veli), None).await;
    assert_eq!(common::total(&res.body), 0, "an inert slot is not offered");
    let res = book_slot(&app, &veli, &slot).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
}

// --- per-attempt history (retakes preserve prior sittings) ------------------

/// Sit an exam, answer + draw, retake, answer + draw differently — then the
/// teacher's per-attempt history endpoints still surface the seq-1 sitting
/// alongside seq-2: the attempts list is `[1, 2]`, and each seq's answer text
/// and drawing bytes are its own, not the latest sitting's.
#[tokio::test]
async fn attempt_history_preserves_each_prior_sitting() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "hist_t", "teacher").await;
    let student = login(&app, "hist_s").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "biology").await;
    let subject = create_subject(&app, &teacher, &course, "cells").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "cell quiz", "kind": "quiz", "mode": "open", "max_attempts": 2 }),
    )
    .await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Name an organelle.", "kind": "text", "points": 10 }),
    )
    .await;
    let question = id_of(&question_body);
    let own_image = format!("/exams/{exam}/attempt/answers/{question}/image");

    // Seq 1: answer, draw, finish.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "sit seq 1: {}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": question, "text": "mitochondria" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let (status, _) = post_image(&app, &student, &own_image, "image/png", b"seq1-draw").await;
    assert_eq!(status, StatusCode::CREATED);
    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    // Seq 2: retake, answer + draw differently.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.body["attempt"], 2, "retake is the second sitting");
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": question, "text": "chloroplast" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let (status, _) = post_image(&app, &student, &own_image, "image/png", b"seq2-draw").await;
    assert_eq!(status, StatusCode::CREATED);

    // The attempts list carries both sittings, ascending.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/students/{student_id}/attempts"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body, json!([1, 2]));

    // Each sitting keeps its own answer text — seq 1 was NOT overwritten.
    let seq1 = send(
        &app,
        "GET",
        &format!("/exams/{exam}/students/{student_id}/attempts/1/answers"),
        Some(&teacher),
        None,
    )
    .await;
    let seq2 = send(
        &app,
        "GET",
        &format!("/exams/{exam}/students/{student_id}/attempts/2/answers"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(seq1.body["answers"][0]["text"], "mitochondria");
    assert_eq!(seq2.body["answers"][0]["text"], "chloroplast");
    assert_ne!(
        seq1.body["answers"][0]["text"],
        seq2.body["answers"][0]["text"]
    );

    // Each sitting keeps its own drawing bytes.
    for (seq, want) in [(1, b"seq1-draw".as_slice()), (2, b"seq2-draw".as_slice())] {
        let uri =
            format!("/exams/{exam}/students/{student_id}/attempts/{seq}/answers/{question}/image");
        let (status, _, bytes) =
            common::send_raw(&app, "GET", &uri, Some(&teacher), None, Vec::new()).await;
        assert_eq!(status, StatusCode::OK, "seq {seq} image");
        assert_eq!(bytes, want, "seq {seq} kept its own drawing");
    }
}

/// The grade-of-record follows the latest sitting: grading seq 1, then seq 2
/// with a different mark, leaves the roster read showing seq 2's mark — while
/// the full mark-history endpoint returns both, oldest first.
#[tokio::test]
async fn grade_of_record_is_latest_but_history_keeps_both() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "grd_t", "teacher").await;
    let student = login(&app, "grd_s").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "drill", "kind": "quiz", "mode": "open", "max_attempts": 2 }),
    )
    .await;

    let results_uri = format!("/exams/{exam}/results");

    // Seq 1: sit, grade 40, finish.
    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    let res = send(
        &app,
        "POST",
        &results_uri,
        Some(&teacher),
        Some(json!({ "mark": 40, "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    // Seq 2: retake, grade 90 — the mark lands on the new sitting.
    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    let res = send(
        &app,
        "POST",
        &results_uri,
        Some(&teacher),
        Some(json!({ "mark": 90, "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // The roster (latest-per-pair) reports one row, at seq 2's mark.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let rows = common::items(&res.body);
    assert_eq!(rows.len(), 1, "one grade-of-record per student");
    assert_eq!(rows[0]["mark"], 90, "latest sitting is the grade-of-record");

    // The history endpoint keeps both marks, oldest first.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/students/{student_id}/marks"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let marks: Vec<i64> = res
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["mark"].as_i64().unwrap())
        .collect();
    assert_eq!(marks, vec![40, 90], "both sittings' marks, oldest first");
}

/// The per-attempt history endpoints are teacher-only: an enrolled student
/// asking for another student's history is refused, like every grader read.
#[tokio::test]
async fn attempt_history_endpoints_are_teacher_walled() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "wall_t", "teacher").await;
    let student = login(&app, "wall_s").await;
    let student_id = me_id(&app, &student).await;
    let snooper = login(&app, "wall_o").await;

    let course = create_course(&app, &teacher, "history").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "quiz", "kind": "quiz", "mode": "open" }),
    )
    .await;

    // A non-teacher student is refused on every history route for another student.
    for uri in [
        format!("/exams/{exam}/students/{student_id}/attempts"),
        format!("/exams/{exam}/students/{student_id}/attempts/1/answers"),
        format!("/exams/{exam}/students/{student_id}/marks"),
    ] {
        let res = send(&app, "GET", &uri, Some(&snooper), None).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{uri}");
    }
}

// --- student self-review (own-scoped, allow_review + a mark gate it) ---------

/// Sit, answer, teacher grades — but with `allow_review` left at its default
/// (false), the student's own review read is refused: 403, "review not enabled".
#[tokio::test]
async fn self_review_is_forbidden_until_the_teacher_enables_it() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "rev_off_t", "teacher").await;
    let student = login(&app, "rev_off_s").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "physics").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "kinematics", "kind": "quiz", "mode": "open" }),
    )
    .await;

    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        Some(json!({ "mark": 55, "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/review/attempts"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
}

/// `allow_review` on, but the student has no mark yet — nothing to review, so
/// the gate 404s rather than leaking an empty (or in-progress) sheet.
#[tokio::test]
async fn self_review_is_not_found_until_a_mark_exists() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "rev_ungraded_t", "teacher").await;
    let student = login(&app, "rev_ungraded_s").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "chemistry").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "moles", "kind": "quiz", "mode": "open", "allow_review": true }),
    )
    .await;

    // Sat, but never graded — no ExamResult row.
    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;

    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/review/attempts"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
}

/// A draft exam is invisible to students even with review on and a mark in hand:
/// the draft check comes first, so every self-review read is a flat 404.
#[tokio::test]
async fn self_review_of_a_draft_exam_is_not_found() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "rev_draft_t", "teacher").await;
    let student = login(&app, "rev_draft_s").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "geography").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({
            "title": "maps", "kind": "quiz", "mode": "open",
            "allow_review": true, "draft": true,
        }),
    )
    .await;

    for uri in [
        format!("/exams/{exam}/review/attempts"),
        format!("/exams/{exam}/review/attempts/1/answers"),
    ] {
        let res = send(&app, "GET", &uri, Some(&student), None).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{uri}: {}", res.body);
    }
}

/// The happy path across retakes: review on, both sittings graded, and the
/// student reads their own seqs `[1, 2]` plus each sitting's own answer text —
/// seq 1 was not overwritten by the retake.
#[tokio::test]
async fn self_review_returns_own_seqs_and_per_sitting_answers() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "rev_ok_t", "teacher").await;
    let student = login(&app, "rev_ok_s").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "biology").await;
    let subject = create_subject(&app, &teacher, &course, "cells").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({
            "title": "cell quiz", "kind": "quiz", "mode": "open",
            "max_attempts": 2, "allow_review": true,
        }),
    )
    .await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Name an organelle.", "kind": "text", "points": 10 }),
    )
    .await;
    let question = id_of(&question_body);
    let results_uri = format!("/exams/{exam}/results");

    // Seq 1: answer, grade, finish.
    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": question, "text": "mitochondria" })),
    )
    .await;
    send(
        &app,
        "POST",
        &results_uri,
        Some(&teacher),
        Some(json!({ "mark": 40, "user_id": student_id })),
    )
    .await;
    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    // Seq 2: retake, answer differently, grade.
    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": question, "text": "chloroplast" })),
    )
    .await;
    send(
        &app,
        "POST",
        &results_uri,
        Some(&teacher),
        Some(json!({ "mark": 90, "user_id": student_id })),
    )
    .await;
    // Review refuses while the sitting is still writable, so submit it first.
    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;

    // Own seqs, ascending.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/review/attempts"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body, json!([1, 2]));

    // Each sitting keeps its own answer text.
    let seq1 = send(
        &app,
        "GET",
        &format!("/exams/{exam}/review/attempts/1/answers"),
        Some(&student),
        None,
    )
    .await;
    let seq2 = send(
        &app,
        "GET",
        &format!("/exams/{exam}/review/attempts/2/answers"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(seq1.status, StatusCode::OK, "{}", seq1.body);
    assert_eq!(seq1.body["answers"][0]["text"], "mitochondria");
    assert_eq!(seq2.body["answers"][0]["text"], "chloroplast");

    // The answer key: a marked student reads the questions with `correct`
    // exposed (the whole point of review) — the same list the teacher sees.
    let key = send(
        &app,
        "GET",
        &format!("/exams/{exam}/review/questions"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(key.status, StatusCode::OK, "{}", key.body);
    assert_eq!(key.body["items"][0]["id"], question);
    assert!(
        key.body["items"][0]
            .as_object()
            .unwrap()
            .contains_key("correct"),
        "review/questions must expose `correct`: {}",
        key.body
    );
}

/// Own-scoped isolation: two graded students, review on. There is no `{user}`
/// path param, so student A's token resolves to A — A sees A's own seqs and
/// answer text, never B's, even though both are graded on the same exam.
#[tokio::test]
async fn self_review_is_own_scoped_between_students() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "rev_iso_t", "teacher").await;
    let alice = login(&app, "rev_iso_a").await;
    let alice_id = me_id(&app, &alice).await;
    let bob = login(&app, "rev_iso_b").await;
    let bob_id = me_id(&app, &bob).await;

    let course = create_course(&app, &teacher, "history").await;
    let subject = create_subject(&app, &teacher, &course, "rome").await;
    enroll(&app, &teacher, &course, &alice_id).await;
    enroll(&app, &teacher, &course, &bob_id).await;
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "quiz", "kind": "quiz", "mode": "open", "allow_review": true }),
    )
    .await;
    let question_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "Name a consul.", "kind": "text", "points": 10 }),
    )
    .await;
    let question = id_of(&question_body);
    let results_uri = format!("/exams/{exam}/results");

    for (who, id, text, mark) in [
        (&alice, &alice_id, "cicero", 70),
        (&bob, &bob_id, "cato", 30),
    ] {
        send(
            &app,
            "POST",
            &format!("/exams/{exam}/attempt"),
            Some(who),
            None,
        )
        .await;
        send(
            &app,
            "POST",
            &format!("/exams/{exam}/attempt/answers"),
            Some(who),
            Some(json!({ "question_id": question, "text": text })),
        )
        .await;
        send(
            &app,
            "POST",
            &results_uri,
            Some(&teacher),
            Some(json!({ "mark": mark, "user_id": id })),
        )
        .await;
        // Review refuses while the sitting is still writable.
        send(
            &app,
            "POST",
            &format!("/exams/{exam}/attempt/finish"),
            Some(who),
            None,
        )
        .await;
    }

    // Alice's token sees only Alice's data — one seq, her own answer.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/review/attempts"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body, json!([1]));
    let sheet = send(
        &app,
        "GET",
        &format!("/exams/{exam}/review/attempts/1/answers"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(
        sheet.body["answers"][0]["text"], "cicero",
        "A sees her own answer"
    );
    assert_ne!(sheet.body["answers"][0]["text"], "cato", "never B's answer");
}

// --- question bank -----------------------------------------------------------

#[tokio::test]
async fn bank_question_create_get_list_and_instantiate() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bank_t", "teacher").await;
    let course = create_course(&app, &teacher, "algebra").await;
    let subject = create_subject(&app, &teacher, &course, "linear").await;

    // Create a template — subject is origin metadata, not held to a course.
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&teacher),
        Some(json!({ "subject_id": subject, "text": "2 + 2?", "kind": "choice",
                     "points": 10, "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}, {"id": "c2", "text": "5"}], "correct": "c1" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["kind"], "choice");
    assert_eq!(res.body["correct"], res.body["choices"][1]["id"]);
    assert_eq!(res.body["subject"], subject);
    let bid = id_of(&res.body);

    // School-wide read + list.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["text"], "2 + 2?");
    let res = send(&app, "GET", "/bank-questions", Some(&teacher), None).await;
    assert_eq!(common::items(&res.body).len(), 1);

    // `?subject=` matches, and a foreign id excludes.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions?subject={subject}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::items(&res.body).len(), 1);
    let res = send(
        &app,
        "GET",
        "/bank-questions?subject=nope",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::items(&res.body).len(), 0);

    // Instantiate into an exam under one of the course's subjects.
    let now = Timestamp::now().as_millis();
    let exam = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "midterm", "kind": "final", "mode": "sync",
                "starts_at": now - 1_000, "ends_at": now + 600_000 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/from-bank/{bid}"),
        Some(&teacher),
        Some(json!({ "subject_id": subject })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["exam"], exam);
    assert_eq!(res.body["subject"], subject);
    assert_eq!(res.body["correct"], res.body["choices"][1]["id"]);
    assert_eq!(res.body["choices"][1]["text"], "4");

    // The copy is independent — the template still stands.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
}

/// The bank list pages in SQL: newest first, every row reachable by offset,
/// `total` counting the matches rather than the page, and `?q=` filtering
/// server-side. Regression for the whole-table-then-slice version, which hid
/// template #101 onward from a client asking for `limit=100`.
#[tokio::test]
async fn bank_question_list_pages_filters_and_names() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bank_p", "teacher").await;
    let other = login_as(&app, &db, "bank_p2", "teacher").await;
    let course = create_course(&app, &teacher, "algebra").await;
    let subject = create_subject(&app, &teacher, &course, "linear").await;
    let other_subject = create_subject(&app, &teacher, &course, "quadratic").await;

    // 120 templates — more than the clients' `limit=100`.
    for i in 0..120 {
        let res = send(
            &app,
            "POST",
            "/bank-questions",
            Some(&teacher),
            Some(
                json!({ "subject_id": subject, "text": format!("template {i:03}"),
                         "kind": "text", "points": 1 }),
            ),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    }
    // A foreign owner + subject, to prove the filters actually narrow.
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&other),
        Some(
            json!({ "subject_id": other_subject, "text": "someone else's",
                     "kind": "text", "points": 1 }),
        ),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    // Published, so it shows up in the first teacher's paging assertions below;
    // a private one would (correctly) be invisible to them.
    let foreign = id_of(&res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/bank-questions/{foreign}"),
        Some(&other),
        Some(json!({ "visibility": "school" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Page 1: newest first, full window, honest total.
    let res = send(
        &app,
        "GET",
        "/bank-questions?limit=100",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::items(&res.body).len(), 100);
    assert_eq!(res.body["total"], 121);
    assert_eq!(common::items(&res.body)[0]["text"], "someone else's");
    assert_eq!(common::items(&res.body)[1]["text"], "template 119");
    // The names are joined on, not left for the client to look up per row.
    assert_eq!(common::items(&res.body)[1]["subject_name"], "linear");
    assert_eq!(common::items(&res.body)[1]["owner_name"], "bank_p");

    // Item #101 onward is reachable — the bug was that it never was.
    let res = send(
        &app,
        "GET",
        "/bank-questions?limit=100&offset=100",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::items(&res.body).len(), 21);
    assert_eq!(res.body["total"], 121);
    assert_eq!(common::items(&res.body)[0]["text"], "template 020");
    assert_eq!(common::items(&res.body)[20]["text"], "template 000");

    // `?q=` filters server-side: "template 005" sits on page 2 unfiltered, but
    // leads page 1 once filtered — and matches case-insensitively.
    let res = send(
        &app,
        "GET",
        "/bank-questions?limit=100&q=TEMPLATE%20005",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["total"], 1);
    assert_eq!(common::items(&res.body).len(), 1);
    assert_eq!(common::items(&res.body)[0]["text"], "template 005");
    // A blank `q` is no filter at all.
    let res = send(&app, "GET", "/bank-questions?q=", Some(&teacher), None).await;
    assert_eq!(res.body["total"], 121);

    // `owner=me` + `subject=` combine, and still page.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions?owner=me&subject={subject}&limit=100&offset=100"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["total"], 120);
    assert_eq!(common::items(&res.body).len(), 20);
    assert_eq!(common::items(&res.body)[0]["text"], "template 019");
    // The other teacher's `me` is their own single template.
    let res = send(&app, "GET", "/bank-questions?owner=me", Some(&other), None).await;
    assert_eq!(res.body["total"], 1);
    assert_eq!(common::items(&res.body)[0]["owner_name"], "bank_p2");
    // Owner and subject that never co-occur → an empty page, total 0.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions?owner=me&subject={other_subject}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["total"], 0);
    assert!(common::items(&res.body).is_empty());
}

/// `?visibility=` is a real SQL filter (so the client never has to fetch the
/// bank unpaged and window it in memory — the very bug the SQL paging fixed),
/// and it narrows the caller's view without ever widening it.
#[tokio::test]
async fn bank_question_list_filters_by_visibility() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bank_v", "teacher").await;
    let other = login_as(&app, &db, "bank_v2", "teacher").await;
    let course = create_course(&app, &teacher, "biology").await;
    let subject = create_subject(&app, &teacher, &course, "cells").await;
    let other_subject = create_subject(&app, &teacher, &course, "genes").await;

    // Mine: 3 drafts on `subject`, 2 published on `other_subject`.
    let mut published = Vec::new();
    for i in 0..5 {
        let on = if i < 3 { &subject } else { &other_subject };
        let res = send(
            &app,
            "POST",
            "/bank-questions",
            Some(&teacher),
            Some(json!({ "subject_id": on, "text": format!("mine {i}"),
                         "kind": "text", "points": 1 })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
        if i >= 3 {
            published.push(id_of(&res.body));
        }
    }
    for bid in &published {
        let res = send(
            &app,
            "PATCH",
            &format!("/bank-questions/{bid}"),
            Some(&teacher),
            Some(json!({ "visibility": "school" })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    }
    // The other teacher keeps one private template — nobody else may ever see
    // it, whatever they pass in `?visibility=`.
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&other),
        Some(json!({ "subject_id": subject, "text": "their secret",
                     "kind": "text", "points": 1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    // `private` = my drafts; `total` honours the filter, not the whole bank.
    let res = send(
        &app,
        "GET",
        "/bank-questions?visibility=private",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["total"], 3, "{}", res.body);
    assert_eq!(common::items(&res.body).len(), 3);
    assert_eq!(common::items(&res.body)[0]["text"], "mine 2");
    // `school` = the published shelf — and NOT the other teacher's private row.
    let res = send(
        &app,
        "GET",
        "/bank-questions?visibility=school",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["total"], 2, "{}", res.body);
    let texts: Vec<&str> = common::items(&res.body)
        .iter()
        .map(|item| item["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["mine 4", "mine 3"]);
    assert!(!texts.contains(&"their secret"));
    // The stranger's `private` view is empty: the filter can't widen the gate.
    let res = send(
        &app,
        "GET",
        "/bank-questions?visibility=private",
        Some(&teacher),
        None,
    )
    .await;
    assert!(
        !common::items(&res.body)
            .iter()
            .any(|item| item["text"] == "their secret")
    );
    let res = send(
        &app,
        "GET",
        "/bank-questions?visibility=private",
        Some(&other),
        None,
    )
    .await;
    assert_eq!(res.body["total"], 1, "{}", res.body);
    assert_eq!(common::items(&res.body)[0]["text"], "their secret");

    // Composes with `q`, `subject`, `owner`, and with paging.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions?visibility=private&subject={subject}&owner=me&q=MINE%201"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["total"], 1, "{}", res.body);
    assert_eq!(common::items(&res.body)[0]["text"], "mine 1");
    // Right owner and subject, wrong shelf → nothing.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions?visibility=school&subject={subject}&owner=me"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["total"], 0, "{}", res.body);
    assert!(common::items(&res.body).is_empty());
    // Paging windows the filtered set, and `total` stays the filtered total.
    let res = send(
        &app,
        "GET",
        "/bank-questions?visibility=private&limit=2&offset=2",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["total"], 3, "{}", res.body);
    assert_eq!(common::items(&res.body).len(), 1);
    assert_eq!(common::items(&res.body)[0]["text"], "mine 0");

    // An unknown shelf is a 400, worded like the other param errors.
    let res = send(
        &app,
        "GET",
        "/bank-questions?visibility=public",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    assert!(
        res.body["error"]
            .as_str()
            .unwrap()
            .contains("must be private or school"),
        "{}",
        res.body
    );
}

/// The app is EN/TR, so `?q=` must fold Turkish casing: `İ` lowercases to
/// `i` + U+0307 in both Rust and SurrealQL, which used to make `istanbul` and
/// `İSTANBUL` two disjoint searches — a teacher typing lowercase got an empty
/// bank.
#[tokio::test]
async fn bank_question_search_folds_turkish_casing() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bank_tr", "teacher").await;
    let course = create_course(&app, &teacher, "cografya").await;
    let subject = create_subject(&app, &teacher, &course, "iller").await;
    for text in ["İSTANBUL kaç ilçedir?", "istanbul boğazı nerededir?"] {
        let res = send(
            &app,
            "POST",
            "/bank-questions",
            Some(&teacher),
            Some(json!({ "subject_id": subject, "text": text,
                         "kind": "text", "points": 1 })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    }

    // Lowercase needle finds the uppercase row, and every casing finds both.
    for needle in ["istanbul", "%C4%B0STANBUL", "%C4%B1stanbul", "ISTANBUL"] {
        let res = send(
            &app,
            "GET",
            &format!("/bank-questions?q={needle}"),
            Some(&teacher),
            None,
        )
        .await;
        assert_eq!(res.body["total"], 2, "needle {needle} missed: {}", res.body);
    }
    // Diacritics fold too, in both directions.
    for needle in ["ilce", "il%C3%A7e", "bogaz", "bo%C4%9Faz"] {
        let res = send(
            &app,
            "GET",
            &format!("/bank-questions?q={needle}"),
            Some(&teacher),
            None,
        )
        .await;
        assert_eq!(res.body["total"], 1, "needle {needle} missed: {}", res.body);
    }
    // Folding widens the match, it does not match everything.
    let res = send(
        &app,
        "GET",
        "/bank-questions?q=ankara",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["total"], 0);
}

#[tokio::test]
async fn bank_question_owner_gate_cross_course_and_freeze() {
    let (app, db) = app_and_db().await;
    let owner = login_as(&app, &db, "bank_o", "teacher").await;
    let other = login_as(&app, &db, "bank_x", "teacher").await;
    let admin = login_as(&app, &db, "bank_a", "admin").await;

    let course = create_course(&app, &owner, "geo").await;
    let subject = create_subject(&app, &owner, &course, "angles").await;
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&owner),
        Some(json!({ "subject_id": subject, "text": "q", "kind": "text", "points": 5 })),
    )
    .await;
    let bid = id_of(&res.body);
    // Published, so this test is about the *owner* gate alone — the visibility
    // gate (404 while private) has its own test below.
    let res = send(
        &app,
        "PATCH",
        &format!("/bank-questions/{bid}"),
        Some(&owner),
        Some(json!({ "visibility": "school" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // A non-owner teacher reads it, but may not edit or delete.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{bid}"),
        Some(&other),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(
        &app,
        "PATCH",
        &format!("/bank-questions/{bid}"),
        Some(&other),
        Some(json!({ "points": 7 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/bank-questions/{bid}"),
        Some(&other),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    // Admin bypasses ownership.
    let res = send(
        &app,
        "PATCH",
        &format!("/bank-questions/{bid}"),
        Some(&admin),
        Some(json!({ "points": 7 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["points"], 7);

    // Instantiate with a subject from ANOTHER course is a 400.
    let course2 = create_course(&app, &owner, "chem").await;
    let subject2 = create_subject(&app, &owner, &course2, "bonds").await;
    let now = Timestamp::now().as_millis();
    let exam = scheduled_exam(
        &app,
        &owner,
        &course,
        json!({ "title": "t", "kind": "final", "mode": "sync",
                "starts_at": now - 1_000, "ends_at": now + 600_000 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/from-bank/{bid}"),
        Some(&owner),
        Some(json!({ "subject_id": subject2 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // Once a student sits the exam, instantiate freezes (409).
    let student = login(&app, "sinem").await;
    let student_id = me_id(&app, &student).await;
    enroll(&app, &owner, &course, &student_id).await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/from-bank/{bid}"),
        Some(&owner),
        Some(json!({ "subject_id": subject })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
}

/// A bank template carries `correct` — the answer key — so it is `private`
/// until its owner publishes it. Regression for the one-click answer-key
/// broadcast: saving a live exam's question to the bank used to hand the key
/// (and the pictures) to every teacher in the school, permanently and silently.
///
/// Every read path is checked from a second teacher's seat, and each one must
/// answer **404, not 403** — a 403 confirms the template exists.
#[tokio::test]
async fn bank_private_template_is_invisible_until_published() {
    let (app, db) = app_and_db().await;
    let owner = login_as(&app, &db, "bvis_o", "teacher").await;
    let other = login_as(&app, &db, "bvis_x", "teacher").await;
    let course = create_course(&app, &owner, "bio").await;
    let subject = create_subject(&app, &owner, &course, "cells").await;

    // Two templates; only the second is ever published.
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&owner),
        Some(json!({ "subject_id": subject, "text": "Which organelle?", "kind": "choice",
                     "points": 10, "choices": [{"id": "c0", "text": "nucleus"}, {"id": "c1", "text": "ribosome"}], "correct": "c0" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    // New templates are born private — nothing has to opt in to be safe.
    assert_eq!(res.body["visibility"], "private");
    let secret = id_of(&res.body);
    let opt = choice_of(&res.body, 0);
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&owner),
        Some(json!({ "subject_id": subject, "text": "shared later", "kind": "text", "points": 1 })),
    )
    .await;
    let shared = id_of(&res.body);

    let illus = b"private-illustration".as_slice();
    let pic = b"private-choice-pic".as_slice();
    let (status, _) = post_image(
        &app,
        &owner,
        &format!("/bank-questions/{secret}/image"),
        "image/png",
        illus,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = post_image(
        &app,
        &owner,
        &format!("/bank-questions/{secret}/choices/{opt}/image"),
        "image/webp",
        pic,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // The other teacher's own exam, to attempt an instantiate from.
    let course2 = create_course(&app, &other, "bio2").await;
    let subject2 = create_subject(&app, &other, &course2, "cells2").await;
    let exam2 = create_exam(&app, &other, &course2, "quiz", "final").await;
    let owner_id = me_id(&app, &owner).await;

    // --- the stranger sees nothing, and is told nothing ---
    let res = send(&app, "GET", "/bank-questions", Some(&other), None).await;
    assert!(common::items(&res.body).is_empty());
    assert_eq!(
        res.body["total"], 0,
        "total must count only visible templates"
    );
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{secret}"),
        Some(&other),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
    let (status, _, _) = common::send_raw(
        &app,
        "GET",
        &format!("/bank-questions/{secret}/image"),
        Some(&other),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = common::send_raw(
        &app,
        "GET",
        &format!("/bank-questions/{secret}/choices/{opt}/image"),
        Some(&other),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam2}/questions/from-bank/{secret}"),
        Some(&other),
        Some(json!({ "subject_id": subject2 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
    // Not even by asking for the owner's templates by name.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions?owner={owner_id}"),
        Some(&other),
        None,
    )
    .await;
    assert_eq!(res.body["total"], 0);

    // --- the owner still has full use of their own template ---
    let res = send(&app, "GET", "/bank-questions", Some(&owner), None).await;
    assert_eq!(res.body["total"], 2);
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{secret}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["correct"], res.body["choices"][0]["id"]);
    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &format!("/bank-questions/{secret}/image"),
        Some(&owner),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!((status, bytes.as_slice()), (StatusCode::OK, illus));
    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &format!("/bank-questions/{secret}/choices/{opt}/image"),
        Some(&owner),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!((status, bytes.as_slice()), (StatusCode::OK, pic));
    let exam = create_exam(&app, &owner, &course, "midterm", "final").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/from-bank/{secret}"),
        Some(&owner),
        Some(json!({ "subject_id": subject })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    // --- publishing one template opens exactly that one ---
    let res = send(
        &app,
        "PATCH",
        &format!("/bank-questions/{shared}"),
        Some(&owner),
        Some(json!({ "visibility": "school" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["visibility"], "school");
    let res = send(&app, "GET", "/bank-questions", Some(&other), None).await;
    assert_eq!(res.body["total"], 1, "the private one must still be hidden");
    assert_eq!(common::items(&res.body)[0]["id"], shared);
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{shared}"),
        Some(&other),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam2}/questions/from-bank/{shared}"),
        Some(&other),
        Some(json!({ "subject_id": subject2 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    // The still-private one stays shut on every path.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{secret}"),
        Some(&other),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam2}/questions/from-bank/{secret}"),
        Some(&other),
        Some(json!({ "subject_id": subject2 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
    // Unpublishing closes it again.
    let res = send(
        &app,
        "PATCH",
        &format!("/bank-questions/{shared}"),
        Some(&owner),
        Some(json!({ "visibility": "private" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{shared}"),
        Some(&other),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
    // A bogus value is a 400, not a silent publish.
    let res = send(
        &app,
        "PATCH",
        &format!("/bank-questions/{shared}"),
        Some(&owner),
        Some(json!({ "visibility": "public" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
}

/// Admins see private templates: they already read every exam's `correct` via
/// course management, and they may already delete any template.
#[tokio::test]
async fn bank_private_template_is_visible_to_admins() {
    let (app, db) = app_and_db().await;
    let owner = login_as(&app, &db, "badm_o", "teacher").await;
    let admin = login_as(&app, &db, "badm_a", "admin").await;
    let course = create_course(&app, &owner, "hist").await;
    let subject = create_subject(&app, &owner, &course, "ottoman").await;
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&owner),
        Some(json!({ "subject_id": subject, "text": "when?", "kind": "text", "points": 1 })),
    )
    .await;
    let bid = id_of(&res.body);

    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{bid}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(&app, "GET", "/bank-questions", Some(&admin), None).await;
    assert_eq!(res.body["total"], 1);
}

#[tokio::test]
async fn exam_question_saves_to_bank() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "s2b_t", "teacher").await;
    let course = create_course(&app, &teacher, "phys").await;
    let subject = create_subject(&app, &teacher, &course, "kinematics").await;
    let exam = create_exam(&app, &teacher, &course, "final", "final").await;
    let qid = create_question(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "v=?", "kind": "choice", "points": 10, "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}], "correct": "c0" }),
    )
    .await;

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{qid}/to-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["text"], "v=?");
    assert_eq!(res.body["correct"], res.body["choices"][0]["id"]);
    assert_eq!(res.body["subject"], subject);
    assert_eq!(res.body["owner"], me_id(&app, &teacher).await);
    // The one-click broadcast that was: the saved key stays the teacher's until
    // they publish it.
    assert_eq!(res.body["visibility"], "private");
    let bid = id_of(&res.body);

    // It now lives in their bank.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
}

/// Instantiating a bank template copies its image blobs to fresh files under the
/// new exam question, byte-for-byte, and leaves the source template's images
/// untouched.
#[tokio::test]
async fn bank_instantiate_copies_images() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bimg_t", "teacher").await;
    let course = create_course(&app, &teacher, "geo1").await;
    let subject = create_subject(&app, &teacher, &course, "maps1").await;

    // A choice template with an illustration and one option picture.
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&teacher),
        Some(json!({ "subject_id": subject, "text": "Which city?", "kind": "choice",
                     "points": 10, "choices": [{"id": "c0", "text": "Ankara"}, {"id": "c1", "text": "İzmir"}], "correct": "c0" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let bid = id_of(&res.body);
    let opt = choice_of(&res.body, 0);

    let illus = b"bank-illustration".as_slice();
    let choice = b"bank-choice-pic".as_slice();
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/bank-questions/{bid}/image"),
        "image/png",
        illus,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/bank-questions/{bid}/choices/{opt}/image"),
        "image/webp",
        choice,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let src_bank_keys = bank_image_blob_keys(&db).await;
    assert_eq!(src_bank_keys.len(), 2);

    // Instantiate into an exam.
    let exam = create_exam(&app, &teacher, &course, "midterm", "final").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/from-bank/{bid}"),
        Some(&teacher),
        Some(json!({ "subject_id": subject })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let qid = id_of(&res.body);

    // Both images retrievable on the new exam question, bytes equal to source.
    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &format!("/exams/{exam}/questions/{qid}/image"),
        Some(&teacher),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, illus);
    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &format!("/exams/{exam}/questions/{qid}/choices/{opt}/image"),
        Some(&teacher),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, choice);

    // Copies are fresh files — new keys, none shared with the bank.
    let exam_keys = image_blob_keys(&db).await;
    assert_eq!(exam_keys.len(), 2);
    for k in &exam_keys {
        assert!(
            !src_bank_keys.contains(k),
            "copied blob reused a source file"
        );
    }

    // Source bank images untouched — still 200 with the same bytes and keys.
    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &format!("/bank-questions/{bid}/image"),
        Some(&teacher),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, illus);
    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &format!("/bank-questions/{bid}/choices/{opt}/image"),
        Some(&teacher),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, choice);
    assert_eq!(
        bank_image_blob_keys(&db).await,
        src_bank_keys,
        "source keys changed"
    );
}

/// Saving an exam question to the bank copies its image blobs to fresh files
/// under the new template, byte-for-byte, source exam images untouched.
#[tokio::test]
async fn bank_save_copies_images() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "simg_t", "teacher").await;
    let course = create_course(&app, &teacher, "phys1").await;
    let subject = create_subject(&app, &teacher, &course, "kin1").await;
    let exam = create_exam(&app, &teacher, &course, "final", "final").await;
    let qid_body = create_question_body(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "v=?", "kind": "choice", "points": 10, "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}], "correct": "c0" }),
    )
    .await;
    let qid = id_of(&qid_body);
    let opt = choice_of(&qid_body, 0);

    let illus = b"exam-illustration".as_slice();
    let choice = b"exam-choice-pic".as_slice();
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/exams/{exam}/questions/{qid}/image"),
        "image/png",
        illus,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/exams/{exam}/questions/{qid}/choices/{opt}/image"),
        "image/jpeg",
        choice,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let src_exam_keys = image_blob_keys(&db).await;
    assert_eq!(src_exam_keys.len(), 2);

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{qid}/to-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let bid = id_of(&res.body);
    let opt = choice_of(&res.body, 0);

    // The new bank question carries both images, bytes equal.
    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &format!("/bank-questions/{bid}/image"),
        Some(&teacher),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, illus);
    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &format!("/bank-questions/{bid}/choices/{opt}/image"),
        Some(&teacher),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, choice);

    // Fresh keys; source exam images untouched.
    let bank_keys = bank_image_blob_keys(&db).await;
    assert_eq!(bank_keys.len(), 2);
    for k in &bank_keys {
        assert!(
            !src_exam_keys.contains(k),
            "copied blob reused a source file"
        );
    }
    assert_eq!(
        image_blob_keys(&db).await,
        src_exam_keys,
        "source keys changed"
    );
    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &format!("/exams/{exam}/questions/{qid}/image"),
        Some(&teacher),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, illus);
}

/// An image survives a full round trip: exam question -> bank -> a second exam,
/// with the bytes intact at the far end.
#[tokio::test]
async fn bank_round_trip_preserves_image_bytes() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "rt_t", "teacher").await;
    let course = create_course(&app, &teacher, "hist1").await;
    let subject = create_subject(&app, &teacher, &course, "eras1").await;
    let exam1 = create_exam(&app, &teacher, &course, "e1", "final").await;
    let qid = create_question(
        &app,
        &teacher,
        &exam1,
        &subject,
        json!({ "text": "When?", "kind": "text", "points": 10 }),
    )
    .await;
    let pic = b"round-trip-illustration".as_slice();
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/exams/{exam1}/questions/{qid}/image"),
        "image/png",
        pic,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Hop one: exam -> bank.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam1}/questions/{qid}/to-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let bid = id_of(&res.body);

    // Hop two: bank -> a second exam.
    let exam2 = create_exam(&app, &teacher, &course, "e2", "final").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam2}/questions/from-bank/{bid}"),
        Some(&teacher),
        Some(json!({ "subject_id": subject })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let qid2 = id_of(&res.body);

    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &format!("/exams/{exam2}/questions/{qid2}/image"),
        Some(&teacher),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, pic, "bytes drifted across the round trip");
}

/// Gap-1 guard: a missing source blob mid-copy is all-or-nothing — the
/// instantiate 500s and leaves NO partial destination (no exam question row, no
/// exam image rows), while the source template survives intact.
#[tokio::test]
async fn bank_instantiate_rolls_back_on_missing_source_blob() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "rb_t", "teacher").await;
    let course = create_course(&app, &teacher, "geo2").await;
    let subject = create_subject(&app, &teacher, &course, "maps2").await;

    // Two images so the copy loop has multiple steps.
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&teacher),
        Some(json!({ "subject_id": subject, "text": "q", "kind": "choice",
                     "points": 10, "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}], "correct": "c0" })),
    )
    .await;
    let bid = id_of(&res.body);
    let opt = choice_of(&res.body, 0);
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/bank-questions/{bid}/image"),
        "image/png",
        b"illus",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/bank-questions/{bid}/choices/{opt}/image"),
        "image/png",
        b"opt",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Corrupt the source on disk: delete one blob so the copy loop hits an
    // unreadable source mid-flight.
    let src_keys = bank_image_blob_keys(&db).await;
    assert_eq!(src_keys.len(), 2);
    tokio::fs::remove_file(common::blob_dir().join(&src_keys[0]))
        .await
        .unwrap();

    let exam = create_exam(&app, &teacher, &course, "midterm", "final").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/from-bank/{bid}"),
        Some(&teacher),
        Some(json!({ "subject_id": subject })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        res.body
    );

    // No partial destination: no question row, no exam image rows (isolated
    // per-test DB is the race-safe source of truth; the shared parallel blob
    // dir rules out an exact on-disk diff).
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    assert!(
        common::items(&res.body).is_empty(),
        "orphan question survived rollback"
    );
    assert!(
        image_blob_keys(&db).await.is_empty(),
        "orphan exam image rows survived rollback"
    );

    // Source intact: the template still reads, and its surviving blob is still
    // on disk.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        bank_image_blob_keys(&db).await,
        src_keys,
        "source bank rows changed"
    );
    assert!(
        common::blob_dir().join(&src_keys[1]).exists(),
        "surviving source blob vanished"
    );
}

/// A bank template's subject is origin metadata, but it must still exist:
/// create and update both 400 on an unknown subject id.
#[tokio::test]
async fn bank_rejects_unknown_subject() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bsub_t", "teacher").await;
    let course = create_course(&app, &teacher, "bsub_c").await;
    let subject = create_subject(&app, &teacher, &course, "bsub_s").await;

    // Create with a bogus subject id — 400, not a stored row.
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&teacher),
        Some(json!({ "subject_id": "nope", "text": "q", "kind": "text", "points": 5 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // Create a real one, then PATCH its subject to a bogus id — also 400.
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&teacher),
        Some(json!({ "subject_id": subject, "text": "q", "kind": "text", "points": 5 })),
    )
    .await;
    let bid = id_of(&res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        Some(json!({ "subject_id": "nope" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
}

/// Deleting a subject clears it off the bank templates that carried it as
/// origin metadata, instead of being refused. Blocking would have been a dead
/// end (only a template's owner may re-tag it, so a manager could not clear
/// their own 409) and an existence oracle for other teachers' private
/// templates — hence the cascade.
#[tokio::test]
async fn subject_delete_clears_the_bank_templates_subject() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bdel_t", "teacher").await;
    let manager = login_as(&app, &db, "bdel_m", "manager").await;
    let course = create_course(&app, &teacher, "bdel_c").await;
    let subject = create_subject(&app, &teacher, &course, "bdel_s").await;
    let other = create_subject(&app, &teacher, &course, "bdel_s2").await;

    let mut ids = Vec::new();
    for subject_id in [&subject, &other] {
        let res = send(
            &app,
            "POST",
            "/bank-questions",
            Some(&teacher),
            Some(json!({ "subject_id": subject_id, "text": "q", "kind": "text", "points": 5 })),
        )
        .await;
        ids.push(id_of(&res.body));
    }
    let (bid, kept) = (&ids[0], &ids[1]);

    // The manager owns none of these templates, yet the delete goes through.
    let res = send(
        &app,
        "DELETE",
        &format!("/subjects/{subject}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);

    // The template survives with a cleared origin; the other one is untouched.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["subject"], serde_json::Value::Null);
    assert_eq!(res.body["text"], "q");
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{kept}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["subject"], json!(other));

    // The list join renders the cleared one with an empty subject name.
    let res = send(&app, "GET", "/bank-questions", Some(&teacher), None).await;
    let cleared = common::items(&res.body)
        .iter()
        .find(|q| q["id"] == *bid)
        .expect("template in list")
        .clone();
    assert_eq!(cleared["subject"], serde_json::Value::Null);
    assert_eq!(cleared["subject_name"], "");

    // A cleared template still edits: PATCH without `subject_id` keeps it null.
    let res = send(
        &app,
        "PATCH",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        Some(json!({ "text": "still editable" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["subject"], serde_json::Value::Null);
    assert_eq!(res.body["text"], "still editable");
}

/// `?owner=me` (and an explicit owner id) narrows the bank to one owner's
/// templates — on top of the visibility gate, never around it.
#[tokio::test]
async fn bank_owner_filter() {
    let (app, db) = app_and_db().await;
    let alice = login_as(&app, &db, "bown_a", "teacher").await;
    let bob = login_as(&app, &db, "bown_b", "teacher").await;
    let course = create_course(&app, &alice, "bown_c").await;
    let subject = create_subject(&app, &alice, &course, "bown_s").await;

    for who in [&alice, &bob] {
        let res = send(
            &app,
            "POST",
            "/bank-questions",
            Some(who),
            Some(json!({ "subject_id": subject, "text": "q", "kind": "text", "points": 5 })),
        )
        .await;
        // Published, so this test is about the owner filter, not visibility.
        let bid = id_of(&res.body);
        let res = send(
            &app,
            "PATCH",
            &format!("/bank-questions/{bid}"),
            Some(who),
            Some(json!({ "visibility": "school" })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    }
    let alice_id = me_id(&app, &alice).await;
    let bob_id = me_id(&app, &bob).await;

    // Whole bank has both.
    let res = send(&app, "GET", "/bank-questions", Some(&alice), None).await;
    assert_eq!(common::items(&res.body).len(), 2);

    // `me` resolves to the caller.
    let res = send(&app, "GET", "/bank-questions?owner=me", Some(&alice), None).await;
    let items = common::items(&res.body);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["owner"], alice_id);

    // An explicit id filters to that owner.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions?owner={bob_id}"),
        Some(&alice),
        None,
    )
    .await;
    let items = common::items(&res.body);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["owner"], bob_id);
}

/// A template's response carries its image metadata (illustration + per-choice
/// slots) once images are uploaded — the same shape the exam side returns.
#[tokio::test]
async fn bank_response_carries_image_metas() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bmeta_t", "teacher").await;
    let course = create_course(&app, &teacher, "bmeta_c").await;
    let subject = create_subject(&app, &teacher, &course, "bmeta_s").await;
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&teacher),
        Some(json!({ "subject_id": subject, "text": "q", "kind": "choice",
                     "points": 10, "choices": [{"id": "c0", "text": "a"}, {"id": "c1", "text": "b"}], "correct": "c0" })),
    )
    .await;
    let bid = id_of(&res.body);
    let opt = choice_of(&res.body, 0);

    // No images yet: metas are null / all-empty.
    assert!(res.body["image"].is_null());
    assert_eq!(res.body["choice_images"], json!([null, null]));

    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/bank-questions/{bid}/image"),
        "image/png",
        b"illus",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/bank-questions/{bid}/choices/{opt}/image"),
        "image/jpeg",
        b"opt",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["image"]["content_type"], "image/png");
    assert_eq!(res.body["image"]["size"], 5);
    assert_eq!(res.body["choice_images"][0]["content_type"], "image/jpeg");
    assert!(res.body["choice_images"][1].is_null());

    // The list carries them too (batched, not per-row).
    let res = send(
        &app,
        "GET",
        "/bank-questions?owner=me",
        Some(&teacher),
        None,
    )
    .await;
    let items = common::items(&res.body);
    assert_eq!(items[0]["image"]["content_type"], "image/png");
    assert_eq!(items[0]["choice_images"][0]["content_type"], "image/jpeg");
}

/// Provenance rides both directions, one column per direction: instantiate
/// records `from_bank` on the new exam question (and leaves `banked_as` null —
/// coming *from* the bank is not being saved *to* it); save-to-bank records
/// `source_exam` on the new template and `banked_as` on the question, without
/// touching `from_bank`.
#[tokio::test]
async fn bank_provenance_both_directions() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bprov_t", "teacher").await;
    let course = create_course(&app, &teacher, "bprov_c").await;
    let subject = create_subject(&app, &teacher, &course, "bprov_s").await;

    // Bank -> exam: from_bank is the template id, banked_as stays null.
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&teacher),
        Some(json!({ "subject_id": subject, "text": "q", "kind": "text", "points": 5 })),
    )
    .await;
    let bid = id_of(&res.body);
    let exam = create_exam(&app, &teacher, &course, "midterm", "final").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/from-bank/{bid}"),
        Some(&teacher),
        Some(json!({ "subject_id": subject })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["from_bank"], bid);
    // The lie the split fixes: an inserted question must not read as banked.
    assert!(res.body["banked_as"].is_null(), "{}", res.body);
    let inserted = id_of(&res.body);

    // Saving that inserted question sets banked_as and leaves from_bank alone.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{inserted}/to-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let template = id_of(&res.body);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    let question = common::items(&res.body)
        .iter()
        .find(|q| q["id"] == inserted)
        .expect("question in list")
        .clone();
    assert_eq!(question["from_bank"], bid, "{question}");
    assert_eq!(question["banked_as"], template, "{question}");

    // A directly authored exam question has neither link.
    let qid = create_question(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "plain", "kind": "text", "points": 5 }),
    )
    .await;

    // Exam -> bank: source_exam is the origin exam id.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{qid}/to-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["source_exam"], exam);

    // A directly authored template has no source_exam.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        None,
    )
    .await;
    assert!(res.body["source_exam"].is_null());
}

/// Save-to-bank also links the *question* back at the template it produced, so
/// a client can tell it was already banked. A repeat save is allowed and
/// repoints the link at the newer template.
#[tokio::test]
async fn to_bank_links_the_question_at_the_new_template() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bback_t", "teacher").await;
    let course = create_course(&app, &teacher, "bback_c").await;
    let subject = create_subject(&app, &teacher, &course, "bback_s").await;
    let exam = create_exam(&app, &teacher, &course, "midterm", "final").await;
    let qid = create_question(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "plain", "kind": "text", "points": 5 }),
    )
    .await;

    // Fresh question: no link yet.
    let banked_as_of = |body: &serde_json::Value| -> serde_json::Value {
        common::items(body)
            .iter()
            .find(|q| q["id"] == qid)
            .expect("question in list")["banked_as"]
            .clone()
    };
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    assert!(banked_as_of(&res.body).is_null());

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{qid}/to-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let first = id_of(&res.body);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(banked_as_of(&res.body), json!(first));

    // Second save: still 201, a distinct template, and the link follows it.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{qid}/to-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let second = id_of(&res.body);
    assert_ne!(first, second);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(banked_as_of(&res.body), json!(second));
}

/// The back-link is a field-scoped write. `question_to_bank` reads the question,
/// then awaits a bank insert and the whole blob-copy loop before linking, holding
/// no lock over `exam_question` — so a `PATCH` can land in that window. A
/// whole-row save from the stale struct would silently revert it. Driven at the
/// domain level: HTTP offers no way to interleave inside the handler.
#[tokio::test]
async fn linking_the_banked_template_does_not_clobber_a_concurrent_edit() {
    use hezarfen_backend::db::exam_question;
    use hezarfen_backend::domain::bank_question::BankQuestionId;
    use hezarfen_backend::domain::exam_question::ExamQuestionId;

    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bclob_t", "teacher").await;
    let course = create_course(&app, &teacher, "bclob_c").await;
    let subject = create_subject(&app, &teacher, &course, "bclob_s").await;
    let exam = create_exam(&app, &teacher, &course, "midterm", "final").await;
    let qid = create_question(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "original", "kind": "text", "points": 5 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&teacher),
        Some(json!({ "subject_id": subject, "text": "q", "kind": "text", "points": 5 })),
    )
    .await;
    let bid = id_of(&res.body);

    // The handler's read, then someone else's edit lands mid-window.
    let stale = exam_question::read(&db, &ExamQuestionId::from_key(&qid))
        .await
        .unwrap()
        .expect("question exists");
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{qid}"),
        Some(&teacher),
        Some(json!({ "text": "edited by someone else", "points": 9 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    exam_question::link_banked_as(&db, stale, BankQuestionId::from_key(&bid))
        .await
        .expect("link written");

    // The edit survives and the link is set.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    let question = common::items(&res.body)
        .iter()
        .find(|q| q["id"] == qid)
        .expect("question in list")
        .clone();
    assert_eq!(question["text"], "edited by someone else");
    assert_eq!(question["points"], 9);
    assert_eq!(question["banked_as"], bid);
}

/// A bank PATCH writes only the fields it was given: the handler reads the
/// template, then awaits an ownership check, a subject lookup and (on a choice
/// replace) an image sweep, so another write can land in that window — here the
/// exam-delete cascade clearing `source_exam`, a field the PATCH never sets.
/// A whole-row save from the stale struct would resurrect the dead exam link.
#[tokio::test]
async fn a_bank_patch_does_not_clobber_a_concurrent_write() {
    use hezarfen_backend::domain::bank_question::{BankQuestion, BankQuestionId, BankVisibility};
    use hezarfen_backend::domain::exam_question::{
        QuestionKind, QuestionPoints, QuestionSpec, QuestionText,
    };

    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bpatch_t", "teacher").await;
    let course = create_course(&app, &teacher, "bpatch_c").await;
    let subject = create_subject(&app, &teacher, &course, "bpatch_s").await;
    let exam = create_exam(&app, &teacher, &course, "midterm", "final").await;
    let qid = create_question(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "original", "kind": "text", "points": 5 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{qid}/to-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let bid = id_of(&res.body);
    assert_eq!(res.body["source_exam"], json!(exam));

    // The handler's read, then the exam delete lands mid-window and clears the
    // template's `source_exam`.
    let stale = BankQuestion::read(&BankQuestionId::from_key(&bid), &db)
        .await
        .unwrap()
        .expect("template exists");
    assert!(stale.get_source_exam().is_some());
    let res = send(
        &app,
        "DELETE",
        &format!("/exams/{exam}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);

    // The stale struct writes its own fields only.
    let updated = stale
        .update_if_unchanged(
            None,
            QuestionText::try_new("edited").unwrap(),
            QuestionPoints::try_new(7).unwrap(),
            QuestionSpec::try_new(QuestionKind::try_new("text").unwrap(), None, None, &[]).unwrap(),
            BankVisibility::try_new(BANK_VISIBILITY_SCHOOL).unwrap(),
            &db,
        )
        .await
        .expect("update ran")
        .expect("update written — the exam delete touched no compared column");
    assert_eq!(updated.get_text().as_str(), "edited");
    assert!(
        updated.get_source_exam().is_none(),
        "the dead exam link came back"
    );

    // The edit landed and the cleared provenance stayed cleared.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["source_exam"], serde_json::Value::Null);
    assert_eq!(res.body["subject"], serde_json::Value::Null);
    assert_eq!(res.body["text"], "edited");
    assert_eq!(res.body["points"], 7);
    assert_eq!(res.body["visibility"], "school");
}

/// Deleting a bank template clears *both* provenance links that can point at
/// it — `from_bank` on the questions instantiated from it and `banked_as` on
/// the question it was saved out of — instead of leaving either pointing at a
/// dead row.
#[tokio::test]
async fn deleting_a_template_clears_both_question_links() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bdang_t", "teacher").await;
    let course = create_course(&app, &teacher, "bdang_c").await;
    let subject = create_subject(&app, &teacher, &course, "bdang_s").await;
    let exam = create_exam(&app, &teacher, &course, "midterm", "final").await;

    // Question A: authored here, then saved to the bank -> banked_as = saved.
    let authored = create_question(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "plain", "kind": "text", "points": 5 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{authored}/to-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let saved = id_of(&res.body);

    // Question B: instantiated from a separate template -> from_bank = origin.
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&teacher),
        Some(json!({ "subject_id": subject, "text": "t", "kind": "text", "points": 5 })),
    )
    .await;
    let origin = id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/from-bank/{origin}"),
        Some(&teacher),
        Some(json!({ "subject_id": subject })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let inserted = id_of(&res.body);

    let link_of = |body: &serde_json::Value, qid: &str, field: &str| -> serde_json::Value {
        common::items(body)
            .iter()
            .find(|q| q["id"] == qid)
            .expect("question in list")[field]
            .clone()
    };
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(link_of(&res.body, &authored, "banked_as"), json!(saved));
    assert_eq!(link_of(&res.body, &inserted, "from_bank"), json!(origin));

    for bid in [&saved, &origin] {
        let res = send(
            &app,
            "DELETE",
            &format!("/bank-questions/{bid}"),
            Some(&teacher),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    }

    // Both questions survive their templates with no dangling link.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(
        link_of(&res.body, &authored, "banked_as").is_null(),
        "{}",
        res.body
    );
    assert!(
        link_of(&res.body, &inserted, "from_bank").is_null(),
        "{}",
        res.body
    );
}

/// The mirror: deleting the origin exam clears `source_exam` on the templates
/// saved out of it — the template is a reusable library row and outlives the
/// exam, but the bank page must not read a link to a dead exam.
#[tokio::test]
async fn deleting_an_exam_clears_the_templates_source_exam() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "edang_t", "teacher").await;
    let course = create_course(&app, &teacher, "edang_c").await;
    let subject = create_subject(&app, &teacher, &course, "edang_s").await;
    let exam = create_exam(&app, &teacher, &course, "midterm", "final").await;
    let qid = create_question(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "plain", "kind": "text", "points": 5 }),
    )
    .await;

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{qid}/to-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let bid = id_of(&res.body);
    assert_eq!(res.body["source_exam"], exam);

    let res = send(
        &app,
        "DELETE",
        &format!("/exams/{exam}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);

    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body["source_exam"].is_null(), "{}", res.body);
}

/// `used_count` on the bank list: how many exam questions were copied out of
/// each template. It is the divergence surface made visible — every one of
/// those copies is detached, so an edit here never reaches them. Joined once
/// for the whole page (one grouped query), list-only exactly like
/// `subject_name`/`owner_name`.
#[tokio::test]
async fn bank_list_counts_the_copies_made_from_each_template() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "buse_t", "teacher").await;
    let course = create_course(&app, &teacher, "buse_c").await;
    let subject = create_subject(&app, &teacher, &course, "buse_s").await;

    let template = |text: &'static str| {
        send(
            &app,
            "POST",
            "/bank-questions",
            Some(&teacher),
            Some(json!({ "subject_id": subject, "text": text, "kind": "text", "points": 4 })),
        )
    };
    let used = id_of(&template("reused").await.body);
    let unused = id_of(&template("never used").await.body);

    // Two exams, three copies: twice out of `used` in one exam, once in the other.
    let mut exams = Vec::new();
    for title in ["one", "two"] {
        exams.push(create_exam(&app, &teacher, &course, title, "final").await);
    }
    for (exam, times) in [(&exams[0], 2), (&exams[1], 1)] {
        for _ in 0..times {
            let res = send(
                &app,
                "POST",
                &format!("/exams/{exam}/questions/from-bank/{used}"),
                Some(&teacher),
                Some(json!({ "subject_id": subject })),
            )
            .await;
            assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
        }
    }
    // A hand-authored question in the same exam has no template and must not
    // be tallied against one.
    create_question(
        &app,
        &teacher,
        &exams[0],
        &subject,
        json!({ "text": "mine alone", "kind": "text", "points": 1 }),
    )
    .await;

    let res = send(
        &app,
        "GET",
        "/bank-questions?limit=100",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let counts: std::collections::HashMap<String, i64> = common::items(&res.body)
        .iter()
        .map(|item| (id_of(item), item["used_count"].as_i64().unwrap()))
        .collect();
    assert_eq!(counts.get(&used).copied(), Some(3), "{}", res.body);
    assert_eq!(counts.get(&unused).copied(), Some(0), "{}", res.body);

    // Deleting a copy takes it back off the tally (its `from_bank` goes with it).
    let res = send(
        &app,
        "GET",
        &format!("/exams/{}/questions", exams[1]),
        Some(&teacher),
        None,
    )
    .await;
    let qid = id_of(&common::items(&res.body)[0]);
    let res = send(
        &app,
        "DELETE",
        &format!("/exams/{}/questions/{qid}", exams[1]),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    let res = send(
        &app,
        "GET",
        "/bank-questions?limit=100",
        Some(&teacher),
        None,
    )
    .await;
    let after = common::items(&res.body)
        .iter()
        .find(|item| id_of(item) == used)
        .cloned()
        .unwrap();
    assert_eq!(after["used_count"], 2, "{}", res.body);

    // List-only, like the other joined fields: the single-template GET says 0.
    let res = send(
        &app,
        "GET",
        &format!("/bank-questions/{used}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.body["used_count"], 0, "{}", res.body);
}

/// The escape hatch out of the divergence trap: re-copy a template's *current*
/// content over the exam question that was instantiated from it.
///
/// Choice ids come from the template — the same funnel the original insert
/// used — so pictures land back on the right options and a stale option's
/// picture goes with it. A student's recorded `selected` can never be stranded
/// by that: the route dies on `ensure_questions_editable` (409) the moment any
/// attempt exists, and answers only exist under attempts.
#[tokio::test]
async fn refreshing_a_question_recopies_its_template() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "bref_t", "teacher").await;
    let manager = login_as(&app, &db, "bref_m", "manager").await;
    let course = create_course(&app, &teacher, "bref_c").await;
    let subject = create_subject(&app, &teacher, &course, "bref_s").await;

    // A template with an illustration and a picture on its first option.
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&teacher),
        Some(json!({ "subject_id": subject, "text": "capitol of France?", "kind": "choice",
                     "points": 5, "choices": [{"id": "a", "text": "Paris"}, {"id": "b", "text": "Lyon"}],
                     "correct": "a" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let bid = id_of(&res.body);
    let keep_choice = choice_of(&res.body, 0);
    let drop_choice = choice_of(&res.body, 1);
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/bank-questions/{bid}/image"),
        "image/png",
        b"old-illustration",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/bank-questions/{bid}/choices/{drop_choice}/image"),
        "image/png",
        b"lyon-picture",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let exam = create_exam(&app, &teacher, &course, "midterm", "final").await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/from-bank/{bid}"),
        Some(&teacher),
        Some(json!({ "subject_id": subject })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let qid = id_of(&res.body);

    // The exam copy drifts (a local edit), and the template is fixed and
    // reshaped: the typo goes, "Lyon" is replaced by a new option (a new id),
    // points change, and the illustration is replaced.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}/questions/{qid}"),
        Some(&teacher),
        Some(json!({ "text": "local edit that will be lost", "points": 99 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        Some(json!({ "text": "capital of France?", "points": 7,
                     "choices": [{"id": keep_choice, "text": "Paris"}, {"id": "new", "text": "Marseille"}],
                     "correct": keep_choice })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let fresh_choice = choice_of(&res.body, 1);
    assert_ne!(fresh_choice, drop_choice, "the replaced option is a new id");
    let (status, _) = post_image(
        &app,
        &teacher,
        &format!("/bank-questions/{bid}/image"),
        "image/png",
        b"new-illustration",
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // A template the caller cannot see is a 404 on this route too — never a
    // 403, which would confirm it exists. (A manager may manage the course but
    // is not the template's owner, and the template is private.)
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{qid}/refresh-from-bank"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);

    // The refresh itself: content, points, choices (ids and all), and correct
    // come back from the template; the local edit is gone.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{qid}/refresh-from-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["id"], qid, "the question keeps its id");
    assert_eq!(res.body["text"], "capital of France?");
    assert_eq!(res.body["points"], 7);
    assert_eq!(res.body["subject"], subject, "the exam-side subject stays");
    assert_eq!(res.body["from_bank"], bid, "provenance survives");
    assert_eq!(
        choice_of(&res.body, 0),
        keep_choice,
        "template choice ids adopted"
    );
    assert_eq!(choice_of(&res.body, 1), fresh_choice);
    assert_eq!(res.body["choices"][1]["text"], "Marseille");
    assert_eq!(res.body["correct"], keep_choice);
    // Pictures match the template: the new illustration landed, and the
    // dropped option's picture is gone rather than orphaned on a stale slot.
    assert_eq!(
        res.body["image"]["content_type"], "image/png",
        "{}",
        res.body
    );
    assert!(res.body["choice_images"][0].is_null(), "{}", res.body);
    assert!(res.body["choice_images"][1].is_null(), "{}", res.body);
    let (status, _, bytes) = common::send_raw(
        &app,
        "GET",
        &format!("/exams/{exam}/questions/{qid}/image"),
        Some(&teacher),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        bytes, b"new-illustration",
        "the blob was re-copied, not kept"
    );
    // No orphan blob rows left behind by the dropped option.
    assert_eq!(image_blob_keys(&db).await.len(), 1);

    // A hand-authored question has nothing to refresh from — 400, not a 404
    // that would read as "your exam is gone".
    let plain = create_question(
        &app,
        &teacher,
        &exam,
        &subject,
        json!({ "text": "mine alone", "kind": "text", "points": 1 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{plain}/refresh-from-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // Once a student sits an exam, its questions freeze — the refresh answers
    // to the same guard as every other question mutation.
    let now = Timestamp::now().as_millis();
    let live = scheduled_exam(
        &app,
        &teacher,
        &course,
        json!({ "title": "live", "kind": "final", "mode": "sync",
                "starts_at": now - 1_000, "ends_at": now + 600_000 }),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{live}/questions/from-bank/{bid}"),
        Some(&teacher),
        Some(json!({ "subject_id": subject })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let live_qid = id_of(&res.body);
    let student = login(&app, "bref_s").await;
    let student_id = me_id(&app, &student).await;
    enroll(&app, &teacher, &course, &student_id).await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{live}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{live}/questions/{live_qid}/refresh-from-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // Deleting the template clears `from_bank`, so the copy has nothing left to
    // refresh from — the action stops existing rather than half-working.
    let res = send(
        &app,
        "DELETE",
        &format!("/bank-questions/{bid}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        None,
    )
    .await;
    let refreshed = common::items(&res.body)
        .iter()
        .find(|item| id_of(item) == qid)
        .cloned()
        .unwrap();
    assert!(refreshed["from_bank"].is_null(), "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions/{qid}/refresh-from-bank"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
}

/// A course PATCH is a field-scoped write. The handler reads the course, then
/// awaits a term lookup (and its `TERM_LOCK`) before saving, holding nothing
/// over the course row itself — so a manager's teacher assignment can land in
/// that window. A whole-row save from the stale struct would silently revert
/// the staffing. Driven at the domain level: HTTP offers no way to interleave
/// inside the handler.
#[tokio::test]
async fn a_course_patch_does_not_clobber_a_concurrent_teacher_assignment() {
    use hezarfen_backend::db::course;
    use hezarfen_backend::domain::course::{CourseDescription, CourseId, CourseKind, CourseTitle};

    let (app, db) = app_and_db().await;
    let owner = login_as(&app, &db, "cclob_o", "teacher").await;
    let helper = login_as(&app, &db, "cclob_h", "teacher").await;
    let boss = login_as(&app, &db, "cclob_m", "manager").await;
    let helper_id = me_id(&app, &helper).await;
    let course = create_course(&app, &owner, "cclob_c").await;

    // The handler's read, then the assignment lands mid-window.
    let stale = course::read(&db, &CourseId::from_key(&course))
        .await
        .unwrap()
        .expect("course exists");
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/teachers"),
        Some(&boss),
        Some(json!({ "user_id": helper_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let updated = course::update(
        &db,
        stale,
        Some(CourseTitle::try_new("renamed").unwrap()),
        Some(CourseDescription::try_new("").unwrap()),
        Some(CourseKind::course()),
        None,
        None,
    )
    .await
    .expect("update written");
    assert_eq!(
        updated.get_teachers().len(),
        1,
        "the assignment was reverted"
    );

    // The rename landed and the assigned teacher stayed assigned.
    let res = send(
        &app,
        "GET",
        &format!("/courses/{course}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(res.body["title"], "renamed");
    assert_eq!(res.body["teachers"][0]["username"], "cclob_h");
}

/// The mirror image: staffing writes only `teachers`. `assign_teacher` reads
/// the course, then awaits the target user's row (a role check) before saving,
/// so a course PATCH can land in between — and a whole-row save would revert
/// the rename. `unassign_teacher` gets the same field-scoped treatment.
#[tokio::test]
async fn staffing_a_course_does_not_clobber_a_concurrent_edit() {
    use hezarfen_backend::db::course;
    use hezarfen_backend::domain::course::CourseId;

    let (app, db) = app_and_db().await;
    let owner = login_as(&app, &db, "cstaff_o", "teacher").await;
    let helper = login_as(&app, &db, "cstaff_h", "teacher").await;
    let helper_id = me_id(&app, &helper).await;
    let course = create_course(&app, &owner, "cstaff_c").await;

    // The handler's read, then someone else's edit lands mid-window.
    let stale = course::read(&db, &CourseId::from_key(&course))
        .await
        .unwrap()
        .expect("course exists");
    let res = send(
        &app,
        "PATCH",
        &format!("/courses/{course}"),
        Some(&owner),
        Some(json!({ "title": "edited by someone else", "capacity": 9 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let assigned = course::assign_teacher(&db, stale, &UserId::from_key(&helper_id))
        .await
        .expect("assignment written");
    assert_eq!(assigned.get_title().as_str(), "edited by someone else");
    assert_eq!(assigned.get_capacity(), Some(9));
    assert_eq!(assigned.get_teachers().len(), 1);

    // Unassigning from a struct read before another edit is just as safe.
    let stale = course::read(&db, &CourseId::from_key(&course))
        .await
        .unwrap()
        .expect("course exists");
    let res = send(
        &app,
        "PATCH",
        &format!("/courses/{course}"),
        Some(&owner),
        Some(json!({ "title": "edited again" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let dropped = course::unassign_teacher(&db, stale, &UserId::from_key(&helper_id))
        .await
        .expect("unassignment written")
        .expect("teacher was assigned");
    assert_eq!(dropped.get_title().as_str(), "edited again");
    assert!(dropped.get_teachers().is_empty());

    let res = send(
        &app,
        "GET",
        &format!("/courses/{course}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(res.body["title"], "edited again");
    assert_eq!(res.body["capacity"], 9);
    assert_eq!(res.body["teachers"].as_array().unwrap().len(), 0);
}

// --- schedule-window list filters ----------------------------------------

/// Titles of a list envelope, in response order.
fn titles(body: &serde_json::Value) -> Vec<String> {
    common::items(body)
        .iter()
        .map(|item| item["title"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn events_schedule_window_filters_and_orders() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let now = Timestamp::now().as_millis();

    // Four events, created oldest-id first: one with no schedule at all, one
    // already running (inside the 60s backdating grace), one starting later
    // with no end, and one that only carries an end.
    for body in [
        json!({ "title": "no-schedule", "description": "" }),
        json!({ "title": "running", "starts_at": now - 30_000, "ends_at": now + 600_000 }),
        json!({ "title": "later", "starts_at": now + 900_000 }),
        json!({ "title": "ending", "ends_at": now + 300_000 }),
    ] {
        let res = send(&app, "POST", "/events", Some(&ali), Some(body)).await;
        assert_eq!(res.status, StatusCode::CREATED);
    }

    // No window params: unchanged behaviour — every row, newest-created first.
    let all = send(&app, "GET", "/events", Some(&ali), None).await;
    assert_eq!(common::total(&all.body), 4);
    assert_eq!(
        titles(&all.body),
        ["ending", "later", "running", "no-schedule"]
    );

    // ends_after: keeps unfinished rows (running on ends_at, later on the
    // starts_at fallback), drops the schedule-less one, ascending by schedule.
    let res = send(
        &app,
        "GET",
        &format!("/events?ends_after={now}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 3);
    assert_eq!(titles(&res.body), ["running", "ending", "later"]);

    // Past the running event's end: only the pure-end row and the fallback row.
    let res = send(
        &app,
        "GET",
        &format!("/events?ends_after={}", now + 700_000),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(titles(&res.body), ["later"]);

    // starts_after excludes the already-started row and every start-less row.
    let res = send(
        &app,
        "GET",
        &format!("/events?starts_after={now}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 1);
    assert_eq!(titles(&res.body), ["later"]);

    // total is the filtered, pre-pagination count; the page is the soonest.
    let res = send(
        &app,
        "GET",
        &format!("/events?ends_after={now}&limit=1"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 3);
    assert_eq!(titles(&res.body), ["running"]);

    // Invalid values are a 400 naming the field.
    for query in ["starts_after=-1", "ends_after=-1", "ends_after=soon"] {
        let res = send(&app, "GET", &format!("/events?{query}"), Some(&ali), None).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{query}");
    }
}

#[tokio::test]
async fn exams_schedule_window_applies_after_visibility_and_drafts() {
    let (app, db) = app_and_db().await;
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let ayse = login_as(&app, &db, "ayse", "student").await;
    let now = Timestamp::now().as_millis();

    let course = create_course(&app, &ali, "math").await;
    let other = create_course(&app, &ali, "hidden").await;
    enroll(&app, &ali, &course, &me_id(&app, &ayse).await).await;

    // Windowed exams in the student's course, plus a draft, an open-mode exam
    // (no window at all), and one in a course they cannot see.
    for (course, body) in [
        (
            &course,
            json!({ "title": "soon", "kind": "quiz", "mode": "sync",
                          "starts_at": now + 600_000, "ends_at": now + 900_000 }),
        ),
        (
            &course,
            json!({ "title": "late", "kind": "quiz", "mode": "sync",
                          "starts_at": now + 1_800_000, "ends_at": now + 2_400_000 }),
        ),
        (
            &course,
            json!({ "title": "draft", "kind": "quiz", "mode": "sync", "draft": true,
                          "starts_at": now + 60_000, "ends_at": now + 120_000 }),
        ),
        (
            &course,
            json!({ "title": "open", "kind": "quiz", "mode": "open" }),
        ),
        (
            &other,
            json!({ "title": "unseen", "kind": "quiz", "mode": "sync",
                         "starts_at": now + 60_000, "ends_at": now + 120_000 }),
        ),
    ] {
        let res = create_exam_with(&app, &ali, course, body).await;
        assert_eq!(res.status, StatusCode::CREATED, "{:?}", res.body);
    }

    // Unfiltered: the student sees their course's non-draft exams only.
    let all = send(&app, "GET", "/exams", Some(&ayse), None).await;
    assert_eq!(common::total(&all.body), 3);
    assert_eq!(titles(&all.body), ["open", "late", "soon"]);

    // Windowed: the draft and the other course stay invisible (the window runs
    // after visibility), the window-less open exam drops out, soonest first.
    let res = send(
        &app,
        "GET",
        &format!("/exams?ends_after={now}"),
        Some(&ayse),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 2);
    assert_eq!(titles(&res.body), ["soon", "late"]);

    // The teacher's own draft is windowed like any other exam of theirs.
    let res = send(
        &app,
        "GET",
        &format!("/exams?starts_after={now}&limit=2"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 4);
    assert_eq!(titles(&res.body), ["draft", "unseen"]);

    let res = send(&app, "GET", "/exams?starts_after=-1", Some(&ayse), None).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
}

// --- meals: menus and dishes ---------------------------------------------

/// The whole published side, end to end: publish, list, patch the cap, add and
/// edit and drop a dish, and the unique day+slot rule.
#[tokio::test]
async fn menus_and_dishes_round_trip() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "meal_mgr", "manager").await;

    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&mgr),
        Some(json!({ "date": "2099-09-14", "slot": "lunch", "capacity": 120 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let menu = id_of(&res.body);
    assert_eq!(res.body["slot"], "lunch");
    assert_eq!(res.body["capacity"], 120);
    assert_eq!(res.body["created_by"]["username"], "meal_mgr");

    // A second menu for the same day and slot is the UNIQUE rule, as a 409 —
    // never a 500 out of the index.
    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&mgr),
        Some(json!({ "date": "2099-09-14", "slot": "lunch" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT);

    // Same day, another slot is a different meal and lands fine.
    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&mgr),
        Some(json!({ "date": "2099-09-14", "slot": "breakfast" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);

    // Dishes: money is minor units, tags come from the school's list.
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/dishes"),
        Some(&mgr),
        Some(json!({
            "name": "Mercimek çorbası",
            "description": "  ",
            "price_minor": 4550,
            "tags": ["vegetarian", "vegetarian"],
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let dish = id_of(&res.body);
    assert_eq!(res.body["price_minor"], 4550);
    // A whitespace-only description is the absence of one, and repeated tags
    // collapse.
    assert_eq!(res.body["description"], json!(null));
    assert_eq!(res.body["tags"], json!(["vegetarian"]));

    let res = send(
        &app,
        "PATCH",
        &format!("/meals/dishes/{dish}"),
        Some(&mgr),
        Some(json!({ "price_minor": 5000, "description": "günlük" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["price_minor"], 5000);
    assert_eq!(res.body["description"], "günlük");
    assert_eq!(res.body["name"], "Mercimek çorbası");

    // The menu read carries its dishes, and `null` clears the cap.
    let res = send(
        &app,
        "PATCH",
        &format!("/meals/menus/{menu}"),
        Some(&mgr),
        Some(json!({ "capacity": null })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["capacity"], json!(null));
    assert_eq!(res.body["dishes"].as_array().expect("dishes").len(), 1);

    // Any authenticated user reads; the range filter is inclusive.
    let student = login(&app, "meal_student").await;
    let res = send(
        &app,
        "GET",
        "/meals/menus?from=2099-09-14&to=2099-09-14",
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(common::total(&res.body), 2);
    let res = send(
        &app,
        "GET",
        "/meals/menus?from=2099-09-15",
        Some(&student),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 0);

    let res = send(
        &app,
        "DELETE",
        &format!("/meals/dishes/{dish}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "GET",
        &format!("/meals/menus/{menu}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.body["dishes"], json!([]));

    let res = send(
        &app,
        "DELETE",
        &format!("/meals/menus/{menu}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    let res = send(
        &app,
        "GET",
        &format!("/meals/menus/{menu}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

/// Publishing is manager+, and both school-editable lists are enforced: a slot
/// nobody serves and a tag nobody defined are 400s, not stored strings.
#[tokio::test]
async fn menu_writes_are_manager_only_and_validate_against_settings() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "meal_mgr2", "manager").await;
    let teacher = login_as(&app, &db, "meal_teacher", "teacher").await;

    let body = json!({ "date": "2099-09-15", "slot": "lunch" });
    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&teacher),
        Some(body.clone()),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // A slot outside the school's `meal_slots` never reaches the column.
    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&mgr),
        Some(json!({ "date": "2099-09-15", "slot": "brunch" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // Neither does a malformed day.
    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&mgr),
        Some(json!({ "date": "2026-9-15", "slot": "lunch" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    let res = send(&app, "POST", "/meals/menus", Some(&mgr), Some(body)).await;
    assert_eq!(res.status, StatusCode::CREATED);
    let menu = id_of(&res.body);

    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/dishes"),
        Some(&teacher),
        Some(json!({ "name": "Pilav", "price_minor": 1000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);

    // A tag outside the school's `dietary_tags` is a 400, on create...
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/dishes"),
        Some(&mgr),
        Some(json!({ "name": "Pilav", "price_minor": 1000, "tags": ["halal"] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);

    // ...and on patch, and negative money never lands either.
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/dishes"),
        Some(&mgr),
        Some(json!({ "name": "Pilav", "price_minor": 1000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let dish = id_of(&res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/meals/dishes/{dish}"),
        Some(&mgr),
        Some(json!({ "tags": ["halal"] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    let res = send(
        &app,
        "PATCH",
        &format!("/meals/dishes/{dish}"),
        Some(&mgr),
        Some(json!({ "price_minor": -1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
}

// --- meals: bookings ------------------------------------------------------

/// A seat, end to end: book, see it on both listings, cancel it (the row stays,
/// flipped), re-book the same seat, and the menu-delete guard in between.
#[tokio::test]
async fn meal_bookings_round_trip() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "booking_mgr", "manager").await;
    let ali = login(&app, "booking_ali").await;

    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&mgr),
        Some(json!({ "date": "2099-09-14", "slot": "lunch", "capacity": 2 })),
    )
    .await;
    let menu = id_of(&res.body);

    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&ali),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let booking = id_of(&res.body);
    assert_eq!(res.body["status"], "booked");
    assert_eq!(res.body["student"]["username"], "booking_ali");
    assert_eq!(res.body["booked_by"]["username"], "booking_ali");
    assert_eq!(res.body["cancelled_at"], json!(null));

    // Booking twice is the same seat, not a second one — the composite id.
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&ali),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert_eq!(id_of(&res.body), booking);

    let res = send(&app, "GET", "/meals/bookings/me", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(common::total(&res.body), 1);
    assert_eq!(res.body["items"][0]["id"], json!(booking));

    // The kitchen's list is manager+; nobody else's `/me` shows the seat.
    let res = send(
        &app,
        "GET",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 1);
    let res = send(
        &app,
        "GET",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN);
    let res = send(&app, "GET", "/meals/bookings/me", Some(&mgr), None).await;
    assert_eq!(common::total(&res.body), 0);

    // A menu somebody still holds a seat on cannot be unpublished.
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/menus/{menu}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // Cancelling flips the row, it never deletes it.
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{booking}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "cancelled");
    let stamp = res.body["cancelled_at"].as_i64().expect("stamped");
    assert!(stamp > 0);
    // Cancelling again is idempotent — a `200` with the row as it stands, so a
    // cancel interrupted before its refund can be recovered by repeating it.
    // The stamp is not re-written: the seat was freed once.
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{booking}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "cancelled");
    assert_eq!(res.body["cancelled_at"], stamp);
    let res = send(
        &app,
        "GET",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 1, "the cancelled row survives");

    // Re-booking is the same row flipped back — one seat, never two.
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&ali),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(id_of(&res.body), booking);
    assert_eq!(res.body["status"], "booked");
    assert_eq!(res.body["cancelled_at"], json!(null));
    let res = send(
        &app,
        "GET",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 1);

    // Cancelled, the menu is free to go.
    send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{booking}"),
        Some(&ali),
        None,
    )
    .await;
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/menus/{menu}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
}

/// The cap refuses the seat that would overflow it, and a cancel genuinely
/// gives that seat back.
#[tokio::test]
async fn meal_booking_capacity_caps_and_a_cancel_frees_a_seat() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "cap_mgr", "manager").await;
    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&mgr),
        Some(json!({ "date": "2099-09-14", "slot": "lunch", "capacity": 2 })),
    )
    .await;
    let menu = id_of(&res.body);

    let students: Vec<String> = {
        let mut cookies = Vec::new();
        for i in 0..3 {
            cookies.push(login(&app, &format!("cap_student{i}")).await);
        }
        cookies
    };
    let book = |who: String| {
        let app = app.clone();
        let menu = menu.clone();
        async move {
            send(
                &app,
                "POST",
                &format!("/meals/menus/{menu}/bookings"),
                Some(&who),
                Some(json!({})),
            )
            .await
        }
    };

    let first = book(students[0].clone()).await;
    assert_eq!(first.status, StatusCode::CREATED);
    assert_eq!(book(students[1].clone()).await.status, StatusCode::CREATED);
    assert_eq!(book(students[2].clone()).await.status, StatusCode::CONFLICT);

    // Cancelling frees the seat — a cancelled row must not count.
    let booking = id_of(&first.body);
    send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{booking}"),
        Some(&students[0]),
        None,
    )
    .await;
    assert_eq!(book(students[2].clone()).await.status, StatusCode::CREATED);
    // …and the freed seat is gone again, so the first student cannot return.
    assert_eq!(book(students[0].clone()).await.status, StatusCode::CONFLICT);
}

/// The cap is a count-then-write pair, which SurrealDB does not serialize:
/// under a stampede it must still admit exactly `capacity` seats, and the
/// losers must get a 409 rather than a 500. Mirrors
/// `concurrent_duplicate_registrations_conflict_not_500`.
#[tokio::test]
async fn concurrent_meal_bookings_never_exceed_the_capacity() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "race_mgr", "manager").await;
    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&mgr),
        Some(json!({ "date": "2099-09-14", "slot": "lunch", "capacity": 3 })),
    )
    .await;
    let menu = id_of(&res.body);

    let mut students = Vec::new();
    for i in 0..24 {
        students.push(login(&app, &format!("race_student{i}")).await);
    }

    let mut handles = Vec::new();
    for cookie in students {
        let app = app.clone();
        let menu = menu.clone();
        handles.push(tokio::spawn(async move {
            send(
                &app,
                "POST",
                &format!("/meals/menus/{menu}/bookings"),
                Some(&cookie),
                Some(json!({})),
            )
            .await
            .status
        }));
    }

    let mut booked = 0;
    for h in handles {
        let status = h.await.unwrap();
        assert!(
            status == StatusCode::CREATED || status == StatusCode::CONFLICT,
            "a lost race must be a 409, never a 500: {status}"
        );
        booked += i32::from(status == StatusCode::CREATED);
    }
    assert_eq!(booked, 3, "exactly the capacity was admitted");

    let res = send(
        &app,
        "GET",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 3, "and only three rows exist");

    // The rows are downstream of the counter, so assert the counter itself:
    // it is what every booking's `WHERE` compares, and a drift here would open
    // the cap on the next booking however tidy the listing looks.
    let mut counted = db
        .query("SELECT VALUE seats_booked FROM type::record('menu', $m)")
        .bind(("m", menu.clone()))
        .await
        .expect("counter read")
        .check()
        .expect("counter read");
    assert_eq!(
        counted.take::<Vec<i64>>(0).expect("counter column"),
        vec![3],
        "the stored seat counter must agree with the seats handed out"
    );
}

/// Publishing the same day+slot twice at once must be one menu and one 409 —
/// never two rows and never a 500. The day and slot *are* the record id, so the
/// racers collide on a single record inside the database rather than on a
/// check-then-write pair, which is the only form no interleaving can defeat.
#[tokio::test]
async fn concurrent_duplicate_menu_publishes_conflict_not_500() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "publish_race_mgr", "manager").await;

    let mut handles = Vec::new();
    for _ in 0..8 {
        let app = app.clone();
        let mgr = mgr.clone();
        handles.push(tokio::spawn(async move {
            send(
                &app,
                "POST",
                "/meals/menus",
                Some(&mgr),
                Some(json!({ "date": "2099-09-14", "slot": "lunch" })),
            )
            .await
            .status
        }));
    }
    let mut published = 0;
    for h in handles {
        let status = h.await.unwrap();
        assert!(
            status == StatusCode::CREATED || status == StatusCode::CONFLICT,
            "a lost publish must be a 409, never a 500: {status}"
        );
        published += i32::from(status == StatusCode::CREATED);
    }
    assert_eq!(published, 1, "exactly one publish won");

    let res = send(&app, "GET", "/meals/menus", Some(&mgr), None).await;
    assert_eq!(common::total(&res.body), 1, "and exactly one menu exists");
}

/// Two appends of one ledger id must both succeed: the id is the idempotence
/// key, so the loser reads back the winner's line instead of surfacing the
/// duplicate-key error as a 500. Driven at the domain level on purpose — every
/// HTTP path into the ledger runs under `MENU_LOCK`, which serializes the two
/// writers before they can ever collide.
#[tokio::test]
async fn concurrent_ledger_appends_of_one_id_write_one_line_not_a_500() {
    use hezarfen_backend::domain::meal_booking::{MealBooking, MealBookingId};
    use hezarfen_backend::domain::meal_ledger::MealLedger;

    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "ledger_race_mgr", "manager").await;
    let ali = login(&app, "ledger_race_ali").await;

    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&mgr),
        Some(json!({ "date": "2099-09-15", "slot": "lunch" })),
    )
    .await;
    let menu = id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/dishes"),
        Some(&mgr),
        Some(json!({ "name": "Pilav", "price_minor": 1000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);

    // The seat charges 1000; its reversal line does not exist yet, so both
    // racers below derive the same unwritten id.
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&ali),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let booking = MealBooking::read(&MealBookingId::from_key(&id_of(&res.body)), &db)
        .await
        .expect("read the booking")
        .expect("the booking exists");
    let student = booking.get_student().clone();
    assert_eq!(
        MealLedger::balance_of(&student, &db).await.unwrap(),
        -1000,
        "the seat was charged once"
    );

    let (first, second) = tokio::join!(
        MealLedger::reverse_booking(&booking, &student, &db),
        MealLedger::reverse_booking(&booking, &student, &db),
    );
    assert!(first.is_ok(), "the winner appended: {first:?}");
    assert!(
        second.is_ok(),
        "the loser must read back the winner's line, not fail: {second:?}"
    );

    let (lines, _) = MealLedger::list_for_student(&student, None, 0, &db)
        .await
        .unwrap();
    assert_eq!(lines.len(), 2, "one charge and exactly one reversal");
    assert_eq!(
        MealLedger::balance_of(&student, &db).await.unwrap(),
        0,
        "the money moved back exactly once"
    );
}

/// One knob closes both ends: past the school's `meal_cancel_cutoff_minutes`
/// neither a new booking nor a cancellation lands.
#[tokio::test]
async fn meal_cutoff_closes_booking_and_cancelling_alike() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "cutoff_mgr", "manager").await;
    let ali = login(&app, "cutoff_ali").await;

    // No cutoff configured yet, so today's meal books at any hour — the day
    // being over is the *other* refusal, and today's is not over.
    let today = hezarfen_backend::domain::timestamp::Timestamp::today_utc()
        .format("%Y-%m-%d")
        .to_string();
    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&mgr),
        Some(json!({ "date": today, "slot": "lunch" })),
    )
    .await;
    let menu = id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&ali),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let booking = id_of(&res.body);

    // The serving hour goes with the knob: a slot without one has no instant
    // for the deadline to count back from, so the cutoff binds none of its
    // menus (see `meal_cutoff_counts_back_from_the_slots_serving_time`). Lunch
    // is served at 00:00 UTC, so today's deadline passed an hour before the day
    // began — whatever hour this test runs at.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&mgr),
        Some(json!({
            "meal_cancel_cutoff_minutes": 60,
            "meal_slots": [{ "name": "lunch", "serving_minute": 0 }],
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Both ends now refuse: the meal is settled.
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{booking}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let veli = login(&app, "cutoff_veli").await;
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&veli),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // A meal far enough ahead is untouched by the same cutoff.
    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&mgr),
        Some(json!({ "date": "2099-01-06", "slot": "lunch" })),
    )
    .await;
    let later = id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{later}/bookings"),
        Some(&ali),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
}

/// The cutoff counts back from the slot's **serving time**, not from midnight.
/// The discriminating case: a menu on tomorrow's date whose lunch is served at
/// 23:59 UTC, with a cutoff wide enough that *midnight* of that date is already
/// inside it. Under the old midnight rule the seat is shut; measured from the
/// serving time it is open for another ~24 hours. Then, with no serving time on
/// the slot, the very same menu closes at **nothing at all**: there is no
/// instant left to count the deadline back from, and counting from midnight —
/// as this did — shut every same-day menu of a school that had set the cutoff
/// knob without serving hours, which is how all three slots ship.
#[tokio::test]
async fn meal_cutoff_counts_back_from_the_slots_serving_time() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "serve_mgr", "manager").await;
    let ali = login(&app, "serve_ali").await;

    // Tomorrow UTC, so midnight of the menu's date is 0–1440 minutes ahead.
    let now = Timestamp::now().as_millis();
    let tomorrow = Timestamp::today_utc() + chrono::Days::new(1);
    let midnight = tomorrow
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis();
    // Five minutes wider than the gap to midnight: the midnight deadline is
    // now in the past, while the 23:59 serving deadline is ~24 hours out.
    let cutoff = (midnight - now).div_euclid(60_000) + 5;
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&mgr),
        Some(json!({
            "meal_slots": [{ "name": "lunch", "serving_minute": 1439 }],
            "meal_cancel_cutoff_minutes": cutoff,
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&mgr),
        Some(json!({ "date": tomorrow.to_string(), "slot": "lunch" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let menu = id_of(&res.body);

    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&ali),
        Some(json!({})),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "measured from 23:59 the seat is still open; the midnight rule would \
         have refused it: {}",
        res.body
    );
    let booking = id_of(&res.body);

    // Drop the serving time from the same slot: there is now no instant to
    // count the deadline back from, so this menu closes at nothing — it does
    // *not* snap back to midnight, which used to shut the canteen of every
    // school that set the cutoff without setting hours. The slot list is read
    // live, so the menu published under it moves with the edit.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&mgr),
        Some(json!({ "meal_slots": [{ "name": "lunch" }] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        res.body["meal_slots"],
        json!([{ "name": "lunch", "serving_minute": null }]),
        "{}",
        res.body
    );

    let veli = login(&app, "serve_veli").await;
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&veli),
        Some(json!({})),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "an unset serving hour is an unenforced cutoff: {}",
        res.body
    );
    // …and the student holding a seat can still give it back themselves,
    // which the midnight fallback took away from them.
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{booking}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Serving lunch at midnight instead makes the same cutoff bite: the
    // deadline is that instant minus the cutoff, which is already past.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&mgr),
        Some(json!({ "meal_slots": [{ "name": "lunch", "serving_minute": 0 }] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&ali),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
}

/// Who may take a seat: the student it is for, or a parent holding a link to
/// them. Nobody else — a teacher does not order lunch for a child.
#[tokio::test]
async fn meal_bookings_are_student_or_linked_parent_only() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let mgr = login_as(&app, &db, "kin_mgr", "manager").await;
    let teacher = login_as(&app, &db, "kin_teacher", "teacher").await;
    let mom = login_as(&app, &db, "kin_mom", "parent").await;
    let mom_id = me_id(&app, &mom).await;
    let ali = login(&app, "kin_ali").await;
    let ali_id = me_id(&app, &ali).await;
    let veli = login(&app, "kin_veli").await;
    let veli_id = me_id(&app, &veli).await;

    let res = send(
        &app,
        "POST",
        &format!("/users/{mom_id}/students"),
        Some(&admin),
        Some(json!({ "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&mgr),
        Some(json!({ "date": "2099-09-14", "slot": "lunch" })),
    )
    .await;
    let menu = id_of(&res.body);
    let path = format!("/meals/menus/{menu}/bookings");

    // The parent books for their own child.
    let res = send(
        &app,
        "POST",
        &path,
        Some(&mom),
        Some(json!({ "student_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["student"]["username"], "kin_ali");
    assert_eq!(res.body["booked_by"]["username"], "kin_mom");
    let booking = id_of(&res.body);
    // …and sees it on their own list, though the seat is not theirs.
    let res = send(&app, "GET", "/meals/bookings/me", Some(&mom), None).await;
    assert_eq!(common::total(&res.body), 1);

    // Never for someone else's child, and never without naming one.
    let res = send(
        &app,
        "POST",
        &path,
        Some(&mom),
        Some(json!({ "student_id": veli_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = send(&app, "POST", &path, Some(&mom), Some(json!({}))).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // Staff do not order for children through this route.
    let res = send(
        &app,
        "POST",
        &path,
        Some(&teacher),
        Some(json!({ "student_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &path,
        Some(&mgr),
        Some(json!({ "student_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);

    // A student books only for themselves, and cancels only their own.
    let res = send(
        &app,
        "POST",
        &path,
        Some(&veli),
        Some(json!({ "student_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{booking}"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);

    // The linked parent may give the seat back.
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{booking}"),
        Some(&mom),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "cancelled");
}

// --- meals: the ledger ----------------------------------------------------

/// The money, end to end: booking charges the menu's price *snapshot*, editing
/// a dish afterwards never moves that charge, cancelling appends a reversal
/// (leaving the charge standing — the append-only proof), and re-booking bills
/// the new price.
#[tokio::test]
async fn meal_booking_charges_a_price_snapshot_and_a_cancel_reverses_it() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "money_mgr", "manager").await;
    let ali = login(&app, "money_ali").await;
    let ali_id = me_id(&app, &ali).await;

    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&mgr),
        Some(json!({ "date": "2099-09-14", "slot": "lunch" })),
    )
    .await;
    let menu = id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/dishes"),
        Some(&mgr),
        Some(json!({ "name": "Çorba", "price_minor": 4_500 })),
    )
    .await;
    let dish = id_of(&res.body);
    send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/dishes"),
        Some(&mgr),
        Some(json!({ "name": "Pilav", "price_minor": 2_000 })),
    )
    .await;

    // A fresh student owes nothing.
    let res = send(&app, "GET", "/meals/balance/me", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["balance_minor"], 0);

    // Booking charges the sum of the dishes — negative means "owes".
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&ali),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let booking = id_of(&res.body);
    let res = send(&app, "GET", "/meals/balance/me", Some(&ali), None).await;
    assert_eq!(res.body["balance_minor"], -6_500);

    // Booking again is the same seat, so it must not bill twice.
    send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&ali),
        Some(json!({})),
    )
    .await;
    let res = send(&app, "GET", "/meals/balance/me", Some(&ali), None).await;
    assert_eq!(res.body["balance_minor"], -6_500, "one seat, one charge");

    // A price edit is not retroactive: what the student owes is what the menu
    // cost the day they booked.
    let res = send(
        &app,
        "PATCH",
        &format!("/meals/dishes/{dish}"),
        Some(&mgr),
        Some(json!({ "price_minor": 9_900 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(&app, "GET", "/meals/balance/me", Some(&ali), None).await;
    assert_eq!(res.body["balance_minor"], -6_500, "the snapshot is frozen");

    // Cancelling appends a reversal; the charge row stays right where it was.
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{booking}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(&app, "GET", "/meals/balance/me", Some(&ali), None).await;
    assert_eq!(res.body["balance_minor"], 0);

    let res = send(
        &app,
        "GET",
        &format!("/meals/ledger/{ali_id}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(common::total(&res.body), 2, "both lines survive the cancel");
    let lines = res.body["items"].as_array().unwrap().clone();
    // Newest first: the reversal, then the charge it points at.
    assert_eq!(lines[0]["kind"], "reversal");
    assert_eq!(lines[0]["amount_minor"], 6_500);
    assert_eq!(lines[1]["kind"], "charge");
    assert_eq!(lines[1]["amount_minor"], 6_500);
    assert_eq!(lines[1]["source"], json!(booking));
    assert_eq!(lines[0]["source"], lines[1]["id"]);

    // Re-booking is a fresh charge, at the *new* price.
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&ali),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(&app, "GET", "/meals/balance/me", Some(&ali), None).await;
    assert_eq!(res.body["balance_minor"], -11_900);
    let res = send(
        &app,
        "GET",
        &format!("/meals/ledger/{ali_id}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 3);
}

/// Recording cash is admin-only — a manager runs the kitchen, not the till.
#[tokio::test]
async fn meal_credits_are_admin_only_and_raise_the_balance() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "till_admin", "admin").await;
    let mgr = login_as(&app, &db, "till_mgr", "manager").await;
    let ali = login(&app, "till_ali").await;
    let ali_id = me_id(&app, &ali).await;

    let body = json!({ "student_id": ali_id, "amount_minor": 25_000, "method": "cash" });
    let res = send(
        &app,
        "POST",
        "/meals/credits",
        Some(&mgr),
        Some(body.clone()),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = send(
        &app,
        "POST",
        "/meals/credits",
        Some(&ali),
        Some(body.clone()),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);

    let res = send(&app, "POST", "/meals/credits", Some(&admin), Some(body)).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["kind"], "credit");
    assert_eq!(res.body["amount_minor"], 25_000);
    assert_eq!(res.body["method"], "cash");
    assert_eq!(res.body["note"], json!(null));
    assert_eq!(res.body["source"], json!(null));
    assert_eq!(res.body["recorded_by"]["username"], "till_admin");

    let res = send(&app, "GET", "/meals/balance/me", Some(&ali), None).await;
    assert_eq!(res.body["balance_minor"], 25_000);

    // The amount is bounded and positive; the target must be a student.
    for bad in [
        json!({ "student_id": ali_id, "amount_minor": 0 }),
        json!({ "student_id": ali_id, "amount_minor": -100 }),
        json!({ "student_id": ali_id, "amount_minor": 10_000_001 }),
    ] {
        let res = send(&app, "POST", "/meals/credits", Some(&admin), Some(bad)).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    }
    let mgr_id = me_id(&app, &mgr).await;
    let res = send(
        &app,
        "POST",
        "/meals/credits",
        Some(&admin),
        Some(json!({ "student_id": mgr_id, "amount_minor": 100 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    // …and the failed writes left the balance exactly where it was.
    let res = send(&app, "GET", "/meals/balance/me", Some(&ali), None).await;
    assert_eq!(res.body["balance_minor"], 25_000);
}

/// Whose money a caller may read: their own, their linked child's, or — as
/// manager+ — anyone's. Never another family's, and never a teacher's business.
#[tokio::test]
async fn meal_balance_reads_are_manager_only_outside_the_family() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "wallet_boss", "admin").await;
    let teacher = login_as(&app, &db, "wallet_teacher", "teacher").await;
    let mom = login_as(&app, &db, "wallet_mom", "parent").await;
    let mom_id = me_id(&app, &mom).await;
    let ali = login(&app, "wallet_ali").await;
    let ali_id = me_id(&app, &ali).await;
    let veli = login(&app, "wallet_veli").await;
    let veli_id = me_id(&app, &veli).await;

    let res = send(
        &app,
        "POST",
        &format!("/users/{mom_id}/students"),
        Some(&admin),
        Some(json!({ "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // The linked parent reads their child's balance and statement.
    let res = send(
        &app,
        "GET",
        &format!("/meals/balance/{ali_id}"),
        Some(&mom),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["student"]["username"], "wallet_ali");
    let res = send(
        &app,
        "GET",
        &format!("/meals/ledger/{ali_id}"),
        Some(&mom),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Someone else's child is a 403, on both routes.
    for path in [
        format!("/meals/balance/{veli_id}"),
        format!("/meals/ledger/{veli_id}"),
    ] {
        let res = send(&app, "GET", &path, Some(&mom), None).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    }

    // A student reads their own money, never a classmate's.
    let res = send(
        &app,
        "GET",
        &format!("/meals/balance/{ali_id}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/meals/balance/{ali_id}"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);

    // A teacher does not, however: what a family owes the canteen is money,
    // and money is manager+ here exactly as it is on /payments.
    let res = send(
        &app,
        "GET",
        &format!("/meals/balance/{ali_id}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);

    // Manager+ reads any student's.
    let res = send(
        &app,
        "GET",
        &format!("/meals/balance/{ali_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

// --- meals: attendance ----------------------------------------------------

/// Marking who ate is reporting, never billing: a booked no-show still owes the
/// full price, and a walk-in served without a booking owes nothing. Also covers
/// the composite id (re-marking flips the row) and the teacher+ gate.
#[tokio::test]
async fn meal_attendance_never_moves_money() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "ate_mgr", "manager").await;
    let teacher = login_as(&app, &db, "ate_teacher", "teacher").await;
    let ali = login(&app, "ate_ali").await;
    let ali_id = me_id(&app, &ali).await;
    let veli = login(&app, "ate_veli").await;
    let veli_id = me_id(&app, &veli).await;

    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&mgr),
        Some(json!({ "date": "2099-09-14", "slot": "lunch" })),
    )
    .await;
    let menu = id_of(&res.body);
    send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/dishes"),
        Some(&mgr),
        Some(json!({ "name": "Çorba", "price_minor": 4_500 })),
    )
    .await;

    // Ali books (and is charged); Veli never books.
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&ali),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(&app, "GET", "/meals/balance/me", Some(&ali), None).await;
    assert_eq!(res.body["balance_minor"], -4_500);

    let path = format!("/meals/menus/{menu}/attendance");

    // Served shows up on the menu's list.
    let res = send(
        &app,
        "POST",
        &path,
        Some(&teacher),
        Some(json!({ "student_id": ali_id, "status": "served" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let mark = id_of(&res.body);
    assert_eq!(res.body["status"], "served");
    assert_eq!(res.body["student"]["username"], "ate_ali");
    assert_eq!(res.body["marked_by"]["username"], "ate_teacher");
    assert_eq!(res.body["menu_id"], json!(menu));
    let res = send(&app, "GET", &path, Some(&teacher), None).await;
    assert_eq!(common::total(&res.body), 1);
    assert_eq!(res.body["items"][0]["id"], json!(mark));

    // Re-marking the same student flips the row — one row per (menu, student).
    let res = send(
        &app,
        "POST",
        &path,
        Some(&teacher),
        Some(json!({ "student_id": ali_id, "status": "missed" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(id_of(&res.body), mark);
    assert_eq!(res.body["status"], "missed");
    let res = send(&app, "GET", &path, Some(&teacher), None).await;
    assert_eq!(
        common::total(&res.body),
        1,
        "the mark was flipped, not doubled"
    );

    // THE BILLING-ISOLATION PROOF: the no-show still owes the whole price, and
    // his statement grew no reversal line. The kitchen bought the food.
    let res = send(&app, "GET", "/meals/balance/me", Some(&ali), None).await;
    assert_eq!(
        res.body["balance_minor"], -4_500,
        "attendance must never move money"
    );
    let res = send(
        &app,
        "GET",
        &format!("/meals/ledger/{ali_id}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 1, "only the booking's charge");
    assert_eq!(res.body["items"][0]["kind"], "charge");

    // A walk-in with no booking is recordable — and still not charged.
    let res = send(
        &app,
        "POST",
        &path,
        Some(&teacher),
        Some(json!({ "student_id": veli_id, "status": "served" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(&app, "GET", "/meals/balance/me", Some(&veli), None).await;
    assert_eq!(res.body["balance_minor"], 0, "a walk-in is never billed");

    // Students never mark, not even themselves, and never read the list.
    let res = send(
        &app,
        "POST",
        &path,
        Some(&ali),
        Some(json!({ "student_id": ali_id, "status": "served" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = send(&app, "GET", &path, Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);

    // Bad inputs: a roll-call status is not a canteen status, and the menu and
    // the student both have to exist.
    let res = send(
        &app,
        "POST",
        &path,
        Some(&teacher),
        Some(json!({ "student_id": ali_id, "status": "excused" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &path,
        Some(&teacher),
        Some(json!({ "student_id": "nobody", "status": "served" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    let res = send(
        &app,
        "POST",
        "/meals/menus/missing/attendance",
        Some(&teacher),
        Some(json!({ "student_id": ali_id, "status": "served" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
}

/// Deleting a menu takes its marks with it. The menu id is deterministic on
/// `(date, slot)`, so republishing the same meal mints the *same* record id: a
/// mark left behind by the deleted menu would come back as a mark on the new
/// one — the kitchen would read "served" for a student who never came, and the
/// student's own report would cite a menu that no longer exists.
#[tokio::test]
async fn deleting_a_menu_takes_its_attendance_with_it() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "ghost_mgr", "manager").await;
    let teacher = login_as(&app, &db, "ghost_teacher", "teacher").await;
    let ali = login(&app, "ghost_ali").await;
    let ali_id = me_id(&app, &ali).await;

    let publish = json!({ "date": "2099-10-05", "slot": "lunch" });
    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&mgr),
        Some(publish.clone()),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let menu = id_of(&res.body);

    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/attendance"),
        Some(&teacher),
        Some(json!({ "student_id": ali_id, "status": "served" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // No seat was ever taken, so the counter guard lets the delete through.
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/menus/{menu}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);

    // The student's own report must not cite a menu that is gone.
    let res = send(
        &app,
        "GET",
        &format!("/meals/attendance/{ali_id}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        common::total(&res.body),
        0,
        "the mark outlived the menu it was taken on"
    );

    // Republishing the same day and slot lands on the same record id.
    let res = send(&app, "POST", "/meals/menus", Some(&mgr), Some(publish)).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let again = id_of(&res.body);
    assert_eq!(again, menu, "the id is deterministic on (date, slot)");

    let res = send(
        &app,
        "GET",
        &format!("/meals/menus/{menu}/attendance"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        common::total(&res.body),
        0,
        "the deleted menu's mark came back on the republished one"
    );
}

/// The per-student report: a lexical `?from=&to=` window over the menu's day,
/// readable by teacher+ or a linked parent and by nobody else.
#[tokio::test]
async fn meal_attendance_report_is_ranged_and_gated() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "ate_boss", "admin").await;
    let mgr = login_as(&app, &db, "range_mgr", "manager").await;
    let teacher = login_as(&app, &db, "range_teacher", "teacher").await;
    let mom = login_as(&app, &db, "range_mom", "parent").await;
    let mom_id = me_id(&app, &mom).await;
    let ali = login(&app, "range_ali").await;
    let ali_id = me_id(&app, &ali).await;
    let veli = login(&app, "range_veli").await;
    let veli_id = me_id(&app, &veli).await;

    let res = send(
        &app,
        "POST",
        &format!("/users/{mom_id}/students"),
        Some(&admin),
        Some(json!({ "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Three days, one mark each for Ali.
    for date in ["2099-09-01", "2099-09-15", "2099-09-30"] {
        let res = send(
            &app,
            "POST",
            "/meals/menus",
            Some(&mgr),
            Some(json!({ "date": date, "slot": "lunch" })),
        )
        .await;
        let menu = id_of(&res.body);
        let res = send(
            &app,
            "POST",
            &format!("/meals/menus/{menu}/attendance"),
            Some(&teacher),
            Some(json!({ "student_id": ali_id, "status": "served" })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    }

    // Unbounded, then each bound, then both — the window is the menu's day.
    for (query, expected) in [
        ("", 3),
        ("?from=2099-09-15", 2),
        ("?to=2099-09-15", 2),
        ("?from=2099-09-10&to=2099-09-20", 1),
        ("?from=2099-10-01", 0),
    ] {
        let res = send(
            &app,
            "GET",
            &format!("/meals/attendance/{ali_id}{query}"),
            Some(&teacher),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        assert_eq!(common::total(&res.body), expected, "range {query}");
    }

    // A malformed bound is a 400, never a silently ignored filter.
    let res = send(
        &app,
        "GET",
        &format!("/meals/attendance/{ali_id}?from=2026-9-1"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // The linked parent reads her child's report; another child's is a 403,
    // and a classmate never reads it at all.
    let res = send(
        &app,
        "GET",
        &format!("/meals/attendance/{ali_id}"),
        Some(&mom),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(common::total(&res.body), 3);
    let res = send(
        &app,
        "GET",
        &format!("/meals/attendance/{veli_id}"),
        Some(&mom),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/meals/attendance/{ali_id}"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
}

/// The dietary profile and the `conflicts` it produces on a menu read: the
/// school writes it, everyone in the reading gate sees it, and the overlap is
/// computed against the *caller's* own tags.
#[tokio::test]
async fn dietary_profile_drives_menu_conflicts() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "diet_boss", "admin").await;
    let mgr = login_as(&app, &db, "diet_mgr", "manager").await;
    let teacher = login_as(&app, &db, "diet_teacher", "teacher").await;
    let mom = login_as(&app, &db, "diet_mom", "parent").await;
    let mom_id = me_id(&app, &mom).await;
    let other_mom = login_as(&app, &db, "diet_aunt", "parent").await;
    let ali = login(&app, "diet_ali").await;
    let ali_id = me_id(&app, &ali).await;
    let veli = login(&app, "diet_veli").await;
    let veli_id = me_id(&app, &veli).await;

    let res = send(
        &app,
        "POST",
        &format!("/users/{mom_id}/students"),
        Some(&admin),
        Some(json!({ "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // No profile yet: empty, never a 404.
    let res = send(&app, "GET", "/meals/profiles/me", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["tags"], json!([]));
    assert_eq!(res.body["note"], json!(null));
    assert_eq!(res.body["updated_at"], json!(null));

    // A student does not write their own allergen list, and neither does a
    // teacher — this is a manager+ safety record.
    for actor in [&ali, &teacher] {
        let res = send(
            &app,
            "PATCH",
            &format!("/meals/profiles/{ali_id}"),
            Some(actor),
            Some(json!({ "tags": ["nut_allergy"] })),
        )
        .await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    }

    // A tag outside the school's list is a 400, never a stored string.
    let res = send(
        &app,
        "PATCH",
        &format!("/meals/profiles/{ali_id}"),
        Some(&mgr),
        Some(json!({ "tags": ["halal"] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    let res = send(
        &app,
        "PATCH",
        &format!("/meals/profiles/{ali_id}"),
        Some(&mgr),
        Some(json!({ "tags": ["nut_allergy", "nut_allergy"], "note": "  EpiPen  " })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["tags"], json!(["nut_allergy"]));
    assert_eq!(res.body["note"], "EpiPen");
    assert_eq!(res.body["updated_by"]["username"], "diet_mgr");

    // The student reads it back at `/me`; a second PATCH edits the same row.
    let res = send(&app, "GET", "/meals/profiles/me", Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["tags"], json!(["nut_allergy"]));
    assert_eq!(res.body["student"]["username"], "diet_ali");

    let res = send(
        &app,
        "PATCH",
        &format!("/meals/profiles/{ali_id}"),
        Some(&mgr),
        Some(json!({ "note": null })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["note"], json!(null));
    assert_eq!(res.body["tags"], json!(["nut_allergy"]), "tags kept");

    // The linked parent reads her child's; an unlinked one does not.
    let res = send(
        &app,
        "GET",
        &format!("/meals/profiles/{ali_id}"),
        Some(&mom),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["tags"], json!(["nut_allergy"]));
    let res = send(
        &app,
        "GET",
        &format!("/meals/profiles/{ali_id}"),
        Some(&other_mom),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);

    // A profile is a student's thing — a teacher never carries one.
    let teacher_id = me_id(&app, &teacher).await;
    let res = send(
        &app,
        "PATCH",
        &format!("/meals/profiles/{teacher_id}"),
        Some(&mgr),
        Some(json!({ "tags": ["vegan"] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // One menu, two dishes: one Ali must avoid, one he must not.
    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&mgr),
        Some(json!({ "date": "2099-10-05", "slot": "lunch" })),
    )
    .await;
    let menu = id_of(&res.body);
    for (name, tags) in [
        ("Fıstıklı baklava", json!(["nut_allergy", "vegetarian"])),
        ("Pilav", json!(["vegan"])),
    ] {
        let res = send(
            &app,
            "POST",
            &format!("/meals/menus/{menu}/dishes"),
            Some(&mgr),
            Some(json!({ "name": name, "price_minor": 1000, "tags": tags })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    }

    // Ali sees his own conflict on the one dish, and nothing on the other.
    for path in [format!("/meals/menus/{menu}"), "/meals/menus".to_string()] {
        let res = send(&app, "GET", &path, Some(&ali), None).await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        let menu_body = if path == "/meals/menus" {
            res.body["items"][0].clone()
        } else {
            res.body.clone()
        };
        assert_eq!(
            menu_body["dishes"][0]["conflicts"],
            json!(["nut_allergy"]),
            "{path}: the tag Ali holds, and only it"
        );
        assert_eq!(menu_body["dishes"][1]["conflicts"], json!([]), "{path}");
    }

    // A student with no profile, and a manager (who never has one), both read
    // empty conflicts — that is correct, not a bug.
    for actor in [&veli, &mgr] {
        let res = send(
            &app,
            "GET",
            &format!("/meals/menus/{menu}"),
            Some(actor),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        assert_eq!(res.body["dishes"][0]["conflicts"], json!([]));
        assert_eq!(res.body["dishes"][1]["conflicts"], json!([]));
    }

    // Cleanup 1: a student reads their OWN meal-attendance history, like they
    // already read their own balance and statement.
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/attendance"),
        Some(&teacher),
        Some(json!({ "student_id": veli_id, "status": "served" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/meals/attendance/{veli_id}"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(common::total(&res.body), 1);
}

// --- meals: money under concurrency and stale grants ----------------------

/// Publish a lunch menu carrying one dish at `price`; returns (menu id,
/// student cookie). Every money-race test below needs the same three rows.
async fn priced_menu(tag: &str, price: i64) -> (axum::Router, String, String) {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, &format!("{tag}_mgr"), "manager").await;
    let stu = login(&app, &format!("{tag}_stu")).await;
    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&mgr),
        Some(json!({ "date": "2099-09-14", "slot": "lunch" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let menu = id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/dishes"),
        Some(&mgr),
        Some(json!({ "name": "Pilav", "price_minor": price })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    (app, menu, stu)
}

/// The sequential "booking twice bills once" test cannot see this: idempotence
/// that rests on a "is there already a charge?" scan is no idempotence at all,
/// since eight simultaneous POSTs of one seat all read "not yet" and all
/// append. One seat must mean one charge, so the ledger line is keyed by
/// (booking, attempt) and written under the same lock as the seat.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_bookings_of_one_seat_bill_it_once() {
    let (app, menu, stu) = priced_menu("race", 5_000).await;

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let app = app.clone();
        let stu = stu.clone();
        let menu = menu.clone();
        tasks.push(tokio::spawn(async move {
            send(
                &app,
                "POST",
                &format!("/meals/menus/{menu}/bookings"),
                Some(&stu),
                Some(json!({})),
            )
            .await
        }));
    }
    for task in tasks {
        let res = task.await.unwrap();
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    }

    let res = send(&app, "GET", "/meals/bookings/me", Some(&stu), None).await;
    assert_eq!(common::total(&res.body), 1, "one seat: {}", res.body);
    let res = send(&app, "GET", "/meals/balance/me", Some(&stu), None).await;
    assert_eq!(
        res.body["balance_minor"], -5_000,
        "one seat, one charge: {}",
        res.body
    );
}

/// The same stampede on a menu with exactly one seat. Every racer is the *same*
/// student, so between them they owe one seat — but each claims before it can
/// know another already placed the row, and the losers' claims are refused as
/// "full". Answering that with a 409 would refuse a student the booking that
/// just succeeded on their behalf, so a refused claim looks for this very seat
/// before it blames the cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_bookings_of_one_seat_never_refuse_their_own_winner() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "tight_mgr", "manager").await;
    let stu = login(&app, "tight_stu").await;
    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&mgr),
        Some(json!({ "date": "2099-09-14", "slot": "lunch", "capacity": 1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let menu = id_of(&res.body);

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let (app, stu, menu) = (app.clone(), stu.clone(), menu.clone());
        tasks.push(tokio::spawn(async move {
            send(
                &app,
                "POST",
                &format!("/meals/menus/{menu}/bookings"),
                Some(&stu),
                Some(json!({})),
            )
            .await
        }));
    }
    for task in tasks {
        let res = task.await.unwrap();
        assert_eq!(
            res.status,
            StatusCode::CREATED,
            "one student, one seat, no refusal: {}",
            res.body
        );
    }

    let res = send(&app, "GET", "/meals/bookings/me", Some(&stu), None).await;
    assert_eq!(common::total(&res.body), 1, "one seat: {}", res.body);
    let mut counted = db
        .query("SELECT VALUE seats_booked FROM type::record('menu', $m)")
        .bind(("m", menu.clone()))
        .await
        .expect("counter read")
        .check()
        .expect("counter read");
    assert_eq!(
        counted.take::<Vec<i64>>(0).expect("counter column"),
        vec![1],
        "and the seat it cost is the one seat the menu has"
    );
}

/// A repeat POST racing the cancel. A cancel that reverses whatever charge it
/// can find, off a booking row read before the lock, can flip the seat to
/// `cancelled` while the charge of a newer attempt stands — the student owes
/// for a seat they do not hold. Keying the reversal to the attempt, and
/// re-reading the row inside the lock, is what closes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_cancelled_seat_never_keeps_its_charge() {
    let (app, menu, stu) = priced_menu("race2", 3_000).await;
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&stu),
        Some(json!({})),
    )
    .await;
    let booking = id_of(&res.body);

    for round in 0..20 {
        let book = {
            let (app, stu, menu) = (app.clone(), stu.clone(), menu.clone());
            tokio::spawn(async move {
                send(
                    &app,
                    "POST",
                    &format!("/meals/menus/{menu}/bookings"),
                    Some(&stu),
                    Some(json!({})),
                )
                .await
            })
        };
        let cancel = {
            let (app, stu, booking) = (app.clone(), stu.clone(), booking.clone());
            tokio::spawn(async move {
                send(
                    &app,
                    "DELETE",
                    &format!("/meals/bookings/{booking}"),
                    Some(&stu),
                    None,
                )
                .await
            })
        };
        book.await.unwrap();
        cancel.await.unwrap();
        // Settle the seat into `cancelled`, then the money must be back.
        send(
            &app,
            "DELETE",
            &format!("/meals/bookings/{booking}"),
            Some(&stu),
            None,
        )
        .await;
        let res = send(&app, "GET", "/meals/bookings/me", Some(&stu), None).await;
        assert_eq!(
            common::items(&res.body)[0]["status"],
            "cancelled",
            "round {round}: {}",
            res.body
        );
        let res = send(&app, "GET", "/meals/balance/me", Some(&stu), None).await;
        assert_eq!(
            res.body["balance_minor"], 0,
            "round {round}: a cancelled seat still owes: {}",
            res.body
        );
    }
}

/// "Was free" is a recorded fact, not the absence of a charge. A seat taken
/// off a dishless menu writes no ledger line; if that were all the code knew,
/// a dish added afterwards would make the next POST of the *same held seat*
/// look unbilled and charge it — a price the student never agreed to.
#[tokio::test]
async fn a_repeat_post_never_bills_a_seat_that_was_free_when_taken() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "free_mgr", "manager").await;
    let stu = login(&app, "free_stu").await;
    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&mgr),
        Some(json!({ "date": "2099-09-14", "slot": "lunch" })),
    )
    .await;
    let menu = id_of(&res.body);

    // The seat is taken while the menu carries no dishes at all.
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&stu),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(&app, "GET", "/meals/balance/me", Some(&stu), None).await;
    assert_eq!(res.body["balance_minor"], 0, "a free menu bills nothing");

    // The kitchen prices the menu afterwards.
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/dishes"),
        Some(&mgr),
        Some(json!({ "name": "Pilav", "price_minor": 9_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    // A refresh re-POSTs the seat the student already holds.
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&stu),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(&app, "GET", "/meals/balance/me", Some(&stu), None).await;
    assert_eq!(
        res.body["balance_minor"], 0,
        "a repeat POST on a held seat moves no money: {}",
        res.body
    );

    // Cancelling and taking the seat again *is* a fresh attempt, at the price
    // the menu carries now — that is the same rule, not an exception to it.
    let res = send(&app, "GET", "/meals/bookings/me", Some(&stu), None).await;
    let booking = common::items(&res.body)[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{booking}"),
        Some(&stu),
        None,
    )
    .await;
    send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&stu),
        Some(json!({})),
    )
    .await;
    let res = send(&app, "GET", "/meals/balance/me", Some(&stu), None).await;
    assert_eq!(res.body["balance_minor"], -9_000, "{}", res.body);
}

/// A revoked parent link must go inert everywhere, `booked_by` included: the
/// seats a parent paid for stay visible while the link lives, and vanish with
/// it. The leak this catches was a *live* one — the unlinked parent kept
/// watching the child's row, seeing a cancellation made after the unlink.
#[tokio::test]
async fn an_unlinked_parent_loses_the_live_view_of_a_childs_bookings() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "lv_boss", "admin").await;
    let mgr = login_as(&app, &db, "lv_mgr", "manager").await;
    let mom = login_as(&app, &db, "lv_mom", "parent").await;
    let mom_id = me_id(&app, &mom).await;
    let kid = login(&app, "lv_kid").await;
    let kid_id = me_id(&app, &kid).await;
    let res = send(
        &app,
        "POST",
        &format!("/users/{mom_id}/students"),
        Some(&admin),
        Some(json!({ "user_id": kid_id })),
    )
    .await;
    assert!(res.status.is_success(), "link: {}", res.body);
    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&mgr),
        Some(json!({ "date": "2027-08-01", "slot": "lunch" })),
    )
    .await;
    let menu = id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&mom),
        Some(json!({ "student_id": kid_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let booking = id_of(&res.body);

    // While the link lives, mom sees the seat she booked.
    let res = send(&app, "GET", "/meals/bookings/me", Some(&mom), None).await;
    assert_eq!(common::total(&res.body), 1, "{}", res.body);

    let res = send(
        &app,
        "DELETE",
        &format!("/users/{mom_id}/students/{kid_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert!(res.status.is_success(), "unlink: {}", res.body);

    // The kid cancels *after* the link is gone — mom must not see that.
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{booking}"),
        Some(&kid),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(&app, "GET", "/meals/bookings/me", Some(&mom), None).await;
    assert_eq!(
        common::total(&res.body),
        0,
        "an unlinked parent still reads the child's seat: {}",
        res.body
    );
    // The child's own view is untouched.
    let res = send(&app, "GET", "/meals/bookings/me", Some(&kid), None).await;
    assert_eq!(common::total(&res.body), 1, "{}", res.body);
}

/// A cancel is two writes — the status flip, then the reversal — and axum drops
/// the handler future when the client goes away (tab closed, proxy timeout). A
/// drop landing between them freed the seat and left the charge standing, and
/// while an already-cancelled row was a `409` **no route on the API could ever
/// append the missing reversal**: the student stayed billed for a meal they had
/// given back, and re-booking billed them a second time. So the cancel is
/// idempotent — repeating it replays the reversal, keyed by (seat, attempt), so
/// it heals the gap exactly once.
#[tokio::test]
async fn a_cancel_cut_short_mid_flight_is_healed_by_repeating_it() {
    let (app, menu, stu) = priced_menu("cutshort", 3_000).await;
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&stu),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let booking = id_of(&res.body);
    let res = send(&app, "GET", "/meals/balance/me", Some(&stu), None).await;
    assert_eq!(res.body["balance_minor"], -3_000, "{}", res.body);

    // The DELETE, polled just far enough to flip the row, then dropped.
    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/meals/bookings/{booking}"))
        .header("cookie", &stu)
        .body(Body::empty())
        .unwrap();
    let mut fut = Box::pin(app.clone().oneshot(req));
    for _ in 0..7 {
        assert!(
            futures_util::poll!(fut.as_mut()).is_pending(),
            "the cancel finished before it could be interrupted"
        );
        tokio::task::yield_now().await;
    }
    drop(fut);

    // Repeating the cancel is the documented recovery: a `200`, not a `409`.
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{booking}"),
        Some(&stu),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "cancelled", "{}", res.body);
    let res = send(&app, "GET", "/meals/balance/me", Some(&stu), None).await;
    assert_eq!(
        res.body["balance_minor"], 0,
        "still billed for a cancelled seat: {}",
        res.body
    );

    // And a third cancel refunds nothing extra — the reversal is keyed, not
    // counted — while a re-book is one fresh charge, not a doubled one.
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{booking}"),
        Some(&stu),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(&app, "GET", "/meals/balance/me", Some(&stu), None).await;
    assert_eq!(res.body["balance_minor"], 0, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&stu),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(&app, "GET", "/meals/balance/me", Some(&stu), None).await;
    assert_eq!(
        res.body["balance_minor"], -3_000,
        "one meal, one charge: {}",
        res.body
    );
}

/// A dish must never outlive its menu — the two orderings that decide it, with
/// no race in either: adding to a menu that is already gone is a `404` that
/// writes nothing, and deleting a menu takes the dishes on it with it.
///
/// This deliberately does **not** race the two requests. It used to (40 rounds,
/// two spawned tasks), and that assertion could not be honest here: the
/// production guard is a store-detected conflict — both transactions write
/// `menu:<id>`'s revision, so one is refused — and the embedded engine behind
/// `init_mem` does not conflict-check concurrent writes to one record at all.
/// It committed **both** and answered `Ok` to each, orphaning 4 of 3600 and 13
/// of 6000 rounds, which surfaced as a whole-suite failure a few percent of
/// runs. The same code orphaned 0 of 9600 rounds against a real server. So the
/// concurrent claim lives where the store can testify about it —
/// `domain::menu::tests::a_child_written_inside_a_delete_never_outlives_the_menu`,
/// `#[ignore]`d and run against a real server — and what stays here is the
/// logic, pinned deterministically.
#[tokio::test]
async fn a_dish_never_lands_on_a_deleted_menu() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "orphan_mgr", "manager").await;
    let dishes_on = async |menu: &str| -> Vec<surrealdb::types::RecordId> {
        let mut res = db
            .query("SELECT VALUE id FROM menu_dish WHERE menu = $menu")
            .bind(("menu", surrealdb::types::RecordId::new("menu", menu)))
            .await
            .unwrap();
        res.take(0).unwrap()
    };
    let publish = async |date: &str| -> String {
        let res = send(
            &app,
            "POST",
            "/meals/menus",
            Some(&mgr),
            Some(json!({ "date": date, "slot": "lunch" })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
        id_of(&res.body)
    };
    let add_dish = async |menu: &str| -> common::Res {
        send(
            &app,
            "POST",
            &format!("/meals/menus/{menu}/dishes"),
            Some(&mgr),
            Some(json!({ "name": "Pilav", "price_minor": 1 })),
        )
        .await
    };

    // Gone first: the add is refused, and refusing must write nothing.
    let menu = publish("2027-10-01").await;
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/menus/{menu}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    let res = add_dish(&menu).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
    assert!(
        dishes_on(&menu).await.is_empty(),
        "a refused add left a dish behind"
    );

    // Dish first: the delete's cascade takes it. Menu ids are deterministic on
    // (date, slot), so a survivor would come back as a dish on the next menu
    // published for that meal — priced by the old one.
    let menu = publish("2027-10-01").await;
    let res = add_dish(&menu).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(dishes_on(&menu).await.len(), 1);
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/menus/{menu}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    assert!(
        dishes_on(&menu).await.is_empty(),
        "the delete's cascade left a dish on a menu that is gone"
    );
}

// --- school payments -----------------------------------------------------

/// Create a fee plan as `cookie` (asserts 201); returns its id. Installments
/// are `(amount_minor, due_at)` pairs, billed in the order given.
async fn create_plan(
    app: &axum::Router,
    cookie: &str,
    name: &str,
    installments: &[(i64, i64)],
) -> String {
    let body = json!({
        "name": name,
        "installments": installments
            .iter()
            .map(|(amount, due)| json!({ "amount_minor": amount, "due_at": due }))
            .collect::<Vec<_>>(),
    });
    let res = send(app, "POST", "/payments/plans", Some(cookie), Some(body)).await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "create plan {name}: {}",
        res.body
    );
    id_of(&res.body)
}

/// Place `students` on `plan` (no assertion — the per-student outcomes are
/// what most of these tests are about).
async fn assign_plan(
    app: &axum::Router,
    cookie: &str,
    plan: &str,
    students: &[&str],
) -> common::Res {
    send(
        app,
        "POST",
        &format!("/payments/plans/{plan}/assignments"),
        Some(cookie),
        Some(json!({ "student_ids": students })),
    )
    .await
}

/// One student's ledger lines, newest first (asserts 200).
async fn ledger_of(app: &axum::Router, cookie: &str, student: &str) -> Vec<serde_json::Value> {
    let res = send(
        app,
        "GET",
        &format!("/payments/ledger/{student}"),
        Some(cookie),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::OK,
        "ledger of {student}: {}",
        res.body
    );
    common::items(&res.body).clone()
}

/// The id of the student's `charge` line worth exactly `amount_minor`.
async fn charge_worth(app: &axum::Router, cookie: &str, student: &str, amount: i64) -> String {
    let lines = ledger_of(app, cookie, student).await;
    let line = lines
        .iter()
        .find(|line| line["kind"] == "charge" && line["amount_minor"] == amount)
        .unwrap_or_else(|| panic!("no charge worth {amount} in {lines:?}"));
    id_of(line)
}

/// Record money in against `charge` (no assertion).
async fn pay(app: &axum::Router, cookie: &str, charge: &str, amount: i64) -> common::Res {
    send(
        app,
        "POST",
        "/payments/credits",
        Some(cookie),
        Some(json!({ "charge_id": charge, "amount_minor": amount })),
    )
    .await
}

/// Hand money back against `credit` (no assertion).
async fn refund(app: &axum::Router, cookie: &str, credit: &str, amount: i64) -> common::Res {
    send(
        app,
        "POST",
        "/payments/refunds",
        Some(cookie),
        Some(json!({ "credit_id": credit, "amount_minor": amount })),
    )
    .await
}

/// Record money in against `charge`, carrying a client idempotence key.
async fn pay_keyed(
    app: &axum::Router,
    cookie: &str,
    charge: &str,
    amount: i64,
    key: &str,
) -> common::Res {
    send(
        app,
        "POST",
        "/payments/credits",
        Some(cookie),
        Some(json!({ "charge_id": charge, "amount_minor": amount, "request_key": key })),
    )
    .await
}

/// Hand money back against `credit`, carrying a client idempotence key.
async fn refund_keyed(
    app: &axum::Router,
    cookie: &str,
    credit: &str,
    amount: i64,
    key: &str,
) -> common::Res {
    send(
        app,
        "POST",
        "/payments/refunds",
        Some(cookie),
        Some(json!({ "credit_id": credit, "amount_minor": amount, "request_key": key })),
    )
    .await
}

/// Undo `line` (no assertion).
async fn reverse(app: &axum::Router, cookie: &str, line: &str) -> common::Res {
    send(
        app,
        "POST",
        "/payments/reversals",
        Some(cookie),
        Some(json!({ "line_id": line })),
    )
    .await
}

/// Row count of `table`, straight from the database. The in-memory engine
/// drops writes under concurrency and still answers `201`, so a response is
/// never proof that a line landed — stored state is.
async fn pay_row_count(db: &hezarfen_backend::database::Database, table: &str) -> usize {
    let mut result = db
        .query(format!("SELECT VALUE id FROM {table}"))
        .await
        .unwrap()
        .check()
        .unwrap();
    result
        .take::<Vec<surrealdb::types::RecordId>>(0)
        .unwrap()
        .len()
}

/// A manager, a student, and a plan the student is on — the starting point of
/// every money test below. Returns `(manager cookie, student id, plan id)`.
async fn billed_student(
    app: &axum::Router,
    db: &hezarfen_backend::database::Database,
    installments: &[(i64, i64)],
) -> (String, String, String) {
    let mgr = login_as(app, db, "bursar", "manager").await;
    let student = login(app, "ali").await;
    let student_id = me_id(app, &student).await;
    let plan = create_plan(app, &mgr, "Yearly", installments).await;
    let res = assign_plan(app, &mgr, &plan, &[&student_id]).await;
    assert_eq!(res.status, StatusCode::OK, "assign: {}", res.body);
    (mgr, student_id, plan)
}

#[tokio::test]
async fn fee_plans_are_manager_written_and_freeze_once_assigned() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "bursar", "manager").await;

    let plan = create_plan(&app, &mgr, "Yearly", &[(10_000, 1_000), (20_000, 2_000)]).await;
    let res = send(
        &app,
        "GET",
        &format!("/payments/plans/{plan}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["name"], "Yearly");
    assert_eq!(res.body["installments"].as_array().unwrap().len(), 2);
    assert_eq!(res.body["installments"][1]["amount_minor"], 20_000);

    // Editing and deleting are both free while nobody is on the plan.
    let res = send(
        &app,
        "PATCH",
        &format!("/payments/plans/{plan}"),
        Some(&mgr),
        Some(json!({ "name": "Yearly (revised)" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["name"], "Yearly (revised)");

    let doomed = create_plan(&app, &mgr, "Scrapped", &[(500, 1_000)]).await;
    let res = send(
        &app,
        "DELETE",
        &format!("/payments/plans/{doomed}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::NO_CONTENT,
        "delete before assignment"
    );

    // One assignment freezes both: the charges are frozen copies, so an edit
    // would only make the plan and the money disagree.
    let student = login(&app, "ali").await;
    let student_id = me_id(&app, &student).await;
    let res = assign_plan(&app, &mgr, &plan, &[&student_id]).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let res = send(
        &app,
        "PATCH",
        &format!("/payments/plans/{plan}"),
        Some(&mgr),
        Some(json!({ "name": "Too late" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/payments/plans/{plan}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    // The refused edit changed nothing.
    let res = send(
        &app,
        "GET",
        &format!("/payments/plans/{plan}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.body["name"], "Yearly (revised)");
}

#[tokio::test]
async fn assigning_bills_every_installment_and_replays_without_billing_twice() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "bursar", "manager").await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;
    let veli = login(&app, "veli").await;
    let veli_id = me_id(&app, &veli).await;

    let plan = create_plan(
        &app,
        &mgr,
        "Yearly",
        &[(10_000, 1_000), (20_000, 2_000), (30_000, 3_000)],
    )
    .await;
    let res = assign_plan(&app, &mgr, &plan, &[&ali_id, &veli_id]).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let outcomes = res.body.as_array().expect("one outcome per student");
    assert_eq!(outcomes.len(), 2);
    assert!(outcomes.iter().all(|o| o["status"] == "assigned"));

    // 2 students × 3 installments, counted in the database — a 200 is not
    // evidence that six lines landed.
    assert_eq!(pay_row_count(&db, "payment_ledger").await, 6);
    assert_eq!(pay_row_count(&db, "fee_plan_assignment").await, 2);

    // The same call again: reported as a replay, and it bills nothing.
    let res = assign_plan(&app, &mgr, &plan, &[&ali_id, &veli_id]).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(
        res.body
            .as_array()
            .unwrap()
            .iter()
            .all(|o| o["status"] == "already_assigned"),
        "{}",
        res.body
    );
    assert_eq!(
        pay_row_count(&db, "payment_ledger").await,
        6,
        "a replayed assignment must not append a second set of charges"
    );
    assert_eq!(pay_row_count(&db, "fee_plan_assignment").await, 2);
}

#[tokio::test]
async fn partial_payments_accumulate_up_to_the_charge_and_no_further() {
    let (app, db) = app_and_db().await;
    let (mgr, student_id, _) = billed_student(&app, &db, &[(100, 1_000)]).await;
    let charge = charge_worth(&app, &mgr, &student_id, 100).await;

    assert_eq!(
        pay(&app, &mgr, &charge, 40).await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        pay(&app, &mgr, &charge, 60).await.status,
        StatusCode::CREATED
    );
    let res = pay(&app, &mgr, &charge, 1).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // The refused payment left no line behind: 1 charge + 2 credits.
    assert_eq!(pay_row_count(&db, "payment_ledger").await, 3);
    let res = send(
        &app,
        "GET",
        &format!("/payments/balance/{student_id}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.body["balance_minor"], 0, "paid in full");
}

#[tokio::test]
async fn a_refund_frees_the_room_it_took_and_a_reversal_takes_it_back() {
    let (app, db) = app_and_db().await;
    let (mgr, student_id, _) = billed_student(&app, &db, &[(100, 1_000)]).await;
    let charge = charge_worth(&app, &mgr, &student_id, 100).await;

    let credit = pay(&app, &mgr, &charge, 100).await;
    assert_eq!(credit.status, StatusCode::CREATED);
    let credit_id = id_of(&credit.body);

    let refunded = refund(&app, &mgr, &credit_id, 100).await;
    assert_eq!(refunded.status, StatusCode::CREATED, "{}", refunded.body);
    let refund_id = id_of(&refunded.body);

    // Money handed back is money owed again, so the charge is payable again.
    let again = pay(&app, &mgr, &charge, 100).await;
    assert_eq!(
        again.status,
        StatusCode::CREATED,
        "a refunded charge must be payable again: {}",
        again.body
    );

    // Undoing the refund says the money never left — the room it freed is
    // taken back, and a further payment is over-paying.
    assert_eq!(
        reverse(&app, &mgr, &refund_id).await.status,
        StatusCode::CREATED
    );
    let res = pay(&app, &mgr, &charge, 1).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
}

#[tokio::test]
async fn reversal_undoes_a_charge_once_and_refuses_a_payment() {
    let (app, db) = app_and_db().await;
    let (mgr, student_id, _) = billed_student(&app, &db, &[(100, 1_000)]).await;
    let charge = charge_worth(&app, &mgr, &student_id, 100).await;

    let first = reverse(&app, &mgr, &charge).await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);
    // Keyed `<line>_r`, so a retry reverses the same line rather than again.
    let replay = reverse(&app, &mgr, &charge).await;
    assert_eq!(replay.status, StatusCode::CREATED, "{}", replay.body);
    assert_eq!(id_of(&replay.body), id_of(&first.body));
    assert_eq!(
        pay_row_count(&db, "payment_ledger").await,
        2,
        "a replayed reversal must not append a second line"
    );

    // A reversed charge is no longer owed, so it takes no money — and the
    // refusal must say *reversed*. The reversal folds in at `+amount` exactly
    // as a payment does, so the cap alone cannot tell the two apart, and
    // "already paid in full" would send a bursar hunting for money that never
    // arrived against a charge nobody ever paid a kuruş on.
    let res = pay(&app, &mgr, &charge, 1).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let message = res.body["error"].as_str().unwrap_or_default();
    assert!(
        message.contains("reversed"),
        "the refusal must name the reversal, not a payment: {message}"
    );
    assert!(
        !message.contains("paid in full"),
        "no money ever arrived on this charge: {message}"
    );
    assert_eq!(
        pay_row_count(&db, "payment_ledger").await,
        2,
        "the refused payment must not have landed"
    );

    // A mistaken payment is corrected with a refund, never a reversal.
    let (mgr2, other_id, _) = {
        let student = login(&app, "veli").await;
        let other_id = me_id(&app, &student).await;
        let plan = create_plan(&app, &mgr, "Second", &[(100, 1_000)]).await;
        assign_plan(&app, &mgr, &plan, &[&other_id]).await;
        (mgr.clone(), other_id, plan)
    };
    let other_charge = charge_worth(&app, &mgr2, &other_id, 100).await;
    let credit = pay(&app, &mgr2, &other_charge, 100).await;
    let credit_id = id_of(&credit.body);
    let res = reverse(&app, &mgr2, &credit_id).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    let refunded = refund(&app, &mgr2, &credit_id, 100).await;
    assert_eq!(refunded.status, StatusCode::CREATED);
    let res = reverse(&app, &mgr2, &id_of(&refunded.body)).await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "a refund may be reversed: {}",
        res.body
    );
}

#[tokio::test]
async fn the_statement_reports_overdue_and_its_rollup_matches_the_balance() {
    let (app, db) = app_and_db().await;
    let future = Timestamp::now().as_millis() + 30 * 24 * 60 * 60 * 1000;
    // One installment already past due, one not.
    let (mgr, student_id, _) = billed_student(&app, &db, &[(100, 1_000), (200, future)]).await;

    let statement = |cookie: String, who: String| {
        let app = app.clone();
        async move {
            let res = send(
                &app,
                "GET",
                &format!("/payments/statement/{who}"),
                Some(&cookie),
                None,
            )
            .await;
            assert_eq!(res.status, StatusCode::OK, "{}", res.body);
            res.body
        }
    };

    let body = statement(mgr.clone(), student_id.clone()).await;
    let row = |body: &serde_json::Value, amount: i64| {
        body["entries"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["amount_minor"] == amount)
            .unwrap_or_else(|| panic!("no statement row worth {amount}"))
            .clone()
    };
    let overdue = row(&body, 100);
    assert_eq!(overdue["plan_name"], "Yearly");
    assert_eq!(overdue["outstanding_minor"], 100);
    assert_eq!(
        overdue["overdue"], true,
        "unpaid and its due date has passed"
    );
    assert_eq!(row(&body, 200)["overdue"], false, "not due yet");
    assert_eq!(body["balance_minor"], -300);

    // Pay the overdue one off, then hand a quarter of it back.
    let charge = charge_worth(&app, &mgr, &student_id, 100).await;
    let credit = pay(&app, &mgr, &charge, 100).await;
    let body = statement(mgr.clone(), student_id.clone()).await;
    let settled = row(&body, 100);
    assert_eq!(settled["credited_minor"], 100);
    assert_eq!(settled["outstanding_minor"], 0);
    assert_eq!(settled["overdue"], false, "settled, however late it was");

    assert_eq!(
        refund(&app, &mgr, &id_of(&credit.body), 25).await.status,
        StatusCode::CREATED
    );
    let body = statement(mgr.clone(), student_id.clone()).await;
    let clawed_back = row(&body, 100);
    assert_eq!(clawed_back["credited_minor"], 100);
    assert_eq!(clawed_back["refunded_minor"], 25);
    // 100 billed - 100 paid + 25 handed back, and owed again means overdue again.
    assert_eq!(clawed_back["outstanding_minor"], 25);
    assert_eq!(clawed_back["overdue"], true);
    assert_eq!(body["balance_minor"], -225);

    // The student's own statement is the same document, and the balance
    // endpoint agrees with the fold behind it.
    let student = send(
        &app,
        "POST",
        "/auth/login",
        None,
        Some(
            json!({ "school": "demo", "school": "demo", "username": "ali", "password": "secret1" }),
        ),
    )
    .await
    .cookie
    .unwrap();
    let own = send(&app, "GET", "/payments/statement/me", Some(&student), None).await;
    assert_eq!(own.status, StatusCode::OK);
    assert_eq!(own.body["balance_minor"], -225);
    assert_eq!(own.body["entries"]["items"].as_array().unwrap().len(), 2);
    let own = send(&app, "GET", "/payments/balance/me", Some(&student), None).await;
    assert_eq!(own.body["balance_minor"], -225);
}

#[tokio::test]
async fn payment_reads_are_self_parent_or_manager_and_writes_are_manager_only() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let parent = login_as(&app, &db, "mom", "parent").await;
    let parent_id = me_id(&app, &parent).await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;
    let veli = login(&app, "veli").await;
    let veli_id = me_id(&app, &veli).await;

    // Mom is tied to Ali only.
    let res = send(
        &app,
        "POST",
        &format!("/users/{parent_id}/students"),
        Some(&admin),
        Some(json!({ "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let plan = create_plan(&app, &admin, "Yearly", &[(100, 1_000)]).await;
    assert_eq!(
        assign_plan(&app, &admin, &plan, &[&ali_id, &veli_id])
            .await
            .status,
        StatusCode::OK
    );
    let charge = charge_worth(&app, &admin, &ali_id, 100).await;

    let reads = |who: &str| {
        [
            format!("/payments/ledger/{who}"),
            format!("/payments/statement/{who}"),
            format!("/payments/balance/{who}"),
        ]
    };
    for uri in reads(&ali_id) {
        // Own record, always.
        assert_eq!(
            send(&app, "GET", &uri, Some(&ali), None).await.status,
            StatusCode::OK,
            "{uri} as the student themselves"
        );
        // A live parent link, and manager+.
        assert_eq!(
            send(&app, "GET", &uri, Some(&parent), None).await.status,
            StatusCode::OK,
            "{uri} as the linked parent"
        );
        assert_eq!(
            send(&app, "GET", &uri, Some(&admin), None).await.status,
            StatusCode::OK,
            "{uri} as an admin"
        );
        // Not a teacher: what a family owes the school is not classroom
        // information, so this is narrower than every other per-student report.
        assert_eq!(
            send(&app, "GET", &uri, Some(&teacher), None).await.status,
            StatusCode::FORBIDDEN,
            "{uri} as a teacher"
        );
    }
    for uri in reads(&veli_id) {
        // Another student's record: not the student, not the unlinked parent.
        assert_eq!(
            send(&app, "GET", &uri, Some(&ali), None).await.status,
            StatusCode::FORBIDDEN,
            "{uri} as another student"
        );
        assert_eq!(
            send(&app, "GET", &uri, Some(&parent), None).await.status,
            StatusCode::FORBIDDEN,
            "{uri} as a parent with no link to them"
        );
    }

    // Writes — and the plan routes, which are money administration end to end.
    for cookie in [&ali, &parent, &teacher] {
        assert_eq!(
            send(&app, "GET", "/payments/plans", Some(cookie), None)
                .await
                .status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            send(
                &app,
                "POST",
                "/payments/plans",
                Some(cookie),
                Some(
                    json!({ "name": "Mine", "installments": [{ "amount_minor": 1, "due_at": 0 }] })
                ),
            )
            .await
            .status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            assign_plan(&app, cookie, &plan, &[&ali_id]).await.status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            pay(&app, cookie, &charge, 1).await.status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            refund(&app, cookie, &charge, 1).await.status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            reverse(&app, cookie, &charge).await.status,
            StatusCode::FORBIDDEN
        );
    }
    // Nothing above landed: one charge per student, and no other line.
    assert_eq!(pay_row_count(&db, "payment_ledger").await, 2);
}

/// A client retry after a network timeout must not charge a family twice. The
/// key is what makes the retry the *same* line, so the interesting case is a
/// payment that filled its charge exactly: the cap, consulted first, would
/// refuse precisely the payment that landed.
#[tokio::test]
async fn a_replayed_request_key_returns_the_same_payment_instead_of_a_second_one() {
    let (app, db) = app_and_db().await;
    let (mgr, student_id, _) =
        billed_student(&app, &db, &[(100, 1_000), (60, 2_000), (30, 3_000)]).await;
    let charge = |amount| {
        let app = app.clone();
        let mgr = mgr.clone();
        let student_id = student_id.clone();
        async move { charge_worth(&app, &mgr, &student_id, amount).await }
    };
    let full = charge(100).await;

    // This payment fills the charge to the penny.
    let first = pay_keyed(&app, &mgr, &full, 100, "receipt-114").await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);
    assert_eq!(pay_row_count(&db, "payment_ledger").await, 4);

    let replay = pay_keyed(&app, &mgr, &full, 100, "receipt-114").await;
    assert_eq!(
        replay.status,
        StatusCode::CREATED,
        "a replay of a cap-filling payment must return the line, not a 409: {}",
        replay.body
    );
    assert_eq!(id_of(&replay.body), id_of(&first.body));
    assert_eq!(
        pay_row_count(&db, "payment_ledger").await,
        4,
        "the replay must not append a second credit"
    );

    // The same key for different money is a client bug, not a replay.
    let mismatch = pay_keyed(&app, &mgr, &full, 50, "receipt-114").await;
    assert_eq!(mismatch.status, StatusCode::CONFLICT, "{}", mismatch.body);
    assert_eq!(pay_row_count(&db, "payment_ledger").await, 4);

    // Two different keys are two payments, and the cap still applies to them.
    let sixty = charge(60).await;
    let a = pay_keyed(&app, &mgr, &sixty, 30, "a").await;
    let b = pay_keyed(&app, &mgr, &sixty, 30, "b").await;
    assert_eq!(a.status, StatusCode::CREATED);
    assert_eq!(b.status, StatusCode::CREATED, "{}", b.body);
    assert_ne!(id_of(&a.body), id_of(&b.body));
    assert_eq!(
        pay_keyed(&app, &mgr, &sixty, 1, "c").await.status,
        StatusCode::CONFLICT,
        "a fresh key does not buy room the charge does not have"
    );
    assert_eq!(pay_row_count(&db, "payment_ledger").await, 6);

    // No key: the old semantics stay exactly as they were — two identical
    // posts are two payments, because a desk taking the same amount twice is.
    let thirty = charge(30).await;
    let one = pay(&app, &mgr, &thirty, 10).await;
    let two = pay(&app, &mgr, &thirty, 10).await;
    assert_eq!(one.status, StatusCode::CREATED);
    assert_eq!(two.status, StatusCode::CREATED);
    assert_ne!(id_of(&one.body), id_of(&two.body));
    assert_eq!(pay_row_count(&db, "payment_ledger").await, 8);
}

#[tokio::test]
async fn a_replayed_request_key_returns_the_same_refund_instead_of_a_second_one() {
    let (app, db) = app_and_db().await;
    let (mgr, student_id, _) = billed_student(&app, &db, &[(100, 1_000)]).await;
    let charge = charge_worth(&app, &mgr, &student_id, 100).await;
    let credit = pay(&app, &mgr, &charge, 100).await;
    let credit_id = id_of(&credit.body);

    let first = refund_keyed(&app, &mgr, &credit_id, 100, "back-1").await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);
    assert_eq!(pay_row_count(&db, "payment_ledger").await, 3);

    // The refund used the credit up, so the cap would refuse a second one —
    // the replay must still answer with the line that exists.
    let replay = refund_keyed(&app, &mgr, &credit_id, 100, "back-1").await;
    assert_eq!(replay.status, StatusCode::CREATED, "{}", replay.body);
    assert_eq!(id_of(&replay.body), id_of(&first.body));
    assert_eq!(pay_row_count(&db, "payment_ledger").await, 3);

    let mismatch = refund_keyed(&app, &mgr, &credit_id, 25, "back-1").await;
    assert_eq!(mismatch.status, StatusCode::CONFLICT, "{}", mismatch.body);
    assert_eq!(pay_row_count(&db, "payment_ledger").await, 3);

    // The balance is what it was after the single refund: money out once.
    let res = send(
        &app,
        "GET",
        &format!("/payments/balance/{student_id}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.body["balance_minor"], -100);
}

/// The statement pages its per-charge rows — but the money it reports is folded
/// from every line, so `balance_minor` must not move between pages.
#[tokio::test]
async fn the_statement_pages_its_rows_without_moving_the_balance() {
    let (app, db) = app_and_db().await;
    let (mgr, student_id, _) =
        billed_student(&app, &db, &[(100, 1_000), (60, 2_000), (30, 3_000)]).await;
    let statement = |query: &str| {
        let app = app.clone();
        let uri = format!("/payments/statement/{student_id}{query}");
        let mgr = mgr.clone();
        async move {
            let res = send(&app, "GET", &uri, Some(&mgr), None).await;
            assert_eq!(res.status, StatusCode::OK, "{}", res.body);
            res.body
        }
    };

    let all = statement("").await;
    assert_eq!(all["entries"]["items"].as_array().unwrap().len(), 3);
    assert_eq!(all["entries"]["total"], 3);
    assert_eq!(all["balance_minor"], -190);

    let head = statement("?limit=2&offset=0").await;
    assert_eq!(head["entries"]["items"].as_array().unwrap().len(), 2);
    assert_eq!(head["entries"]["total"], 3);
    assert_eq!(head["entries"]["limit"], 2);
    assert_eq!(head["entries"]["offset"], 0);
    assert_eq!(head["balance_minor"], -190);

    let tail = statement("?limit=2&offset=2").await;
    assert_eq!(
        tail["entries"]["items"].as_array().unwrap().len(),
        1,
        "the tail returns the remainder"
    );
    assert_eq!(tail["balance_minor"], -190, "every page folds every line");
    // The window is a slice of one order, so the pages do not overlap.
    let page_ids = |body: &serde_json::Value| {
        body["entries"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["charge_id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    let mut seen = page_ids(&head);
    seen.extend(page_ids(&tail));
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 3, "two pages, three distinct charges");

    let res = send(
        &app,
        "GET",
        &format!("/payments/statement/{student_id}?limit=0"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
}

#[tokio::test]
async fn payment_lists_page_through_the_envelope() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "bursar", "manager").await;
    for n in 0..3 {
        create_plan(&app, &mgr, &format!("Plan {n}"), &[(100, 1_000)]).await;
    }
    let plan = create_plan(&app, &mgr, "Shared", &[(100, 1_000), (200, 2_000)]).await;
    let mut students = Vec::new();
    for name in ["ali", "veli", "ayse"] {
        let cookie = login(&app, name).await;
        students.push(me_id(&app, &cookie).await);
    }
    let ids: Vec<&str> = students.iter().map(String::as_str).collect();
    assert_eq!(
        assign_plan(&app, &mgr, &plan, &ids).await.status,
        StatusCode::OK
    );

    let res = send(
        &app,
        "GET",
        "/payments/plans?limit=2&offset=0",
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 4);
    assert_eq!(common::items(&res.body).len(), 2);
    assert_eq!(res.body["limit"], 2);
    assert_eq!(res.body["offset"], 0);

    let res = send(
        &app,
        "GET",
        &format!("/payments/plans/{plan}/assignments?limit=2&offset=2"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 3);
    assert_eq!(
        common::items(&res.body).len(),
        1,
        "tail returns the remainder"
    );

    let student = &students[0];
    let res = send(
        &app,
        "GET",
        &format!("/payments/ledger/{student}?limit=1&offset=0"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 2, "both installments were billed");
    assert_eq!(common::items(&res.body).len(), 1);
    // Unpaged is still the whole list.
    assert_eq!(ledger_of(&app, &mgr, student).await.len(), 2);
}

#[tokio::test]
async fn assignment_rejects_a_non_student_without_billing_anyone() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "bursar", "manager").await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let teacher_id = me_id(&app, &teacher).await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;

    let plan = create_plan(&app, &mgr, "Yearly", &[(100, 1_000)]).await;
    let res = assign_plan(&app, &mgr, &plan, &[&teacher_id, &ali_id]).await;
    assert_eq!(
        res.status,
        StatusCode::OK,
        "one bad id never loses the batch"
    );
    let outcomes = res.body.as_array().unwrap();
    assert_eq!(outcomes[0]["student_id"], teacher_id.as_str());
    assert_eq!(outcomes[0]["status"], "rejected");
    assert_eq!(outcomes[0]["reason"], "no such student");
    assert_eq!(outcomes[1]["status"], "assigned");

    // Only the student was billed, and only the student was placed.
    assert_eq!(pay_row_count(&db, "payment_ledger").await, 1);
    assert_eq!(pay_row_count(&db, "fee_plan_assignment").await, 1);
    assert!(ledger_of(&app, &mgr, &teacher_id).await.is_empty());
}

#[tokio::test]
async fn bulk_assignment_is_capped_at_two_hundred_students() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "bursar", "manager").await;
    let plan = create_plan(&app, &mgr, "Yearly", &[(100, 1_000)]).await;

    let too_many: Vec<String> = (0..MAX_FEE_PLAN_ASSIGN_STUDENTS + 1)
        .map(|n| format!("nobody{n}"))
        .collect();
    let ids: Vec<&str> = too_many.iter().map(String::as_str).collect();
    let res = assign_plan(&app, &mgr, &plan, &ids).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    // Refused whole, not partly: nothing was written before the count check.
    assert_eq!(pay_row_count(&db, "fee_plan_assignment").await, 0);
    assert_eq!(pay_row_count(&db, "payment_ledger").await, 0);
}

// --- whiteboards ---------------------------------------------------------

/// Open a board (asserts 201); returns its id.
async fn create_board(
    app: &axum::Router,
    cookie: &str,
    title: &str,
    participants: &[&str],
) -> String {
    let res = send(
        app,
        "POST",
        "/boards",
        Some(cookie),
        Some(json!({ "title": title, "participants": participants })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "create board: {}",
        res.body
    );
    id_of(&res.body)
}

/// Draw one mark straight through the domain. Drawing is a WebSocket surface
/// (`tests/e2e.rs` owns the room), but every REST read below needs marks on the
/// board first, and the two caps below need one refused at a boundary no HTTP
/// route can reach.
async fn draw(
    db: &Database,
    board: &str,
    author: &str,
    epoch: i64,
    payload: &str,
) -> Result<BoardStroke, hezarfen_backend::error::AppError> {
    BoardStroke::append(
        &BoardId::from_key(board),
        &UserId::from_key(author),
        payload,
        epoch,
        db,
    )
    .await
}

/// The board row as the store holds it — never the response body, which cannot
/// prove a counter moved (src/domain/cap.rs:44-49).
async fn stored_board(db: &Database, board: &str) -> Option<Board> {
    Board::read(&BoardId::from_key(board), db).await.unwrap()
}

/// Every stroke row of a board, oldest first, straight out of the table.
/// `SELECT *`, because SurrealDB 3 refuses `ORDER BY id` on a projection that
/// omits `id`.
async fn stroke_rows(db: &Database, board: &str) -> Vec<BoardStroke> {
    let mut result = db
        .query("SELECT * FROM board_stroke WHERE board = $b ORDER BY id")
        .bind(("b", BoardId::from_key(board).record()))
        .await
        .unwrap()
        .check()
        .unwrap();
    result.take(0).unwrap()
}

/// The stored `[epoch_stroke_count, total_stroke_count]` pair.
async fn stored_counters(db: &Database, board: &str) -> Vec<i64> {
    let mut result = db
        .query(
            "SELECT VALUE [epoch_stroke_count ?? 0, total_stroke_count ?? 0] \
             FROM $id",
        )
        .bind(("id", BoardId::from_key(board).record()))
        .await
        .unwrap()
        .check()
        .unwrap();
    result.take::<Vec<Vec<i64>>>(0).unwrap().remove(0)
}

async fn set_counters(db: &Database, board: &str, epoch: i64, total: i64) {
    db.query("UPDATE $id SET epoch_stroke_count = $e, total_stroke_count = $t")
        .bind(("id", BoardId::from_key(board).record()))
        .bind(("e", epoch))
        .bind(("t", total))
        .await
        .unwrap()
        .check()
        .unwrap();
}

/// The creator's stored `board_count` — the authority on how many boards they
/// hold, so the per-creator cap is asserted here and not off a 409.
async fn stored_board_count(db: &Database, user: &str) -> i64 {
    let mut result = db
        .query("SELECT VALUE board_count ?? 0 FROM $id")
        .bind(("id", UserId::from_key(user).record()))
        .await
        .unwrap()
        .check()
        .unwrap();
    result.take::<Vec<i64>>(0).unwrap()[0]
}

/// Every id-scoped board route, with a body where the route needs one. The
/// `PATCH` arms are split because they carry different rights: `title` is every
/// participant's, `participants` and `locked` are the creator's alone.
fn board_routes(id: &str) -> Vec<(&'static str, String, Option<serde_json::Value>)> {
    vec![
        ("GET", format!("/boards/{id}"), None),
        ("GET", format!("/boards/{id}/strokes"), None),
        ("GET", format!("/boards/{id}/history"), None),
        ("GET", format!("/boards/{id}/epochs"), None),
        (
            "PATCH",
            format!("/boards/{id}"),
            Some(json!({"title": "x"})),
        ),
        (
            "PATCH",
            format!("/boards/{id}"),
            Some(json!({"participants": []})),
        ),
        (
            "PATCH",
            format!("/boards/{id}"),
            Some(json!({"locked": true})),
        ),
        ("POST", format!("/boards/{id}/clear"), None),
        ("POST", format!("/boards/{id}/close"), None),
        ("DELETE", format!("/boards/{id}"), None),
    ]
}

/// A board that exists must be indistinguishable from one that does not, on
/// every single route — a 403 anywhere here would tell an outsider the school
/// holds a board with that id, and the id list is guessable from nothing else.
#[tokio::test]
async fn every_board_route_is_a_404_for_an_outsider() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;
    let veli = login(&app, "veli").await;
    let zeynep = login(&app, "zeynep").await;
    let veli_id = me_id(&app, &veli).await;
    let board = create_board(&app, &ali, "Geometri", &[&veli_id]).await;
    draw(&db, &board, &me_id(&app, &ali).await, 0, "{\"p\":[1,2]}")
        .await
        .unwrap();

    // The real board and an id that was never minted answer identically.
    let ghost = board_routes("nosuchboard");
    for (n, (method, uri, body)) in board_routes(&board).into_iter().enumerate() {
        let res = send(&app, method, &uri, Some(&zeynep), body).await;
        assert_eq!(
            res.status,
            StatusCode::NOT_FOUND,
            "{method} {uri} must 404 for an outsider, got {}",
            res.body
        );
        let (gm, gu, gb) = ghost[n].clone();
        let gone = send(&app, gm, &gu, Some(&zeynep), gb).await;
        assert_eq!(
            res.status, gone.status,
            "{method} {uri} must answer exactly like a board that does not exist"
        );
        assert_eq!(res.body, gone.body, "{method} {uri} bodies must match too");
    }

    // The list route leaks nothing either, and none of the refusals above
    // touched the row: still open, still one stroke, still the same roster.
    let res = send(&app, "GET", "/boards", Some(&zeynep), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(common::total(&res.body), 0, "an outsider lists no boards");
    let stored = stored_board(&db, &board).await.expect("board survived");
    assert_eq!(stored.get_title().as_str(), "Geometri");
    assert_eq!(stored.get_epoch(), 0);
    assert!(!stored.is_locked());
    assert!(stored.get_closed_at().is_none());
    assert_eq!(stored.get_participants().len(), 1);
    assert_eq!(stroke_rows(&db, &board).await.len(), 1);

    // Opening a board is nobody's privilege — the outsider gets their own.
    let mine = create_board(&app, &zeynep, "Kendi tahtam", &[]).await;
    assert_ne!(mine, board);
}

/// The `parent` role has no whiteboard at all. Three cuts in one test, because
/// they only work together: a parent cannot open a board, cannot be *named* on
/// one, and — the stale-row case, forced here by writing a parent straight into
/// a roster — still gets the outsider's `404` rather than a `403` that would
/// confirm the board is there.
#[tokio::test]
async fn a_parent_gets_no_whiteboard_at_all() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;
    let anne = login_as(&app, &db, "anne", "parent").await;
    let anne_id = me_id(&app, &anne).await;

    // 1. A parent cannot open one. A 403, not a 404: no board exists to hide.
    let res = send(
        &app,
        "POST",
        "/boards",
        Some(&anne),
        Some(json!({ "title": "Gizli", "participants": [] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    // And nothing was written on the way to that refusal.
    assert_eq!(stored_board_count(&db, &anne_id).await, 0);

    // 2. A parent cannot be named on one either — refused whole, so the board
    //    is not created half-invited.
    let res = send(
        &app,
        "POST",
        "/boards",
        Some(&ali),
        Some(json!({ "title": "Geometri", "participants": [&anne_id] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    assert_eq!(stored_board_count(&db, &me_id(&app, &ali).await).await, 0);

    // The same refusal on the way in through PATCH, on a board that does exist.
    let board = create_board(&app, &ali, "Geometri", &[]).await;
    let res = send(
        &app,
        "PATCH",
        &format!("/boards/{board}"),
        Some(&ali),
        Some(json!({ "participants": [&anne_id] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    assert!(
        stored_board(&db, &board)
            .await
            .unwrap()
            .get_participants()
            .is_empty(),
        "a refused roster change must leave the old roster"
    );

    // 3. The stale row: a parent already on a roster, written before this rule.
    //    Every id-scoped route must still answer exactly like a board that was
    //    never minted — a 403 anywhere here leaks the board's existence.
    db.query("UPDATE $b SET participants = [$u]")
        .bind(("b", BoardId::from_key(&board).record()))
        .bind(("u", UserId::from_key(&anne_id).record()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let ghost = board_routes("nosuchboard");
    for (n, (method, uri, body)) in board_routes(&board).into_iter().enumerate() {
        let res = send(&app, method, &uri, Some(&anne), body).await;
        assert_eq!(
            res.status,
            StatusCode::NOT_FOUND,
            "{method} {uri} must 404 for a parent, got {}",
            res.body
        );
        let (gm, gu, gb) = ghost[n].clone();
        let gone = send(&app, gm, &gu, Some(&anne), gb).await;
        assert_eq!(res.status, gone.status, "{method} {uri}");
        assert_eq!(res.body, gone.body, "{method} {uri} bodies must match too");
    }
    // The list route is barred outright — nothing to hide, so a 403 is honest.
    let res = send(&app, "GET", "/boards", Some(&anne), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    // None of it disturbed the board.
    assert!(stored_board(&db, &board).await.is_some());
}

/// The read-modify-write half of the same defect. A roster the creator was just
/// served carries a participant who has since stopped qualifying, and echoing
/// it back used to be a `400` — the invite list frozen by a value the server
/// itself handed over. It is dropped now, silently. What must NOT be dropped is
/// a *newly named* unqualified id: that split is the whole of the fix, and
/// backwards it would let any caller seed a roster with anyone.
#[tokio::test]
async fn a_creator_can_patch_back_a_roster_holding_a_participant_who_no_longer_qualifies() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;
    let veli = login(&app, "veli").await;
    let veli_id = me_id(&app, &veli).await;
    let anne = login_as(&app, &db, "anne", "parent").await;
    let anne_id = me_id(&app, &anne).await;

    let board = create_board(&app, &ali, "Geometri", &[&veli_id]).await;
    // Demoted straight in the store: `set_role` would sweep the roster, and the
    // stale row is exactly the shape a volume written before that sweep holds.
    db.query("UPDATE $u SET role = 'parent'")
        .bind(("u", UserId::from_key(&veli_id).record()))
        .await
        .unwrap()
        .check()
        .unwrap();

    // What the client sees, and sends straight back.
    let read = send(&app, "GET", &format!("/boards/{board}"), Some(&ali), None).await;
    assert_eq!(read.status, StatusCode::OK, "{}", read.body);
    let roster = read.body["participants"].clone();
    assert_eq!(
        roster,
        json!([&veli_id]),
        "the stale id is served, not hidden"
    );

    let res = send(
        &app,
        "PATCH",
        &format!("/boards/{board}"),
        Some(&ali),
        Some(json!({ "participants": roster })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["participants"], json!([]), "{}", res.body);
    assert!(
        stored_board(&db, &board)
            .await
            .unwrap()
            .get_participants()
            .is_empty(),
        "the drop is stored, not just rendered"
    );

    // The other half: a fresh unqualified id still fails the whole call, and
    // both flavours of it — a parent, and an id no user answers to.
    for id in [anne_id.as_str(), "nosuchuser"] {
        let res = send(
            &app,
            "PATCH",
            &format!("/boards/{board}"),
            Some(&ali),
            Some(json!({ "participants": [id] })),
        )
        .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{id}: {}", res.body);
    }
    assert!(
        stored_board(&db, &board)
            .await
            .unwrap()
            .get_participants()
            .is_empty()
    );
}

/// The roster is spelled `participants` on the way in, out and through `PATCH`.
/// It used to be `participant_ids` on `POST` alone, and with serde ignoring the
/// unknown key a client that posted back a board it had just read got a `201`
/// for a board it was silently alone on. A misspelled roster must now be
/// refused, not dropped — the assert is the *stored* row, because the response
/// echoing an empty list is exactly what the old bug looked like.
#[tokio::test]
async fn a_misspelled_roster_is_refused_instead_of_silently_dropped() {
    let (app, db) = common::app_and_db().await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;
    let veli = login(&app, "veli").await;
    let veli_id = me_id(&app, &veli).await;

    let res = send(
        &app,
        "POST",
        "/boards",
        Some(&ali),
        Some(json!({ "title": "Geometri", "participant_ids": [&veli_id] })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the old spelling must not open a board: {}",
        res.body
    );
    // Refused whole: no solo board, and no seat spent on one.
    assert_eq!(stored_board_count(&db, &ali_id).await, 0);

    let board = create_board(&app, &ali, "Geometri", &[&veli_id]).await;
    let stored = stored_board(&db, &board).await.unwrap();
    assert_eq!(
        stored
            .get_participants()
            .iter()
            .map(|user| user.key().to_string())
            .collect::<Vec<_>>(),
        vec![veli_id],
        "the roster must reach the row, not just the response"
    );
}

/// The other half of the two-tier line: a participant sees the board and may
/// re-title it, but the four commands and the roster/lock arms of `PATCH` are
/// the creator's alone — a 403, because hiding a board they are already
/// rendering would be a lie their client cannot act on.
#[tokio::test]
async fn a_participant_reads_and_retitles_but_cannot_command() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;
    let veli = login(&app, "veli").await;
    let ali_id = me_id(&app, &ali).await;
    let veli_id = me_id(&app, &veli).await;
    let board = create_board(&app, &ali, "Geometri", &[&veli_id]).await;
    draw(&db, &board, &veli_id, 0, "{\"p\":[1,2]}")
        .await
        .unwrap();

    // Every read, and the title arm: the participant's.
    for uri in [
        format!("/boards/{board}"),
        format!("/boards/{board}/strokes"),
        format!("/boards/{board}/history"),
        format!("/boards/{board}/epochs"),
        "/boards".to_string(),
    ] {
        let res = send(&app, "GET", &uri, Some(&veli), None).await;
        assert_eq!(res.status, StatusCode::OK, "GET {uri}: {}", res.body);
    }
    let res = send(&app, "GET", "/boards", Some(&veli), None).await;
    assert_eq!(common::total(&res.body), 1, "an invitee lists the board");

    let res = send(
        &app,
        "PATCH",
        &format!("/boards/{board}"),
        Some(&veli),
        Some(json!({ "title": "Cebir" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        stored_board(&db, &board)
            .await
            .unwrap()
            .get_title()
            .as_str(),
        "Cebir",
        "the re-title really landed"
    );

    // The creator's four commands, and the two creator-only PATCH arms.
    for (method, uri, body) in [
        (
            "PATCH",
            format!("/boards/{board}"),
            Some(json!({"participants": []})),
        ),
        (
            "PATCH",
            format!("/boards/{board}"),
            Some(json!({"locked": true})),
        ),
        ("POST", format!("/boards/{board}/clear"), None),
        ("POST", format!("/boards/{board}/close"), None),
        ("DELETE", format!("/boards/{board}"), None),
    ] {
        let res = send(&app, method, &uri, Some(&veli), body).await;
        assert_eq!(
            res.status,
            StatusCode::FORBIDDEN,
            "{method} {uri} must 403 for a non-creator participant, got {}",
            res.body
        );
    }

    // Not one of those refusals wrote anything.
    let stored = stored_board(&db, &board).await.expect("board survived");
    assert_eq!(stored.get_participants().len(), 1, "roster untouched");
    assert!(!stored.is_locked(), "lock untouched");
    assert_eq!(stored.get_epoch(), 0, "no clear landed");
    assert!(stored.get_closed_at().is_none(), "not closed");
    assert_eq!(stroke_rows(&db, &board).await.len(), 1, "stroke kept");
    assert_eq!(stored.get_creator().key(), ali_id);

    // And the creator's own PATCH arms do work — the 403s above are about the
    // caller, not a route that refuses everyone.
    let res = send(
        &app,
        "PATCH",
        &format!("/boards/{board}"),
        Some(&ali),
        Some(json!({ "locked": true })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(stored_board(&db, &board).await.unwrap().is_locked());
}

/// The user's core requirement, over HTTP: a clear empties the live canvas and
/// destroys nothing. If `/history` ever shrinks here, the feature is wrong.
#[tokio::test]
async fn a_clear_empties_the_canvas_and_keeps_the_history() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;
    let board = create_board(&app, &ali, "Geometri", &[]).await;
    for n in 0..3 {
        draw(&db, &board, &ali_id, 0, &format!("{{\"p\":[{n}]}}"))
            .await
            .unwrap();
    }
    let before: Vec<String> = stroke_rows(&db, &board)
        .await
        .iter()
        .map(|row| row.get_id().key().to_string())
        .collect();
    assert_eq!(before.len(), 3);

    let res = send(
        &app,
        "GET",
        &format!("/boards/{board}/strokes"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 3, "the live canvas has 3 marks");

    let res = send(
        &app,
        "POST",
        &format!("/boards/{board}/clear"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["kind"], "clear");
    assert_eq!(
        res.body["epoch"], 0,
        "the marker carries the epoch it closed"
    );
    assert_eq!(res.body["count"], 3);
    assert!(res.body["payload"].is_null());

    // The canvas is blank...
    let res = send(
        &app,
        "GET",
        &format!("/boards/{board}/strokes"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 0, "the canvas is empty");
    assert!(common::items(&res.body).is_empty());

    // ...and the history is not. 3 strokes + the marker.
    let res = send(
        &app,
        "GET",
        &format!("/boards/{board}/history"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 4);
    let kept: Vec<&str> = common::items(&res.body)
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    for id in &before {
        assert!(kept.contains(&id.as_str()), "a clear lost stroke {id}");
    }

    // Asserted at the table too, not just through the reader that a bug could
    // share with the writer.
    let rows = stroke_rows(&db, &board).await;
    assert_eq!(rows.len(), 4, "nothing was deleted");
    let stored: Vec<String> = rows.iter().map(|r| r.get_id().key().to_string()).collect();
    for id in &before {
        assert!(stored.contains(id), "stroke {id} is gone from the table");
    }

    // Drawing resumes on the new epoch, and the old one is still readable
    // whole through `?epoch=`.
    let board_row = stored_board(&db, &board).await.unwrap();
    assert_eq!(board_row.get_epoch(), 1);
    draw(&db, &board, &ali_id, 1, "{\"p\":[9]}").await.unwrap();
    let res = send(
        &app,
        "GET",
        &format!("/boards/{board}/strokes"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 1, "the new canvas has one mark");
    let res = send(
        &app,
        "GET",
        &format!("/boards/{board}/history?epoch=0"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 4, "epoch 0 replays in full");
    let res = send(
        &app,
        "GET",
        &format!("/boards/{board}/history"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 5, "the whole session is one log");
}

/// The epoch index: one entry per clear, each carrying the final stroke count
/// of the epoch it closed — which must equal the rows actually on the table for
/// that epoch, or "replay session 2" replays the wrong thing.
#[tokio::test]
async fn the_epoch_index_counts_each_epoch_it_closed() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;
    let board = create_board(&app, &ali, "Geometri", &[]).await;

    // Epoch 0 gets 2 marks, epoch 1 gets 3, then epoch 2 is left open.
    for (epoch, marks) in [(0, 2), (1, 3)] {
        for n in 0..marks {
            draw(&db, &board, &ali_id, epoch, &format!("{{\"p\":[{n}]}}"))
                .await
                .unwrap();
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
    }
    draw(&db, &board, &ali_id, 2, "{\"p\":[7]}").await.unwrap();

    let res = send(
        &app,
        "GET",
        &format!("/boards/{board}/epochs"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(common::total(&res.body), 2, "one marker per clear, no more");
    let markers = common::items(&res.body);
    assert_eq!(markers[0]["epoch"], 0);
    assert_eq!(markers[0]["count"], 2);
    assert_eq!(markers[1]["epoch"], 1);
    assert_eq!(markers[1]["count"], 3);
    for marker in markers {
        assert_eq!(marker["kind"], "clear");
        assert_eq!(marker["author"], ali_id.as_str());
    }

    // Each `count` against the rows really stored under that epoch.
    let rows = stroke_rows(&db, &board).await;
    for marker in markers {
        let epoch = marker["epoch"].as_i64().unwrap();
        let drawn = rows
            .iter()
            .filter(|row| row.get_epoch() == epoch && !row.is_clear())
            .count() as i64;
        assert_eq!(
            marker["count"].as_i64().unwrap(),
            drawn,
            "marker for epoch {epoch} miscounts its strokes"
        );
    }
    // The open epoch is deliberately absent — the markers *are* the index.
    assert_eq!(stored_board(&db, &board).await.unwrap().get_epoch(), 2);
}

/// A full live canvas is a *recoverable* refusal: the board stays open and a
/// clear hands the whole cap back.
#[tokio::test]
async fn a_full_canvas_is_refused_until_the_creator_clears_it() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;
    let board = create_board(&app, &ali, "Geometri", &[]).await;
    set_counters(&db, &board, MAX_EPOCH_STROKES, MAX_EPOCH_STROKES).await;

    let refused = draw(&db, &board, &ali_id, 0, "{\"p\":[1]}").await;
    assert!(refused.is_err(), "a full canvas must refuse the append");
    assert!(stroke_rows(&db, &board).await.is_empty(), "nothing written");
    let stored = stored_board(&db, &board).await.unwrap();
    assert!(
        stored.get_closed_at().is_none(),
        "a full canvas must NOT close the board — it is recoverable"
    );
    assert_eq!(
        stored_counters(&db, &board).await,
        vec![MAX_EPOCH_STROKES, MAX_EPOCH_STROKES],
        "the refusal claimed nothing"
    );

    // The creator clears, and drawing resumes against a fresh cap.
    let res = send(
        &app,
        "POST",
        &format!("/boards/{board}/clear"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["count"], MAX_EPOCH_STROKES);
    assert!(
        draw(&db, &board, &ali_id, 1, "{\"p\":[1]}").await.is_ok(),
        "a clear must un-refuse the board"
    );
    assert_eq!(
        stored_counters(&db, &board).await,
        vec![1, MAX_EPOCH_STROKES + 2],
        "the epoch counter reset; the lifetime counter kept the clear marker \
         it minted and the new stroke"
    );
    let res = send(
        &app,
        "GET",
        &format!("/boards/{board}/strokes"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 1, "the new mark is on the canvas");
}

/// The lifetime cap is the permanent one: it stamps `closed_at`, and a closed
/// board still serves every read it ever served.
#[tokio::test]
async fn the_lifetime_cap_closes_the_board_and_it_stays_readable() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;
    let board = create_board(&app, &ali, "Geometri", &[]).await;
    draw(&db, &board, &ali_id, 0, "{\"p\":[1]}").await.unwrap();
    let res = send(
        &app,
        "POST",
        &format!("/boards/{board}/clear"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    set_counters(&db, &board, 0, MAX_BOARD_STROKES).await;

    assert!(
        draw(&db, &board, &ali_id, 1, "{\"p\":[2]}").await.is_err(),
        "the lifetime cap must refuse the append"
    );
    let stamp = stored_board(&db, &board)
        .await
        .unwrap()
        .get_closed_at()
        .expect("the lifetime cap stamps closed_at");
    assert_eq!(stroke_rows(&db, &board).await.len(), 2, "nothing written");

    // Permanently read-only: no more strokes, and no more clears either.
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    assert!(draw(&db, &board, &ali_id, 1, "{\"p\":[3]}").await.is_err());
    assert_eq!(
        stored_board(&db, &board).await.unwrap().get_closed_at(),
        Some(stamp),
        "a second refusal must not re-stamp closed_at"
    );
    let res = send(
        &app,
        "POST",
        &format!("/boards/{board}/clear"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // Still fully readable — that is the whole point of closing rather than
    // deleting.
    let res = send(&app, "GET", &format!("/boards/{board}"), Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["closed_at"], stamp.as_millis());
    let res = send(
        &app,
        "GET",
        &format!("/boards/{board}/history"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 2, "the whole log is still served");
    let res = send(
        &app,
        "GET",
        &format!("/boards/{board}/epochs"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 1);
    let res = send(
        &app,
        "GET",
        &format!("/boards/{board}/strokes"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let res = send(&app, "GET", "/boards", Some(&ali), None).await;
    assert_eq!(
        common::total(&res.body),
        1,
        "a closed board is still listed"
    );
}

/// Demoting a board's **creator** to `parent` closes the room, and closes it
/// through the ordinary role route. Without that the board is commandable by
/// nobody: the ex-creator is 404'd off their own board (the whiteboard is shut
/// to parents), clear/lock/close/delete are creator-only for everyone else, and
/// no route lists a board the caller is not on — so not even an admin can find
/// the id, while the participants keep drawing on it. Closed and not deleted:
/// every read the board ever served, it still serves.
#[tokio::test]
async fn demoting_a_board_s_creator_closes_the_room_and_keeps_it_readable() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "yonetici", "admin").await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;
    let veli = login(&app, "veli").await;
    let veli_id = me_id(&app, &veli).await;
    let board = create_board(&app, &ali, "Geometri", &[&veli_id]).await;
    draw(&db, &board, &veli_id, 0, "{\"p\":[1]}").await.unwrap();

    let res = send(
        &app,
        "PATCH",
        &format!("/users/{ali_id}/role"),
        Some(&admin),
        Some(json!({ "role": "parent" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let stamp = stored_board(&db, &board)
        .await
        .unwrap()
        .get_closed_at()
        .expect("the demotion must close the room its creator can no longer command");
    // The seat stays taken: the row it counts still exists.
    assert_eq!(stored_board_count(&db, &ali_id).await, 1);

    // The participant still reads everything, and their marks are still there.
    let res = send(&app, "GET", &format!("/boards/{board}"), Some(&veli), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["closed_at"], stamp.as_millis());
    let res = send(
        &app,
        "GET",
        &format!("/boards/{board}/history"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 1, "the whole log is still served");
    assert_eq!(stroke_rows(&db, &board).await.len(), 1);
    let res = send(&app, "GET", "/boards", Some(&veli), None).await;
    assert_eq!(common::total(&res.body), 1, "and it is still listed");

    // But every write answers the terminal refusal, not the recoverable one.
    let refused = draw(&db, &board, &veli_id, 0, "{\"p\":[2]}").await;
    assert!(
        matches!(
            refused,
            Err(hezarfen_backend::error::AppError::Conflict(
                hezarfen_backend::domain::board_stroke::BOARD_CLOSED
            ))
        ),
        "a closed board must refuse a stroke as closed, got {refused:?}"
    );
    assert_eq!(stroke_rows(&db, &board).await.len(), 1, "nothing written");

    // The ex-creator is out of their own room, which is the shape that made
    // this uncommandable in the first place.
    let res = send(&app, "GET", &format!("/boards/{board}"), Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
}

/// The per-creator cap, asserted on the stored counter — and a delete really
/// hands the seat back, or a busy teacher's limit ratchets shut forever.
#[tokio::test]
async fn the_per_creator_cap_refuses_and_a_delete_frees_a_seat() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;
    let board = create_board(&app, &ali, "Geometri", &[]).await;
    assert_eq!(stored_board_count(&db, &ali_id).await, 1);

    // Age the counter to full rather than open 200 boards.
    db.query("UPDATE $id SET board_count = $full")
        .bind(("id", UserId::from_key(&ali_id).record()))
        .bind(("full", MAX_BOARDS_PER_CREATOR))
        .await
        .unwrap()
        .check()
        .unwrap();

    let res = send(
        &app,
        "POST",
        "/boards",
        Some(&ali),
        Some(json!({ "title": "Bir tane daha" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert_eq!(
        stored_board_count(&db, &ali_id).await,
        MAX_BOARDS_PER_CREATOR,
        "the refusal must not have claimed a seat"
    );
    let mut result = db
        .query("SELECT VALUE id FROM board")
        .await
        .unwrap()
        .check()
        .unwrap();
    assert_eq!(
        result
            .take::<Vec<surrealdb::types::RecordId>>(0)
            .unwrap()
            .len(),
        1,
        "no row was written by the refused create"
    );

    // Deleting frees exactly one seat, and the next create takes it.
    let res = send(
        &app,
        "DELETE",
        &format!("/boards/{board}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);
    assert_eq!(
        stored_board_count(&db, &ali_id).await,
        MAX_BOARDS_PER_CREATOR - 1,
        "the delete handed the seat back"
    );
    let next = create_board(&app, &ali, "Bir tane daha", &[]).await;
    assert_eq!(
        stored_board_count(&db, &ali_id).await,
        MAX_BOARDS_PER_CREATOR
    );
    assert!(stored_board(&db, &next).await.is_some());
    // And the deleted board is gone for its own creator too.
    let res = send(&app, "GET", &format!("/boards/{board}"), Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND);
}

/// The `next_ulid` hazard, over HTTP: a burst of marks all lands inside one or
/// two milliseconds, and `Ulid::new()` would sort those rows at random
/// (src/domain/monotonic_id.rs:41-46). The canvas is a drawing, so an order
/// that shuffles is a drawing that redraws wrong.
#[tokio::test]
async fn a_burst_of_strokes_comes_back_in_mint_order() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;
    let board = create_board(&app, &ali, "Geometri", &[]).await;

    let mut minted = Vec::new();
    for n in 0..60 {
        minted.push(
            draw(&db, &board, &ali_id, 0, &format!("{{\"p\":[{n}]}}"))
                .await
                .unwrap()
                .get_id()
                .key()
                .to_string(),
        );
    }
    // Whether *those* 60 shared a millisecond depends on how loaded the
    // machine is, so the same-millisecond half of the hazard is pinned with no
    // database in the way: a tight mint loop that certainly does share one, and
    // `ORDER BY id` on a string key is a plain string sort — so mint order must
    // already BE sorted order. `Ulid::new()` fails this within a few ids.
    let ids: Vec<String> = (0..500)
        .map(|_| BoardStrokeId::generate().key().to_string())
        .collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(
        sorted, ids,
        "ids minted in one millisecond must sort in mint order"
    );

    let res = send(
        &app,
        "GET",
        &format!("/boards/{board}/strokes"),
        Some(&ali),
        None,
    )
    .await;
    let served: Vec<&str> = common::items(&res.body)
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    assert_eq!(served, minted, "/strokes must replay in mint order");
    let res = send(
        &app,
        "GET",
        &format!("/boards/{board}/history"),
        Some(&ali),
        None,
    )
    .await;
    let served: Vec<&str> = common::items(&res.body)
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    assert_eq!(served, minted, "/history must replay in mint order too");
    // And the payloads ride along in that same order.
    let payloads: Vec<String> = common::items(&res.body)
        .iter()
        .map(|row| row["payload"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(payloads[0], "{\"p\":[0]}");
    assert_eq!(payloads[59], "{\"p\":[59]}");
}

// --- class sections (şube) --------------------------------------------------

/// The class routes' own gates, end to end: who may write, who may not, and
/// that the writes really are enrollments. The pump itself is proven in the
/// domain unit tests — what only the router can prove is that the two axes are
/// gated differently (roster = manager+, attach = *that course's* manager) and
/// that a class is enrollment in bulk rather than a parallel roster.
#[tokio::test]
async fn a_class_pumps_enrollments_and_guards_each_axis_separately() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let teacher = login_as(&app, &db, "tch", "teacher").await;
    let other = login_as(&app, &db, "other", "teacher").await;
    let student = login_as(&app, &db, "stu", "student").await;
    let student_id = me_id(&app, &student).await;
    let teacher_id = me_id(&app, &teacher).await;
    let course = create_course(&app, &teacher, "algebra").await;

    // Creating a class is the office's call — a teacher may read, not write.
    let res = send(
        &app,
        "POST",
        "/classes",
        Some(&teacher),
        Some(json!({"name": "9-A"})),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "class create is manager+"
    );
    let res = send(
        &app,
        "POST",
        "/classes",
        Some(&manager),
        Some(json!({"name": "9-A", "grade": ""})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    assert!(
        res.body["class"]["grade"].is_null(),
        "an empty grade is no grade"
    );
    let class = id_of(&res.body["class"]);
    assert_eq!(
        send(&app, "GET", "/classes", Some(&teacher), None)
            .await
            .status,
        StatusCode::OK,
        "teachers read the class list"
    );

    // Only students go in a roster, and only real users.
    for (body, who) in [
        (json!({"user_id": "nobody"}), "an unknown user"),
        (json!({"user_id": teacher_id}), "a teacher"),
    ] {
        let res = send(
            &app,
            "POST",
            &format!("/classes/{class}/members"),
            Some(&manager),
            Some(body),
        )
        .await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{who} is a 400");
    }
    let res = send(
        &app,
        "POST",
        &format!("/classes/{class}/members"),
        Some(&manager),
        Some(json!({ "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);

    // Attaching writes the course's roster, so it takes the course's own bar —
    // a teacher who does not manage it is refused even though they are teacher+.
    let attach = json!({ "course_id": course });
    let res = send(
        &app,
        "POST",
        &format!("/classes/{class}/courses"),
        Some(&other),
        Some(attach.clone()),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "attach needs course rights"
    );
    let res = send(
        &app,
        "POST",
        &format!("/classes/{class}/courses"),
        Some(&teacher),
        Some(attach.clone()),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let res = send(
        &app,
        "POST",
        &format!("/classes/{class}/courses"),
        Some(&teacher),
        Some(attach),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "a second attach is a 409");

    // The membership is a real enrollment: it shows up on the course roster.
    let res = send(
        &app,
        "GET",
        &format!("/courses/{course}/enrollments"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(
        common::total(&res.body),
        1,
        "the class must have enrolled its member"
    );
    assert_eq!(common::items(&res.body)[0]["user"]["id"], student_id);

    // A class carrying either is undeletable until both ends are cleared.
    let res = send(
        &app,
        "DELETE",
        &format!("/classes/{class}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT);
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/classes/{class}/courses/{course}"),
            Some(&manager),
            None,
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/classes/{class}/members/{student_id}"),
            Some(&manager),
            None,
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    let res = send(
        &app,
        "DELETE",
        &format!("/classes/{class}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::NO_CONTENT,
        "empty on both axes, it goes"
    );
    // And the pumped enrollment left with the detach.
    let res = send(
        &app,
        "GET",
        &format!("/courses/{course}/enrollments"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 0);
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/classes/{class}"),
            Some(&teacher),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

/// Who may look at a class, and the one write a manager holds without teaching
/// anything. Every `/classes` read is teacher+, so the student on the roster —
/// and the parent watching them — is refused their own row; and attaching takes
/// management of the course, which a manager has over every course in the
/// school whether or not they run it.
#[tokio::test]
async fn class_reads_are_staff_only_and_a_manager_manages_every_course() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let teacher = login_as(&app, &db, "tch", "teacher").await;
    let parent = login_as(&app, &db, "anne", "parent").await;
    let student = login(&app, "stu").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "algebra").await;

    let res = send(
        &app,
        "POST",
        "/classes",
        Some(&manager),
        Some(json!({ "name": "9-A" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let class = id_of(&res.body["class"]);
    // The student really is on this roster, so the 403s below are the gate
    // talking and not an empty database.
    let res = send(
        &app,
        "POST",
        &format!("/classes/{class}/members"),
        Some(&manager),
        Some(json!({ "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    for (who, cookie) in [("a student", &student), ("a parent", &parent)] {
        for uri in [
            "/classes".to_string(),
            format!("/classes/{class}"),
            format!("/classes/{class}/members"),
            format!("/classes/{class}/courses"),
        ] {
            let res = send(&app, "GET", &uri, Some(cookie), None).await;
            assert_eq!(res.status, StatusCode::FORBIDDEN, "GET {uri} for {who}");
        }
        // …and neither of them writes, on either axis.
        let res = send(
            &app,
            "POST",
            &format!("/classes/{class}/members"),
            Some(cookie),
            Some(json!({ "user_id": student_id })),
        )
        .await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "member add for {who}");
        let res = send(
            &app,
            "POST",
            &format!("/classes/{class}/courses"),
            Some(cookie),
            Some(json!({ "course_id": course })),
        )
        .await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "attach for {who}");
    }

    // The manager neither created this course nor teaches it, and still
    // attaches it: the bar is management of the course, not authorship of it.
    let res = send(
        &app,
        "POST",
        &format!("/classes/{class}/courses"),
        Some(&manager),
        Some(json!({ "course_id": course })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    // Stored, not merely answered: the link is on the class and the roster moved.
    let res = send(
        &app,
        "GET",
        &format!("/classes/{class}/courses"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 1);
    assert_eq!(common::items(&res.body)[0]["course"], course);
    let res = send(
        &app,
        "GET",
        &format!("/courses/{course}/enrollments"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 1, "{}", res.body);
}

/// A student promoted out of studenthood leaves their class as well as their
/// rosters. Two sweeps run on that path and each owns exactly half the state —
/// the memberships and the class counter, the enrollment rows and the course
/// counter — so both halves are asserted stored, and then the class is deleted,
/// which only a *released* member counter permits.
#[tokio::test]
async fn a_demotion_sweeps_the_class_membership_and_what_it_pumped() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let teacher = login_as(&app, &db, "tch", "teacher").await;
    let student = login(&app, "veli").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "algebra").await;

    let res = send(
        &app,
        "POST",
        "/classes",
        Some(&manager),
        Some(json!({ "name": "9-A" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let class = id_of(&res.body["class"]);
    let res = send(
        &app,
        "POST",
        &format!("/classes/{class}/courses"),
        Some(&teacher),
        Some(json!({ "course_id": course })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/classes/{class}/members"),
        Some(&manager),
        Some(json!({ "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/courses/{course}/enrollments"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 1, "the class seated them first");

    // Out of studenthood, through the endpoint an admin actually uses.
    let res = send(
        &app,
        "PATCH",
        &format!("/users/{student_id}/role"),
        Some(&admin),
        Some(json!({ "role": "teacher" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let res = send(
        &app,
        "GET",
        &format!("/classes/{class}/members"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 0, "the roster let them go");
    let res = send(
        &app,
        "GET",
        &format!("/courses/{course}/enrollments"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 0, "and so did the course");

    // Both tables and both counters, read off the store itself.
    let mut result = db
        .query(
            "SELECT VALUE id FROM class_member; \
             SELECT VALUE id FROM enrollment; \
             SELECT VALUE class_member_count FROM class_group; \
             SELECT VALUE enrollment_count FROM course;",
        )
        .await
        .expect("read the swept state")
        .check()
        .expect("read the swept state");
    assert!(
        result
            .take::<Vec<surrealdb::types::RecordId>>(0)
            .expect("class_member rows")
            .is_empty(),
        "the membership row is deleted, not merely filtered out"
    );
    assert!(
        result
            .take::<Vec<surrealdb::types::RecordId>>(1)
            .expect("enrollment rows")
            .is_empty(),
        "the pumped enrollment is deleted too"
    );
    assert_eq!(
        result.take::<Vec<i64>>(2).expect("class_member_count"),
        vec![0],
        "the class counter came back"
    );
    assert_eq!(
        result.take::<Vec<i64>>(3).expect("enrollment_count"),
        vec![0],
        "the course counter came back"
    );

    // The bite: a membership left behind (or a counter never released) makes
    // the class undeletable forever.
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/classes/{class}/courses/{course}"),
            Some(&teacher),
            None,
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    let res = send(
        &app,
        "DELETE",
        &format!("/classes/{class}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
}

/// A class files itself under a term exactly as a course does, so the term's
/// delete guard has to count classes too — otherwise deleting the calendar
/// entry would leave the class pointing at nothing.
#[tokio::test]
async fn a_term_a_class_points_at_refuses_to_delete() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;

    let res = send(
        &app,
        "POST",
        "/terms",
        Some(&manager),
        Some(json!({
            "name": "2025 Fall",
            "starts_at": 1_600_000_000_000_i64,
            "ends_at": 1_610_000_000_000_i64,
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let term = id_of(&res.body);
    let res = send(
        &app,
        "POST",
        "/classes",
        Some(&manager),
        Some(json!({ "name": "9-A", "term_id": term })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["class"]["term"].as_str(), Some(term.as_str()));
    let class = id_of(&res.body["class"]);

    let res = send(
        &app,
        "DELETE",
        &format!("/terms/{term}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CONFLICT,
        "a class still points at it: {}",
        res.body
    );

    // Unlinking gives the reference back — an explicit `null` on the named
    // field, because a `{}` body short-circuits before any guard runs.
    let res = send(
        &app,
        "PATCH",
        &format!("/classes/{class}"),
        Some(&manager),
        Some(json!({ "term_id": null })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(res.body["term"].is_null());
    let res = send(
        &app,
        "DELETE",
        &format!("/terms/{term}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    // And the class outlives the term it was filed under.
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/classes/{class}"),
            Some(&manager),
            None
        )
        .await
        .status,
        StatusCode::OK
    );
}

/// All-or-nothing where a client can see it: a course with one free seat and a
/// class of two takes *neither* of them. The 409 names the course so a manager
/// knows whose capacity to raise, and — the half only stored state can show —
/// the seats spent on the way to that refusal are all given back.
#[tokio::test]
async fn a_course_that_cannot_seat_the_whole_class_seats_none_of_it() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let teacher = login_as(&app, &db, "tch", "teacher").await;
    let hand = login(&app, "hand").await;
    let ali = login(&app, "ali").await;
    let veli = login(&app, "veli").await;
    let hand_id = me_id(&app, &hand).await;
    let ali_id = me_id(&app, &ali).await;
    let veli_id = me_id(&app, &veli).await;

    // Two seats, one of them already spent by hand: the class of two needs two.
    let res = send(
        &app,
        "POST",
        "/courses",
        Some(&teacher),
        Some(json!({ "title": "small", "capacity": 2 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let course = id_of(&res.body);
    enroll(&app, &teacher, &course, &hand_id).await;

    let res = send(
        &app,
        "POST",
        "/classes",
        Some(&manager),
        Some(json!({ "name": "9-A" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let class = id_of(&res.body["class"]);
    for student in [&ali_id, &veli_id] {
        let res = send(
            &app,
            "POST",
            &format!("/classes/{class}/members"),
            Some(&manager),
            Some(json!({ "user_id": student })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    }

    let res = send(
        &app,
        "POST",
        &format!("/classes/{class}/courses"),
        Some(&teacher),
        Some(json!({ "course_id": course })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let message = res.body["error"].as_str().unwrap_or_default();
    assert!(
        message.contains(&format!("course:{course}")),
        "the refusal must name the full course: {message}"
    );

    // Nothing at all was written: not the seat that did fit, not the link.
    let res = send(
        &app,
        "GET",
        &format!("/courses/{course}/enrollments"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 1, "{}", res.body);
    assert_eq!(
        common::items(&res.body)[0]["user"]["id"],
        hand_id,
        "only the hand-placed row survives"
    );
    let res = send(
        &app,
        "GET",
        &format!("/classes/{class}/courses"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 0);
    assert!(common::items(&res.body).is_empty(), "no link row");
    let mut result = db
        .query("SELECT VALUE enrollment_count FROM course; SELECT VALUE id FROM class_course;")
        .await
        .expect("read the refused state")
        .check()
        .expect("read the refused state");
    assert_eq!(
        result.take::<Vec<i64>>(0).expect("enrollment_count"),
        vec![1],
        "the counter must not have moved"
    );
    assert!(
        result
            .take::<Vec<surrealdb::types::RecordId>>(1)
            .expect("class_course rows")
            .is_empty(),
    );

    // It really was a seat shortfall: a class of one fits, and lands.
    assert_eq!(
        send(
            &app,
            "DELETE",
            &format!("/classes/{class}/members/{veli_id}"),
            Some(&manager),
            None,
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    let res = send(
        &app,
        "POST",
        &format!("/classes/{class}/courses"),
        Some(&teacher),
        Some(json!({ "course_id": course })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/courses/{course}/enrollments"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 2, "{}", res.body);
}

// --- course notes: authz matrix over HTTP -----------------------------------

/// Create a course note as `cookie` on `course` (asserts 201); returns its id.
async fn create_course_note(app: &axum::Router, cookie: &str, course: &str, title: &str) -> String {
    let res = send(
        app,
        "POST",
        "/course-notes",
        Some(cookie),
        Some(json!({ "course": course, "title": title, "content": "body" })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "create course note: {}",
        res.body
    );
    id_of(&res.body)
}

/// (a) an assigned (not creating) teacher writes a note+file on their course;
/// an enrolled student reads it end to end — list, get, file list, download —
/// with byte-identical bytes and the right `Content-Disposition`.
/// (d) the course *creator*, who was never added to the assigned-teacher
/// list, still manages it (update + delete).
#[tokio::test]
async fn course_note_assigned_teacher_and_creator_manage_enrolled_student_reads() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "boss", "manager").await;
    let creator = login_as(&app, &db, "creator_teacher", "teacher").await;
    let assigned = login_as(&app, &db, "assigned_teacher", "teacher").await;
    let student = login_as(&app, &db, "stu", "student").await;

    let course = create_course(&app, &creator, "algebra").await;
    let assigned_id = me_id(&app, &assigned).await;
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/teachers"),
        Some(&manager),
        Some(json!({ "user_id": assigned_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let student_id = me_id(&app, &student).await;
    enroll(&app, &creator, &course, &student_id).await;

    // (a) assigned teacher writes note + file.
    let note = create_course_note(&app, &assigned, &course, "recap").await;
    let bytes = b"%PDF-1.4 fake";
    let up = upload_course_note_file(
        &app,
        &assigned,
        &note,
        "recap.pdf",
        "application/pdf",
        bytes,
    )
    .await;
    assert_eq!(up.status, StatusCode::CREATED, "{}", up.body);
    let file_id = id_of(&up.body);

    // Enrolled student reads: list, get, file list, download.
    let list = send(
        &app,
        "GET",
        &format!("/course-notes?course={course}"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(list.status, StatusCode::OK);
    assert_eq!(common::total(&list.body), 1);

    let get = send(
        &app,
        "GET",
        &format!("/course-notes/{note}"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(get.status, StatusCode::OK);
    assert_eq!(get.body["title"], "recap");

    let files = send(
        &app,
        "GET",
        &format!("/course-notes/{note}/files"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(files.status, StatusCode::OK);
    assert_eq!(common::total(&files.body), 1);

    let (status, headers, body) = common::send_raw(
        &app,
        "GET",
        &format!("/course-notes/{note}/files/{file_id}"),
        Some(&student),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, bytes);
    assert_eq!(headers["content-type"], "application/pdf");
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename=\"recap.pdf\"; filename*=UTF-8''recap.pdf"
    );

    // (d) the creator, never assigned, still manages: update then delete.
    let upd = send(
        &app,
        "PATCH",
        &format!("/course-notes/{note}"),
        Some(&creator),
        Some(json!({ "title": "recap v2" })),
    )
    .await;
    assert_eq!(upd.status, StatusCode::OK, "{}", upd.body);
    assert_eq!(upd.body["title"], "recap v2");

    let del = send(
        &app,
        "DELETE",
        &format!("/course-notes/{note}"),
        Some(&creator),
        None,
    )
    .await;
    assert_eq!(del.status, StatusCode::NO_CONTENT);
}

/// (b) everyone without a seat at the table is turned away: a teacher with no
/// relation to the course (403 on write), an unenrolled student (403 on
/// read), a parent (403 on read), and an unauthenticated caller (401).
#[tokio::test]
async fn course_note_denies_unrelated_teacher_student_parent_and_anon() {
    let (app, db) = app_and_db().await;
    let creator = login_as(&app, &db, "owner_teacher", "teacher").await;
    let other_teacher = login_as(&app, &db, "other_teacher", "teacher").await;
    let other_student = login_as(&app, &db, "other_student", "student").await;
    let parent = login_as(&app, &db, "mom", "parent").await;

    let course = create_course(&app, &creator, "geometry").await;
    let note = create_course_note(&app, &creator, &course, "notes").await;

    // Non-responsible teacher: write paths all 403.
    let create_res = send(
        &app,
        "POST",
        "/course-notes",
        Some(&other_teacher),
        Some(json!({ "course": course, "title": "x", "content": "y" })),
    )
    .await;
    assert_eq!(
        create_res.status,
        StatusCode::FORBIDDEN,
        "{}",
        create_res.body
    );

    let upload_res =
        upload_course_note_file(&app, &other_teacher, &note, "a.txt", "text/plain", b"x").await;
    assert_eq!(
        upload_res.status,
        StatusCode::FORBIDDEN,
        "{}",
        upload_res.body
    );

    let delete_res = send(
        &app,
        "DELETE",
        &format!("/course-notes/{note}"),
        Some(&other_teacher),
        None,
    )
    .await;
    assert_eq!(
        delete_res.status,
        StatusCode::FORBIDDEN,
        "{}",
        delete_res.body
    );

    // Unenrolled student: read paths 403.
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/course-notes?course={course}"),
            Some(&other_student),
            None,
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // Parent: read paths 403 too.
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/course-notes/{note}"),
            Some(&parent),
            None,
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );

    // Unauthenticated: 401.
    assert_eq!(
        send(&app, "GET", &format!("/course-notes/{note}"), None, None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(
            &app,
            "POST",
            "/course-notes",
            None,
            Some(json!({ "course": course, "title": "x" })),
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
}

/// (c) a manager creates and deletes a note on a course they neither created
/// nor are assigned to.
#[tokio::test]
async fn course_note_manager_manages_any_course() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "boss2", "manager").await;
    let creator = login_as(&app, &db, "creator3", "teacher").await;
    let course = create_course(&app, &creator, "chem").await;

    let note = create_course_note(&app, &manager, &course, "manager note").await;
    let del = send(
        &app,
        "DELETE",
        &format!("/course-notes/{note}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(del.status, StatusCode::NO_CONTENT);
}

/// (e) the file cap (409 past `MAX_COURSE_NOTE_FILES`) and the school's byte
/// cap (413 over, 201 at) both apply to course-note files.
#[tokio::test]
async fn course_note_file_cap_and_size_limit() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "boss3", "manager").await;
    let creator = login_as(&app, &db, "creator4", "teacher").await;
    let course = create_course(&app, &creator, "bio").await;
    let note = create_course_note(&app, &creator, &course, "capped").await;

    for i in 0..MAX_COURSE_NOTE_FILES {
        let res = upload_course_note_file(
            &app,
            &creator,
            &note,
            &format!("f{i}.txt"),
            "text/plain",
            b"x",
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "file {i}: {}", res.body);
    }
    let over = upload_course_note_file(
        &app,
        &creator,
        &note,
        "one_too_many.txt",
        "text/plain",
        b"x",
    )
    .await;
    assert_eq!(over.status, StatusCode::CONFLICT, "{}", over.body);

    // Lower the school's byte cap and check the size wall.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "max_file_bytes": 1024 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    let note2 = create_course_note(&app, &creator, &course, "sized").await;
    let big =
        upload_course_note_file(&app, &creator, &note2, "big.bin", "", &vec![7u8; 1025]).await;
    assert_eq!(big.status, StatusCode::PAYLOAD_TOO_LARGE, "{}", big.body);
    let fits =
        upload_course_note_file(&app, &creator, &note2, "fits.bin", "", &vec![7u8; 1024]).await;
    assert_eq!(fits.status, StatusCode::CREATED, "{}", fits.body);
}

/// (f) deleting the course cascades its notes: a note that existed a moment
/// ago 404s once its course is gone.
#[tokio::test]
async fn course_note_cascades_on_course_delete() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "boss4", "manager").await;
    let creator = login_as(&app, &db, "creator5", "teacher").await;
    let course = create_course(&app, &creator, "temp").await;
    let note = create_course_note(&app, &creator, &course, "will vanish").await;

    let del = send(
        &app,
        "DELETE",
        &format!("/courses/{course}"),
        Some(&creator),
        None,
    )
    .await;
    assert_eq!(del.status, StatusCode::NO_CONTENT, "{}", del.body);

    let get = send(
        &app,
        "GET",
        &format!("/course-notes/{note}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(get.status, StatusCode::NOT_FOUND, "{}", get.body);
}

/// (g) a creator demoted teacher -> student mid-session loses every
/// course-note write path and the read path too, on the note they made
/// while still a teacher.
#[tokio::test]
async fn course_note_demoted_creator_loses_writes_and_reads() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "probe_admin", "admin").await;
    let creator = login_as(&app, &db, "probe_creator", "teacher").await;
    let creator_id = me_id(&app, &creator).await;
    let course = create_course(&app, &creator, "algebra2").await;
    let note = create_course_note(&app, &creator, &course, "before").await;

    let r = send(
        &app,
        "PATCH",
        &format!("/users/{creator_id}/role"),
        Some(&admin),
        Some(json!({ "role": "student" })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);

    for (m, u, b) in [
        (
            "POST",
            "/course-notes".to_string(),
            Some(json!({ "course": course, "title": "x" })),
        ),
        (
            "PATCH",
            format!("/course-notes/{note}"),
            Some(json!({ "title": "y" })),
        ),
        ("DELETE", format!("/course-notes/{note}"), None),
    ] {
        let res = send(&app, m, &u, Some(&creator), b).await;
        assert_eq!(
            res.status,
            StatusCode::FORBIDDEN,
            "{m} {u} -> {} {}",
            res.status,
            res.body
        );
    }
    let up = upload_course_note_file(&app, &creator, &note, "a.txt", "text/plain", b"x").await;
    assert_eq!(
        up.status,
        StatusCode::FORBIDDEN,
        "upload -> {} {}",
        up.status,
        up.body
    );
    let read = send(
        &app,
        "GET",
        &format!("/course-notes/{note}"),
        Some(&creator),
        None,
    )
    .await;
    assert_eq!(
        read.status,
        StatusCode::FORBIDDEN,
        "read -> {} {}",
        read.status,
        read.body
    );
}

/// (h) a note id doesn't unlock a file that belongs to another note: mounting
/// course A's file id under course B's note id 404s (note-scoped lookup
/// fails before authz), straight cross-course access 403s, and course A's
/// file survives every one of these attempts.
#[tokio::test]
async fn course_note_cross_course_file_is_not_reachable() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "probe_mgr", "manager").await;
    let t_a = login_as(&app, &db, "probe_ta", "teacher").await;
    let t_b = login_as(&app, &db, "probe_tb", "teacher").await;
    let outsider = login_as(&app, &db, "probe_out", "student").await;

    let course_a = create_course(&app, &t_a, "probeA").await;
    let course_b = create_course(&app, &t_b, "probeB").await;
    let note_a = create_course_note(&app, &t_a, &course_a, "na").await;
    let note_b = create_course_note(&app, &t_b, &course_b, "nb").await;
    let up = upload_course_note_file(&app, &t_a, &note_a, "secret.txt", "text/plain", b"top").await;
    assert_eq!(up.status, StatusCode::CREATED, "{}", up.body);
    let file_a = id_of(&up.body);

    // teacher B mounts A's file id under his own note id (manager can view both, so
    // the only wall left is the note-scoping in read_for).
    for who in [&t_b, &manager] {
        let res = send(
            &app,
            "GET",
            &format!("/course-notes/{note_b}/files/{file_a}"),
            Some(who),
            None,
        )
        .await;
        assert_eq!(
            res.status,
            StatusCode::NOT_FOUND,
            "cross-note -> {} {}",
            res.status,
            res.body
        );
    }
    // teacher B and an unenrolled student straight at A's own note.
    for who in [&t_b, &outsider] {
        let res = send(
            &app,
            "GET",
            &format!("/course-notes/{note_a}/files/{file_a}"),
            Some(who),
            None,
        )
        .await;
        assert_eq!(
            res.status,
            StatusCode::FORBIDDEN,
            "idor -> {} {}",
            res.status,
            res.body
        );
        let res = send(
            &app,
            "GET",
            &format!("/course-notes/{note_a}/files"),
            Some(who),
            None,
        )
        .await;
        assert_eq!(
            res.status,
            StatusCode::FORBIDDEN,
            "list -> {} {}",
            res.status,
            res.body
        );
    }
    // teacher B may not patch/delete A's note or A's file.
    let res = send(
        &app,
        "PATCH",
        &format!("/course-notes/{note_a}"),
        Some(&t_b),
        Some(json!({ "title": "pwn" })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "patch -> {} {}",
        res.status,
        res.body
    );
    let res = send(
        &app,
        "DELETE",
        &format!("/course-notes/{note_a}/files/{file_a}"),
        Some(&t_b),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "del file -> {} {}",
        res.status,
        res.body
    );
    // and the manager deleting through the wrong note is a 404, not a silent hit.
    let res = send(
        &app,
        "DELETE",
        &format!("/course-notes/{note_b}/files/{file_a}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::NOT_FOUND,
        "cross del -> {} {}",
        res.status,
        res.body
    );
    // A's file survives all of it.
    let res = send(
        &app,
        "GET",
        &format!("/course-notes/{note_a}/files"),
        Some(&t_a),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 1, "{}", res.body);
}

/// (i) the per-note file cap refuses without stranding a blob on disk, and
/// deleting the course unlinks every blob it cascaded through. `integration`
/// shares one process-wide files dir across every test in the binary (many
/// running concurrently), so this checks the ten known blob paths by their
/// own id rather than a directory-wide count, which would race.
#[tokio::test]
async fn course_note_cap_refusal_and_course_delete_leave_no_orphan_blobs() {
    let (app, db) = app_and_db().await;
    let creator = login_as(&app, &db, "probe_blob", "teacher").await;
    let course = create_course(&app, &creator, "blobs").await;
    let note = create_course_note(&app, &creator, &course, "n").await;

    let mut blob_paths = Vec::new();
    for i in 0..10 {
        let r = upload_course_note_file(
            &app,
            &creator,
            &note,
            &format!("f{i}.txt"),
            "text/plain",
            b"x",
        )
        .await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
        blob_paths.push(common::blob_dir().join(id_of(&r.body)));
    }
    for p in &blob_paths {
        assert!(p.exists(), "blob missing after upload: {p:?}");
    }
    let over = upload_course_note_file(&app, &creator, &note, "over.txt", "text/plain", b"x").await;
    assert_eq!(over.status, StatusCode::CONFLICT, "{}", over.body);
    for p in &blob_paths {
        assert!(p.exists(), "cap refusal disturbed an existing blob: {p:?}");
    }

    let del = send(
        &app,
        "DELETE",
        &format!("/courses/{course}"),
        Some(&creator),
        None,
    )
    .await;
    assert_eq!(del.status, StatusCode::NO_CONTENT, "{}", del.body);
    for p in &blob_paths {
        assert!(!p.exists(), "course delete left an orphan blob: {p:?}");
    }
    let g = send(
        &app,
        "GET",
        &format!("/course-notes/{note}"),
        Some(&creator),
        None,
    )
    .await;
    assert_eq!(g.status, StatusCode::NOT_FOUND, "{}", g.body);
}

/// The stored AI outputs of a course note over HTTP: readable by anyone who
/// can view the course, deletable only by whoever can manage it, and scoped
/// to the note in the path (an output of a sibling note is a 404, not a
/// cross-note delete). No AI service runs in tests, so the rows are inserted
/// through the domain directly.
#[tokio::test]
async fn course_note_rag_outputs_read_and_delete() {
    let (app, db) = app_and_db().await;
    let creator = login_as(&app, &db, "rag_teacher", "teacher").await;
    let outsider = login_as(&app, &db, "rag_other_teacher", "teacher").await;
    let student = login_as(&app, &db, "rag_student", "student").await;
    let stranger = login_as(&app, &db, "rag_stranger", "student").await;

    let course = create_course(&app, &creator, "chemistry").await;
    let student_id = me_id(&app, &student).await;
    enroll(&app, &creator, &course, &student_id).await;

    let note = create_course_note(&app, &creator, &course, "indexed").await;
    let sibling = create_course_note(&app, &creator, &course, "also indexed").await;
    let up =
        upload_course_note_file(&app, &creator, &note, "src.pdf", "application/pdf", b"x").await;
    assert_eq!(up.status, StatusCode::CREATED, "{}", up.body);
    let file_id = id_of(&up.body);

    let output = RagOutput::create(
        &CourseNoteId::from_key(&note),
        &CourseId::from_key(&course),
        vec![CourseNoteFileId::from_key(&file_id)],
        json!({ "summary": "acids and bases" }),
        &db,
    )
    .await
    .unwrap();
    let output_id = output.get_id().key().to_string();
    let sibling_output = RagOutput::create(
        &CourseNoteId::from_key(&sibling),
        &CourseId::from_key(&course),
        Vec::new(),
        json!({ "summary": "other" }),
        &db,
    )
    .await
    .unwrap();
    let sibling_output_id = sibling_output.get_id().key().to_string();

    // (1) an enrolled student reads the output, sources and payload included.
    let list = send(
        &app,
        "GET",
        &format!("/course-notes/{note}/rag"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.body);
    assert_eq!(common::total(&list.body), 1, "{}", list.body);
    let item = &list.body["items"][0];
    assert_eq!(item["id"], output_id);
    assert_eq!(item["course_note"], note);
    assert_eq!(item["course"], course);
    assert_eq!(item["sources"][0], file_id);
    assert_eq!(item["payload"]["summary"], "acids and bases");
    assert!(item["generated_at"].as_i64().unwrap() > 0, "{}", list.body);

    // (2) a student with no seat in the course gets the same refusal the note
    // itself gives them: 403, not a 404.
    for path in [
        format!("/course-notes/{note}"),
        format!("/course-notes/{note}/rag"),
    ] {
        let res = send(&app, "GET", &path, Some(&stranger), None).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{path}: {}", res.body);
    }

    // (3) reading is not deleting.
    let res = send(
        &app,
        "DELETE",
        &format!("/course-notes/{note}/rag/{output_id}"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);

    // (5) a teacher who does not manage this course cannot either.
    let res = send(
        &app,
        "DELETE",
        &format!("/course-notes/{note}/rag/{output_id}"),
        Some(&outsider),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);

    // (6) a sibling note's output is not reachable under this note.
    let res = send(
        &app,
        "DELETE",
        &format!("/course-notes/{note}/rag/{sibling_output_id}"),
        Some(&creator),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
    assert!(
        RagOutput::read(sibling_output.get_id(), &db)
            .await
            .unwrap()
            .is_some()
    );

    // (4) the managing teacher deletes, and the note's page empties.
    let res = send(
        &app,
        "DELETE",
        &format!("/course-notes/{note}/rag/{output_id}"),
        Some(&creator),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    let list = send(
        &app,
        "GET",
        &format!("/course-notes/{note}/rag"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(common::total(&list.body), 0, "{}", list.body);
}

// =========================================================================
// Multi-school isolation probes (REFUTE lane B). Two schools, same usernames.
// =========================================================================

use hezarfen_backend::tenant::{DEMO_SLUG, SchoolStatus, Slug, Tenants};

/// Demo (school A) plus a second school "beta" (school B), and the registry.
async fn two_schools() -> (
    axum::Router,
    hezarfen_backend::database::Database,
    hezarfen_backend::database::Database,
    Tenants,
) {
    let (app, db_a, tenants) = common::app_and_tenants().await;
    let slug_b = Slug::try_new("beta").expect("slug");
    let db_b = tenants
        .create(&slug_b, "Beta Koleji", ModuleSet::all())
        .await
        .expect("school B");
    (app, db_a, db_b, tenants)
}

/// The same two schools for a remote deployment: [`common::remote_deployment`]
/// creates them itself, since there is no `init_mem_tenants` to seed a demo.
const TWO_SCHOOLS: &[(&str, &str)] = &[(DEMO_SLUG, "Demo School"), ("beta", "Beta Koleji")];

/// One school's handle out of the registry — the one lookup that works in both
/// modes, where `two_schools()`'s return values are memory-mode only.
async fn school_db(tenants: &Tenants, slug: &str) -> hezarfen_backend::database::Database {
    tenants
        .get(&Slug::try_new(slug).expect("a school slug"))
        .await
        .unwrap_or_else(|err| panic!("the {slug} handle: {err}"))
}

/// Every list surface that takes no path parameter — the sweep target.
const LIST_ROUTES: [&str; 19] = [
    "/users",
    "/notes",
    "/messages",
    "/events",
    "/appointments",
    "/appointments/slots",
    "/courses",
    "/courses/me",
    "/classes",
    "/exams",
    "/meals/menus",
    "/payments/plans",
    "/work/me",
    "/pomodoro/me",
    "/questions",
    "/bank-questions",
    "/terms",
    "/boards",
    "/chatbot/threads",
];

/// Fill school B with one row of as many kinds as a single admin can mint.
async fn seed_school_b(app: &axum::Router, cookie: &str) -> serde_json::Value {
    let course = create_course(app, cookie, "B Course").await;
    let subject = create_subject(app, cookie, &course, "B Subject").await;
    let exam = create_exam(app, cookie, &course, "B Exam", "quiz").await;
    let note = send(
        app,
        "POST",
        "/notes",
        Some(cookie),
        Some(json!({ "title": "b-note", "content": "secret of B" })),
    )
    .await;
    assert_eq!(note.status, StatusCode::CREATED, "{}", note.body);
    let event = send(
        app,
        "POST",
        "/events",
        Some(cookie),
        Some(json!({ "title": "b-event", "description": "B only" })),
    )
    .await;
    assert_eq!(event.status, StatusCode::CREATED, "{}", event.body);
    let class = send(
        app,
        "POST",
        "/classes",
        Some(cookie),
        Some(json!({ "name": "9-B" })),
    )
    .await;
    assert_eq!(class.status, StatusCode::CREATED, "{}", class.body);
    let board = send(
        app,
        "POST",
        "/boards",
        Some(cookie),
        Some(json!({ "title": "b-board", "participants": [] })),
    )
    .await;
    assert_eq!(board.status, StatusCode::CREATED, "{}", board.body);
    let course_note = send(
        app,
        "POST",
        "/course-notes",
        Some(cookie),
        Some(json!({ "course": course, "title": "b-cnote", "content": "body" })),
    )
    .await;
    assert_eq!(
        course_note.status,
        StatusCode::CREATED,
        "{}",
        course_note.body
    );
    let bank = send(
        app,
        "POST",
        "/bank-questions",
        Some(cookie),
        Some(
            json!({ "subject_id": subject, "text": "B?", "kind": "choice", "points": 10,
                     "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}],
                     "correct": "c1" }),
        ),
    )
    .await;
    assert_eq!(bank.status, StatusCode::CREATED, "{}", bank.body);
    let thread = send(
        app,
        "POST",
        "/chatbot/threads",
        Some(cookie),
        Some(json!({})),
    )
    .await;
    assert_eq!(thread.status, StatusCode::CREATED, "{}", thread.body);

    let idf = |what: &str, v: &serde_json::Value| -> String {
        v["id"]
            .as_str()
            .unwrap_or_else(|| panic!("{what} response has no id: {v}"))
            .to_string()
    };
    json!({
        "course": course,
        "subject": subject,
        "exam": exam,
        "note": idf("note", &note.body),
        "event": idf("event", &event.body),
        "class": idf("class", &class.body["class"]),
        "board": idf("board", &board.body),
        "course_note": idf("course_note", &course_note.body),
        "bank": idf("bank", &bank.body),
        "thread": idf("thread", &thread.body),
    })
}

/// Invariant 1, read side: with A's cookie every list surface answers with A's
/// own rows only — the totals do not move when B fills up.
#[tokio::test]
async fn probe_cross_school_lists_show_only_own_rows() {
    let (app, _db_a, _db_b, tenants) = two_schools().await;
    probe_cross_school_lists_show_only_own_rows_on(&app, &tenants).await;
}

/// [`probe_cross_school_lists_show_only_own_rows`] on a **real remote deployment** — production's `Mode::Remote`,
/// where isolation rests on each connection's `use_db` pin.
#[tokio::test]
async fn remote_probe_cross_school_lists_show_only_own_rows() {
    let Some(d) = common::remote_deployment(TWO_SCHOOLS).await else {
        return;
    };
    probe_cross_school_lists_show_only_own_rows_on(&d.app, &d.tenants).await;
}

async fn probe_cross_school_lists_show_only_own_rows_on(app: &axum::Router, tenants: &Tenants) {
    let db_a = school_db(tenants, DEMO_SLUG).await;
    let db_b = school_db(tenants, "beta").await;
    let a = common::login_as_school(app, &db_a, DEMO_SLUG, "ada", "admin").await;
    let b = common::login_as_school(app, &db_b, "beta", "boran", "admin").await;

    let mut before = Vec::new();
    for route in LIST_ROUTES {
        let res = send(app, "GET", route, Some(&a), None).await;
        assert_eq!(res.status, StatusCode::OK, "GET {route} as A: {}", res.body);
        before.push(common::total(&res.body));
    }

    let ids = seed_school_b(app, &b).await;

    for (i, route) in LIST_ROUTES.iter().enumerate() {
        let res = send(app, "GET", route, Some(&a), None).await;
        assert_eq!(res.status, StatusCode::OK, "GET {route} as A: {}", res.body);
        assert_eq!(
            common::total(&res.body),
            before[i],
            "GET {route} under A's cookie moved after B created rows: {}",
            res.body
        );
        let dump = res.body.to_string();
        for key in [
            "secret of B",
            "b-note",
            "b-event",
            "9-B",
            "b-board",
            "B Course",
        ] {
            assert!(!dump.contains(key), "GET {route} leaked B's {key}: {dump}");
        }
    }
    // and B still sees its own.
    let res = send(app, "GET", "/notes", Some(&b), None).await;
    assert_eq!(common::total(&res.body), 1, "{}", res.body);
    assert!(ids["note"].is_string());
}

/// Invariant 1, id side: a record minted in B is a 404 under A's cookie on
/// every verb — never a 200, never a 500.
#[tokio::test]
async fn probe_cross_school_ids_are_not_found_under_the_other_cookie() {
    let (app, _db_a, _db_b, tenants) = two_schools().await;
    probe_cross_school_ids_are_not_found_under_the_other_cookie_on(&app, &tenants).await;
}

/// [`probe_cross_school_ids_are_not_found_under_the_other_cookie`] on a **real remote deployment** — production's `Mode::Remote`,
/// where isolation rests on each connection's `use_db` pin.
#[tokio::test]
async fn remote_probe_cross_school_ids_are_not_found_under_the_other_cookie() {
    let Some(d) = common::remote_deployment(TWO_SCHOOLS).await else {
        return;
    };
    probe_cross_school_ids_are_not_found_under_the_other_cookie_on(&d.app, &d.tenants).await;
}

async fn probe_cross_school_ids_are_not_found_under_the_other_cookie_on(
    app: &axum::Router,
    tenants: &Tenants,
) {
    let db_a = school_db(tenants, DEMO_SLUG).await;
    let db_b = school_db(tenants, "beta").await;
    let a = common::login_as_school(app, &db_a, DEMO_SLUG, "ada", "admin").await;
    let b = common::login_as_school(app, &db_b, "beta", "boran", "admin").await;
    let ids = seed_school_b(app, &b).await;
    let s = |k: &str| ids[k].as_str().unwrap().to_string();

    let targets: Vec<(String, serde_json::Value)> = vec![
        (format!("/notes/{}", s("note")), json!({ "title": "x" })),
        (format!("/events/{}", s("event")), json!({ "title": "x" })),
        (format!("/courses/{}", s("course")), json!({ "title": "x" })),
        (format!("/exams/{}", s("exam")), json!({ "title": "x" })),
        (
            format!("/subjects/{}", s("subject")),
            json!({ "name": "x" }),
        ),
        (format!("/classes/{}", s("class")), json!({ "name": "x" })),
        (format!("/boards/{}", s("board")), json!({ "title": "x" })),
        (
            format!("/course-notes/{}", s("course_note")),
            json!({ "title": "x" }),
        ),
        (
            format!("/bank-questions/{}", s("bank")),
            json!({ "text": "x" }),
        ),
    ];

    for (uri, patch) in targets {
        for (method, body) in [
            ("GET", None),
            ("PATCH", Some(patch.clone())),
            ("DELETE", None),
        ] {
            let res = send(app, method, &uri, Some(&a), body).await;
            assert_ne!(
                res.status,
                StatusCode::OK,
                "{method} {uri} under A returned 200 for a row that lives in B: {}",
                res.body
            );
            assert_ne!(
                res.status,
                StatusCode::NO_CONTENT,
                "{method} {uri} under A deleted/updated a row that lives in B"
            );
            assert!(
                !res.status.is_server_error(),
                "{method} {uri} under A: {} {}",
                res.status,
                res.body
            );
        }
    }

    // The one list surface that takes its scope from a query parameter: B's
    // course id under A's cookie must not open B's notes.
    let res = send(
        app,
        "GET",
        &format!("/course-notes?course={}", s("course")),
        Some(&a),
        None,
    )
    .await;
    assert_ne!(
        res.status,
        StatusCode::OK,
        "GET /course-notes?course=<B course> under A's cookie: {}",
        res.body
    );
    assert!(!res.status.is_server_error(), "{} {}", res.status, res.body);

    // B's rows survived every one of A's attempts.
    for (uri, _) in [
        (format!("/notes/{}", s("note")), ()),
        (format!("/courses/{}", s("course")), ()),
        (format!("/boards/{}", s("board")), ()),
    ] {
        let res = send(app, "GET", &uri, Some(&b), None).await;
        assert_eq!(
            res.status,
            StatusCode::OK,
            "B still owns {uri}: {}",
            res.body
        );
    }
}

/// Invariant 1, body side: an id from B carried in a request *body* under A's
/// cookie must be refused, never silently linked.
#[tokio::test]
async fn probe_cross_school_ids_in_bodies_are_refused() {
    let (app, _db_a, _db_b, tenants) = two_schools().await;
    probe_cross_school_ids_in_bodies_are_refused_on(&app, &tenants).await;
}

/// [`probe_cross_school_ids_in_bodies_are_refused`] on a **real remote deployment** — production's `Mode::Remote`,
/// where isolation rests on each connection's `use_db` pin.
#[tokio::test]
async fn remote_probe_cross_school_ids_in_bodies_are_refused() {
    let Some(d) = common::remote_deployment(TWO_SCHOOLS).await else {
        return;
    };
    probe_cross_school_ids_in_bodies_are_refused_on(&d.app, &d.tenants).await;
}

async fn probe_cross_school_ids_in_bodies_are_refused_on(app: &axum::Router, tenants: &Tenants) {
    let db_a = school_db(tenants, DEMO_SLUG).await;
    let db_b = school_db(tenants, "beta").await;
    let a_admin = common::login_as_school(app, &db_a, DEMO_SLUG, "ada", "admin").await;
    let a_student = common::login_as_school(app, &db_a, DEMO_SLUG, "ali", "student").await;
    let a_parent = common::login_as_school(app, &db_a, DEMO_SLUG, "veli", "parent").await;
    let b_admin = common::login_as_school(app, &db_b, "beta", "boran", "admin").await;
    common::login_as_school(app, &db_b, "beta", "bstudent", "student").await;

    let b_user = {
        let res = send(app, "GET", "/users", Some(&b_admin), None).await;
        assert_eq!(res.status, StatusCode::OK);
        id_of(&common::items(&res.body)[0])
    };
    let a_parent_id = common::me_id(app, &a_parent).await;
    let a_course = create_course(app, &a_admin, "A Course").await;
    let a_board = {
        let res = send(
            app,
            "POST",
            "/boards",
            Some(&a_admin),
            Some(json!({ "title": "a-board", "participants": [] })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
        id_of(&res.body)
    };

    let cases: Vec<(&str, String, &str, serde_json::Value)> = vec![
        (
            "message to a B recipient",
            "/messages".into(),
            &a_student,
            json!({ "recipient_id": b_user, "subject": "hi", "body": "cross" }),
        ),
        (
            "enroll a B user into an A course",
            format!("/courses/{a_course}/enrollments"),
            &a_admin,
            json!({ "user_id": b_user }),
        ),
        (
            "assign a B user as an A course teacher",
            format!("/courses/{a_course}/teachers"),
            &a_admin,
            json!({ "user_id": b_user }),
        ),
        (
            "link a B student to an A parent",
            format!("/users/{a_parent_id}/students"),
            &a_admin,
            json!({ "user_id": b_user }),
        ),
        (
            "invite a B user onto an A board",
            format!("/boards/{a_board}/invite"),
            &a_admin,
            json!({ "participants": [b_user] }),
        ),
        (
            "create an A board with a B participant",
            "/boards".into(),
            &a_admin,
            json!({ "title": "x", "participants": [b_user] }),
        ),
    ];

    for (what, uri, cookie, body) in cases {
        let res = send(app, "POST", &uri, Some(cookie), Some(body)).await;
        assert!(
            res.status.is_client_error(),
            "{what}: POST {uri} answered {} (expected a 4xx refusal): {}",
            res.status,
            res.body
        );
        assert!(
            !res.body.to_string().contains(&b_user),
            "{what}: POST {uri} echoed B's user id back: {}",
            res.body
        );
    }
}

/// Invariant 2: a suspended school is refused everywhere, and a resume brings
/// the *same* cookie back to life.
#[tokio::test]
async fn probe_suspension_blocks_login_and_a_live_cookie_then_resume_restores_it() {
    let (app, _db_a, _db_b, tenants) = two_schools().await;
    probe_suspension_blocks_login_and_a_live_cookie_then_resume_restores_it_on(&app, &tenants)
        .await;
}

/// [`probe_suspension_blocks_login_and_a_live_cookie_then_resume_restores_it`] on a **real remote deployment** — production's `Mode::Remote`,
/// where isolation rests on each connection's `use_db` pin.
#[tokio::test]
async fn remote_probe_suspension_blocks_login_and_a_live_cookie_then_resume_restores_it() {
    let Some(d) = common::remote_deployment(TWO_SCHOOLS).await else {
        return;
    };
    probe_suspension_blocks_login_and_a_live_cookie_then_resume_restores_it_on(&d.app, &d.tenants)
        .await;
}

async fn probe_suspension_blocks_login_and_a_live_cookie_then_resume_restores_it_on(
    app: &axum::Router,
    tenants: &Tenants,
) {
    let db_b = school_db(tenants, "beta").await;
    let slug_b = Slug::try_new("beta").unwrap();
    let b = common::login_as_school(app, &db_b, "beta", "boran", "admin").await;

    // Alive first.
    let res = send(app, "GET", "/auth/me", Some(&b), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    tenants
        .set_status(&slug_b, SchoolStatus::Suspended)
        .await
        .expect("suspend");

    // The already-issued cookie, on the very next request.
    for route in ["/auth/me", "/users", "/notes", "/courses", "/settings"] {
        let res = send(app, "GET", route, Some(&b), None).await;
        assert_eq!(
            res.status,
            StatusCode::FORBIDDEN,
            "GET {route} with a suspended school's live cookie: {} {}",
            res.status,
            res.body
        );
    }
    // A write too.
    let res = send(
        app,
        "POST",
        "/notes",
        Some(&b),
        Some(json!({ "title": "t", "content": "c" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);

    // Login is no exception.
    let res = send(
        app,
        "POST",
        "/auth/login",
        None,
        Some(json!({ "school": "beta", "username": "boran", "password": "secret1" })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "login into a suspended school: {}",
        res.body
    );

    // Register into it is refused too.
    let res = send(
        app,
        "POST",
        "/auth/register",
        None,
        Some(json!({ "school": "beta", "username": "newcomer", "password": "secret1" })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "register into a suspended school: {}",
        res.body
    );

    // Resume: the same cookie works again — the session rows were not deleted.
    tenants
        .set_status(&slug_b, SchoolStatus::Active)
        .await
        .expect("resume");
    let res = send(app, "GET", "/auth/me", Some(&b), None).await;
    assert_eq!(
        res.status,
        StatusCode::OK,
        "the same cookie after a resume: {}",
        res.body
    );
    assert_eq!(res.body["username"], "boran");
}

/// Invariant 3: no cookie is usable as another kind of cookie. The last case —
/// a real token from school A presented under school B's prefix — is the one
/// that would be a full takeover.
#[tokio::test]
async fn probe_cookie_confusion_is_impossible_both_ways() {
    let (app, _db_a, _db_b, tenants) = two_schools().await;
    probe_cookie_confusion_is_impossible_both_ways_on(&app, &tenants).await;
}

/// [`probe_cookie_confusion_is_impossible_both_ways`] on a **real remote deployment** — production's `Mode::Remote`,
/// where isolation rests on each connection's `use_db` pin.
#[tokio::test]
async fn remote_probe_cookie_confusion_is_impossible_both_ways() {
    let Some(d) = common::remote_deployment(TWO_SCHOOLS).await else {
        return;
    };
    probe_cookie_confusion_is_impossible_both_ways_on(&d.app, &d.tenants).await;
}

async fn probe_cookie_confusion_is_impossible_both_ways_on(app: &axum::Router, tenants: &Tenants) {
    let db_a = school_db(tenants, DEMO_SLUG).await;
    let db_b = school_db(tenants, "beta").await;
    let a = common::login_as_school(app, &db_a, DEMO_SLUG, "ada", "admin").await;
    let b = common::login_as_school(app, &db_b, "beta", "boran", "admin").await;
    let token_a = common::cookie_token(&a).to_string();

    hezarfen_backend::domain::builder::Builder::ensure(
        Username::try_new("operator").unwrap(),
        Password::try_new("secret1").unwrap(),
        tenants.control(),
    )
    .await
    .expect("seed the builder");
    let res = send(
        app,
        "POST",
        "/builder/login",
        None,
        Some(json!({ "username": "operator", "password": "secret1" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let builder_cookie = res.cookie.expect("builder cookie");
    let builder_token = common::cookie_token(&builder_cookie).to_string();

    // (1) builder cookie on a school surface.
    for route in ["/auth/me", "/users", "/notes"] {
        let res = send(app, "GET", route, Some(&builder_cookie), None).await;
        assert_eq!(
            res.status,
            StatusCode::UNAUTHORIZED,
            "builder cookie on GET {route}: {} {}",
            res.status,
            res.body
        );
    }
    // (2) school cookie on the builder surface.
    for cookie in [&a, &b] {
        for route in ["/builder/me", "/schools"] {
            let res = send(app, "GET", route, Some(cookie), None).await;
            assert_eq!(
                res.status,
                StatusCode::UNAUTHORIZED,
                "school cookie on GET {route}: {} {}",
                res.status,
                res.body
            );
        }
    }
    // (3) bare token, no dot.
    for raw in [
        format!("session={token_a}"),
        format!("session={builder_token}"),
        "session=.".to_string(),
        format!("session=.{token_a}"),
        format!("session={token_a}."),
    ] {
        for route in ["/auth/me", "/builder/me"] {
            let res = send(app, "GET", route, Some(&raw), None).await;
            assert_eq!(
                res.status,
                StatusCode::UNAUTHORIZED,
                "{raw} on GET {route}: {} {}",
                res.status,
                res.body
            );
        }
    }
    // (4) a builder token wearing a school prefix.
    for raw in [
        format!("session=demo.{builder_token}"),
        format!("session=beta.{builder_token}"),
    ] {
        let res = send(app, "GET", "/auth/me", Some(&raw), None).await;
        assert_eq!(
            res.status,
            StatusCode::UNAUTHORIZED,
            "{raw} on /auth/me: {} {}",
            res.status,
            res.body
        );
    }
    // (5) THE one: A's live token under B's prefix, and B's under A's.
    let token_b = common::cookie_token(&b).to_string();
    for (raw, whose) in [
        (format!("session=beta.{token_a}"), "A's token under B"),
        (format!("session=demo.{token_b}"), "B's token under A"),
    ] {
        for route in ["/auth/me", "/users", "/notes", "/settings"] {
            let res = send(app, "GET", route, Some(&raw), None).await;
            assert_eq!(
                res.status,
                StatusCode::UNAUTHORIZED,
                "{whose} on GET {route}: {} {}",
                res.status,
                res.body
            );
        }
    }
    // (6) a school cookie naming a school that does not exist.
    let res = send(
        app,
        "GET",
        "/auth/me",
        Some(&format!("session=nosuchschool.{token_a}")),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED, "{}", res.body);
}

/// Invariant 4: `POST /auth/register` names its school, registers into that one
/// only, and an unknown school is indistinguishable from a bad credential.
#[tokio::test]
async fn probe_register_is_scoped_to_the_named_school() {
    let (app, _db_a, _db_b, tenants) = two_schools().await;
    probe_register_is_scoped_to_the_named_school_on(&app, &tenants).await;
}

/// [`probe_register_is_scoped_to_the_named_school`] on a **real remote deployment** — production's `Mode::Remote`,
/// where isolation rests on each connection's `use_db` pin.
#[tokio::test]
async fn remote_probe_register_is_scoped_to_the_named_school() {
    let Some(d) = common::remote_deployment(TWO_SCHOOLS).await else {
        return;
    };
    probe_register_is_scoped_to_the_named_school_on(&d.app, &d.tenants).await;
}

async fn probe_register_is_scoped_to_the_named_school_on(app: &axum::Router, tenants: &Tenants) {
    let db_a = school_db(tenants, DEMO_SLUG).await;
    let db_b = school_db(tenants, "beta").await;

    // Missing `school` is a 4xx, not a silent default.
    let res = send(
        app,
        "POST",
        "/auth/register",
        None,
        Some(json!({ "username": "noschool", "password": "secret1" })),
    )
    .await;
    assert!(
        res.status.is_client_error(),
        "register with no school: {} {}",
        res.status,
        res.body
    );

    // The same username in both schools, independently.
    for (slug, db) in [(DEMO_SLUG, &db_a), ("beta", &db_b)] {
        let res = send(
            app,
            "POST",
            "/auth/register",
            None,
            Some(json!({ "school": slug, "username": "ayse", "password": "secret1" })),
        )
        .await;
        assert_eq!(
            res.status,
            StatusCode::CREATED,
            "register into {slug}: {}",
            res.body
        );
        let found: Vec<String> = db
            .query("SELECT VALUE username FROM user WHERE username = 'ayse'")
            .await
            .unwrap()
            .take(0)
            .unwrap();
        assert_eq!(
            found,
            vec!["ayse".to_string()],
            "exactly one 'ayse' in {slug}"
        );
    }

    // Unknown school: same 401 and same body as a bad credential — no
    // enumeration of the customer list.
    let unknown = send(
        app,
        "POST",
        "/auth/login",
        None,
        Some(json!({ "school": "not-a-school", "username": "ayse", "password": "secret1" })),
    )
    .await;
    let bad_pass = send(
        app,
        "POST",
        "/auth/login",
        None,
        Some(json!({ "school": DEMO_SLUG, "username": "ayse", "password": "wrongpw1" })),
    )
    .await;
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED, "{}", unknown.body);
    assert_eq!(unknown.status, bad_pass.status, "status differs");
    assert_eq!(
        unknown.body, bad_pass.body,
        "an unknown school answers differently from a bad password: {} vs {}",
        unknown.body, bad_pass.body
    );

    // Register against an unknown school: same treatment.
    let reg_unknown = send(
        app,
        "POST",
        "/auth/register",
        None,
        Some(json!({ "school": "not-a-school", "username": "zeynep", "password": "secret1" })),
    )
    .await;
    assert_eq!(
        reg_unknown.status,
        StatusCode::UNAUTHORIZED,
        "register into an unknown school: {} {}",
        reg_unknown.status,
        reg_unknown.body
    );
}

/// Invariant 5: an upload under A lands under `FILES_PATH/A/` and is a 404
/// under B's cookie even with the exact ids.
#[tokio::test]
async fn probe_uploaded_files_are_school_scoped() {
    let (app, _db_a, _db_b, tenants) = two_schools().await;
    probe_uploaded_files_are_school_scoped_on(&app, &tenants, common::files_dir().as_path()).await;
}

/// [`probe_uploaded_files_are_school_scoped`] on a **real remote deployment** — production's `Mode::Remote`,
/// where isolation rests on each connection's `use_db` pin.
#[tokio::test]
async fn remote_probe_uploaded_files_are_school_scoped() {
    let Some(d) = common::remote_deployment(TWO_SCHOOLS).await else {
        return;
    };
    probe_uploaded_files_are_school_scoped_on(&d.app, &d.tenants, &d.files).await;
}

async fn probe_uploaded_files_are_school_scoped_on(
    app: &axum::Router,
    tenants: &Tenants,
    files: &std::path::Path,
) {
    let db_a = school_db(tenants, DEMO_SLUG).await;
    let db_b = school_db(tenants, "beta").await;
    let a = common::login_as_school(app, &db_a, DEMO_SLUG, "ali", "student").await;
    let b = common::login_as_school(app, &db_b, "beta", "ali", "student").await;

    let note = send(
        app,
        "POST",
        "/notes",
        Some(&a),
        Some(json!({ "title": "n", "content": "c" })),
    )
    .await;
    assert_eq!(note.status, StatusCode::CREATED, "{}", note.body);
    let note_id = id_of(&note.body);

    let up = common::upload_file(app, &a, &note_id, "a.txt", "text/plain", b"A-ONLY-BYTES").await;
    assert_eq!(up.status, StatusCode::CREATED, "{}", up.body);
    let file_id = id_of(&up.body);

    // It landed under the school's own directory, and nowhere else.
    let a_dir = files.join(DEMO_SLUG);
    let b_dir = files.join("beta");
    let count = |dir: &std::path::Path| {
        std::fs::read_dir(dir)
            .map(|it| it.count())
            .unwrap_or_default()
    };
    assert!(count(&a_dir) >= 1, "A's blob directory {a_dir:?} is empty");
    assert_eq!(
        count(&b_dir),
        0,
        "B's blob directory {b_dir:?} is not empty"
    );

    // A reads it back.
    let uri = format!("/notes/{note_id}/files/{file_id}");
    let (status, _, bytes) = common::send_raw(app, "GET", &uri, Some(&a), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK, "A reads its own file");
    assert_eq!(bytes, b"A-ONLY-BYTES");

    // B, with the exact ids, gets nothing — and no 500.
    let (status, _, bytes) = common::send_raw(app, "GET", &uri, Some(&b), None, Vec::new()).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "B read A's file: {}",
        String::from_utf8_lossy(&bytes)
    );
    assert!(!bytes.windows(12).any(|w| w == b"A-ONLY-BYTES"));

    let (status, _, _) = common::send_raw(app, "DELETE", &uri, Some(&b), None, Vec::new()).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "B deleted A's file");
    let (status, _, _) = common::send_raw(app, "GET", &uri, Some(&a), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK, "A's file survived B's DELETE");
}

/// Invariant 1, the leak that would be invisible from the API: no school row
/// may land in the **control** database, and the search + settings surfaces
/// (which are not paged lists) stay inside the caller's school.
#[tokio::test]
async fn probe_school_rows_never_reach_the_control_database() {
    let (app, _db_a, _db_b, tenants) = two_schools().await;
    probe_school_rows_never_reach_the_control_database_on(&app, &tenants).await;
}

/// [`probe_school_rows_never_reach_the_control_database`] on a **real remote deployment** — production's `Mode::Remote`,
/// where isolation rests on each connection's `use_db` pin.
#[tokio::test]
async fn remote_probe_school_rows_never_reach_the_control_database() {
    let Some(d) = common::remote_deployment(TWO_SCHOOLS).await else {
        return;
    };
    probe_school_rows_never_reach_the_control_database_on(&d.app, &d.tenants).await;
}

async fn probe_school_rows_never_reach_the_control_database_on(
    app: &axum::Router,
    tenants: &Tenants,
) {
    let db_a = school_db(tenants, DEMO_SLUG).await;
    let db_b = school_db(tenants, "beta").await;
    let a = common::login_as_school(app, &db_a, DEMO_SLUG, "ada", "admin").await;
    let b = common::login_as_school(app, &db_b, "beta", "boran", "admin").await;
    common::login_as_school(app, &db_b, "beta", "bstudent", "student").await;
    let ids = seed_school_b(app, &b).await;
    assert!(ids["course"].is_string());

    // Nothing a school writes may appear in the control database.
    let control = tenants.control();
    for table in [
        "user",
        "session",
        "note",
        "course",
        "event",
        "message",
        "class_group",
        "board",
        "course_note",
        "bank_question",
        "chatbot_thread",
        "exam",
    ] {
        let rows: Vec<surrealdb::types::RecordId> = control
            .query(format!("SELECT VALUE id FROM {table}"))
            .await
            .expect("control query")
            .take(0)
            .unwrap_or_default();
        assert!(
            rows.is_empty(),
            "the control database holds {} `{table}` row(s): {rows:?}",
            rows.len()
        );
    }
    // And the control database does hold the two schools.
    let schools: Vec<String> = control
        .query("SELECT VALUE slug FROM school ORDER BY slug")
        .await
        .unwrap()
        .take(0)
        .unwrap();
    assert_eq!(schools, vec!["beta".to_string(), "demo".to_string()]);

    // User search never crosses the wall.
    for (cookie, forbidden) in [(&a, "boran"), (&b, "ada")] {
        for q in ["b", "a", "boran", "ada", "bstudent"] {
            let res = send(
                app,
                "GET",
                &format!("/users/search?q={q}"),
                Some(cookie),
                None,
            )
            .await;
            assert_eq!(res.status, StatusCode::OK, "search {q}: {}", res.body);
            assert!(
                !res.body.to_string().contains(forbidden),
                "GET /users/search?q={q} returned the other school's `{forbidden}`: {}",
                res.body
            );
        }
    }

    // Settings are the school's own: moving A's leaves B's alone.
    let before = send(app, "GET", "/settings", Some(&b), None).await;
    assert_eq!(before.status, StatusCode::OK, "{}", before.body);
    let patched = send(
        app,
        "PATCH",
        "/settings",
        Some(&a),
        Some(json!({ "max_file_bytes": 4096 })),
    )
    .await;
    assert_eq!(patched.status, StatusCode::OK, "{}", patched.body);
    assert_eq!(patched.body["max_file_bytes"], 4096);
    let after = send(app, "GET", "/settings", Some(&b), None).await;
    assert_eq!(
        after.body, before.body,
        "A's settings PATCH moved B's settings"
    );

    // A cookie prefix that is not a well-formed slug is a 401, never a lookup.
    for prefix in ["DEMO", "Demo", "de mo", "de/mo", "..", "demo%2e", "control"] {
        let raw = format!("session={prefix}.{}", common::cookie_token(&a));
        let res = send(app, "GET", "/auth/me", Some(&raw), None).await;
        assert_eq!(
            res.status,
            StatusCode::UNAUTHORIZED,
            "prefix `{prefix}`: {} {}",
            res.status,
            res.body
        );
    }
}

/// Invariant 1 on the path production actually uses: `Mode::Remote`, where every
/// school is a *database inside one namespace on one server* and isolation rests
/// entirely on each connection's `use_db` pin — unlike `Mode::Mem`, where a
/// school is its own embedded datastore and isolation cannot fail.
///
/// The two slugs are the ones that are not bare identifiers: `ata-koleji` parses
/// as a subtraction unquoted, `2024school` as a duration.
#[tokio::test]
async fn remote_probe_remote_mode_keeps_two_schools_apart() {
    let Some(d) = common::remote_deployment(&[("ata-koleji", "Ata"), ("2024school", "2024")]).await
    else {
        return;
    };
    let (app, tenants) = (&d.app, &d.tenants);
    let slug_a = Slug::try_new("ata-koleji").unwrap();
    let slug_b = Slug::try_new("2024school").unwrap();

    // The same username in both schools, over the real router.
    let mut cookies = Vec::new();
    for slug in [&slug_a, &slug_b] {
        let creds = json!({ "school": slug.as_str(), "username": "ada", "password": "secret1" });
        let reg = send(app, "POST", "/auth/register", None, Some(creds.clone())).await;
        assert_eq!(
            reg.status,
            StatusCode::CREATED,
            "register in {slug}: {}",
            reg.body
        );
        let db = tenants.get(slug).await.unwrap();
        common::set_role(&db, "ada", "admin").await;
        let res = send(app, "POST", "/auth/login", None, Some(creds)).await;
        assert_eq!(res.status, StatusCode::OK, "login in {slug}: {}", res.body);
        cookies.push(res.cookie.expect("cookie"));
    }
    let (a, b) = (cookies[0].clone(), cookies[1].clone());

    // B fills up; A must not see a single row of it.
    let ids = seed_school_b(app, &b).await;
    for route in LIST_ROUTES {
        let res = send(app, "GET", route, Some(&a), None).await;
        assert_eq!(res.status, StatusCode::OK, "GET {route} as A: {}", res.body);
        let dump = res.body.to_string();
        for key in [
            "secret of B",
            "b-note",
            "b-event",
            "9-B",
            "b-board",
            "B Course",
        ] {
            assert!(!dump.contains(key), "GET {route} leaked B's {key}: {dump}");
        }
    }
    // Each school sees exactly its own user, and only one.
    for cookie in [&a, &b] {
        let res = send(app, "GET", "/users", Some(cookie), None).await;
        assert_eq!(common::total(&res.body), 1, "{}", res.body);
    }
    // A row minted in B is a 404 under A on every verb.
    for key in ["note", "course", "board", "event", "class"] {
        let id = ids[key].as_str().unwrap();
        let uri = match key {
            "note" => format!("/notes/{id}"),
            "course" => format!("/courses/{id}"),
            "board" => format!("/boards/{id}"),
            "event" => format!("/events/{id}"),
            _ => format!("/classes/{id}"),
        };
        for method in ["GET", "DELETE"] {
            let res = send(app, method, &uri, Some(&a), None).await;
            assert_ne!(
                res.status,
                StatusCode::OK,
                "{method} {uri} as A: {}",
                res.body
            );
            assert_ne!(res.status, StatusCode::NO_CONTENT, "{method} {uri} as A");
            assert!(
                !res.status.is_server_error(),
                "{method} {uri}: {} {}",
                res.status,
                res.body
            );
        }
        let res = send(app, "GET", &uri, Some(&b), None).await;
        assert_eq!(
            res.status,
            StatusCode::OK,
            "B still owns {uri}: {}",
            res.body
        );
    }

    // A's token under B's prefix stays a 401 on a real server too.
    let raw = format!("session={}.{}", slug_b, common::cookie_token(&a));
    let res = send(app, "GET", "/auth/me", Some(&raw), None).await;
    assert_eq!(res.status, StatusCode::UNAUTHORIZED, "{}", res.body);
}

// -------------------------------------------------------------------------
// Remote-only invariants: things `Mode::Mem` cannot even express, because a
// memory-mode school is its own datastore rather than a database on a server.
// -------------------------------------------------------------------------

/// The namespace's database list, out of `INFO FOR NS` on the control handle.
async fn namespace_databases(tenants: &Tenants) -> Vec<String> {
    let mut info = tenants
        .control()
        .query("INFO FOR NS")
        .await
        .expect("INFO FOR NS");
    let info: serde_json::Value = info
        .take::<Option<serde_json::Value>>(0)
        .expect("INFO FOR NS row")
        .expect("INFO FOR NS is never empty");
    let mut names: Vec<String> = info["databases"]
        .as_object()
        .unwrap_or_else(|| panic!("INFO FOR NS has no databases map: {info}"))
        .keys()
        .cloned()
        .collect();
    names.sort();
    names
}

/// Remote-only: a school **is** a database, so the namespace must hold exactly
/// the created schools plus `control` — and `DELETE /schools/{slug}` must
/// `REMOVE DATABASE`, not merely delete the registry row.
#[tokio::test]
async fn remote_probe_a_school_is_a_database_and_delete_removes_it() {
    let Some(d) = common::remote_deployment(TWO_SCHOOLS).await else {
        return;
    };
    assert_eq!(
        namespace_databases(&d.tenants).await,
        vec![
            "beta".to_string(),
            "control".to_string(),
            "demo".to_string()
        ],
        "the namespace holds exactly the two schools and the control database"
    );

    hezarfen_backend::domain::builder::Builder::ensure(
        Username::try_new("operator").unwrap(),
        Password::try_new("secret1").unwrap(),
        d.tenants.control(),
    )
    .await
    .expect("seed the builder");
    let res = send(
        &d.app,
        "POST",
        "/builder/login",
        None,
        Some(json!({ "username": "operator", "password": "secret1" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let builder = res.cookie.expect("builder cookie");

    let res = send(&d.app, "DELETE", "/schools/beta", Some(&builder), None).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    assert_eq!(
        namespace_databases(&d.tenants).await,
        vec!["control".to_string(), "demo".to_string()],
        "DELETE /schools/beta left beta's database standing"
    );
}

/// Remote-only: raw SQL on a school's own handle sees that school's rows and
/// no others — the `use_db` pin, checked under the API rather than through it.
#[tokio::test]
async fn remote_probe_raw_sql_on_a_school_handle_counts_only_its_own_rows() {
    let Some(d) = common::remote_deployment(TWO_SCHOOLS).await else {
        return;
    };
    // Different counts, so a leak cannot hide behind equal numbers.
    for (slug, users) in [
        (DEMO_SLUG, ["ada", "ali", "ayse"].as_slice()),
        ("beta", &["boran"]),
    ] {
        for username in users {
            let res = send(
                &d.app,
                "POST",
                "/auth/register",
                None,
                Some(json!({ "school": slug, "username": username, "password": "secret1" })),
            )
            .await;
            assert_eq!(
                res.status,
                StatusCode::CREATED,
                "{slug}/{username}: {}",
                res.body
            );
        }
    }

    let count = async |db: &hezarfen_backend::database::Database| -> i64 {
        db.query("SELECT count() FROM user GROUP ALL")
            .await
            .expect("count query")
            .take::<Vec<serde_json::Value>>(0)
            .expect("count row")
            .first()
            .and_then(|row| row["count"].as_i64())
            .unwrap_or_default()
    };
    let db_a = school_db(&d.tenants, DEMO_SLUG).await;
    let db_b = school_db(&d.tenants, "beta").await;
    assert_eq!(count(&db_a).await, 3, "demo's own users");
    assert_eq!(count(&db_b).await, 1, "beta's own users");
}

/// Remote-only: one namespace, so the *same record id string* exists in both
/// databases' address space. Selected on the other school's handle it must
/// still find nothing — record ids are not global.
#[tokio::test]
async fn remote_probe_a_record_id_minted_in_one_school_is_absent_in_the_other() {
    let Some(d) = common::remote_deployment(TWO_SCHOOLS).await else {
        return;
    };
    let res = send(
        &d.app,
        "POST",
        "/auth/register",
        None,
        Some(json!({ "school": DEMO_SLUG, "username": "ada", "password": "secret1" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    let db_a = school_db(&d.tenants, DEMO_SLUG).await;
    let db_b = school_db(&d.tenants, "beta").await;
    let id: surrealdb::types::RecordId = db_a
        .query("SELECT VALUE id FROM user LIMIT 1")
        .await
        .expect("A's user id")
        .take::<Vec<surrealdb::types::RecordId>>(0)
        .expect("id row")
        .pop()
        .expect("A minted exactly one user");

    let mine: Option<serde_json::Value> = db_a.select(id.clone()).await.expect("select in A");
    assert!(mine.is_some(), "{id:?} is A's own row");
    let theirs: Option<serde_json::Value> = db_b.select(id.clone()).await.expect("select in B");
    assert!(
        theirs.is_none(),
        "{id:?} — minted in demo — resolved on beta's handle: {theirs:?}"
    );
}

/// Remote-only: eviction really drops the socket here (in memory mode the
/// cached handle *is* the store, so it is never dropped). A suspend/resume must
/// therefore reconnect and find the school's rows exactly as they were.
#[tokio::test]
async fn remote_probe_a_suspended_school_reconnects_with_its_rows_intact() {
    let Some(d) = common::remote_deployment(TWO_SCHOOLS).await else {
        return;
    };
    let slug = Slug::try_new(DEMO_SLUG).unwrap();
    let db = school_db(&d.tenants, DEMO_SLUG).await;
    let cookie = common::login_as_school(&d.app, &db, DEMO_SLUG, "ada", "admin").await;
    let note = send(
        &d.app,
        "POST",
        "/notes",
        Some(&cookie),
        Some(json!({ "title": "before", "content": "kept" })),
    )
    .await;
    assert_eq!(note.status, StatusCode::CREATED, "{}", note.body);
    let note_id = id_of(&note.body);

    d.tenants
        .set_status(&slug, SchoolStatus::Suspended)
        .await
        .expect("suspend");
    let res = send(&d.app, "GET", "/auth/me", Some(&cookie), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    d.tenants
        .set_status(&slug, SchoolStatus::Active)
        .await
        .expect("resume");

    // A fresh connection (the old one was evicted) onto the same database.
    let reconnected = school_db(&d.tenants, DEMO_SLUG).await;
    let titles: Vec<String> = reconnected
        .query("SELECT VALUE title FROM note")
        .await
        .expect("notes after the resume")
        .take(0)
        .expect("title rows");
    assert_eq!(titles, vec!["before".to_string()], "the rows survived");
    let res = send(
        &d.app,
        "GET",
        &format!("/notes/{note_id}"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::OK,
        "the same cookie and the same note after a resume: {}",
        res.body
    );
}

// --- module entitlements -------------------------------------------------
//
// Foundation-level only: one nest proves the gate, one child route proves the
// `/courses` exceptions, and the two WebSocket nests prove an upgrade is
// refused like any other GET. The full per-module sweep is a later lane.

use hezarfen_backend::module::Module;

/// Turning a module off refuses **its** nest and nothing else — with the same
/// cookie, and with the URL space untouched (an unmatched path inside the
/// disabled nest is still a `404`, which is what `route_layer` buys).
#[tokio::test]
async fn a_disabled_module_refuses_its_nest_and_leaves_the_rest_alone() {
    let (app, db, tenants) = common::app_and_tenants().await;
    let slug = Slug::try_new(DEMO_SLUG).unwrap();
    let cookie = login_as(&app, &db, "ada", "admin").await;

    let res = send(&app, "GET", "/meals/menus", Some(&cookie), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let mut without_meals = ModuleSet::all();
    without_meals.remove(Module::Meals);
    tenants
        .set_modules(&slug, &without_meals)
        .await
        .expect("take the meals module back");

    let res = send(&app, "GET", "/meals/menus", Some(&cookie), None).await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    assert_eq!(
        res.body,
        json!({ "error": "module disabled", "module": "meals" }),
        "the refusal body is contract"
    );

    for route in ["/auth/me", "/limits", "/settings"] {
        let res = send(&app, "GET", route, Some(&cookie), None).await;
        assert_eq!(res.status, StatusCode::OK, "{route}: {}", res.body);
    }

    let res = send(&app, "GET", "/meals/does-not-exist", Some(&cookie), None).await;
    assert_eq!(
        res.status,
        StatusCode::NOT_FOUND,
        "a disabled module must not swallow the 404: {}",
        res.body
    );

    tenants
        .set_modules(&slug, &ModuleSet::all())
        .await
        .expect("sell it back");
    let res = send(&app, "GET", "/meals/menus", Some(&cookie), None).await;
    assert_eq!(
        res.status,
        StatusCode::OK,
        "the very same cookie after re-enabling: {}",
        res.body
    );
}

/// The four route pairs mounted under `/courses` that belong to another
/// module answer for *that* module: exams off refuses
/// `POST /courses/{id}/exams` while the course itself stays reachable.
/// Every module except `module` and everything that (transitively) needs it —
/// the only shape the registry accepts now that `set_modules` validates the
/// dependency graph itself.
fn all_without(module: Module) -> ModuleSet {
    let mut set = ModuleSet::all();
    let mut pending = vec![module];
    while let Some(next) = pending.pop() {
        if set.contains(next) {
            set.remove(next);
            pending.extend(next.dependents());
        }
    }
    set
}

#[tokio::test]
async fn a_course_child_route_is_gated_by_its_own_module() {
    let (app, db, tenants) = common::app_and_tenants().await;
    let slug = Slug::try_new(DEMO_SLUG).unwrap();
    let cookie = login_as(&app, &db, "ada", "admin").await;
    let course = create_course(&app, &cookie, "Fizik").await;

    tenants
        .set_modules(&slug, &all_without(Module::Exams))
        .await
        .unwrap();

    // An empty body: the gate must answer before the payload is even parsed.
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/exams"),
        Some(&cookie),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    assert_eq!(res.body["module"], "exams");

    let res = send(
        &app,
        "GET",
        &format!("/courses/{course}"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::OK,
        "courses itself is still enabled: {}",
        res.body
    );
}

/// A WebSocket upgrade is a `GET` inside its nest, so it is gated by
/// construction — proven for both rooms rather than assumed.
#[tokio::test]
async fn a_disabled_module_refuses_its_websocket_upgrade() {
    let (app, db, tenants) = common::app_and_tenants().await;
    let slug = Slug::try_new(DEMO_SLUG).unwrap();
    let cookie = login_as(&app, &db, "ada", "admin").await;

    let mut without = all_without(Module::Exams);
    without.remove(Module::Boards);
    tenants.set_modules(&slug, &without).await.unwrap();

    for (route, module) in [("/boards/x/ws", "boards"), ("/exams/x/attempt/ws", "exams")] {
        let res = send(&app, "GET", route, Some(&cookie), None).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{route}: {}", res.body);
        assert_eq!(res.body["module"], module, "{route}");
    }
}

// --- module entitlements: the full sweep ---------------------------------
//
// The foundation tests above prove the mechanism on one nest; these prove it
// holds for *every* module, driven through the vendor's own surface, plus the
// coverage test that fails when a future nest is mounted ungated.

use hezarfen_backend::domain::builder::Builder;

const SWEEP_BUILDER_USER: &str = "operator";
const SWEEP_BUILDER_PASS: &str = "secret1";

/// A deployment with a builder account, so the sweep can sell and take back
/// modules by the same path production does (mirrors `tests/builder_api.rs`).
async fn sweep_deployment() -> (axum::Router, hezarfen_backend::database::Database, Tenants) {
    let (app, db, tenants) = common::app_and_tenants().await;
    Builder::ensure(
        Username::try_new(SWEEP_BUILDER_USER).unwrap(),
        Password::try_new(SWEEP_BUILDER_PASS).unwrap(),
        tenants.control(),
    )
    .await
    .expect("seed the builder");
    (app, db, tenants)
}

async fn sweep_builder_login(app: &axum::Router) -> String {
    let res = send(
        app,
        "POST",
        "/builder/login",
        None,
        Some(json!({ "username": SWEEP_BUILDER_USER, "password": SWEEP_BUILDER_PASS })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "builder login: {}", res.body);
    res.cookie.expect("builder cookie")
}

fn is_module_disabled(res: &common::Res) -> bool {
    res.status == StatusCode::FORBIDDEN && res.body["error"] == "module disabled"
}

/// One route per module that exists and lives in that module's nest — the
/// probe the sweep drives. GET list routes wherever the nest has one; the
/// status when the module is *on* is irrelevant (a `{id}` route may 400 or
/// 404), only "not the module refusal" is.
const MODULE_ROUTES: [(Module, &str, &str); 21] = [
    (Module::Chatbot, "GET", "/chatbot/threads"),
    (Module::Notes, "GET", "/notes"),
    (Module::Messages, "GET", "/messages"),
    (Module::Events, "GET", "/events"),
    (Module::Appointments, "GET", "/appointments/slots"),
    (Module::Courses, "GET", "/courses"),
    (Module::CourseNotes, "GET", "/course-notes"),
    (Module::Classes, "GET", "/classes"),
    // No list route of its own: sessions are listed under a course.
    (Module::Sessions, "GET", "/sessions/nope"),
    (Module::Exams, "GET", "/exams"),
    (Module::Marks, "GET", "/marks/me"),
    (Module::Meals, "GET", "/meals/menus"),
    (Module::Payments, "GET", "/payments/plans"),
    (Module::Work, "GET", "/work/me"),
    (Module::Pomodoro, "GET", "/pomodoro/me"),
    (Module::Questions, "GET", "/questions"),
    (Module::BankQuestions, "GET", "/bank-questions"),
    (Module::Attendance, "GET", "/attendance/me"),
    // Same as sessions: a subject is reached by id or through its course.
    (Module::Subjects, "GET", "/subjects/nope"),
    (Module::Homework, "GET", "/homework"),
    (Module::Boards, "GET", "/boards"),
];

/// Every one of the 21 modules, one at a time: on → not refused, taken back →
/// `403 {"error":"module disabled","module":"<name>"}` on the *same* school
/// cookie, sold back → not refused again.
#[tokio::test]
async fn every_module_gates_its_own_nest_when_the_builder_takes_it_back() {
    let (app, db, tenants) = sweep_deployment().await;
    let slug = Slug::try_new(DEMO_SLUG).unwrap();
    let cookie = login_as(&app, &db, "ada", "admin").await;
    let builder = sweep_builder_login(&app).await;

    for (module, method, path) in MODULE_ROUTES {
        let res = send(&app, method, path, Some(&cookie), None).await;
        assert!(
            !is_module_disabled(&res),
            "{module} is enabled, so {method} {path} must not be the module refusal: {} {}",
            res.status,
            res.body
        );

        let one = format!("/schools/{DEMO_SLUG}/modules/{module}");
        if module.dependents().is_empty() {
            let res = send(&app, "DELETE", &one, Some(&builder), None).await;
            assert_eq!(
                res.status,
                StatusCode::OK,
                "take back {module}: {}",
                res.body
            );
        } else {
            // The five modules something else structurally needs. This sweep
            // leaves every *other* module on, so the builder API rightly
            // refuses (`409 "<m> is required by …"`) — asserted here rather
            // than worked around, then the entitlement is written through the
            // registry call the API itself ends in.
            let res = send(&app, "DELETE", &one, Some(&builder), None).await;
            assert_eq!(
                res.status,
                StatusCode::CONFLICT,
                "{module} has dependents {:?}, so the API must refuse: {}",
                module.dependents(),
                res.body
            );
            tenants
                .set_modules(&slug, &all_without(module))
                .await
                .unwrap();
        }

        let res = send(&app, method, path, Some(&cookie), None).await;
        assert_eq!(
            res.status,
            StatusCode::FORBIDDEN,
            "{method} {path}: {}",
            res.body
        );
        assert_eq!(
            res.body,
            json!({ "error": "module disabled", "module": module.as_str() }),
            "the refusal body is contract, for {module}"
        );

        // Sold back through the API in every case: enabling never conflicts.
        let res = send(&app, "POST", &one, Some(&builder), None).await;
        assert_eq!(
            res.status,
            StatusCode::OK,
            "sell {module} back: {}",
            res.body
        );
        // `all_without` also took the module's dependents away; restore them
        // so the next module in the sweep starts from a fully sold school.
        if !module.dependents().is_empty() {
            tenants.set_modules(&slug, &ModuleSet::all()).await.unwrap();
        }
        let res = send(&app, method, path, Some(&cookie), None).await;
        assert!(
            !is_module_disabled(&res),
            "{module} is back on: {} {}",
            res.status,
            res.body
        );
    }
}

/// All four foreign-module route pairs under `/courses`, not just the one the
/// foundation test drives: each answers for its *own* module while `courses`
/// stays on.
#[tokio::test]
async fn every_course_child_route_names_its_own_module() {
    let (app, db, tenants) = common::app_and_tenants().await;
    let slug = Slug::try_new(DEMO_SLUG).unwrap();
    let cookie = login_as(&app, &db, "ada", "admin").await;
    let course = create_course(&app, &cookie, "Fizik").await;

    for (module, child) in [
        (Module::Exams, "exams"),
        (Module::Sessions, "sessions"),
        (Module::Subjects, "subjects"),
        (Module::Homework, "homework"),
    ] {
        // Three of the four are needed by another module (marks, attendance,
        // exams), so the builder API would `409` here — the set is written
        // directly, uniformly, since the API's own refusal is proven above.
        tenants
            .set_modules(&slug, &all_without(module))
            .await
            .unwrap();

        let path = format!("/courses/{course}/{child}");
        let res = send(&app, "GET", &path, Some(&cookie), None).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{path}: {}", res.body);
        assert_eq!(
            res.body,
            json!({ "error": "module disabled", "module": module.as_str() }),
            "{path} answers for its own module"
        );

        let res = send(
            &app,
            "GET",
            &format!("/courses/{course}"),
            Some(&cookie),
            None,
        )
        .await;
        assert_eq!(
            res.status,
            StatusCode::OK,
            "courses stays on while {module} is off: {}",
            res.body
        );

        tenants.set_modules(&slug, &ModuleSet::all()).await.unwrap();
    }
}

/// A school that bought nothing is still a school: it logs in, reads itself,
/// and can be told what it has — none of that lives behind a module.
#[tokio::test]
async fn a_school_with_no_modules_can_still_use_the_core_routes() {
    let (app, _db, _tenants) = sweep_deployment().await;
    let builder = sweep_builder_login(&app).await;

    let res = send(
        &app,
        "POST",
        "/schools",
        Some(&builder),
        Some(json!({
            "slug": "bare",
            "name": "Bare School",
            "admin_username": "admin",
            "admin_password": "secret1",
            "modules": [],
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["modules"], json!([]));

    let res = send(
        &app,
        "POST",
        "/auth/login",
        None,
        Some(json!({ "school": "bare", "username": "admin", "password": "secret1" })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::OK,
        "a bare school still logs in: {}",
        res.body
    );
    let cookie = res.cookie.expect("session cookie");

    for route in [
        "/auth/me",
        "/limits",
        "/settings",
        "/terms",
        "/users/me/profile",
    ] {
        let res = send(&app, "GET", route, Some(&cookie), None).await;
        assert_eq!(res.status, StatusCode::OK, "{route}: {}", res.body);
    }

    let res = send(&app, "GET", "/modules", Some(&cookie), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body, json!({ "enabled": [] }));
}

// --- module entitlements: coverage ---------------------------------------

/// Route prefixes that are deliberately ungated, each with the reason it is —
/// a nest here is one no school can be sold or refused.
const CORE_PREFIXES: [(&str, &str); 12] = [
    ("/", "the health mirror at the root"),
    ("/health", "liveness, read before any school is resolved"),
    ("/time", "the server clock, a deploy constant"),
    ("/limits", "deploy-time constants, needed to draw any form"),
    (
        "/modules",
        "the entitlement lookup itself; gating it is circular",
    ),
    ("/ai", "the AI bridge's own surface, not a school's nest"),
    (
        "/auth",
        "the door: a school must log in before anything is refused",
    ),
    (
        "/users",
        "identity and profiles, part of being a school at all",
    ),
    ("/settings", "the school's own settings"),
    (
        "/terms",
        "the academic calendar every other module hangs off",
    ),
    (
        "/builder",
        "the vendor principal, which is not a school user",
    ),
    ("/schools", "the vendor's registry surface, same principal"),
];

/// Every module's nest prefix. Written down rather than derived: the point is
/// to catch a nest that was mounted without a gate, and a derived list would
/// be derived from the same code it is checking.
const MODULE_PREFIXES: [(Module, &str); 21] = [
    (Module::Chatbot, "/chatbot"),
    (Module::Notes, "/notes"),
    (Module::Messages, "/messages"),
    (Module::Events, "/events"),
    (Module::Appointments, "/appointments"),
    (Module::Courses, "/courses"),
    (Module::CourseNotes, "/course-notes"),
    (Module::Classes, "/classes"),
    (Module::Sessions, "/sessions"),
    (Module::Exams, "/exams"),
    (Module::Marks, "/marks"),
    (Module::Meals, "/meals"),
    (Module::Payments, "/payments"),
    (Module::Work, "/work"),
    (Module::Pomodoro, "/pomodoro"),
    (Module::Questions, "/questions"),
    (Module::BankQuestions, "/bank-questions"),
    (Module::Attendance, "/attendance"),
    (Module::Subjects, "/subjects"),
    (Module::Homework, "/homework"),
    (Module::Boards, "/boards"),
];

/// `path` is inside `prefix` — segment-wise, so `/course-notes` is not inside
/// `/courses` and `/` is only ever itself.
fn under(path: &str, prefix: &str) -> bool {
    if prefix == "/" {
        return path == "/";
    }
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

/// The published surface is exactly: the ungated core, plus the 21 gated
/// nests. A nest added to `build_router` without a gate matches neither list
/// and fails here by name — which is the only way an ungated nest is ever
/// noticed, since nothing else in the suite knows a new route exists.
///
/// corner-cut: it reads the OpenAPI document, so it sees only routes mounted
/// with `.routes(routes!(…))` — a bare `.route()` is invisible to it. Every
/// nest in `build_router` is documented today, so the ceiling is an
/// undocumented route — which is already against this repo's convention.
#[tokio::test]
async fn every_published_route_is_either_core_or_inside_a_gated_module_nest() {
    let app = mem_app().await;
    let spec = send(&app, "GET", "/api-docs/openapi.json", None, None).await;
    assert_eq!(spec.status, StatusCode::OK);
    let paths = spec.body["paths"].as_object().expect("the spec has paths");
    assert!(
        paths.len() > 100,
        "the whole spec, not a stub: {}",
        paths.len()
    );

    let mut ungated = Vec::new();
    for path in paths.keys() {
        let core: Vec<&str> = CORE_PREFIXES
            .iter()
            .filter(|(prefix, _)| under(path, prefix))
            .map(|(prefix, _)| *prefix)
            .collect();
        let gated: Vec<&str> = MODULE_PREFIXES
            .iter()
            .filter(|(_, prefix)| under(path, prefix))
            .map(|(_, prefix)| *prefix)
            .collect();
        match core.len() + gated.len() {
            1 => {}
            0 => ungated.push(format!(
                "{path}: in no core prefix and no gated module nest — mount it under a module (and gate it), or add it to CORE_PREFIXES with its reason"
            )),
            _ => ungated.push(format!("{path}: ambiguous, matches {core:?} and {gated:?}")),
        }
    }
    assert!(
        ungated.is_empty(),
        "ungated routes:\n{}",
        ungated.join("\n")
    );
}

/// The catalog a client draws the switchboard from lists every module the
/// server can refuse — a module missing here is one nobody can buy.
#[tokio::test]
async fn the_catalog_lists_every_module() {
    let app = mem_app().await;
    let res = send(&app, "GET", "/modules/catalog", None, None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let listed: Vec<&str> = res.body["modules"]
        .as_array()
        .expect("modules")
        .iter()
        .map(|m| m["module"].as_str().expect("name"))
        .collect();
    assert_eq!(listed.len(), Module::ALL.len());
    for module in Module::ALL {
        assert!(
            listed.contains(&module.as_str()),
            "{module} is not for sale"
        );
    }
}
