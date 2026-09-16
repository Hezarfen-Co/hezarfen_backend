//! The `exam_answer` table: the autosave upsert keyed by the (question, user,
//! seq) sitting triple, the per-sitting and per-exam reads, and the sweeps a
//! retake and the cascades use. The sitting key shape and the payload
//! validation's pure half live in [`crate::domain::exam_answer`].

use sqlx::PgConnection;

use crate::database::{Database, tx_with_retry};
use crate::domain::exam::ExamId;
use crate::domain::exam_answer::{AnswerText, ExamAnswer};
use crate::domain::exam_question::{ChoiceId, ExamQuestion, ExamQuestionId};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// Save (or overwrite) `user`'s answer to `question` for sitting `seq` —
/// the one write path, shared by the REST handler and the WebSocket room.
/// The payload must match the question's kind: a choice question takes
/// `selected` (the id of one of its choices), a text question takes `text`.
/// The caller has already checked that the attempt is in progress. Keyed
/// by `seq`, so a retake's save is a new row, not an overwrite of an
/// earlier sitting's answer.
pub async fn save(
    db: &Database,
    question: &ExamQuestion,
    user: &UserId,
    seq: i64,
    selected: Option<String>,
    text: Option<String>,
) -> Result<ExamAnswer, AppError> {
    let invalid = |field, reason| AppError::Validation(ValidationError::Invalid { field, reason });
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
                .ok_or_else(|| invalid("selected", "must name one of the question's choices"))?;
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
    // The save locks the *exam row* before upserting the answer, in one
    // transaction, and that is what ties the answer's fate to its exam:
    // reading the exam does not survive
    // [`crate::db::exam::delete`]'s window — a save landing after its
    // cascade swept the answers but before it committed would have read an
    // exam that was still there and written an orphan no sweep would ever
    // visit. Locking the key the delete removes first makes the two
    // serialize: the save either lands before the sweep (which then takes
    // the row too) or finds no exam and answers the same `404` the old
    // `no_exam` THROW did. Real foreign keys stand behind the lock — a
    // child insert whose parent is gone refuses itself — which is what
    // retired the bump-and-restore this used to ride on.
    let question = question.clone();
    let user = *user;
    tx_with_retry(db, false, async move |conn| {
        save_in(conn, &question, &user, seq, selected.clone(), text.clone()).await
    })
    .await
}

/// The locked exam-row probe plus the upsert, on one connection — split out
/// so the exam room's room-bound save shares the exact statements.
pub(crate) async fn save_in(
    conn: &mut PgConnection,
    question: &ExamQuestion,
    user: &UserId,
    seq: i64,
    selected: Option<ChoiceId>,
    text: Option<AnswerText>,
) -> Result<ExamAnswer, AppError> {
    let touched = sqlx::query!(
        r#"SELECT id AS "id: ExamId" FROM exam WHERE id = $1 FOR UPDATE"#,
        question.get_exam().uuid(),
    )
    .fetch_optional(&mut *conn)
    .await?;
    if touched.is_none() {
        return Err(AppError::NotFound);
    }
    let now = Timestamp::now();
    sqlx::query_as!(
        ExamAnswer,
        r#"INSERT INTO exam_answer (exam, question, app_user, selected, text, updated_at, seq)
           VALUES ($1, $2, $3, $4, $5, $6, $7)
           ON CONFLICT (question, app_user, seq) DO UPDATE
               SET selected = EXCLUDED.selected, text = EXCLUDED.text,
                   updated_at = EXCLUDED.updated_at
           RETURNING exam AS "exam: ExamId", question AS "question: ExamQuestionId",
                     app_user AS "user: UserId", seq,
                     selected AS "selected: ChoiceId",
                     text AS "text: AnswerText",
                     updated_at AS "updated_at: Timestamp""#,
        question.get_exam().uuid(),
        question.get_id().uuid(),
        user.uuid(),
        selected.as_ref().map(ChoiceId::as_str),
        text.as_ref().map(AnswerText::as_str),
        now.as_millis(),
        seq,
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(AppError::from)
}

/// One student's stored answer for a question in sitting `seq`, if any.
pub async fn read(
    db: &Database,
    question: &ExamQuestionId,
    user: &UserId,
    seq: i64,
) -> Result<Option<ExamAnswer>, AppError> {
    Ok(sqlx::query_as!(
        ExamAnswer,
        r#"SELECT exam AS "exam: ExamId", question AS "question: ExamQuestionId",
                  app_user AS "user: UserId", seq,
                  selected AS "selected: ChoiceId",
                  text AS "text: AnswerText",
                  updated_at AS "updated_at: Timestamp"
           FROM exam_answer
           WHERE question = $1 AND app_user = $2 AND seq = $3"#,
        question.uuid(),
        user.uuid(),
        seq,
    )
    .fetch_optional(db)
    .await?)
}

/// Drop one student's answer to a single question in sitting `seq`.
pub async fn delete(
    db: &Database,
    question: &ExamQuestionId,
    user: &UserId,
    seq: i64,
) -> Result<(), AppError> {
    sqlx::query!(
        r#"DELETE FROM exam_answer WHERE question = $1 AND app_user = $2 AND seq = $3"#,
        question.uuid(),
        user.uuid(),
        seq,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// One student's answers for a single sitting (`seq`) across an exam, in
/// question (id) order — the live-sitting read-back and, for a past
/// `seq`, that attempt's answer sheet.
pub async fn list_for_exam_user(
    db: &Database,
    exam: &ExamId,
    user: &UserId,
    seq: i64,
) -> Result<Vec<ExamAnswer>, AppError> {
    Ok(sqlx::query_as!(
        ExamAnswer,
        r#"SELECT exam AS "exam: ExamId", question AS "question: ExamQuestionId",
                  app_user AS "user: UserId", seq,
                  selected AS "selected: ChoiceId",
                  text AS "text: AnswerText",
                  updated_at AS "updated_at: Timestamp"
           FROM exam_answer
           WHERE exam = $1 AND app_user = $2 AND seq = $3
           ORDER BY question ASC"#,
        exam.uuid(),
        user.uuid(),
        seq,
    )
    .fetch_all(db)
    .await?)
}

/// The distinct sittings a student has any answer for at `exam`, ascending
/// — the index a history view lists attempts from.
pub async fn list_seqs_for_user(
    db: &Database,
    exam: &ExamId,
    user: &UserId,
) -> Result<Vec<i64>, AppError> {
    let rows = sqlx::query!(
        r#"SELECT DISTINCT seq FROM exam_answer
           WHERE exam = $1 AND app_user = $2 ORDER BY seq ASC"#,
        exam.uuid(),
        user.uuid(),
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|row| row.seq).collect())
}

/// Every answer of an exam — the live monitor aggregates these per student.
pub async fn list_for_exam(db: &Database, exam: &ExamId) -> Result<Vec<ExamAnswer>, AppError> {
    Ok(sqlx::query_as!(
        ExamAnswer,
        r#"SELECT exam AS "exam: ExamId", question AS "question: ExamQuestionId",
                  app_user AS "user: UserId", seq,
                  selected AS "selected: ChoiceId",
                  text AS "text: AnswerText",
                  updated_at AS "updated_at: Timestamp"
           FROM exam_answer WHERE exam = $1 ORDER BY question ASC"#,
        exam.uuid(),
    )
    .fetch_all(db)
    .await?)
}

/// Drop one student's answers across an exam — a retake starts from a
/// blank sheet.
pub async fn delete_for_exam_user(
    db: &Database,
    exam: &ExamId,
    user: &UserId,
) -> Result<(), AppError> {
    sqlx::query!(
        r#"DELETE FROM exam_answer WHERE exam = $1 AND app_user = $2"#,
        exam.uuid(),
        user.uuid(),
    )
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::exam::ExamId;
    use crate::domain::exam_question::{ChoiceId, ChoiceInput, QuestionKind, QuestionSpec};

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

    /// The minted id of the question's option at `index`.
    fn choice_id(question: &ExamQuestion, index: usize) -> ChoiceId {
        question.get_choices().unwrap()[index].get_id().clone()
    }

    fn student() -> UserId {
        UserId::from_key("019732e3-7b00-7000-8000-00000000aaaa")
    }

    /// The bite test for the exam-row touch in [`save`]: it exists
    /// to collide with [`crate::db::exam::delete`], so it must leave
    /// the counter it borrows exactly where it found it — absent stays absent
    /// (the boot backfill keys on `result_count = NONE`), and a real count is
    /// not moved by a student typing. The race half is
    /// `db::exam::tests::an_answer_written_inside_a_delete_never_outlives_the_exam`,
    /// which needs a real server; this half is the arithmetic and runs anywhere.
    #[tokio::test]
    async fn a_save_puts_the_exams_mark_counter_back_exactly() {
        use crate::domain::exam::{
            ExamAttemptLimit, ExamDescription, ExamKind, ExamSchedule, ExamTitle,
        };
        let (db, _leases) = crate::database::init_test_db().await;
        // The student is a foreign key now: a real row under the fixture's
        // fixed key.
        sqlx::query(
            "INSERT INTO app_user (id, username, created_at) \
             VALUES ($1, 'aaaa-fixture', 0)",
        )
        .bind(student().uuid())
        .execute(&db)
        .await
        .unwrap();
        let kinds = crate::domain::settings::Settings::defaults()
            .get_exam_kinds()
            .to_vec();
        // The instance and the dönem are foreign keys now: real rows the
        // fixture mints.
        let (instance, _course) = crate::db::course::a_test_instance(&db).await;
        let term = crate::db::term::a_test_term(&db).await;
        let exam = crate::db::exam::create(
            &db,
            &student(),
            &instance,
            &term,
            ExamTitle::try_new("quiz").unwrap(),
            ExamDescription::try_new("").unwrap(),
            ExamKind::try_new("yazili", &kinds).unwrap(),
            ExamSchedule::try_new(None, None, None, None).unwrap(),
            ExamAttemptLimit::try_new(1).unwrap(),
            true,
            false,
            false,
        )
        .await
        .unwrap();
        let stored = async |db: &Database| -> Option<i64> {
            sqlx::query_scalar::<_, Option<i64>>("SELECT result_count FROM exam WHERE id = $1")
                .bind(exam.get_id().uuid())
                .fetch_optional(db)
                .await
                .unwrap()
                .flatten()
        };
        // The question is a foreign key: a real row, so the choice ids the
        // answer names exist in the store too.
        let spec = QuestionSpec::try_new(
            QuestionKind::try_new("choice").unwrap(),
            Some(
                ["a", "b", "c"]
                    .iter()
                    .map(|l| ChoiceInput {
                        id: Some((*l).into()),
                        text: (*l).into(),
                    })
                    .collect(),
            ),
            Some("b".into()),
            &[],
        )
        .unwrap();
        let subject = crate::db::subject::create(
            &db,
            &crate::db::course::a_test_course(&db).await,
            crate::domain::subject::SubjectName::try_new("sorular").unwrap(),
            crate::domain::subject::SubjectDescription::try_new("").unwrap(),
        )
        .await
        .unwrap();
        let question = crate::db::exam_question::create(
            &db,
            exam.get_id(),
            *subject.get_id(),
            crate::domain::exam_question::QuestionText::try_new("pick one").unwrap(),
            crate::domain::exam_question::QuestionPoints::try_new(10).unwrap(),
            spec,
        )
        .await
        .unwrap();
        let pick = choice_id(&question, 1).as_str().to_string();

        // A fresh exam's counter is zero (the column is NOT NULL DEFAULT 0),
        // and a save must still not move it.
        assert_eq!(stored(&db).await, Some(0), "the fixture starts at zero");
        save(&db, &question, &student(), 1, Some(pick.clone()), None)
            .await
            .unwrap();
        assert_eq!(
            stored(&db).await,
            Some(0),
            "the touch left the counter alone"
        );

        // …and a counter that marks have moved is put back at its own value.
        sqlx::query("UPDATE exam SET result_count = 7 WHERE id = $1")
            .bind(exam.get_id().uuid())
            .execute(&db)
            .await
            .unwrap();
        save(&db, &question, &student(), 2, Some(pick), None)
            .await
            .unwrap();
        assert_eq!(stored(&db).await, Some(7), "the touch moved a real count");

        // The gate that makes the touch worth having: no exam, no answer.
        let orphan = choice_question(
            &ExamId::from_key("019732e3-7b00-7000-8000-00000000e0a0"),
            10,
            0,
        );
        let pick = choice_id(&orphan, 0).as_str().to_string();
        let refused = save(&db, &orphan, &student(), 1, Some(pick), None).await;
        assert!(
            matches!(refused, Err(AppError::NotFound)),
            "a save into a missing exam must 404, got {refused:?}"
        );
    }
}
