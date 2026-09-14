//! The photo of a pool question — one optional image row per question, the
//! single-slot spelling of [`crate::domain::question_image`]'s child-table
//! shape. The row carries metadata only; the bytes live on disk under
//! [`crate::config::Config::files_path`] in a file named by `file` — a fresh
//! server-generated id per upload, so a replace never overwrites bytes in
//! place. The question row itself carries no image columns; the child row is
//! the whole photo. The writes ride the parent's own transactions in
//! [`crate::db::pool_question`] (pending-only, like every content write);
//! the reads live in [`crate::db::pool_question_image`]; the web layer owns
//! the blob I/O and its ordering (new blob before row, row before old blob).

use crate::domain::note_file::FileContentType;
use crate::domain::pool_question::PoolQuestionId;

/// One question's photo row. `question` is the primary key — one image per
/// question by construction, and a replace is a plain upsert.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PoolQuestionImage {
    pub(crate) question: PoolQuestionId,
    /// The blob's on-disk name — a fresh server-generated id every upload.
    pub(crate) file: String,
    pub(crate) content_type: FileContentType,
    pub(crate) size: i64,
}

impl PoolQuestionImage {
    pub fn get_question(&self) -> &PoolQuestionId {
        &self.question
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
