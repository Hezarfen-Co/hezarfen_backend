//! Pool-question-photo funnels: the read doors the web layer takes for the
//! blob endpoints and the page bucketing. The writes do not pass through
//! here — they are the question's own guarded transactions in
//! [`crate::service::pool_question`] (`set_image`/`clear_image`/`delete`).

use crate::database::Database;
use crate::db;
use crate::domain::pool_question::PoolQuestionId;
use crate::domain::pool_question_image::PoolQuestionImage;
use crate::error::AppError;

/// The question's photo row, if any.
pub async fn read(
    db: &Database,
    question: &PoolQuestionId,
) -> Result<Option<PoolQuestionImage>, AppError> {
    db::pool_question_image::read(db, question).await
}

/// The photo rows of a page of questions, in one query.
pub async fn list_for_questions(
    db: &Database,
    questions: &[&PoolQuestionId],
) -> Result<Vec<PoolQuestionImage>, AppError> {
    db::pool_question_image::list_for_questions(db, questions).await
}
