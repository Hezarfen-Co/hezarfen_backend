//! An image pinned to an exam question — the question's own illustration
//! (`slot = NONE`, any kind: a map above the prompt) or one choice's picture
//! (`slot` = that choice's stable id, choice questions only). The row carries metadata; the bytes
//! live on disk under [`crate::config::Config::files_path`] in a file named by
//! `file` — a fresh server-generated ULID per upload, so no user input ever
//! shapes a disk path and a replace never overwrites bytes in place. The row
//! id is *deterministic* per (question, slot), so "one image per slot" holds
//! by construction and a replace is a plain UPSERT. The queries and
//! transactions over these rows live in [`crate::db::question_image`]; the web
//! layer owns the blob I/O and its ordering (new blob before row, row before
//! old blob).

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::QUESTION_IMAGE_TABLE;
use crate::domain::exam::ExamId;
use crate::domain::exam_question::{ChoiceId, ExamQuestionId};
use crate::domain::key;
use crate::domain::note_file::FileContentType;

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
    pub(crate) id: QuestionImageId,
    pub(crate) exam: ExamId,
    pub(crate) question: ExamQuestionId,
    /// `NONE` = the question's illustration; otherwise the id of the option
    /// this picture belongs to.
    pub(crate) slot: Option<ChoiceId>,
    /// The blob's on-disk name — a fresh ULID every upload.
    pub(crate) file: String,
    pub(crate) content_type: FileContentType,
    pub(crate) size: i64,
}

impl QuestionImage {
    /// Assemble a row (fresh blob name generated here) without persisting it.
    /// The caller writes the blob under [`Self::get_file`] first, then calls
    /// [`crate::db::question_image::upsert`] — so a stored row always points
    /// at a real blob.
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
            file: Ulid::generate().to_string(),
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
}
