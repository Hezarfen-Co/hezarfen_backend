//! Regressions for the school-fee routes. Each test stands for one defect that
//! shipped: a statement that folded its two halves from two different reads of
//! the ledger, a due date the schema advertised a floor for and nobody
//! enforced, an edit of a deleted plan answered as if the plan were assigned,
//! and the two unbounded loops — a ledger fold that grew one query per payment
//! under the process-global lock, and an assignment request that could issue
//! twelve thousand sequential writes.
//!
//! Router-level where the defect is reachable through a route; the edit-versus-
//! delete window is a direct domain call, because the handler's own read is
//! what closes it over HTTP — the defect lives *below* that read.

mod common;

use axum::http::StatusCode;
use common::{app_and_db, id_of, login, login_as, me_id, send};
use hezarfen_backend::constant::{MAX_FEE_PLAN_ASSIGN_WRITES, MAX_LEDGER_APPLIED_LINES};
use hezarfen_backend::domain::fee_plan::{FeePlan, FeePlanName, Installment};
use hezarfen_backend::domain::fee_plan_assignment::FeePlanAssignment;
use hezarfen_backend::domain::payment_ledger::LedgerAmount;
use hezarfen_backend::domain::timestamp::Timestamp;
use hezarfen_backend::domain::user::UserId;
use hezarfen_backend::error::AppError;
use serde_json::{Value, json};

/// A plan of `installments`, as the manager writes it.
async fn create_plan(app: &axum::Router, mgr: &str, installments: Value) -> String {
    let res = send(
        app,
        "POST",
        "/payments/plans",
        Some(mgr),
        Some(json!({ "name": "Yearly", "installments": installments })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    id_of(&res.body)
}

/// The whole ledger of `student`, newest first.
async fn ledger(app: &axum::Router, mgr: &str, student: &str) -> Vec<Value> {
    let res = send(
        app,
        "GET",
        &format!("/payments/ledger/{student}"),
        Some(mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    common::items(&res.body).clone()
}

/// The `balance_minor` a statement reports must be folded from the very lines
/// its per-charge rollup walked. It was folded from a *second*, later read of
/// the ledger, three round trips after the first: a payment landing in that
/// window made one document say `outstanding_minor: 5 000` about a charge while
/// its `balance_minor` already netted the money — one self-inconsistent money
/// document, served as the authoritative one.
///
/// Pinned as the arithmetic rather than as the race (the mem engine drops
/// concurrent writes and would only make that flaky): for a fixed ledger the
/// two halves must agree to the kuruş, which is exactly what one read buys.
#[tokio::test]
async fn a_statement_folds_its_balance_from_the_lines_it_rolled_up() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "fee_mgr", "manager").await;
    let ali = login(&app, "fee_ali").await;
    let ali_id = me_id(&app, &ali).await;

    let future = Timestamp::now().as_millis() + 30 * 24 * 60 * 60 * 1000;
    let plan = create_plan(
        &app,
        &mgr,
        json!([
            { "amount_minor": 10_000, "due_at": 1_000 },
            { "amount_minor": 20_000, "due_at": future },
        ]),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/payments/plans/{plan}/assignments"),
        Some(&mgr),
        Some(json!({ "student_ids": [ali_id] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body[0]["status"], "assigned", "{}", res.body);

    let lines = ledger(&app, &mgr, &ali_id).await;
    let charge = |amount: i64| {
        lines
            .iter()
            .find(|line| line["amount_minor"] == amount && line["kind"] == "charge")
            .map(id_of)
            .expect("the installment was billed")
    };

    // 6 000 paid against the 10 000 charge, 1 000 of it handed back, and the
    // 20 000 charge reversed outright.
    let res = send(
        &app,
        "POST",
        "/payments/credits",
        Some(&mgr),
        Some(json!({ "charge_id": charge(10_000), "amount_minor": 6_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let credit = id_of(&res.body);
    let res = send(
        &app,
        "POST",
        "/payments/refunds",
        Some(&mgr),
        Some(json!({ "credit_id": credit, "amount_minor": 1_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(
        &app,
        "POST",
        "/payments/reversals",
        Some(&mgr),
        Some(json!({ "line_id": charge(20_000) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    let res = send(
        &app,
        "GET",
        &format!("/payments/statement/{ali_id}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let entries = common::items(&res.body["entries"]).clone();
    let row = |amount: i64| {
        entries
            .iter()
            .find(|entry| entry["amount_minor"] == amount)
            .unwrap_or_else(|| panic!("a row per charge, got {entries:?}"))
    };
    assert_eq!(row(10_000)["credited_minor"], 6_000);
    assert_eq!(row(10_000)["refunded_minor"], 1_000);
    assert_eq!(row(10_000)["outstanding_minor"], 5_000);
    assert_eq!(row(20_000)["reversed"], true);
    assert_eq!(row(20_000)["outstanding_minor"], 0);

    // credits 6 000 + reversals 20 000 - charges 30 000 - refunds 1 000.
    assert_eq!(res.body["balance_minor"], -5_000, "{}", res.body);
    // And the two halves of the document are the same arithmetic: what the
    // rollup says is still owed is what the balance says the family owes.
    let owed: i64 = entries
        .iter()
        .map(|entry| entry["outstanding_minor"].as_i64().expect("minor units"))
        .sum();
    assert_eq!(res.body["balance_minor"], -owed);
}

/// `due_at` declares `minimum = 0` in the schema (pinned by `spec_bounds`) and
/// nothing enforced it: `-1` stored as written, and the charge it billed read
/// as `overdue` for ever, against a date no client could have meant.
#[tokio::test]
async fn a_negative_due_date_is_refused_on_both_write_paths() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "due_mgr", "manager").await;

    let res = send(
        &app,
        "POST",
        "/payments/plans",
        Some(&mgr),
        Some(json!({
            "name": "Yearly",
            "installments": [{ "amount_minor": 1, "due_at": -1 }],
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    assert!(
        res.body.to_string().contains("due_at"),
        "the refusal names the field: {}",
        res.body
    );
    // Nothing was written: a refused plan is not a stored one.
    let res = send(&app, "GET", "/payments/plans", Some(&mgr), None).await;
    assert_eq!(common::total(&res.body), 0, "{}", res.body);

    // The edit path validates the same body, so it must refuse the same way —
    // and leave the stored schedule alone.
    let plan = create_plan(&app, &mgr, json!([{ "amount_minor": 1, "due_at": 0 }])).await;
    let res = send(
        &app,
        "PATCH",
        &format!("/payments/plans/{plan}"),
        Some(&mgr),
        Some(json!({ "installments": [{ "amount_minor": 1, "due_at": -1 }] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);

    let res = send(
        &app,
        "GET",
        &format!("/payments/plans/{plan}"),
        Some(&mgr),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["installments"][0]["due_at"], 0, "{}", res.body);
}

/// The window a handler's own read cannot close: the plan was read, then
/// deleted, and only then does the guarded `UPDATE` run. It matched nothing —
/// because the row is gone, not because anyone is on the plan — and the answer
/// was `409 "an assigned plan cannot be edited"` about a plan that never was.
/// `FeePlan::delete` already pays for the read that tells the two apart on its
/// refusal path; the edit now does too, and a genuinely assigned plan is still
/// the `409` it always was.
#[tokio::test]
async fn editing_a_plan_deleted_since_it_was_read_is_a_404_not_a_409() {
    let (_app, db) = app_and_db().await;
    let manager = UserId::from_key("del_mgr");
    let one = || {
        vec![Installment::new(
            LedgerAmount::try_new(10_000).unwrap(),
            Timestamp::from_millis(1_000),
        )]
    };
    let plan = FeePlan::create(
        FeePlanName::try_new("Yearly").unwrap(),
        one(),
        &manager,
        &db,
    )
    .await
    .unwrap();

    // The handler's snapshot, taken before the delete lands.
    let read = FeePlan::read(plan.get_id(), &db).await.unwrap().unwrap();
    assert!(plan.delete(&db).await.unwrap(), "nobody is on it");
    let refused = read
        .update(Some(FeePlanName::try_new("Renamed").unwrap()), None, &db)
        .await
        .unwrap_err();
    assert!(
        matches!(refused, AppError::NotFound),
        "a plan that is gone is a 404, not an assigned-plan 409: {refused}"
    );

    // The genuine refusal is untouched: a plan somebody is actually on still
    // answers 409, and its schedule is not edited.
    let plan = FeePlan::create(
        FeePlanName::try_new("Yearly").unwrap(),
        one(),
        &manager,
        &db,
    )
    .await
    .unwrap();
    FeePlanAssignment::assign(&plan, &UserId::from_key("del_stu"), &manager, &db)
        .await
        .unwrap();
    let refused = plan
        .clone()
        .update(Some(FeePlanName::try_new("Renamed").unwrap()), None, &db)
        .await
        .unwrap_err();
    assert!(
        matches!(refused, AppError::Conflict(_)),
        "an assigned plan is still refused as assigned: {refused}"
    );
    let stored = FeePlan::read(plan.get_id(), &db).await.unwrap().unwrap();
    assert_eq!(stored.get_name().as_str(), "Yearly", "nothing was written");
}

/// The over-payment cap folds the target's whole subtree, one query per line,
/// with the process-global payment lock held — and nothing bounded that
/// subtree. A charge of 10 000 000 kuruş settled one kuruş at a time made
/// payment 2 001 issue 2 002 sequential queries while every other payment,
/// refund and reversal in the school queued behind it.
#[tokio::test]
async fn a_charge_stops_taking_payments_past_the_applied_line_ceiling() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "cap_mgr", "manager").await;
    let ali = login(&app, "cap_ali").await;
    let ali_id = me_id(&app, &ali).await;

    let plan = create_plan(
        &app,
        &mgr,
        json!([{ "amount_minor": 100, "due_at": 1_000 }]),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/payments/plans/{plan}/assignments"),
        Some(&mgr),
        Some(json!({ "student_ids": [ali_id] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let charge = id_of(&ledger(&app, &mgr, &ali_id).await[0]);

    let pay = async |amount: i64| {
        send(
            &app,
            "POST",
            "/payments/credits",
            Some(&mgr),
            Some(json!({ "charge_id": charge, "amount_minor": amount })),
        )
        .await
    };
    // Right up to the ceiling the money still lands, one kuruş at a time.
    let mut first_credit = String::new();
    for n in 1..=MAX_LEDGER_APPLIED_LINES {
        let res = pay(1).await;
        assert_eq!(res.status, StatusCode::CREATED, "payment {n}: {}", res.body);
        if n == 1 {
            first_credit = id_of(&res.body);
        }
    }
    // The one past it is refused, and refused *whole*.
    let res = pay(1).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    let lines = ledger(&app, &mgr, &ali_id).await;
    assert_eq!(
        lines.len(),
        MAX_LEDGER_APPLIED_LINES + 1,
        "one charge and its 20 payments, and nothing from the refusal"
    );
    let credited: i64 = lines
        .iter()
        .filter(|line| line["kind"] == "credit")
        .map(|line| line["amount_minor"].as_i64().expect("minor units"))
        .sum();
    assert_eq!(credited, 20, "twenty kuruş in, exactly");

    // The ceiling is per line and binds only what is applied *to* it: a charge
    // sitting at the ceiling still refunds through its own payments, which is
    // what keeps a row written before this rule from becoming unfixable money.
    let res = send(
        &app,
        "POST",
        "/payments/refunds",
        Some(&mgr),
        Some(json!({ "credit_id": first_credit, "amount_minor": 1 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    // ...and it is still reversible.
    let res = send(
        &app,
        "POST",
        "/payments/reversals",
        Some(&mgr),
        Some(json!({ "line_id": charge })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
}

/// One assignment request appends every installment of the plan for every
/// student it names, and only the head count was capped: 200 students on a
/// 60-installment plan was 12 000 sequential writes in one request, with no
/// timeout anywhere in the stack. The bound is the product, and the refusal is
/// all-or-nothing — a money route that billed half a batch is worse than one
/// that billed none.
#[tokio::test]
async fn an_assignment_is_bounded_by_the_charges_it_would_raise() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "batch_mgr", "manager").await;
    let ali = login(&app, "batch_ali").await;
    let ali_id = me_id(&app, &ali).await;

    let installments = 20;
    let schedule: Vec<Value> = (0..installments)
        .map(|n| json!({ "amount_minor": 100, "due_at": 1_000 + n }))
        .collect();
    let plan = create_plan(&app, &mgr, json!(schedule)).await;
    let fits = MAX_FEE_PLAN_ASSIGN_WRITES / installments as usize;

    // One student over the ceiling. The others do not exist, so the whole cost
    // of the batch is the real student's charges — and none of them is written.
    let mut ids: Vec<String> = (0..fits).map(|n| format!("nobody{n}")).collect();
    ids.push(ali_id.clone());
    let assign = async |ids: &Vec<String>| {
        send(
            &app,
            "POST",
            &format!("/payments/plans/{plan}/assignments"),
            Some(&mgr),
            Some(json!({ "student_ids": ids })),
        )
        .await
    };
    let res = assign(&ids).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
    assert!(
        res.body.to_string().contains("split the batch"),
        "the refusal names what the caller has to do: {}",
        res.body
    );
    assert!(
        ledger(&app, &mgr, &ali_id).await.is_empty(),
        "refused whole: not one charge was written"
    );

    // Exactly at the ceiling it goes through, and it really bills.
    ids.remove(0);
    let res = assign(&ids).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        res.body[ids.len() - 1]["status"],
        "assigned",
        "{}",
        res.body
    );
    let lines = ledger(&app, &mgr, &ali_id).await;
    assert_eq!(
        lines.len(),
        installments as usize,
        "every installment billed"
    );
}

/// `balance_of` decoded every row of a student's ledger to produce one integer,
/// and a fee ledger grows by design: one assignment appends a charge per
/// installment per student. It is a `GROUP BY kind` aggregate now, folded in
/// Rust by the same `sign()` the formula is written in.
///
/// **This is an equivalence pin, not a repro** — the old whole-ledger fold
/// returns the same numbers, so it stays green on the unfixed code. What it
/// catches is a *wrong* aggregate: a `WHERE` that stopped scoping the sum to
/// one student, or a fold that lost the signs. Both were mutation-checked.
#[tokio::test]
async fn the_balance_aggregate_scopes_to_one_student_and_keeps_the_signs() {
    let (app, db) = app_and_db().await;
    let mgr = login_as(&app, &db, "agg_mgr", "manager").await;
    let ali = login(&app, "agg_ali").await;
    let ali_id = me_id(&app, &ali).await;
    let veli = login(&app, "agg_veli").await;
    let veli_id = me_id(&app, &veli).await;
    let zero = login(&app, "agg_zero").await;
    let zero_id = me_id(&app, &zero).await;

    let plan = create_plan(
        &app,
        &mgr,
        json!([{ "amount_minor": 10_000, "due_at": 1_000 }]),
    )
    .await;
    let other = create_plan(
        &app,
        &mgr,
        json!([{ "amount_minor": 20_000, "due_at": 1_000 }]),
    )
    .await;
    for (plan, student) in [(&plan, &ali_id), (&other, &veli_id)] {
        let res = send(
            &app,
            "POST",
            &format!("/payments/plans/{plan}/assignments"),
            Some(&mgr),
            Some(json!({ "student_ids": [student] })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    }
    // Ali also pays 6 000 and is handed 1 000 of it back, so all four kinds
    // are in play: a fold that dropped the signs would read 17 000.
    let charge = id_of(&ledger(&app, &mgr, &ali_id).await[0]);
    let res = send(
        &app,
        "POST",
        "/payments/credits",
        Some(&mgr),
        Some(json!({ "charge_id": charge, "amount_minor": 6_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let credit = id_of(&res.body);
    let res = send(
        &app,
        "POST",
        "/payments/refunds",
        Some(&mgr),
        Some(json!({ "credit_id": credit, "amount_minor": 1_000 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    let balance = async |cookie: &str, path: String| {
        let res = send(&app, "GET", &path, Some(cookie), None).await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
        res.body["balance_minor"].as_i64().expect("minor units")
    };
    // credits 6 000 - charges 10 000 - refunds 1 000. Veli's 20 000 is not in
    // it, and Ali's is not in Veli's.
    assert_eq!(
        balance(&mgr, format!("/payments/balance/{ali_id}")).await,
        -5_000
    );
    assert_eq!(
        balance(&mgr, format!("/payments/balance/{veli_id}")).await,
        -20_000
    );
    // The route that reads it for the family itself, and one with no lines at
    // all: no groups come back, which is 0 rather than an error.
    assert_eq!(balance(&ali, "/payments/balance/me".into()).await, -5_000);
    assert_eq!(balance(&zero, "/payments/balance/me".into()).await, 0);
    assert_eq!(
        balance(&mgr, format!("/payments/balance/{zero_id}")).await,
        0
    );

    // And the grouped fold and the raw-line fold still answer alike — the
    // statement folds its own lines, so the two must not drift apart.
    assert_eq!(
        balance(&mgr, format!("/payments/statement/{ali_id}")).await,
        -5_000
    );
}
