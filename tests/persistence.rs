//! Boot-migration tests: age rows into the shapes old binaries left behind,
//! then re-run the idempotent migration on the same database — a second boot —
//! and assert the backfills repair (or deliberately destroy) them. The storage
//! engine itself is the SurrealDB server's concern since the embedded engine
//! was retired; these tests run on the in-memory engine like every other suite.

mod common;

use axum::Router;
use axum::http::StatusCode;
use common::{create_course, create_exam, create_subject, enroll, me_id, send, set_role};
use hezarfen_backend::database::Database;
use hezarfen_backend::domain::timestamp::Timestamp;
use hezarfen_backend::rate_limit::RateLimitConfig;
use hezarfen_backend::state::AppState;
use hezarfen_backend::{build_router, database};
use serde_json::json;

/// Simulate a reboot: re-run the migration on the same database and hand back
/// a fresh router over it.
async fn reboot(db: &Database) -> Router {
    database::migrate(db).await.expect("re-migration");
    build_router(AppState {
        db: db.clone(),
        files_path: common::files_dir(),
        cookie_secure: false,
        rate_limit: RateLimitConfig::unlimited(),
        chatbot_limit: Default::default(),
        exam_presence: Default::default(),
        board_hub: Default::default(),
        db_up: Default::default(),
        ai: None,
    })
}

/// School policy and terms survive a second boot — the settings singleton
/// (nested band objects included) and the course→term link both come back,
/// untouched by the re-applied idempotent migration.
#[tokio::test]
async fn settings_and_terms_survive_remigration() {
    let (app, db) = common::app_and_db().await;

    let creds = json!({ "username": "boss", "password": "secret1" });
    send(&app, "POST", "/auth/register", None, Some(creds.clone())).await;
    set_role(&db, "boss", "manager").await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds.clone()))
        .await
        .cookie
        .unwrap();

    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&cookie),
        Some(json!({
            "exam_kinds": [
                { "name": "lab", "weight": 2 },
                { "name": "quiz", "weight": 1 },
            ],
            "grade_bands": [{ "min": 0, "label": "F" }, { "min": 50, "label": "P" }],
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);

    let res = send(
        &app,
        "POST",
        "/terms",
        Some(&cookie),
        Some(json!({ "name": "2026 Fall", "starts_at": 1, "ends_at": 2 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let term = res.body["id"].as_str().unwrap().to_string();

    let res = send(
        &app,
        "POST",
        "/courses",
        Some(&cookie),
        Some(json!({ "title": "History", "term_id": term })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);

    // Second boot: migration re-applied over live data.
    let app = reboot(&db).await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds))
        .await
        .cookie
        .unwrap();

    let res = send(&app, "GET", "/settings", Some(&cookie), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        res.body["exam_kinds"],
        json!([
            { "name": "lab", "weight": 2 },
            { "name": "quiz", "weight": 1 },
        ])
    );
    assert_eq!(res.body["grade_bands"][0]["label"], "P");

    let res = send(&app, "GET", &format!("/terms/{term}"), Some(&cookie), None).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.body["name"], "2026 Fall");

    let res = send(&app, "GET", "/courses", Some(&cookie), None).await;
    let courses = common::items(&res.body);
    assert_eq!(courses.len(), 1);
    assert_eq!(courses[0]["term"].as_str(), Some(term.as_str()));
}

/// Event rows written before the `audience` column existed are backfilled to
/// school-wide on the next boot (`UPDATE event SET audience = { kind: 'school' }
/// WHERE audience = NONE`), so an old volume keeps serving its events.
#[tokio::test]
async fn legacy_events_backfill_to_school_audience() {
    let (app, db) = common::app_and_db().await;
    let creds = json!({ "username": "ali", "password": "secret1" });

    // A normal event — then strip its audience the way an old binary's schema
    // would have left it: drop the column definitions so SCHEMAFULL stops
    // enforcing them, and unset the field on the row.
    assert_eq!(
        send(&app, "POST", "/auth/register", None, Some(creds.clone()))
            .await
            .status,
        StatusCode::CREATED
    );
    set_role(&db, "ali", "teacher").await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds.clone()))
        .await
        .cookie
        .unwrap();
    let res = send(
        &app,
        "POST",
        "/events",
        Some(&cookie),
        Some(json!({ "title": "before audiences" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let event_id = common::id_of(&res.body);

    db.query(
        "REMOVE FIELD IF EXISTS audience.users ON TABLE event;
         REMOVE FIELD IF EXISTS audience.course ON TABLE event;
         REMOVE FIELD IF EXISTS audience.role ON TABLE event;
         REMOVE FIELD IF EXISTS audience.kind ON TABLE event;
         REMOVE FIELD IF EXISTS audience ON TABLE event;
         UPDATE event SET audience = NONE;",
    )
    .await
    .expect("strip audience")
    .check()
    .expect("strip audience check");

    // Second boot re-runs the migration: the field definitions come back and
    // the backfill stamps the legacy row school-wide — it reads, lists, and
    // rosters like any new event.
    let app = reboot(&db).await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds))
        .await
        .cookie
        .unwrap();
    let res = send(
        &app,
        "GET",
        &format!("/events/{event_id}"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["audience"]["kind"], "school");
    let res = send(
        &app,
        "GET",
        &format!("/events/{event_id}/roster"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(common::total(&res.body), 1, "whole school = the one user");
}

/// Event rows carrying the retired hand-picked (`users`) audience convert on
/// the next boot: the audience becomes an uncapped registration list and each
/// listed user gets a signup row credited to the event's creator — the old
/// roster survives the clean break byte for byte.
#[tokio::test]
async fn legacy_users_audiences_convert_to_registrations() {
    let (app, db) = common::app_and_db().await;
    let creds = json!({ "username": "ali", "password": "secret1" });

    // A registration-audience event — then rewrite it into the shape an old
    // binary left behind: restore the `audience.users` column definition and
    // store a hand-picked list on the row.
    assert_eq!(
        send(&app, "POST", "/auth/register", None, Some(creds.clone()))
            .await
            .status,
        StatusCode::CREATED
    );
    set_role(&db, "ali", "teacher").await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds.clone()))
        .await
        .cookie
        .unwrap();
    let student = json!({ "username": "veli", "password": "secret1" });
    assert_eq!(
        send(&app, "POST", "/auth/register", None, Some(student.clone()))
            .await
            .status,
        StatusCode::CREATED
    );
    let student_cookie = send(&app, "POST", "/auth/login", None, Some(student))
        .await
        .cookie
        .unwrap();
    let student_id = common::me_id(&app, &student_cookie).await;

    let res = send(
        &app,
        "POST",
        "/events",
        Some(&cookie),
        Some(json!({ "title": "trip", "audience": { "kind": "registration" } })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let event_id = common::id_of(&res.body);

    db.query(format!(
        "DEFINE FIELD OVERWRITE audience.users ON TABLE event TYPE option<array<record<user>>>;
         UPDATE event SET audience = {{ kind: 'users', users: [type::record('user', $usr)] }}
             WHERE id = type::record('event', '{event_id}');"
    ))
    .bind(("usr", student_id.clone()))
    .await
    .expect("rewrite to legacy users audience")
    .check()
    .expect("rewrite check");

    // Second boot runs the conversion: the audience reads back as an uncapped
    // registration list, the listed student holds a seat credited to the
    // creator, and the roster (and marking gate) see them.
    let app = reboot(&db).await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds))
        .await
        .cookie
        .unwrap();
    let res = send(
        &app,
        "GET",
        &format!("/events/{event_id}"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["audience"]["kind"], "registration");
    assert!(res.body["audience"]["capacity"].is_null());
    let res = send(
        &app,
        "GET",
        &format!("/events/{event_id}/roster"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(common::total(&res.body), 1, "the listed user became a seat");
    assert_eq!(common::items(&res.body)[0]["user"]["id"], student_id);
    let res = send(
        &app,
        "POST",
        &format!("/events/{event_id}/attendance"),
        Some(&cookie),
        Some(json!({ "status": "present", "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "converted seat is markable");
}

/// Exam-question rows written before subjects existed (2026-07) are destroyed
/// on the next boot, answers first — a subject is mandatory and there is
/// nothing truthful to backfill. The exam and the course's subjects survive.
#[tokio::test]
async fn legacy_subjectless_questions_are_destroyed_on_boot() {
    let (app, db) = common::app_and_db().await;
    let creds = json!({ "username": "ali", "password": "secret1" });

    // A full exam — course, subject, open exam, one answered question — then
    // strip the question's subject the way an old binary's schema would have
    // left it.
    assert_eq!(
        send(&app, "POST", "/auth/register", None, Some(creds.clone()))
            .await
            .status,
        StatusCode::CREATED
    );
    set_role(&db, "ali", "teacher").await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds.clone()))
        .await
        .cookie
        .unwrap();
    let student = json!({ "username": "veli", "password": "secret1" });
    assert_eq!(
        send(&app, "POST", "/auth/register", None, Some(student.clone()))
            .await
            .status,
        StatusCode::CREATED
    );
    let student_cookie = send(&app, "POST", "/auth/login", None, Some(student))
        .await
        .cookie
        .unwrap();
    let student_id = common::me_id(&app, &student_cookie).await;

    let course = create_course(&app, &cookie, "algebra").await;
    enroll(&app, &cookie, &course, &student_id).await;
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/subjects"),
        Some(&cookie),
        Some(json!({ "name": "arithmetic" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let subject_id = common::id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/exams"),
        Some(&cookie),
        Some(json!({ "title": "drill", "kind": "quiz", "mode": "open" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED);
    let exam_id = common::id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam_id}/questions"),
        Some(&cookie),
        Some(
            json!({ "subject_id": subject_id, "text": "2 + 2?", "kind": "choice",
                     "points": 10, "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}], "correct": "c1" }),
        ),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let question_id = common::id_of(&res.body);
    // Choice ids are minted server-side, so the answer names one off the response.
    let picked = res.body["choices"][1]["id"]
        .as_str()
        .expect("choice id")
        .to_string();
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam_id}/attempt"),
        Some(&student_cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam_id}/attempt/answers"),
        Some(&student_cookie),
        Some(json!({ "question_id": question_id, "selected": picked })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    db.query(
        "REMOVE FIELD IF EXISTS subject ON TABLE exam_question;
         UPDATE exam_question SET subject = NONE;",
    )
    .await
    .expect("strip subject")
    .check()
    .expect("strip subject check");

    // Second boot re-runs the migration and the destroy-backfill: the
    // subjectless question and its answer are gone, the exam and subject
    // still stand.
    let app = reboot(&db).await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds))
        .await
        .cookie
        .unwrap();
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam_id}/questions"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(common::total(&res.body), 0, "legacy questions destroyed");
    let res = send(
        &app,
        "GET",
        &format!("/subjects/{subject_id}"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "the subject itself survives");
    let mut result = db
        .query("SELECT VALUE id FROM exam_answer")
        .await
        .expect("count answers")
        .check()
        .expect("count answers check");
    let answers: Vec<surrealdb::types::RecordId> = result.take(0).expect("answer rows");
    assert!(answers.is_empty(), "orphaned answers destroyed");
}

/// Enrollment rows whose user is no longer a student — promoted before the
/// role endpoint swept enrollments (2026-07-18), or deleted outright — are
/// removed by the boot backfill. A real student's row survives.
#[tokio::test]
async fn stale_staff_enrollments_are_swept_on_boot() {
    let (app, db) = common::app_and_db().await;
    let creds = json!({ "username": "ali", "password": "secret1" });

    // A course with three enrolled students, then age two rows the way a
    // pre-fix binary could have — flip one student to teacher directly in the
    // DB, and delete the other's user record entirely.
    assert_eq!(
        send(&app, "POST", "/auth/register", None, Some(creds.clone()))
            .await
            .status,
        StatusCode::CREATED
    );
    set_role(&db, "ali", "teacher").await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds.clone()))
        .await
        .cookie
        .unwrap();
    let course = create_course(&app, &cookie, "algebra").await;

    let mut ids = Vec::new();
    for name in ["veli", "ayse", "can"] {
        let student = json!({ "username": name, "password": "secret1" });
        assert_eq!(
            send(&app, "POST", "/auth/register", None, Some(student.clone()))
                .await
                .status,
            StatusCode::CREATED
        );
        let student_cookie = send(&app, "POST", "/auth/login", None, Some(student))
            .await
            .cookie
            .unwrap();
        let id = me_id(&app, &student_cookie).await;
        enroll(&app, &cookie, &course, &id).await;
        ids.push(id);
    }
    let keeper_id = ids[1].clone();
    set_role(&db, "veli", "teacher").await;
    db.query("DELETE user WHERE username = 'can'")
        .await
        .expect("delete user")
        .check()
        .expect("delete user check");

    // Second boot: the backfill swept the promoted user's row and the deleted
    // user's dangling row; the remaining student's enrollment is intact.
    let app = reboot(&db).await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds))
        .await
        .cookie
        .unwrap();
    let roster = send(
        &app,
        "GET",
        &format!("/courses/{course}/enrollments"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(roster.status, StatusCode::OK, "{}", roster.body);
    assert_eq!(
        common::total(&roster.body),
        1,
        "stale rows swept: {}",
        roster.body
    );
    assert_eq!(
        common::items(&roster.body)[0]["user"]["id"],
        keeper_id.as_str(),
        "the real student's row survives"
    );
}

/// Exam rows written before the draft flag existed (2026-07-19) backfill to
/// published (`draft = false`) on the next boot — an old volume's exams stay
/// exactly as visible as they were.
#[tokio::test]
async fn legacy_exams_backfill_to_published() {
    let (app, db) = common::app_and_db().await;
    let teacher_creds = json!({ "username": "ali", "password": "secret1" });
    let student_creds = json!({ "username": "ayse", "password": "secret1" });

    // A normal exam — then strip `draft` the way an old binary's schema would
    // have left it.
    for creds in [&teacher_creds, &student_creds] {
        assert_eq!(
            send(&app, "POST", "/auth/register", None, Some((*creds).clone()))
                .await
                .status,
            StatusCode::CREATED
        );
    }
    set_role(&db, "ali", "teacher").await;
    let teacher = send(
        &app,
        "POST",
        "/auth/login",
        None,
        Some(teacher_creds.clone()),
    )
    .await
    .cookie
    .unwrap();
    let student = send(
        &app,
        "POST",
        "/auth/login",
        None,
        Some(student_creds.clone()),
    )
    .await
    .cookie
    .unwrap();
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let exam_id = create_exam(&app, &teacher, &course, "midterm", "midterm").await;

    db.query(
        "REMOVE FIELD IF EXISTS draft ON TABLE exam;
         UPDATE exam SET draft = NONE;",
    )
    .await
    .expect("strip draft")
    .check()
    .expect("strip draft check");

    // Second boot re-runs the migration: the legacy exam reads back published
    // and the enrolled student still sees it.
    let app = reboot(&db).await;
    let student = send(&app, "POST", "/auth/login", None, Some(student_creds))
        .await
        .cookie
        .unwrap();
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam_id}"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["draft"], false);
    let listed = send(&app, "GET", "/exams", Some(&student), None).await;
    assert_eq!(common::items(&listed.body).len(), 1);
}

/// A recurring publish and the booking sitting on it survive a second boot:
/// the slot's `note` and shared `series`, and the booking's counter-proposal
/// triple, `decided_by` and `status`, all come back untouched by the
/// re-applied migration. Every time in this schema is a unix-millisecond
/// `TYPE int` — never a `datetime` — so the round trip is asserted on odd
/// millisecond values, which any lossy conversion would round off.
#[tokio::test]
async fn appointments_survive_remigration() {
    /// Far enough ahead to clear `check_not_past` without touching the clock,
    /// and deliberately not a round number of seconds.
    const START: i64 = 1_900_000_000_123;
    const END: i64 = 1_900_000_003_777;
    const WEEK: i64 = 7 * 24 * 60 * 60 * 1000;
    const PROPOSED_START: i64 = START + 5_000_001;
    const PROPOSED_END: i64 = START + 8_000_003;

    let (app, db) = common::app_and_db().await;
    let teacher_creds = json!({ "username": "ali", "password": "secret1" });
    let student_creds = json!({ "username": "ayse", "password": "secret1" });
    for creds in [&teacher_creds, &student_creds] {
        assert_eq!(
            send(&app, "POST", "/auth/register", None, Some((*creds).clone()))
                .await
                .status,
            StatusCode::CREATED
        );
    }
    set_role(&db, "ali", "teacher").await;
    let teacher = send(
        &app,
        "POST",
        "/auth/login",
        None,
        Some(teacher_creds.clone()),
    )
    .await
    .cookie
    .unwrap();
    let student = send(
        &app,
        "POST",
        "/auth/login",
        None,
        Some(student_creds.clone()),
    )
    .await
    .cookie
    .unwrap();

    // A weekly publish: three occurrences sharing one series id and one note.
    let res = send(
        &app,
        "POST",
        "/appointments/slots",
        Some(&teacher),
        Some(json!({
            "starts_at": START,
            "ends_at": END,
            "note": "veli görüşmesi",
            "repeat_weekly": true,
            "until": START + 2 * WEEK,
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let published = res
        .body
        .as_array()
        .expect("publish returns an array")
        .clone();
    assert_eq!(published.len(), 3, "{}", res.body);
    let slot_id = published[0]["id"].as_str().unwrap().to_string();
    let series = published[0]["series"].as_str().unwrap().to_string();
    let slot_created_at = published[0]["created_at"].as_i64().unwrap();

    // Book the first occurrence, counter-propose another time, accept it — the
    // row then carries every optional column at once: a proposal triple, a
    // decider, and `approved`.
    let res = send(
        &app,
        "POST",
        "/appointments",
        Some(&student),
        Some(json!({ "slot": slot_id, "reason": "ödev" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let appointment = common::id_of(&res.body);
    let created_at = res.body["created_at"].as_i64().unwrap();
    let res = send(
        &app,
        "PATCH",
        &format!("/appointments/{appointment}/reschedule"),
        Some(&teacher),
        Some(json!({ "starts_at": PROPOSED_START, "ends_at": PROPOSED_END })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/appointments/{appointment}/reschedule/accept"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Second boot: migration re-applied over live data.
    let app = reboot(&db).await;
    let teacher = send(&app, "POST", "/auth/login", None, Some(teacher_creds))
        .await
        .cookie
        .unwrap();
    let student = send(&app, "POST", "/auth/login", None, Some(student_creds))
        .await
        .cookie
        .unwrap();

    let res = send(&app, "GET", "/appointments/slots", Some(&teacher), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(common::total(&res.body), 3, "{}", res.body);
    let slots = common::items(&res.body);
    assert_eq!(slots[0]["id"], slot_id.as_str());
    assert_eq!(slots[0]["starts_at"], START, "exact unix-ms, not rounded");
    assert_eq!(slots[0]["ends_at"], END);
    assert_eq!(slots[0]["note"], "veli görüşmesi");
    assert_eq!(slots[0]["created_at"], slot_created_at);
    for (week, slot) in slots.iter().enumerate() {
        assert_eq!(slot["series"], series.as_str(), "one series, all weeks");
        assert_eq!(slot["starts_at"], START + week as i64 * WEEK);
        assert_eq!(slot["ends_at"], END + week as i64 * WEEK);
    }

    let res = send(&app, "GET", "/appointments", Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(common::total(&res.body), 1, "{}", res.body);
    let booking = &common::items(&res.body)[0];
    assert_eq!(booking["id"], appointment.as_str());
    assert_eq!(booking["slot"], slot_id.as_str());
    assert_eq!(booking["status"], "approved");
    assert_eq!(booking["reason"], "ödev");
    assert_eq!(booking["proposed_starts_at"], PROPOSED_START);
    assert_eq!(booking["proposed_ends_at"], PROPOSED_END);
    assert_eq!(booking["proposed_by"]["username"], "ali");
    assert_eq!(booking["decided_by"]["username"], "ayse");
    // The accepted proposal *is* the meeting's time; the slot's own window lost.
    assert_eq!(booking["starts_at"], PROPOSED_START);
    assert_eq!(booking["ends_at"], PROPOSED_END);
    assert_eq!(booking["teacher"]["username"], "ali");
    assert_eq!(booking["requester"]["username"], "ayse");
    assert_eq!(booking["created_at"], created_at);
}

/// Per-attempt exam history is durable, not just an in-request join: two
/// sittings' answers and marks (keyed by `seq`) written before a reboot come
/// back with the right seq afterwards — the seq-1 answer keeps its own text and
/// both marks survive, oldest first, with seq 2 as the grade-of-record.
#[tokio::test]
async fn attempt_history_survives_remigration() {
    let (app, db) = common::app_and_db().await;
    let teacher_creds = json!({ "username": "ali", "password": "secret1" });
    let student_creds = json!({ "username": "ayse", "password": "secret1" });
    for creds in [&teacher_creds, &student_creds] {
        assert_eq!(
            send(&app, "POST", "/auth/register", None, Some((*creds).clone()))
                .await
                .status,
            StatusCode::CREATED
        );
    }
    set_role(&db, "ali", "teacher").await;
    let teacher = send(
        &app,
        "POST",
        "/auth/login",
        None,
        Some(teacher_creds.clone()),
    )
    .await
    .cookie
    .unwrap();
    let student = send(
        &app,
        "POST",
        "/auth/login",
        None,
        Some(student_creds.clone()),
    )
    .await
    .cookie
    .unwrap();
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "biology").await;
    let subject = create_subject(&app, &teacher, &course, "cells").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/exams"),
        Some(&teacher),
        Some(json!({ "title": "quiz", "kind": "quiz", "mode": "open", "max_attempts": 2 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let exam = common::id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        Some(json!({ "subject_id": subject, "text": "Name an organelle.",
                     "kind": "text", "points": 10 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let question = common::id_of(&res.body);

    // Two sittings: distinct answer text + distinct mark on each.
    for (text, mark) in [("mitochondria", 40), ("chloroplast", 90)] {
        assert_eq!(
            send(
                &app,
                "POST",
                &format!("/exams/{exam}/attempt"),
                Some(&student),
                None
            )
            .await
            .status,
            StatusCode::CREATED
        );
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/attempt/answers"),
            Some(&student),
            Some(json!({ "question_id": question, "text": text })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/results"),
            Some(&teacher),
            Some(json!({ "mark": mark, "user_id": student_id })),
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
    }

    // Second boot: migration re-applied over the two sittings' live rows.
    let app = reboot(&db).await;
    let teacher = send(&app, "POST", "/auth/login", None, Some(teacher_creds))
        .await
        .cookie
        .unwrap();

    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/students/{student_id}/attempts"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body, json!([1, 2]), "both sittings survived the reboot");

    // The seq-1 answer kept its own text through the reopen.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/students/{student_id}/attempts/1/answers"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["answers"][0]["text"], "mitochondria");

    // Both marks survive, oldest first; the roster's grade-of-record is seq 2.
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
    assert_eq!(
        marks,
        vec![40, 90],
        "both sittings' marks durable, oldest first"
    );
    let res = send(
        &app,
        "GET",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(
        common::items(&res.body)[0]["mark"],
        90,
        "seq 2 is grade-of-record"
    );
}

/// A chatbot thread and every turn in it die together: deleting the
/// thread cascades its `chatbot_message` rows in one transaction, so no
/// orphan survives a reboot. And the thread is owner-scoped end to end — a
/// second user reads neither the thread nor its messages.
#[tokio::test]
async fn chat_thread_delete_cascades_and_stays_owner_scoped() {
    use hezarfen_backend::domain::chatbot_message::{ChatContent, ChatbotMessage, MessageStatus};
    use hezarfen_backend::domain::chatbot_thread::ChatbotThread;
    use hezarfen_backend::domain::user::UserId;

    let (app, db) = common::app_and_db().await;
    let owner_cookie = common::login(&app, "ali").await;
    let other_cookie = common::login(&app, "ayse").await;
    let owner = UserId::from_key(&me_id(&app, &owner_cookie).await);
    let other = UserId::from_key(&me_id(&app, &other_cookie).await);

    let thread = ChatbotThread::create(&owner, None, &db)
        .await
        .expect("create");
    let id = thread.get_id().clone();
    let prompt = ChatbotMessage::append_user(
        &id,
        &owner,
        ChatContent::try_new("selam").expect("content"),
        &db,
    )
    .await
    .expect("append user");
    let reply = ChatbotMessage::append_pending_assistant(&id, &owner, &db)
        .await
        .expect("append assistant");
    assert_eq!(reply.get_status(), MessageStatus::Pending);

    let reply = ChatbotMessage::complete(
        reply.get_id(),
        ChatContent::try_new("aleykum selam").expect("content"),
        false,
        &db,
    )
    .await
    .expect("complete");
    assert_eq!(reply.get_status(), MessageStatus::Complete);
    assert_eq!(reply.get_content().as_str(), "aleykum selam");
    assert!(reply.get_completed_at().is_some());
    assert_eq!(
        ChatbotMessage::list_for_thread(&id, None, 0, &db)
            .await
            .expect("thread")
            .0
            .len(),
        2
    );

    // The other user sees nothing of it, by thread or by message.
    assert!(
        ChatbotThread::read_for(&id, &other, &db)
            .await
            .expect("cross-user thread")
            .is_none()
    );
    assert!(
        ChatbotMessage::read_for(prompt.get_id(), &other, &db)
            .await
            .expect("cross-user message")
            .is_none()
    );
    assert_eq!(
        ChatbotThread::count_for_user(&other, &db)
            .await
            .expect("count"),
        0
    );
    assert_eq!(
        ChatbotThread::count_for_user(&owner, &db)
            .await
            .expect("count"),
        1
    );

    thread.delete(&db).await.expect("delete");
    assert!(
        ChatbotThread::read_for(&id, &owner, &db)
            .await
            .expect("deleted thread")
            .is_none()
    );
    assert_eq!(
        ChatbotThread::list_for_user(&owner, None, 0, &db)
            .await
            .unwrap()
            .0
            .len(),
        0
    );
    // Not just the thread's own view: no `chatbot_message` row is left anywhere.
    let mut left = db
        .query("SELECT VALUE id FROM chatbot_message")
        .await
        .expect("sweep")
        .check()
        .expect("sweep check");
    assert!(
        left.take::<Vec<surrealdb::types::RecordId>>(0)
            .expect("ids")
            .is_empty()
    );
}

/// Chat turns written before the clipped-answer flag existed (2026-07-23)
/// backfill to `truncated = false` on the next boot: an old volume's answers
/// read back as whole, which is exactly how they were shown at the time.
#[tokio::test]
async fn legacy_chat_turns_backfill_to_untruncated() {
    use hezarfen_backend::domain::chatbot_message::{ChatContent, ChatbotMessage};
    use hezarfen_backend::domain::chatbot_thread::ChatbotThread;
    use hezarfen_backend::domain::user::UserId;

    let (app, db) = common::app_and_db().await;
    let creds = json!({ "username": "ali", "password": "secret1" });
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
    let owner = UserId::from_key(&me_id(&app, &cookie).await);

    let thread = ChatbotThread::create(&owner, None, &db)
        .await
        .expect("create");
    let id = thread.get_id().clone();
    ChatbotMessage::append_user(
        &id,
        &owner,
        ChatContent::try_new("selam").expect("content"),
        &db,
    )
    .await
    .expect("append user");
    let reply = ChatbotMessage::append_pending_assistant(&id, &owner, &db)
        .await
        .expect("append assistant");
    ChatbotMessage::complete(
        reply.get_id(),
        ChatContent::try_new("aleykum selam").expect("content"),
        false,
        &db,
    )
    .await
    .expect("complete");

    // Strip the column the way a pre-flag binary's schema would have left it.
    db.query(
        "REMOVE FIELD IF EXISTS truncated ON TABLE chatbot_message;
         UPDATE chatbot_message SET truncated = NONE;",
    )
    .await
    .expect("strip truncated")
    .check()
    .expect("strip truncated check");

    // Second boot: the backfill stamps the legacy rows, and the thread reads
    // back through the API instead of failing to deserialize.
    let app = reboot(&db).await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds))
        .await
        .cookie
        .unwrap();
    let res = send(
        &app,
        "GET",
        &format!("/chatbot/threads/{}/messages", id.key()),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let turns = common::items(&res.body);
    assert_eq!(turns.len(), 2, "{}", res.body);
    for turn in turns {
        assert_eq!(turn["truncated"], false, "{turn}");
    }
}

/// A settings singleton whose `meal_slots` predate `serving_minute` — the live
/// podman volume's shape — still reads, and still **saves**. The trap is the
/// compare-and-set in `save_if_unchanged`: it re-binds the loaded slot list as
/// the expected value, so if a slot object without the key round-tripped to one
/// *with* `serving_minute: NONE`, the guard would never match again and every
/// `PATCH /settings` would 409 forever on an upgraded database.
#[tokio::test]
async fn settings_slots_without_a_serving_minute_still_patch() {
    let (app, db) = common::app_and_db().await;

    let creds = json!({ "username": "boss", "password": "secret1" });
    send(&app, "POST", "/auth/register", None, Some(creds.clone())).await;
    set_role(&db, "boss", "manager").await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds.clone()))
        .await
        .cookie
        .unwrap();

    // Materialize the singleton, then age its slots into the pre-serving-time
    // shape: objects carrying nothing but a name.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&cookie),
        Some(json!({ "meal_slots": [{ "name": "lunch", "serving_minute": 720 }] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    db.query("UPDATE settings:school SET meal_slots = [{ name: 'lunch' }];")
        .await
        .expect("age slots")
        .check()
        .expect("age slots check");

    // Second boot over that row: it reads back, with the field simply absent.
    let app = reboot(&db).await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds))
        .await
        .cookie
        .unwrap();
    let res = send(&app, "GET", "/settings", Some(&cookie), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["meal_slots"][0]["name"], "lunch", "{}", res.body);
    assert!(
        res.body["meal_slots"][0]["serving_minute"].is_null(),
        "{}",
        res.body
    );

    // And a PATCH of an unrelated knob still lands: the CAS matched the aged
    // row. This is the assertion that would have caught a `DEFAULT []` column.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&cookie),
        Some(json!({ "max_file_bytes": 2048 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["max_file_bytes"], 2048, "{}", res.body);
    assert!(
        res.body["meal_slots"][0]["serving_minute"].is_null(),
        "{}",
        res.body
    );

    // Setting the serving time on that legacy slot round-trips too.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&cookie),
        Some(json!({ "meal_slots": [{ "name": "lunch", "serving_minute": 615 }] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        res.body["meal_slots"][0]["serving_minute"], 615,
        "{}",
        res.body
    );
}

/// Rows written before the cap counters existed (2026-07-27) carry no counter
/// at all, and an absent counter reads as zero — which would hand a full course
/// and a full note a clean slate. The backfill seeds each parent from the
/// children it actually has, so the caps keep holding across the upgrade.
#[tokio::test]
async fn cap_counters_are_seeded_from_the_rows_that_predate_them() {
    let (app, db) = common::app_and_db().await;
    let teacher = common::login_as(&app, &db, "teacher", "teacher").await;
    let ali = common::login(&app, "ali").await;
    let veli = common::login(&app, "veli").await;
    let ayse = common::login(&app, "ayse").await;
    let (ali_id, veli_id, ayse_id) = (
        me_id(&app, &ali).await,
        me_id(&app, &veli).await,
        me_id(&app, &ayse).await,
    );

    let res = send(
        &app,
        "POST",
        "/courses",
        Some(&teacher),
        Some(json!({ "title": "small", "description": "", "capacity": 2 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let course = common::id_of(&res.body);
    enroll(&app, &teacher, &course, &ali_id).await;
    enroll(&app, &teacher, &course, &veli_id).await;

    let res = send(
        &app,
        "POST",
        "/notes",
        Some(&ali),
        Some(json!({ "title": "full", "content": "x" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let note = common::id_of(&res.body);
    for i in 0..10 {
        let up = common::upload_file(&app, &ali, &note, &format!("f{i}.txt"), "", b"x").await;
        assert_eq!(up.status, StatusCode::CREATED, "seed file {i}");
    }

    // Age both parents into the pre-counter shape.
    db.query("UPDATE course UNSET enrollment_count; UPDATE note UNSET file_count;")
        .await
        .expect("age the rows")
        .check()
        .expect("age the rows");

    let app = reboot(&db).await;

    let mut counters = db
        .query("SELECT VALUE enrollment_count FROM course; SELECT VALUE file_count FROM note;")
        .await
        .expect("read counters")
        .check()
        .expect("read counters");
    assert_eq!(
        counters.take::<Vec<i64>>(0).expect("enrollment_count"),
        vec![2],
        "the course counter must be seeded from its roster"
    );
    assert_eq!(
        counters.take::<Vec<i64>>(1).expect("file_count"),
        vec![10],
        "the note counter must be seeded from its files"
    );

    // And the caps still bite, which a zeroed counter would not have done.
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/enrollments"),
        Some(&teacher),
        Some(json!({ "user_id": ayse_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let up = common::upload_file(&app, &ali, &note, "eleventh.txt", "", b"x").await;
    assert_eq!(up.status, StatusCode::CONFLICT, "{}", up.body);
}

/// The appointment slot's counter is the odd one out: it seeds from the
/// bookings that are still *live*, because a rejected or cancelled one gave the
/// slot back long before the column existed (2026-07-27). A slot aged into the
/// pre-counter shape must come back taken if someone is still waiting on it,
/// and free if nobody is — a flat recount of its rows would get the second case
/// wrong and lock a free slot out of the calendar for good.
#[tokio::test]
async fn slot_occupancy_is_seeded_from_the_bookings_that_are_still_live() {
    let (app, db) = common::app_and_db().await;
    let teacher = common::login_as(&app, &db, "teacher", "teacher").await;
    let veli = common::login(&app, "veli").await;
    let ayse = common::login(&app, "ayse").await;
    let now = hezarfen_backend::domain::timestamp::Timestamp::now().as_millis();
    let hour = 3_600_000;

    async fn publish(app: &Router, who: &str, starts_at: i64) -> String {
        let res = send(
            app,
            "POST",
            "/appointments/slots",
            Some(who),
            Some(json!({ "starts_at": starts_at, "ends_at": starts_at + 3_600_000 })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
        common::id_of(&res.body[0])
    }
    async fn book(app: &Router, who: &str, slot: &str) -> common::Res {
        send(
            app,
            "POST",
            "/appointments",
            Some(who),
            Some(json!({ "slot": slot, "reason": "görüşme" })),
        )
        .await
    }

    let taken = publish(&app, &teacher, now + hour).await;
    let freed = publish(&app, &teacher, now + 3 * hour).await;
    let held = common::id_of(&book(&app, &veli, &taken).await.body);
    let gone = common::id_of(&book(&app, &veli, &freed).await.body);
    let res = send(
        &app,
        "PATCH",
        &format!("/appointments/{gone}/reject"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Age both slots into the pre-counter shape.
    db.query("UPDATE appointment_slot UNSET occupied;")
        .await
        .expect("age the rows")
        .check()
        .expect("age the rows");

    let app = reboot(&db).await;

    // The still-pending booking keeps its slot; the rejected one's slot is free.
    let res = book(&app, &ayse, &taken).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let res = book(&app, &ayse, &freed).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    // And the seeded seat is still a seat, not a stuck flag: cancelling the
    // held booking hands it back.
    let res = send(
        &app,
        "PATCH",
        &format!("/appointments/{held}/cancel"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = book(&app, &ayse, &taken).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
}

/// A submission graded before the freeze moved into the database (2026-07-27)
/// carries no `graded_by_result` stamp, and an absent stamp is exactly what the
/// student's writes now take as "still open" — so without the backfill an
/// upgrade would quietly unfreeze every already-graded submission. The backfill
/// stamps each one from the grade it already has; graded-but-never-submitted
/// work has no row to stamp and must stay that way (the report reads
/// `submitted`/`missing` straight off its absence).
#[tokio::test]
async fn graded_submissions_are_stamped_by_the_backfill() {
    let (app, db) = common::app_and_db().await;
    let teacher = common::login_as(&app, &db, "teacher", "teacher").await;
    let ali = common::login(&app, "ali").await;
    let veli = common::login(&app, "veli").await;
    let (ali_id, veli_id) = (me_id(&app, &ali).await, me_id(&app, &veli).await);
    let course = create_course(&app, &teacher, "math").await;
    let subject = create_subject(&app, &teacher, &course, "algebra").await;
    enroll(&app, &teacher, &course, &ali_id).await;
    enroll(&app, &teacher, &course, &veli_id).await;
    let due_at = Timestamp::now().as_millis() + 86_400_000;
    let hw = common::create_homework(&app, &teacher, &course, &subject, "essay", due_at).await;

    // Ali hands in and is graded; Veli never hands in and is graded `missing`.
    let res = send(
        &app,
        "POST",
        &format!("/homework/{hw}/submission"),
        Some(&ali),
        Some(json!({ "text": "version A" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    for (user, status) in [(&ali_id, "done"), (&veli_id, "missing")] {
        let res = send(
            &app,
            "POST",
            &format!("/homework/{hw}/results"),
            Some(&teacher),
            Some(json!({ "user": user, "status": status })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    }

    // Age the row into the pre-stamp shape.
    db.query("UPDATE homework_submission UNSET graded_by_result;")
        .await
        .expect("age the row")
        .check()
        .expect("age the row");
    let app = reboot(&db).await;

    let mut stamps = db
        .query("SELECT VALUE graded_by_result FROM homework_submission; SELECT VALUE id FROM homework_submission;")
        .await
        .expect("read stamps")
        .check()
        .expect("read stamps");
    let stamped = stamps
        .take::<Vec<surrealdb::types::RecordId>>(0)
        .expect("graded_by_result");
    let rows = stamps
        .take::<Vec<surrealdb::types::RecordId>>(1)
        .expect("submission ids");
    assert_eq!(rows.len(), 1, "grading absent work must not conjure a row");
    assert_eq!(
        stamped.len(),
        1,
        "the surviving submission must come back stamped by its grade"
    );
    assert_eq!(
        format!("{:?}", stamped[0].key),
        format!("{:?}", rows[0].key),
        "the stamp must name the grade of that very (homework, user) pair"
    );
    assert_eq!(
        stamped[0].table.to_string(),
        "homework_result",
        "the stamp must point at the grade table"
    );

    // And the freeze bites again, which an unstamped row would not have done.
    let res = send(
        &app,
        "POST",
        &format!("/homework/{hw}/submission"),
        Some(&ali),
        Some(json!({ "text": "version B" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    // Veli, graded but never submitted, still reports as not-submitted.
    let res = send(
        &app,
        "GET",
        &format!("/homework/{hw}/submission"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
}

/// A menu written before the seat counter existed (2026-07-27) carries none,
/// which reads as zero — an uncapped menu for every seat already sold. The
/// backfill seeds it from the bookings that are still *held*: a cancelled one
/// gave its seat back long before the column existed, so counting rows flatly
/// would lock out a seat nobody holds.
#[tokio::test]
async fn menu_seats_are_seeded_from_the_bookings_that_are_still_held() {
    let (app, db) = common::app_and_db().await;
    let mgr = common::login_as(&app, &db, "seat_mgr", "manager").await;
    let ali = common::login(&app, "seat_ali").await;
    let veli = common::login(&app, "seat_veli").await;
    let ayse = common::login(&app, "seat_ayse").await;

    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&mgr),
        Some(json!({ "date": "2026-09-14", "slot": "lunch", "capacity": 2 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let menu = common::id_of(&res.body);

    let book = async |who: &str| {
        send(
            &app,
            "POST",
            &format!("/meals/menus/{menu}/bookings"),
            Some(who),
            Some(json!({})),
        )
        .await
    };
    let held = book(&ali).await;
    assert_eq!(held.status, StatusCode::CREATED, "{}", held.body);
    let given_back = common::id_of(&book(&veli).await.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{given_back}"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Age the menu into the pre-counter shape.
    db.query("UPDATE menu UNSET seats_booked;")
        .await
        .expect("age the row")
        .check()
        .expect("age the row");

    // Same database, so the router above keeps serving; what matters is that
    // the migration ran again over the aged row.
    let _ = reboot(&db).await;

    let mut counted = db
        .query("SELECT VALUE seats_booked FROM menu")
        .await
        .expect("counter read")
        .check()
        .expect("counter read");
    assert_eq!(
        counted.take::<Vec<i64>>(0).expect("counter column"),
        vec![1],
        "only the seat still held may be counted back"
    );

    // The seeded counter is a real cap, not a decoration: one seat is left.
    assert_eq!(book(&ayse).await.status, StatusCode::CREATED);
    assert_eq!(book(&veli).await.status, StatusCode::CONFLICT);
}

/// A subject written before its two reference counters existed (2026-07-27)
/// carries neither, and an absent counter reads as zero — which is exactly
/// "nothing points at me", so the delete guard would wave through a subject
/// half the school's questions are tagged with. The backfill counts the rows
/// that actually point at each subject, once.
///
/// The bite is the last assertion: seed the counters wrong (or not at all) and
/// the delete comes back `204` instead of `409`.
#[tokio::test]
async fn subject_reference_counts_are_seeded_from_the_rows_that_predate_them() {
    let (app, db) = common::app_and_db().await;
    let teacher = common::login_as(&app, &db, "seed_t", "teacher").await;
    let course = create_course(&app, &teacher, "biology").await;
    let tagged = create_subject(&app, &teacher, &course, "cells").await;
    let untouched = create_subject(&app, &teacher, &course, "genes").await;
    let exam = create_exam(&app, &teacher, &course, "quiz", "quiz").await;
    for text in ["what?", "why?"] {
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/questions"),
            Some(&teacher),
            Some(json!({
                "text": text,
                "kind": "text",
                "points": 1,
                "subject_id": tagged,
            })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    }
    let due = Timestamp::now().as_millis() + 86_400_000;
    common::create_homework(&app, &teacher, &course, &tagged, "mitosis", due).await;

    // Age both subjects into the pre-counter shape.
    db.query("UPDATE subject UNSET exam_question_count, homework_count;")
        .await
        .expect("age the rows")
        .check()
        .expect("age the rows");

    let app = reboot(&db).await;

    let mut counted = db
        .query(
            "SELECT VALUE [exam_question_count ?? 0, homework_count ?? 0] \
             FROM subject ORDER BY id ASC",
        )
        .await
        .expect("counter read")
        .check()
        .expect("counter read");
    assert_eq!(
        counted.take::<Vec<Vec<i64>>>(0).expect("counter columns"),
        vec![vec![2, 1], vec![0, 0]],
        "each subject must be seeded from the rows that point at it"
    );

    // The seeded counters are the delete guard itself, not a decoration.
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
        "DELETE",
        &format!("/subjects/{untouched}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
}

/// A term written before its refcount existed (2026-07-27) carries none, and an
/// absent counter reads as zero — which would let a manager delete the term half
/// the timetable still points at. The backfill counts the courses that actually
/// link to each term, once; a term nobody links to gets an explicit zero, and a
/// course with no term at all must not be counted into anyone's bucket.
///
/// The bite is the last pair of assertions: seed the counter wrong (or not at
/// all) and the linked term's delete comes back `204` instead of `409`.
#[tokio::test]
async fn term_course_counts_are_seeded_from_the_courses_that_link_to_them() {
    let (app, db) = common::app_and_db().await;
    let manager = common::login_as(&app, &db, "term_seed_m", "manager").await;

    async fn create_term(app: &Router, cookie: &str, name: &str, starts_at: i64) -> String {
        let res = send(
            app,
            "POST",
            "/terms",
            Some(cookie),
            Some(json!({ "name": name, "starts_at": starts_at, "ends_at": starts_at + 1 })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
        common::id_of(&res.body)
    }
    async fn link_course(app: &Router, cookie: &str, title: &str, term: Option<&str>) {
        let res = send(
            app,
            "POST",
            "/courses",
            Some(cookie),
            Some(json!({ "title": title, "term_id": term })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    }

    let linked = create_term(&app, &manager, "linked term", 1).await;
    let empty = create_term(&app, &manager, "empty term", 10).await;
    link_course(&app, &manager, "History", Some(&linked)).await;
    link_course(&app, &manager, "Physics", Some(&linked)).await;
    link_course(&app, &manager, "Floating", None).await;

    // Age both terms into the pre-counter shape.
    db.query("UPDATE term UNSET course_count;")
        .await
        .expect("age the rows")
        .check()
        .expect("age the rows");

    let app = reboot(&db).await;

    let mut counted = db
        .query("SELECT VALUE course_count FROM term ORDER BY starts_at ASC")
        .await
        .expect("counter read")
        .check()
        .expect("counter read");
    assert_eq!(
        counted.take::<Vec<i64>>(0).expect("counter column"),
        vec![2, 0],
        "each term must be seeded from the courses that link to it, \
         and the termless course from neither"
    );

    // The seeded counter is the delete guard itself, not a decoration.
    let res = send(
        &app,
        "DELETE",
        &format!("/terms/{linked}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/terms/{empty}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
}

/// The settings removal guards are two reference-counter tables (2026-07-27):
/// `kind_ref:<name>` counts the marks graded under an exam kind, `slot_ref:<name>`
/// the menus published for a meal slot, and a name may leave the list exactly
/// while its counter reads zero. A volume written before those tables existed
/// carries no rows at all, and a missing row reads as zero — which would let a
/// manager drop a kind the whole school is already graded under. The backfill
/// counts what actually points at each name, once; a name nobody ever used needs
/// no row (there is no zero pass here, deliberately).
///
/// The bite is the last three assertions: seed the counters at zero (or not at
/// all) and the removals come back `200` instead of `409`.
#[tokio::test]
async fn settings_reference_counts_are_seeded_from_the_marks_and_menus_that_predate_them() {
    let (app, db) = common::app_and_db().await;
    let teacher = common::login_as(&app, &db, "ref_seed_t", "teacher").await;
    let manager = common::login_as(&app, &db, "ref_seed_m", "manager").await;
    let ali = common::login(&app, "ref_seed_ali").await;
    let veli = common::login(&app, "ref_seed_veli").await;
    let (ali_id, veli_id) = (me_id(&app, &ali).await, me_id(&app, &veli).await);

    // Two marks under `final`, none under any other kind.
    let course = create_course(&app, &teacher, "Biology").await;
    enroll(&app, &teacher, &course, &ali_id).await;
    enroll(&app, &teacher, &course, &veli_id).await;
    let exam = create_exam(&app, &teacher, &course, "Final", "final").await;
    for student in [&ali_id, &veli_id] {
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/results"),
            Some(&teacher),
            Some(json!({ "user_id": student, "mark": 70 })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    }

    // Two menus under `lunch`, none under any other slot.
    for date in ["2026-09-21", "2026-09-22"] {
        let res = send(
            &app,
            "POST",
            "/meals/menus",
            Some(&manager),
            Some(json!({ "date": date, "slot": "lunch" })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    }

    // Age the volume into the pre-counter shape: the tables did not exist, so
    // neither did any of their rows.
    db.query("DELETE kind_ref; DELETE slot_ref;")
        .await
        .expect("age the volume")
        .check()
        .expect("age the volume");

    let app = reboot(&db).await;

    let mut counted = db
        .query(
            "SELECT VALUE [record::id(id), count ?? 0] FROM kind_ref ORDER BY id ASC; \
             SELECT VALUE [record::id(id), count ?? 0] FROM slot_ref ORDER BY id ASC;",
        )
        .await
        .expect("counter read")
        .check()
        .expect("counter read");
    assert_eq!(
        counted.take::<Vec<(String, i64)>>(0).expect("kind_ref"),
        vec![("final".to_string(), 2)],
        "the graded kind must be seeded from its marks, and only it"
    );
    assert_eq!(
        counted.take::<Vec<(String, i64)>>(1).expect("slot_ref"),
        vec![("lunch".to_string(), 2)],
        "the published slot must be seeded from its menus, and only it"
    );

    // The seeded counters are the removal guards themselves, not decoration:
    // the referenced names are stuck, an unreferenced one still leaves freely.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "exam_kinds": [{ "name": "quiz", "weight": 1 }] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "meal_slots": [{ "name": "breakfast" }, { "name": "snack" }] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&manager),
        Some(json!({ "meal_slots": [{ "name": "breakfast" }, { "name": "lunch" }] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

/// A fee plan's installments are an `array<object>` on the plan row, and the
/// charges it raised are frozen copies of them. A SCHEMAFULL re-migration
/// rewrites every field definition it owns, so this is where a nested array
/// would quietly come back as `NONE` or lose a key — the plan, the placement
/// and the money all have to survive a second boot unchanged.
#[tokio::test]
async fn fee_plans_and_their_charges_survive_remigration() {
    let (app, db) = common::app_and_db().await;
    let manager_creds = json!({ "username": "ali", "password": "secret1" });
    let student_creds = json!({ "username": "ayse", "password": "secret1" });
    for creds in [&manager_creds, &student_creds] {
        assert_eq!(
            send(&app, "POST", "/auth/register", None, Some((*creds).clone()))
                .await
                .status,
            StatusCode::CREATED
        );
    }
    set_role(&db, "ali", "manager").await;
    let manager = send(&app, "POST", "/auth/login", None, Some(manager_creds))
        .await
        .cookie
        .unwrap();
    let student = send(&app, "POST", "/auth/login", None, Some(student_creds))
        .await
        .cookie
        .unwrap();
    let student_id = me_id(&app, &student).await;

    let res = send(
        &app,
        "POST",
        "/payments/plans",
        Some(&manager),
        Some(json!({
            "name": "2026-2027 Yearly",
            "installments": [
                { "amount_minor": 150_000, "due_at": 1_760_000_000_123_i64 },
                { "amount_minor": 250_000, "due_at": 1_770_000_000_456_i64 },
            ],
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let plan = res.body["id"].as_str().unwrap().to_string();
    let res = send(
        &app,
        "POST",
        &format!("/payments/plans/{plan}/assignments"),
        Some(&manager),
        Some(json!({ "student_ids": [student_id] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let app = reboot(&db).await;

    let res = send(
        &app,
        "GET",
        &format!("/payments/plans/{plan}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["name"], "2026-2027 Yearly");
    let installments = res.body["installments"].as_array().expect("installments");
    assert_eq!(installments.len(), 2, "{}", res.body);
    // Order and both keys of each entry, not merely the count.
    assert_eq!(installments[0]["amount_minor"], 150_000);
    assert_eq!(installments[0]["due_at"], 1_760_000_000_123_i64);
    assert_eq!(installments[1]["amount_minor"], 250_000);
    assert_eq!(installments[1]["due_at"], 1_770_000_000_456_i64);

    let res = send(
        &app,
        "GET",
        &format!("/payments/statement/{student_id}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["entries"]["items"].as_array().unwrap().len(), 2);
    assert_eq!(
        res.body["balance_minor"], -400_000,
        "the charges the placement raised are still owed"
    );
}

/// A user row written before `palette_color` existed needs no backfill: the
/// column is `option<string>`, so the absent key reads back as "never chose"
/// and a later patch sets it in place. Adding an option column to a SCHEMAFULL
/// table must not strand the rows already there.
#[tokio::test]
async fn a_user_row_without_palette_color_still_reads_and_patches() {
    let (app, db) = common::app_and_db().await;
    let ali = common::login(&app, "ali").await;

    // Write the column first: a fresh row stores no key for a `None` option, so
    // UNSET alone would be a no-op and this test would prove nothing.
    let set = send(
        &app,
        "PATCH",
        "/users/me/preferences",
        Some(&ali),
        Some(json!({ "palette_color": "#283618" })),
    )
    .await;
    assert_eq!(set.status, StatusCode::OK, "{}", set.body);
    let stored: Vec<String> = db
        .query("SELECT VALUE palette_color FROM user WHERE palette_color != NONE;")
        .await
        .expect("read the column")
        .check()
        .expect("read the column")
        .take(0)
        .expect("read the column");
    assert_eq!(
        stored,
        vec!["#283618"],
        "the column must exist to be aged away"
    );

    // Now age the row into the pre-accent shape: the key goes away, exactly as
    // on a row a binary without this column wrote.
    db.query("UPDATE user UNSET palette_color;")
        .await
        .expect("age the row")
        .check()
        .expect("age the row");

    let app = reboot(&db).await;

    let me = send(&app, "GET", "/auth/me", Some(&ali), None).await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.body);
    assert!(
        me.body["palette_color"].is_null(),
        "an aged row must read as never-chosen, not error"
    );

    let set = send(
        &app,
        "PATCH",
        "/users/me/preferences",
        Some(&ali),
        Some(json!({ "palette_color": "#fefae0" })),
    )
    .await;
    assert_eq!(set.status, StatusCode::OK, "{}", set.body);
    assert_eq!(set.body["palette_color"], "#fefae0");
}

/// A whiteboard, its whole stroke log and the creator's `board_count` all
/// survive a second boot. `board_count` is the authority on how many boards a
/// creator holds, so a migration that reset it would ratchet a busy teacher's
/// limit — and the strokes are the one thing in this feature that a clear is
/// promised never to lose.
#[tokio::test]
async fn boards_and_their_strokes_survive_remigration() {
    let (app, db) = common::app_and_db().await;
    let creds = json!({ "username": "ali", "password": "secret1" });
    let cookie = common::login(&app, "ali").await;
    let veli = common::login(&app, "veli").await;
    let ali_id = me_id(&app, &cookie).await;
    let veli_id = me_id(&app, &veli).await;

    let res = send(
        &app,
        "POST",
        "/boards",
        Some(&cookie),
        Some(json!({ "title": "Geometri", "participants": [veli_id] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let board = res.body["id"].as_str().unwrap().to_string();

    // Two epochs: two marks cleared away, then one live mark.
    let draw = async |epoch: i64, payload: &str| {
        hezarfen_backend::domain::board_stroke::BoardStroke::append(
            &hezarfen_backend::domain::board::BoardId::from_key(&board),
            &hezarfen_backend::domain::user::UserId::from_key(&ali_id),
            payload,
            epoch,
            &db,
        )
        .await
        .expect("append")
    };
    draw(0, "{\"p\":[1]}").await;
    draw(0, "{\"p\":[2]}").await;
    let res = send(
        &app,
        "POST",
        &format!("/boards/{board}/clear"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    draw(1, "{\"p\":[3]}").await;

    // Second boot: the migration re-applied over live board data.
    let app = reboot(&db).await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds))
        .await
        .cookie
        .unwrap();

    let res = send(
        &app,
        "GET",
        &format!("/boards/{board}"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["title"], "Geometri");
    assert_eq!(res.body["creator"], ali_id.as_str());
    assert_eq!(res.body["participants"], json!([veli_id]));
    assert_eq!(res.body["epoch"], 1, "the epoch survived");
    assert!(res.body["closed_at"].is_null());

    // The log came back whole: 2 marks, the marker that counted them, 1 live.
    let res = send(
        &app,
        "GET",
        &format!("/boards/{board}/history"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 4, "{}", res.body);
    let rows = common::items(&res.body);
    assert_eq!(rows[0]["payload"], "{\"p\":[1]}");
    assert_eq!(rows[2]["kind"], "clear");
    assert_eq!(rows[2]["count"], 2, "the marker kept its count");
    assert_eq!(rows[3]["payload"], "{\"p\":[3]}");
    let res = send(
        &app,
        "GET",
        &format!("/boards/{board}/strokes"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(
        common::total(&res.body),
        1,
        "the live canvas is the new epoch"
    );

    // The counters the schema carries but the struct does not, straight from
    // the row: a re-migration must not have reset either of them.
    let mut result = db
        .query(
            "SELECT VALUE [epoch_stroke_count ?? 0, total_stroke_count ?? 0, \
             (SELECT VALUE board_count ?? 0 FROM $usr)[0]] FROM $id",
        )
        .bind((
            "id",
            hezarfen_backend::domain::board::BoardId::from_key(&board).record(),
        ))
        .bind((
            "usr",
            hezarfen_backend::domain::user::UserId::from_key(&ali_id).record(),
        ))
        .await
        .unwrap()
        .check()
        .unwrap();
    assert_eq!(
        result.take::<Vec<Vec<i64>>>(0).unwrap()[0],
        vec![1, 3, 1],
        "epoch counter, lifetime counter and the creator's board_count"
    );

    // And the board is still writable after the boot — the counters that came
    // back are the ones the cap reads.
    draw(1, "{\"p\":[4]}").await;
    let res = send(
        &app,
        "GET",
        &format!("/boards/{board}/strokes"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(
        common::total(&res.body),
        2,
        "drawing resumed after the boot"
    );
}

/// Every course's `enrollment_count`, straight off the store.
async fn seats_taken(db: &Database) -> Vec<i64> {
    db.query("SELECT VALUE enrollment_count FROM course;")
        .await
        .expect("read the counter")
        .check()
        .expect("read the counter")
        .take::<Vec<i64>>(0)
        .expect("enrollment_count")
}

/// An `enrollment` row written before the class layer (2026-08-01) carries no
/// `source` key at all, and that absence *is* the meaning — "a person placed
/// this student" — so there is nothing to backfill and everything to preserve.
/// Such a row must still decode once the column exists, still count towards its
/// course's cap, and never be adopted by a class that later attaches the same
/// course: the class skips a pair that already has a row, so the sweep on its
/// way out must find nothing of its own to take.
#[tokio::test]
async fn pre_class_enrollments_keep_their_absent_source() {
    let (app, db) = common::app_and_db().await;
    let manager = common::login_as(&app, &db, "mgr", "manager").await;
    let teacher = common::login_as(&app, &db, "teacher", "teacher").await;
    let ali = common::login(&app, "ali").await;
    let veli = common::login(&app, "veli").await;
    let (ali_id, veli_id) = (me_id(&app, &ali).await, me_id(&app, &veli).await);

    let course = create_course(&app, &teacher, "algebra").await;
    enroll(&app, &teacher, &course, &ali_id).await;
    enroll(&app, &teacher, &course, &veli_id).await;

    // Age both rows into the pre-class shape — no `source` key — and the course
    // into the pre-counter shape a volume that old also carries.
    db.query("UPDATE enrollment UNSET source; UPDATE course UNSET enrollment_count;")
        .await
        .expect("age the rows")
        .check()
        .expect("age the rows");

    let app = reboot(&db).await;

    // They decode, and the counter is seeded from them.
    let roster = send(
        &app,
        "GET",
        &format!("/courses/{course}/enrollments"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(roster.status, StatusCode::OK, "{}", roster.body);
    assert_eq!(
        common::total(&roster.body),
        2,
        "a sourceless row must still read: {}",
        roster.body
    );
    assert_eq!(seats_taken(&db).await, vec![2], "seeded from the roster");

    // A class attaching that same course finds ali already seated: no second
    // seat is charged, and their row keeps its absent `source`…
    let res = send(
        &app,
        "POST",
        "/classes",
        Some(&manager),
        Some(json!({ "name": "9-A" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let class = common::id_of(&res.body);
    let res = send(
        &app,
        "POST",
        &format!("/classes/{class}/members"),
        Some(&manager),
        Some(json!({ "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/classes/{class}/courses"),
        Some(&manager),
        Some(json!({ "course_id": course })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(seats_taken(&db).await, vec![2], "no second seat charged");

    // …so the detach sweeps nothing, and the pre-class rows outlive the class.
    let res = send(
        &app,
        "DELETE",
        &format!("/classes/{class}/courses/{course}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
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
        2,
        "hand-placed rows are never swept by a class: {}",
        roster.body
    );
    assert_eq!(
        seats_taken(&db).await,
        vec![2],
        "and no seat was given back"
    );
}
