//! The sitting answer sheet's workflows: the doors the web layer takes for
//! every answer read and write (the gate chain itself lives in
//! [`crate::service::exam_attempt`], which funnels every save through
//! [`save`]). The queries live in [`crate::db::exam_answer`].

use crate::database::Database;
use crate::db;
use crate::domain::exam::ExamId;
use crate::domain::exam_answer::ExamAnswer;
use crate::domain::exam_question::{ExamQuestion, ExamQuestionId};
use crate::domain::user::UserId;
use crate::error::AppError;

/// Save (or overwrite) one answer — the payload validation and the atomic
/// exam-row-touching upsert. The caller has already checked that the attempt
/// is in progress.
pub async fn save(
    db: &Database,
    question: &ExamQuestion,
    user: &UserId,
    seq: i64,
    selected: Option<String>,
    text: Option<String>,
) -> Result<ExamAnswer, AppError> {
    db::exam_answer::save(db, question, user, seq, selected, text).await
}

/// One student's stored answer for a question in sitting `seq`, if any.
pub async fn read(
    db: &Database,
    question: &ExamQuestionId,
    user: &UserId,
    seq: i64,
) -> Result<Option<ExamAnswer>, AppError> {
    db::exam_answer::read(db, question, user, seq).await
}

/// Drop one student's answer to a single question in sitting `seq`.
pub async fn delete(
    db: &Database,
    question: &ExamQuestionId,
    user: &UserId,
    seq: i64,
) -> Result<(), AppError> {
    db::exam_answer::delete(db, question, user, seq).await
}

/// One student's answers for a single sitting (`seq`) across an exam.
pub async fn list_for_exam_user(
    db: &Database,
    exam: &ExamId,
    user: &UserId,
    seq: i64,
) -> Result<Vec<ExamAnswer>, AppError> {
    db::exam_answer::list_for_exam_user(db, exam, user, seq).await
}

/// Every answer of an exam — the live monitor aggregates these per student.
pub async fn list_for_exam(db: &Database, exam: &ExamId) -> Result<Vec<ExamAnswer>, AppError> {
    db::exam_answer::list_for_exam(db, exam).await
}

/// The distinct sittings a student has any answer for at `exam`, ascending
/// — the index a history view lists attempts from.
pub async fn list_seqs_for_user(
    db: &Database,
    exam: &ExamId,
    user: &UserId,
) -> Result<Vec<i64>, AppError> {
    db::exam_answer::list_seqs_for_user(db, exam, user).await
}
