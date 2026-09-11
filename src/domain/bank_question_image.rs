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
//! I/O and its ordering; the queries live in
//! [`crate::db::bank_question_image`].

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::BANK_QUESTION_IMAGE_TABLE;
use crate::domain::bank_question::BankQuestionId;
use crate::domain::exam_question::ChoiceId;
use crate::domain::key;
use crate::domain::note_file::FileContentType;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct BankQuestionImageId(RecordId);

impl BankQuestionImageId {
    /// The one id a (question, slot) pair can have — see [`key::slot`] for the
    /// key shape and why the two forms can never collide.
    pub fn for_slot(question: &BankQuestionId, slot: Option<&ChoiceId>) -> Self {
        Self(RecordId::new(
            BANK_QUESTION_IMAGE_TABLE,
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
pub struct BankQuestionImage {
    pub(crate) id: BankQuestionImageId,
    pub(crate) bank_question: BankQuestionId,
    /// `NONE` = the question's illustration; otherwise the id of the option
    /// this picture belongs to.
    pub(crate) slot: Option<ChoiceId>,
    /// The blob's on-disk name — a fresh ULID every upload.
    pub(crate) file: String,
    pub(crate) content_type: FileContentType,
    pub(crate) size: i64,
}

impl BankQuestionImage {
    /// Assemble a row (fresh blob name generated here) without persisting it.
    /// The caller writes the blob under [`Self::get_file`] first, then calls
    /// [`crate::db::bank_question_image::upsert`] — so a stored row always
    /// points at a real blob.
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

    pub fn get_id(&self) -> &BankQuestionImageId {
        &self.id
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice_ids() -> Vec<ChoiceId> {
        use crate::domain::exam_question::{ChoiceInput, QuestionKind, QuestionSpec};
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

    #[test]
    fn for_slot_keys_by_question_and_choice_id() {
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
}
