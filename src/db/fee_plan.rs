//! The `fee_plan` table: the create, the reads, and the guarded edit and
//! delete. Both guards are conditional single-record writes on the plan's own
//! refcount (`assignment_count`) — the WHERE clause *is* the assigned-plan
//! freeze rule — so they live here whole. The web-facing doors are
//! [`crate::service::fee_plan`].
//!
//! Postgres replaces the old process lock with the row itself: the assign's
//! claim (`UPDATE fee_plan SET assignment_count = …` in
//! [`crate::db::fee_plan_assignment::create`]) and the guarded edit/delete
//! below all rewrite this one row, so they take its row lock and re-check
//! their condition against the committed winner — an assign racing an edit
//! either claims first (the edit is refused) or claims after (and bills the
//! edited plan, which it re-reads). No window, no mutex.

use crate::database::Database;
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
    let installments = serde_json::to_value(&installments)
        .map_err(|e| AppError::Internal(format!("fee plan encode: {e}")))?;
    let id = FeePlanId::generate();
    let created_at = Timestamp::now();
    sqlx::query_as!(
        FeePlan,
        "INSERT INTO fee_plan (id, name, installments, created_by, created_at) \
         VALUES ($1, $2, $3, $4, $5) \
         RETURNING id AS \"id: FeePlanId\", name AS \"name: FeePlanName\", installments AS \"installments: sqlx::types::Json<Vec<Installment>>\", created_by AS \"created_by: UserId\", created_at AS \"created_at: Timestamp\"",
        id.uuid(),
        name.as_str(),
        installments,
        created_by.uuid(),
        created_at.as_millis(),
    )
    .fetch_one(db)
    .await
    .map_err(Into::into)
}

pub async fn read(db: &Database, id: &FeePlanId) -> Result<Option<FeePlan>, AppError> {
    sqlx::query_as!(
        FeePlan,
        "SELECT id AS \"id: FeePlanId\", name AS \"name: FeePlanName\", installments AS \"installments: sqlx::types::Json<Vec<Installment>>\", created_by AS \"created_by: UserId\", created_at AS \"created_at: Timestamp\"
         FROM fee_plan WHERE id = $1",
        id.uuid(),
    )
    .fetch_optional(db)
    .await
    .map_err(Into::into)
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
/// omitted it, so the column is left alone (`COALESCE` keeps the stored
/// value) rather than re-stated from the snapshot this struct was read into.
/// Zero rows = refused, nothing was written: somebody is already on the plan.
///
/// Editing a plan never moves money: charges are frozen copies of the
/// installments as they stood when the plan was assigned. That is exactly
/// why an assigned plan may not be edited at all — the plan and the charges
/// raised from it would tell different stories. The roster is the plan's own
/// `assignment_count` refcount, claimed in the same statement as the
/// assignment row ([`crate::db::fee_plan_assignment::create`]), so the check
/// and the write are one conditional `UPDATE` on one record: an assign
/// racing this either claims first (and the edit is refused) or claims
/// after (and bills the edited plan, which it re-reads). A
/// `SELECT`-then-write could be, and was, stepped over in the gap between
/// the two.
///
/// A request that carried no field at all is answered before the guarded
/// statement: it writes nothing (and must not be refused as "assigned"),
/// so the row is simply read back.
pub async fn update(
    db: &Database,
    plan: FeePlan,
    name: Option<FeePlanName>,
    installments: Option<Vec<Installment>>,
) -> Result<FeePlan, AppError> {
    if let Some(installments) = &installments {
        validate_installments(installments)?;
    }
    if name.is_none() && installments.is_none() {
        // A PATCH asking for no change writes nothing; the row (or its 404)
        // is the whole answer, assigned or not.
        return read(db, &plan.id).await?.ok_or(AppError::NotFound);
    }
    let installments = installments
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|e| AppError::Internal(format!("fee plan encode: {e}")))?;
    let edited = sqlx::query_as!(
        FeePlan,
        "UPDATE fee_plan \
         SET name = COALESCE($2, name), installments = COALESCE($3, installments) \
         WHERE id = $1 AND assignment_count = 0 \
         RETURNING id AS \"id: FeePlanId\", name AS \"name: FeePlanName\", installments AS \"installments: sqlx::types::Json<Vec<Installment>>\", created_by AS \"created_by: UserId\", created_at AS \"created_at: Timestamp\"",
        plan.id.uuid(),
        name.map(|n| n.as_str().to_string()),
        installments,
    )
    .fetch_optional(db)
    .await?;
    // Still assigned or already gone: the guarded `UPDATE` cannot tell those
    // apart (zero rows covers both), so — exactly as [`delete`] does — only
    // the refusal path pays for the read that can. A plan deleted in the
    // window since the handler read it must not be refused as "assigned": it
    // never was.
    match edited {
        Some(plan) => Ok(plan),
        None => match read(db, &plan.id).await? {
            Some(_) => Err(AppError::Conflict("an assigned plan cannot be edited")),
            None => Err(AppError::NotFound),
        },
    }
}

/// Delete the plan, but only while nobody is on it. `false` = refused,
/// nothing was written; `Err(NotFound)` keeps the answer a concurrent
/// *delete* gives. Same one-record guard as [`update`], and the
/// same reason — the charges an assignment raised name this plan, and a
/// school's financial history keeps its references.
///
/// A single guarded statement needs no retry loop: the guard rewrites the
/// plan row itself, so it contends with an assign's claim on that row's lock
/// and whichever version lands, the loser re-evaluated against the winner's
/// committed state. Nothing was written on a loss, so there is nothing to
/// unwind.
pub async fn delete(db: &Database, plan: FeePlan) -> Result<bool, AppError> {
    let deleted = sqlx::query!(
        "DELETE FROM fee_plan WHERE id = $1 AND assignment_count = 0 RETURNING id",
        plan.id.uuid(),
    )
    .fetch_optional(db)
    .await?;
    if deleted.is_some() {
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
