//! A student's drawn answer to an exam question — their own illustration for
//! one (exam, user, question), mirroring [`crate::domain::question_image`] but
//! keyed by the answering student instead of a choice slot. The row carries
//! metadata; the bytes live on disk under [`crate::config::Config::files_path`]
//! in a file named by `file` — a fresh server-generated ULID per upload, so no
//! user input ever shapes a disk path and a replace never overwrites bytes in
//! place. The row id is *deterministic* per (question, user, seq) — the same
//! shape as [`crate::domain::exam_attempt::ExamAttemptId::composite`] — so "one
//! drawing per student per question per sitting" holds by construction and a
//! replace within a sitting is a plain UPSERT, while a retake (`seq + 1`)
//! accumulates its own rows instead of overwriting. The queries and
//! transactions over these rows live in [`crate::db::answer_image`]; the web
//! layer owns the blob I/O and its ordering (new blob before row, row before
//! old blob).

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::ANSWER_IMAGE_TABLE;
use crate::domain::exam::ExamId;
use crate::domain::exam_question::ExamQuestionId;
use crate::domain::key;
use crate::domain::note_file::FileContentType;
use crate::domain::user::UserId;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct AnswerImageId(RecordId);

impl AnswerImageId {
    /// The one id a (question, user, seq) triple can have, so "one drawing per
    /// student per question per sitting" needs no index. See [`key::sitting`]
    /// for the key shape and why the first sitting stays bare.
    pub fn composite(question: &ExamQuestionId, user: &UserId, seq: i64) -> Self {
        let key = key::sitting(question.key(), user.key(), seq);
        Self(RecordId::new(ANSWER_IMAGE_TABLE, key))
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

/// One student's drawing for one question. `exam` is denormalized (like
/// [`crate::domain::exam_answer::ExamAnswer`]) so per-exam reads (cascade) and
/// per-(exam, user) reads (sitting/grading views, retake blob-GC) don't fan out
/// through `exam_question`.
#[derive(Debug, Clone, SurrealValue)]
pub struct AnswerImage {
    pub(crate) id: AnswerImageId,
    pub(crate) exam: ExamId,
    pub(crate) question: ExamQuestionId,
    pub(crate) user: UserId,
    /// Which sitting this drawing belongs to — 1 for the first attempt,
    /// counting up, so retakes accumulate instead of overwriting.
    pub(crate) seq: i64,
    /// The blob's on-disk name — a fresh ULID every upload.
    pub(crate) file: String,
    pub(crate) content_type: FileContentType,
    pub(crate) size: i64,
}

impl AnswerImage {
    /// Assemble a row (fresh blob name generated here) without persisting it.
    /// The caller writes the blob under [`Self::get_file`] first, then calls
    /// [`crate::db::answer_image::upsert`] — so a stored row always points at
    /// a real blob.
    pub fn new(
        exam: &ExamId,
        question: &ExamQuestionId,
        user: &UserId,
        seq: i64,
        content_type: FileContentType,
        size: i64,
    ) -> Self {
        Self {
            id: AnswerImageId::composite(question, user, seq),
            exam: exam.clone(),
            question: question.clone(),
            user: user.clone(),
            seq,
            file: Ulid::generate().to_string(),
            content_type,
            size,
        }
    }

    pub fn get_question(&self) -> &ExamQuestionId {
        &self.question
    }

    /// Which sitting this drawing belongs to — 1 for the first attempt.
    pub fn get_seq(&self) -> i64 {
        self.seq
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
