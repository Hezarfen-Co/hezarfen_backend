//! The assign workflow: placing a plan on a student and billing it — the
//! moment the plan becomes money owed — plus the batch loop and the bounds
//! one assign request may carry. The queries live in
//! [`crate::db::fee_plan_assignment`]; the ledger appends in
//! [`crate::db::payment_ledger`].
//!
//! The row is keyed `<plan>_<student>`, and every charge it raises is keyed
//! from that same key plus the installment number, so the whole operation is
//! idempotent **by identity**: assigning twice writes nothing the second time,
//! however the two requests interleave, and an assign cut short after three of
//! twelve charges landed completes itself when it is repeated. No scan decides
//! "has this been billed yet?" — a scan can be slipped past by a concurrent
//! writer, and the cost of that mistake here is a double-billed family.

use crate::constant::{MAX_FEE_PLAN_ASSIGN_STUDENTS, MAX_FEE_PLAN_ASSIGN_WRITES};
use crate::database::Database;
use crate::db::cap::Claimed;
use crate::db::{fee_plan, fee_plan_assignment, payment_ledger};
use crate::domain::fee_plan::FeePlan;
use crate::domain::fee_plan_assignment::{FeePlanAssignment, FeePlanAssignmentId};
use crate::domain::role::Role;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// Place `plan` on `student` and bill it. Returns the assignment and
/// whether it was *already* there, so the web layer can answer a replay
/// honestly instead of pretending it just happened.
///
/// The row and the plan's assignment refcount are written in **one
/// statement** ([`fee_plan_assignment::create`]'s claim): that increment is
/// what makes the
/// plan un-editable and un-deletable, and it has to be indivisible from the
/// row it counts, or an edit could slip between the two. A plan the counter
/// cannot be claimed on is one a concurrent delete already removed, which is
/// the same `404` the handler's own lookup would have given.
///
/// The installments are then re-read **from the stored plan**, not taken
/// from the caller's snapshot: the claim is the moment the plan freezes, and
/// an edit that landed between the handler's read and that claim is
/// legitimate. Billing the snapshot would raise charges from a schedule the
/// plan no longer shows — precisely the divergence the 409 exists to
/// prevent.
///
/// The charges are appended after the row, never before: each one is keyed
/// by (assignment, installment number), so this loop is safe to re-enter
/// from the top — which is exactly what a repeated assign does, and the
/// only way a half-written assignment ever gets its missing lines.
pub async fn assign(
    db: &Database,
    plan: &FeePlan,
    student: &UserId,
    assigned_by: &UserId,
) -> Result<(FeePlanAssignment, bool), AppError> {
    let id = FeePlanAssignmentId::composite(plan.get_id(), student);
    let claimed =
        fee_plan_assignment::create(db, plan.get_id(), student, assigned_by, Timestamp::now())
            .await?;
    let (assignment, existed) = match claimed {
        Claimed::Made(created) => (created, false),
        // Someone assigned this pair first — their row is the answer, and
        // the charges below are keyed the same, so continuing here can only
        // *complete* the billing. Their claim already counts this row.
        Claimed::Duplicate => (require(&id, db).await?, true),
        // The counter is uncapped, so the only miss is a plan that is gone.
        Claimed::Full => return Err(AppError::NotFound),
    };
    // Frozen as of the claim above: no edit can land past it any more.
    let plan = fee_plan::read(db, plan.get_id())
        .await?
        .ok_or_else(|| AppError::Internal("the assigned fee plan vanished".into()))?;
    for (index, installment) in plan.get_installments().iter().enumerate() {
        payment_ledger::charge_for_installment(
            db,
            &assignment,
            index + 1,
            installment,
            assigned_by,
        )
        .await?;
    }
    Ok((assignment, existed))
}

async fn require(id: &FeePlanAssignmentId, db: &Database) -> Result<FeePlanAssignment, AppError> {
    fee_plan_assignment::read(db, id)
        .await?
        .ok_or_else(|| AppError::Internal("failed to assign the fee plan".into()))
}

/// What one student in an assign batch came to. The web layer spells these
/// as its response statuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Placed and billed now.
    Assigned,
    /// A replay: the student was already on this plan, nothing billed again.
    AlreadyAssigned,
    /// Refused for this student alone — the batch carries on.
    Rejected(&'static str),
}

/// Place one plan on a batch of students, which is what turns it into money
/// owed: every installment is appended as a charge line right away, each with
/// its own due date. Returns one outcome per student, in the order sent.
///
/// Only students carry a fee record, so any other target is rejected. One bad
/// id never loses the rest of the batch: the outcome reports each student
/// separately.
///
/// Placing the first student **freezes** the plan: it can no longer be edited
/// or deleted. The installments billed are read back from the stored plan at
/// that instant, so an edit that landed a moment earlier is the one billed,
/// never the version this request first looked at. A plan deleted while the
/// batch is running is `Rejected` from that student on — a per-student
/// outcome like any other, so the students it already billed stay in the
/// report.
///
/// One request is bounded by the **charges it would raise**, not by the head
/// count alone: `student_ids × installments` may not exceed 3 000 (200
/// students up to a 15-installment plan; 50 at a time on a 60-installment
/// one). Past that the whole call is a `400` telling the caller to split the
/// batch — nothing is written, because a money route that billed half a batch
/// and gave up would leave a bursar guessing which families were charged.
pub async fn assign_batch(
    db: &Database,
    plan_key: &str,
    assigner: &UserId,
    student_ids: Vec<String>,
) -> Result<Vec<(String, Outcome)>, AppError> {
    if student_ids.len() > MAX_FEE_PLAN_ASSIGN_STUDENTS {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "student_ids",
            reason: "may name at most 200 students",
        }));
    }
    let plan = fee_plan::read(db, &crate::domain::fee_plan::FeePlanId::from_key(plan_key))
        .await?
        .ok_or(AppError::NotFound)?;
    // The student cap alone cannot see the schedule: every student named
    // appends *every* installment, so what really bounds this request is the
    // product. Refused whole and before anything is written — a batch this API
    // billed only part of would leave a bursar guessing which families were
    // charged.
    if student_ids
        .len()
        .saturating_mul(plan.get_installments().len())
        > MAX_FEE_PLAN_ASSIGN_WRITES
    {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "student_ids",
            reason: "too many charges for one request: students × installments \
                     may not exceed 3000, so split the batch",
        }));
    }
    let mut outcomes = Vec::with_capacity(student_ids.len());
    for student_id in student_ids {
        let student = UserId::from_key(&student_id);
        // A fee record belongs to a student; billing anyone else is a typo, and
        // a typo here is money against the wrong person.
        let is_student = crate::db::user::read(db, &student)
            .await?
            .is_some_and(|user| user.get_role() == Role::Student);
        let outcome = if is_student {
            match assign(db, &plan, &student, assigner).await {
                Ok((_, true)) => Outcome::AlreadyAssigned,
                Ok((_, false)) => Outcome::Assigned,
                // The plan was deleted mid-batch. That is a per-student outcome
                // like any other, not a reason to throw away the report for the
                // students this batch already billed — their charges are
                // written and the caller has to be told about them.
                Err(AppError::NotFound) => Outcome::Rejected("no such plan"),
                Err(err) => return Err(err),
            }
        } else {
            Outcome::Rejected("no such student")
        };
        outcomes.push((student_id, outcome));
    }
    Ok(outcomes)
}

/// The placement, for callers that only inspect it.
pub async fn read(
    db: &Database,
    id: &FeePlanAssignmentId,
) -> Result<Option<FeePlanAssignment>, AppError> {
    fee_plan_assignment::read(db, id).await
}

/// Who is on this plan, newest first.
pub async fn list_for_plan(
    db: &Database,
    plan: &crate::domain::fee_plan::FeePlanId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<FeePlanAssignment>, i64), AppError> {
    fee_plan_assignment::list_for_plan(db, plan, limit, offset).await
}

/// Which plans a student is on.
pub async fn list_for_student(
    db: &Database,
    student: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<FeePlanAssignment>, i64), AppError> {
    fee_plan_assignment::list_for_student(db, student, limit, offset).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_test_db;
    use crate::domain::fee_plan::{FeePlanName, Installment};
    use crate::domain::payment_ledger::LedgerAmount;

    /// A real `app_user` row: managers and students are foreign keys now. The
    /// label names the row's username; the id is minted, so repeated calls are
    /// new people, not the same row.
    async fn a_person(db: &Database, label: &str, role: &str) -> UserId {
        let user = UserId::generate();
        sqlx::query(
            "INSERT INTO app_user (id, username, password_hash, role) \
             VALUES ($1, $2, 'x', $3)",
        )
        .bind(user.uuid())
        .bind(format!("{label}-{}", &user.key()[30..]))
        .bind(role)
        .execute(db)
        .await
        .unwrap();
        user
    }

    async fn a_plan(db: &Database, installments: Vec<Installment>) -> FeePlan {
        let manager = a_person(db, "mgr", "manager").await;
        fee_plan::create(
            db,
            FeePlanName::try_new("Yearly").unwrap(),
            installments,
            &manager,
        )
        .await
        .unwrap()
    }

    /// The invariant the whole module exists for: assigning bills every
    /// installment once, and assigning *again* bills nothing — the second call
    /// must not double a family's debt.
    #[tokio::test]
    async fn assigning_bills_every_installment_and_a_replay_bills_nothing() {
        let (db, _leases) = init_test_db().await;
        let manager = a_person(&db, "mgr", "manager").await;
        let student = a_person(&db, "stu", "student").await;
        let plan = a_plan(
            &db,
            vec![
                Installment::new(
                    LedgerAmount::try_new(150_000).unwrap(),
                    Timestamp::from_millis(1_000),
                ),
                Installment::new(
                    LedgerAmount::try_new(250_000).unwrap(),
                    Timestamp::from_millis(2_000),
                ),
            ],
        )
        .await;

        let (_, existed) = assign(&db, &plan, &student, &manager).await.unwrap();
        assert!(!existed);
        assert_eq!(
            payment_ledger::balance_of(&db, &student).await.unwrap(),
            -400_000,
            "both installments must be owed"
        );

        let (_, existed) = assign(&db, &plan, &student, &manager).await.unwrap();
        assert!(
            existed,
            "the second assign is a replay, not a new placement"
        );
        let (lines, _) = payment_ledger::list_for_student(&db, &student, None, 0)
            .await
            .unwrap();
        assert_eq!(lines.len(), 2, "a replay may not append a single line");
        assert!(
            lines
                .iter()
                .all(|l| l.get_kind() == crate::domain::payment_ledger::PaymentLedgerKind::Charge)
        );
        assert_eq!(
            payment_ledger::balance_of(&db, &student).await.unwrap(),
            -400_000
        );
        assert!(
            fee_plan_assignment::exists_for_plan(&db, plan.get_id())
                .await
                .unwrap()
        );
    }

    /// The race the refcount exists for: an edit and an assign, both licensed
    /// by the same instant.
    ///
    /// "Neither lands" is *not* the invariant — the two are legal in one order
    /// (edit, then assign the edited plan) and illegal in the other. What may
    /// never happen is the money and the plan disagreeing: the charges raised
    /// must be copies of the schedule the plan actually ends up carrying, and
    /// once anybody is on the plan no further edit or delete may land at all.
    ///
    /// Asserted against the **stored** state, never against which call returned
    /// `Ok`: a winner's word is not evidence about what the store kept.
    /// The one thing the *returned* values are good for is the second
    /// invariant: neither racer may answer 500. Losing a single-record round to
    /// a rival is the ordinary way this guard works, and the loser's recovery is
    /// to re-send, not to fail the request.
    ///
    /// Multi-threaded on purpose: on the single-threaded runtime the two tasks
    /// interleave only at await points and the store never reports a conflict at
    /// all, which is how a version of this test watched a 45-in-50 500 rate and
    /// passed.
    ///
    /// Real server, and `#[ignore]`d rather than falling back to `init_mem`:
    /// the embedded engine drops one of two concurrent writes to a record and
    /// answers `Ok` to both, which fails this very assertion out of nowhere
    /// (measured 2026-07-30: 2 runs in 36 on `memory` under host load, 0 in
    /// 10 000 rounds on the server). See [`crate::database::init_test_server`].
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_edit_racing_an_assign_leaves_the_plan_and_the_money_agreeing() {
        let (db, _leases) = crate::database::init_test_db().await;
        let mut reached = 0;
        for round in 0..20 {
            let manager = a_person(&db, "mgr", "manager").await;
            let student = a_person(&db, "stu", "student").await;
            let plan = a_plan(
                &db,
                vec![Installment::new(
                    LedgerAmount::try_new(100).unwrap(),
                    Timestamp::from_millis(1_000),
                )],
            )
            .await;

            let edit = {
                let (plan, db) = (plan.clone(), db.clone());
                tokio::spawn(async move {
                    fee_plan::update(
                        &db,
                        plan,
                        None,
                        Some(vec![Installment::new(
                            LedgerAmount::try_new(999).unwrap(),
                            Timestamp::from_millis(2_000),
                        )]),
                    )
                    .await
                })
            };
            let assign = {
                let (db, plan, manager, student) =
                    (db.clone(), plan.clone(), manager.clone(), student.clone());
                tokio::spawn(async move { assign(&db, &plan, &student, &manager).await })
            };
            let (edit, assign) = (edit.await.unwrap(), assign.await.unwrap());
            assert!(
                !matches!(edit, Err(AppError::Db(_))),
                "a raced edit must be refused, not 500: {edit:?}"
            );
            assert!(
                !matches!(assign, Err(AppError::Db(_))),
                "a raced assign must retry, not 500: {assign:?}"
            );

            let stored = fee_plan::read(&db, plan.get_id()).await.unwrap().unwrap();
            let assigned = fee_plan_assignment::exists_for_plan(&db, plan.get_id())
                .await
                .unwrap();
            if assigned {
                reached += 1;
                // The counter is what refuses every later edit, so it has to
                // agree with the rows it stands for.
                let (lines, _) = payment_ledger::list_for_student(&db, &student, None, 0)
                    .await
                    .unwrap();
                assert_eq!(lines.len(), 1, "one installment, one charge");
                assert_eq!(
                    lines[0].get_amount_minor().as_minor(),
                    stored.get_installments()[0].get_amount_minor().as_minor(),
                    "the charge must be a copy of the plan as it now stands"
                );
                assert!(
                    fee_plan::update(&db, plan.clone(), None, None)
                        .await
                        .is_ok(),
                    "an empty PATCH writes nothing, so it is not refused"
                );
                assert!(
                    matches!(
                        fee_plan::update(
                            &db,
                            plan.clone(),
                            Some(FeePlanName::try_new("Nope").unwrap()),
                            None
                        )
                        .await,
                        Err(AppError::Conflict(_))
                    ),
                    "an assigned plan stays frozen afterwards"
                );
                assert!(
                    !fee_plan::delete(&db, plan.clone()).await.unwrap(),
                    "and it cannot be deleted either"
                );
            }
        }
        // Every assertion above sits behind "the assign landed", so a run where
        // it never did asserts nothing at all — the failure this counter turns
        // into a loud one.
        assert!(reached > 0, "no round ever placed the assignment");
    }

    /// The other half, and here "never both" *is* the invariant: a delete and
    /// an assign racing must never leave a live assignment — with the charges
    /// it billed — pointing at a plan that is gone. Stored state again, and a
    /// real server again, for the same reasons.
    ///
    /// Both orderings are forced, because the interesting one does not happen on
    /// its own: with both racers released together the assign wins ~24 rounds in
    /// 25, so a run that only ever saw that ordering never tested a delete
    /// landing first at all. Half the rounds therefore hold the assign back by a
    /// beat, and the two counters below fail the test if either ordering went
    /// unseen. Neither racer may answer 500 here either — this is the pair that
    /// contends hardest, both writing the plan record itself.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_delete_racing_an_assign_never_orphans_an_assignment() {
        let (db, _leases) = crate::database::init_test_db().await;
        let (mut deleted_first, mut assigned_first) = (0, 0);
        for round in 0..20 {
            let hold_back_the_assign = round % 2 == 0;
            let manager = a_person(&db, "mgr", "manager").await;
            let student = a_person(&db, "stu", "student").await;
            let plan = a_plan(
                &db,
                vec![Installment::new(
                    LedgerAmount::try_new(100).unwrap(),
                    Timestamp::from_millis(1_000),
                )],
            )
            .await;

            let head_start = std::time::Duration::from_millis(5);
            let drop_it = {
                let (db, plan) = (db.clone(), plan.clone());
                tokio::spawn(async move {
                    if !hold_back_the_assign {
                        tokio::time::sleep(head_start).await;
                    }
                    fee_plan::delete(&db, plan).await
                })
            };
            let assign = {
                let (db, plan, manager, student) =
                    (db.clone(), plan.clone(), manager.clone(), student.clone());
                tokio::spawn(async move {
                    if hold_back_the_assign {
                        tokio::time::sleep(head_start).await;
                    }
                    assign(&db, &plan, &student, &manager).await
                })
            };
            let (drop_it, assign) = (drop_it.await.unwrap(), assign.await.unwrap());
            assert!(
                !matches!(drop_it, Err(AppError::Db(_))),
                "a raced delete must retry, not 500: {drop_it:?}"
            );
            assert!(
                !matches!(assign, Err(AppError::Db(_))),
                "a raced assign must retry, not 500: {assign:?}"
            );

            let gone = fee_plan::read(&db, plan.get_id()).await.unwrap().is_none();
            let assigned = fee_plan_assignment::exists_for_plan(&db, plan.get_id())
                .await
                .unwrap();
            deleted_first += usize::from(gone);
            assigned_first += usize::from(assigned);
            assert!(
                !(gone && assigned),
                "an assignment may not outlive the plan it names"
            );
            let (lines, _) = payment_ledger::list_for_student(&db, &student, None, 0)
                .await
                .unwrap();
            assert!(
                !(gone && !lines.is_empty()),
                "and neither may the charges it raised"
            );
        }
        // The premise, not a nicety: "never both" is trivially true in a run
        // where one of the two never landed, so a run that only saw one
        // ordering has proven nothing and says so.
        assert!(deleted_first > 0, "no round ever let the delete land first");
        assert!(
            assigned_first > 0,
            "no round ever let the assign land first"
        );
    }
}
