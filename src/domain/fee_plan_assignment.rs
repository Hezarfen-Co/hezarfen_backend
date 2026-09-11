//! One fee plan placed on one student — and the moment the plan becomes money
//! owed: assigning appends *every* installment of the plan as a charge line at
//! once, each carrying its own due date.
//!
//! The row is keyed `<plan>_<student>`, and every charge it raises is keyed
//! from that same key plus the installment number, so the whole operation is
//! idempotent **by identity**: assigning twice writes nothing the second time,
//! however the two requests interleave, and an assign cut short after three of
//! twelve charges landed completes itself when it is repeated. No scan decides
//! "has this been billed yet?" — a scan can be slipped past by a concurrent
//! writer, and the cost of that mistake here is a double-billed family.
//!
//! Nothing is ever edited or deleted: unassigning is not a thing, because the
//! charges are already history. A plan raised in error is undone by reversing
//! its charges. That is also why the plan's assignment refcount is only ever
//! claimed and never released — a plan, once assigned, stays frozen for good.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{FEE_PLAN_ASSIGNMENT_COUNT_FIELD, FEE_PLAN_ASSIGNMENT_TABLE};
use crate::database::Database;
use crate::db::cap::{self, Claimed};
use crate::domain::fee_plan::{FeePlan, FeePlanId};
use crate::db::page::PagedList;
use crate::domain::payment_ledger::PaymentLedger;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct FeePlanAssignmentId(RecordId);

impl FeePlanAssignmentId {
    /// A deterministic id for the (plan, student) pair — one assignment per
    /// pair by construction, and the prefix every charge line of that
    /// assignment is keyed from. ULID keys are alphanumeric, so `_` is an
    /// unambiguous joiner.
    pub fn composite(plan: &FeePlanId, student: &UserId) -> Self {
        Self(RecordId::new(
            FEE_PLAN_ASSIGNMENT_TABLE,
            format!("{}_{}", plan.key(), student.key()),
        ))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(FEE_PLAN_ASSIGNMENT_TABLE, key))
    }

    pub fn record(&self) -> RecordId {
        self.0.clone()
    }

    pub fn key(&self) -> &str {
        match &self.0.key {
            RecordIdKey::String(key) => key,
            _ => "",
        }
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct FeePlanAssignment {
    id: FeePlanAssignmentId,
    plan: FeePlanId,
    student: UserId,
    assigned_by: UserId,
    created_at: Timestamp,
}

impl FeePlanAssignment {
    pub fn get_id(&self) -> &FeePlanAssignmentId {
        &self.id
    }

    pub fn get_plan(&self) -> &FeePlanId {
        &self.plan
    }

    pub fn get_student(&self) -> &UserId {
        &self.student
    }

    pub fn get_assigned_by(&self) -> &UserId {
        &self.assigned_by
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }

    /// Place `plan` on `student` and bill it. Returns the assignment and
    /// whether it was *already* there, so the web layer can answer a replay
    /// honestly instead of pretending it just happened.
    ///
    /// The row and the plan's assignment refcount are written in **one**
    /// transaction ([`cap::claim_and_create`]): that increment is what makes the
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
        plan: &FeePlan,
        student: &UserId,
        assigned_by: &UserId,
        db: &Database,
    ) -> Result<(FeePlanAssignment, bool), AppError> {
        let id = FeePlanAssignmentId::composite(plan.get_id(), student);
        let row = FeePlanAssignment {
            id: id.clone(),
            plan: plan.get_id().clone(),
            student: student.clone(),
            assigned_by: assigned_by.clone(),
            created_at: Timestamp::now(),
        };
        let claimed = cap::claim_and_create(
            &plan.get_id().record(),
            FEE_PLAN_ASSIGNMENT_COUNT_FIELD,
            cap::UNLIMITED,
            &id.record(),
            &row,
            db,
        )
        .await?;
        let (assignment, existed) = match claimed {
            Claimed::Made(created) => (created, false),
            // Someone assigned this pair first — their row is the answer, and
            // the charges below are keyed the same, so continuing here can only
            // *complete* the billing. Their claim already counts this row.
            Claimed::Duplicate => (Self::require(&id, db).await?, true),
            // The counter is uncapped, so the only miss is a plan that is gone.
            Claimed::Full => return Err(AppError::NotFound),
        };
        // Frozen as of the claim above: no edit can land past it any more.
        let plan = FeePlan::read(plan.get_id(), db)
            .await?
            .ok_or_else(|| AppError::Internal("the assigned fee plan vanished".into()))?;
        for (index, installment) in plan.get_installments().iter().enumerate() {
            PaymentLedger::charge_for_installment(
                &assignment,
                index + 1,
                installment,
                assigned_by,
                db,
            )
            .await?;
        }
        Ok((assignment, existed))
    }

    async fn require(
        id: &FeePlanAssignmentId,
        db: &Database,
    ) -> Result<FeePlanAssignment, AppError> {
        Self::read(id, db)
            .await?
            .ok_or_else(|| AppError::Internal("failed to assign the fee plan".into()))
    }

    pub async fn read(
        id: &FeePlanAssignmentId,
        db: &Database,
    ) -> Result<Option<FeePlanAssignment>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// Who is on this plan, newest first.
    pub async fn list_for_plan(
        plan: &FeePlanId,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<FeePlanAssignment>, i64), AppError> {
        PagedList::new(
            "fee_plan_assignment WHERE plan = $plan",
            "ORDER BY created_at DESC, id DESC",
        )
        .bind("plan", plan.record())
        .run(limit, offset, db)
        .await
    }

    /// Is anybody on this plan? A scan, and no longer a guard: the edit and
    /// delete guards read the plan's own refcount, which no concurrent assign
    /// can be behind. Kept because a *test* asserting the rows and the counter
    /// agree is the only thing that would catch the counter drifting.
    pub async fn exists_for_plan(plan: &FeePlanId, db: &Database) -> Result<bool, AppError> {
        let mut result = db
            .query("SELECT VALUE id FROM fee_plan_assignment WHERE plan = $plan LIMIT 1")
            .bind(("plan", plan.record()))
            .await?
            .check()?;
        Ok(!result.take::<Vec<RecordId>>(0)?.is_empty())
    }

    /// Which plans a student is on — one student may hold several.
    pub async fn list_for_student(
        student: &UserId,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<FeePlanAssignment>, i64), AppError> {
        PagedList::new(
            "fee_plan_assignment WHERE student = $student",
            "ORDER BY created_at DESC, id DESC",
        )
        .bind("student", student.record())
        .run(limit, offset, db)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::fee_plan::{FeePlanName, Installment};
    use crate::domain::payment_ledger::{LedgerAmount, PaymentLedgerKind};

    /// The invariant the whole module exists for: assigning bills every
    /// installment once, and assigning *again* bills nothing — the second call
    /// must not double a family's debt.
    #[tokio::test]
    async fn assigning_bills_every_installment_and_a_replay_bills_nothing() {
        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("mgr1");
        let student = UserId::from_key("stu1");
        let plan = FeePlan::create(
            FeePlanName::try_new("Yearly").unwrap(),
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
            &manager,
            &db,
        )
        .await
        .unwrap();

        let (_, existed) = FeePlanAssignment::assign(&plan, &student, &manager, &db)
            .await
            .unwrap();
        assert!(!existed);
        assert_eq!(
            PaymentLedger::balance_of(&student, &db).await.unwrap(),
            -400_000,
            "both installments must be owed"
        );

        let (_, existed) = FeePlanAssignment::assign(&plan, &student, &manager, &db)
            .await
            .unwrap();
        assert!(
            existed,
            "the second assign is a replay, not a new placement"
        );
        let (lines, _) = PaymentLedger::list_for_student(&student, None, 0, &db)
            .await
            .unwrap();
        assert_eq!(lines.len(), 2, "a replay may not append a single line");
        assert!(
            lines
                .iter()
                .all(|l| l.get_kind() == PaymentLedgerKind::Charge)
        );
        assert_eq!(
            PaymentLedger::balance_of(&student, &db).await.unwrap(),
            -400_000
        );
        assert!(
            FeePlanAssignment::exists_for_plan(plan.get_id(), &db)
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
    #[ignore = "needs a real SurrealDB server: podman start hezarfen-surrealdb && cargo test -- --ignored"]
    async fn an_edit_racing_an_assign_leaves_the_plan_and_the_money_agreeing() {
        let (db, _serialized) = crate::database::init_test_server("edit_race").await;
        let mut reached = 0;
        for round in 0..20 {
            let manager = UserId::from_key("mgr1");
            let student = UserId::from_key(&format!("stu{round}"));
            let plan = FeePlan::create(
                FeePlanName::try_new("Yearly").unwrap(),
                vec![Installment::new(
                    LedgerAmount::try_new(100).unwrap(),
                    Timestamp::from_millis(1_000),
                )],
                &manager,
                &db,
            )
            .await
            .unwrap();

            let edit = {
                let (plan, db) = (plan.clone(), db.clone());
                tokio::spawn(async move {
                    plan.update(
                        None,
                        Some(vec![Installment::new(
                            LedgerAmount::try_new(999).unwrap(),
                            Timestamp::from_millis(2_000),
                        )]),
                        &db,
                    )
                    .await
                })
            };
            let assign = {
                let (plan, db, manager, student) =
                    (plan.clone(), db.clone(), manager.clone(), student.clone());
                tokio::spawn(async move {
                    FeePlanAssignment::assign(&plan, &student, &manager, &db).await
                })
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

            let stored = FeePlan::read(plan.get_id(), &db).await.unwrap().unwrap();
            let assigned = FeePlanAssignment::exists_for_plan(plan.get_id(), &db)
                .await
                .unwrap();
            if assigned {
                reached += 1;
                // The counter is what refuses every later edit, so it has to
                // agree with the rows it stands for.
                let (lines, _) = PaymentLedger::list_for_student(&student, None, 0, &db)
                    .await
                    .unwrap();
                assert_eq!(lines.len(), 1, "one installment, one charge");
                assert_eq!(
                    lines[0].get_amount_minor().as_minor(),
                    stored.get_installments()[0].get_amount_minor().as_minor(),
                    "the charge must be a copy of the plan as it now stands"
                );
                assert!(
                    plan.clone().update(None, None, &db).await.is_ok(),
                    "an empty PATCH writes nothing, so it is not refused"
                );
                assert!(
                    matches!(
                        plan.clone()
                            .update(Some(FeePlanName::try_new("Nope").unwrap()), None, &db)
                            .await,
                        Err(AppError::Conflict(_))
                    ),
                    "an assigned plan stays frozen afterwards"
                );
                assert!(
                    !plan.clone().delete(&db).await.unwrap(),
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
    #[ignore = "needs a real SurrealDB server: podman start hezarfen-surrealdb && cargo test -- --ignored"]
    async fn a_delete_racing_an_assign_never_orphans_an_assignment() {
        let (db, _serialized) = crate::database::init_test_server("delete_race").await;
        let (mut deleted_first, mut assigned_first) = (0, 0);
        for round in 0..20 {
            let hold_back_the_assign = round % 2 == 0;
            let manager = UserId::from_key("mgr1");
            let student = UserId::from_key(&format!("stu{round}"));
            let plan = FeePlan::create(
                FeePlanName::try_new("Yearly").unwrap(),
                vec![Installment::new(
                    LedgerAmount::try_new(100).unwrap(),
                    Timestamp::from_millis(1_000),
                )],
                &manager,
                &db,
            )
            .await
            .unwrap();

            let head_start = std::time::Duration::from_millis(5);
            let drop_it = {
                let (plan, db) = (plan.clone(), db.clone());
                tokio::spawn(async move {
                    if !hold_back_the_assign {
                        tokio::time::sleep(head_start).await;
                    }
                    plan.delete(&db).await
                })
            };
            let assign = {
                let (plan, db, manager, student) =
                    (plan.clone(), db.clone(), manager.clone(), student.clone());
                tokio::spawn(async move {
                    if hold_back_the_assign {
                        tokio::time::sleep(head_start).await;
                    }
                    FeePlanAssignment::assign(&plan, &student, &manager, &db).await
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

            let gone = FeePlan::read(plan.get_id(), &db).await.unwrap().is_none();
            let assigned = FeePlanAssignment::exists_for_plan(plan.get_id(), &db)
                .await
                .unwrap();
            deleted_first += usize::from(gone);
            assigned_first += usize::from(assigned);
            assert!(
                !(gone && assigned),
                "an assignment may not outlive the plan it names"
            );
            let (lines, _) = PaymentLedger::list_for_student(&student, None, 0, &db)
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
