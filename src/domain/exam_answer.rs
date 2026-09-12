use crate::constant::MAX_ANSWER_TEXT_LEN;
use crate::domain::exam::ExamId;
use crate::domain::exam_question::{ChoiceId, ExamQuestion, ExamQuestionId};
use crate::domain::key;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::validate_optional;

/// The identity of one (question, user, seq) triple — a sitting's answer. Not
/// a row column: the table's primary key *is* the triple, which is what makes
/// saving a single atomic UPSERT keyed by seq: re-answering *within a sitting*
/// overwrites its one row, but a retake's `seq` writes a new row, so every
/// sitting keeps its own answer history. See [`key::sitting`] for the wire
/// shape and why the first sitting stays bare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExamAnswerId {
    pub(crate) question: ExamQuestionId,
    pub(crate) user: UserId,
    pub(crate) seq: i64,
}

impl ExamAnswerId {
    pub fn composite(question: &ExamQuestionId, user: &UserId, seq: i64) -> Self {
        Self {
            question: question.clone(),
            user: user.clone(),
            seq,
        }
    }

    /// The underscore-joined wire form (`{question}_{user}[_{seq}]`).
    pub fn key(&self) -> String {
        key::sitting(
            self.question.key().as_str(),
            self.user.key().as_str(),
            self.seq,
        )
    }
}

/// A free-text answer: may be empty (a student clearing their draft is a valid
/// save), at most `MAX_ANSWER_TEXT_LEN` characters.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
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
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExamAnswer {
    pub(crate) exam: ExamId,
    pub(crate) question: ExamQuestionId,
    #[sqlx(rename = "app_user")]
    pub(crate) user: UserId,
    pub(crate) seq: i64,
    pub(crate) selected: Option<ChoiceId>,
    pub(crate) text: Option<AnswerText>,
    pub(crate) updated_at: Timestamp,
}

impl ExamAnswer {
    /// The row's identity, built back from its primary-key columns.
    pub fn get_id(&self) -> ExamAnswerId {
        ExamAnswerId::composite(&self.question, &self.user, self.seq)
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

    /// Which sitting this answer belongs to — 1 for the first attempt, up.
    pub fn get_seq(&self) -> i64 {
        self.seq
    }

    pub fn get_selected(&self) -> Option<&ChoiceId> {
        self.selected.as_ref()
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
        Some(self.selected.as_ref() == Some(correct))
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
    use crate::domain::exam_question::{ChoiceInput, QuestionKind, QuestionSpec};

    /// A three-option question whose `correct` is the option at `correct`.
    /// Positions are a *test* convenience only — the ids are minted, and every
    /// assertion below goes through `choice_id`.
    fn choice_question(exam: &ExamId, points: i64, correct: usize) -> ExamQuestion {
        let labels = ["a", "b", "c"];
        let spec = QuestionSpec::try_new(
            QuestionKind::try_new("choice").unwrap(),
            Some(
                labels
                    .iter()
                    .map(|l| ChoiceInput {
                        id: Some((*l).into()),
                        text: (*l).into(),
                    })
                    .collect(),
            ),
            Some(labels[correct].into()),
            &[],
        )
        .unwrap();
        ExamQuestion::test_new(exam, "pick one", points, spec)
    }

    fn text_question(exam: &ExamId, points: i64) -> ExamQuestion {
        let spec =
            QuestionSpec::try_new(QuestionKind::try_new("text").unwrap(), None, None, &[]).unwrap();
        ExamQuestion::test_new(exam, "explain", points, spec)
    }

    /// The minted id of the question's option at `index`.
    fn choice_id(question: &ExamQuestion, index: usize) -> ChoiceId {
        question.get_choices().unwrap()[index].get_id().clone()
    }

    fn answer(question: &ExamQuestion, user: &UserId, selected: Option<usize>) -> ExamAnswer {
        let selected = selected.map(|index| choice_id(question, index));
        ExamAnswer {
            exam: question.get_exam().clone(),
            question: question.get_id().clone(),
            user: user.clone(),
            seq: 1,
            text: selected
                .is_none()
                .then(|| AnswerText::try_new("essay").unwrap()),
            selected,
            updated_at: Timestamp::from_millis(1),
        }
    }

    fn student() -> UserId {
        UserId::from_key("0198f1a2-3b4c-7d5e-8f90-aa2b3c4d5e6f")
    }

    #[tokio::test]
    async fn seqs_key_distinct_rows_but_seq_one_keeps_the_bare_key() {
        let question = ExamQuestionId::from_key("0198f1a2-3b4c-7d5e-8f90-1a2b3c4d5e6f");
        let user = student();
        // seq 1 keeps the pre-history bare `{question}_{user}` key…
        let first = ExamAnswerId::composite(&question, &user, 1);
        assert_eq!(
            first.key(),
            format!("{}_{}", question.key(), user.key()),
            "seq 1 stays addressable at its historical key"
        );
        // …and a later sitting is a *different* id, so its answer is a new row.
        let second = ExamAnswerId::composite(&question, &user, 2);
        assert_ne!(first, second, "two seqs must not collide onto one row");
        assert_eq!(second.key(), format!("{}_{}_2", question.key(), user.key()));
    }

    #[tokio::test]
    async fn answer_text_may_be_empty_but_bounded() {
        assert!(AnswerText::try_new("").is_ok());
        assert!(AnswerText::try_new(&"x".repeat(10_000)).is_ok());
        assert!(AnswerText::try_new(&"x".repeat(10_001)).is_err());
    }

    #[tokio::test]
    async fn auto_score_counts_correct_choices_only() {
        let exam = ExamId::from_key("0198f1a2-3b4c-7d5e-8f90-be2b3c4d5e6f");
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
        let exam = ExamId::from_key("0198f1a2-3b4c-7d5e-8f90-be2b3c4d5e6f");
        let questions = vec![text_question(&exam, 50)];
        assert_eq!(auto_score(&questions, &[]), (0, 0));
    }

    #[tokio::test]
    async fn is_correct_judges_choice_questions_only() {
        let exam = ExamId::from_key("0198f1a2-3b4c-7d5e-8f90-be2b3c4d5e6f");
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
