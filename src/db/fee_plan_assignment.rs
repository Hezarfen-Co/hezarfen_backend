//! The `fee_plan_assignment` table: the reads over placements. The row is
//! written by [`crate::service::fee_plan_assignment::assign`], whose claim
//! ([`crate::db::cap::claim_and_create`]) is the write that freezes the plan.

use surrealdb::types::RecordId;

use crate::database::Database;
use crate::db::page::PagedList;
use crate::domain::fee_plan::FeePlanId;
use crate::domain::fee_plan_assignment::{FeePlanAssignment, FeePlanAssignmentId};
use crate::domain::user::UserId;
use crate::error::AppError;

pub async fn read(
    db: &Database,
    id: &FeePlanAssignmentId,
) -> Result<Option<FeePlanAssignment>, AppError> {
    Ok(db.select(id.record()).await?)
}

/// Who is on this plan, newest first.
pub async fn list_for_plan(
    db: &Database,
    plan: &FeePlanId,
    limit: Option<i64>,
    offset: i64,
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
pub async fn exists_for_plan(db: &Database, plan: &FeePlanId) -> Result<bool, AppError> {
    let mut result = db
        .query("SELECT VALUE id FROM fee_plan_assignment WHERE plan = $plan LIMIT 1")
        .bind(("plan", plan.record()))
        .await?
        .check()?;
    Ok(!result.take::<Vec<RecordId>>(0)?.is_empty())
}

/// Which plans a student is on — one student may hold several.
pub async fn list_for_student(
    db: &Database,
    student: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<FeePlanAssignment>, i64), AppError> {
    PagedList::new(
        "fee_plan_assignment WHERE student = $student",
        "ORDER BY created_at DESC, id DESC",
    )
    .bind("student", student.record())
    .run(limit, offset, db)
    .await
}
