//! An image pinned to an exam question — the question's own illustration
//! (`slot = NONE`, any kind: a map above the prompt) or one choice's picture
//! (`slot` = that choice's stable id, choice questions only). The row carries metadata; the bytes
//! live on disk under [`crate::config::Config::files_path`] in a file named by
//! `file` — a fresh server-generated ULID per upload, so no user input ever
//! shapes a disk path and a replace never overwrites bytes in place. The row
//! id is *deterministic* per (question, slot), so "one image per slot" holds
//! by construction and a replace is a plain UPSERT. The web layer owns the
//! blob I/O and its ordering (new blob before row, row before old blob);
//! this module owns the rows.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::QUESTION_IMAGE_TABLE;
use crate::database::Database;
use crate::domain::course::CourseId;
use crate::domain::exam::ExamId;
use crate::domain::exam_question::{ChoiceId, ExamQuestionId};
use crate::domain::key;
use crate::domain::note_file::FileContentType;
use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct QuestionImageId(RecordId);

impl QuestionImageId {
    /// The one id a (question, slot) pair can have — see [`key::slot`] for the
    /// key shape and why the two forms can never collide.
    pub fn for_slot(question: &ExamQuestionId, slot: Option<&ChoiceId>) -> Self {
        Self(RecordId::new(
            QUESTION_IMAGE_TABLE,
            key::slot(question.key(), slot.map(|id| id.as_str())),
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

#[derive(Debug, Clone, SurrealValue)]
pub struct QuestionImage {
    id: QuestionImageId,
    exam: ExamId,
    question: ExamQuestionId,
    /// `NONE` = the question's illustration; otherwise the id of the option
    /// this picture belongs to.
    slot: Option<ChoiceId>,
    /// The blob's on-disk name — a fresh ULID every upload.
    file: String,
    content_type: FileContentType,
    size: i64,
}

/// What one [`QuestionImage::upsert`] transaction returns: the row it stored
/// and the blob name it replaced. Both are arrays because SurrealDB drops an
/// object key valued `NONE` on the way out, while an empty array survives —
/// "nothing was replaced" has to be readable, not missing.
#[derive(SurrealValue)]
struct UpsertOutcome {
    stored: Vec<QuestionImage>,
    replaced: Vec<String>,
}

impl QuestionImage {
    /// Assemble a row (fresh blob name generated here) without persisting it.
    /// The caller writes the blob under [`Self::get_file`] first, then calls
    /// [`Self::upsert`] — so a stored row always points at a real blob.
    pub fn new(
        exam: &ExamId,
        question: &ExamQuestionId,
        slot: Option<&ChoiceId>,
        content_type: FileContentType,
        size: i64,
    ) -> Self {
        Self {
            id: QuestionImageId::for_slot(question, slot),
            exam: exam.clone(),
            question: question.clone(),
            slot: slot.cloned(),
            file: Ulid::new().to_string(),
            content_type,
            size,
        }
    }

    pub fn get_question(&self) -> &ExamQuestionId {
        &self.question
    }

    pub fn get_slot(&self) -> Option<&ChoiceId> {
        self.slot.as_ref()
    }

    pub fn get_file(&self) -> &str {
        &self.file
    }

    pub fn get_content_type(&self) -> &FileContentType {
        &self.content_type
    }

    pub fn get_size(&self) -> i64 {
        self.size
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
    pub async fn upsert(self, db: &Database) -> Result<(QuestionImage, Option<String>), AppError> {
        // whole-row-save-ok: self is built in place, never read back, and the slot id is deterministic
        let (exam, id) = (self.exam.clone(), self.id.record());
        let mut result = crate::db::exam_attempt::write_unfrozen(
            db,
            &exam,
            "LET $replaced = (SELECT VALUE file FROM $id);
             LET $stored = (UPSERT $id CONTENT $image);
             RETURN { stored: $stored, replaced: $replaced };",
            vec![
                ("id".into(), id.into_value()),
                ("image".into(), self.into_value()),
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
        question: &ExamQuestionId,
        slot: Option<&ChoiceId>,
        db: &Database,
    ) -> Result<Option<QuestionImage>, AppError> {
        Ok(db
            .select(QuestionImageId::for_slot(question, slot).record())
            .await?)
    }

    /// Every image of the exam's questions — one query for the list views.
    pub async fn list_for_exam(
        exam: &ExamId,
        db: &Database,
    ) -> Result<Vec<QuestionImage>, AppError> {
        let mut result = db
            .query("SELECT * FROM question_image WHERE exam = $ex")
            .bind(("ex", exam.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<QuestionImage>>(0)?)
    }

    pub async fn list_for_question(
        question: &ExamQuestionId,
        db: &Database,
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
        question: &ExamQuestionId,
        keep: &[ChoiceId],
        db: &Database,
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
        course: &CourseId,
        db: &Database,
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
    /// gate, same reason as [`Self::upsert`].
    pub async fn delete(self, db: &Database) -> Result<QuestionImage, AppError> {
        let mut result = crate::db::exam_attempt::write_unfrozen(
            db,
            &self.exam,
            "DELETE $id RETURN BEFORE;",
            vec![("id".into(), self.id.record().into_value())],
        )
        .await?;
        result
            .take::<Vec<QuestionImage>>(crate::db::exam_attempt::FROZEN_SLOT)?
            .into_iter()
            .next()
            .ok_or(AppError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::exam_question::{ChoiceInput, QuestionKind, QuestionSpec};

    /// A real exam row: an image write moves its exam's counter (that is what
    /// keeps a picture from outliving its exam), so a minted id nothing wrote
    /// is a 404.
    async fn exam_row(db: &Database) -> ExamId {
        crate::domain::exam::published_exam(db)
            .await
            .get_id()
            .clone()
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

        let (first, retired) = QuestionImage::new(&exam, &question, None, png(), 3)
            .upsert(&db)
            .await
            .unwrap();
        assert_eq!(retired, None, "a first upload retires no blob");
        let (second, retired) = QuestionImage::new(&exam, &question, None, png(), 5)
            .upsert(&db)
            .await
            .unwrap();
        // Same slot, same row — the replace swapped the blob pointer, and the
        // write itself names the blob the caller must unlink.
        assert_ne!(first.get_file(), second.get_file());
        assert_eq!(retired.as_deref(), Some(first.get_file()));
        let rows = QuestionImage::list_for_question(&question, &db)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_size(), 5);

        // A choice slot is its own row, keyed by the option's id.
        QuestionImage::new(&exam, &question, Some(&ids[0]), png(), 7)
            .upsert(&db)
            .await
            .unwrap();
        assert_eq!(
            QuestionImage::list_for_question(&question, &db)
                .await
                .unwrap()
                .len(),
            2
        );
        let choice = QuestionImage::read_slot(&question, Some(&ids[0]), &db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(choice.get_slot(), Some(&ids[0]));
        assert!(
            QuestionImage::read_slot(&question, Some(&ids[1]), &db)
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
        QuestionImage::new(&exam, &question, None, png(), 1)
            .upsert(&db)
            .await
            .unwrap();
        for id in &ids {
            QuestionImage::new(&exam, &question, Some(id), png(), 1)
                .upsert(&db)
                .await
                .unwrap();
        }

        // Keep the first and last option (reordered — order is irrelevant now).
        let keep = vec![ids[2].clone(), ids[0].clone()];
        let dropped = QuestionImage::delete_choices_not_in(&question, &keep, &db)
            .await
            .unwrap();
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].get_slot(), Some(&ids[1]));

        let left = QuestionImage::list_for_question(&question, &db)
            .await
            .unwrap();
        // The question illustration plus the two surviving option pictures.
        assert_eq!(left.len(), 3);
        assert!(
            QuestionImage::read_slot(&question, Some(&ids[0]), &db)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            QuestionImage::read_slot(&question, Some(&ids[2]), &db)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            QuestionImage::read_slot(&question, None, &db)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn an_empty_keep_set_clears_every_option_picture_but_not_the_illustration() {
        let db = crate::database::init_mem().await.unwrap();
        let exam = exam_row(&db).await;
        let question = ExamQuestionId::generate();
        let ids = choice_ids();
        QuestionImage::new(&exam, &question, None, png(), 1)
            .upsert(&db)
            .await
            .unwrap();
        for id in &ids[..2] {
            QuestionImage::new(&exam, &question, Some(id), png(), 1)
                .upsert(&db)
                .await
                .unwrap();
        }

        let dropped = QuestionImage::delete_choices_not_in(&question, &[], &db)
            .await
            .unwrap();
        assert_eq!(dropped.len(), 2);
        assert!(dropped.iter().all(|image| image.get_slot().is_some()));

        let left = QuestionImage::list_for_question(&question, &db)
            .await
            .unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].get_slot(), None);
    }

    #[tokio::test]
    async fn exam_listing_scopes_by_exam() {
        let db = crate::database::init_mem().await.unwrap();
        let exam_a = exam_row(&db).await;
        let exam_b = exam_row(&db).await;
        QuestionImage::new(&exam_a, &ExamQuestionId::generate(), None, png(), 1)
            .upsert(&db)
            .await
            .unwrap();
        assert_eq!(
            QuestionImage::list_for_exam(&exam_a, &db)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            QuestionImage::list_for_exam(&exam_b, &db)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
