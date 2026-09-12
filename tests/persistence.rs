//! Boot-migration tests: seed a real deployment through the API, re-run the
//! idempotent migration over the same live database — a second boot — and
//! assert the seeded rows come back intact. The backfill-era machinery these
//! tests once exercised is gone with the old engine; what remains is the
//! durability contract: a migration must never disturb data that is already
//! there.

mod common;

use axum::Router;
use axum::http::StatusCode;
use common::{create_course, create_subject, enroll, me_id, send, set_role};
use hezarfen_backend::database::Database;
use hezarfen_backend::module::ModuleSet;
use hezarfen_backend::rate_limit::RateLimitConfig;
use hezarfen_backend::state::AppState;
use hezarfen_backend::tenant::{Slug, Tenants};
use hezarfen_backend::{build_router, database};
use serde_json::json;

/// Re-apply the school schema to the live school database and hand back a
/// fresh router over the same deployment — a second boot in every way that
/// matters to these tests.
async fn reboot(db: &Database, tenants: &Tenants) -> Router {
    database::migrate_school(db).await.expect("re-migration");
    build_router(AppState {
        db: tenants.control().clone(),
        tenants: tenants.clone(),
        files_path: common::files_dir(),
        cookie_secure: false,
        rate_limit: RateLimitConfig::unlimited(),
        chatbot_limit: Default::default(),
        exam_presence: Default::default(),
        board_hub: Default::default(),
        ai: None,
        metrics: hezarfen_backend::telemetry::Metrics::noop(),
    })
}

/// School policy and terms survive a second boot — the settings singleton
/// (nested band objects included) and the course→term link both come back,
/// untouched by the re-applied idempotent migration.
#[tokio::test]
async fn settings_and_terms_survive_remigration() {
    let (app, db, tenants) = common::app_and_tenants().await;

    let creds = json!({ "school": "demo", "username": "boss", "password": "secret1" });
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
    let app = reboot(&db, &tenants).await;
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

/// A weekly appointment series and a booking that carries a full proposal
/// triple survive the re-applied migration, exact unix-ms stamps and all.
#[tokio::test]
async fn appointments_survive_remigration() {
    /// Far enough ahead to clear `check_not_past` without touching the clock,
    /// and deliberately not a round number of seconds.
    const START: i64 = 1_900_000_000_123;
    const END: i64 = 1_900_000_003_777;
    const WEEK: i64 = 7 * 24 * 60 * 60 * 1000;
    const PROPOSED_START: i64 = START + 5_000_001;
    const PROPOSED_END: i64 = START + 8_000_003;

    let (app, db, tenants) = common::app_and_tenants().await;
    let teacher_creds = json!({ "school": "demo", "username": "ali", "password": "secret1" });
    let student_creds = json!({ "school": "demo", "username": "ayse", "password": "secret1" });
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
    // row then carries every optional column at once.
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
        Some(json!({
            "proposed_starts_at": PROPOSED_START,
            "proposed_ends_at": PROPOSED_END,
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Second boot: migration re-applied over live data.
    let app = reboot(&db, &tenants).await;
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
    let (app, db, tenants) = common::app_and_tenants().await;
    let teacher_creds = json!({ "school": "demo", "username": "ali", "password": "secret1" });
    let student_creds = json!({ "school": "demo", "username": "ayse", "password": "secret1" });
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
    let app = reboot(&db, &tenants).await;
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
    use hezarfen_backend::db::{chatbot_message, chatbot_thread};
    use hezarfen_backend::domain::chatbot_message::{ChatContent, MessageStatus};
    use hezarfen_backend::domain::user::UserId;

    let (app, db, tenants) = common::app_and_tenants().await;
    let owner_cookie = common::login(&app, "ali").await;
    let other_cookie = common::login(&app, "ayse").await;
    let owner = UserId::from_key(&me_id(&app, &owner_cookie).await);
    let other = UserId::from_key(&me_id(&app, &other_cookie).await);

    let thread = chatbot_thread::create_capped(&db, &owner, None)
        .await
        .expect("create");
    let id = thread.get_id().clone();
    let prompt = chatbot_message::append_user(
        &db,
        &id,
        &owner,
        ChatContent::try_new("selam").expect("content"),
    )
    .await
    .expect("append user");
    let reply = chatbot_message::append_pending_assistant(&db, &id, &owner)
        .await
        .expect("append assistant");
    assert_eq!(reply.get_status(), MessageStatus::Pending);

    let reply = chatbot_message::complete(
        &db,
        reply.get_id(),
        ChatContent::try_new("aleykum selam").expect("content"),
        false,
    )
    .await
    .expect("complete");
    assert_eq!(reply.get_status(), MessageStatus::Complete);
    assert_eq!(reply.get_content().as_str(), "aleykum selam");
    assert!(reply.get_completed_at().is_some());
    assert_eq!(
        chatbot_message::list_for_thread(&db, &id, None, 0)
            .await
            .expect("thread")
            .0
            .len(),
        2
    );

    // The other user sees nothing of it, by thread or by message.
    assert!(
        chatbot_thread::read_for(&db, &id, &other)
            .await
            .expect("cross-user thread")
            .is_none()
    );
    assert!(
        chatbot_message::read_for(&db, prompt.get_id(), &other)
            .await
            .expect("cross-user message")
            .is_none()
    );
    assert_eq!(
        chatbot_thread::count_for_user(&db, &other)
            .await
            .expect("count"),
        0
    );
    assert_eq!(
        chatbot_thread::count_for_user(&db, &owner)
            .await
            .expect("count"),
        1
    );

    chatbot_thread::delete(&db, thread).await.expect("delete");
    assert!(
        chatbot_thread::read_for(&db, &id, &owner)
            .await
            .expect("deleted thread")
            .is_none()
    );
    assert_eq!(
        chatbot_thread::list_for_user(&db, &owner, None, 0)
            .await
            .expect("owner's list after delete")
            .0
            .len(),
        0
    );

    // Second boot: the migration re-applied over the swept store — and no
    // `chatbot_message` row is left anywhere.
    let _app = reboot(&db, &tenants).await;
    let left = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM chatbot_message")
        .fetch_one(&db)
        .await
        .expect("sweep");
    assert_eq!(left, 0, "no chatbot_message row survived the delete");
}

/// A meal-slot list whose entries carry no serving minute survives a reboot
/// and is still patchable afterwards — an omitted optional never wedges the
/// settings row.
#[tokio::test]
async fn settings_slots_without_a_serving_minute_still_patch() {
    let (app, db, tenants) = common::app_and_tenants().await;

    let creds = json!({ "school": "demo", "username": "boss", "password": "secret1" });
    send(&app, "POST", "/auth/register", None, Some(creds.clone())).await;
    set_role(&db, "boss", "manager").await;

    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&send(&app, "POST", "/auth/login", None, Some(creds.clone()))
            .await
            .cookie
            .unwrap()),
        Some(json!({ "meal_slots": [{ "name": "lunch" }] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Second boot, then read and patch again.
    let app = reboot(&db, &tenants).await;
    let cookie = send(&app, "POST", "/auth/login", None, Some(creds))
        .await
        .cookie
        .unwrap();

    let res = send(&app, "GET", "/settings", Some(&cookie), None).await;
    assert_eq!(res.status, StatusCode::OK);
    let slots = res.body["meal_slots"].as_array().expect("meal slots");
    assert_eq!(slots.len(), 1, "{}", res.body);
    assert_eq!(slots[0]["name"], "lunch");

    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&cookie),
        Some(json!({ "meal_slots": [
            { "name": "lunch" },
            { "name": "breakfast" },
        ] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

/// Fee plans, their installment shape and the charges a placement raised all
/// come back through a second boot: order, both keys of every installment,
/// and the statement's arithmetic.
#[tokio::test]
async fn fee_plans_and_their_charges_survive_remigration() {
    let (app, db, tenants) = common::app_and_tenants().await;
    let manager_creds = json!({ "school": "demo", "username": "ali", "password": "secret1" });
    let student_creds = json!({ "school": "demo", "username": "ayse", "password": "secret1" });
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

    let app = reboot(&db, &tenants).await;

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

/// Boards, their stroke log across epochs, and the counters the cap reads all
/// survive a second boot — and the board is still writable afterwards.
#[tokio::test]
async fn boards_and_their_strokes_survive_remigration() {
    let (app, db, tenants) = common::app_and_tenants().await;
    let creds = json!({ "school": "demo", "username": "ali", "password": "secret1" });
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
        hezarfen_backend::db::board_stroke::append(
            &db,
            &hezarfen_backend::domain::board::BoardId::from_key(&board),
            &hezarfen_backend::domain::user::UserId::from_key(&ali_id),
            payload,
            epoch,
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
    let app = reboot(&db, &tenants).await;
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
    // the rows: a re-migration must not have reset either of them.
    use sqlx::Row as _;
    let board_id = hezarfen_backend::domain::board::BoardId::from_key(&board);
    let row = sqlx::query(
        "SELECT epoch_stroke_count, total_stroke_count FROM board WHERE id = $1",
    )
    .bind(board_id.uuid())
    .fetch_one(&db)
    .await
    .expect("board row");
    let epoch_count: i64 = row.try_get(0).unwrap();
    let total_count: i64 = row.try_get(1).unwrap();
    let user_id = hezarfen_backend::domain::user::UserId::from_key(&ali_id);
    let board_count: i64 =
        sqlx::query_scalar("SELECT board_count FROM app_user WHERE id = $1")
            .bind(user_id.uuid())
            .fetch_one(&db)
            .await
            .expect("creator row");
    assert_eq!(
        (epoch_count, total_count, board_count),
        (1, 4, 1),
        "epoch counter, lifetime counter (three strokes and the clear marker \
         they paid for) and the creator's board_count"
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

/// An empty module set is a decision, never a hole to fill: a school created
/// with nothing switched on still reads an empty list after a second boot of
/// the control schema.
#[tokio::test]
async fn an_empty_module_list_survives_a_second_boot() {
    let tenants = database::init_test_tenants().await;
    let slug = Slug::try_new("bare").unwrap();
    tenants
        .create(&slug, "Bare School", ModuleSet::empty())
        .await
        .expect("a school with nothing switched on");

    database::migrate_control(tenants.control())
        .await
        .expect("a second boot");

    use sqlx::Row as _;
    let stored: Option<Vec<String>> = sqlx::query(
        "SELECT modules FROM school WHERE slug = $1",
    )
    .bind(slug.as_str())
    .fetch_optional(tenants.control())
    .await
    .expect("read the row back")
    .map(|row| row.try_get::<Vec<String>, _>(0).unwrap());
    assert_eq!(
        stored.unwrap_or_default(),
        Vec::<String>::new(),
        "an empty list is a decision, never a hole to fill"
    );
}

/// The control migration run twice over schools seeded through the registry
/// leaves every school row exactly as it stood — idempotence is the whole
/// contract of a second boot.
#[tokio::test]
async fn probe_control_migration_is_idempotent_over_aged_rows() {
    let tenants = database::init_test_tenants().await;
    let control = tenants.control().clone();

    // Seed through the registry path itself: three schools, one per module
    // posture the product distinguishes.
    let mut narrow = ModuleSet::all();
    narrow.remove(hezarfen_backend::module::Module::try_from_str("marks").unwrap());
    for (slug, name, modules) in [
        ("full", "Full School", ModuleSet::all()),
        ("empty", "Empty School", ModuleSet::empty()),
        ("narrow", "Narrow School", narrow),
    ] {
        tenants
            .create(&Slug::try_new(slug).unwrap(), name, modules)
            .await
            .unwrap_or_else(|err| panic!("create {slug}: {err}"));
    }

    let before = school_rows(&control).await;
    // The harness's demo school plus the three seeded here.
    assert_eq!(before.len(), 4, "demo plus the three seeded schools");

    database::migrate_control(&control)
        .await
        .expect("a second control boot");
    database::migrate_control(&control)
        .await
        .expect("a third control boot");

    let after = school_rows(&control).await;
    assert_eq!(before, after, "a re-run must not disturb school rows");
}

/// Every school row as `(slug, name, status, modules)`, in slug order.
async fn school_rows(control: &Database) -> Vec<(String, String, String, Vec<String>)> {
    use sqlx::Row as _;
    sqlx::query("SELECT slug, name, status, modules FROM school ORDER BY slug")
        .fetch_all(control)
        .await
        .expect("school rows")
        .into_iter()
        .map(|row| {
            (
                row.try_get::<String, _>(0).unwrap(),
                row.try_get::<String, _>(1).unwrap(),
                row.try_get::<String, _>(2).unwrap(),
                row.try_get::<Vec<String>, _>(3).unwrap(),
            )
        })
        .collect()
}
