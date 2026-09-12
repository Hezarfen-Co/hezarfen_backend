//! An image pinned to an exam question — the question's own illustration
//! (`slot = NULL`, any kind: a map above the prompt) or one choice's picture
//! (`slot` = that choice's stable id, choice questions only). The row carries
//! metadata; the bytes live on disk under
//! [`crate::config::Config::files_path`] in a file named by `file` — a fresh
//! server-generated id per upload, so no user input ever shapes a disk path
//! and a replace never overwrites bytes in place. The row's identity is the
//! natural pair of its question and slot — "one image per slot" holds by
//! construction and a replace is a plain upsert. The queries and
//! transactions over these rows live in [`crate::db::question_image`]; the
//! web layer owns the blob I/O and its ordering (new blob before row, row
//! before old blob).

use crate::domain::exam::ExamId;
use crate::domain::exam_question::{ChoiceId, ExamQuestionId};
use crate::domain::monotonic_id::next_uuid;
use crate::domain::note_file::FileContentType;

/// The (question, slot) pair — the table's natural unique identity. The same
/// trick as the exam id types' composite keys: the slot sentinel `"q"` keeps
/// the illustration apart from every choice's picture, and a choice's stable
/// id can never be `"q"`, so the two shapes can never collide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionImageId {
    question: ExamQuestionId,
    slot: Option<ChoiceId>,
}

impl QuestionImageId {
    /// The one identity a (question, slot) pair can have — see
    /// [`crate::domain::key::slot`] for the key shape and why the two forms
    /// can never collide.
    pub fn for_slot(question: &ExamQuestionId, slot: Option<&ChoiceId>) -> Self {
        Self {
            question: question.clone(),
            slot: slot.cloned(),
        }
    }

    /// The wire form, `None` spelled with the `"q"` sentinel — the same
    /// spelling the stored key uses.
    pub fn key(&self) -> String {
        crate::domain::key::slot(&self.question.key(), self.slot.as_ref().map(|id| id.as_str()))
    }

    pub fn question(&self) -> &ExamQuestionId {
        &self.question
    }

    pub fn slot(&self) -> Option<&ChoiceId> {
        self.slot.as_ref()
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct QuestionImage {
    pub(crate) exam: ExamId,
    pub(crate) question: ExamQuestionId,
    /// `NULL` = the question's illustration; otherwise the id of the option
    /// this picture belongs to. Together with `question` this is the row's
    /// `UNIQUE NULLS NOT DISTINCT` identity.
    pub(crate) slot: Option<ChoiceId>,
    /// The blob's on-disk name — a fresh server-generated id every upload.
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
            exam: exam.clone(),
            question: question.clone(),
            slot: slot.cloned(),
            file: next_uuid().to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The illustration (`None`) and a choice picture key apart — the `"q"`
    /// sentinel can never be a choice's id.
    #[test]
    fn the_illustration_and_a_choice_never_collide() {
        let question = ExamQuestionId::from_key("018f1a00-0000-7000-8000-000000000001");
        let illustration = QuestionImageId::for_slot(&question, None);
        assert_eq!(
            illustration.key(),
            format!("{}_q", question.key()),
            "the sentinel spelling is the stored key's spelling"
        );
    }
}
