//! Solution-photo funnels: the read doors the web layer takes for the blob
//! endpoints and the page bucketing. The writes do not pass through here —
//! they are the solution's own transactions in
//! [`crate::service::solution`] (`set_image`/`clear_image`/`delete`).

use crate::database::Database;
use crate::db;
use crate::domain::solution::SolutionId;
use crate::domain::solution_image::SolutionImage;
use crate::error::AppError;

/// The solution's photo row, if any.
pub async fn read(db: &Database, solution: &SolutionId) -> Result<Option<SolutionImage>, AppError> {
    db::solution_image::read(db, solution).await
}

/// The photo rows of a page of solutions, in one query.
pub async fn list_for_solutions(
    db: &Database,
    solutions: &[&SolutionId],
) -> Result<Vec<SolutionImage>, AppError> {
    db::solution_image::list_for_solutions(db, solutions).await
}
