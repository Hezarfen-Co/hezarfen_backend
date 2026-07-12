use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::MAX_ANSWER_TEXT_LEN;
use crate::database::{Database, EXAM_ANSWER_TABLE};
use crate::domain::exam::ExamId;
use crate::domain::exam_question::{ExamQuestion, ExamQuestionId};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_optional;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ExamAnswerId(RecordId);

impl ExamAnswerId {
    /// A deterministic id for the (question, user) pair — the question key is
    /// its own ULID, so the pair key is unambiguous. Saving is a single atomic
    /// UPSERT: re-answering overwrites the one row instead of racing the
    /// unique index into a 500. ULID keys are alphanumeric, so `_` is an
    /// unambiguous joiner.
    pub fn composite(question: &ExamQuestionId, user: &UserId) -> Self {
        Self(RecordId::new(
            EXAM_ANSWER_TABLE,
            format!("{}_{}", question.key(), user.key()),
        ))
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

/// A free-text answer: may be empty (a student clearing their draft is a valid
/// save), at most `MAX_ANSWER_TEXT_LEN` characters.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct AnswerText(String);

impl AnswerText {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_optional("text", value, MAX_ANSWER_TEXT_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One student's answer to one question, autosaved as they type or pick.
/// `exam` is denormalized from the question so per-exam reads (live monitor,
/// cascades) don't fan out through `exam_question`.
#[derive(Debug, Clone, SurrealValue)]
pub struct ExamAnswer {
    id: ExamAnswerId,
    exam: ExamId,
    question: ExamQuestionId,
    user: UserId,
    selected: Option<i64>,
    text: Option<AnswerText>,
    updated_at: Timestamp,
}

impl ExamAnswer {
    pub fn get_id(&self) -> &ExamAnswerId {
        &self.id
    }

    pub fn get_exam(&self) -> &ExamId {
        &self.exam
    }

    pub fn get_question(&self) -> &ExamQuestionId {
        &self.question
    }

    pub fn get_user(&self) -> &UserId {
        &self.user
    }

    pub fn get_selected(&self) -> Option<i64> {
        self.selected
    }

    pub fn get_text(&self) -> Option<&AnswerText> {
        self.text.as_ref()
    }

    pub fn get_updated_at(&self) -> Timestamp {
        self.updated_at
    }

    /// Whether this answer hits the question's correct choice. `None` for a
    /// text question — correctness is the grader's call, not the machine's.
    pub fn is_correct(&self, question: &ExamQuestion) -> Option<bool> {
        let correct = question.get_correct()?;
        Some(self.selected == Some(correct))
    }

    /// Save (or overwrite) `user`'s answer to `question` — the one write path,
    /// shared by the REST handler and the WebSocket room. The payload must
    /// match the question's kind: a choice question takes `selected` (indexing
    /// one of its choices), a text question takes `text`. The caller has
    /// already checked that the attempt is in progress.
    pub async fn save(
        question: &ExamQuestion,
        user: &UserId,
        selected: Option<i64>,
        text: Option<String>,
        db: &Database,
    ) -> Result<ExamAnswer, AppError> {
        let invalid =
            |field, reason| AppError::Validation(ValidationError::Invalid { field, reason });
        let (selected, text) = match question.get_kind().as_str() {
            "choice" => {
                if text.is_some() {
                    return Err(invalid(
                        "text",
                        "a choice question takes selected, not text",
                    ));
                }
                let Some(selected) = selected else {
                    return Err(invalid("selected", "required for a choice question"));
                };
                let count = question.get_choices().map_or(0, <[_]>::len);
                if !(0..count as i64).contains(&selected) {
                    return Err(invalid(
                        "selected",
                        "must index one of the question's choices",
                    ));
                }
                (Some(selected), None)
            }
            _ => {
                if selected.is_some() {
                    return Err(invalid(
                        "selected",
                        "a text question takes text, not selected",
                    ));
                }
                let Some(text) = text else {
                    return Err(invalid("text", "required for a text question"));
                };
                (None, Some(AnswerText::try_new(&text)?))
            }
        };
        let answer = ExamAnswer {
            id: ExamAnswerId::composite(question.get_id(), user),
            exam: question.get_exam().clone(),
            question: question.get_id().clone(),
            user: user.clone(),
            selected,
            text,
            updated_at: Timestamp::now(),
        };
        let saved: Option<ExamAnswer> = db.upsert(answer.id.record()).content(answer).await?;
        saved.ok_or_else(|| AppError::Internal("failed to save exam answer".into()))
    }

    /// One student's answers across an exam, in question (ULID) order.
    pub async fn list_for_exam_user(
        exam: &ExamId,
        user: &UserId,
        db: &Database,
    ) -> Result<Vec<ExamAnswer>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM exam_answer WHERE exam = $ex AND user = $usr ORDER BY question ASC",
            )
            .bind(("ex", exam.record()))
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<ExamAnswer>>(0)?)
    }

    /// Every answer of an exam — the live monitor aggregates these per student.
    pub async fn list_for_exam(exam: &ExamId, db: &Database) -> Result<Vec<ExamAnswer>, AppError> {
        let mut result = db
            .query("SELECT * FROM exam_answer WHERE exam = $ex ORDER BY question ASC")
            .bind(("ex", exam.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<ExamAnswer>>(0)?)
    }
}

/// The choice-question score suggested by the machine: `earned` points from
/// answers hitting `correct`, out of `possible` points across all choice
/// questions. Text questions never count — the final mark stays a human call
/// (`POST /exams/{id}/results`).
pub fn auto_score(questions: &[ExamQuestion], answers: &[ExamAnswer]) -> (i64, i64) {
    let mut earned = 0;
    let mut possible = 0;
    for question in questions {
        let Some(correct) = question.get_correct() else {
            continue;
        };
        possible += question.get_points().as_i64();
        if answers
            .iter()
            .any(|a| a.get_question() == question.get_id() && a.get_selected() == Some(correct))
        {
            earned += question.get_points().as_i64();
        }
    }
    (earned, possible)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::exam_question::{QuestionKind, QuestionSpec};

    fn choice_question(exam: &ExamId, points: i64, correct: i64) -> ExamQuestion {
        let spec = QuestionSpec::try_new(
            QuestionKind::try_new("choice").unwrap(),
            Some(vec!["a".into(), "b".into(), "c".into()]),
            Some(correct),
        )
        .unwrap();
        ExamQuestion::test_new(exam, "pick one", points, spec)
    }

    fn text_question(exam: &ExamId, points: i64) -> ExamQuestion {
        let spec =
            QuestionSpec::try_new(QuestionKind::try_new("text").unwrap(), None, None).unwrap();
        ExamQuestion::test_new(exam, "explain", points, spec)
    }

    fn answer(question: &ExamQuestion, user: &UserId, selected: Option<i64>) -> ExamAnswer {
        ExamAnswer {
            id: ExamAnswerId::composite(question.get_id(), user),
            exam: question.get_exam().clone(),
            question: question.get_id().clone(),
            user: user.clone(),
            selected,
            text: selected
                .is_none()
                .then(|| AnswerText::try_new("essay").unwrap()),
            updated_at: Timestamp::from_millis(1),
        }
    }

    fn student() -> UserId {
        UserId::from_key("01TESTUSERAAAAAAAAAAAAAAAA")
    }

    #[tokio::test]
    async fn answer_text_may_be_empty_but_bounded() {
        assert!(AnswerText::try_new("").is_ok());
        assert!(AnswerText::try_new(&"x".repeat(10_000)).is_ok());
        assert!(AnswerText::try_new(&"x".repeat(10_001)).is_err());
    }

    #[tokio::test]
    async fn auto_score_counts_correct_choices_only() {
        let exam = ExamId::from_key("01TESTEXAMAAAAAAAAAAAAAAAA");
        let user = student();
        let q1 = choice_question(&exam, 10, 0); // answered right
        let q2 = choice_question(&exam, 20, 1); // answered wrong
        let q3 = choice_question(&exam, 30, 2); // unanswered
        let q4 = text_question(&exam, 40); // never auto-scored
        let questions = vec![q1.clone(), q2.clone(), q3, q4.clone()];
        let answers = vec![
            answer(&q1, &user, Some(0)),
            answer(&q2, &user, Some(0)),
            answer(&q4, &user, None),
        ];
        assert_eq!(auto_score(&questions, &answers), (10, 60));
    }

    #[tokio::test]
    async fn auto_score_of_nothing_is_zero() {
        assert_eq!(auto_score(&[], &[]), (0, 0));
        let exam = ExamId::from_key("01TESTEXAMAAAAAAAAAAAAAAAA");
        let questions = vec![text_question(&exam, 50)];
        assert_eq!(auto_score(&questions, &[]), (0, 0));
    }

    #[tokio::test]
    async fn is_correct_judges_choice_questions_only() {
        let exam = ExamId::from_key("01TESTEXAMAAAAAAAAAAAAAAAA");
        let user = student();
        let choice = choice_question(&exam, 10, 1);
        assert_eq!(
            answer(&choice, &user, Some(1)).is_correct(&choice),
            Some(true)
        );
        assert_eq!(
            answer(&choice, &user, Some(0)).is_correct(&choice),
            Some(false)
        );
        let text = text_question(&exam, 10);
        assert_eq!(answer(&text, &user, None).is_correct(&text), None);
    }
}
