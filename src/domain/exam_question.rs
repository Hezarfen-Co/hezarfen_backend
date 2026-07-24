use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::{
    MAX_CHOICE_TEXT_LEN, MAX_QUESTION_CHOICES, MAX_QUESTION_TEXT_LEN, MIN_QUESTION_CHOICES,
};
use crate::database::{Database, EXAM_QUESTION_TABLE};
use crate::domain::bank_question::BankQuestionId;
use crate::domain::exam::ExamId;
use crate::domain::subject::SubjectId;
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_question_kind, validate_question_points, validate_required};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ExamQuestionId(RecordId);

impl ExamQuestionId {
    pub fn generate() -> Self {
        Self(RecordId::new(EXAM_QUESTION_TABLE, Ulid::new().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(EXAM_QUESTION_TABLE, key))
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

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct QuestionText(String);

impl QuestionText {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("text", value, MAX_QUESTION_TEXT_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A validated question kind: `choice` (pick one option, auto-scorable) or
/// `text` (free text, judged by the grader).
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct QuestionKind(String);

impl QuestionKind {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_question_kind(value)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A validated question weight in the auto-score, held to
/// `[MIN_QUESTION_POINTS, MAX_QUESTION_POINTS]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, SurrealValue)]
pub struct QuestionPoints(i64);

impl QuestionPoints {
    pub fn try_new(value: i64) -> Result<Self, ValidationError> {
        validate_question_points(value)?;
        Ok(Self(value))
    }

    pub fn as_i64(&self) -> i64 {
        self.0
    }
}

/// One option of a choice question: non-blank, at most `MAX_CHOICE_TEXT_LEN`
/// characters.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ChoiceText(String);

impl ChoiceText {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("choice", value, MAX_CHOICE_TEXT_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The kind-dependent fields of a question, validated as a unit — they only
/// make sense together. `try_new` is the sole constructor, so a `QuestionSpec`
/// in hand always satisfies:
///
/// - `text` → no `choices`, no `correct` (the grader judges the answer),
/// - `choice` → 2–10 `choices` plus a `correct` index into them.
#[derive(Debug, Clone)]
pub struct QuestionSpec {
    kind: QuestionKind,
    choices: Option<Vec<ChoiceText>>,
    correct: Option<i64>,
}

impl QuestionSpec {
    pub fn try_new(
        kind: QuestionKind,
        choices: Option<Vec<String>>,
        correct: Option<i64>,
    ) -> Result<Self, ValidationError> {
        let invalid = |field, reason| ValidationError::Invalid { field, reason };
        let choices = match (kind.as_str(), choices) {
            ("text", Some(_)) => {
                return Err(invalid("choices", "only choice questions take choices"));
            }
            ("text", None) => {
                if correct.is_some() {
                    return Err(invalid(
                        "correct",
                        "only choice questions take a correct index",
                    ));
                }
                None
            }
            (_, None) => return Err(invalid("choices", "required for a choice question")),
            (_, Some(choices)) => {
                if !(MIN_QUESTION_CHOICES..=MAX_QUESTION_CHOICES).contains(&choices.len()) {
                    return Err(invalid("choices", "must list 2 to 10 choices"));
                }
                let Some(correct) = correct else {
                    return Err(invalid("correct", "required for a choice question"));
                };
                if !(0..choices.len() as i64).contains(&correct) {
                    return Err(invalid("correct", "must index one of the choices"));
                }
                Some(
                    choices
                        .iter()
                        .map(|choice| ChoiceText::try_new(choice))
                        .collect::<Result<Vec<_>, _>>()?,
                )
            }
        };
        Ok(Self {
            kind,
            choices,
            correct,
        })
    }

    pub fn get_kind(&self) -> &QuestionKind {
        &self.kind
    }

    /// The validated fields, consumed — for a writer in another module (the
    /// question bank) that stores the same three columns.
    pub fn into_parts(self) -> (QuestionKind, Option<Vec<ChoiceText>>, Option<i64>) {
        (self.kind, self.choices, self.correct)
    }
}

/// One question of an exam. Order within the exam is the id's ULID order
/// (creation order); the kind-dependent columns always satisfy the
/// [`QuestionSpec`] invariants because every write goes through one. Every
/// question links to a subject of the exam's course — the handlers verify the
/// subject's course matches before any write.
#[derive(Debug, Clone, SurrealValue)]
pub struct ExamQuestion {
    id: ExamQuestionId,
    exam: ExamId,
    subject: SubjectId,
    text: QuestionText,
    kind: QuestionKind,
    points: QuestionPoints,
    choices: Option<Vec<ChoiceText>>,
    correct: Option<i64>,
    /// The bank template this question was instantiated from, if any.
    source_bank: Option<BankQuestionId>,
}

impl ExamQuestion {
    pub fn get_id(&self) -> &ExamQuestionId {
        &self.id
    }

    pub fn get_exam(&self) -> &ExamId {
        &self.exam
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

    pub fn get_source_bank(&self) -> Option<&BankQuestionId> {
        self.source_bank.as_ref()
    }

    /// The stored kind-dependent fields as the validated bundle (for
    /// merge-on-update). Bypasses `try_new`: the fields were written through a
    /// `QuestionSpec`, so the invariants already hold.
    pub fn spec(&self) -> QuestionSpec {
        QuestionSpec {
            kind: self.kind.clone(),
            choices: self.choices.clone(),
            correct: self.correct,
        }
    }

    pub async fn create(
        exam: &ExamId,
        subject: SubjectId,
        text: QuestionText,
        points: QuestionPoints,
        spec: QuestionSpec,
        db: &Database,
    ) -> Result<ExamQuestion, AppError> {
        Self::insert(exam, subject, text, points, spec, None, db).await
    }

    /// Like [`Self::create`], but records the bank template this question was
    /// instantiated from (`POST …/questions/from-bank/{bid}`).
    pub async fn create_from_bank(
        exam: &ExamId,
        subject: SubjectId,
        text: QuestionText,
        points: QuestionPoints,
        spec: QuestionSpec,
        source: BankQuestionId,
        db: &Database,
    ) -> Result<ExamQuestion, AppError> {
        Self::insert(exam, subject, text, points, spec, Some(source), db).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert(
        exam: &ExamId,
        subject: SubjectId,
        text: QuestionText,
        points: QuestionPoints,
        spec: QuestionSpec,
        source_bank: Option<BankQuestionId>,
        db: &Database,
    ) -> Result<ExamQuestion, AppError> {
        let question = ExamQuestion {
            id: ExamQuestionId::generate(),
            exam: exam.clone(),
            subject,
            text,
            points,
            kind: spec.kind,
            choices: spec.choices,
            correct: spec.correct,
            source_bank,
        };
        let created: Option<ExamQuestion> =
            db.create(question.id.record()).content(question).await?;
        created.ok_or_else(|| AppError::Internal("failed to create exam question".into()))
    }

    pub async fn read(
        id: &ExamQuestionId,
        db: &Database,
    ) -> Result<Option<ExamQuestion>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// The exam's questions in presentation order (ULID ids sort by creation).
    pub async fn list_for_exam(
        exam: &ExamId,
        db: &Database,
    ) -> Result<Vec<ExamQuestion>, AppError> {
        let mut result = db
            .query("SELECT * FROM exam_question WHERE exam = $ex ORDER BY id ASC")
            .bind(("ex", exam.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<ExamQuestion>>(0)?)
    }

    /// Whether any question anywhere references `subject` — the gate that
    /// blocks deleting a subject still in use.
    pub async fn any_for_subject(subject: &SubjectId, db: &Database) -> Result<bool, AppError> {
        let mut result = db
            .query("SELECT VALUE id FROM exam_question WHERE subject = $subject LIMIT 1")
            .bind(("subject", subject.record()))
            .await?
            .check()?;
        Ok(!result.take::<Vec<RecordId>>(0)?.is_empty())
    }

    pub async fn update(
        mut self,
        subject: SubjectId,
        text: QuestionText,
        points: QuestionPoints,
        spec: QuestionSpec,
        db: &Database,
    ) -> Result<ExamQuestion, AppError> {
        self.subject = subject;
        self.text = text;
        self.points = points;
        self.kind = spec.kind;
        self.choices = spec.choices;
        self.correct = spec.correct;
        let updated: Option<ExamQuestion> = db.update(self.id.record()).content(self).await?;
        updated.ok_or(AppError::NotFound)
    }

    /// Delete the question and cascade-remove its answers and image rows, so
    /// neither can point at a missing question. The image *blobs* are the web
    /// layer's to remove — it collects their names before calling this.
    pub async fn delete(self, db: &Database) -> Result<ExamQuestion, AppError> {
        db.query(
            "DELETE exam_answer WHERE question = $q;
             DELETE question_image WHERE question = $q;",
        )
        .bind(("q", self.id.record()))
        .await?
        .check()?;
        let deleted: Option<ExamQuestion> = db.delete(self.id.record()).await?;
        deleted.ok_or(AppError::NotFound)
    }

    /// A question constructed without a database, for pure-function tests.
    #[cfg(test)]
    pub fn test_new(exam: &ExamId, text: &str, points: i64, spec: QuestionSpec) -> ExamQuestion {
        ExamQuestion {
            id: ExamQuestionId::generate(),
            exam: exam.clone(),
            subject: SubjectId::generate(),
            text: QuestionText::try_new(text).unwrap(),
            points: QuestionPoints::try_new(points).unwrap(),
            kind: spec.kind,
            choices: spec.choices,
            correct: spec.correct,
            source_bank: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(value: &str) -> QuestionKind {
        QuestionKind::try_new(value).unwrap()
    }

    fn two_choices() -> Option<Vec<String>> {
        Some(vec!["yes".into(), "no".into()])
    }

    #[tokio::test]
    async fn text_is_required() {
        assert!(QuestionText::try_new("What is 2 + 2?").is_ok());
        assert!(QuestionText::try_new("").is_err());
        assert!(QuestionText::try_new("   ").is_err());
        assert!(QuestionText::try_new(&"x".repeat(2_001)).is_err());
    }

    #[tokio::test]
    async fn kind_must_be_known() {
        for value in ["choice", "text"] {
            assert_eq!(QuestionKind::try_new(value).unwrap().as_str(), value);
        }
        assert!(QuestionKind::try_new("essay").is_err());
    }

    #[tokio::test]
    async fn points_range_is_enforced() {
        assert_eq!(QuestionPoints::try_new(1).unwrap().as_i64(), 1);
        assert_eq!(QuestionPoints::try_new(100).unwrap().as_i64(), 100);
        assert!(QuestionPoints::try_new(0).is_err());
        assert!(QuestionPoints::try_new(101).is_err());
    }

    #[tokio::test]
    async fn choice_text_is_required() {
        assert!(ChoiceText::try_new("yes").is_ok());
        assert!(ChoiceText::try_new("").is_err());
        assert!(ChoiceText::try_new("   ").is_err());
        assert!(ChoiceText::try_new(&"x".repeat(501)).is_err());
    }

    #[tokio::test]
    async fn spec_invariants_hold() {
        // Text questions carry neither choices nor a correct index.
        assert!(QuestionSpec::try_new(kind("text"), None, None).is_ok());
        assert!(QuestionSpec::try_new(kind("text"), two_choices(), None).is_err());
        assert!(QuestionSpec::try_new(kind("text"), None, Some(0)).is_err());
        assert!(QuestionSpec::try_new(kind("text"), two_choices(), Some(0)).is_err());

        // Choice questions need both.
        assert!(QuestionSpec::try_new(kind("choice"), two_choices(), Some(0)).is_ok());
        assert!(QuestionSpec::try_new(kind("choice"), two_choices(), Some(1)).is_ok());
        assert!(QuestionSpec::try_new(kind("choice"), None, None).is_err());
        assert!(QuestionSpec::try_new(kind("choice"), None, Some(0)).is_err());
        assert!(QuestionSpec::try_new(kind("choice"), two_choices(), None).is_err());
    }

    #[tokio::test]
    async fn spec_correct_must_index_a_choice() {
        assert!(QuestionSpec::try_new(kind("choice"), two_choices(), Some(-1)).is_err());
        assert!(QuestionSpec::try_new(kind("choice"), two_choices(), Some(2)).is_err());
    }

    #[tokio::test]
    async fn spec_choice_count_is_bounded() {
        let n = |count: usize| Some(vec!["option".to_string(); count]);
        assert!(QuestionSpec::try_new(kind("choice"), n(1), Some(0)).is_err());
        assert!(QuestionSpec::try_new(kind("choice"), n(2), Some(0)).is_ok());
        assert!(QuestionSpec::try_new(kind("choice"), n(10), Some(0)).is_ok());
        assert!(QuestionSpec::try_new(kind("choice"), n(11), Some(0)).is_err());
    }

    #[tokio::test]
    async fn spec_rejects_blank_or_oversized_choices() {
        let blank = Some(vec!["yes".to_string(), "  ".to_string()]);
        assert!(QuestionSpec::try_new(kind("choice"), blank, Some(0)).is_err());
        let oversized = Some(vec!["yes".to_string(), "x".repeat(501)]);
        assert!(QuestionSpec::try_new(kind("choice"), oversized, Some(0)).is_err());
    }
}
