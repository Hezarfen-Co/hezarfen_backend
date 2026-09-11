//! The `fee_plan` table: the create, the reads, and the guarded edit and
//! delete. Both guards are conditional single-record writes on the plan's own
//! refcount — the WHERE clause *is* the assigned-plan freeze rule — so they
//! live here whole. The web-facing doors are [`crate::service::fee_plan`].

use surrealdb::types::SurrealValue;

use crate::constant::FEE_PLAN_UNASSIGNED_GUARD;
use crate::database::{Database, write_with_retry};
use crate::db::field_update::FieldUpdate;
use crate::db::page::PagedList;
use crate::domain::fee_plan::{
    FeePlan, FeePlanId, FeePlanName, Installment, validate_installments,
};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

pub async fn create(
    db: &Database,
    name: FeePlanName,
    installments: Vec<Installment>,
    created_by: &UserId,
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

pub async fn read(db: &Database, id: &FeePlanId) -> Result<Option<FeePlan>, AppError> {
    Ok(db.select(id.record()).await?)
}

/// Every plan, newest first — a school runs a handful.
pub async fn list_all(
    db: &Database,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<FeePlan>, i64), AppError> {
    PagedList::new("fee_plan", "ORDER BY created_at DESC, id DESC")
        .run(limit, offset, db)
        .await
}

/// Write only the fields the PATCH carried — `None` means the request
/// omitted it, so the column is left alone rather than re-stated from the
/// snapshot this struct was read into. `Err(Conflict)` = refused, nothing
/// was written: somebody is already on the plan.
///
/// Editing a plan never moves money: charges are frozen copies of the
/// installments as they stood when the plan was assigned. That is exactly
/// why an assigned plan may not be edited at all — the plan and the charges
/// raised from it would tell different stories. The roster is the plan's own
/// [`crate::constant::FEE_PLAN_ASSIGNMENT_COUNT_FIELD`] refcount, claimed in the same
/// transaction as the assignment row, so the check and the write are one
/// conditional `UPDATE` on one record: an assign racing this either claims
/// first (and the edit is refused) or claims after (and bills the edited
/// plan, which it re-reads). A `SELECT`-then-write could be, and was,
/// stepped over in the gap between the two.
pub async fn update(
    db: &Database,
    plan: FeePlan,
    name: Option<FeePlanName>,
    installments: Option<Vec<Installment>>,
) -> Result<FeePlan, AppError> {
    if let Some(installments) = &installments {
        validate_installments(installments)?;
    }
    let edited = FieldUpdate::new(plan.id.record())
        .set("name", name)
        .set("installments", installments)
        .guard(
            FEE_PLAN_UNASSIGNED_GUARD,
            AppError::Conflict("an assigned plan cannot be edited"),
        )
        .run::<FeePlan>(db)
        .await;
    // Still assigned or already gone: the guarded `UPDATE` cannot tell those
    // apart either (it reports the refusal it was given), so — exactly as
    // [`crate::db::fee_plan::delete`] does — only the refusal path pays for the read
    // that can. A plan deleted in the window since the handler read it must
    // not be refused as "assigned": it never was.
    if matches!(edited, Err(AppError::Conflict(_))) && read(db, &plan.id).await?.is_none() {
        return Err(AppError::NotFound);
    }
    edited
}

/// Delete the plan, but only while nobody is on it. `false` = refused,
/// nothing was written; `Err(NotFound)` keeps the answer a concurrent
/// *delete* gives. Same one-record guard as [`update`], and the
/// same reason — the charges an assignment raised name this plan, and a
/// school's financial history keeps its references.
///
/// Retried while the store answers "conflict, retry": the guard reads a
/// counter an assign *writes*, so the two contend on this one record by
/// design — and a delete that loses that round has written nothing, so
/// re-sending it is the whole of the recovery. Without the retry an
/// ordinary raced delete answers 500 instead of the 404 or 409 it owes.
pub async fn delete(db: &Database, plan: FeePlan) -> Result<bool, AppError> {
    let sql = format!("DELETE $plan WHERE {FEE_PLAN_UNASSIGNED_GUARD} RETURN BEFORE");
    let deleted: Vec<FeePlan> =
        write_with_retry(db, &sql, &[("plan".into(), plan.id.record().into_value())]).await?;
    if !deleted.is_empty() {
        return Ok(true);
    }
    // Still assigned or already gone: the one statement cannot tell those
    // apart, and only the refusal path pays for the read that can.
    match read(db, &plan.id).await? {
        Some(_) => Ok(false),
        None => Err(AppError::NotFound),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::payment_ledger::LedgerAmount;

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
        let plan = create(
            &db,
            FeePlanName::try_new("Yearly").unwrap(),
            installments.clone(),
            &manager,
        )
        .await
        .unwrap();

        let stored = read(&db, plan.get_id()).await.unwrap().unwrap();
        assert_eq!(stored.get_installments(), installments.as_slice());
        assert_eq!(stored.get_name().as_str(), "Yearly");

        // And a PATCH of the array lands whole.
        let edited = update(&db, stored, None, Some(vec![installments[0].clone()]))
            .await
            .unwrap();
        assert_eq!(edited.get_installments().len(), 1);
    }
}
