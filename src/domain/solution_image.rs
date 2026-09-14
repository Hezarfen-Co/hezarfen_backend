//! The photo of an offered solution — one optional image row per solution,
//! the single-slot spelling of [`crate::domain::question_image`]'s
//! child-table shape. The row carries metadata only; the bytes live on disk
//! under [`crate::config::Config::files_path`] in a file named by `file` — a
//! fresh server-generated id per upload, so a replace never overwrites bytes
//! in place. The solution row itself carries no image columns; the child row
//! is the whole photo. The writes ride the parent's own transactions in
//! [`crate::db::solution`] (unconditional — solutions are unmoderated); the
//! reads live in [`crate::db::solution_image`]; the web layer owns the blob
//! I/O and its ordering (new blob before row, row before old blob).

use crate::domain::note_file::FileContentType;
use crate::domain::solution::SolutionId;

/// One solution's photo row. `solution` is the primary key — one image per
/// solution by construction, and a replace is a plain upsert.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SolutionImage {
    pub(crate) solution: SolutionId,
    /// The blob's on-disk name — a fresh server-generated id every upload.
    pub(crate) file: String,
    pub(crate) content_type: FileContentType,
    pub(crate) size: i64,
}

impl SolutionImage {
    pub fn get_solution(&self) -> &SolutionId {
        &self.solution
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
