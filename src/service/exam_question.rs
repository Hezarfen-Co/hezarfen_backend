//! Exam-question authoring rules: the freeze pre-flight every authoring path
//! runs, the question-lookup and option-addressing gates, and the read and
//! write funnels the web layer goes through. The queries and their
//! transactions live in [`crate::db::exam_question`]; the HTTP shaping stays
//! in the web layer.

use crate::database::Database;
use crate::domain::bank_question::BankQuestionId;
use crate::domain::exam::ExamId;
use crate::domain::exam_question::{
    ChoiceId, ExamQuestion, ExamQuestionId, QuestionPoints, QuestionSpec, QuestionText,
};
use crate::domain::subject::SubjectId;
use crate::error::{AppError, ValidationError};

/// The questions freeze once anyone has started an attempt — editing them
/// under a student mid-exam would fork what "the exam" means.
pub async fn ensure_questions_editable(exam: &ExamId, db: &Database) -> Result<(), AppError> {
    if crate::service::exam_attempt::any_for_exam(db, exam).await? {
        return Err(AppError::Conflict(
            "cannot change questions after attempts have started",
        ));
    }
    Ok(())
}

/// The question's option named by `choice_id` — a 400 for a text question or an
/// id the question doesn't have, so an option picture can only ever be
/// addressed through an option that exists.
pub fn choice_slot(question: &ExamQuestion, choice_id: &str) -> Result<ChoiceId, AppError> {
    let Some(choices) = question.get_choices() else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "choice_id",
            reason: "only choice questions take option pictures",
        }));
    };
    choices
        .iter()
        .find(|choice| choice.get_id().as_str() == choice_id)
        .map(|choice| choice.get_id().clone())
        .ok_or(AppError::Validation(ValidationError::Invalid {
            field: "choice_id",
            reason: "must name one of the choices",
        }))
}

/// The question, provided it belongs to `exam` — a qid under someone else's
/// exam is a plain 404, not a leak.
pub async fn question_of_exam(
    exam: &ExamId,
    qid: &str,
    db: &Database,
) -> Result<ExamQuestion, AppError> {
    let question = crate::db::exam_question::read(db, &ExamQuestionId::from_key(qid))
        .await?
        .ok_or(AppError::NotFound)?;
    if question.get_exam() != exam {
        return Err(AppError::NotFound);
    }
    Ok(question)
}

/// Add a question to an exam. The freeze gate and the subject's reference
/// counter ride in the write's own transaction.
pub async fn create(
    db: &Database,
    exam: &ExamId,
    subject: SubjectId,
    text: QuestionText,
    points: QuestionPoints,
    spec: QuestionSpec,
) -> Result<ExamQuestion, AppError> {
    crate::db::exam_question::create(db, exam, subject, text, points, spec).await
}

/// Like [`create`], but records the bank template the question was
/// instantiated from.
pub async fn create_from_bank(
    db: &Database,
    exam: &ExamId,
    subject: SubjectId,
    text: QuestionText,
    points: QuestionPoints,
    spec: QuestionSpec,
    source: BankQuestionId,
) -> Result<ExamQuestion, AppError> {
    crate::db::exam_question::create_from_bank(db, exam, subject, text, points, spec, source).await
}

/// The exam's questions in presentation order — the web layer's paging read.
pub async fn list_for_exam(
    db: &Database,
    exam: &ExamId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<ExamQuestion>, i64), AppError> {
    crate::db::exam_question::list_for_exam(db, exam, limit, offset).await
}

/// The question ids of `exam` sharing a bank template with a question under
/// one of `live` — the review hiding key.
pub async fn list_shared_with(
    db: &Database,
    exam: &ExamId,
    live: &[ExamId],
) -> Result<std::collections::HashSet<String>, AppError> {
    crate::db::exam_question::list_shared_with(db, exam, live).await
}

/// Write the editable fields; a stale read is a 409, nothing was written.
pub async fn update(
    db: &Database,
    question: ExamQuestion,
    subject: SubjectId,
    text: QuestionText,
    points: QuestionPoints,
    spec: QuestionSpec,
) -> Result<ExamQuestion, AppError> {
    crate::db::exam_question::update(db, question, subject, text, points, spec).await
}

/// Delete the question and cascade its answers and image rows.
pub async fn delete(db: &Database, question: ExamQuestion) -> Result<ExamQuestion, AppError> {
    crate::db::exam_question::delete(db, question).await
}

/// Point the question's `banked_as` at the bank template it was saved into.
pub async fn link_banked_as(
    db: &Database,
    question: ExamQuestion,
    template: BankQuestionId,
) -> Result<ExamQuestion, AppError> {
    crate::db::exam_question::link_banked_as(db, question, template).await
}
