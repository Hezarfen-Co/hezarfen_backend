//! The `rag_output` table: what an AI service produced for a course note —
//! an index, a summary, an embedding manifest — stored as this side's own
//! copy of the answer. The AI service never writes here (it answers a
//! request over the bridge, and this side stores the answer), which is what
//! keeps the api-read bridge GET-only. Derived, disposable data: the row is
//! dropped and regenerated whenever its input changes, so the cascades
//! below — [`delete_for_note`] when the note goes, [`delete_with_source`]
//! when one of the attachments it was built from goes — keep a stale output
//! from outliving its input, even in a deployment with no AI service
//! connected. The row shape lives in [`crate::domain::rag_output`].

use crate::database::Database;
use crate::db::page::PagedList;
use crate::domain::course::CourseId;
use crate::domain::course_note::CourseNoteId;
use crate::domain::course_note_file::CourseNoteFileId;
use crate::domain::rag_output::{RagOutput, RagOutputId};
use crate::domain::timestamp::Timestamp;
use crate::error::AppError;
use serde_json::Value;

/// Store one service output against `note`.
pub async fn create(
    db: &Database,
    note: &CourseNoteId,
    course: &CourseId,
    sources: Vec<CourseNoteFileId>,
    payload: Value,
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

pub async fn read(db: &Database, id: &RagOutputId) -> Result<Option<RagOutput>, AppError> {
    Ok(db.select(id.record()).await?)
}

/// A note's outputs, newest first.
pub async fn list_for(
    db: &Database,
    note: &CourseNoteId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<RagOutput>, i64), AppError> {
    PagedList::new("rag_output WHERE course_note = $note", "ORDER BY id DESC")
        .bind("note", note.record())
        .run(limit, offset, db)
        .await
}

pub async fn delete(db: &Database, id: &RagOutputId) -> Result<RagOutput, AppError> {
    let deleted: Option<RagOutput> = db.delete(id.record()).await?;
    deleted.ok_or(AppError::NotFound)
}

/// Cascade: every output of `note`. Deleting none is a success — a note
/// no service ever indexed has nothing to drop.
pub async fn delete_for_note(db: &Database, note: &CourseNoteId) -> Result<(), AppError> {
    db.query("DELETE rag_output WHERE course_note = $note")
        .bind(("note", note.record()))
        .await?
        .check()?;
    Ok(())
}

/// Newest-wins replace, without a lock: store `payload` first, then drop
/// this note's older rows. Two concurrent index tasks can interleave in any
/// order and still leave exactly one row — the newest — because ids come
/// from the process-wide monotonic generator, so "older" is `id <` the row
/// just written and the loser's row is always below the winner's.
pub async fn replace_for_note(
    db: &Database,
    note: &CourseNoteId,
    course: &CourseId,
    sources: Vec<CourseNoteFileId>,
    payload: Value,
) -> Result<RagOutput, AppError> {
    let created = create(db, note, course, sources, payload).await?;
    db.query("DELETE rag_output WHERE course_note = $note AND id < $new")
        .bind(("note", note.record()))
        .bind(("new", created.id.record()))
        .await?
        .check()?;
    Ok(created)
}

/// Cascade: every output built from `file`. Runs on a file delete even
/// with no AI service connected, so a stale output cannot survive its
/// source.
pub async fn delete_with_source(db: &Database, file: &CourseNoteFileId) -> Result<(), AppError> {
    db.query("DELETE rag_output WHERE sources CONTAINS $file")
        .bind(("file", file.record()))
        .await?
        .check()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::course::{CourseDescription, CourseKind, CourseTitle};
    use crate::domain::course_note::{CourseNote, CourseNoteContent, CourseNoteTitle};
    use crate::domain::course_note_file::{CourseNoteFile, FileContentType, FileName};
    use serde_json::json;

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

    /// Replace converges on one row — the newest — even when an earlier
    /// interleaving already left two rows behind for the note.
    #[tokio::test]
    async fn replace_keeps_only_the_newest_output() {
        let db = crate::database::init_mem().await.unwrap();
        let note = note_of(&db, "a").await;
        let stale = create(
            &db,
            note.get_id(),
            note.get_course(),
            Vec::new(),
            json!({ "n": 1 }),
        )
        .await
        .unwrap();
        // The race this fixes: a second row for the same note.
        create(
            &db,
            note.get_id(),
            note.get_course(),
            Vec::new(),
            json!({ "n": 2 }),
        )
        .await
        .unwrap();

        let fresh = replace_for_note(
            &db,
            note.get_id(),
            note.get_course(),
            Vec::new(),
            json!({ "n": 3 }),
        )
        .await
        .unwrap();

        let rows = list_for(&db, note.get_id(), None, 0).await.unwrap().0;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_id(), fresh.get_id());
        assert!(fresh.get_id().key() > stale.get_id().key());
    }

    /// The payload survives the round trip unread, and both cascades take only
    /// what they are aimed at.
    #[tokio::test]
    async fn outputs_round_trip_and_cascade() {
        let db = crate::database::init_mem().await.unwrap();
        let note = note_of(&db, "a").await;
        let other = note_of(&db, "b").await;
        let file = file_on(&db, note.get_id()).await;

        let stored = create(
            &db,
            note.get_id(),
            note.get_course(),
            vec![file.clone()],
            json!({ "summary": "x", "chunks": [{ "text": "y" }] }),
        )
        .await
        .unwrap();
        let roundtrip = read(&db, stored.get_id()).await.unwrap().unwrap();
        assert_eq!(roundtrip.get_payload()["summary"], "x");
        assert_eq!(roundtrip.get_payload()["chunks"][0]["text"], "y");
        assert_eq!(roundtrip.get_sources(), std::slice::from_ref(&file));
        assert_eq!(roundtrip.get_course(), note.get_course());
        let untouched = create(
            &db,
            other.get_id(),
            other.get_course(),
            Vec::new(),
            json!({ "summary": "z" }),
        )
        .await
        .unwrap();

        let listed =
            async |note: &CourseNoteId| list_for(&db, note, None, 0).await.unwrap().0.len();
        assert_eq!(listed(note.get_id()).await, 1);

        // Losing a source drops the output that cited it, and nothing else.
        delete_with_source(&db, &file).await.unwrap();
        assert_eq!(listed(note.get_id()).await, 0);
        assert_eq!(listed(other.get_id()).await, 1);

        // Cascading a note with no outputs left is still a success.
        delete_for_note(&db, note.get_id()).await.unwrap();
        delete_for_note(&db, other.get_id()).await.unwrap();
        assert!(read(&db, untouched.get_id()).await.unwrap().is_none());
    }
}
