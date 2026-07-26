use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{EXAM_ANSWER_TABLE, MAX_ANSWER_TEXT_LEN};
use crate::database::Database;
use crate::domain::exam::ExamId;
use crate::domain::exam_question::{ChoiceId, ExamQuestion, ExamQuestionId};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_optional;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ExamAnswerId(RecordId);

impl ExamAnswerId {
    /// A deterministic id for the (question, user, seq) triple — the question
    /// key is its own ULID, so the triple key is unambiguous. Saving is a
    /// single atomic UPSERT keyed by seq: re-answering *within a sitting*
    /// overwrites its one row, but a retake's `seq` writes a new row, so every
    /// sitting keeps its own answer history. The first sitting keeps the
    /// historical `{question}_{user}` shape (rows written before history
    /// existed stay addressable); later sittings append their number. ULID
    /// keys are alphanumeric, so `_` is an unambiguous joiner.
    pub fn composite(question: &ExamQuestionId, user: &UserId, seq: i64) -> Self {
        let key = if seq == 1 {
            format!("{}_{}", question.key(), user.key())
        } else {
            format!("{}_{}_{}", question.key(), user.key(), seq)
        };
        Self(RecordId::new(EXAM_ANSWER_TABLE, key))
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
    seq: i64,
    selected: Option<ChoiceId>,
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

    /// Save (or overwrite) `user`'s answer to `question` for sitting `seq` —
    /// the one write path, shared by the REST handler and the WebSocket room.
    /// The payload must match the question's kind: a choice question takes
    /// `selected` (the id of one of its choices), a text question takes `text`.
    /// The caller has already checked that the attempt is in progress. Keyed
    /// by `seq`, so a retake's save is a new row, not an overwrite of an
    /// earlier sitting's answer.
    pub async fn save(
        question: &ExamQuestion,
        user: &UserId,
        seq: i64,
        selected: Option<String>,
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
                // Membership, not a range: `selected` names an option by its
                // stable id, so a reorder of the list can never repoint it.
                let picked = question
                    .get_choices()
                    .unwrap_or_default()
                    .iter()
                    .find(|choice| choice.get_id().as_str() == selected)
                    .ok_or_else(|| {
                        invalid("selected", "must name one of the question's choices")
                    })?;
                (Some(picked.get_id().clone()), None)
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
            id: ExamAnswerId::composite(question.get_id(), user, seq),
            exam: question.get_exam().clone(),
            question: question.get_id().clone(),
            user: user.clone(),
            seq,
            selected,
            text,
            updated_at: Timestamp::now(),
        };
        let saved: Option<ExamAnswer> = db.upsert(answer.id.record()).content(answer).await?;
        saved.ok_or_else(|| AppError::Internal("failed to save exam answer".into()))
    }

    /// One student's stored answer for a question in sitting `seq`, if any.
    pub async fn read(
        question: &ExamQuestionId,
        user: &UserId,
        seq: i64,
        db: &Database,
    ) -> Result<Option<ExamAnswer>, AppError> {
        Ok(db
            .select(ExamAnswerId::composite(question, user, seq).record())
            .await?)
    }

    /// Drop one student's answer to a single question in sitting `seq`.
    pub async fn delete(
        question: &ExamQuestionId,
        user: &UserId,
        seq: i64,
        db: &Database,
    ) -> Result<(), AppError> {
        let _: Option<ExamAnswer> = db
            .delete(ExamAnswerId::composite(question, user, seq).record())
            .await?;
        Ok(())
    }

    /// One student's answers for a single sitting (`seq`) across an exam, in
    /// question (ULID) order — the live-sitting read-back and, for a past
    /// `seq`, that attempt's answer sheet.
    pub async fn list_for_exam_user(
        exam: &ExamId,
        user: &UserId,
        seq: i64,
        db: &Database,
    ) -> Result<Vec<ExamAnswer>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM exam_answer WHERE exam = $ex AND user = $usr AND seq = $seq
                 ORDER BY question ASC",
            )
            .bind(("ex", exam.record()))
            .bind(("usr", user.record()))
            .bind(("seq", seq))
            .await?
            .check()?;
        Ok(result.take::<Vec<ExamAnswer>>(0)?)
    }

    /// The distinct sittings a student has any answer for at `exam`, ascending
    /// — the index a history view lists attempts from.
    pub async fn list_seqs_for_user(
        exam: &ExamId,
        user: &UserId,
        db: &Database,
    ) -> Result<Vec<i64>, AppError> {
        let mut result = db
            .query(
                "SELECT VALUE seq FROM exam_answer WHERE exam = $ex AND user = $usr
                 ORDER BY seq ASC",
            )
            .bind(("ex", exam.record()))
            .bind(("usr", user.record()))
            .await?
            .check()?;
        let mut seqs = result.take::<Vec<i64>>(0)?;
        seqs.dedup();
        Ok(seqs)
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

    /// Drop one student's answers across an exam — a retake starts from a
    /// blank sheet.
    pub async fn delete_for_exam_user(
        exam: &ExamId,
        user: &UserId,
        db: &Database,
    ) -> Result<(), AppError> {
        db.query("DELETE exam_answer WHERE exam = $ex AND user = $usr")
            .bind(("ex", exam.record()))
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(())
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
            id: ExamAnswerId::composite(question.get_id(), user, 1),
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
        UserId::from_key("01TESTUSERAAAAAAAAAAAAAAAA")
    }

    #[tokio::test]
    async fn seqs_key_distinct_rows_but_seq_one_keeps_the_bare_key() {
        let question = ExamQuestionId::from_key("01TESTQUESTIONAAAAAAAAAAAA");
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
