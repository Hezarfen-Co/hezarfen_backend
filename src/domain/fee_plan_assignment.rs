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

use crate::constant::FEE_PLAN_ASSIGNMENT_TABLE;
use crate::domain::fee_plan::FeePlanId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;

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
    pub(crate) id: FeePlanAssignmentId,
    pub(crate) plan: FeePlanId,
    pub(crate) student: UserId,
    pub(crate) assigned_by: UserId,
    pub(crate) created_at: Timestamp,
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
}
