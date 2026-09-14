//! The `pool_question_image` table: the reads behind the pool photo
//! endpoints. One row per question at most (`question` is the primary key);
//! the writes ride the question's own guarded transactions in
//! [`crate::db::pool_question`] (`set_image`/`clear_image`/`delete`), where
//! the pending-freeze discipline and the replaced-blob accounting live. The
//! row's pure half lives in [`crate::domain::pool_question_image`]; the blob
//! bytes stay the web layer's.

use crate::database::Database;
use crate::domain::note_file::FileContentType;
use crate::domain::pool_question::PoolQuestionId;
use crate::domain::pool_question_image::PoolQuestionImage;
use crate::error::AppError;

/// The question's photo row, if it carries one — the blob-serving read.
pub async fn read(
    db: &Database,
    question: &PoolQuestionId,
) -> Result<Option<PoolQuestionImage>, AppError> {
    let row = sqlx::query_as!(
        PoolQuestionImage,
        r#"SELECT question AS "question: PoolQuestionId", file,
               content_type AS "content_type: FileContentType", size
           FROM pool_question_image WHERE question = $1"#,
        question.uuid()
    )
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// The photo rows of several questions in one query — for bucketing onto a
/// listing's *page* (one image per question at most, so a page of `n`
/// questions reads at most `n` rows).
pub async fn list_for_questions(
    db: &Database,
    questions: &[&PoolQuestionId],
) -> Result<Vec<PoolQuestionImage>, AppError> {
    if questions.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<uuid::Uuid> = questions.iter().map(|q| q.uuid()).collect();
    let rows = sqlx::query_as!(
        PoolQuestionImage,
        r#"SELECT question AS "question: PoolQuestionId", file,
               content_type AS "content_type: FileContentType", size
           FROM pool_question_image WHERE question = ANY($1)"#,
        &ids
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}
