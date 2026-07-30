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
//! its charges.

use surrealdb::types::{AlreadyExistsError, RecordId, RecordIdKey, SurrealValue};

use crate::constant::FEE_PLAN_ASSIGNMENT_TABLE;
use crate::database::{Database, lost_the_race};
use crate::domain::fee_plan::{FeePlan, FeePlanId};
use crate::domain::page::PagedList;
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
        let (assignment, existed) = match Self::read(&id, db).await? {
            Some(existing) => (existing, true),
            None => {
                let row = FeePlanAssignment {
                    id: id.clone(),
                    plan: plan.get_id().clone(),
                    student: student.clone(),
                    assigned_by: assigned_by.clone(),
                    created_at: Timestamp::now(),
                };
                match db.create(id.record()).content(row).await {
                    Ok(Some(created)) => (created, false),
                    // Someone assigned this pair first, or the write was
                    // aborted as retryable — either way their row is the
                    // answer, and the charges below are keyed the same, so
                    // continuing here can only *complete* the billing.
                    Ok(None) => (Self::require(&id, db).await?, true),
                    Err(e) if is_duplicate_record(&e) || lost_the_race(&e) => {
                        (Self::require(&id, db).await?, true)
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        };
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

    /// Is anybody on this plan? Backs the edit/delete guard — see
    /// [`FeePlan::has_assignments`] for the race it accepts.
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

/// Did this `CREATE` fail *only* because the row is already there? Same typed
/// match, and the same reasoning, as the ledger's.
fn is_duplicate_record(error: &surrealdb::Error) -> bool {
    matches!(
        error.already_exists_details(),
        Some(AlreadyExistsError::Record { .. })
    )
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
}
