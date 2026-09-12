//! A file attached to a note. The row carries metadata only (original
//! filename, MIME type, byte size); the bytes themselves live on disk under
//! [`crate::config::Config::files_path`], in a file named by this row's key —
//! a server-generated UUIDv7, so no user input ever shapes a disk path. The
//! web layer owns the blob I/O and its ordering (blob before row on upload,
//! row before blob on delete); persistence lives in [`crate::db::note_file`].

use uuid::Uuid;

use crate::constant::{MAX_FILE_CONTENT_TYPE_LEN, MAX_FILE_NAME_LEN};
use crate::domain::monotonic_id::next_uuid;
use crate::domain::note::NoteId;
use crate::error::ValidationError;

/// Typed note-file row id. A UUIDv7 minted by the process-wide monotonic
/// generator, so `id` order is mint order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct NoteFileId(Uuid);

impl NoteFileId {
    /// Minted from the process-wide monotonic generator, not a random v4:
    /// a note's files list `id DESC` (newest first,
    /// [`crate::db::note_file::list_for`]), and a random low half scrambles
    /// rows minted in the same millisecond.
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    /// Parse a wire key. A key that parses as no UUID — a malformed path
    /// segment — reads as the nil id, which matches no row: exactly the 404 a
    /// dangling record key produced under the old store, without turning a
    /// typo into a panic.
    pub fn from_key(key: &str) -> Self {
        Self(Uuid::parse_str(key).unwrap_or(Uuid::nil()))
    }

    /// The hyphenated wire form.
    pub fn key(&self) -> String {
        self.0.to_string()
    }
}

/// The uploader's original filename, kept for display and download headers.
/// Never used as a disk path — but path separators and control characters are
/// refused anyway: they are not filenames, only header/log injection attempts.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct FileName(String);

impl FileName {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        let value = value.trim();
        if value.is_empty() {
            return Err(ValidationError::Empty("filename"));
        }
        if value.chars().count() > MAX_FILE_NAME_LEN {
            return Err(ValidationError::TooLong {
                field: "filename",
                max: MAX_FILE_NAME_LEN,
                got: value.chars().count(),
            });
        }
        if value
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
        {
            return Err(ValidationError::Invalid {
                field: "filename",
                reason: "must not contain path separators or control characters",
            });
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The MIME type the client declared for the file. Blank defaults to
/// `application/octet-stream`; it is echoed back on download, so it is held
/// to header-safe ASCII.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct FileContentType(String);

impl FileContentType {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        let value = value.trim();
        if value.is_empty() {
            return Ok(Self("application/octet-stream".to_string()));
        }
        if value.chars().count() > MAX_FILE_CONTENT_TYPE_LEN {
            return Err(ValidationError::TooLong {
                field: "content_type",
                max: MAX_FILE_CONTENT_TYPE_LEN,
                got: value.chars().count(),
            });
        }
        if !value.is_ascii() {
            return Err(ValidationError::NotAscii("content_type"));
        }
        if value.chars().any(|c| c.is_ascii_control()) {
            return Err(ValidationError::Invalid {
                field: "content_type",
                reason: "must not contain control characters",
            });
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct NoteFile {
    pub(crate) id: NoteFileId,
    pub(crate) note: NoteId,
    pub(crate) name: FileName,
    pub(crate) content_type: FileContentType,
    pub(crate) size: i64,
}

impl NoteFile {
    /// Assemble a new attachment row (id generated here) without persisting
    /// it. The caller writes the blob to disk under the fresh id first, then
    /// calls [`crate::db::note_file::insert`] — so a stored row always points
    /// at a real blob.
    pub fn new(note: &NoteId, name: FileName, content_type: FileContentType, size: i64) -> Self {
        Self {
            id: NoteFileId::generate(),
            note: note.clone(),
            name,
            content_type,
            size,
        }
    }

    pub fn get_id(&self) -> &NoteFileId {
        &self.id
    }

    pub fn get_name(&self) -> &FileName {
        &self.name
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

    #[tokio::test]
    async fn filename_rules() {
        assert_eq!(
            FileName::try_new(" ödev.pdf ").unwrap().as_str(),
            "ödev.pdf"
        );
        assert!(FileName::try_new("").is_err());
        assert!(FileName::try_new("   ").is_err());
        assert!(FileName::try_new(&"x".repeat(256)).is_err());
        // Path separators and control characters are injection, not filenames.
        assert!(FileName::try_new("a/b.pdf").is_err());
        assert!(FileName::try_new("a\\b.pdf").is_err());
        assert!(FileName::try_new("a\nb.pdf").is_err());
        assert!(FileName::try_new("a\x00b").is_err());
    }

    #[tokio::test]
    async fn content_type_rules() {
        assert_eq!(
            FileContentType::try_new("application/pdf")
                .unwrap()
                .as_str(),
            "application/pdf"
        );
        // Blank means the client didn't say — fall back, don't fail.
        assert_eq!(
            FileContentType::try_new("").unwrap().as_str(),
            "application/octet-stream"
        );
        assert!(FileContentType::try_new(&"x".repeat(101)).is_err());
        assert!(FileContentType::try_new("appli¢ation/pdf").is_err());
        assert!(FileContentType::try_new("a\r\nb").is_err());
    }
}
