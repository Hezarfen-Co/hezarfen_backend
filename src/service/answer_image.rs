//! The drawn-answer workflows: the doors the web layer takes for every
//! answer-image read and write. The blob I/O and its disk ordering stay in
//! the web layer; the row transactions live in [`crate::db::answer_image`].

use crate::database::Database;
use crate::db;
use crate::domain::answer_image::AnswerImage;
use crate::domain::exam::ExamId;
use crate::domain::exam_question::ExamQuestionId;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Create or replace the student's drawing for the question, handing back
/// what it stored plus the blob name it replaced, for the caller to take
/// off disk.
pub async fn upsert(
    db: &Database,
    image: AnswerImage,
) -> Result<(AnswerImage, Option<String>), AppError> {
    db::answer_image::upsert(db, image).await
}

/// The student's drawing for one question in one sitting, if any.
pub async fn read(
    db: &Database,
    question: &ExamQuestionId,
    user: &UserId,
    seq: i64,
) -> Result<Option<AnswerImage>, AppError> {
    db::answer_image::read(db, question, user, seq).await
}

/// Drop one drawing — the returned row names the blob to unlink.
pub async fn delete(db: &Database, image: AnswerImage) -> Result<AnswerImage, AppError> {
    db::answer_image::delete(db, image).await
}

/// One student's answer drawings for a single sitting.
pub async fn list_for_exam_user(
    db: &Database,
    exam: &ExamId,
    user: &UserId,
    seq: i64,
) -> Result<Vec<AnswerImage>, AppError> {
    db::answer_image::list_for_exam_user(db, exam, user, seq).await
}
