//! The `fee_plan_assignment` table: the claim that places a student on a plan
//! (and freezes the plan), plus the reads over placements. The billing the
//! claim triggers lives in [`crate::service::fee_plan_assignment::assign`];
//! the ledger appends in [`crate::db::payment_ledger`].

use crate::database::Database;
use crate::db::cap::{self, Claimed};
use crate::db::page::PagedList;
use crate::domain::fee_plan::FeePlanId;
use crate::domain::fee_plan_assignment::{FeePlanAssignment, FeePlanAssignmentId};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

pub async fn read(
    db: &Database,
    id: &FeePlanAssignmentId,
) -> Result<Option<FeePlanAssignment>, AppError> {
    sqlx::query_as!(
        FeePlanAssignment,
        "SELECT plan, student, assigned_by, created_at \
         FROM fee_plan_assignment WHERE plan = $1 AND student = $2",
        id.plan,
        id.student,
    )
    .fetch_optional(db)
    .await
    .map_err(Into::into)
}

/// Place one (plan, student) assignment and claim the plan's assignment
/// refcount in the **same statement** — that increment is what makes the plan
/// un-editable and un-deletable ([`crate::db::fee_plan::update`]/[`delete`]),
/// and it has to be indivisible from the row it counts, or an edit could slip
/// between the two. The claim recipe ([`crate::db::cap`]'s
/// claim-and-create): a conditional `UPDATE` on the plan row seats the count,
/// the `INSERT` rides `WHERE EXISTS` on that seat, and one statement decides.
///
/// The verdict maps as the guard layer promises: 23505 on
/// `fee_plan_assignment_plan` → [`Claimed::Duplicate`] (someone assigned this
/// pair first; their row is the answer); zero rows → [`Claimed::Full`] (the
/// counter is uncapped, so the only miss is a plan a concurrent delete
/// already removed); one row → [`Claimed::Made`].
pub async fn create(
    db: &Database,
    plan: &FeePlanId,
    student: &UserId,
    assigned_by: &UserId,
    created_at: Timestamp,
) -> Result<Claimed<FeePlanAssignment>, AppError> {
    let created = sqlx::query_as!(
        FeePlanAssignment,
        // The seat bump and the row land together or not at all: a refused
        // insert takes its own seat bump back.
        r#"WITH seat AS (
               UPDATE fee_plan SET assignment_count = assignment_count + 1
               WHERE id = $1 AND assignment_count < $2
               RETURNING 1)
           INSERT INTO fee_plan_assignment (plan, student, assigned_by, created_at)
           SELECT $3, $4, $5, $6 WHERE EXISTS (SELECT 1 FROM seat)
           RETURNING plan, student, assigned_by, created_at"#,
        plan,
        cap::UNLIMITED,
        plan,
        student,
        assigned_by,
        created_at,
    )
    .fetch_optional(db)
    .await;
    // Read the verdict off the *error* before the row count: a duplicate is
    // the other writer's win, not a full house.
    match created {
        Ok(Some(row)) => Ok(Claimed::Made(row)),
        Ok(None) => Ok(Claimed::Full),
        Err(e) if crate::database::unique_violation(&e) == Some("fee_plan_assignment_plan") => {
            Ok(Claimed::Duplicate)
        }
        Err(e) => Err(e.into()),
    }
}

/// Who is on this plan, newest first.
pub async fn list_for_plan(
    db: &Database,
    plan: &FeePlanId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<FeePlanAssignment>, i64), AppError> {
    PagedList::new(
        "fee_plan_assignment WHERE plan = $1",
        "ORDER BY created_at DESC, plan DESC, student DESC",
    )
    .bind(plan.uuid())
    .run(limit, offset, db)
    .await
}

/// Is anybody on this plan? A scan, and no longer a guard: the edit and
/// delete guards read the plan's own refcount, which no concurrent assign
/// can be behind. Kept because a *test* asserting the rows and the counter
/// agree is the only thing that would catch the counter drifting.
pub async fn exists_for_plan(db: &Database, plan: &FeePlanId) -> Result<bool, AppError> {
    let (exists,) = sqlx::query!(
        "SELECT EXISTS (SELECT 1 FROM fee_plan_assignment WHERE plan = $1)",
        plan
    )
    .fetch_one(db)
    .await?;
    Ok(exists)
}

/// Which plans a student is on — one student may hold several.
pub async fn list_for_student(
    db: &Database,
    student: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<FeePlanAssignment>, i64), AppError> {
    PagedList::new(
        "fee_plan_assignment WHERE student = $1",
        "ORDER BY created_at DESC, plan DESC, student DESC",
    )
    .bind(student.uuid())
    .run(limit, offset, db)
    .await
}
