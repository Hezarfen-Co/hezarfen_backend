use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{
    COURSE_NOTE_TABLE, ENROLLMENT_COUNT_FIELD, MAX_NOTE_CONTENT_LEN, MAX_NOTE_TITLE_LEN,
};
use crate::database::Database;
use crate::domain::cap;
use crate::domain::course::CourseId;
use crate::domain::course_note_file::CourseNoteFile;
use crate::domain::field_update::FieldUpdate;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::page::PagedList;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_optional, validate_required};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct CourseNoteId(RecordId);

impl CourseNoteId {
    /// Minted from the process-wide monotonic generator, not `Ulid::new()`:
    /// a course's notes list `id DESC` (newest first,
    /// [`CourseNote::list_for_course`]), and a random low half scrambles rows
    /// minted in the same millisecond.
    pub fn generate() -> Self {
        Self(RecordId::new(COURSE_NOTE_TABLE, next_ulid().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(COURSE_NOTE_TABLE, key))
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

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct CourseNoteTitle(String);

impl CourseNoteTitle {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("title", value, MAX_NOTE_TITLE_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct CourseNoteContent(String);

impl CourseNoteContent {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_optional("content", value, MAX_NOTE_CONTENT_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What [`CourseNote::delete`]'s transaction removed: the note row (empty if
/// it had already vanished) and every attachment row the cascade took with
/// it — the only set whose blobs are safe to unlink.
#[derive(Debug, SurrealValue)]
struct DeleteOutcome {
    note: Vec<CourseNote>,
    files: Vec<CourseNoteFile>,
}

#[derive(Debug, Clone, SurrealValue)]
pub struct CourseNote {
    id: CourseNoteId,
    course: CourseId,
    author: UserId,
    title: CourseNoteTitle,
    content: CourseNoteContent,
}

impl CourseNote {
    pub fn get_id(&self) -> &CourseNoteId {
        &self.id
    }

    pub fn get_course(&self) -> &CourseId {
        &self.course
    }

    pub fn get_title(&self) -> &CourseNoteTitle {
        &self.title
    }

    pub fn get_content(&self) -> &CourseNoteContent {
        &self.content
    }

    pub async fn create(
        course: &CourseId,
        author: &UserId,
        title: CourseNoteTitle,
        content: CourseNoteContent,
        db: &Database,
    ) -> Result<CourseNote, AppError> {
        let note = CourseNote {
            id: CourseNoteId::generate(),
            course: course.clone(),
            author: author.clone(),
            title,
            content,
        };
        // The course row is *written* (bumped and put back), not read, so this
        // collides with `Course::delete`'s cascade: a note that outlives its
        // course is unreachable forever — every route to it goes through the
        // course. See [`cap::touch_and_create`].
        cap::touch_and_create(
            &course.record(),
            ENROLLMENT_COUNT_FIELD,
            &note.id.record(),
            &note,
            db,
        )
        .await?
        .ok_or(AppError::NotFound)
    }

    pub async fn read(id: &CourseNoteId, db: &Database) -> Result<Option<CourseNote>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    pub async fn list_for_course(
        course: &CourseId,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<CourseNote>, i64), AppError> {
        PagedList::new("course_note WHERE course = $crs", "ORDER BY id DESC")
            .bind("crs", course.record())
            .run(limit, offset, db)
            .await
    }

    /// Request-scoped: no lock spans the handler's read and this write, so an
    /// omitted field (`None`) is not written at all. Passing the snapshot's
    /// value back instead would revert a concurrent edit of that field —
    /// scoping the `SET` alone does not prevent that, the values have to come
    /// from the request. Neither column is nullable, so plain `Option` per
    /// field says everything there is to say.
    pub async fn update(
        self,
        title: Option<CourseNoteTitle>,
        content: Option<CourseNoteContent>,
        db: &Database,
    ) -> Result<CourseNote, AppError> {
        FieldUpdate::new(self.id.record())
            .set("title", title)
            .set("content", content)
            .run::<CourseNote>(db)
            .await
    }

    /// Delete the note and cascade-remove its attachment rows, returning both:
    /// the note, and the attachment rows this transaction actually removed.
    /// Blob files on disk are the web layer's to remove, but only for *these*
    /// rows — a row uploaded after the caller listed the note's files is
    /// deleted here too, and a pre-read snapshot would strand its blob.
    pub async fn delete(
        self,
        db: &Database,
    ) -> Result<(CourseNote, Vec<CourseNoteFile>), AppError> {
        let mut result = db
            .query(
                "BEGIN TRANSACTION;
                 LET $files = (DELETE course_note_file WHERE course_note = $note RETURN BEFORE);
                 LET $gone = (DELETE $note RETURN BEFORE);
                 RETURN { note: $gone, files: $files };
                 COMMIT TRANSACTION;",
            )
            .bind(("note", self.id.record()))
            .await?
            .check()?;
        // BEGIN is slot 0, the two LETs slots 1-2; the RETURN is slot 3.
        let outcome = result
            .take::<Vec<DeleteOutcome>>(3)?
            .into_iter()
            .next()
            .ok_or_else(|| AppError::Internal("failed to delete course note".into()))?;
        let note = outcome.note.into_iter().next().ok_or(AppError::NotFound)?;
        Ok((note, outcome.files))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn title_is_required() {
        assert_eq!(CourseNoteTitle::try_new("hi").unwrap().as_str(), "hi");
        assert!(CourseNoteTitle::try_new("  ").is_err());
    }

    #[tokio::test]
    async fn content_is_optional() {
        assert_eq!(CourseNoteContent::try_new("").unwrap().as_str(), "");
        assert_eq!(CourseNoteContent::try_new("body").unwrap().as_str(), "body");
    }

    /// The blobs the handler unlinks are exactly the rows this transaction
    /// removed — including one uploaded after any pre-read snapshot would have
    /// been taken, which is the row whose blob used to leak.
    #[tokio::test]
    async fn delete_returns_the_attachment_rows_it_removed() {
        use crate::domain::course::{Course, CourseDescription, CourseKind, CourseTitle};
        use crate::domain::course_note_file::{FileContentType, FileName};

        let db = crate::database::init_mem().await.unwrap();
        let creator = crate::domain::user::UserId::generate();
        let course = Course::create(
            &creator,
            CourseTitle::try_new("Math").unwrap(),
            CourseDescription::try_new("").unwrap(),
            CourseKind::try_new("course").unwrap(),
            None,
            None,
            &db,
        )
        .await
        .unwrap();
        let note = CourseNote::create(
            course.get_id(),
            &creator,
            CourseNoteTitle::try_new("a").unwrap(),
            CourseNoteContent::try_new("body").unwrap(),
            &db,
        )
        .await
        .unwrap();
        // What a handler snapshot would have seen...
        let early = CourseNoteFile::new(
            note.get_id(),
            FileName::try_new("early.pdf").unwrap(),
            FileContentType::try_new("application/pdf").unwrap(),
            3,
        )
        .insert(&db)
        .await
        .unwrap();
        let (snapshot, _) = CourseNoteFile::list_for(note.get_id(), None, 0, &db)
            .await
            .unwrap();
        assert_eq!(snapshot.len(), 1);
        // ...and the upload that races in after it.
        let late = CourseNoteFile::new(
            note.get_id(),
            FileName::try_new("late.pdf").unwrap(),
            FileContentType::try_new("application/pdf").unwrap(),
            3,
        )
        .insert(&db)
        .await
        .unwrap();

        let (gone, files) = note.delete(&db).await.unwrap();
        assert_eq!(gone.get_title().as_str(), "a");
        let mut keys: Vec<_> = files
            .iter()
            .map(|file| file.get_id().key().to_string())
            .collect();
        keys.sort();
        let mut want = vec![
            early.get_id().key().to_string(),
            late.get_id().key().to_string(),
        ];
        want.sort();
        assert_eq!(keys, want);
        assert!(
            CourseNoteFile::list_for(gone.get_id(), None, 0, &db)
                .await
                .unwrap()
                .0
                .is_empty()
        );
    }
}
