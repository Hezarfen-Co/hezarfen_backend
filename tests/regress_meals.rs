//! Regressions for the food program's money and its menus. Each test here
//! stands for one defect that shipped: a seat that could not be replayed, a
//! seat nobody could cancel, a debt nobody could settle, a mark that outlived
//! its menu, a dish edit that outlived its own.
//!
//! Router-level, same shape as `integration.rs`, plus a few direct domain calls
//! where the defect sits *below* a handler's read (a mark landing after the
//! menu is gone is not reachable through the route, which reads it first).

mod common;

use axum::http::StatusCode;
use common::{app_and_db, id_of, login, login_as, me_id, send, set_role};
use hezarfen_backend::domain::meal_attendance::{MealAttendance, MealAttendanceStatus};
use hezarfen_backend::domain::menu::MenuId;
use hezarfen_backend::domain::user::UserId;
use hezarfen_backend::error::AppError;
use serde_json::json;

/// Publish a menu for `date`, uncapped unless told otherwise.
async fn publish(app: &axum::Router, mgr: &str, date: &str) -> String {
    let res = send(
        app,
        "POST",
        "/meals/menus",
        Some(mgr),
        Some(json!({ "date": date, "slot": "lunch" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    id_of(&res.body)
}

async fn add_dish(app: &axum::Router, mgr: &str, menu: &str, price: i64) -> String {
    let res = send(
        app,
        "POST",
        &format!("/meals/menus/{menu}/dishes"),
        Some(mgr),
        Some(json!({ "name": "çorba", "price_minor": price })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    id_of(&res.body)
}

/// One student's balance as the API answers it.
async fn balance_of(app: &axum::Router, cookie: &str, student: &str) -> i64 {
    let res = send(
        app,
        "GET",
        &format!("/meals/balance/{student}"),
        Some(cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    res.body["balance_minor"].as_i64().expect("a balance")
}

/// A repeat `POST` of a seat the student already holds must replay that seat,
/// whatever the menu costs *now*. Pricing the menu before the already-booked
/// short circuit made it a `400` ("amount_minor must be between…") the moment
/// the dishes summed past the chargeable maximum — refusing to hand back a seat
/// over a price it was never going to be billed.
#[tokio::test]
async fn a_repeat_booking_replays_the_held_seat_however_dear_the_menu_gets() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "dear_mgr", "manager").await;
    let ali = login(&app, "dear_ali").await;
    let menu = publish(&app, &mgr, "2026-09-14").await;
    add_dish(&app, &mgr, &menu, 1_000).await;

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

    // The menu is re-dressed until a seat on it could not be billed at all
    // (`MAX_LEDGER_AMOUNT_MINOR` is 10 000 000 minor units).
    for _ in 0..11 {
        add_dish(&app, &mgr, &menu, 1_000_000).await;
    }

    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&ali),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(id_of(&res.body), booking, "the same seat, replayed");
    assert_eq!(res.body["status"], "booked");

    // …and the replay bills nothing new: the seat still owes its own price.
    let res = send(
        &app,
        "GET",
        &format!("/meals/balance/{}", me_id(&app, &ali).await),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.body["balance_minor"], -1_000);

    // A student who has no seat yet is still refused outright, rather than
    // being handed one nothing can charge for.
    let veli = login(&app, "dear_veli").await;
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&veli),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
}

/// A seat outlives the role that took it. Only the student or their parent may
/// cancel, so a promoted student's live seat used to be uncancellable: the menu
/// refused its own delete forever and the charge could never be reversed, since
/// cancelling is the only route that appends a reversal. Manager+ may cancel
/// any seat, and nothing sweeps bookings on a role change — moving money is a
/// decision, never a side effect.
#[tokio::test]
async fn a_manager_can_cancel_the_seat_of_someone_who_is_no_longer_a_student() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "promo_mgr", "manager").await;
    let ali = login(&app, "promo_ali").await;
    let ali_id = me_id(&app, &ali).await;
    let menu = publish(&app, &mgr, "2026-09-15").await;
    add_dish(&app, &mgr, &menu, 4_500).await;

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

    // Ali becomes staff. The seat is still held, still counted, still charged.
    set_role(&db, "promo_ali", "teacher").await;
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{booking}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "a teacher does not book meals, so they do not cancel their own either"
    );

    let res = send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{booking}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "cancelled");

    // The seat is genuinely back — the menu may be unpublished — and so is the
    // money: the reversal landed with the flip.
    let res = send(
        &app,
        "GET",
        &format!("/meals/balance/{ali_id}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.body["balance_minor"], 0);
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/menus/{menu}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);

    // A classmate still cannot cancel someone else's seat.
    let menu = publish(&app, &mgr, "2026-09-16").await;
    let veli = login(&app, "promo_veli").await;
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&veli),
        Some(json!({})),
    )
    .await;
    let seat = id_of(&res.body);
    let can = login(&app, "promo_can").await;
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{seat}"),
        Some(&can),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
}

/// A debt survives its debtor's role change, so settling it has to. The credit
/// route took students only, which made such a debt permanently unpayable —
/// there is no other route that appends a credit. A target with no ledger
/// history at all is still refused, so a typo'd staff id cannot take money.
#[tokio::test]
async fn a_credit_settles_a_debt_that_outlived_the_students_role() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "credit_admin", "admin").await;
    let ali = login(&app, "credit_ali").await;
    let ali_id = me_id(&app, &ali).await;
    let menu = publish(&app, &admin, "2026-09-17").await;
    add_dish(&app, &admin, &menu, 4_500).await;
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&ali),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    set_role(&db, "credit_ali", "teacher").await;
    let res = send(
        &app,
        "POST",
        "/meals/credits",
        Some(&admin),
        Some(json!({ "student_id": ali_id, "amount_minor": 4_500 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/meals/balance/{ali_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(res.body["balance_minor"], 0, "the debt is settled");

    // Staff who never ate here carry no balance, so the typo still bites.
    let clean = login_as(&app, &db, "credit_clean", "teacher").await;
    let clean_id = me_id(&app, &clean).await;
    let res = send(
        &app,
        "POST",
        "/meals/credits",
        Some(&admin),
        Some(json!({ "student_id": clean_id, "amount_minor": 4_500 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
}

/// A credit resent after a lost response must not take the money twice. The id
/// was a fresh ulid, so the ledger's identity-based idempotence could never
/// bite: a proxy timing out anywhere after the commit left the client with a
/// second credit and no route to undo it, since nothing here edits or deletes a
/// line. A `request_key` folds into the id, exactly as it does on
/// `POST /payments/credits`, and the same key for a different amount is a `409`
/// rather than a `201` that hides a client bug behind a stored line.
#[tokio::test]
async fn a_credit_replayed_with_its_request_key_records_the_money_once() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "key_admin", "admin").await;
    let ali = login(&app, "key_ali").await;
    let ali_id = me_id(&app, &ali).await;
    let credit = |body: serde_json::Value| {
        let (app, admin) = (app.clone(), admin.clone());
        async move { send(&app, "POST", "/meals/credits", Some(&admin), Some(body)).await }
    };
    let body =
        json!({ "student_id": ali_id, "amount_minor": 25_000, "request_key": "receipt-114" });

    let first = credit(body.clone()).await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);
    // The retry: same body, same line back, and no second one behind it.
    let again = credit(body.clone()).await;
    assert_eq!(again.status, StatusCode::CREATED, "{}", again.body);
    assert_eq!(again.body["id"], first.body["id"]);

    // The stored ledger, not the two responses: only one line ever landed.
    let res = send(
        &app,
        "GET",
        &format!("/meals/ledger/{ali_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(res.body["total"], 1, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/meals/balance/{ali_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(res.body["balance_minor"], 25_000, "credited once");

    // The same key for other money is a client bug, and is refused as one.
    let res = credit(json!({
        "student_id": ali_id, "amount_minor": 30_000, "request_key": "receipt-114"
    }))
    .await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    // A key with the id separator in it cannot be spelled at all: it would let
    // one line's key derive another's id.
    let res = credit(json!({
        "student_id": ali_id, "amount_minor": 30_000, "request_key": "receipt_114"
    }))
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    // …and without a key a resent request is a second credit, as before.
    let plain = json!({ "student_id": ali_id, "amount_minor": 1_000 });
    assert_eq!(credit(plain.clone()).await.status, StatusCode::CREATED);
    assert_eq!(credit(plain).await.status, StatusCode::CREATED);
    let res = send(
        &app,
        "GET",
        &format!("/meals/balance/{ali_id}"),
        Some(&admin),
        None,
    )
    .await;
    assert_eq!(res.body["balance_minor"], 27_000, "{}", res.body);
}

/// A mark must not create a row for a menu that is gone. The handler reads the
/// menu first, so this is driven below it — exactly the interleaving a mark in
/// flight while `DELETE /meals/menus/{id}` commits produces. The stake is not
/// tidiness: menu ids are deterministic on (date, slot), so an orphan mark comes
/// back as a mark on the *next* menu published for that meal.
#[tokio::test]
async fn a_mark_for_a_menu_that_is_gone_writes_no_row() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "mark_mgr", "manager").await;
    let ali = login(&app, "mark_ali").await;
    let ali_id = UserId::from_key(&me_id(&app, &ali).await);
    let mgr_id = UserId::from_key(&me_id(&app, &mgr).await);
    let menu = MenuId::from_key(&publish(&app, &mgr, "2026-09-18").await);

    // The menu goes while the mark is on its way down.
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/menus/{}", menu.key()),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);

    let marked = MealAttendance::mark(
        &menu,
        &ali_id,
        MealAttendanceStatus::try_new("served").unwrap(),
        &mgr_id,
        &db,
    )
    .await
    .expect_err("the mark is refused");
    assert!(
        matches!(marked, AppError::NotFound),
        "no menu, no mark: {marked:?}"
    );
    assert_eq!(
        MealAttendance::list_for_menu(&menu, None, 0, &db)
            .await
            .unwrap()
            .1,
        0,
        "the refusal must have written nothing"
    );

    // Republishing that very day and slot mints the same id — and must not
    // inherit a mark for a meal nobody attended.
    let again = publish(&app, &mgr, "2026-09-18").await;
    assert_eq!(again, menu.key(), "the id is deterministic on date+slot");
    let res = send(
        &app,
        "GET",
        &format!("/meals/menus/{again}/attendance"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 0, "{}", res.body);

    // The live menu still takes marks, and re-marking corrects in place.
    for status in ["served", "missed"] {
        let res = send(
            &app,
            "POST",
            &format!("/meals/menus/{again}/attendance"),
            Some(&mgr),
            Some(json!({ "student_id": ali_id.key(), "status": status })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        assert_eq!(res.body["status"], status);
    }
    let res = send(
        &app,
        "GET",
        &format!("/meals/menus/{again}/attendance"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 1, "one row per (menu, person)");
}

/// A dish write is fenced by its menu's existence, because the revision bump it
/// rides on *is* that fence: both are one transaction now, so an edit can no
/// longer land on a menu that has been unpublished — which is the same window
/// that let a booking see a bumped revision with the old price.
#[tokio::test]
async fn a_dish_write_is_refused_once_its_menu_is_gone() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "orphan_mgr", "manager").await;
    let menu = publish(&app, &mgr, "2026-09-19").await;
    let dish = add_dish(&app, &mgr, &menu, 1_000).await;

    // The menu row alone is removed, leaving the dish exactly as a delete
    // interrupted between the row and its cascade would have.
    db.query("DELETE $id")
        .bind(("id", MenuId::from_key(&menu).record()))
        .await
        .unwrap()
        .check()
        .unwrap();

    let res = send(
        &app,
        "PATCH",
        &format!("/meals/dishes/{dish}"),
        Some(&mgr),
        Some(json!({ "price_minor": 9_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/dishes/{dish}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);

    // …and on a live menu both still work, at a fresh revision each time.
    let menu = publish(&app, &mgr, "2026-09-20").await;
    let dish = add_dish(&app, &mgr, &menu, 1_000).await;
    let res = send(
        &app,
        "PATCH",
        &format!("/meals/dishes/{dish}"),
        Some(&mgr),
        Some(json!({ "price_minor": 9_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["price_minor"], 9_000);
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/dishes/{dish}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
}

/// Unpublishing takes the dishes with it *in the delete's own transaction*: the
/// menu id is deterministic, so a cascade that ran afterwards and was cut short
/// left the next menu for that meal serving — and pricing — the old one's food.
#[tokio::test]
async fn unpublishing_takes_the_dishes_with_the_row() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "cascade_mgr", "manager").await;
    let menu = publish(&app, &mgr, "2026-09-21").await;
    add_dish(&app, &mgr, &menu, 1_000).await;
    send(
        &app,
        "DELETE",
        &format!("/meals/menus/{menu}"),
        Some(&mgr),
        None,
    )
    .await;

    let again = publish(&app, &mgr, "2026-09-21").await;
    assert_eq!(again, menu, "the id is deterministic on date+slot");
    let res = send(
        &app,
        "GET",
        &format!("/meals/menus/{again}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        res.body["dishes"].as_array().unwrap().len(),
        0,
        "a republished meal starts empty"
    );

    // A seat on it is therefore free, not billed for food nobody serves.
    let ali = login(&app, "cascade_ali").await;
    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{again}/bookings"),
        Some(&ali),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/meals/balance/{}", me_id(&app, &ali).await),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.body["balance_minor"], 0);
}

/// A slot name goes verbatim into the menu's record id, and that id is a URL
/// path segment. A slot named `a/b` published a menu at an address no route
/// could ever match again — unreadable, uneditable, undeletable.
///
/// The settings now refuse the name outright, so the only way such a slot can
/// still be on a school's list is a row written before that rule — which is
/// what is staged straight into the store here. This gate is what that row
/// runs into.
#[tokio::test]
async fn a_slot_whose_name_would_break_the_menu_url_cannot_be_published() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "slot_admin", "admin").await;
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&admin),
        Some(json!({ "meal_slots": [{"name": "a/b"}, {"name": "lunch"}] })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::BAD_REQUEST,
        "the settings must refuse the name themselves: {}",
        res.body
    );

    // The legal half of that edit lands, which is what creates the row...
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&admin),
        Some(json!({ "meal_slots": [{"name": "lunch"}] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // ...and the stale row is that list as a database written before the rule.
    db.query(
        "UPDATE settings:school SET meal_slots = \
         [{ name: 'a/b', serving_minute: NONE }, { name: 'lunch', serving_minute: NONE }]",
    )
    .await
    .unwrap()
    .check()
    .unwrap();

    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&admin),
        Some(json!({ "date": "2026-09-22", "slot": "a/b" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    // The school's other slots are unaffected.
    let res = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&admin),
        Some(json!({ "date": "2026-09-22", "slot": "lunch" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
}

/// The cutoff exists to stop students moving the kitchen's headcount at the
/// last minute. It bound *staff* too, and that half-closed the very wedge
/// manager-cancel was widened to open: past the deadline the seat could not be
/// freed by anyone, and a menu refuses its own delete while a seat is held —
/// so the menu was undeletable and the charge unreversible, forever. Manager+
/// now cancels through a closed cutoff; students and parents still do not.
#[tokio::test]
async fn a_closed_cutoff_still_lets_a_manager_free_the_seat() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "shut_adm", "admin").await;
    let mgr = login_as(&app, &db, "shut_mgr", "manager").await;
    let ali = login(&app, "shut_ali").await;
    let ali_id = me_id(&app, &ali).await;
    // A day long past — booked while the school ran no deadline at all.
    let menu = publish(&app, &mgr, "2020-01-02").await;
    add_dish(&app, &mgr, &menu, 1_000).await;
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

    // The school sets a deadline, and it closed on this menu long ago. The
    // serving hour goes with it: a slot without one has no instant for the
    // deadline to count back from, so the cutoff would bind nobody.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&admin),
        Some(json!({
            "meal_cancel_cutoff_minutes": 60,
            "meal_slots": [{ "name": "lunch", "serving_minute": 720 }],
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let res = send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{booking}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CONFLICT,
        "the student is still bound by the cutoff: {}",
        res.body
    );

    let res = send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{booking}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    // The refund is the point: a bypass that freed the seat and kept the charge
    // would only move the wedge onto the money.
    let res = send(
        &app,
        "GET",
        &format!("/meals/balance/{ali_id}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.body["balance_minor"], 0, "{}", res.body);

    // …and with the seat back, the menu is deletable again.
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/menus/{menu}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
}

/// Canteen debt is family debt, not classroom information — the same rule
/// `/payments` has always held. The two money reads shipped gated teacher+
/// (`ensure_can_observe`), so a form teacher could read what a family owed the
/// canteen while the identical `/payments` read 403'd them. Everything else in
/// the meals block — dietary profiles, bookings, attendance — stays teacher+ on
/// purpose; only the money moved.
#[tokio::test]
async fn only_manager_reads_another_students_meal_money() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "money_admin", "admin").await;
    let mgr = login_as(&app, &db, "money_mgr", "manager").await;
    let teacher = login_as(&app, &db, "money_teacher", "teacher").await;
    let ali = login(&app, "money_ali").await;
    let ali_id = me_id(&app, &ali).await;
    let mother = login_as(&app, &db, "money_mother", "parent").await;
    let stranger = login_as(&app, &db, "money_stranger", "parent").await;
    let res = send(
        &app,
        "POST",
        &format!("/users/{}/students", me_id(&app, &mother).await),
        Some(&admin),
        Some(json!({ "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    for route in [
        format!("/meals/balance/{ali_id}"),
        format!("/meals/ledger/{ali_id}"),
    ] {
        for (who, cookie, want) in [
            ("teacher", &teacher, StatusCode::FORBIDDEN),
            ("unlinked parent", &stranger, StatusCode::FORBIDDEN),
            ("manager", &mgr, StatusCode::OK),
            ("admin", &admin, StatusCode::OK),
            ("linked parent", &mother, StatusCode::OK),
            ("the student themselves", &ali, StatusCode::OK),
        ] {
            let res = send(&app, "GET", &route, Some(cookie), None).await;
            assert_eq!(res.status, want, "{who} on {route}: {}", res.body);
        }
    }

    // The rest of the block did not move with the money: a teacher still reads
    // the allergen list and the attendance history they need at the door.
    for route in [
        format!("/meals/profiles/{ali_id}"),
        format!("/meals/attendance/{ali_id}"),
    ] {
        let res = send(&app, "GET", &route, Some(&teacher), None).await;
        assert_eq!(
            res.status,
            StatusCode::OK,
            "teacher on {route}: {}",
            res.body
        );
    }
}

/// A booking id is fully derivable — `{date}_{slot}_{student}` — and both
/// halves are readable by any teacher (`GET /users`, `GET /meals/menus`). The
/// cancel read the row before it checked who was asking, so the status code
/// answered a question the route never meant to: `403` for a seat that exists,
/// `404` for one that does not. That is the whole-school booking list, whose
/// own route is deliberately manager+, handed out one student at a time — and
/// any peer holding a user id (every `PersonRef` carries one) could ask it too.
#[tokio::test]
async fn an_unauthorised_cancel_tells_nobody_whether_the_seat_exists() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "oracle_mgr", "manager").await;
    let teacher = login_as(&app, &db, "oracle_teacher", "teacher").await;
    let ali = login(&app, "oracle_ali").await;
    let veli = login(&app, "oracle_veli").await;
    let veli_id = me_id(&app, &veli).await;
    let can_id = me_id(&app, &login(&app, "oracle_can").await).await;
    let menu = publish(&app, &mgr, "2026-09-19").await;

    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&ali),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let booked = id_of(&res.body);
    // Never booked, and derived exactly as the seat that was: the menu key
    // joined to a student key anyone can read off a response.
    let never = format!("{menu}_{can_id}");

    for (who, cookie) in [("a teacher", &teacher), ("a peer student", &veli)] {
        let held = send(
            &app,
            "DELETE",
            &format!("/meals/bookings/{booked}"),
            Some(cookie),
            None,
        )
        .await;
        let absent = send(
            &app,
            "DELETE",
            &format!("/meals/bookings/{never}"),
            Some(cookie),
            None,
        )
        .await;
        assert_eq!(
            held.status, absent.status,
            "{who} can tell a held seat from an unbooked one: {} vs {}",
            held.body, absent.body
        );
        assert_eq!(held.status, StatusCode::FORBIDDEN, "{}", held.body);
    }

    // The gate did not turn into a blanket 403: the student it is for still
    // learns their own seat is gone, and still cancels the one they hold.
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{menu}_{veli_id}"),
        Some(&veli),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{booked}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "cancelled", "{}", res.body);
    // …and manager+, who may read the whole list anyway, still sees existence.
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{never}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
}

/// Two first `PATCH`es of a profile nobody ever recorded both read "no row" and
/// both tried to `CREATE` it. The loser's create collided (on the id, and on
/// the `dietary_profile_student` UNIQUE index behind it) and propagated as a
/// `500` on a perfectly legitimate request. The winner's row is now written
/// over instead, which is what the second `PATCH` was asking for anyway.
#[tokio::test]
async fn two_first_profile_writes_land_without_a_500() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "first_mgr", "manager").await;
    let ali = login(&app, "first_ali").await;
    let ali_id = me_id(&app, &ali).await;

    let route = format!("/meals/profiles/{ali_id}");
    let (one, two) = tokio::join!(
        send(
            &app,
            "PATCH",
            &route,
            Some(&mgr),
            Some(json!({ "tags": ["vegan"] })),
        ),
        send(
            &app,
            "PATCH",
            &route,
            Some(&mgr),
            Some(json!({ "note": "fındık" })),
        ),
    );
    for res in [&one, &two] {
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    }

    // Stored state, never a reported win: whichever landed second, the row
    // carries both writes — neither was dropped and neither 500'd.
    let res = send(&app, "GET", &route, Some(&mgr), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["tags"], json!(["vegan"]), "{}", res.body);
    assert_eq!(res.body["note"], "fındık", "{}", res.body);
}

/// A repeat `POST` of a held seat answered off the row it read a round trip
/// earlier. A cancel committing in that gap frees the seat, refunds it and
/// leaves the row `cancelled` — and the `201` still said `"status": "booked"`,
/// `cancelled_at: null`, so a client trusting the answer thought it held a seat
/// the store had given away. The stored state was right throughout; only the
/// answer lied.
///
/// The cancel is injected the way `regress_roles` does it: a `DEFINE EVENT`
/// fires inside the very write the replay makes — here the charge it replays,
/// which is why the ledger is stripped first (a charge already there is a
/// no-op). Fenced on `attempt = 1`, so the re-booked seat's own charge does not
/// trip it a second time.
#[tokio::test]
async fn a_replayed_booking_answers_off_the_row_the_store_holds() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "stale_mgr", "manager").await;
    let ali = login(&app, "stale_ali").await;
    let menu = publish(&app, &mgr, "2026-09-20").await;
    add_dish(&app, &mgr, &menu, 1_000).await;

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

    db.query(format!(
        "DELETE meal_ledger;
         DEFINE EVENT cancel_inside ON TABLE meal_ledger WHEN $event = 'CREATE' THEN {{
             UPDATE type::record('meal_booking', '{booking}') \
                 SET status = 'cancelled', cancelled_at = 1 WHERE attempt = 1;
         }};"
    ))
    .await
    .unwrap()
    .check()
    .unwrap();

    let res = send(
        &app,
        "POST",
        &format!("/meals/menus/{menu}/bookings"),
        Some(&ali),
        Some(json!({})),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    // The answer against the store, field by field — that is the whole claim.
    let stored = send(&app, "GET", "/meals/bookings/me", Some(&ali), None).await;
    let row = &stored.body["items"][0];
    assert_eq!(row["id"], json!(booking), "{}", stored.body);
    assert_eq!(
        res.body["status"], row["status"],
        "the answer disagrees with the row: {} vs {}",
        res.body, stored.body
    );
    assert_eq!(
        res.body["cancelled_at"], row["cancelled_at"],
        "the answer disagrees with the row: {} vs {}",
        res.body, stored.body
    );
    assert_eq!(res.body["status"], "booked", "{}", res.body);
}

/// A slot with **no serving hour** has no instant for a deadline to count back
/// from, and the code counted from midnight UTC instead. All three shipped
/// slots carry no serving minute, so the day a school set
/// `meal_cancel_cutoff_minutes` — the one knob — every same-day menu was
/// already past its deadline: today's lunch could not be booked, and the seats
/// already held could not be given back by the students and parents holding
/// them, only by a manager. The fallback was chosen to keep the canteen up on
/// an upgrade and did the opposite. An unset hour is now an unenforced cutoff.
///
/// The existing unit test never caught it because it only ever passed a slot
/// *with* an hour, and the `None` case was exercised against an impossible
/// date, where the refusal comes from the date instead.
#[tokio::test]
async fn a_slot_with_no_serving_hour_closes_nothing() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "hour_adm", "admin").await;
    let mgr = login_as(&app, &db, "hour_mgr", "manager").await;
    let ali = login(&app, "hour_ali").await;

    // The school sets its one cutoff knob and nothing else — the shipped
    // slots still carry no `serving_minute`, which is the whole case.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&admin),
        Some(json!({ "meal_cancel_cutoff_minutes": 120 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert!(
        res.body["meal_slots"]
            .as_array()
            .unwrap()
            .iter()
            .all(|slot| slot["serving_minute"].is_null()),
        "the case only exists while no slot has an hour: {}",
        res.body
    );

    // Today's menu: midnight UTC is behind us for all but the first two hours
    // of the day, so the fallback deadline had already passed.
    let today = hezarfen_backend::domain::timestamp::Timestamp::today_utc()
        .format("%Y-%m-%d")
        .to_string();
    let menu = publish(&app, &mgr, &today).await;
    add_dish(&app, &mgr, &menu, 1_000).await;

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
        "today's menu must still be bookable: {}",
        res.body
    );
    let booking = id_of(&res.body);

    // …and the student can still free the seat themselves. This half is worse
    // than the refusal: the seat was already held and only a manager could
    // give it back.
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{booking}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["status"], "cancelled", "{}", res.body);

    // The deadline starts binding the moment the school sets the hour — on
    // menus already published, since the slot list is read live. Midnight UTC
    // plus two hours, minus a two-hour cutoff, is midnight: long past.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&admin),
        Some(json!({ "meal_slots": [{ "name": "lunch", "serving_minute": 120 }] })),
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

/// Nothing bounded how often one seat could be retaken, and every cycle
/// appends two permanent, undeletable ledger lines (the charge and its
/// reversal). At the API's own request ceiling that is hundreds of thousands
/// of rows a day against one account — and every later balance read used to
/// fold the lot. The cycle ceiling is the growth half of that fix.
#[tokio::test]
async fn a_seat_cannot_be_retaken_without_end() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "cycle_mgr", "manager").await;
    let ali = login(&app, "cycle_ali").await;
    let ali_id = me_id(&app, &ali).await;
    let menu = publish(&app, &mgr, "2026-09-21").await;
    add_dish(&app, &mgr, &menu, 1_000).await;

    // `max_booking_attempts` is published, so a client can say why before it
    // sends the call that fails.
    let limits = send(&app, "GET", "/limits", Some(&ali), None).await;
    let ceiling = limits.body["meal"]["max_booking_attempts"]
        .as_i64()
        .unwrap_or_else(|| panic!("the ceiling is not published: {}", limits.body));

    let seats = format!("/meals/menus/{menu}/bookings");
    for round in 1..=ceiling {
        let res = send(&app, "POST", &seats, Some(&ali), Some(json!({}))).await;
        assert_eq!(
            res.status,
            StatusCode::CREATED,
            "round {round}: {}",
            res.body
        );
        let res = send(
            &app,
            "DELETE",
            &format!("/meals/bookings/{}", id_of(&res.body)),
            Some(&ali),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "round {round}: {}", res.body);
    }
    let res = send(&app, "POST", &seats, Some(&ali), Some(json!({}))).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    assert!(
        res.body["error"].as_str().unwrap().contains("maximum"),
        "the refusal must say what happened: {}",
        res.body
    );

    // Stored state: the seat stayed cancelled, and the refusal wrote no line —
    // ten cycles, twenty lines, and the balance nets to zero.
    let res = send(&app, "GET", "/meals/bookings/me", Some(&ali), None).await;
    assert_eq!(res.body["items"][0]["status"], "cancelled", "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/meals/ledger/{ali_id}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.body["total"], ceiling * 2, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/meals/balance/{ali_id}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.body["balance_minor"], 0, "{}", res.body);
}

/// The balance is summed **per kind by the database** now, not folded line by
/// line in this process — the reason the ledger's length stopped being a cost
/// on every read. What must not change with it is the answer: `credits +
/// reversals - charges`, with the signs still applied by `MealLedgerKind::sign`
/// rather than respelled in SQL.
#[tokio::test]
async fn the_balance_sums_every_kind_the_same_way_it_always_did() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "sum_adm", "admin").await;
    let mgr = login_as(&app, &db, "sum_mgr", "manager").await;
    let ali = login(&app, "sum_ali").await;
    let ali_id = me_id(&app, &ali).await;

    // Nothing at all: no rows, so the aggregate returns no groups.
    assert_eq!(balance_of(&app, &mgr, &ali_id).await, 0);

    // A credit, then two charges on different menus, then one reversal — every
    // kind, and more than one line in two of the three groups.
    let res = send(
        &app,
        "POST",
        "/meals/credits",
        Some(&admin),
        Some(json!({ "student_id": ali_id, "amount_minor": 10_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(balance_of(&app, &mgr, &ali_id).await, 10_000);

    let mut bookings = Vec::new();
    for (day, price) in [("2026-09-23", 4_500), ("2026-09-24", 1_500)] {
        let menu = publish(&app, &mgr, day).await;
        add_dish(&app, &mgr, &menu, price).await;
        let res = send(
            &app,
            "POST",
            &format!("/meals/menus/{menu}/bookings"),
            Some(&ali),
            Some(json!({})),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
        bookings.push(id_of(&res.body));
    }
    assert_eq!(
        balance_of(&app, &mgr, &ali_id).await,
        10_000 - 4_500 - 1_500
    );

    let res = send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{}", bookings[0]),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        balance_of(&app, &mgr, &ali_id).await,
        10_000 - 1_500,
        "credit + reversal - charges"
    );
    // A second student's lines are somebody else's: the aggregate is scoped by
    // `WHERE student = …` exactly as the fold was.
    let veli = login(&app, "sum_veli").await;
    let res = send(
        &app,
        "POST",
        "/meals/credits",
        Some(&admin),
        Some(json!({ "student_id": me_id(&app, &veli).await, "amount_minor": 777 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(
        balance_of(&app, &mgr, &ali_id).await,
        10_000 - 1_500,
        "another student's credit leaked into this balance"
    );
}

/// The stale-data half of the cycle ceiling: a live volume may already hold a
/// seat retaken more times than the new ceiling allows. Such a row must stay
/// **cancellable** — the ceiling binds the write that would add another cycle,
/// nothing else. A ceiling consulted on the cancel path instead would strand
/// exactly the seats it was meant to stop growing: the menu refuses its own
/// delete while a seat is held, and cancelling is the only route that reverses
/// a charge.
#[tokio::test]
async fn an_over_ceiling_seat_from_an_older_build_is_still_cancellable() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "old_mgr", "manager").await;
    let ali = login(&app, "old_ali").await;
    // A free menu: the row is aged by hand below, and a charge keyed to an
    // attempt this test never really made would only muddy what is asserted.
    let menu = publish(&app, &mgr, "2026-09-25").await;
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

    // Aged past the ceiling, the way an older binary would have left it.
    db.query(format!(
        "UPDATE type::record('meal_booking', '{booking}') SET attempt = 25"
    ))
    .await
    .unwrap()
    .check()
    .unwrap();

    let res = send(
        &app,
        "DELETE",
        &format!("/meals/bookings/{booking}"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::OK,
        "an over-ceiling seat must stay freeable: {}",
        res.body
    );
    assert_eq!(res.body["status"], "cancelled", "{}", res.body);
    // …and with the seat back the menu is deletable again, which is the whole
    // point of not letting the ceiling reach the cancel.
    let res = send(
        &app,
        "DELETE",
        &format!("/meals/menus/{menu}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
}
