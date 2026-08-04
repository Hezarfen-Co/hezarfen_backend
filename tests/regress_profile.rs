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
use common::{app_and_db, create_course, enroll, login, login_as, me_id, send, set_role};
use hezarfen_backend::constant::{MAX_BIO_LEN, MAX_DISPLAY_NAME_LEN};
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
    std::fs::read_dir(common::files_dir())
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
    for private in ["email", "phone", "birth_date", "badges"] {
        assert!(
            seen.body.get(private).is_none(),
            "{private} must not ride on a profile: {}",
            seen.body
        );
    }
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

/// The counters are derived, so a fresh account reads a true `0` on all four —
/// never `null`, never an absent key a client would have to guard.
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
    ] {
        assert_eq!(
            stats.get(field).and_then(Value::as_i64),
            Some(0),
            "stats.{field} must be a numeric 0: {:?}",
            stats.get(field)
        );
    }
    assert_eq!(stats.len(), 4, "no fifth counter shipped: {stats:?}");
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

/// A profile that was never there is a 404, at any role — the id is not an
/// existence oracle wearing a 403.
#[tokio::test]
async fn an_unknown_user_profile_is_a_404() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "teach", "teacher").await;

    let gone = profile(&app, &teacher, "01J8XZ0K3Q8G7X2M4N5P6R7S8T").await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND, "{}", gone.body);
}
