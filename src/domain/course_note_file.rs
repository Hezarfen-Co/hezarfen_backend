//! A file attached to a course note. The row carries metadata only (original
//! filename, MIME type, byte size); the bytes themselves live on disk under
//! [`crate::config::Config::files_path`], in a file named by this row's key —
//! a server-generated ULID, so no user input ever shapes a disk path. The web
//! layer owns the blob I/O and its ordering (blob before row on upload, row
//! before blob on delete); this module owns the rows. `FileName` and
//! `FileContentType` are shared with [`crate::domain::note_file`] — same
//! validation, no reason to duplicate it.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

pub use crate::domain::note_file::{FileContentType, FileName};

use crate::constant::{
    COURSE_NOTE_FILE_COUNT_FIELD, COURSE_NOTE_FILE_TABLE, MAX_COURSE_NOTE_FILES,
};
use crate::database::Database;
use crate::db::cap;
use crate::domain::course_note::CourseNoteId;
use crate::domain::monotonic_id::next_ulid;
use crate::db::page::PagedList;
use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct CourseNoteFileId(RecordId);

impl CourseNoteFileId {
    /// Minted from the process-wide monotonic generator, not `Ulid::new()`:
    /// a note's files list `id DESC` (newest first,
    /// [`CourseNoteFile::list_for`]), and a random low half scrambles rows
    /// minted in the same millisecond.
    pub fn generate() -> Self {
        Self(RecordId::new(
            COURSE_NOTE_FILE_TABLE,
            next_ulid().to_string(),
        ))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(COURSE_NOTE_FILE_TABLE, key))
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
pub struct CourseNoteFile {
    id: CourseNoteFileId,
    course_note: CourseNoteId,
    name: FileName,
    content_type: FileContentType,
    size: i64,
}

impl CourseNoteFile {
    /// Assemble a new attachment row (id generated here) without persisting
    /// it. The caller writes the blob to disk under the fresh id first, then
    /// calls [`Self::insert`] — so a stored row always points at a real blob.
    pub fn new(
        note: &CourseNoteId,
        name: FileName,
        content_type: FileContentType,
        size: i64,
    ) -> Self {
        Self {
            id: CourseNoteFileId::generate(),
            course_note: note.clone(),
            name,
            content_type,
            size,
        }
    }

    pub fn get_id(&self) -> &CourseNoteFileId {
        &self.id
    }

    pub fn get_course_note(&self) -> &CourseNoteId {
        &self.course_note
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

    /// Persist the row assembled by [`Self::new`], refusing once its note
    /// already holds [`MAX_COURSE_NOTE_FILES`]. The slot and the row are
    /// taken together by [`cap::claim_and_create`] on the note row — see
    /// [`crate::domain::note_file::NoteFile::insert`] for why that is the
    /// only guard a concurrent insert cannot outrun.
    pub async fn insert(self, db: &Database) -> Result<CourseNoteFile, AppError> {
        // whole-row-save-ok: create of a fresh ULID row built in place by `new` — there is no prior row to clobber
        match cap::claim_and_create(
            &self.course_note.record(),
            COURSE_NOTE_FILE_COUNT_FIELD,
            MAX_COURSE_NOTE_FILES as i64,
            &self.id.record(),
            &self,
            db,
        )
        .await?
        {
            cap::Claimed::Made(created) => Ok(created),
            // Also how a missing note reads: no note row means no slot to take.
            cap::Claimed::Full => Err(AppError::Conflict(
                "the course note already holds the maximum of 10 files — delete one first",
            )),
            // Unreachable: the id is a ULID this call just generated.
            cap::Claimed::Duplicate => Err(AppError::Internal(
                "failed to create course note file".into(),
            )),
        }
    }

    /// Read a file's row by id alone, for the callers that have no note in
    /// hand yet and reach the note *through* the file (the AI bridge's blob
    /// stream). A key from any other table simply reads as `None`, which is
    /// what keeps a personal note's file id unreachable here.
    pub async fn read(
        id: &CourseNoteFileId,
        db: &Database,
    ) -> Result<Option<CourseNoteFile>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// Read a file's row only if it belongs to `note` — callers have already
    /// checked the note belongs to a course the caller may act on.
    pub async fn read_for(
        id: &CourseNoteFileId,
        note: &CourseNoteId,
        db: &Database,
    ) -> Result<Option<CourseNoteFile>, AppError> {
        let file: Option<CourseNoteFile> = db.select(id.record()).await?;
        Ok(file.filter(|file| &file.course_note == note))
    }

    /// All of `note`'s attachment rows, newest first.
    pub async fn list_for(
        note: &CourseNoteId,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<CourseNoteFile>, i64), AppError> {
        PagedList::new(
            "course_note_file WHERE course_note = $note",
            "ORDER BY id DESC",
        )
        .bind("note", note.record())
        .run(limit, offset, db)
        .await
    }

    /// The blob names (record keys, since a file's blob is named by its own
    /// id) behind every note attachment of `course` — collected *before* the
    /// course-delete cascade wipes the rows.
    pub async fn file_keys_for_course(
        course: &crate::domain::course::CourseId,
        db: &Database,
    ) -> Result<Vec<String>, AppError> {
        let mut result = db
            .query(
                "SELECT VALUE id FROM course_note_file \
                 WHERE course_note IN (SELECT VALUE id FROM course_note WHERE course = $course)",
            )
            .bind(("course", course.record()))
            .await?
            .check()?;
        Ok(result
            .take::<Vec<RecordId>>(0)?
            .into_iter()
            .map(|id| match id.key {
                RecordIdKey::String(key) => key,
                _ => String::new(),
            })
            .collect())
    }

    /// Delete the row and give its slot back in the same transaction — the
    /// note itself is untouched, so unlike the note-delete cascade this one
    /// has a counter to correct.
    pub async fn delete(self, db: &Database) -> Result<CourseNoteFile, AppError> {
        let mut result = db
            .query(
                "BEGIN TRANSACTION;
                 LET $gone = (DELETE $id RETURN BEFORE);
                 UPDATE $note SET file_count = math::max([(file_count ?? 0) - array::len($gone), 0]);
                 RETURN $gone;
                 COMMIT TRANSACTION;",
            )
            .bind(("id", self.id.record()))
            .bind(("note", self.course_note.record()))
            .await?
            .check()?;
        result
            .take::<Vec<CourseNoteFile>>(3)?
            .into_iter()
            .next()
            .ok_or(AppError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::course::{CourseDescription, CourseKind, CourseTitle};
    use crate::domain::course_note::{CourseNote, CourseNoteContent, CourseNoteTitle};

    async fn note_of(db: &Database, title: &str) -> CourseNote {
        let creator = crate::domain::user::UserId::generate();
        let course = crate::db::course::create(
            db,
            &creator,
            CourseTitle::try_new("c").unwrap(),
            CourseDescription::try_new("").unwrap(),
            CourseKind::try_new("course").unwrap(),
            None,
            None,
        )
        .await
        .unwrap();
        CourseNote::create(
            course.get_id(),
            &creator,
            CourseNoteTitle::try_new(title).unwrap(),
            CourseNoteContent::try_new("body").unwrap(),
            db,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn rows_scope_to_their_note() {
        let db = crate::database::init_mem().await.unwrap();
        let note_a = note_of(&db, "a").await.get_id().clone();
        let note_b = note_of(&db, "b").await.get_id().clone();
        let file = CourseNoteFile::new(
            &note_a,
            FileName::try_new("plan.pdf").unwrap(),
            FileContentType::try_new("application/pdf").unwrap(),
            3,
        )
        .insert(&db)
        .await
        .unwrap();

        let found = CourseNoteFile::read_for(file.get_id(), &note_a, &db)
            .await
            .unwrap();
        assert_eq!(found.unwrap().get_name().as_str(), "plan.pdf");
        assert!(
            CourseNoteFile::read_for(file.get_id(), &note_b, &db)
                .await
                .unwrap()
                .is_none()
        );

        let listed = async |note: &CourseNoteId| {
            CourseNoteFile::list_for(note, None, 0, &db)
                .await
                .unwrap()
                .0
        };
        assert_eq!(listed(&note_a).await.len(), 1);
        assert!(listed(&note_b).await.is_empty());

        file.delete(&db).await.unwrap();
        assert!(listed(&note_a).await.is_empty());
    }

    /// The counter and the rows are written in one transaction, so the stored
    /// count must equal the stored rows — after a success *and* after the
    /// refusal that fills the cap, which must move neither.
    #[tokio::test]
    async fn counter_tracks_stored_rows() {
        let db = crate::database::init_mem().await.unwrap();
        let note = note_of(&db, "a").await.get_id().clone();
        let stored_count = async |note: &CourseNoteId| -> i64 {
            db.query("SELECT VALUE file_count FROM $note")
                .bind(("note", note.record()))
                .await
                .unwrap()
                .take::<Vec<i64>>(0)
                .unwrap()
                .into_iter()
                .next()
                .unwrap_or(0)
        };
        let add = async |note: &CourseNoteId| {
            CourseNoteFile::new(
                note,
                FileName::try_new("plan.pdf").unwrap(),
                FileContentType::try_new("application/pdf").unwrap(),
                3,
            )
            .insert(&db)
            .await
        };

        for filled in 1..=MAX_COURSE_NOTE_FILES {
            add(&note).await.unwrap();
            assert_eq!(stored_count(&note).await, filled as i64);
            assert_eq!(
                CourseNoteFile::list_for(&note, None, 0, &db)
                    .await
                    .unwrap()
                    .1,
                filled as i64
            );
        }

        assert!(matches!(add(&note).await, Err(AppError::Conflict(_))));
        assert_eq!(stored_count(&note).await, MAX_COURSE_NOTE_FILES as i64);
        assert_eq!(
            CourseNoteFile::list_for(&note, None, 0, &db)
                .await
                .unwrap()
                .1,
            MAX_COURSE_NOTE_FILES as i64
        );
    }
}
