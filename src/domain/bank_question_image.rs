//! An image pinned to a bank question — the question's own illustration
//! (`slot = NONE`) or one choice's picture (`slot = i`, choice questions only).
//! A structural copy of [`crate::domain::question_image`] with the exam FK
//! dropped: a bank template belongs to no exam, and its owner is reachable
//! through the `bank_question` row. The row carries metadata; the bytes live
//! on disk under [`crate::config::Config::files_path`] in a file named by
//! `file` — a fresh server-generated ULID per upload. The row id is
//! *deterministic* per (question, slot), so "one image per slot" holds by
//! construction and a replace is a plain UPSERT. The web layer owns the blob
//! I/O and its ordering; this module owns the rows.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::database::{BANK_QUESTION_IMAGE_TABLE, Database};
use crate::domain::bank_question::BankQuestionId;
use crate::domain::note_file::FileContentType;
use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct BankQuestionImageId(RecordId);

impl BankQuestionImageId {
    /// The one id a (question, slot) pair can have: `{bid}_q` for the
    /// question's own image, `{bid}_{i}` for choice `i` — uniqueness per slot
    /// needs no index this way.
    pub fn for_slot(question: &BankQuestionId, slot: Option<i64>) -> Self {
        let suffix = match slot {
            None => "q".to_string(),
            Some(index) => index.to_string(),
        };
        Self(RecordId::new(
            BANK_QUESTION_IMAGE_TABLE,
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
pub struct BankQuestionImage {
    id: BankQuestionImageId,
    bank_question: BankQuestionId,
    /// `NONE` = the question's illustration; `i` = the picture of choice `i`.
    slot: Option<i64>,
    /// The blob's on-disk name — a fresh ULID every upload.
    file: String,
    content_type: FileContentType,
    size: i64,
}

impl BankQuestionImage {
    /// Assemble a row (fresh blob name generated here) without persisting it.
    /// The caller writes the blob under [`Self::get_file`] first, then calls
    /// [`Self::upsert`] — so a stored row always points at a real blob.
    pub fn new(
        question: &BankQuestionId,
        slot: Option<i64>,
        content_type: FileContentType,
        size: i64,
    ) -> Self {
        Self {
            id: BankQuestionImageId::for_slot(question, slot),
            bank_question: question.clone(),
            slot,
            file: Ulid::new().to_string(),
            content_type,
            size,
        }
    }

    pub fn get_bank_question(&self) -> &BankQuestionId {
        &self.bank_question
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
    pub async fn upsert(self, db: &Database) -> Result<BankQuestionImage, AppError> {
        let written: Option<BankQuestionImage> = db.upsert(self.id.record()).content(self).await?;
        written.ok_or_else(|| AppError::Internal("failed to store bank question image".into()))
    }

    pub async fn read_slot(
        question: &BankQuestionId,
        slot: Option<i64>,
        db: &Database,
    ) -> Result<Option<BankQuestionImage>, AppError> {
        Ok(db
            .select(BankQuestionImageId::for_slot(question, slot).record())
            .await?)
    }

    pub async fn list_for_question(
        question: &BankQuestionId,
        db: &Database,
    ) -> Result<Vec<BankQuestionImage>, AppError> {
        let mut result = db
            .query("SELECT * FROM bank_question_image WHERE bank_question = $b")
            .bind(("b", question.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<BankQuestionImage>>(0)?)
    }

    /// Every bank image row, school-wide — for bucketing onto a whole template
    /// list in one query (the bank list is itself school-wide).
    pub async fn list_all(db: &Database) -> Result<Vec<BankQuestionImage>, AppError> {
        let mut result = db
            .query("SELECT * FROM bank_question_image")
            .await?
            .check()?;
        Ok(result.take::<Vec<BankQuestionImage>>(0)?)
    }

    /// Drop every *choice* image of the question (the question's own
    /// illustration stays), returning the removed rows so the caller can take
    /// their blobs off disk. Runs when a PATCH replaces the choice list — the
    /// old pictures belong to the old options.
    pub async fn delete_choices_for(
        question: &BankQuestionId,
        db: &Database,
    ) -> Result<Vec<BankQuestionImage>, AppError> {
        let mut result = db
            .query("DELETE bank_question_image WHERE bank_question = $b AND slot != NONE RETURN BEFORE")
            .bind(("b", question.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<BankQuestionImage>>(0)?)
    }

    pub async fn delete(self, db: &Database) -> Result<BankQuestionImage, AppError> {
        let deleted: Option<BankQuestionImage> = db.delete(self.id.record()).await?;
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
    async fn for_slot_keys_by_question_and_slot() {
        let question = BankQuestionId::from_key("abc");
        assert_eq!(
            BankQuestionImageId::for_slot(&question, None).key(),
            "abc_q"
        );
        assert_eq!(
            BankQuestionImageId::for_slot(&question, Some(0)).key(),
            "abc_0"
        );
        assert_eq!(
            BankQuestionImageId::for_slot(&question, Some(3)).key(),
            "abc_3"
        );
    }

    #[tokio::test]
    async fn upsert_replaces_per_slot() {
        let db = crate::database::init_mem().await.unwrap();
        let question = BankQuestionId::generate();

        let first = BankQuestionImage::new(&question, None, png(), 3)
            .upsert(&db)
            .await
            .unwrap();
        let second = BankQuestionImage::new(&question, None, png(), 5)
            .upsert(&db)
            .await
            .unwrap();
        // Same slot, same row — the replace swapped the blob pointer.
        assert_ne!(first.get_file(), second.get_file());
        let rows = BankQuestionImage::list_for_question(&question, &db)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_size(), 5);
    }

    #[tokio::test]
    async fn choice_wipe_spares_the_question_image() {
        let db = crate::database::init_mem().await.unwrap();
        let question = BankQuestionId::generate();
        for slot in [None, Some(0), Some(1)] {
            BankQuestionImage::new(&question, slot, png(), 1)
                .upsert(&db)
                .await
                .unwrap();
        }

        let dropped = BankQuestionImage::delete_choices_for(&question, &db)
            .await
            .unwrap();
        assert_eq!(dropped.len(), 2);
        assert!(dropped.iter().all(|image| image.get_slot().is_some()));

        let left = BankQuestionImage::list_for_question(&question, &db)
            .await
            .unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].get_slot(), None);
    }
}
