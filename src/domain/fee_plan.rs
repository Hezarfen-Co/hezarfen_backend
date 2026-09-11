//! A school fee plan: a name and the installments it is paid in. Managers
//! write them; assigning one to a student is what turns it into money owed
//! (see [`crate::domain::fee_plan_assignment`]).
//!
//! The installments are **embedded** on the plan row rather than child rows:
//! a charge line copies the amount and due date it was assigned at, so the
//! installment is a template, never a thing that has to be joined back to. No
//! field inside the object is optional — SurrealDB 3 drops an object key whose
//! value is `NONE`, which would make the stored shape differ from the written
//! one.
//!
//! Due dates may be in the **past**: a school adopting the app mid-year
//! legitimately assigns a plan whose first installments were already due, so
//! the no-past rule that guards exams, lessons and events deliberately does not
//! apply here.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{FEE_PLAN_TABLE, MAX_FEE_PLAN_INSTALLMENTS, MAX_FEE_PLAN_NAME_LEN};
use crate::domain::monotonic_id::next_ulid;
use crate::domain::payment_ledger::LedgerAmount;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::validate_required;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct FeePlanId(RecordId);

impl FeePlanId {
    pub fn generate() -> Self {
        Self(RecordId::new(FEE_PLAN_TABLE, next_ulid().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(FEE_PLAN_TABLE, key))
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

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct FeePlanName(String);

impl FeePlanName {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        let value = value.trim();
        validate_required("name", value, MAX_FEE_PLAN_NAME_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One instalment of a plan: what is owed, and when it falls due. Both fields
/// are required — see the module doc on `NONE` keys.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct Installment {
    amount_minor: LedgerAmount,
    due_at: Timestamp,
}

impl Installment {
    pub fn new(amount_minor: LedgerAmount, due_at: Timestamp) -> Self {
        Self {
            amount_minor,
            due_at,
        }
    }

    pub fn get_amount_minor(&self) -> LedgerAmount {
        self.amount_minor
    }

    pub fn get_due_at(&self) -> Timestamp {
        self.due_at
    }
}

/// A plan is at least one installment and at most [`MAX_FEE_PLAN_INSTALLMENTS`]
/// — an empty plan owes nothing and would assign nothing, and the ceiling is
/// what bounds how many ledger lines one assignment may append.
pub fn validate_installments(installments: &[Installment]) -> Result<(), ValidationError> {
    if installments.is_empty() || installments.len() > MAX_FEE_PLAN_INSTALLMENTS {
        return Err(ValidationError::Invalid {
            field: "installments",
            reason: "a plan must carry 1 to 60 installments",
        });
    }
    Ok(())
}

#[derive(Debug, Clone, SurrealValue)]
pub struct FeePlan {
    pub(crate) id: FeePlanId,
    pub(crate) name: FeePlanName,
    pub(crate) installments: Vec<Installment>,
    pub(crate) created_by: UserId,
    pub(crate) created_at: Timestamp,
}

impl FeePlan {
    pub fn get_id(&self) -> &FeePlanId {
        &self.id
    }

    pub fn get_name(&self) -> &FeePlanName {
        &self.name
    }

    pub fn get_installments(&self) -> &[Installment] {
        &self.installments
    }

    pub fn get_created_by(&self) -> &UserId {
        &self.created_by
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_required_trimmed_and_bounded() {
        assert_eq!(
            FeePlanName::try_new("  Yearly  ").unwrap().as_str(),
            "Yearly"
        );
        assert!(FeePlanName::try_new("").is_err());
        assert!(FeePlanName::try_new("   ").is_err());
        assert!(FeePlanName::try_new(&"x".repeat(MAX_FEE_PLAN_NAME_LEN + 1)).is_err());
    }

    #[test]
    fn a_plan_carries_between_one_and_sixty_installments() {
        let one = Installment::new(
            LedgerAmount::try_new(100).unwrap(),
            Timestamp::from_millis(1),
        );
        assert!(validate_installments(&[]).is_err());
        assert!(validate_installments(std::slice::from_ref(&one)).is_ok());
        assert!(validate_installments(&vec![one.clone(); MAX_FEE_PLAN_INSTALLMENTS]).is_ok());
        assert!(validate_installments(&vec![one; MAX_FEE_PLAN_INSTALLMENTS + 1]).is_err());
    }
}
