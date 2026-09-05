//! What an AI service produced for a course note — an index, a summary, an
//! embedding manifest: the backend does not read into it. The row is the
//! backend's own copy of that output; the AI service never writes here (it
//! answers a request over the bridge, and this side stores the answer), which
//! is what keeps the api-read bridge GET-only.
//!
//! Derived data, so it is disposable: the note is the source of truth and a
//! row here can be dropped and regenerated at any time. It therefore cascades
//! from both sides of what it was built from — [`Self::delete_for_note`] when
//! the note goes, [`Self::delete_with_source`] when one of the attachments it
//! was built from goes — so a stale output never outlives its input, even in a
//! deployment with no AI service connected.

use serde_json::Value;
use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::RAG_OUTPUT_TABLE;
use crate::database::Database;
use crate::domain::course::CourseId;
use crate::domain::course_note::CourseNoteId;
use crate::domain::course_note_file::CourseNoteFileId;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::page::PagedList;
use crate::domain::timestamp::Timestamp;
use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct RagOutputId(RecordId);

impl RagOutputId {
    /// Minted from the process-wide monotonic generator, not `Ulid::new()`:
    /// a note's outputs list `id DESC` (newest first, [`RagOutput::list_for`]).
    pub fn generate() -> Self {
        Self(RecordId::new(RAG_OUTPUT_TABLE, next_ulid().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(RAG_OUTPUT_TABLE, key))
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
pub struct RagOutput {
    id: RagOutputId,
    course_note: CourseNoteId,
    /// The note's course, denormalised so a course-wide read needs no join.
    course: CourseId,
    /// The attachments the output was built from, as they stood at generation
    /// time. Deleting any one of them drops this row
    /// ([`Self::delete_with_source`]) rather than leaving an output citing a
    /// file that no longer exists.
    sources: Vec<CourseNoteFileId>,
    /// The service's answer, stored verbatim. Opaque to the backend — it is a
    /// service-owned shape, so this side neither validates nor interprets it,
    /// beyond it having to be a JSON **object** (the stored column is one).
    payload: Value,
    generated_at: Timestamp,
}

impl RagOutput {
    pub fn get_id(&self) -> &RagOutputId {
        &self.id
    }

    pub fn get_course_note(&self) -> &CourseNoteId {
        &self.course_note
    }

    pub fn get_course(&self) -> &CourseId {
        &self.course
    }

    pub fn get_sources(&self) -> &[CourseNoteFileId] {
        &self.sources
    }

    pub fn get_payload(&self) -> &Value {
        &self.payload
    }

    pub fn get_generated_at(&self) -> Timestamp {
        self.generated_at
    }

    /// Store one service output against `note`.
    pub async fn create(
        note: &CourseNoteId,
        course: &CourseId,
        sources: Vec<CourseNoteFileId>,
        payload: Value,
        db: &Database,
    ) -> Result<RagOutput, AppError> {
        let row = RagOutput {
            id: RagOutputId::generate(),
            course_note: note.clone(),
            course: course.clone(),
            sources,
            payload,
            generated_at: Timestamp::now(),
        };
        // whole-row-save-ok: create of a fresh ULID row built in place — there is no prior row to clobber
        let created: Option<RagOutput> = db.create(row.id.record()).content(row).await?;
        created.ok_or_else(|| AppError::Internal("failed to create rag output".into()))
    }

    pub async fn read(id: &RagOutputId, db: &Database) -> Result<Option<RagOutput>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// A note's outputs, newest first.
    pub async fn list_for(
        note: &CourseNoteId,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<RagOutput>, i64), AppError> {
        PagedList::new("rag_output WHERE course_note = $note", "ORDER BY id DESC")
            .bind("note", note.record())
            .run(limit, offset, db)
            .await
    }

    pub async fn delete(id: &RagOutputId, db: &Database) -> Result<RagOutput, AppError> {
        let deleted: Option<RagOutput> = db.delete(id.record()).await?;
        deleted.ok_or(AppError::NotFound)
    }

    /// Cascade: every output of `note`. Deleting none is a success — a note
    /// no service ever indexed has nothing to drop.
    pub async fn delete_for_note(note: &CourseNoteId, db: &Database) -> Result<(), AppError> {
        db.query("DELETE rag_output WHERE course_note = $note")
            .bind(("note", note.record()))
            .await?
            .check()?;
        Ok(())
    }

    /// Cascade: every output built from `file`. Runs on a file delete even
    /// with no AI service connected, so a stale output cannot survive its
    /// source.
    pub async fn delete_with_source(
        file: &CourseNoteFileId,
        db: &Database,
    ) -> Result<(), AppError> {
        db.query("DELETE rag_output WHERE $file IN sources")
            .bind(("file", file.record()))
            .await?
            .check()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::course::{Course, CourseDescription, CourseKind, CourseTitle};
    use crate::domain::course_note::{CourseNote, CourseNoteContent, CourseNoteTitle};
    use crate::domain::course_note_file::{CourseNoteFile, FileContentType, FileName};
    use serde_json::json;

    async fn note_of(db: &Database, title: &str) -> CourseNote {
        let creator = crate::domain::user::UserId::generate();
        let course = Course::create(
            &creator,
            CourseTitle::try_new("c").unwrap(),
            CourseDescription::try_new("").unwrap(),
            CourseKind::try_new("course").unwrap(),
            None,
            None,
            db,
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

    async fn file_on(db: &Database, note: &CourseNoteId) -> CourseNoteFileId {
        CourseNoteFile::new(
            note,
            FileName::try_new("plan.pdf").unwrap(),
            FileContentType::try_new("application/pdf").unwrap(),
            3,
        )
        .insert(db)
        .await
        .unwrap()
        .get_id()
        .clone()
    }

    /// The payload survives the round trip unread, and both cascades take only
    /// what they are aimed at.
    #[tokio::test]
    async fn outputs_round_trip_and_cascade() {
        let db = crate::database::init_mem().await.unwrap();
        let note = note_of(&db, "a").await;
        let other = note_of(&db, "b").await;
        let file = file_on(&db, note.get_id()).await;

        let stored = RagOutput::create(
            note.get_id(),
            note.get_course(),
            vec![file.clone()],
            json!({ "summary": "x", "chunks": [{ "text": "y" }] }),
            &db,
        )
        .await
        .unwrap();
        let read = RagOutput::read(stored.get_id(), &db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read.get_payload()["summary"], "x");
        assert_eq!(read.get_payload()["chunks"][0]["text"], "y");
        assert_eq!(read.get_sources(), &[file.clone()]);
        assert_eq!(read.get_course(), note.get_course());

        let untouched = RagOutput::create(
            other.get_id(),
            other.get_course(),
            Vec::new(),
            json!({ "summary": "z" }),
            &db,
        )
        .await
        .unwrap();

        let listed = async |note: &CourseNoteId| {
            RagOutput::list_for(note, None, 0, &db)
                .await
                .unwrap()
                .0
                .len()
        };
        assert_eq!(listed(note.get_id()).await, 1);

        // Losing a source drops the output that cited it, and nothing else.
        RagOutput::delete_with_source(&file, &db).await.unwrap();
        assert_eq!(listed(note.get_id()).await, 0);
        assert_eq!(listed(other.get_id()).await, 1);

        // Cascading a note with no outputs left is still a success.
        RagOutput::delete_for_note(note.get_id(), &db)
            .await
            .unwrap();
        RagOutput::delete_for_note(other.get_id(), &db)
            .await
            .unwrap();
        assert!(
            RagOutput::read(untouched.get_id(), &db)
                .await
                .unwrap()
                .is_none()
        );
    }
}
