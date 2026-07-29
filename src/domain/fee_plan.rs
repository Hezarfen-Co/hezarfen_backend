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
use crate::database::Database;
use crate::domain::field_update::FieldUpdate;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::page::PagedList;
use crate::domain::payment_ledger::LedgerAmount;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
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
    id: FeePlanId,
    name: FeePlanName,
    installments: Vec<Installment>,
    created_by: UserId,
    created_at: Timestamp,
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

    pub async fn create(
        name: FeePlanName,
        installments: Vec<Installment>,
        created_by: &UserId,
        db: &Database,
    ) -> Result<FeePlan, AppError> {
        validate_installments(&installments)?;
        let plan = FeePlan {
            id: FeePlanId::generate(),
            name,
            installments,
            created_by: created_by.clone(),
            created_at: Timestamp::now(),
        };
        let created: Option<FeePlan> = db.create(plan.id.record()).content(plan).await?;
        created.ok_or_else(|| AppError::Internal("failed to create fee plan".into()))
    }

    pub async fn read(id: &FeePlanId, db: &Database) -> Result<Option<FeePlan>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// Every plan, newest first — a school runs a handful.
    pub async fn list_all(
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<FeePlan>, i64), AppError> {
        PagedList::new("fee_plan", "ORDER BY created_at DESC, id DESC")
            .run(limit, offset, db)
            .await
    }

    /// Write only the fields the PATCH carried — `None` means the request
    /// omitted it, so the column is left alone rather than re-stated from the
    /// snapshot this struct was read into.
    ///
    /// Editing a plan never moves money: charges are frozen copies of the
    /// installments as they stood when the plan was assigned. The web layer
    /// refuses the edit outright once [`FeePlan::has_assignments`] holds, so a
    /// plan and the charges raised from it cannot drift apart in the first
    /// place.
    pub async fn update(
        self,
        name: Option<FeePlanName>,
        installments: Option<Vec<Installment>>,
        db: &Database,
    ) -> Result<FeePlan, AppError> {
        if let Some(installments) = &installments {
            validate_installments(installments)?;
        }
        FieldUpdate::new(self.id.record())
            .set("name", name)
            .set("installments", installments)
            .run::<FeePlan>(db)
            .await
    }

    pub async fn delete(self, db: &Database) -> Result<(), AppError> {
        let _: Option<FeePlan> = db.delete(self.id.record()).await?;
        Ok(())
    }

    /// Has this plan ever been assigned? The 409 guard on edit and delete.
    ///
    /// A scan, and deliberately so: it races an assign landing in the same
    /// instant on the other replica, which can leave a plan edited *and*
    /// assigned. Accepted — both versions of the plan were manager-approved,
    /// and the charges the assign raised are frozen copies either way, so no
    /// money moves behind anyone's back.
    pub async fn has_assignments(id: &FeePlanId, db: &Database) -> Result<bool, AppError> {
        crate::domain::fee_plan_assignment::FeePlanAssignment::exists_for_plan(id, db).await
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

    /// The embedded-object DDL (`installments.*.amount_minor`) proved against
    /// the engine: SCHEMAFULL rejects any nested key it was not told about, so
    /// a plan that writes and reads back unchanged is what says the declared
    /// shape and the stored one agree. A past due date is legal on purpose.
    #[tokio::test]
    async fn installments_survive_a_round_trip_through_the_schema() {
        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("mgr1");
        let installments = vec![
            Installment::new(
                LedgerAmount::try_new(150_000).unwrap(),
                Timestamp::from_millis(1_000),
            ),
            Installment::new(
                LedgerAmount::try_new(250_000).unwrap(),
                Timestamp::from_millis(2_000),
            ),
        ];
        let plan = FeePlan::create(
            FeePlanName::try_new("Yearly").unwrap(),
            installments.clone(),
            &manager,
            &db,
        )
        .await
        .unwrap();

        let stored = FeePlan::read(plan.get_id(), &db).await.unwrap().unwrap();
        assert_eq!(stored.get_installments(), installments.as_slice());
        assert_eq!(stored.get_name().as_str(), "Yearly");

        // And a PATCH of the array lands whole.
        let edited = stored
            .update(None, Some(vec![installments[0].clone()]), &db)
            .await
            .unwrap();
        assert_eq!(edited.get_installments().len(), 1);
    }
}
