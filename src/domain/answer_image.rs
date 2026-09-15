//! A student's drawn answer to an exam question — their own illustration for
//! one (exam, user, question), mirroring [`crate::domain::question_image`] but
//! keyed by the answering student instead of a choice slot. The row carries
//! metadata; the bytes live on disk under [`crate::config::Config::files_path`]
//! in a file named by `file` — a fresh server-generated UUID per upload, so no
//! user input ever shapes a disk path and a replace never overwrites bytes in
//! place. The row is keyed by (question, user, seq) — the same shape as
//! [`crate::domain::exam_attempt::ExamAttemptId`] — so "one
//! drawing per student per question per sitting" holds by construction and a
//! replace within a sitting is a plain UPSERT on the composite primary key,
//! while a retake (`seq + 1`) accumulates its own rows instead of
//! overwriting. The queries and transactions over these rows live in
//! [`crate::db::answer_image`]; the web layer owns the blob I/O and its
//! ordering (new blob before row, row before old blob).

use crate::domain::exam::ExamId;
use crate::domain::exam_question::ExamQuestionId;
use crate::domain::key;
use crate::domain::monotonic_id::next_uuid;
use crate::domain::note_file::FileContentType;
use crate::domain::user::UserId;

/// The identity of one (question, user, seq) triple — a sitting's drawing.
/// Not a row column: the table's primary key *is* the (question, user, seq)
/// triple, and this struct's job is the underscore-joined wire form at the
/// HTTP edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerImageId {
    pub(crate) question: ExamQuestionId,
    pub(crate) user: UserId,
    pub(crate) seq: i64,
}

impl AnswerImageId {
    /// The one id a (question, user, seq) triple can have, so "one drawing per
    /// student per question per sitting" needs no index. See [`key::sitting`]
    /// for the wire shape and why the first sitting stays bare.
    pub fn composite(question: &ExamQuestionId, user: &UserId, seq: i64) -> Self {
        Self {
            question: question.clone(),
            user: *user,
            seq,
        }
    }

    /// The underscore-joined wire form (`{question}_{user}[_{seq}]`).
    pub fn key(&self) -> String {
        key::sitting(
            self.question.key().as_str(),
            self.user.key().as_str(),
            self.seq,
        )
    }
}

/// One student's drawing for one question. `exam` is denormalized (like
/// [`crate::domain::exam_answer::ExamAnswer`]) so per-exam reads (cascade) and
/// per-(exam, user) reads (sitting/grading views, retake blob-GC) don't fan out
/// through `exam_question`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AnswerImage {
    pub(crate) exam: ExamId,
    pub(crate) question: ExamQuestionId,
    #[sqlx(rename = "app_user")]
    pub(crate) user: UserId,
    /// Which sitting this drawing belongs to — 1 for the first attempt,
    /// counting up, so retakes accumulate instead of overwriting.
    pub(crate) seq: i64,
    /// The blob's on-disk name — a fresh UUID every upload.
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
            exam: exam.clone(),
            question: question.clone(),
            user: *user,
            seq,
            file: next_uuid().to_string(),
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
