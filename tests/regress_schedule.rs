//! Regressions for the weekly-plan materializer, the holiday calendar, and
//! the per-section course lists. Router-level, same shape as the other
//! `regress_*` suites: every test mints its own school and drives the real
//! router over `tests/common`.
//!
//! Every instant here is derived from the clock plus the demo school's fixed
//! zone offset (`Europe/Istanbul`, +180 minutes): the next Monday is rounded
//! forward once per process, so the suite never ages into the past-`from`
//! refusal while two calls in one run still name the same day. Each named date
//! asserts its own weekday, so a bad derivation fails loudly instead of
//! quietly moving the lesson to a Tuesday.

mod common;

use axum::http::StatusCode;
use chrono::{DateTime, Datelike, NaiveDate, Weekday};
use common::{
    app_and_db, attach_instance, create_class, create_course, create_session, ensure_year, enroll,
    id_of, items, login, login_as, me_id, send, taught,
};
use serde_json::{json, Value};

// ---- the calendar helpers ----------------------------------------------------

/// The demo school's zone offset: `Europe/Istanbul`, +180 minutes.
const OFFSET_MIN: i64 = 180;
const DAY_MS: i64 = 86_400_000;

/// A local-midnight instant for `day` in the school's zone.
fn day_start(day: NaiveDate) -> i64 {
    day.and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis()
        - OFFSET_MIN * 60_000
}

/// The school-zone day an instant lands on.
fn local_day(millis: i64) -> NaiveDate {
    DateTime::from_timestamp_millis(millis + OFFSET_MIN * 60_000)
        .unwrap()
        .date_naive()
}

/// Minutes an instant sits past its local midnight — `540` is 09:00.
fn minute_of_day(millis: i64) -> i64 {
    (millis - day_start(local_day(millis))) / 60_000
}

/// A whole local day, as a materialize request names it.
fn day_range(day: NaiveDate) -> (i64, i64) {
    (day_start(day), day_start(day) + DAY_MS - 1)
}

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

/// The Monday the generation tests materialize onto: the next Monday strictly
/// ahead of the clock, so the suite never ages into the past-`from` refusal.
/// Cached for the process, so every call in one run names the same day.
fn lesson_monday() -> NaiveDate {
    static DAY: std::sync::LazyLock<NaiveDate> = std::sync::LazyLock::new(|| {
        let today = local_day(hezarfen_backend::domain::timestamp::Timestamp::now().as_millis());
        // Days to the next Monday — 7 when today *is* Monday, because today's
        // midnight is already behind the clock.
        let ahead = (7 - today.weekday().num_days_from_monday() as i64) % 7;
        today + chrono::Duration::days(if ahead == 0 { 7 } else { ahead })
    });
    let day = *DAY;
    assert_eq!(day.weekday(), Weekday::Mon);
    day
}

/// A second Monday, one week out.
fn later_monday() -> NaiveDate {
    let day = lesson_monday() + chrono::Duration::days(7);
    assert_eq!(day.weekday(), Weekday::Mon);
    day
}

/// The Wednesday of that same week — the weekday a Monday-only range must not
/// touch, and the one a Wednesday slot must generate on.
fn some_wednesday() -> NaiveDate {
    let day = lesson_monday() + chrono::Duration::days(2);
    assert_eq!(day.weekday(), Weekday::Wed);
    day
}

/// A Monday comfortably in the past, for the past-`from` refusal.
fn past_monday() -> NaiveDate {
    let day = date(2020, 1, 6);
    assert_eq!(day.weekday(), Weekday::Mon);
    day
}

// ---- the fixtures ------------------------------------------------------------

/// Add one slot to a section's own weekly plan (asserts 201); returns the
/// slot id. `topic` rides the request the way the contract's optional field
/// does — absent means the domain fallback names the generated lesson.
async fn add_slot(
    app: &axum::Router,
    cookie: &str,
    instance: &str,
    weekday: i16,
    starts_min: i64,
    ends_min: i64,
    topic: Option<&str>,
) -> String {
    let mut body = json!({ "weekday": weekday, "starts_at": starts_min, "ends_at": ends_min });
    if let Some(topic) = topic {
        body["topic"] = json!(topic);
    }
    let res = send(
        app,
        "POST",
        &format!("/instances/{instance}/weekly-plan"),
        Some(cookie),
        Some(body),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "add slot: {}", res.body);
    id_of(&res.body)
}

/// Assign `teacher_id` to run `instance` (asserts 200) — staffing is what the
/// materializer's teacher gate reads.
async fn assign_teacher(app: &axum::Router, manager: &str, instance: &str, teacher_id: &str) {
    let res = send(
        app,
        "POST",
        &format!("/instances/{instance}/teachers"),
        Some(manager),
        Some(json!({ "user_id": teacher_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "assign teacher: {}", res.body);
}

/// One materialize call. `apply: None` omits the flag, exercising the
/// request's default (a dry run).
async fn materialize(
    app: &axum::Router,
    cookie: &str,
    instance: &str,
    from: i64,
    to: i64,
    apply: Option<bool>,
) -> common::Res {
    let mut body = json!({ "from": from, "to": to });
    if let Some(apply) = apply {
        body["apply"] = json!(apply);
    }
    send(
        app,
        "POST",
        &format!("/instances/{instance}/weekly-plan/materialize"),
        Some(cookie),
        Some(body),
    )
    .await
}

/// The full sessions page of `instance` — the envelope a dry run must not
/// disturb. Compared whole, so any change in any row or count fails.
async fn sessions_page(app: &axum::Router, cookie: &str, instance: &str) -> Value {
    let res = send(
        app,
        "GET",
        &format!("/instances/{instance}/sessions"),
        Some(cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "list sessions: {}", res.body);
    res.body
}

/// The one-holiday calendar: `name` spanning `[starts, ends)` as manager.
async fn create_holiday(
    app: &axum::Router,
    manager: &str,
    name: &str,
    starts: i64,
    ends: i64,
) -> Value {
    let res = send(
        app,
        "POST",
        "/holidays",
        Some(manager),
        Some(json!({ "name": name, "starts_at": starts, "ends_at": ends, "kind": "resmi" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "create holiday: {}", res.body);
    res.body
}

async fn list_holidays(
    app: &axum::Router,
    cookie: &str,
    from: Option<i64>,
    to: Option<i64>,
) -> Vec<Value> {
    let uri = match (from, to) {
        (Some(from), Some(to)) => format!("/holidays?from={from}&to={to}"),
        _ => "/holidays".to_string(),
    };
    let res = send(app, "GET", &uri, Some(cookie), None).await;
    assert_eq!(res.status, StatusCode::OK, "{uri}: {}", res.body);
    items(&res.body).clone()
}

/// The materialize tests' shared setup: a taught instance whose own weekly
/// plan carries one Monday 09:00–09:40 slot, with `teacher` assigned to run
/// it when the test needs generation to pass the staffing gate. Returns the
/// academic stack and, when staffed, the assigned teacher's user id.
async fn planned_monday(
    app: &axum::Router,
    db: &hezarfen_backend::database::Database,
    manager: &str,
    staffed: bool,
) -> (common::Taught, Option<String>) {
    let t = taught(app, manager, "Matematik").await;
    add_slot(app, manager, &t.instance, 1, 540, 580, None).await;
    let teacher_id = if staffed {
        let teacher = login_as(app, db, "teach", "teacher").await;
        let teacher_id = me_id(app, &teacher).await;
        assign_teacher(app, manager, &t.instance, &teacher_id).await;
        Some(teacher_id)
    } else {
        None
    };
    (t, teacher_id)
}

/// One catalogue course attached to two şubes — 9-A and 12-B — where the
/// 12-B section carries its own resolved-title override. This is the shape
/// the per-section course lists exist to show.
async fn two_sections(
    app: &axum::Router,
    manager: &str,
) -> (String, String, String, String, String) {
    let year = ensure_year(app, manager).await;
    let course = create_course(app, manager, "Fizik").await;
    let class_a = create_class(app, manager, "Fizik 9-A", json!({ "year": year })).await;
    let class_b =
        create_class(app, manager, "Fizik 12-B", json!({ "year": year, "grade_level": 12 })).await;
    let instance_a = attach_instance(app, manager, &class_a, &course).await;
    let instance_b = attach_instance(app, manager, &class_b, &course).await;
    let res = send(
        app,
        "PATCH",
        &format!("/instances/{instance_b}"),
        Some(manager),
        Some(json!({ "title": "12-B Fizik" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "title override: {}", res.body);
    (course, class_a, class_b, instance_a, instance_b)
}

// ---- the holiday calendar ------------------------------------------------------

#[tokio::test]
async fn a_manager_creates_reads_patches_and_deletes_a_holiday() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let manager_id = me_id(&app, &manager).await;

    let starts = day_start(lesson_monday());
    let ends = day_start(later_monday());
    let row = create_holiday(&app, &manager, "29 Ekim", starts, ends).await;
    assert_eq!(row["name"], "29 Ekim");
    assert_eq!(row["kind"], "resmi");
    assert_eq!(row["starts_at"], json!(starts));
    assert_eq!(row["ends_at"], json!(ends));
    assert_eq!(row["creator"]["id"], json!(manager_id), "creator: {}", row);
    assert!(
        row["created_at"].as_i64().is_some(),
        "created_at: {}",
        row
    );
    let id = id_of(&row);

    let got = send(&app, "GET", &format!("/holidays/{id}"), Some(&manager), None).await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.body);
    assert_eq!(got.body["name"], "29 Ekim");

    // PATCH touches only the fields it names.
    let patched = send(
        &app,
        "PATCH",
        &format!("/holidays/{id}"),
        Some(&manager),
        Some(json!({ "name": "Cumhuriyet Bayramı" })),
    )
    .await;
    assert_eq!(patched.status, StatusCode::OK, "{}", patched.body);
    assert_eq!(patched.body["name"], "Cumhuriyet Bayramı");
    assert_eq!(patched.body["kind"], "resmi", "untouched kind: {}", patched.body);
    assert_eq!(patched.body["ends_at"], json!(ends), "untouched end: {}", patched.body);

    let gone = send(&app, "DELETE", &format!("/holidays/{id}"), Some(&manager), None).await;
    assert_eq!(gone.status, StatusCode::NO_CONTENT, "{}", gone.body);

    let list = list_holidays(&app, &manager, None, None).await;
    assert!(
        list.iter().all(|row| row["id"] != json!(id)),
        "deleted holiday still listed: {list:?}"
    );
    let missing = send(&app, "GET", &format!("/holidays/{id}"), Some(&manager), None).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND, "{}", missing.body);
}

#[tokio::test]
async fn a_student_cannot_write_the_calendar_but_may_read_it() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let student = login(&app, "stu").await;

    let starts = day_start(lesson_monday());
    let ends = day_start(later_monday());
    let row = create_holiday(&app, &manager, "29 Ekim", starts, ends).await;
    let id = id_of(&row);

    let refused = send(
        &app,
        "POST",
        "/holidays",
        Some(&student),
        Some(json!({ "name": "x", "starts_at": starts, "ends_at": ends, "kind": "resmi" })),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    let patched = send(
        &app,
        "PATCH",
        &format!("/holidays/{id}"),
        Some(&student),
        Some(json!({ "name": "y" })),
    )
    .await;
    assert_eq!(patched.status, StatusCode::FORBIDDEN, "{}", patched.body);
    let deleted = send(&app, "DELETE", &format!("/holidays/{id}"), Some(&student), None).await;
    assert_eq!(deleted.status, StatusCode::FORBIDDEN, "{}", deleted.body);

    // Reads are any-role: the calendar is school-wide structure.
    let list = list_holidays(&app, &student, None, None).await;
    assert_eq!(list.len(), 1, "the student reads the calendar: {list:?}");
    let got = send(&app, "GET", &format!("/holidays/{id}"), Some(&student), None).await;
    assert_eq!(got.status, StatusCode::OK, "{}", got.body);
}

#[tokio::test]
async fn an_inverted_range_is_refused_on_create_and_patch() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;

    let starts = day_start(lesson_monday());
    let ends = day_start(later_monday());
    let refused = send(
        &app,
        "POST",
        "/holidays",
        Some(&manager),
        Some(json!({ "name": "ters", "starts_at": ends, "ends_at": starts, "kind": "resmi" })),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    let list = list_holidays(&app, &manager, None, None).await;
    assert!(list.is_empty(), "the refused create wrote nothing: {list:?}");

    let row = create_holiday(&app, &manager, "29 Ekim", starts, ends).await;
    let id = id_of(&row);
    for body in [
        json!({ "starts_at": ends + 1 }), // starts alone, past the stored end
        json!({ "ends_at": starts - 1 }), // end alone, before the stored start
    ] {
        let patched = send(
            &app,
            "PATCH",
            &format!("/holidays/{id}"),
            Some(&manager),
            Some(body.clone()),
        )
        .await;
        assert_eq!(
            patched.status,
            StatusCode::BAD_REQUEST,
            "patch {body}: {}",
            patched.body
        );
    }
    let intact = send(&app, "GET", &format!("/holidays/{id}"), Some(&manager), None).await;
    assert_eq!(intact.body["starts_at"], json!(starts), "{}", intact.body);
    assert_eq!(intact.body["ends_at"], json!(ends), "{}", intact.body);
}

#[tokio::test]
async fn the_list_filter_keeps_a_holiday_touching_the_range_at_either_edge() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;

    // Overlap is `ends_at >= from && starts_at <= to` — both touching edges
    // are inside, one step past either is out.
    let starts = 1_790_000_000_000;
    let ends = starts + 10 * DAY_MS;
    let row = create_holiday(&app, &manager, "Bayram", starts, ends).await;
    let id = id_of(&row);

    for (from, to, expect) in [
        (Some(starts), Some(starts + 5 * DAY_MS), true),
        (Some(ends), Some(ends + 10 * DAY_MS), true), // touches at ends_at == from
        (Some(ends + 1), Some(ends + 10 * DAY_MS), false),
        (Some(starts - 10 * DAY_MS), Some(starts), true), // touches at starts_at == to
        (Some(starts - 10 * DAY_MS), Some(starts - 1), false),
        (None, None, true),
    ] {
        let list = list_holidays(&app, &manager, from, to).await;
        let has = list.iter().any(|row| row["id"] == json!(id));
        assert_eq!(has, expect, "from={from:?} to={to:?}: {list:?}");
    }
}

// ---- the materializer: the happy paths -----------------------------------------

#[tokio::test]
async fn a_dry_run_counts_one_candidate_and_writes_nothing() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let (t, _) = planned_monday(&app, &db, &manager, true).await;

    let before = sessions_page(&app, &manager, &t.instance).await;
    let (from, to) = day_range(lesson_monday());
    let res = materialize(&app, &manager, &t.instance, from, to, None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["from"], json!(from), "{}", res.body);
    assert_eq!(res.body["to"], json!(to), "{}", res.body);
    assert_eq!(res.body["applied"], json!(false), "{}", res.body);
    assert_eq!(res.body["slots"], json!(1), "the resolved plan's size: {}", res.body);
    assert_eq!(res.body["candidates"], json!(1), "{}", res.body);
    assert_eq!(res.body["range_days"], json!(1), "{}", res.body);
    assert_eq!(res.body["skipped_existing"], json!(0), "{}", res.body);
    assert_eq!(res.body["skipped_holiday"], json!(0), "{}", res.body);
    assert_eq!(res.body["created"], json!([]), "{}", res.body);
    assert_eq!(res.body["blocked"], json!([]), "{}", res.body);

    let after = sessions_page(&app, &manager, &t.instance).await;
    assert_eq!(before, after, "a dry run must not touch the session list");
    assert_eq!(after["total"], json!(0), "{}", after);
}

#[tokio::test]
async fn an_apply_generates_the_slots_own_lesson_at_the_slots_own_time() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let teacher = login_as(&app, &db, "teach", "teacher").await;
    let teacher_id = me_id(&app, &teacher).await;

    let t = taught(&app, &manager, "Matematik").await;
    add_slot(&app, &manager, &t.instance, 1, 540, 580, Some("Denklemler")).await;
    assign_teacher(&app, &manager, &t.instance, &teacher_id).await;

    let (from, to) = day_range(lesson_monday());
    let res = materialize(&app, &manager, &t.instance, from, to, Some(true)).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["applied"], json!(true), "{}", res.body);
    assert_eq!(res.body["candidates"], json!(1), "{}", res.body);
    assert_eq!(res.body["created"].as_array().unwrap().len(), 1, "{}", res.body);

    let lesson = &res.body["created"][0];
    assert_eq!(lesson["topic"], "Denklemler", "the slot's topic rides: {}", lesson);
    assert_eq!(lesson["teacher"]["id"], json!(teacher_id), "teacher: {}", lesson);
    assert_eq!(lesson["class_course"], json!(t.instance), "{}", lesson);
    assert_eq!(lesson["ends_at"], json!(day_start(lesson_monday()) + 580 * 60_000));
    // The lesson lands on that Monday at 09:00 *school time* — minute-of-day
    // 540 against the +180 zone, not 09:00 UTC.
    let starts = lesson["starts_at"].as_i64().expect("starts_at");
    assert_eq!(starts, day_start(lesson_monday()) + 540 * 60_000, "{}", lesson);
    assert_eq!(local_day(starts), lesson_monday(), "{}", lesson);
    assert_eq!(minute_of_day(starts), 540, "{}", lesson);

    let page = sessions_page(&app, &manager, &t.instance).await;
    assert_eq!(page["total"], json!(1), "{}", page);
    assert_eq!(page["items"][0]["id"], lesson["id"], "listed: {}", page);
}

#[tokio::test]
async fn a_slot_without_a_topic_names_the_lesson_after_the_resolved_title() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let (t, _) = planned_monday(&app, &db, &manager, true).await;

    // The section's own title override is what every generated lesson of it
    // is named after when the slot carries no topic of its own.
    let patched = send(
        &app,
        "PATCH",
        &format!("/instances/{}", t.instance),
        Some(&manager),
        Some(json!({ "title": "9-A Matematik" })),
    )
    .await;
    assert_eq!(patched.status, StatusCode::OK, "{}", patched.body);

    let (from, to) = day_range(lesson_monday());
    let res = materialize(&app, &manager, &t.instance, from, to, Some(true)).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let lesson = &res.body["created"][0];
    assert_eq!(lesson["topic"], "9-A Matematik", "{}", lesson);
    assert_eq!(local_day(lesson["starts_at"].as_i64().unwrap()), lesson_monday());
}

#[tokio::test]
async fn a_second_apply_reports_the_first_lessons_as_skipped() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let (t, _) = planned_monday(&app, &db, &manager, true).await;

    let (from, to) = day_range(lesson_monday());
    let first = materialize(&app, &manager, &t.instance, from, to, Some(true)).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    assert_eq!(first.body["created"].as_array().unwrap().len(), 1, "{}", first.body);

    let second = materialize(&app, &manager, &t.instance, from, to, Some(true)).await;
    assert_eq!(second.status, StatusCode::OK, "{}", second.body);
    assert_eq!(second.body["applied"], json!(true), "{}", second.body);
    assert_eq!(second.body["created"], json!([]), "{}", second.body);
    assert_eq!(second.body["skipped_existing"], json!(1), "{}", second.body);
    // `candidates` counts the rows that would be *inserted*: an already
    // scheduled instant and a holiday-blocked day are excluded from it and
    // reported in their own `skipped_existing` / `skipped_holiday` fields.
    assert_eq!(second.body["candidates"], json!(0), "{}", second.body);

    let page = sessions_page(&app, &manager, &t.instance).await;
    assert_eq!(page["total"], json!(1), "the re-apply wrote nothing: {}", page);
}

#[tokio::test]
async fn a_holiday_blocks_its_days_and_the_report_names_it() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let (t, _) = planned_monday(&app, &db, &manager, true).await;

    let (from, to) = day_range(lesson_monday());
    let row = create_holiday(&app, &manager, "Bayram tatili", from, to).await;
    assert_eq!(row["name"], "Bayram tatili");

    let res = materialize(&app, &manager, &t.instance, from, to, None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["candidates"], json!(0), "{}", res.body);
    assert_eq!(res.body["skipped_holiday"], json!(1), "{}", res.body);
    assert_eq!(res.body["created"], json!([]), "{}", res.body);
    let blocked = res.body["blocked"].as_array().expect("blocked");
    assert_eq!(blocked.len(), 1, "{}", res.body);
    assert_eq!(blocked[0]["date"], json!(lesson_monday().to_string()), "{}", res.body);
    assert_eq!(blocked[0]["holiday"], json!("Bayram tatili"), "{}", res.body);

    let page = sessions_page(&app, &manager, &t.instance).await;
    assert_eq!(page["total"], json!(0), "a blocked day generates nothing: {}", page);
}

// ---- the materializer: the refusals --------------------------------------------

#[tokio::test]
async fn an_empty_own_plan_refuses_to_materialize() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let t = taught(&app, &manager, "Matematik").await;

    // Adding then dropping one slot leaves the section with an authoritative
    // *own* plan that is empty — exactly the state the refusal exists for.
    // No teacher is assigned on purpose: the empty-plan refusal must win.
    let slot = add_slot(&app, &manager, &t.instance, 1, 540, 580, None).await;
    let dropped = send(
        &app,
        "DELETE",
        &format!("/instances/{}/weekly-plan/{slot}", t.instance),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(dropped.status, StatusCode::NO_CONTENT, "{}", dropped.body);
    let plan = send(
        &app,
        "GET",
        &format!("/instances/{}/weekly-plan", t.instance),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(plan.status, StatusCode::OK, "{}", plan.body);
    assert_eq!(plan.body["weekly_plan_inherited"], json!(false), "{}", plan.body);
    assert_eq!(plan.body["weekly_plan"], json!([]), "{}", plan.body);

    let (from, to) = day_range(lesson_monday());
    let res = materialize(&app, &manager, &t.instance, from, to, None).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert_eq!(res.body["code"], "instance_has_no_weekly_plan", "{}", res.body);
}

#[tokio::test]
async fn an_unstaffed_instance_refuses_to_materialize() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let t = taught(&app, &manager, "Matematik").await;

    // Attaching seeds no teacher rows, so the plan is real (the section's own
    // slot) while the staffing gate has nobody to hand the lessons to.
    add_slot(&app, &manager, &t.instance, 1, 540, 580, None).await;
    let plan = send(
        &app,
        "GET",
        &format!("/instances/{}/weekly-plan", t.instance),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(plan.body["weekly_plan"].as_array().unwrap().len(), 1, "{}", plan.body);

    let (from, to) = day_range(lesson_monday());
    let res = materialize(&app, &manager, &t.instance, from, to, None).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert_eq!(res.body["code"], "instance_has_no_teacher", "{}", res.body);
}

#[tokio::test]
async fn materialize_rejects_a_bad_range_before_anything_runs() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    // Staffed on purpose: the empty-plan and teacher gates run *before* the
    // range cap, so a bare instance would answer 409 and the 400 this test is
    // about would never fire.
    let (t, _) = planned_monday(&app, &db, &manager, true).await;

    // More than a year of days.
    let from = day_start(lesson_monday());
    let res = materialize(&app, &manager, &t.instance, from, from + 367 * DAY_MS, None).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // `from` in the past — the same rule a hand-created lesson answers to.
    let res = materialize(
        &app,
        &manager,
        &t.instance,
        day_start(past_monday()),
        day_start(later_monday()),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // A zero-width window (`from == to`) is *not* inverted: it names exactly
    // one school day, the same way `POST /holidays` and every other dated
    // route here treat `ends_at == starts_at` as a valid single instant
    // (`check_time_range` refuses only `ends < starts`). Assert the boundary
    // itself rather than guessing it.
    let res = materialize(&app, &manager, &t.instance, from, from, None).await;
    assert_eq!(res.status, StatusCode::OK, "one-instant range: {}", res.body);
    assert_eq!(res.body["range_days"], json!(1), "{}", res.body);

    // An inverted window — `from` after `to` — is the 400 the range check
    // exists for.
    let res = materialize(
        &app,
        &manager,
        &t.instance,
        day_start(some_wednesday()),
        day_start(lesson_monday()),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // An instant the day math cannot represent: shifting by the zone offset
    // overflows chrono's range and `zoned_day` panics there. The route must
    // refuse with a 400 that names the field — for `i64::MAX` and `i64::MIN`
    // alike — before anything runs.
    for (field, at) in [("from", i64::MAX), ("to", i64::MAX), ("from", i64::MIN), ("to", i64::MIN)] {
        let (from, to) = match field {
            "from" => (at, day_start(later_monday())),
            _ => (day_start(lesson_monday()), at),
        };
        let res = materialize(&app, &manager, &t.instance, from, to, None).await;
        assert_eq!(res.status, StatusCode::BAD_REQUEST, "{field}={at}: {}", res.body);
        let body = res.body.to_string();
        assert!(
            body.contains(field),
            "the 400 must name `{field}`: {body}"
        );
    }

    // A sane range through the same gate still works — the guard refuses
    // magnitudes, not the ordinary calendar.
    let (from, to) = day_range(lesson_monday());
    let res = materialize(&app, &manager, &t.instance, from, to, None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let page = sessions_page(&app, &manager, &t.instance).await;
    assert_eq!(page["total"], json!(0), "nothing was written: {}", page);
}

#[tokio::test]
async fn an_archived_year_refuses_to_materialize() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let (t, _) = planned_monday(&app, &db, &manager, true).await;

    let archived = send(
        &app,
        "POST",
        &format!("/academic-years/{}/archive", t.year),
        Some(&manager),
        None,
    )
    .await;
    assert!(
        archived.status.is_success(),
        "archive: {}",
        archived.body
    );

    let (from, to) = day_range(lesson_monday());
    let res = materialize(&app, &manager, &t.instance, from, to, None).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert_eq!(res.body["code"], "academic_year_archived", "{}", res.body);
}

// ---- the materializer: the uniqueness wall --------------------------------------

#[tokio::test]
async fn a_hand_created_session_on_a_generated_slot_is_a_coded_conflict() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let (t, _) = planned_monday(&app, &db, &manager, true).await;

    let (from, to) = day_range(lesson_monday());
    let res = materialize(&app, &manager, &t.instance, from, to, Some(true)).await;
    assert_eq!(res.body["created"].as_array().unwrap().len(), 1, "{}", res.body);
    let starts = res.body["created"][0]["starts_at"].as_i64().unwrap();

    // The unique index on (class_course, starts_at) comes back as the coded
    // 409, never a raw 500.
    let clash = send(
        &app,
        "POST",
        &format!("/instances/{}/sessions", t.instance),
        Some(&manager),
        Some(json!({ "starts_at": starts })),
    )
    .await;
    assert_eq!(clash.status, StatusCode::CONFLICT, "{}", clash.body);
    assert_eq!(clash.body["code"], "session_time_taken", "{}", clash.body);

    // A different instant is not caught by the same wall.
    let ok = send(
        &app,
        "POST",
        &format!("/instances/{}/sessions", t.instance),
        Some(&manager),
        Some(json!({ "starts_at": starts + 60_000 })),
    )
    .await;
    assert_eq!(ok.status, StatusCode::CREATED, "{}", ok.body);
    let page = sessions_page(&app, &manager, &t.instance).await;
    assert_eq!(page["total"], json!(2), "{}", page);
}

/// The PATCH door answers the same coded 409, not a 500: moving one lesson's
/// `starts_at` onto an instant the section already holds trips the same
/// `UNIQUE (class_course, starts_at)` the create door maps. A PATCH to a free
/// instant still succeeds.
#[tokio::test]
async fn a_session_moved_onto_an_occupied_instant_is_a_coded_conflict() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let (t, _) = planned_monday(&app, &db, &manager, true).await;

    // Two lessons one minute apart.
    create_session(&app, &manager, &t.instance, day_start(lesson_monday()) + 540 * 60_000).await;
    let second = create_session(&app, &manager, &t.instance, day_start(lesson_monday()) + 601 * 60_000).await;

    // PATCH the second onto the first's instant: the unique wall, through the
    // update path, must be the coded 409 — never a raw 500.
    let moved = send(
        &app,
        "PATCH",
        &format!("/sessions/{second}"),
        Some(&manager),
        Some(json!({ "starts_at": day_start(lesson_monday()) + 540 * 60_000 })),
    )
    .await;
    assert_eq!(moved.status, StatusCode::CONFLICT, "{}", moved.body);
    assert_eq!(moved.body["code"], "session_time_taken", "{}", moved.body);

    // The refused PATCH wrote nothing: both lessons still sit where they did.
    let page = sessions_page(&app, &manager, &t.instance).await;
    assert_eq!(page["total"], json!(2), "{}", page);

    // A PATCH onto a free instant is not caught by the same wall.
    let ok = send(
        &app,
        "PATCH",
        &format!("/sessions/{second}"),
        Some(&manager),
        Some(json!({ "starts_at": day_start(lesson_monday()) + 661 * 60_000 })),
    )
    .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
    assert_eq!(
        ok.body["starts_at"],
        json!(day_start(lesson_monday()) + 661 * 60_000),
        "{}",
        ok.body
    );
}

// ---- the materializer: shape of the generated set --------------------------------

#[tokio::test]
async fn a_multi_candidate_dry_run_still_writes_nothing() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let (t, _) = planned_monday(&app, &db, &manager, true).await;
    add_slot(&app, &manager, &t.instance, 1, 600, 640, None).await;

    let before = sessions_page(&app, &manager, &t.instance).await;
    let (from, to) = day_range(lesson_monday());
    let res = materialize(&app, &manager, &t.instance, from, to, None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["applied"], json!(false), "{}", res.body);
    assert_eq!(res.body["slots"], json!(2), "two Monday slots: {}", res.body);
    assert_eq!(res.body["candidates"], json!(2), "{}", res.body);
    assert_eq!(res.body["created"], json!([]), "{}", res.body);

    let after = sessions_page(&app, &manager, &t.instance).await;
    assert_eq!(before, after, "a dry run with real candidates wrote nothing");
    assert_eq!(after["total"], json!(0), "{}", after);
}

#[tokio::test]
async fn a_range_generates_only_on_the_slots_own_weekday() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let teacher = login_as(&app, &db, "teach", "teacher").await;
    let teacher_id = me_id(&app, &teacher).await;

    let t = taught(&app, &manager, "Matematik").await;
    add_slot(&app, &manager, &t.instance, 3, 540, 580, None).await; // Wednesday
    assign_teacher(&app, &manager, &t.instance, &teacher_id).await;

    // The range names only the Monday: no slot matches, and — the point —
    // no holiday was skipped either. The report must say *why* nothing came
    // out, not merely that nothing did.
    let (from, to) = day_range(lesson_monday());
    let res = materialize(&app, &manager, &t.instance, from, to, Some(true)).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["applied"], json!(true), "{}", res.body);
    assert_eq!(res.body["candidates"], json!(0), "{}", res.body);
    assert_eq!(res.body["skipped_holiday"], json!(0), "{}", res.body);
    assert_eq!(res.body["created"], json!([]), "{}", res.body);
    assert_eq!(res.body["blocked"], json!([]), "{}", res.body);

    let page = sessions_page(&app, &manager, &t.instance).await;
    assert_eq!(page["total"], json!(0), "{}", page);

    // The mirror image, so the zero above is the weekday match and nothing
    // else: the same slot over the range that *does* name its weekday
    // generates exactly that day's lesson.
    let (wed_from, wed_to) = day_range(some_wednesday());
    let res = materialize(&app, &manager, &t.instance, wed_from, wed_to, Some(true)).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["candidates"], json!(1), "{}", res.body);
    let lesson = &res.body["created"][0];
    let starts = lesson["starts_at"].as_i64().expect("starts_at");
    assert_eq!(local_day(starts), some_wednesday(), "{}", lesson);
    assert_eq!(minute_of_day(starts), 540, "{}", lesson);

    let page = sessions_page(&app, &manager, &t.instance).await;
    assert_eq!(page["total"], json!(1), "{}", page);
}

// ---- the per-section course lists ------------------------------------------------

#[tokio::test]
async fn the_catalogue_page_lists_a_courses_sections_with_their_resolved_titles() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let (course, class_a, class_b, instance_a, instance_b) = two_sections(&app, &manager).await;

    let res = send(&app, "GET", "/courses", Some(&manager), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let row = items(&res.body)
        .iter()
        .find(|row| row["id"] == json!(course))
        .expect("the course is listed");
    assert_eq!(row["class_course_count"], json!(2), "{}", row);

    let sections = row["sections"].as_array().expect("sections").clone();
    assert_eq!(sections.len(), 2, "{}", row);
    let mut titles: Vec<&str> = sections.iter().map(|s| s["title"].as_str().unwrap()).collect();
    titles.sort();
    titles.dedup();
    assert_eq!(
        titles,
        vec!["12-B Fizik", "Fizik"],
        "one section inherited, one overrode: {}",
        row
    );

    for (class, instance, grade, title, class_name) in [
        (&class_a, &instance_a, 9, "Fizik", "Fizik 9-A"),
        (&class_b, &instance_b, 12, "12-B Fizik", "Fizik 12-B"),
    ] {
        let section = sections
            .iter()
            .find(|s| s["class"] == json!(class))
            .unwrap_or_else(|| panic!("no section for {class}: {row}"));
        assert_eq!(section["id"], json!(instance), "{}", section);
        assert_eq!(section["course"], json!(course), "{}", section);
        assert_eq!(section["class_name"], json!(class_name), "{}", section);
        assert_eq!(section["grade_level"], json!(grade), "{}", section);
        assert_eq!(section["title"], json!(title), "{}", section);
        assert_eq!(section["enrollment_count"], json!(0), "{}", section);
        assert_eq!(section["teachers"], json!([]), "attach seeds no teachers: {}", section);
    }
}

#[tokio::test]
async fn a_student_sees_only_the_sections_of_the_courses_they_reach() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let (course, class_a, _, instance_a, _) = two_sections(&app, &manager).await;

    let student = login(&app, "stu").await;
    let student_id = me_id(&app, &student).await;
    enroll(&app, &manager, &instance_a, &student_id).await;

    let res = send(&app, "GET", "/courses", Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let row = items(&res.body)
        .iter()
        .find(|row| row["id"] == json!(course))
        .expect("the student's course is listed");
    let sections = row["sections"].as_array().expect("sections").clone();
    assert_eq!(sections.len(), 1, "only the enrolled section: {}", row);
    assert_eq!(sections[0]["id"], json!(instance_a), "{}", row);
    assert_eq!(sections[0]["class"], json!(class_a), "{}", row);
}

#[tokio::test]
async fn courses_me_lists_one_row_per_section_with_the_resolved_title() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let (course, class_a, _, instance_a, instance_b) = two_sections(&app, &manager).await;

    // One student standing in *both* sections of the same ders: two rows,
    // each resolved, not one catalogue row.
    let other = login(&app, "stu2").await;
    let other_id = me_id(&app, &other).await;
    enroll(&app, &manager, &instance_a, &other_id).await;
    enroll(&app, &manager, &instance_b, &other_id).await;

    // The 9-A section gets its own title too, so both rows can be told apart
    // from the catalogue title they inherit from.
    let patched = send(
        &app,
        "PATCH",
        &format!("/instances/{instance_a}"),
        Some(&manager),
        Some(json!({ "title": "9-A Fizik" })),
    )
    .await;
    assert_eq!(patched.status, StatusCode::OK, "{}", patched.body);

    let res = send(&app, "GET", "/courses/me", Some(&other), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let rows = items(&res.body).clone();
    assert_eq!(rows.len(), 2, "one row per section: {}", res.body);

    let section = rows
        .iter()
        .find(|row| row["id"] == json!(instance_a))
        .unwrap_or_else(|| panic!("no row for {instance_a}: {}", res.body));
    assert_eq!(section["course"], json!(course), "{}", section);
    assert_eq!(section["class"], json!(class_a), "{}", section);
    assert_eq!(section["class_name"], json!("Fizik 9-A"), "{}", section);
    assert_eq!(section["title"], json!("9-A Fizik"), "resolved, not catalogue: {}", section);
    assert_eq!(section["grade_level"], json!(9), "{}", section);
    // The hours are the section's *resolved* number, not merely an integer:
    // read the section's own `GET /instances/{id}` and compare the two.
    let own = send(
        &app,
        "GET",
        &format!("/instances/{instance_a}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(own.status, StatusCode::OK, "{}", own.body);
    assert_eq!(
        section["ders_saati"], own.body["ders_saati"],
        "resolved hours: {}",
        section
    );
    assert_eq!(section["enrollment_count"], json!(1), "{}", section);

    let other_row = rows
        .iter()
        .find(|row| row["id"] == json!(instance_b))
        .unwrap_or_else(|| panic!("no row for {instance_b}: {}", res.body));
    assert_eq!(other_row["title"], json!("12-B Fizik"), "{}", other_row);
    assert_eq!(other_row["grade_level"], json!(12), "{}", other_row);
}

#[tokio::test]
async fn the_profile_block_lists_sections_with_their_class_and_grade() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let (course, class_a, _, instance_a, _) = two_sections(&app, &manager).await;

    let student = login(&app, "stu").await;
    let student_id = me_id(&app, &student).await;
    enroll(&app, &manager, &instance_a, &student_id).await;

    // `GET /users/{id}/profile` is the surface that carries the course block;
    // the bare `/users/{id}` answers a `UserResponse` with no `courses` key.
    let res = send(
        &app,
        "GET",
        &format!("/users/{student_id}/profile"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let rows = res.body["courses"].as_array().expect("profile courses").clone();
    assert_eq!(rows.len(), 1, "one section, one row: {}", res.body);
    let row = &rows[0];
    assert_eq!(row["id"], json!(instance_a), "the section is the row: {}", row);
    assert_eq!(row["course"], json!(course), "{}", row);
    assert_eq!(row["class"], json!(class_a), "{}", row);
    assert_eq!(row["class_name"], json!("Fizik 9-A"), "{}", row);
    assert_eq!(row["grade_level"], json!(9), "{}", row);
    assert_eq!(row["title"], json!("Fizik"), "resolved title (inherited here): {}", row);
    assert_eq!(row["kind"], json!("course"), "{}", row);
}
