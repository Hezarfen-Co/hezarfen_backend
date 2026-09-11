//! The `course_note` table: a teacher-authored note attached to a course,
//! listed newest first, deleted together with its attachment rows.

use surrealdb::types::SurrealValue;

use crate::constant::ENROLLMENT_COUNT_FIELD;
use crate::database::Database;
use crate::db::cap;
use crate::db::field_update::FieldUpdate;
use crate::db::page::PagedList;
use crate::domain::course::CourseId;
use crate::domain::course_note::{CourseNote, CourseNoteContent, CourseNoteId, CourseNoteTitle};
use crate::domain::course_note_file::CourseNoteFile;
use crate::domain::user::UserId;
use crate::error::AppError;

/// What [`delete`]'s transaction removed: the note row (empty if it had
/// already vanished) and every attachment row the cascade took with it — the
/// only set whose blobs are safe to unlink.
#[derive(Debug, SurrealValue)]
struct DeleteOutcome {
    note: Vec<CourseNote>,
    files: Vec<CourseNoteFile>,
}

pub async fn create(
    db: &Database,
    course: &CourseId,
    author: &UserId,
    title: CourseNoteTitle,
    content: CourseNoteContent,
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

pub async fn read(db: &Database, id: &CourseNoteId) -> Result<Option<CourseNote>, AppError> {
    Ok(db.select(id.record()).await?)
}

pub async fn list_for_course(
    db: &Database,
    course: &CourseId,
    limit: Option<i64>,
    offset: i64,
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
    db: &Database,
    note: CourseNote,
    title: Option<CourseNoteTitle>,
    content: Option<CourseNoteContent>,
) -> Result<CourseNote, AppError> {
    FieldUpdate::new(note.id.record())
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
    db: &Database,
    note: CourseNote,
) -> Result<(CourseNote, Vec<CourseNoteFile>), AppError> {
    let mut result = db
        .query(
            "BEGIN TRANSACTION;
             LET $files = (DELETE course_note_file WHERE course_note = $note RETURN BEFORE);
             LET $gone = (DELETE $note RETURN BEFORE);
             RETURN { note: $gone, files: $files };
             COMMIT TRANSACTION;",
        )
        .bind(("note", note.id.record()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::course::{CourseDescription, CourseKind, CourseTitle};
    use crate::domain::course_note_file::{FileContentType, FileName};

    /// The blobs the handler unlinks are exactly the rows this transaction
    /// removed — including one uploaded after any pre-read snapshot would have
    /// been taken, which is the row whose blob used to leak.
    #[tokio::test]
    async fn delete_returns_the_attachment_rows_it_removed() {
        let db = crate::database::init_mem().await.unwrap();
        let creator = crate::domain::user::UserId::generate();
        let course = crate::db::course::create(
            &db,
            &creator,
            CourseTitle::try_new("Math").unwrap(),
            CourseDescription::try_new("").unwrap(),
            CourseKind::try_new("course").unwrap(),
            None,
            None,
        )
        .await
        .unwrap();
        let note = create(
            &db,
            course.get_id(),
            &creator,
            CourseNoteTitle::try_new("a").unwrap(),
            CourseNoteContent::try_new("body").unwrap(),
        )
        .await
        .unwrap();
        // What a handler snapshot would have seen...
        let early = crate::db::course_note_file::insert(
            &db,
            CourseNoteFile::new(
                note.get_id(),
                FileName::try_new("early.pdf").unwrap(),
                FileContentType::try_new("application/pdf").unwrap(),
                3,
            ),
        )
        .await
        .unwrap();
        let (snapshot, _) = crate::db::course_note_file::list_for(&db, note.get_id(), None, 0)
            .await
            .unwrap();
        assert_eq!(snapshot.len(), 1);
        // ...and the upload that races in after it.
        let late = crate::db::course_note_file::insert(
            &db,
            CourseNoteFile::new(
                note.get_id(),
                FileName::try_new("late.pdf").unwrap(),
                FileContentType::try_new("application/pdf").unwrap(),
                3,
            ),
        )
        .await
        .unwrap();

        let (gone, files) = delete(&db, note).await.unwrap();
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
            crate::db::course_note_file::list_for(&db, gone.get_id(), None, 0)
                .await
                .unwrap()
                .0
                .is_empty()
        );
    }
}
