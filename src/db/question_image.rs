//! The `question_image` table: the slot-keyed upsert (and delete) behind the
//! question-freeze gate, the listing reads, and the choice-cleanup sweep a
//! question PATCH drives. The row's pure half — ids and the fresh-blob-name
//! constructor — lives in [`crate::domain::question_image`]; the blob bytes
//! stay the web layer's.

use surrealdb::types::SurrealValue;

use crate::database::Database;
use crate::domain::course::CourseId;
use crate::domain::exam::ExamId;
use crate::domain::exam_question::{ChoiceId, ExamQuestionId};
use crate::domain::question_image::{QuestionImage, QuestionImageId};
use crate::error::AppError;

/// What one [`upsert`] transaction returns: the row it stored
/// and the blob name it replaced. Both are arrays because SurrealDB drops an object
/// key valued `NONE` on the way out, while an empty array survives — "nothing
/// was replaced" has to be readable, not missing.
#[derive(SurrealValue)]
struct UpsertOutcome {
    stored: Vec<QuestionImage>,
    replaced: Vec<String>,
}

/// Create or replace the slot's image row — the deterministic id makes
/// this the whole "one image per slot" story — handing back what it stored
/// plus the blob name it replaced, for the caller to take off disk. Refused
/// once the exam has an attempt: pictures are part of the question, so they
/// freeze with it, and the gate is in this transaction rather than in a lock
/// the caller held.
///
/// The replaced name is read *here*, not by the caller before it: two
/// uploads to one slot both write this row, so they contend and the loser
/// re-reads the winner's blob name, where two pre-reads both saw the *old*
/// blob and left the loser's fresh one orphaned on disk.
pub async fn upsert(
    db: &Database,
    image: QuestionImage,
) -> Result<(QuestionImage, Option<String>), AppError> {
    // whole-row-save-ok: image is built in place, never read back, and the slot id is deterministic
    let (exam, id) = (image.exam.clone(), image.id.record());
    let mut result = crate::db::exam_attempt::write_unfrozen(
        db,
        &exam,
        "LET $replaced = (SELECT VALUE file FROM $id);
         LET $stored = (UPSERT $id CONTENT $image);
         RETURN { stored: $stored, replaced: $replaced };",
        vec![
            ("id".into(), id.into_value()),
            ("image".into(), image.into_value()),
        ],
    )
    .await?;
    // The trailing `RETURN` is the last statement before `COMMIT`, so its
    // slot follows the statement count rather than a hand-kept number;
    // `num_statements` counts BEGIN and COMMIT.
    let slot = result.num_statements().saturating_sub(2);
    let failed = || AppError::Internal("failed to store question image".into());
    let outcome = result
        .take::<Vec<UpsertOutcome>>(slot)?
        .into_iter()
        .next()
        .ok_or_else(failed)?;
    let stored = outcome.stored.into_iter().next().ok_or_else(failed)?;
    Ok((stored, outcome.replaced.into_iter().next()))
}

pub async fn read_slot(
    db: &Database,
    question: &ExamQuestionId,
    slot: Option<&ChoiceId>,
) -> Result<Option<QuestionImage>, AppError> {
    Ok(db
        .select(QuestionImageId::for_slot(question, slot).record())
        .await?)
}

/// Every image of the exam's questions — one query for the list views.
pub async fn list_for_exam(db: &Database, exam: &ExamId) -> Result<Vec<QuestionImage>, AppError> {
    let mut result = db
        .query("SELECT * FROM question_image WHERE exam = $ex")
        .bind(("ex", exam.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<QuestionImage>>(0)?)
}

pub async fn list_for_question(
    db: &Database,
    question: &ExamQuestionId,
) -> Result<Vec<QuestionImage>, AppError> {
    let mut result = db
        .query("SELECT * FROM question_image WHERE question = $q")
        .bind(("q", question.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<QuestionImage>>(0)?)
}

/// Drop the option pictures whose choice is gone — every choice image of
/// the question whose `slot` is *not* in `keep` (the question's own
/// illustration always stays), returning the removed rows so the caller can
/// take their blobs off disk.
///
/// This is what makes an edit non-destructive: a PATCH that reorders,
/// renames, or drops options passes the surviving choice ids as `keep`, so
/// only the pictures of genuinely removed options go. `keep = &[]` (a text
/// question, or an all-new choice list) still clears the lot.
pub async fn delete_choices_not_in(
    db: &Database,
    question: &ExamQuestionId,
    keep: &[ChoiceId],
) -> Result<Vec<QuestionImage>, AppError> {
    let keep: Vec<String> = keep.iter().map(|id| id.as_str().to_string()).collect();
    let mut result = db
        .query(
            "DELETE question_image \
             WHERE question = $q AND slot != NONE AND slot NOT IN $keep RETURN BEFORE",
        )
        .bind(("q", question.record()))
        .bind(("keep", keep))
        .await?
        .check()?;
    Ok(result.take::<Vec<QuestionImage>>(0)?)
}

/// The blob names behind every image of every exam of `course` — collected
/// *before* the course-delete cascade wipes the rows.
pub async fn file_keys_for_course(
    db: &Database,
    course: &CourseId,
) -> Result<Vec<String>, AppError> {
    let mut result = db
        .query(
            "SELECT VALUE file FROM question_image \
             WHERE exam IN (SELECT VALUE id FROM exam WHERE course = $course)",
        )
        .bind(("course", course.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<String>>(0)?)
}

/// Refused once the exam has an attempt, in the same transaction — same
/// gate, same reason as [`upsert`].
pub async fn delete(db: &Database, image: QuestionImage) -> Result<QuestionImage, AppError> {
    let mut result = crate::db::exam_attempt::write_unfrozen(
        db,
        &image.exam,
        "DELETE $id RETURN BEFORE;",
        vec![("id".into(), image.id.record().into_value())],
    )
    .await?;
    result
        .take::<Vec<QuestionImage>>(crate::db::exam_attempt::FROZEN_SLOT)?
        .into_iter()
        .next()
        .ok_or(AppError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::exam_question::{ChoiceInput, QuestionKind, QuestionSpec};
    use crate::domain::note_file::FileContentType;

    /// A real exam row: an image write moves its exam's counter (that is what
    /// keeps a picture from outliving its exam), so a minted id nothing wrote
    /// is a 404.
    async fn exam_row(db: &Database) -> ExamId {
        crate::db::exam::published_exam(db).await.get_id().clone()
    }

    fn png() -> FileContentType {
        FileContentType::try_new("image/png").unwrap()
    }

    /// Three minted choice ids to slot pictures against.
    fn choice_ids() -> Vec<ChoiceId> {
        QuestionSpec::try_new(
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
            Some("a".into()),
            &[],
        )
        .unwrap()
        .into_parts()
        .1
        .unwrap()
        .iter()
        .map(|c| c.get_id().clone())
        .collect()
    }

    #[tokio::test]
    async fn upsert_replaces_per_slot() {
        let db = crate::database::init_mem().await.unwrap();
        let exam = exam_row(&db).await;
        let question = ExamQuestionId::generate();
        let ids = choice_ids();

        let (first, retired) = upsert(&db, QuestionImage::new(&exam, &question, None, png(), 3))
            .await
            .unwrap();
        assert_eq!(retired, None, "a first upload retires no blob");
        let (second, retired) = upsert(&db, QuestionImage::new(&exam, &question, None, png(), 5))
            .await
            .unwrap();
        // Same slot, same row — the replace swapped the blob pointer, and the
        // write itself names the blob the caller must unlink.
        assert_ne!(first.get_file(), second.get_file());
        assert_eq!(retired.as_deref(), Some(first.get_file()));
        let rows = list_for_question(&db, &question).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_size(), 5);

        // A choice slot is its own row, keyed by the option's id.
        upsert(
            &db,
            QuestionImage::new(&exam, &question, Some(&ids[0]), png(), 7),
        )
        .await
        .unwrap();
        assert_eq!(list_for_question(&db, &question).await.unwrap().len(), 2);
        let choice = read_slot(&db, &question, Some(&ids[0]))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(choice.get_slot(), Some(&ids[0]));
        assert!(
            read_slot(&db, &question, Some(&ids[1]))
                .await
                .unwrap()
                .is_none()
        );
    }

    /// The behaviour the whole remodel exists for: an edit that keeps some
    /// options keeps exactly their pictures, and drops only the removed one's.
    #[tokio::test]
    async fn only_the_dropped_options_lose_their_pictures() {
        let db = crate::database::init_mem().await.unwrap();
        let exam = exam_row(&db).await;
        let question = ExamQuestionId::generate();
        let ids = choice_ids();
        upsert(&db, QuestionImage::new(&exam, &question, None, png(), 1))
            .await
            .unwrap();
        for id in &ids {
            upsert(
                &db,
                QuestionImage::new(&exam, &question, Some(id), png(), 1),
            )
            .await
            .unwrap();
        }

        // Keep the first and last option (reordered — order is irrelevant now).
        let keep = vec![ids[2].clone(), ids[0].clone()];
        let dropped = delete_choices_not_in(&db, &question, &keep).await.unwrap();
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].get_slot(), Some(&ids[1]));

        let left = list_for_question(&db, &question).await.unwrap();
        // The question illustration plus the two surviving option pictures.
        assert_eq!(left.len(), 3);
        assert!(
            read_slot(&db, &question, Some(&ids[0]))
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            read_slot(&db, &question, Some(&ids[2]))
                .await
                .unwrap()
                .is_some()
        );
        assert!(read_slot(&db, &question, None).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn an_empty_keep_set_clears_every_option_picture_but_not_the_illustration() {
        let db = crate::database::init_mem().await.unwrap();
        let exam = exam_row(&db).await;
        let question = ExamQuestionId::generate();
        let ids = choice_ids();
        upsert(&db, QuestionImage::new(&exam, &question, None, png(), 1))
            .await
            .unwrap();
        for id in &ids[..2] {
            upsert(
                &db,
                QuestionImage::new(&exam, &question, Some(id), png(), 1),
            )
            .await
            .unwrap();
        }

        let dropped = delete_choices_not_in(&db, &question, &[]).await.unwrap();
        assert_eq!(dropped.len(), 2);
        assert!(dropped.iter().all(|image| image.get_slot().is_some()));

        let left = list_for_question(&db, &question).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].get_slot(), None);
    }

    #[tokio::test]
    async fn exam_listing_scopes_by_exam() {
        let db = crate::database::init_mem().await.unwrap();
        let exam_a = exam_row(&db).await;
        let exam_b = exam_row(&db).await;
        upsert(
            &db,
            QuestionImage::new(&exam_a, &ExamQuestionId::generate(), None, png(), 1),
        )
        .await
        .unwrap();
        assert_eq!(list_for_exam(&db, &exam_a).await.unwrap().len(), 1);
        assert!(list_for_exam(&db, &exam_b).await.unwrap().is_empty());
    }
}
