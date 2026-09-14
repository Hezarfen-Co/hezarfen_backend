//! The `solution_image` table: the reads behind the solution photo
//! endpoints. One row per solution at most (`solution` is the primary key);
//! the writes ride the solution's own transactions in
//! [`crate::db::solution`] (`set_image`/`clear_image`/`delete`), where the
//! replaced-blob accounting lives. The row's pure half lives in
//! [`crate::domain::solution_image`]; the blob bytes stay the web layer's.

use crate::database::Database;
use crate::domain::note_file::FileContentType;
use crate::domain::solution::SolutionId;
use crate::domain::solution_image::SolutionImage;
use crate::error::AppError;

/// The solution's photo row, if it carries one — the blob-serving read.
pub async fn read(db: &Database, solution: &SolutionId) -> Result<Option<SolutionImage>, AppError> {
    let row = sqlx::query_as!(
        SolutionImage,
        r#"SELECT solution AS "solution: SolutionId", file,
               content_type AS "content_type: FileContentType", size
           FROM solution_image WHERE solution = $1"#,
        solution.uuid()
    )
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// The photo rows of several solutions in one query — for bucketing onto a
/// listing's *page* (one image per solution at most, so a page of `n`
/// solutions reads at most `n` rows).
pub async fn list_for_solutions(
    db: &Database,
    solutions: &[&SolutionId],
) -> Result<Vec<SolutionImage>, AppError> {
    if solutions.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<uuid::Uuid> = solutions.iter().map(|s| s.uuid()).collect();
    let rows = sqlx::query_as!(
        SolutionImage,
        r#"SELECT solution AS "solution: SolutionId", file,
               content_type AS "content_type: FileContentType", size
           FROM solution_image WHERE solution = ANY($1)"#,
        &ids
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}
