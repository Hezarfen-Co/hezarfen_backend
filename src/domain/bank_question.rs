//! A reusable question template in the school-wide question bank. Unlike an
//! [`crate::domain::exam_question::ExamQuestion`], a bank row is fully detached
//! from any exam: no exam FK, so no attempt ever freezes it, and its images
//! live in their own [`crate::domain::bank_question_image`] slot table. Teachers
//! save templates here and later *copy* them into an exam — the copy is a fresh
//! `ExamQuestion` with its own id, images, and answers; the two never share a
//! row. The kind-dependent columns satisfy the [`QuestionSpec`] invariants
//! because every write goes through one.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::database::{BANK_QUESTION_TABLE, Database};
use crate::domain::exam::ExamId;
use crate::domain::exam_question::{
    ChoiceText, QuestionKind, QuestionPoints, QuestionSpec, QuestionText,
};
use crate::domain::subject::SubjectId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct BankQuestionId(RecordId);

impl BankQuestionId {
    pub fn generate() -> Self {
        Self(RecordId::new(BANK_QUESTION_TABLE, Ulid::new().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(BANK_QUESTION_TABLE, key))
    }

    pub fn record(&self) -> RecordId {
        self.0.clone()
    }

    pub fn key(&self) -> &str {
        match &self.0.key {
            RecordIdKey::String(key) => key,
            _ => "",
        }
    }
}

/// One template in the shared question bank. `owner` is the teacher who saved
/// it (the only one who may edit or delete it, admins aside); `subject` is
/// origin metadata only — the same-course rule is checked against the caller's
/// subject when a template is copied into an exam, not here.
#[derive(Debug, Clone, SurrealValue)]
pub struct BankQuestion {
    id: BankQuestionId,
    owner: UserId,
    subject: SubjectId,
    text: QuestionText,
    kind: QuestionKind,
    points: QuestionPoints,
    choices: Option<Vec<ChoiceText>>,
    correct: Option<i64>,
    /// The exam question this template was saved from, if any.
    source_exam: Option<ExamId>,
    created_at: Timestamp,
}

impl BankQuestion {
    pub fn get_id(&self) -> &BankQuestionId {
        &self.id
    }

    pub fn get_owner(&self) -> &UserId {
        &self.owner
    }

    pub fn get_subject(&self) -> &SubjectId {
        &self.subject
    }

    pub fn get_text(&self) -> &QuestionText {
        &self.text
    }

    pub fn get_kind(&self) -> &QuestionKind {
        &self.kind
    }

    pub fn get_points(&self) -> QuestionPoints {
        self.points
    }

    pub fn get_choices(&self) -> Option<&[ChoiceText]> {
        self.choices.as_deref()
    }

    pub fn get_correct(&self) -> Option<i64> {
        self.correct
    }

    pub fn get_source_exam(&self) -> Option<&ExamId> {
        self.source_exam.as_ref()
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }

    /// The stored kind-dependent fields as the validated bundle (for
    /// merge-on-update). Bypasses `try_new`: the fields were written through a
    /// `QuestionSpec`, so the invariants already hold.
    pub fn spec(&self) -> QuestionSpec {
        // `QuestionSpec` fields are private; rebuild through `try_new`, which
        // the stored values are guaranteed to satisfy.
        QuestionSpec::try_new(
            self.kind.clone(),
            self.choices
                .as_ref()
                .map(|cs| cs.iter().map(|c| c.as_str().to_string()).collect()),
            self.correct,
        )
        .expect("stored bank question fields satisfy the spec invariants")
    }

    pub async fn create(
        owner: UserId,
        subject: SubjectId,
        text: QuestionText,
        points: QuestionPoints,
        spec: QuestionSpec,
        db: &Database,
    ) -> Result<BankQuestion, AppError> {
        Self::insert(owner, subject, text, points, spec, None, db).await
    }

    /// Like [`Self::create`], but records the origin exam the template was
    /// saved from (`POST …/questions/{qid}/to-bank`).
    pub async fn create_from_exam(
        owner: UserId,
        subject: SubjectId,
        text: QuestionText,
        points: QuestionPoints,
        spec: QuestionSpec,
        source: ExamId,
        db: &Database,
    ) -> Result<BankQuestion, AppError> {
        Self::insert(owner, subject, text, points, spec, Some(source), db).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert(
        owner: UserId,
        subject: SubjectId,
        text: QuestionText,
        points: QuestionPoints,
        spec: QuestionSpec,
        source_exam: Option<ExamId>,
        db: &Database,
    ) -> Result<BankQuestion, AppError> {
        let question = BankQuestion::from_parts(
            BankQuestionId::generate(),
            owner,
            subject,
            text,
            points,
            spec,
            source_exam,
            Timestamp::now(),
        );
        let created: Option<BankQuestion> =
            db.create(question.id.record()).content(question).await?;
        created.ok_or_else(|| AppError::Internal("failed to create bank question".into()))
    }

    /// Whether any bank template references `subject` as its origin — the gate
    /// that blocks deleting a subject still used by a template.
    pub async fn any_for_subject(subject: &SubjectId, db: &Database) -> Result<bool, AppError> {
        let mut result = db
            .query("SELECT VALUE id FROM bank_question WHERE subject = $subject LIMIT 1")
            .bind(("subject", subject.record()))
            .await?
            .check()?;
        Ok(!result.take::<Vec<RecordId>>(0)?.is_empty())
    }

    pub async fn read(id: &BankQuestionId, db: &Database) -> Result<Option<BankQuestion>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// Every template in the bank, school-wide (no owner filter), in creation
    /// order (ULID ids sort by creation).
    pub async fn list(db: &Database) -> Result<Vec<BankQuestion>, AppError> {
        let mut result = db
            .query("SELECT * FROM bank_question ORDER BY id ASC")
            .await?
            .check()?;
        Ok(result.take::<Vec<BankQuestion>>(0)?)
    }

    pub async fn update(
        mut self,
        subject: SubjectId,
        text: QuestionText,
        points: QuestionPoints,
        spec: QuestionSpec,
        db: &Database,
    ) -> Result<BankQuestion, AppError> {
        self.subject = subject;
        self.text = text;
        self.points = points;
        let (kind, choices, correct) = spec.into_parts();
        self.kind = kind;
        self.choices = choices;
        self.correct = correct;
        let updated: Option<BankQuestion> = db.update(self.id.record()).content(self).await?;
        updated.ok_or(AppError::NotFound)
    }

    /// Delete the template and cascade-remove its bank images, so none points
    /// at a missing template. Bank rows have no answers. The image *blobs* are
    /// the web layer's to remove — it collects their names before calling this.
    ///
    /// Exam questions saved from this template keep living: only their
    /// `source_bank` provenance link is cleared, field-scoped (never a whole-row
    /// save — the question isn't ours and may be edited concurrently), so the
    /// bank page can't read a link to a template that no longer exists.
    pub async fn delete(self, db: &Database) -> Result<BankQuestion, AppError> {
        db.query(
            "DELETE bank_question_image WHERE bank_question = $b;
             UPDATE exam_question SET source_bank = NONE WHERE source_bank = $b;",
        )
        .bind(("b", self.id.record()))
        .await?
        .check()?;
        let deleted: Option<BankQuestion> = db.delete(self.id.record()).await?;
        deleted.ok_or(AppError::NotFound)
    }

    #[allow(clippy::too_many_arguments)]
    fn from_parts(
        id: BankQuestionId,
        owner: UserId,
        subject: SubjectId,
        text: QuestionText,
        points: QuestionPoints,
        spec: QuestionSpec,
        source_exam: Option<ExamId>,
        created_at: Timestamp,
    ) -> BankQuestion {
        let (kind, choices, correct) = spec.into_parts();
        BankQuestion {
            id,
            owner,
            subject,
            text,
            kind,
            points,
            choices,
            correct,
            source_exam,
            created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> QuestionSpec {
        QuestionSpec::try_new(
            QuestionKind::try_new("choice").unwrap(),
            Some(vec!["yes".into(), "no".into()]),
            Some(1),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn list_is_school_wide() {
        let db = crate::database::init_mem().await.unwrap();
        for _ in 0..2 {
            BankQuestion::create(
                UserId::generate(),
                SubjectId::generate(),
                QuestionText::try_new("q").unwrap(),
                QuestionPoints::try_new(1).unwrap(),
                spec(),
                &db,
            )
            .await
            .unwrap();
        }
        // Different owners, yet both listed.
        assert_eq!(BankQuestion::list(&db).await.unwrap().len(), 2);
    }
}
