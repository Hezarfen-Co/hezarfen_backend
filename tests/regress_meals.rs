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

    // The school sets a deadline, and it closed on this menu long ago.
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&admin),
        Some(json!({ "meal_cancel_cutoff_minutes": 60 })),
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
