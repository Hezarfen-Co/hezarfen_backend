//! An image pinned to an exam question — the question's own illustration
//! (`slot = NONE`, any kind: a map above the prompt) or one choice's picture
//! (`slot = i`, choice questions only). The row carries metadata; the bytes
//! live on disk under [`crate::config::Config::files_path`] in a file named by
//! `file` — a fresh server-generated ULID per upload, so no user input ever
//! shapes a disk path and a replace never overwrites bytes in place. The row
//! id is *deterministic* per (question, slot), so "one image per slot" holds
//! by construction and a replace is a plain UPSERT. The web layer owns the
//! blob I/O and its ordering (new blob before row, row before old blob);
//! this module owns the rows.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::database::{Database, QUESTION_IMAGE_TABLE};
use crate::domain::course::CourseId;
use crate::domain::exam::ExamId;
use crate::domain::exam_question::ExamQuestionId;
use crate::domain::note_file::FileContentType;
use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct QuestionImageId(RecordId);

impl QuestionImageId {
    /// The one id a (question, slot) pair can have: `{qid}_q` for the
    /// question's own image, `{qid}_{i}` for choice `i` — uniqueness per slot
    /// needs no index this way.
    pub fn for_slot(question: &ExamQuestionId, slot: Option<i64>) -> Self {
        let suffix = match slot {
            None => "q".to_string(),
            Some(index) => index.to_string(),
        };
        Self(RecordId::new(
            QUESTION_IMAGE_TABLE,
            format!("{}_{suffix}", question.key()),
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
    /// `NONE` = the question's illustration; `i` = the picture of choice `i`.
    slot: Option<i64>,
    /// The blob's on-disk name — a fresh ULID every upload.
    file: String,
    content_type: FileContentType,
    size: i64,
}

impl QuestionImage {
    /// Assemble a row (fresh blob name generated here) without persisting it.
    /// The caller writes the blob under [`Self::get_file`] first, then calls
    /// [`Self::upsert`] — so a stored row always points at a real blob.
    pub fn new(
        exam: &ExamId,
        question: &ExamQuestionId,
        slot: Option<i64>,
        content_type: FileContentType,
        size: i64,
    ) -> Self {
        Self {
            id: QuestionImageId::for_slot(question, slot),
            exam: exam.clone(),
            question: question.clone(),
            slot,
            file: Ulid::new().to_string(),
            content_type,
            size,
        }
    }

    pub fn get_question(&self) -> &ExamQuestionId {
        &self.question
    }

    pub fn get_slot(&self) -> Option<i64> {
        self.slot
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
    /// this the whole "one image per slot" story.
    pub async fn upsert(self, db: &Database) -> Result<QuestionImage, AppError> {
        let written: Option<QuestionImage> = db.upsert(self.id.record()).content(self).await?;
        written.ok_or_else(|| AppError::Internal("failed to store question image".into()))
    }

    pub async fn read_slot(
        question: &ExamQuestionId,
        slot: Option<i64>,
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

    /// Drop every *choice* image of the question (the question's own
    /// illustration stays), returning the removed rows so the caller can take
    /// their blobs off disk. Runs when a PATCH replaces the choice list — the
    /// old pictures belong to the old options.
    pub async fn delete_choices_for(
        question: &ExamQuestionId,
        db: &Database,
    ) -> Result<Vec<QuestionImage>, AppError> {
        let mut result = db
            .query("DELETE question_image WHERE question = $q AND slot != NONE RETURN BEFORE")
            .bind(("q", question.record()))
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

    pub async fn delete(self, db: &Database) -> Result<QuestionImage, AppError> {
        let deleted: Option<QuestionImage> = db.delete(self.id.record()).await?;
        deleted.ok_or(AppError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> FileContentType {
        FileContentType::try_new("image/png").unwrap()
    }

    #[tokio::test]
    async fn upsert_replaces_per_slot() {
        let db = crate::database::init_mem().await.unwrap();
        let exam = ExamId::generate();
        let question = ExamQuestionId::generate();

        let first = QuestionImage::new(&exam, &question, None, png(), 3)
            .upsert(&db)
            .await
            .unwrap();
        let second = QuestionImage::new(&exam, &question, None, png(), 5)
            .upsert(&db)
            .await
            .unwrap();
        // Same slot, same row — the replace swapped the blob pointer.
        assert_ne!(first.get_file(), second.get_file());
        let rows = QuestionImage::list_for_question(&question, &db)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_size(), 5);

        // A choice slot is its own row.
        QuestionImage::new(&exam, &question, Some(0), png(), 7)
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
        let choice = QuestionImage::read_slot(&question, Some(0), &db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(choice.get_slot(), Some(0));
        assert!(
            QuestionImage::read_slot(&question, Some(1), &db)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn choice_wipe_spares_the_question_image() {
        let db = crate::database::init_mem().await.unwrap();
        let exam = ExamId::generate();
        let question = ExamQuestionId::generate();
        for slot in [None, Some(0), Some(1)] {
            QuestionImage::new(&exam, &question, slot, png(), 1)
                .upsert(&db)
                .await
                .unwrap();
        }

        let dropped = QuestionImage::delete_choices_for(&question, &db)
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
        let exam_a = ExamId::generate();
        let exam_b = ExamId::generate();
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
