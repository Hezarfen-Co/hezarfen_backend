//! The public-profile surface: who may read a profile, what it may never
//! carry, the display-name fallback, the derived stats, and the avatar's
//! blob discipline.
//!
//! Every gate here is a *read* gate on data that is otherwise school-wide, so
//! each test is written to fail if its guard is deleted: the parent tests
//! assert the 403 *and* the 200 that proves the read would otherwise work, and
//! the blob test counts what is actually on disk rather than trusting the API's
//! own answer.

mod common;

use axum::Router;
use axum::http::StatusCode;
use common::{
    app_and_db, create_course, create_exam_with, create_homework, create_subject, enroll, id_of,
    items, login, login_as, me_id, send, set_role, ABSENT_ID,
};
use hezarfen_backend::constant::{MAX_BIO_LEN, MAX_DISPLAY_NAME_LEN};
use hezarfen_backend::database::Database;
use hezarfen_backend::domain::timestamp::Timestamp;
use serde_json::{Value, json};

const BOUNDARY: &str = "multipart/form-data; boundary=hezarfen-test-boundary";

/// Upload `bytes` as the caller's avatar; returns the status and parsed body.
async fn upload_avatar(
    app: &Router,
    cookie: &str,
    content_type: &str,
    bytes: &[u8],
) -> (StatusCode, Value) {
    let (status, _, body) = common::send_raw(
        app,
        "POST",
        "/users/me/avatar",
        Some(cookie),
        Some(BOUNDARY),
        common::multipart_file("pic.png", content_type, bytes),
    )
    .await;
    let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, body)
}

/// How many blobs in the test files dir hold exactly `bytes`. The dir is shared
/// by every app in this binary, so a payload no other test writes is the only
/// concurrency-safe way to count one test's own blobs.
fn blobs_holding(bytes: &[u8]) -> usize {
    std::fs::read_dir(common::blob_dir())
        .expect("files dir")
        .filter_map(|entry| std::fs::read(entry.expect("dir entry").path()).ok())
        .filter(|found| found == bytes)
        .count()
}

/// Tie `student_id` to `parent_id` (admin only), asserting the tie landed.
async fn link_student(app: &Router, admin: &str, parent_id: &str, student_id: &str) {
    let res = send(
        app,
        "POST",
        &format!("/users/{parent_id}/students"),
        Some(admin),
        Some(json!({ "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "link student: {}", res.body);
}

async fn profile(app: &Router, cookie: &str, id: &str) -> common::Res {
    send(
        app,
        "GET",
        &format!("/users/{id}/profile"),
        Some(cookie),
        None,
    )
    .await
}

/// A profile is the *public* half of a user row: a peer reads it, and the
/// contact fields that `GET /users/{id}` gates behind teacher+ are absent from
/// it entirely. `badges` is pass 2 — no empty key ships early.
#[tokio::test]
async fn a_peer_profile_carries_no_contact_fields() {
    let (app, _db) = app_and_db().await;
    let ada = login(&app, "ada").await;
    let ada_id = me_id(&app, &ada).await;
    let peer = login(&app, "peer").await;

    let res = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&ada),
        Some(json!({
            "name": "Ada",
            "surname": "Lovelace",
            "email": "ada@example.com",
            "phone": "+90 555 123 45 67",
            "birth_date": "1990-01-02",
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let seen = profile(&app, &peer, &ada_id).await;
    assert_eq!(seen.status, StatusCode::OK, "{}", seen.body);
    assert_eq!(seen.body["username"], "ada");
    for private in ["email", "phone", "birth_date"] {
        assert!(
            seen.body.get(private).is_none(),
            "{private} must not ride on a profile: {}",
            seen.body
        );
    }
    // Badges are public, unlike the contact fields — and an account that has
    // earned none says so with an empty list, never with a missing key.
    assert_eq!(seen.body["badges"], json!([]), "{}", seen.body);
}

/// A parent is an observer of their own children and of nobody else: the
/// unlinked student is a 403, the linked one a 200, and their own row a 200.
/// The 200s are the half that proves the 403 is the gate biting, not the read
/// being broken for everyone.
#[tokio::test]
async fn a_parent_reads_only_their_own_and_their_linked_students() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let parent = login_as(&app, &db, "mom", "parent").await;
    let parent_id = me_id(&app, &parent).await;
    let mine = login(&app, "kid").await;
    let mine_id = me_id(&app, &mine).await;
    let stranger = login(&app, "stranger").await;
    let stranger_id = me_id(&app, &stranger).await;

    link_student(&app, &admin, &parent_id, &mine_id).await;

    let blocked = profile(&app, &parent, &stranger_id).await;
    assert_eq!(
        blocked.status,
        StatusCode::FORBIDDEN,
        "an unlinked student: {}",
        blocked.body
    );
    let linked = profile(&app, &parent, &mine_id).await;
    assert_eq!(linked.status, StatusCode::OK, "{}", linked.body);
    let own = profile(&app, &parent, &parent_id).await;
    assert_eq!(own.status, StatusCode::OK, "{}", own.body);
    let own_me = send(&app, "GET", "/users/me/profile", Some(&parent), None).await;
    assert_eq!(own_me.status, StatusCode::OK, "{}", own_me.body);

    // The avatar bytes ride the same gate as the profile they hang on.
    let (status, _, _) = common::send_raw(
        &app,
        "GET",
        &format!("/users/{stranger_id}/avatar"),
        Some(&parent),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "avatar of an unlinked student"
    );
}

/// The link row alone is not the grant: the target's live role is re-read per
/// call, so a linked student promoted out of the student role stops being
/// readable — a stale link cannot leave a colleague's profile open to a parent.
#[tokio::test]
async fn promoting_a_linked_student_closes_the_parent_read() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let parent = login_as(&app, &db, "mom", "parent").await;
    let parent_id = me_id(&app, &parent).await;
    let kid = login(&app, "kid").await;
    let kid_id = me_id(&app, &kid).await;

    link_student(&app, &admin, &parent_id, &kid_id).await;
    let before = profile(&app, &parent, &kid_id).await;
    assert_eq!(before.status, StatusCode::OK, "{}", before.body);

    set_role(&db, "kid", "teacher").await;

    let after = profile(&app, &parent, &kid_id).await;
    assert_eq!(
        after.status,
        StatusCode::FORBIDDEN,
        "a promoted-out student must stop being readable: {}",
        after.body
    );
}

/// `display_name` is the self-chosen label, the joined legal name is the
/// fallback, and an account carrying neither reads `null` — never the username,
/// never an empty string.
#[tokio::test]
async fn display_name_falls_back_to_the_legal_name_then_null() {
    let (app, _db) = app_and_db().await;
    let ada = login(&app, "ada").await;

    let res = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&ada),
        Some(json!({ "name": "Ada", "surname": "Lovelace", "display_name": "Countess" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let mine = send(&app, "GET", "/users/me/profile", Some(&ada), None).await;
    assert_eq!(
        mine.body["display_name"], "Countess",
        "the chosen name wins"
    );

    let res = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&ada),
        Some(json!({ "display_name": "" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let mine = send(&app, "GET", "/users/me/profile", Some(&ada), None).await;
    assert_eq!(mine.body["display_name"], "Ada Lovelace", "the legal name");

    let res = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&ada),
        Some(json!({ "name": "", "surname": "" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let mine = send(&app, "GET", "/users/me/profile", Some(&ada), None).await;
    assert!(
        mine.body["display_name"].is_null(),
        "nothing to show: {}",
        mine.body
    );
}

/// The embedded person refs resolve the display name in the *same* three steps
/// the profile does — the rule has one spelling, and this asserts the two
/// surfaces agree at every step. It shipped broken because no test had ever set
/// a `display_name` that differs from the legal-name join: the person ref
/// skipped the stored name outright, so anyone who chose one was still shown
/// their legal name by every list that embeds a person.
#[tokio::test]
async fn embedded_person_refs_show_the_chosen_name_like_the_profile() {
    let (app, db) = app_and_db().await;
    // The sender is staff: messaging is upward-only below teacher, and this
    // test needs a send that lands, not the role rule.
    let ali = login_as(&app, &db, "ali", "teacher").await;
    let ayse = login(&app, "ayse").await;
    let ayse_id = me_id(&app, &ayse).await;

    // What `ali` sees of `ayse` in a person ref, and what her own profile says.
    let as_person_ref = async || {
        let sent = send(&app, "GET", "/messages?folder=sent", Some(&ali), None).await;
        assert_eq!(sent.status, StatusCode::OK, "{}", sent.body);
        items(&sent.body)[0]["recipient"]["display_name"].clone()
    };
    let as_profile = async || {
        let profile = send(
            &app,
            "GET",
            &format!("/users/{ayse_id}/profile"),
            Some(&ali),
            None,
        )
        .await;
        assert_eq!(profile.status, StatusCode::OK, "{}", profile.body);
        profile.body["display_name"].clone()
    };

    let sent = send(
        &app,
        "POST",
        "/messages",
        Some(&ali),
        Some(json!({ "recipient_id": ayse_id, "subject": "ödev", "body": "yarın" })),
    )
    .await;
    assert_eq!(sent.status, StatusCode::CREATED, "{}", sent.body);

    // Neither name: `null` on both surfaces, never the username.
    assert!(as_person_ref().await.is_null(), "nothing to show yet");
    assert_eq!(as_person_ref().await, as_profile().await);

    // Legal name only: the join, on both.
    let res = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&ayse),
        Some(json!({ "name": "Ayşe", "surname": "Yılmaz" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(as_person_ref().await, "Ayşe Yılmaz");
    assert_eq!(as_person_ref().await, as_profile().await);

    // A chosen name wins over the legal one — the whole point of choosing it.
    let res = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&ayse),
        Some(json!({ "display_name": "Ada" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        as_person_ref().await,
        "Ada",
        "the legal name must not leak back through a person ref"
    );
    assert_eq!(
        as_person_ref().await,
        as_profile().await,
        "the two surfaces resolve one rule and cannot disagree"
    );
    // The office record is untouched by any of this.
    let office = send(&app, "GET", "/auth/me", Some(&ayse), None).await;
    assert_eq!(office.body["name"], "Ayşe");
    assert_eq!(office.body["surname"], "Yılmaz");
}

/// The read-time badge backstop, and the write it must not make.
///
/// A counter is moved behind the API's back — exactly what a lost `badge::sync`
/// leaves behind, since every counter writer logs and swallows that error. The
/// first profile read must heal it and serve the badges with their stamps; the
/// second must serve the identical shelf and write *nothing*, which is what the
/// `badge_award` write probe counts. Zero extra writes on a read is the whole
/// point: a profile is a read endpoint.
#[tokio::test]
async fn a_profile_read_heals_a_missed_badge_then_writes_nothing() {
    let (app, db) = app_and_db().await;
    let ada = login(&app, "ada").await;
    let ada_id = me_id(&app, &ada).await;

    // The award table watched through its rows: the read must not add, remove
    // or alter one. (The old engine counted writes with a `DEFINE EVENT`;
    // Postgres has no triggers to hang that on, and the write the guard
    // bites on — an insert, or a re-stamp of `earned_at` — always changes
    // what the rows hold.)
    let awards = || async {
        let rows: Vec<(uuid::Uuid, String, Option<i64>)> =
            sqlx::query_as(
                "SELECT app_user, badge, earned_at FROM badge_award ORDER BY app_user, badge",
            )
            .fetch_all(&db)
            .await
            .unwrap();
        rows
    };
    assert!(awards().await.is_empty(), "nothing has been awarded yet");

    // A counter is moved behind the API's back — exactly what a lost
    // `badge::sync` leaves behind, since every counter writer logs and
    // swallows that error.
    sqlx::query("UPDATE app_user SET homework_submitted_total = 10 WHERE id = $1")
        .bind(hezarfen_backend::domain::user::UserId::from_key(&ada_id))
        .execute(&db)
        .await
        .unwrap();

    let healed = profile(&app, &ada, &ada_id).await;
    assert_eq!(healed.status, StatusCode::OK, "{}", healed.body);
    let ids: Vec<&str> = healed.body["badges"]
        .as_array()
        .expect("badges array")
        .iter()
        .map(|badge| badge["id"].as_str().expect("badge id"))
        .collect();
    assert_eq!(ids, vec!["homework_submitted_1", "homework_submitted_10"]);
    assert!(
        healed.body["badges"][0]["earned_at"].as_i64().unwrap_or(0) > 0,
        "a badge carries when it was earned: {}",
        healed.body
    );
    assert_eq!(healed.body["stats"]["homework_submitted_total"], 10);
    let after_heal = awards().await;
    assert!(!after_heal.is_empty(), "the heal wrote the awards");

    // Steady state: everything earned is already held, so this read must not
    // touch the award table at all.
    let again = profile(&app, &ada, &ada_id).await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    assert_eq!(again.body["badges"], healed.body["badges"], "same shelf");
    assert_eq!(awards().await, after_heal, "a profile read wrote something");
}

/// A fresh account reads a true `0` on every counter — never `null`, never an
/// absent key a client would have to guard. Both kinds: the four derived ones
/// have no rows behind them, and the stored ones have no columns on the row.
#[tokio::test]
async fn fresh_stats_read_zero_on_every_field() {
    let (app, _db) = app_and_db().await;
    let fresh = login(&app, "fresh").await;

    let mine = send(&app, "GET", "/users/me/profile", Some(&fresh), None).await;
    assert_eq!(mine.status, StatusCode::OK, "{}", mine.body);
    let stats = mine.body["stats"].as_object().expect("stats object");
    for field in [
        "pomodoro_sessions",
        "pomodoro_focus_ms",
        "courses",
        "classes",
        // The stored lifetime counters: a row that predates the columns carries
        // none of them, and absent must still read as a numeric 0.
        "homework_submitted_total",
        "homework_on_time_total",
        "exam_sat_total",
        "pomodoro_finished_total",
        "pomodoro_focus_ms_total",
        "marks_given_total",
        "lessons_held_total",
        "pool_approved_total",
        "pool_published_total",
        "lessons_attended_total",
        "high_mark_total",
        // A longest, not a running total — the key still carries `_total`
        // because that suffix is how a client joins a badge `stat` to a stat.
        "study_streak_total",
    ] {
        assert_eq!(
            stats.get(field).and_then(Value::as_i64),
            Some(0),
            "stats.{field} must be a numeric 0: {:?}",
            stats.get(field)
        );
    }
    // A 17th key is a new disclosure decision, not just a new number: every
    // magnitude here is the owner's true total for *every* reader, gate or no
    // gate (see `profile_of`). Adding one means re-reading that paragraph and
    // saying so in the README before this list grows.
    assert_eq!(stats.len(), 16, "an unlisted counter shipped: {stats:?}");
    assert!(mine.body["avatar"].is_null());
    assert_eq!(mine.body["classes"], json!([]));
    assert_eq!(mine.body["courses"], json!([]));
}

/// Upload, read back the exact bytes and the declared type, delete, gone.
#[tokio::test]
async fn an_avatar_round_trips_and_delete_makes_it_a_404() {
    let (app, db) = app_and_db().await;
    let ada = login(&app, "ada").await;
    let ada_id = me_id(&app, &ada).await;
    let teacher = login_as(&app, &db, "teach", "teacher").await;
    let bytes = b"ROUNDTRIP-\x89PNG";

    let (status, _, _) = common::send_raw(
        &app,
        "DELETE",
        "/users/me/avatar",
        Some(&ada),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "nothing to delete yet");

    let (status, body) = upload_avatar(&app, &ada, "image/png", bytes).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["content_type"], "image/png");
    assert_eq!(body["size"], bytes.len());

    let meta = send(&app, "GET", "/users/me/profile", Some(&ada), None).await;
    assert_eq!(meta.body["avatar"]["content_type"], "image/png");
    assert_eq!(meta.body["avatar"]["size"], bytes.len());

    let (status, headers, served) = common::send_raw(
        &app,
        "GET",
        &format!("/users/{ada_id}/avatar"),
        Some(&teacher),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(served, bytes, "the exact bytes come back");

    let (status, _, _) = common::send_raw(
        &app,
        "DELETE",
        "/users/me/avatar",
        Some(&ada),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _, _) = common::send_raw(
        &app,
        "GET",
        &format!("/users/{ada_id}/avatar"),
        Some(&teacher),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the picture is gone");
    let meta = send(&app, "GET", "/users/me/profile", Some(&ada), None).await;
    assert!(meta.body["avatar"].is_null());
    assert_eq!(blobs_holding(bytes), 0, "the blob left disk too");
}

/// The self alias reads the caller's own picture. `/me/avatar` is a static
/// segment, so it beats `/{id}/avatar` in the router: without an explicit `GET`
/// the obvious route answers a bodyless `405` and a client is forced to learn
/// its own id first.
#[tokio::test]
async fn the_caller_reads_their_own_avatar_without_knowing_their_id() {
    let (app, _db) = app_and_db().await;
    let ada = login(&app, "ada").await;
    let bytes = b"SELFALIAS-\x89PNG";

    let (status, _, _) = common::send_raw(
        &app,
        "GET",
        "/users/me/avatar",
        Some(&ada),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "no picture yet, not a 405");

    let (status, body) = upload_avatar(&app, &ada, "image/png", bytes).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, headers, served) = common::send_raw(
        &app,
        "GET",
        "/users/me/avatar",
        Some(&ada),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(served, bytes, "the exact bytes, same as the id route");
}

/// SVG is not a raster: it scripts, and an avatar renders inline to the whole
/// school. The allowlist keeps it out.
#[tokio::test]
async fn an_svg_avatar_is_rejected() {
    let (app, _db) = app_and_db().await;
    let ada = login(&app, "ada").await;
    let svg = b"SVGREJECT<svg xmlns=\"http://www.w3.org/2000/svg\"><script/></svg>";

    let (status, body) = upload_avatar(&app, &ada, "image/svg+xml", svg).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let mine = send(&app, "GET", "/users/me/profile", Some(&ada), None).await;
    assert!(mine.body["avatar"].is_null(), "{}", mine.body);
    assert_eq!(blobs_holding(svg), 0, "nothing reached disk");
}

/// Deleting someone else's picture is moderation, so it is admin-only; the
/// admin's delete succeeding is what proves the 403 is the role gate and not a
/// broken route.
#[tokio::test]
async fn only_an_admin_deletes_someone_elses_avatar() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let teacher = login_as(&app, &db, "teach", "teacher").await;
    let ada = login(&app, "ada").await;
    let ada_id = me_id(&app, &ada).await;
    let bytes = b"MODERATION-\x89PNG";

    let (status, body) = upload_avatar(&app, &ada, "image/png", bytes).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    for (who, label) in [(&teacher, "teacher"), (&ada, "the owner via /{id}")] {
        let (status, _, _) = common::send_raw(
            &app,
            "DELETE",
            &format!("/users/{ada_id}/avatar"),
            Some(who),
            None,
            Vec::new(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{label} is not an admin");
    }

    let (status, _, _) = common::send_raw(
        &app,
        "DELETE",
        &format!("/users/{ada_id}/avatar"),
        Some(&admin),
        None,
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let mine = send(&app, "GET", "/users/me/profile", Some(&ada), None).await;
    assert!(mine.body["avatar"].is_null(), "{}", mine.body);
    assert_eq!(blobs_holding(bytes), 0);
}

/// Replacing an avatar takes the old blob off disk. No route deletes a user, so
/// a blob stranded here is stranded forever — this counts what is actually on
/// disk, not what the API claims.
#[tokio::test]
async fn replacing_an_avatar_leaves_exactly_one_blob() {
    let (app, _db) = app_and_db().await;
    let ada = login(&app, "ada").await;
    let first = b"HYGIENE-FIRST-BLOB";
    let second = b"HYGIENE-SECOND-BLOB";

    let (status, body) = upload_avatar(&app, &ada, "image/png", first).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(blobs_holding(first), 1, "the first picture is on disk");

    let (status, body) = upload_avatar(&app, &ada, "image/jpeg", second).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        blobs_holding(first),
        0,
        "the replaced picture must leave disk — nothing ever collects it"
    );
    assert_eq!(blobs_holding(second), 1, "and exactly one replaces it");

    let mine = send(&app, "GET", "/users/me/profile", Some(&ada), None).await;
    assert_eq!(mine.body["avatar"]["content_type"], "image/jpeg");
    assert_eq!(mine.body["avatar"]["size"], second.len());
}

/// The course block follows the owner's **live** role, not the `creator` /
/// `teachers` columns: those are historical and no demotion sweeps them, so a
/// demoted ex-teacher lists what they are enrolled in, like any other student.
#[tokio::test]
async fn a_demoted_teacher_lists_enrolled_courses_not_created_ones() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "boss", "admin").await;
    let ex = login_as(&app, &db, "exteach", "teacher").await;
    let staff = login_as(&app, &db, "coach", "teacher").await;
    let ex_id = me_id(&app, &ex).await;

    create_course(&app, &ex, "Owned").await;
    let joined = create_course(&app, &staff, "Joined").await;

    let as_teacher = send(&app, "GET", "/users/me/profile", Some(&ex), None).await;
    assert_eq!(as_teacher.body["courses"][0]["title"], "Owned");
    assert_eq!(as_teacher.body["stats"]["courses"], 1);

    set_role(&db, "exteach", "student").await;
    enroll(&app, &staff, &joined, &ex_id).await;

    let after = profile(&app, &admin, &ex_id).await;
    assert_eq!(after.status, StatusCode::OK, "{}", after.body);
    let titles: Vec<&str> = after.body["courses"]
        .as_array()
        .expect("courses array")
        .iter()
        .map(|course| course["title"].as_str().expect("title"))
        .collect();
    assert_eq!(titles, ["Joined"], "the course they created is not theirs");
    assert_eq!(after.body["stats"]["courses"], 1);
    assert_eq!(after.body["role"], "student");
}

/// `""` clears either public field; both are length-bounded, and the bound is
/// the one `GET /limits` publishes.
#[tokio::test]
async fn display_name_and_bio_clear_on_blank_and_are_bounded() {
    let (app, _db) = app_and_db().await;
    let ada = login(&app, "ada").await;

    let res = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&ada),
        Some(json!({ "display_name": "Countess", "bio": "hi" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    // The same pair rides every user-shaped response, not just the profile.
    assert_eq!(res.body["display_name"], "Countess");
    assert_eq!(res.body["bio"], "hi");

    let res = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&ada),
        Some(json!({ "display_name": "", "bio": "" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let mine = send(&app, "GET", "/users/me/profile", Some(&ada), None).await;
    assert!(mine.body["display_name"].is_null(), "{}", mine.body);
    assert!(mine.body["bio"].is_null(), "{}", mine.body);

    let over = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&ada),
        Some(json!({ "display_name": "x".repeat(MAX_DISPLAY_NAME_LEN + 1) })),
    )
    .await;
    assert_eq!(over.status, StatusCode::BAD_REQUEST, "{}", over.body);

    let over = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&ada),
        Some(json!({ "bio": "x".repeat(MAX_BIO_LEN + 1) })),
    )
    .await;
    assert_eq!(over.status, StatusCode::BAD_REQUEST, "{}", over.body);

    // The bound holds exactly at the limit, so the reject above is the bound
    // and not an off-by-one that also refuses a legal value.
    let ok = send(
        &app,
        "PATCH",
        "/users/me",
        Some(&ada),
        Some(json!({ "display_name": "x".repeat(MAX_DISPLAY_NAME_LEN) })),
    )
    .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
}

/// A teacher who runs two courses, a student enrolled in one of them, a
/// student enrolled in neither, and a manager — the cast every course-block
/// test needs.
struct School {
    app: Router,
    teacher: String,
    teacher_id: String,
    peer: String,
    stranger: String,
    manager: String,
    secret: String,
}

async fn school() -> School {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "boss", "manager").await;
    let teacher = login_as(&app, &db, "teach", "teacher").await;
    let teacher_id = me_id(&app, &teacher).await;
    let peer = login(&app, "peer").await;
    let peer_id = me_id(&app, &peer).await;
    let stranger = login(&app, "nosy").await;

    let secret = create_course(&app, &teacher, "Secret Club").await;
    let shared = create_course(&app, &teacher, "Shared Math").await;
    enroll(&app, &teacher, &shared, &peer_id).await;

    School {
        app,
        teacher,
        teacher_id,
        peer,
        stranger,
        manager,
        secret,
    }
}

/// The titles a profile's course block hands `cookie`.
async fn course_titles(app: &Router, cookie: &str, id: &str) -> Vec<String> {
    let res = profile(app, cookie, id).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    res.body["courses"]
        .as_array()
        .expect("courses array")
        .iter()
        .map(|course| course["title"].as_str().expect("title").to_string())
        .collect()
}

/// The leak this filter closes: a course a reader is 403'd from at
/// `GET /courses/{id}` used to arrive, title and all, off any teacher's
/// profile. The 403 is asserted in the same test so the two answers cannot
/// drift apart unnoticed.
#[tokio::test]
async fn a_profile_never_names_a_course_the_reader_is_403d_from() {
    let s = school().await;

    let direct = send(
        &s.app,
        "GET",
        &format!("/courses/{}", s.secret),
        Some(&s.stranger),
        None,
    )
    .await;
    assert_eq!(direct.status, StatusCode::FORBIDDEN, "{}", direct.body);

    let seen = profile(&s.app, &s.stranger, &s.teacher_id).await;
    assert_eq!(seen.status, StatusCode::OK, "{}", seen.body);
    assert_eq!(seen.body["courses"], json!([]));
    assert!(
        !seen.body.to_string().contains("Secret Club"),
        "the profile leaked a course title the reader cannot read: {}",
        seen.body
    );
}

/// The intersection is per course, not all-or-nothing: sharing one course
/// reveals that one and nothing else.
#[tokio::test]
async fn a_peer_sees_only_the_course_they_share() {
    let s = school().await;
    assert_eq!(
        course_titles(&s.app, &s.peer, &s.teacher_id).await,
        ["Shared Math"]
    );
}

/// Manager+ reads any course directly, so the filter has nothing to withhold.
#[tokio::test]
async fn a_manager_reads_the_course_block_unfiltered() {
    let s = school().await;
    let mut titles = course_titles(&s.app, &s.manager, &s.teacher_id).await;
    titles.sort();
    assert_eq!(titles, ["Secret Club", "Shared Math"]);
}

/// Your own profile is never cut — both routes onto it.
#[tokio::test]
async fn your_own_profile_shows_your_whole_course_list() {
    let s = school().await;
    let mut titles = course_titles(&s.app, &s.teacher, &s.teacher_id).await;
    titles.sort();
    assert_eq!(titles, ["Secret Club", "Shared Math"]);

    let mine = send(&s.app, "GET", "/users/me/profile", Some(&s.teacher), None).await;
    assert_eq!(mine.body["courses"].as_array().expect("courses").len(), 2);
}

/// `stats.courses` is the owner's true total, deliberately *not* the length of
/// the filtered block: it is the motivational counter, and a per-reader number
/// would be meaningless. A magnitude names no course.
#[tokio::test]
async fn stats_courses_stays_the_owners_true_total() {
    let s = school().await;

    let stranger = profile(&s.app, &s.stranger, &s.teacher_id).await;
    assert_eq!(stranger.body["courses"], json!([]));
    assert_eq!(stranger.body["stats"]["courses"], 2);

    let peer = profile(&s.app, &s.peer, &s.teacher_id).await;
    assert_eq!(peer.body["courses"].as_array().expect("courses").len(), 1);
    assert_eq!(peer.body["stats"]["courses"], 2);
}

// --- The class block ------------------------------------------------------
//
// `GET /classes/{id}` is teacher+ and `GET /classes/user/{id}` is teacher-or-
// linked-parent, so a class's name and grade are not school-wide reads. The
// profile's class block used to hand both to any authenticated peer — a plain
// student sufficed. It now holds the same bar, all-or-nothing (there is no
// "the class we share"), and the drift guard at the end compares the two
// surfaces reader by reader rather than trusting either alone.

/// A manager, a teacher, a linked parent, the student who is in the class, and
/// a nosy fellow student who is in nothing.
struct Section {
    app: Router,
    manager: String,
    teacher: String,
    parent: String,
    peer: String,
    student: String,
    student_id: String,
}

async fn section() -> Section {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "chief", "admin").await;
    let manager = login_as(&app, &db, "boss", "manager").await;
    let teacher = login_as(&app, &db, "teach", "teacher").await;
    let parent = login_as(&app, &db, "mom", "parent").await;
    let parent_id = me_id(&app, &parent).await;
    let peer = login(&app, "nosy").await;
    let student = login(&app, "kid").await;
    let student_id = me_id(&app, &student).await;

    let class = create_class(&app, &manager, "9-A", "9").await;
    add_member(&app, &manager, &class, &student_id).await;
    link_student(&app, &admin, &parent_id, &student_id).await;

    Section {
        app,
        manager,
        teacher,
        parent,
        peer,
        student,
        student_id,
    }
}

async fn create_class(app: &Router, manager: &str, name: &str, grade: &str) -> String {
    let res = send(
        app,
        "POST",
        "/classes",
        Some(manager),
        Some(json!({ "name": name, "grade": grade })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "create class: {}",
        res.body
    );
    id_of(&res.body["class"])
}

async fn add_member(app: &Router, manager: &str, class: &str, user_id: &str) {
    let res = send(
        app,
        "POST",
        &format!("/classes/{class}/members"),
        Some(manager),
        Some(json!({ "user_id": user_id })),
    )
    .await;
    assert!(res.status.is_success(), "add member: {}", res.body);
}

/// The class names a profile's class block hands `cookie`.
async fn class_names(app: &Router, cookie: &str, id: &str) -> Vec<String> {
    let res = profile(app, cookie, id).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    res.body["classes"]
        .as_array()
        .expect("classes array")
        .iter()
        .map(|class| class["name"].as_str().expect("name").to_string())
        .collect()
}

/// The class ids off the same block, sorted — the drift guard's left half.
async fn profile_class_ids(app: &Router, cookie: &str, id: &str) -> Vec<String> {
    let res = profile(app, cookie, id).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let mut ids: Vec<String> = res.body["classes"]
        .as_array()
        .expect("classes array")
        .iter()
        .map(id_of)
        .collect();
    ids.sort();
    ids
}

/// The class ids the *dedicated* routes hand that same reader for that same
/// subject — `None` when they are refused outright. Your own classes are
/// `GET /classes/me`; anybody else's are `GET /classes/user/{id}`.
async fn route_class_ids(app: &Router, cookie: &str, id: &str, own: bool) -> Option<Vec<String>> {
    let path = match own {
        true => "/classes/me".to_string(),
        false => format!("/classes/user/{id}"),
    };
    let res = send(app, "GET", &path, Some(cookie), None).await;
    if res.status == StatusCode::FORBIDDEN {
        return None;
    }
    assert_eq!(res.status, StatusCode::OK, "GET {path}: {}", res.body);
    let mut ids: Vec<String> = items(&res.body).iter().map(id_of).collect();
    ids.sort();
    Some(ids)
}

/// The leak this closes: a class a reader is `403`'d from at
/// `GET /classes/user/{id}` used to arrive off that student's profile, name and
/// grade and all, for any authenticated account. The 403 is asserted in the
/// same test so the two answers cannot drift apart unnoticed.
#[tokio::test]
async fn a_profile_never_names_a_class_the_reader_is_403d_from() {
    let s = section().await;

    let direct = send(
        &s.app,
        "GET",
        &format!("/classes/user/{}", s.student_id),
        Some(&s.peer),
        None,
    )
    .await;
    assert_eq!(direct.status, StatusCode::FORBIDDEN, "{}", direct.body);

    let seen = profile(&s.app, &s.peer, &s.student_id).await;
    assert_eq!(seen.status, StatusCode::OK, "{}", seen.body);
    assert_eq!(seen.body["classes"], json!([]));
    assert!(
        !seen.body.to_string().contains("9-A"),
        "the profile leaked a class the reader cannot read: {}",
        seen.body
    );
}

/// The half that proves the gate is the gate and not a broken read: everyone
/// who may ask the class routes directly still gets the block.
#[tokio::test]
async fn teacher_parent_manager_and_the_owner_still_read_the_class_block() {
    let s = section().await;

    assert_eq!(
        class_names(&s.app, &s.teacher, &s.student_id).await,
        ["9-A"]
    );
    assert_eq!(
        class_names(&s.app, &s.manager, &s.student_id).await,
        ["9-A"]
    );
    assert_eq!(class_names(&s.app, &s.parent, &s.student_id).await, ["9-A"]);
    assert_eq!(
        class_names(&s.app, &s.student, &s.student_id).await,
        ["9-A"]
    );

    let mine = send(&s.app, "GET", "/users/me/profile", Some(&s.student), None).await;
    assert_eq!(mine.status, StatusCode::OK, "{}", mine.body);
    assert_eq!(mine.body["classes"][0]["name"], "9-A");
    assert_eq!(mine.body["classes"][0]["grade"], "9");
}

/// `stats.classes` is the owner's true total, deliberately *not* the length of
/// the gated block — the same call already made for `stats.courses`: it is the
/// motivational counter, and a magnitude names no class.
#[tokio::test]
async fn stats_classes_stays_the_owners_true_total() {
    let s = section().await;

    let peer = profile(&s.app, &s.peer, &s.student_id).await;
    assert_eq!(peer.body["classes"], json!([]));
    assert_eq!(peer.body["stats"]["classes"], 1);

    let teacher = profile(&s.app, &s.teacher, &s.student_id).await;
    assert_eq!(teacher.body["stats"]["classes"], 1);
}

/// The forcing function: for every reader, the profile's class block must say
/// exactly what the dedicated class route says to that same reader — an empty
/// block where the route is a `403`, the identical id set where it is a `200`.
/// Whichever surface moves first, this fails. (One class, so the block's
/// `max_profile_classes` truncation cannot make the two disagree honestly.)
#[tokio::test]
async fn the_profile_class_block_agrees_with_the_class_route() {
    let s = section().await;
    let readers = [
        ("peer", &s.peer, false),
        ("teacher", &s.teacher, false),
        ("manager", &s.manager, false),
        ("parent", &s.parent, false),
        ("owner", &s.student, true),
    ];

    let mut refused = 0;
    let mut served = 0;
    for (who, cookie, own) in readers {
        let route = route_class_ids(&s.app, cookie, &s.student_id, own).await;
        let block = profile_class_ids(&s.app, cookie, &s.student_id).await;
        match &route {
            None => refused += 1,
            Some(ids) if !ids.is_empty() => served += 1,
            Some(_) => {}
        }
        assert_eq!(
            route.unwrap_or_default(),
            block,
            "{who}: the profile's class block and the class route disagree"
        );
    }
    // Neither half may pass vacuously: somebody was refused, somebody was served.
    assert_eq!(refused, 1, "exactly the peer is refused the class route");
    assert_eq!(served, 4, "the other four are served a non-empty list");
}

// --- Badges ---------------------------------------------------------------
//
// Badges are *permanent*: an award row is stamped once and nothing ever moves
// or removes it, while the counters underneath it move both ways. Every test
// below is written against that split — the award, not the counter, is what
// carries the promise — and the counter tests read off the profile rather than
// the database, because the profile is the only surface a client has.

/// A course with a subject and one enrolled student: the smallest cast that
/// can move a homework counter.
struct Classroom {
    app: Router,
    db: Database,
    teacher: String,
    student: String,
    student_id: String,
    course: String,
    subject: String,
}

async fn classroom() -> Classroom {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teach", "teacher").await;
    let student = login(&app, "stu").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "Algebra").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let subject = create_subject(&app, &teacher, &course, "Fractions").await;
    Classroom {
        app,
        db,
        teacher,
        student,
        student_id,
        course,
        subject,
    }
}

/// A homework due `in_ms` from now. Negative backdates it *inside* the
/// scheduling grace — the only way to hand something in late in one test.
async fn homework(c: &Classroom, title: &str, in_ms: i64) -> String {
    let due = Timestamp::now().as_millis() + in_ms;
    create_homework(&c.app, &c.teacher, &c.course, &c.subject, title, due).await
}

/// Move `homework`'s deadline to `in_ms` from now, as the teacher who set it.
/// Negative backdates it *inside* the scheduling grace, which is legal — the
/// deadline is mutable in both directions, which is what the two tests below
/// are about.
async fn move_deadline(c: &Classroom, homework: &str, in_ms: i64) {
    let due = Timestamp::now().as_millis() + in_ms;
    let res = send(
        &c.app,
        "PATCH",
        &format!("/homework/{homework}"),
        Some(&c.teacher),
        Some(json!({ "due_at": due })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

/// Hand in `homework` as `cookie`, asserting the first-hand-in `201`.
async fn hand_in(app: &Router, cookie: &str, homework: &str) {
    let res = send(
        app,
        "POST",
        &format!("/homework/{homework}/submission"),
        Some(cookie),
        Some(json!({ "text": "done" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
}

/// Withdraw the caller's own submission, asserting the `204`.
async fn withdraw(app: &Router, cookie: &str, homework: &str) {
    let res = send(
        app,
        "DELETE",
        &format!("/homework/{homework}/submission"),
        Some(cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
}

/// The caller's own profile body (asserts the `200`).
async fn my_profile(app: &Router, cookie: &str) -> Value {
    let res = send(app, "GET", "/users/me/profile", Some(cookie), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    res.body
}

/// When `id` was earned, off a profile body — `None` if the shelf lacks it.
fn stamp_of(profile: &Value, id: &str) -> Option<i64> {
    profile["badges"]
        .as_array()
        .expect("badges array")
        .iter()
        .find(|badge| badge["id"] == id)
        .map(|badge| badge["earned_at"].as_i64().expect("earned_at millis"))
}

/// The badge ids on a profile, in the order served.
fn badge_ids(profile: &Value) -> Vec<&str> {
    profile["badges"]
        .as_array()
        .expect("badges array")
        .iter()
        .map(|badge| badge["id"].as_str().expect("badge id"))
        .collect()
}

/// Crossing a threshold earns the badge there and then, stamped with the
/// moment it happened — not at some later sweep. The window is taken around
/// the request, so a stamp copied from anywhere else fails.
#[tokio::test]
async fn a_badge_lands_the_moment_its_threshold_is_crossed() {
    let c = classroom().await;
    let first = homework(&c, "Fractions I", 600_000).await;

    let before = my_profile(&c.app, &c.student).await;
    assert_eq!(badge_ids(&before), Vec::<&str>::new(), "nothing earned yet");

    let opened = Timestamp::now().as_millis();
    hand_in(&c.app, &c.student, &first).await;
    let closed = Timestamp::now().as_millis();

    let mine = my_profile(&c.app, &c.student).await;
    assert_eq!(badge_ids(&mine), ["homework_submitted_1"], "{mine}");
    let earned = stamp_of(&mine, "homework_submitted_1").expect("the badge");
    assert!(
        (opened..=closed).contains(&earned),
        "earned_at {earned} is outside the request window {opened}..={closed}: {mine}"
    );
    // One hand-in is one submission, and the ten-badge is nine away.
    assert_eq!(mine["stats"]["homework_submitted_total"], 1);
    assert_eq!(mine["stats"]["homework_on_time_total"], 1);
}

/// The permanence guarantee, at the stamp: crossing again re-runs the sync,
/// and the sync's `WHERE earned_at = NONE` must leave the original stamp
/// standing. The clock is given room to move between the two hand-ins, so a
/// sync that *did* overwrite would write a demonstrably later value.
#[tokio::test]
async fn a_second_crossing_never_moves_the_stamp() {
    let c = classroom().await;
    let first = homework(&c, "Fractions I", 600_000).await;
    let second = homework(&c, "Fractions II", 600_000).await;

    hand_in(&c.app, &c.student, &first).await;
    let earned = stamp_of(
        &my_profile(&c.app, &c.student).await,
        "homework_submitted_1",
    )
    .expect("the badge");

    // Any overwrite from here on is strictly later than this instant.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let moved_on = Timestamp::now().as_millis();
    assert!(earned < moved_on, "the clock never moved — test is vacuous");

    hand_in(&c.app, &c.student, &second).await;
    let after = my_profile(&c.app, &c.student).await;
    assert_eq!(after["stats"]["homework_submitted_total"], 2, "{after}");
    assert_eq!(
        stamp_of(&after, "homework_submitted_1"),
        Some(earned),
        "a re-sync moved a permanent stamp: {after}"
    );
}

/// The whole point of an award row: permanence lives at the award, not at the
/// counter. Withdrawing the submission takes the counter back to zero — and
/// the badge it earned stays on the shelf, stamp untouched.
#[tokio::test]
async fn a_badge_outlives_the_counter_that_earned_it() {
    let c = classroom().await;
    let only = homework(&c, "Fractions I", 600_000).await;

    hand_in(&c.app, &c.student, &only).await;
    let earned_it = my_profile(&c.app, &c.student).await;
    let earned = stamp_of(&earned_it, "homework_submitted_1").expect("the badge");
    assert_eq!(earned_it["stats"]["homework_submitted_total"], 1);

    withdraw(&c.app, &c.student, &only).await;

    let after = my_profile(&c.app, &c.student).await;
    assert_eq!(
        after["stats"]["homework_submitted_total"], 0,
        "the counter must come back down: {after}"
    );
    assert_eq!(after["stats"]["homework_on_time_total"], 0, "{after}");
    assert_eq!(
        stamp_of(&after, "homework_submitted_1"),
        Some(earned),
        "a badge is never taken back: {after}"
    );
}

/// The farm hole, closed at the counter: withdrawing gives the credit back, so
/// submit → withdraw → submit is worth exactly one submission however many
/// times it is run. Without the decrement, one homework mints unlimited ones.
#[tokio::test]
async fn submit_withdraw_submit_is_worth_one_submission() {
    let c = classroom().await;
    let only = homework(&c, "Fractions I", 600_000).await;

    for _ in 0..3 {
        hand_in(&c.app, &c.student, &only).await;
        withdraw(&c.app, &c.student, &only).await;
    }
    hand_in(&c.app, &c.student, &only).await;

    let mine = my_profile(&c.app, &c.student).await;
    assert_eq!(
        mine["stats"]["homework_submitted_total"], 1,
        "one homework handed in once, whatever the churn: {mine}"
    );
    assert_eq!(mine["stats"]["homework_on_time_total"], 1, "{mine}");
}

/// The two homework counters part company at the deadline: everything handed
/// in counts as submitted, only what beat `due_at` counts as on time. The
/// on-time hand-in in the same test is what proves the counter still moves at
/// all, so a permanently-stuck `on_time` cannot pass as "the late one".
#[tokio::test]
async fn a_late_submission_moves_submitted_but_not_on_time() {
    let c = classroom().await;
    // Backdated inside the scheduling grace: already due, still creatable.
    let overdue = homework(&c, "Yesterday's", -30_000).await;
    let punctual = homework(&c, "Next week's", 600_000).await;

    hand_in(&c.app, &c.student, &overdue).await;
    let late = my_profile(&c.app, &c.student).await;
    assert_eq!(late["stats"]["homework_submitted_total"], 1, "{late}");
    assert_eq!(
        late["stats"]["homework_on_time_total"], 0,
        "a submission past due_at earns no on-time credit: {late}"
    );

    hand_in(&c.app, &c.student, &punctual).await;
    let both = my_profile(&c.app, &c.student).await;
    assert_eq!(both["stats"]["homework_submitted_total"], 2, "{both}");
    assert_eq!(both["stats"]["homework_on_time_total"], 1, "{both}");
}

/// A withdrawal gives back what the hand-in was credited with, not what the
/// deadline says *now*: a teacher who extends `due_at` after a late hand-in has
/// not made it on time retroactively. Re-judging at withdrawal debited an
/// on-time credit that was never given, and the zero floor hid it — so a
/// genuinely on-time submission is held throughout, and it is that one's credit
/// the late withdrawal used to eat.
#[tokio::test]
async fn extending_a_deadline_never_debits_an_on_time_credit_it_never_gave() {
    let c = classroom().await;
    let punctual = homework(&c, "Next week's", 600_000).await;
    // Backdated inside the scheduling grace: already due, still creatable.
    let overdue = homework(&c, "Yesterday's", -30_000).await;

    hand_in(&c.app, &c.student, &punctual).await;
    hand_in(&c.app, &c.student, &overdue).await;
    let both = my_profile(&c.app, &c.student).await;
    assert_eq!(both["stats"]["homework_submitted_total"], 2, "{both}");
    assert_eq!(both["stats"]["homework_on_time_total"], 1, "{both}");

    // The teacher relents and moves the deadline out. The hand-in that was late
    // when it landed was still late when it landed.
    move_deadline(&c, &overdue, 600_000).await;
    let moved = my_profile(&c.app, &c.student).await;
    assert_eq!(moved["stats"]["homework_on_time_total"], 1, "{moved}");

    withdraw(&c.app, &c.student, &overdue).await;
    let after = my_profile(&c.app, &c.student).await;
    assert_eq!(after["stats"]["homework_submitted_total"], 1, "{after}");
    assert_eq!(
        after["stats"]["homework_on_time_total"], 1,
        "withdrawing the late one took the punctual one's credit: {after}"
    );

    // ... and the punctual one still has exactly its own credit to give back.
    withdraw(&c.app, &c.student, &punctual).await;
    let empty = my_profile(&c.app, &c.student).await;
    assert_eq!(empty["stats"]["homework_submitted_total"], 0, "{empty}");
    assert_eq!(empty["stats"]["homework_on_time_total"], 0, "{empty}");
}

/// The other direction, where the floor cannot hide it: pulling `due_at` back
/// after a punctual hand-in used to make the withdrawal debit *nothing* on
/// time, leaving `on_time` standing above `submitted` — a student credited with
/// more punctual hand-ins than hand-ins. The invariant is checked at every step,
/// not just at the end.
#[tokio::test]
async fn pulling_a_deadline_back_leaves_on_time_no_higher_than_submitted() {
    let c = classroom().await;
    let only = homework(&c, "Next week's", 600_000).await;

    let counted = |profile: &Value| -> (i64, i64) {
        let submitted = profile["stats"]["homework_submitted_total"]
            .as_i64()
            .unwrap_or_else(|| panic!("submitted total: {profile}"));
        let on_time = profile["stats"]["homework_on_time_total"]
            .as_i64()
            .unwrap_or_else(|| panic!("on-time total: {profile}"));
        assert!(
            on_time <= submitted,
            "on_time {on_time} exceeds submitted {submitted}: {profile}"
        );
        (submitted, on_time)
    };

    hand_in(&c.app, &c.student, &only).await;
    assert_eq!(counted(&my_profile(&c.app, &c.student).await), (1, 1));

    // Backdating inside the grace is a legal PATCH, and it moves the very cut
    // the credit was judged by.
    move_deadline(&c, &only, -30_000).await;
    assert_eq!(
        counted(&my_profile(&c.app, &c.student).await),
        (1, 1),
        "moving a deadline re-judges nothing already credited"
    );

    withdraw(&c.app, &c.student, &only).await;
    assert_eq!(
        counted(&my_profile(&c.app, &c.student).await),
        (0, 0),
        "the withdrawal gave back the on-time credit it took"
    );
}

/// `exam_sat_total` counts exams sat, not requests: re-posting a running
/// attempt resumes it (`200`, same clock) and must leave the counter alone.
#[tokio::test]
async fn a_resumed_exam_attempt_counts_once() {
    let c = classroom().await;
    let now = Timestamp::now().as_millis();
    let res = create_exam_with(
        &c.app,
        &c.teacher,
        &c.course,
        json!({ "title": "Midterm", "kind": "quiz", "mode": "sync",
                "starts_at": now - 1_000, "ends_at": now + 600_000 }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let exam = id_of(&res.body);

    let started = send(
        &c.app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&c.student),
        None,
    )
    .await;
    assert_eq!(started.status, StatusCode::CREATED, "{}", started.body);

    let resumed = send(
        &c.app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&c.student),
        None,
    )
    .await;
    assert_eq!(
        resumed.status,
        StatusCode::OK,
        "the second post must resume, not start: {}",
        resumed.body
    );

    let mine = my_profile(&c.app, &c.student).await;
    assert_eq!(
        mine["stats"]["exam_sat_total"], 1,
        "a resume is not a second sitting: {mine}"
    );
    assert_eq!(badge_ids(&mine), ["exam_sat_1"], "{mine}");
}

/// The catalog a client renders badges from, end to end. `stat` is a **wire**
/// name: a domain test pins that it differs from the user-row column, and this
/// pins that what ships is the wire one — and that appending `_total` to it
/// lands on a real key of a real profile's stats, which is the only thing that
/// lets a client join the two surfaces at all.
#[tokio::test]
async fn the_badge_catalog_is_reachable_and_wire_named() {
    let (app, _db) = app_and_db().await;
    let fresh = login(&app, "fresh").await;
    let stats = my_profile(&app, &fresh).await["stats"]
        .as_object()
        .expect("stats object")
        .clone();

    // Unauthenticated: the form that renders a badge needs no session.
    let res = send(&app, "GET", "/limits", None, None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let catalog = res.body["badges"]["catalog"]
        .as_array()
        .unwrap_or_else(|| panic!("badges.catalog array: {}", res.body));
    assert!(!catalog.is_empty(), "an empty catalog awards nothing");

    for badge in catalog {
        let id = badge["id"]
            .as_str()
            .unwrap_or_else(|| panic!("id: {badge}"));
        let stat = badge["stat"]
            .as_str()
            .unwrap_or_else(|| panic!("stat: {badge}"));
        let threshold = badge["threshold"]
            .as_i64()
            .unwrap_or_else(|| panic!("threshold: {badge}"));
        assert!(threshold > 0, "{id} is earned by doing nothing");
        assert!(
            !stat.ends_with("_total"),
            "{id} serves the database column {stat}, not a wire name: renaming \
             the column would break the API"
        );
        assert!(
            stats.contains_key(&format!("{stat}_total")),
            "{id}'s stat {stat} joins no profile stats key: {stats:?}"
        );
    }
    assert!(
        catalog
            .iter()
            .any(|badge| badge["id"] == "homework_submitted_1" && badge["threshold"] == 1),
        "the first-homework badge left the catalog: {res_body}",
        res_body = res.body
    );
}

/// Badges are public — they are a decoration, not a record — so they ride the
/// profile's existing gate and open no door of their own: a peer reads them,
/// and a parent with no link to the student is still refused the whole
/// profile, badges included.
#[tokio::test]
async fn badges_ride_the_profile_gate_and_open_no_new_door() {
    let c = classroom().await;
    let only = homework(&c, "Fractions I", 600_000).await;
    hand_in(&c.app, &c.student, &only).await;

    let peer = login(&c.app, "peer").await;
    let seen = profile(&c.app, &peer, &c.student_id).await;
    assert_eq!(seen.status, StatusCode::OK, "{}", seen.body);
    assert_eq!(
        badge_ids(&seen.body),
        ["homework_submitted_1"],
        "a peer sees the shelf: {}",
        seen.body
    );

    let stranger = login_as(&c.app, &c.db, "mom", "parent").await;
    let blocked = profile(&c.app, &stranger, &c.student_id).await;
    assert_eq!(
        blocked.status,
        StatusCode::FORBIDDEN,
        "an unlinked parent reads no profile, badges included: {}",
        blocked.body
    );
}

/// A profile that was never there is a 404, at any role — the id is not an
/// existence oracle wearing a 403.
#[tokio::test]
async fn an_unknown_user_profile_is_a_404() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teach", "teacher").await;

    let gone = profile(&app, &teacher, ABSENT_ID).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND, "{}", gone.body);
}
