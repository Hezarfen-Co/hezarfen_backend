//! Bank-question workflows: the PATCH re-derive — the merge (set / clear /
//! keep per field) re-judged against a fresh read every retry round, with the
//! kind/choices/correct trio re-validated as a unit — plus the read, list,
//! create, usage-tally, and delete funnels the web layer goes through. The
//! queries and their transactions live in [`crate::db::bank_question`]; the
//! pure entity and newtypes in [`crate::domain::bank_question`].

use std::collections::HashMap;

use crate::constant::CAS_UPDATE_RETRIES;
use crate::database::Database;
use crate::db::bank_question;
use crate::domain::bank_question::{BankQuestion, BankQuestionId, BankVisibility};
use crate::domain::bank_question_image::BankQuestionImage;
use crate::domain::exam::ExamId;
use crate::domain::exam_question::{
    Choice, ChoiceInput, QuestionKind, QuestionPoints, QuestionSpec, QuestionText,
};
use crate::domain::subject::SubjectId;
use crate::domain::user::UserId;
use crate::error::AppError;

pub async fn read(db: &Database, id: &BankQuestionId) -> Result<Option<BankQuestion>, AppError> {
    bank_question::read(db, id).await
}

pub async fn create(
    db: &Database,
    owner: UserId,
    subject: SubjectId,
    text: QuestionText,
    points: QuestionPoints,
    spec: QuestionSpec,
) -> Result<BankQuestion, AppError> {
    bank_question::create(db, owner, subject, text, points, spec).await
}

/// Like [`create`], but records the origin exam the template was saved from
/// (`POST …/questions/{qid}/to-bank`).
pub async fn create_from_exam(
    db: &Database,
    owner: UserId,
    subject: SubjectId,
    text: QuestionText,
    points: QuestionPoints,
    spec: QuestionSpec,
    source: ExamId,
) -> Result<BankQuestion, AppError> {
    bank_question::create_from_exam(db, owner, subject, text, points, spec, source).await
}

/// One page of the bank the caller may see, plus the total under the same
/// filters — the visibility gate is a WHERE clause inside, never a post-filter.
#[allow(clippy::too_many_arguments)]
pub async fn list(
    db: &Database,
    visible_to: Option<&UserId>,
    owner: Option<&UserId>,
    subject: Option<&SubjectId>,
    visibility: Option<&BankVisibility>,
    q: Option<&str>,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<BankQuestion>, i64), AppError> {
    bank_question::list(db, visible_to, owner, subject, visibility, q, limit, offset).await
}

/// How many exam questions were instantiated from each of `ids`, in one
/// grouped query for the whole page.
pub async fn usage_counts(
    db: &Database,
    ids: &[&BankQuestionId],
) -> Result<HashMap<String, i64>, AppError> {
    bank_question::usage_counts(db, ids).await
}

/// A `PATCH /bank-questions/{bid}` request after field validation: every
/// value the request *sets* is already validated (text, points, and kind by
/// their newtypes; the submitted choices as `ChoiceInput`s; the subject by
/// the web's existence read). What is *kept* — and whether the merged
/// kind/choices/correct trio is a legal question — are the update's own
/// decisions, re-derived against a fresh read every retry round, so they live
/// here and not in the web layer. `choices`/`correct` carry the web DTO's
/// set-or-clear spelling (`Some(None)` = explicit `null`).
pub struct BankQuestionPatch {
    pub subject: Option<SubjectId>,
    pub text: Option<QuestionText>,
    pub points: Option<QuestionPoints>,
    pub kind: Option<QuestionKind>,
    pub choices: Option<Option<Vec<ChoiceInput>>>,
    pub correct: Option<Option<String>>,
    pub visibility: Option<BankVisibility>,
}

/// Re-derive and save the template: merge the patch over a fresh read,
/// re-validate the kind-dependent fields as a unit, and land the whole merge
/// with the compare-and-set — retried while the row keeps moving under the
/// snapshot. Concurrent PATCHes of one template no longer queue behind each
/// other: the lost update they used to cause is refused by the guard, and the
/// next round re-merges over what actually landed.
pub async fn update(
    db: &Database,
    id: &BankQuestionId,
    patch: &BankQuestionPatch,
) -> Result<BankQuestion, AppError> {
    let mut left = CAS_UPDATE_RETRIES;
    loop {
        let question = bank_question::read(db, id)
            .await?
            .ok_or(AppError::NotFound)?;
        // Omitted keeps the stored subject — which may already be `None`, cleared
        // by that subject's delete.
        let subject = match &patch.subject {
            Some(subject) => Some(*subject),
            None => question.get_subject().cloned(),
        };
        let text = patch
            .text
            .clone()
            .unwrap_or_else(|| question.get_text().clone());
        let points = patch.points.unwrap_or_else(|| question.get_points());
        let kind = patch
            .kind
            .clone()
            .unwrap_or_else(|| question.get_kind().clone());
        // Merge the kind-dependent fields (set / clear / keep per field), then
        // re-validate them as a unit — a PATCH can't leave a half-question behind.
        // Omitting `choices` re-submits the stored options *with their ids*, so a
        // text-only edit keeps every identity (and every picture) untouched.
        let stored: Vec<Choice> = question.get_choices().unwrap_or_default().to_vec();
        let choices = match &patch.choices {
            Some(update) => update.clone(),
            None => question.get_choices().map(|stored| {
                stored
                    .iter()
                    .map(|choice| ChoiceInput {
                        id: Some(choice.get_id().as_str().to_string()),
                        text: choice.get_text().as_str().to_string(),
                    })
                    .collect()
            }),
        };
        let correct = patch
            .correct
            .clone()
            .unwrap_or_else(|| question.get_correct().map(|id| id.as_str().to_string()));
        let spec = QuestionSpec::try_new(kind, choices, correct, &stored)?;
        let visibility = patch
            .visibility
            .clone()
            .unwrap_or_else(|| question.get_visibility().clone());

        if let Some(updated) = bank_question::update_if_unchanged(
            db, question, subject, text, points, spec, visibility,
        )
        .await?
        {
            return Ok(updated);
        }
        // The row moved under the snapshot every gate above judged: re-read and
        // re-merge, so both edits land instead of the later reverting the earlier.
        left -= 1;
        if left == 0 {
            return Err(AppError::Conflict(
                "the template kept changing underneath this update — try again",
            ));
        }
    }
}

/// Delete the template and cascade-remove its image rows; the swept rows come
/// back with it, and their blob files are the web layer's to unlink.
pub async fn delete(
    db: &Database,
    target: BankQuestion,
) -> Result<(BankQuestion, Vec<BankQuestionImage>), AppError> {
    bank_question::delete(db, target).await
}
