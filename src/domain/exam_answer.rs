use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{EXAM_ANSWER_TABLE, EXAM_RESULT_COUNT_FIELD, MAX_ANSWER_TEXT_LEN};
use crate::database::{Database, transaction_with_retry};
use crate::domain::exam::ExamId;
use crate::domain::exam_question::{ChoiceId, ExamQuestion, ExamQuestionId};
use crate::domain::key;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_optional;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ExamAnswerId(RecordId);

impl ExamAnswerId {
    /// A deterministic id for the (question, user, seq) triple. Saving is a
    /// single atomic UPSERT keyed by seq: re-answering *within a sitting*
    /// overwrites its one row, but a retake's `seq` writes a new row, so every
    /// sitting keeps its own answer history. See [`key::sitting`] for the key
    /// shape and why the first sitting stays bare.
    pub fn composite(question: &ExamQuestionId, user: &UserId, seq: i64) -> Self {
        let key = key::sitting(question.key(), user.key(), seq);
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
        // The save writes the *exam row* as well as the answer, in one
        // transaction, and that is what ties the answer's fate to its exam:
        // reading the exam does not survive [`crate::domain::exam::Exam::delete`]'s
        // window — a save landing after its `DELETE exam_answer WHERE exam = $ex`
        // but before the commit reads an exam that is still there (uncommitted)
        // while the sweep ran on a snapshot predating this row, so both commit
        // and the answer outlives the exam (measured 4 of 4 raced rounds).
        // Writing the key the delete removes makes the two collide, and the
        // store refuses one of them. It is the shape
        // [`crate::domain::menu::bump_menu_and_write`] uses, and the one a mark
        // already uses on this very row
        // ([`crate::domain::exam_result::ExamResult::grade`]).
        //
        // The bump-and-restore is not a flourish, it is the whole instrument.
        // The exam carries no revision to bump and must not grow one — its save
        // is a whole-row `CONTENT` write, so a column this struct did not know
        // about would be wiped by the next PATCH — so this touches the one
        // counter it already has and puts it back. Writing the *same* value is
        // not enough: an `UPDATE` that leaves the document unchanged is elided
        // and never reaches the store's write set, which the race test proved
        // (green on the raced delete, red the moment the value moves for real).
        // The restore is by captured value, `NONE` included, so the row is
        // byte-identical afterwards: the boot backfill still finds the rows it
        // keys on (`WHERE result_count = NONE`), the PATCH's
        // `(result_count ?? 0) = $was_results` still passes, and no teacher's
        // edit is refused because a student typed. Both statements are inside
        // the transaction, so a crash between them cannot leave the counter up.
        // No `cap::counter_lock` either: no counter moves here, and the hottest
        // write path in the app should not queue behind one.
        //
        // Admissible for `transaction_with_retry`: the `UPDATE`s, `SELECT`,
        // `IF`/`THROW` and `RETURN` can never answer "already exists", and the
        // `UPSERT`'s id is bijective with the (question, user, seq) triple
        // `exam_answer` keys — a lost round wrote nothing, and re-sending
        // resolves onto the same row rather than colliding with it.
        let (mut result, mut errors) = transaction_with_retry(
            db,
            &format!(
                "BEGIN TRANSACTION;
                 LET $was = (SELECT VALUE {EXAM_RESULT_COUNT_FIELD} FROM ONLY $ex);
                 LET $touched = (UPDATE $ex SET {EXAM_RESULT_COUNT_FIELD} = \
                     ({EXAM_RESULT_COUNT_FIELD} ?? 0) + 1 RETURN VALUE id);
                 IF array::len($touched) = 0 {{ THROW 'no_exam' }};
                 UPDATE $ex SET {EXAM_RESULT_COUNT_FIELD} = $was;
                 LET $row = (UPSERT $id CONTENT $answer RETURN AFTER);
                 RETURN $row[0];
                 COMMIT TRANSACTION;"
            ),
            &[
                ("ex".into(), answer.exam.record().into_value()),
                ("id".into(), answer.id.record().into_value()),
                ("answer".into(), answer.into_value()),
            ],
            &["no_exam"],
        )
        .await?;
        // An aborted transaction errors *every* slot, most with a generic "not
        // executed" — only the THROW's own slot names the reason.
        if errors
            .values()
            .any(|error| error.to_string().contains("no_exam"))
        {
            return Err(AppError::NotFound);
        }
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        // The trailing `RETURN` is the last statement before `COMMIT`, so its
        // slot follows the statement count rather than a hand-kept number (the
        // `Exam::delete` treatment); `num_statements` counts BEGIN and COMMIT.
        let slot = result.num_statements().saturating_sub(2);
        result
            .take::<Vec<ExamAnswer>>(slot)?
            .into_iter()
            .next()
            .ok_or_else(|| AppError::Internal("failed to save exam answer".into()))
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

    /// The bite test for the exam-row touch in [`ExamAnswer::save`]: it exists
    /// to collide with [`crate::domain::exam::Exam::delete`], so it must leave
    /// the counter it borrows exactly where it found it — absent stays absent
    /// (the boot backfill keys on `result_count = NONE`), and a real count is
    /// not moved by a student typing. The race half is
    /// `domain::exam::tests::an_answer_written_inside_a_delete_never_outlives_the_exam`,
    /// which needs a real server; this half is the arithmetic and runs anywhere.
    #[tokio::test]
    async fn a_save_puts_the_exams_mark_counter_back_exactly() {
        use crate::domain::exam::{
            Exam, ExamAttemptLimit, ExamDescription, ExamKind, ExamSchedule, ExamTitle,
        };
        let db = crate::database::init_mem().await.unwrap();
        let kinds = crate::domain::settings::Settings::defaults()
            .get_exam_kinds()
            .to_vec();
        let exam = Exam::create(
            &student(),
            &crate::db::course::a_test_course(&db).await,
            ExamTitle::try_new("quiz").unwrap(),
            ExamDescription::try_new("").unwrap(),
            ExamKind::try_new("quiz", &kinds).unwrap(),
            ExamSchedule::try_new(None, None, None, None).unwrap(),
            ExamAttemptLimit::try_new(1).unwrap(),
            true,
            false,
            false,
            &db,
        )
        .await
        .unwrap();
        let stored = async |db: &Database| -> Option<i64> {
            let mut result = db
                .query("SELECT VALUE result_count FROM ONLY $ex")
                .bind(("ex", exam.get_id().record()))
                .await
                .unwrap()
                .check()
                .unwrap();
            result.take::<Option<i64>>(0).unwrap()
        };
        let question = choice_question(exam.get_id(), 10, 1);
        let pick = choice_id(&question, 1).as_str().to_string();

        // A fresh exam carries no counter at all, and must still not after a save.
        assert_eq!(stored(&db).await, None, "the fixture must start absent");
        ExamAnswer::save(&question, &student(), 1, Some(pick.clone()), None, &db)
            .await
            .unwrap();
        assert_eq!(
            stored(&db).await,
            None,
            "the touch left the counter set — the backfill keys on NONE"
        );

        // …and a counter that marks have moved is put back at its own value.
        db.query("UPDATE $ex SET result_count = 7")
            .bind(("ex", exam.get_id().record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        ExamAnswer::save(&question, &student(), 2, Some(pick), None, &db)
            .await
            .unwrap();
        assert_eq!(stored(&db).await, Some(7), "the touch moved a real count");

        // The gate that makes the touch worth having: no exam, no answer.
        let orphan = choice_question(&ExamId::from_key("01NOSUCHEXAMAAAAAAAAAAAAAA"), 10, 0);
        let pick = choice_id(&orphan, 0).as_str().to_string();
        let refused = ExamAnswer::save(&orphan, &student(), 1, Some(pick), None, &db).await;
        assert!(
            matches!(refused, Err(AppError::NotFound)),
            "a save into a missing exam must 404, got {refused:?}"
        );
    }
}
