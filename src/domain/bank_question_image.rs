//! An image pinned to a bank question — the question's own illustration
//! (`slot = NULL`) or one choice's picture (`slot` = that choice's stable id,
//! choice questions only).
//! A structural copy of [`crate::domain::question_image`] with the exam FK
//! dropped: a bank template belongs to no exam, and its owner is reachable
//! through the `bank_question` row. The row carries metadata; the bytes live
//! on disk under [`crate::config::Config::files_path`] in a file named by
//! `file` — a fresh server-generated UUID per upload. The row is keyed by
//! (question, slot) under a `NULLS NOT DISTINCT` unique constraint, so "one
//! image per slot" holds by construction and a replace is a plain UPSERT. The
//! web layer owns the blob I/O and its ordering; the queries live in
//! [`crate::db::bank_question_image`].

use crate::domain::bank_question::BankQuestionId;
use crate::domain::exam_question::ChoiceId;
use crate::domain::key;
use crate::domain::monotonic_id::next_uuid;
use crate::domain::note_file::FileContentType;

/// The identity of one (question, slot) pair. Not a row column: the table's
/// unique constraint *is* the pair, and this struct's job is the
/// underscore-joined wire form at the HTTP edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BankQuestionImageId {
    pub(crate) question: BankQuestionId,
    pub(crate) slot: Option<ChoiceId>,
}

impl BankQuestionImageId {
    /// The one id a (question, slot) pair can have — see [`key::slot`] for
    /// the wire shape and why the two forms can never collide.
    pub fn for_slot(question: &BankQuestionId, slot: Option<&ChoiceId>) -> Self {
        Self {
            question: question.clone(),
            slot: slot.cloned(),
        }
    }

    /// The underscore-joined wire form (`{question}_{slot|q}`).
    pub fn key(&self) -> String {
        key::slot(
            self.question.key().as_str(),
            self.slot.as_ref().map(|id| id.as_str()),
        )
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BankQuestionImage {
    pub(crate) bank_question: BankQuestionId,
    /// `NULL` = the question's illustration; otherwise the id of the option
    /// this picture belongs to.
    pub(crate) slot: Option<ChoiceId>,
    /// The blob's on-disk name — a fresh UUID every upload.
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
            bank_question: question.clone(),
            slot: slot.cloned(),
            file: next_uuid().to_string(),
            content_type,
            size,
        }
    }

    /// The row's identity, built back from its unique-key columns.
    pub fn get_id(&self) -> BankQuestionImageId {
        BankQuestionImageId::for_slot(&self.bank_question, self.slot.as_ref())
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
        let question = BankQuestionId::from_key("0198f1a2-3b4c-7d5e-8f90-1a2b3c4d5e6f");
        let ids = choice_ids();
        assert_eq!(
            BankQuestionImageId::for_slot(&question, None).key(),
            format!("{}_q", question.key())
        );
        assert_eq!(
            BankQuestionImageId::for_slot(&question, Some(&ids[0])).key(),
            format!("{}_{}", question.key(), ids[0].as_str())
        );
        // Distinct options never share a row.
        assert_ne!(
            BankQuestionImageId::for_slot(&question, Some(&ids[0])),
            BankQuestionImageId::for_slot(&question, Some(&ids[1]))
        );
    }
}
