//! Fee-plan doors for the web layer. Every write here is a single guarded
//! statement in [`crate::db::fee_plan`] — the assigned-plan freeze is the
//! store's own, decided as the write lands — so this module is thin
//! pass-throughs; the manager+ gate is the web layer's extractor.

use crate::database::Database;
use crate::db::fee_plan;
use crate::domain::fee_plan::{FeePlan, FeePlanId, FeePlanName, Installment};
use crate::domain::user::UserId;
use crate::error::AppError;

/// Write a fee plan: a name and the installments it is paid in. Creating one
/// bills nobody — assigning it does.
pub async fn create(
    db: &Database,
    name: FeePlanName,
    installments: Vec<Installment>,
    created_by: &UserId,
) -> Result<FeePlan, AppError> {
    fee_plan::create(db, name, installments, created_by).await
}

/// The plan, for callers that only inspect it.
pub async fn read(db: &Database, id: &FeePlanId) -> Result<Option<FeePlan>, AppError> {
    fee_plan::read(db, id).await
}

/// Every plan, newest first.
pub async fn list_all(
    db: &Database,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<FeePlan>, i64), AppError> {
    fee_plan::list_all(db, limit, offset).await
}

/// Edit a plan's name and/or schedule. `Err(Conflict)` = refused, nothing was
/// written: somebody is already on the plan (and `Err(NotFound)` = the plan
/// is gone, so the refusal is not "assigned" but "deleted").
pub async fn update(
    db: &Database,
    plan: FeePlan,
    name: Option<FeePlanName>,
    installments: Option<Vec<Installment>>,
) -> Result<FeePlan, AppError> {
    fee_plan::update(db, plan, name, installments).await
}

/// Delete the plan while nobody is on it. `false` = refused: somebody is on
/// it, and a school's financial history keeps its references.
pub async fn delete(db: &Database, plan: FeePlan) -> Result<bool, AppError> {
    fee_plan::delete(db, plan).await
}
