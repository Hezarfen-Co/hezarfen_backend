//! An image pinned to a bank question — the question's own illustration
//! (`slot = NONE`) or one choice's picture (`slot` = that choice's stable id,
//! choice questions only).
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

use crate::constant::BANK_QUESTION_IMAGE_TABLE;
use crate::database::Database;
use crate::domain::bank_question::BankQuestionId;
use crate::domain::exam_question::ChoiceId;
use crate::domain::note_file::FileContentType;
use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct BankQuestionImageId(RecordId);

impl BankQuestionImageId {
    /// The one id a (question, slot) pair can have: `{bid}_q` for the
    /// question's own image, `{bid}_{choice id}` for one option's picture —
    /// uniqueness per slot needs no index this way. Keyed by the option's
    /// *stable id*, so reordering the choice list moves no picture.
    pub fn for_slot(question: &BankQuestionId, slot: Option<&ChoiceId>) -> Self {
        // `_` is not in Crockford base32 and a ULID is never `"q"`, so
        // `{qid}_{cid}` and `{qid}_q` can never collide.
        let suffix = match slot {
            None => "q",
            Some(id) => id.as_str(),
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
    /// `NONE` = the question's illustration; otherwise the id of the option
    /// this picture belongs to.
    slot: Option<ChoiceId>,
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
        slot: Option<&ChoiceId>,
        content_type: FileContentType,
        size: i64,
    ) -> Self {
        Self {
            id: BankQuestionImageId::for_slot(question, slot),
            bank_question: question.clone(),
            slot: slot.cloned(),
            file: Ulid::new().to_string(),
            content_type,
            size,
        }
    }

    pub fn get_bank_question(&self) -> &BankQuestionId {
        &self.bank_question
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
    /// this the whole "one image per slot" story.
    pub async fn upsert(self, db: &Database) -> Result<BankQuestionImage, AppError> {
        // whole-row-save-ok: self is built in place, never read back, and the slot id is deterministic
        let written: Option<BankQuestionImage> = db.upsert(self.id.record()).content(self).await?;
        written.ok_or_else(|| AppError::Internal("failed to store bank question image".into()))
    }

    pub async fn read_slot(
        question: &BankQuestionId,
        slot: Option<&ChoiceId>,
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

    /// The image rows of several templates in one query — for bucketing onto a
    /// listing's *page* (never the whole table: the bank spans the school).
    pub async fn list_for_questions(
        questions: &[&BankQuestionId],
        db: &Database,
    ) -> Result<Vec<BankQuestionImage>, AppError> {
        if questions.is_empty() {
            return Ok(Vec::new());
        }
        let records: Vec<RecordId> = questions.iter().map(|q| q.record()).collect();
        let mut result = db
            .query("SELECT * FROM bank_question_image WHERE bank_question IN $ids")
            .bind(("ids", records))
            .await?
            .check()?;
        Ok(result.take::<Vec<BankQuestionImage>>(0)?)
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
        question: &BankQuestionId,
        keep: &[ChoiceId],
        db: &Database,
    ) -> Result<Vec<BankQuestionImage>, AppError> {
        let keep: Vec<String> = keep.iter().map(|id| id.as_str().to_string()).collect();
        let mut result = db
            .query(
                "DELETE bank_question_image \
                 WHERE bank_question = $b AND slot != NONE AND slot NOT IN $keep RETURN BEFORE",
            )
            .bind(("b", question.record()))
            .bind(("keep", keep))
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
    use crate::domain::exam_question::{ChoiceInput, QuestionKind, QuestionSpec};

    fn png() -> FileContentType {
        FileContentType::try_new("image/png").unwrap()
    }

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
    async fn for_slot_keys_by_question_and_choice_id() {
        let question = BankQuestionId::from_key("abc");
        let ids = choice_ids();
        assert_eq!(
            BankQuestionImageId::for_slot(&question, None).key(),
            "abc_q"
        );
        assert_eq!(
            BankQuestionImageId::for_slot(&question, Some(&ids[0])).key(),
            format!("abc_{}", ids[0].as_str())
        );
        // Distinct options never share a row.
        assert_ne!(
            BankQuestionImageId::for_slot(&question, Some(&ids[0])),
            BankQuestionImageId::for_slot(&question, Some(&ids[1]))
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

    /// The bank half of the non-destructive edit: keeping an option keeps its
    /// picture, and only a removed option's picture goes.
    #[tokio::test]
    async fn only_the_dropped_options_lose_their_pictures() {
        let db = crate::database::init_mem().await.unwrap();
        let question = BankQuestionId::generate();
        let ids = choice_ids();
        BankQuestionImage::new(&question, None, png(), 1)
            .upsert(&db)
            .await
            .unwrap();
        for id in &ids {
            BankQuestionImage::new(&question, Some(id), png(), 1)
                .upsert(&db)
                .await
                .unwrap();
        }

        let keep = vec![ids[0].clone(), ids[1].clone()];
        let dropped = BankQuestionImage::delete_choices_not_in(&question, &keep, &db)
            .await
            .unwrap();
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].get_slot(), Some(&ids[2]));

        let left = BankQuestionImage::list_for_question(&question, &db)
            .await
            .unwrap();
        assert_eq!(left.len(), 3);
    }

    #[tokio::test]
    async fn an_empty_keep_set_clears_every_option_picture() {
        let db = crate::database::init_mem().await.unwrap();
        let question = BankQuestionId::generate();
        let ids = choice_ids();
        BankQuestionImage::new(&question, None, png(), 1)
            .upsert(&db)
            .await
            .unwrap();
        for id in &ids[..2] {
            BankQuestionImage::new(&question, Some(id), png(), 1)
                .upsert(&db)
                .await
                .unwrap();
        }

        let dropped = BankQuestionImage::delete_choices_not_in(&question, &[], &db)
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
