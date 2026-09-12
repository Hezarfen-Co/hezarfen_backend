//! One fee plan placed on one student — and the moment the plan becomes money
//! owed: assigning appends *every* installment of the plan as a charge line at
//! once, each carrying its own due date.
//!
//! The row is keyed (plan, student), and every charge it raises is keyed from
//! that same pair plus the installment number, so the whole operation is
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

use crate::domain::fee_plan::FeePlanId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;

/// The identity of one (plan, student) pair. Not a row column: the table's
/// primary key *is* the pair — one assignment per pair by construction — and
/// this struct's job is the underscore-joined wire form at the HTTP edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeePlanAssignmentId {
    pub(crate) plan: FeePlanId,
    pub(crate) student: UserId,
}

impl FeePlanAssignmentId {
    pub fn composite(plan: &FeePlanId, student: &UserId) -> Self {
        Self {
            plan: plan.clone(),
            student: *student,
        }
    }

    /// Splits a wire key back into its pair. A malformed key (no `_` joiner,
    /// unparseable halves) yields nil components, which match no row — the
    /// same 404 a well-formed but absent key gets.
    pub fn from_key(key: &str) -> Self {
        let (plan, student) = match key.split_once('_') {
            Some((plan, student)) => (plan, student),
            None => ("", ""),
        };
        Self {
            plan: FeePlanId::from_key(plan),
            student: UserId::from_key(student),
        }
    }

    /// The underscore-joined wire form (`{plan}_{student}`).
    pub fn key(&self) -> String {
        format!("{}_{}", self.plan.key(), self.student.key())
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FeePlanAssignment {
    pub(crate) plan: FeePlanId,
    pub(crate) student: UserId,
    pub(crate) assigned_by: UserId,
    pub(crate) created_at: Timestamp,
}

impl FeePlanAssignment {
    /// The row's identity, built back from its primary-key columns.
    pub fn get_id(&self) -> FeePlanAssignmentId {
        FeePlanAssignmentId::composite(&self.plan, &self.student)
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

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = "0198f1a2-0000-7000-8000-000000000001";
    const STUDENT: &str = "0198f1a2-1111-7000-8000-000000000002";

    /// The wire key is the pair, and the pair survives the round trip — this
    /// is what lets a charge line's `assignment` reference travel as one path
    /// segment.
    #[test]
    fn the_wire_key_round_trips_through_the_pair() {
        let plan = FeePlanId::from_key(PLAN);
        let student = UserId::from_key(STUDENT);
        let id = FeePlanAssignmentId::composite(&plan, &student);
        assert_eq!(id.key(), format!("{PLAN}_{STUDENT}"));
        assert_eq!(FeePlanAssignmentId::from_key(&id.key()), id);
    }

    /// A key that is not a pair cannot name an assignment: it decodes to nil
    /// components, which match no row (a 404, never a panic).
    #[test]
    fn a_malformed_key_decodes_to_something_that_matches_nothing() {
        let junk = FeePlanAssignmentId::from_key("not-a-pair");
        assert_eq!(junk.plan.key(), uuid::Uuid::nil().to_string());
        assert_eq!(junk.student.key(), uuid::Uuid::nil().to_string());
    }
}
