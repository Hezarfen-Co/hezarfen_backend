//! Bank-question image funnels: the per-slot upsert, the slot/page reads, the
//! choice sweep a non-destructive PATCH runs, and the single-row delete. The
//! queries and their transactions live in
//! [`crate::db::bank_question_image`]; the blob files themselves stay the web
//! layer's to write and unlink.

use crate::database::Database;
use crate::db::bank_question_image;
use crate::domain::bank_question::BankQuestionId;
use crate::domain::bank_question_image::BankQuestionImage;
use crate::domain::exam_question::ChoiceId;
use crate::error::AppError;

/// Create or replace the slot's image row, handing back what it stored plus
/// the blob name it replaced, for the caller to take off disk.
pub async fn upsert(
    db: &Database,
    image: BankQuestionImage,
) -> Result<(BankQuestionImage, Option<String>), AppError> {
    bank_question_image::upsert(db, image).await
}

pub async fn read_slot(
    db: &Database,
    question: &BankQuestionId,
    slot: Option<&ChoiceId>,
) -> Result<Option<BankQuestionImage>, AppError> {
    bank_question_image::read_slot(db, question, slot).await
}

pub async fn list_for_question(
    db: &Database,
    question: &BankQuestionId,
) -> Result<Vec<BankQuestionImage>, AppError> {
    bank_question_image::list_for_question(db, question).await
}

/// The image rows of several templates in one query — for bucketing onto a
/// listing's *page*.
pub async fn list_for_questions(
    db: &Database,
    questions: &[&BankQuestionId],
) -> Result<Vec<BankQuestionImage>, AppError> {
    bank_question_image::list_for_questions(db, questions).await
}

/// Drop the option pictures whose choice is gone — every choice image of
/// the question whose `slot` is *not* in `keep` — returning the removed rows
/// so the caller can take their blobs off disk.
pub async fn delete_choices_not_in(
    db: &Database,
    question: &BankQuestionId,
    keep: &[ChoiceId],
) -> Result<Vec<BankQuestionImage>, AppError> {
    bank_question_image::delete_choices_not_in(db, question, keep).await
}

pub async fn delete(
    db: &Database,
    image: BankQuestionImage,
) -> Result<BankQuestionImage, AppError> {
    bank_question_image::delete(db, image).await
}
