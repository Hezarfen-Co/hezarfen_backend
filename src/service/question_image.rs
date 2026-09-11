//! The question-image workflows: the doors the web layer takes for every
//! question/choice image read and write. The blob I/O and its disk ordering
//! stay in the web layer; the row transactions (behind the question-freeze
//! gate) live in [`crate::db::question_image`].

use crate::database::Database;
use crate::db;
use crate::domain::exam::ExamId;
use crate::domain::exam_question::{ChoiceId, ExamQuestionId};
use crate::domain::question_image::QuestionImage;
use crate::error::AppError;

/// Create or replace the slot's image row, handing back what it stored plus
/// the blob name it replaced, for the caller to take off disk.
pub async fn upsert(
    db: &Database,
    image: QuestionImage,
) -> Result<(QuestionImage, Option<String>), AppError> {
    db::question_image::upsert(db, image).await
}

/// The slot's image row, if any (`slot = None` is the question's
/// illustration).
pub async fn read_slot(
    db: &Database,
    question: &ExamQuestionId,
    slot: Option<&ChoiceId>,
) -> Result<Option<QuestionImage>, AppError> {
    db::question_image::read_slot(db, question, slot).await
}

/// Every image of the exam's questions — one query for the list views.
pub async fn list_for_exam(db: &Database, exam: &ExamId) -> Result<Vec<QuestionImage>, AppError> {
    db::question_image::list_for_exam(db, exam).await
}

/// Every image of one question — the illustration plus its option pictures.
pub async fn list_for_question(
    db: &Database,
    question: &ExamQuestionId,
) -> Result<Vec<QuestionImage>, AppError> {
    db::question_image::list_for_question(db, question).await
}

/// Drop the option pictures whose choice is gone, returning the removed rows
/// so the caller can take their blobs off disk.
pub async fn delete_choices_not_in(
    db: &Database,
    question: &ExamQuestionId,
    keep: &[ChoiceId],
) -> Result<Vec<QuestionImage>, AppError> {
    db::question_image::delete_choices_not_in(db, question, keep).await
}

/// Drop one image row — the returned row names the blob to unlink.
pub async fn delete(db: &Database, image: QuestionImage) -> Result<QuestionImage, AppError> {
    db::question_image::delete(db, image).await
}
