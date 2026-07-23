//! Boot-migration tests: age rows into the shapes old binaries left behind,
//! then re-run the idempotent migration on the same database — a second boot —
//! and assert the backfills repair (or deliberately destroy) them. The storage
//! engine itself is the SurrealDB server's concern since the embedded engine
//! was retired; these tests run on the in-memory engine like every other suite.

mod common;

use axum::Router;
use axum::http::StatusCode;
use common::{create_course, create_exam, enroll, me_id, send, set_role};
use hezarfen_backend::database::Database;
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
                     "points": 10, "choices": ["3", "4"], "correct": 1 }),
        ),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let question_id = common::id_of(&res.body);
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
        Some(json!({ "question_id": question_id, "selected": 1 })),
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
        ChatbotMessage::list_for_thread(&id, &db)
            .await
            .expect("thread")
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
        ChatbotThread::list_for_user(&owner, &db)
            .await
            .unwrap()
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
