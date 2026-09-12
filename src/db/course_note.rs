//! The `course_note` table: a course's shared notes, newest first, deleted
//! together with their attachment rows.

use crate::database::{Database, foreign_key_violation, tx_with_retry};
use crate::db::field_update::FieldUpdate;
use crate::db::page::PagedList;
use crate::domain::course::CourseId;
use crate::domain::course_note::{CourseNote, CourseNoteContent, CourseNoteId, CourseNoteTitle};
use crate::domain::course_note_file::{
    CourseNoteFile, CourseNoteFileId, FileContentType, FileName,
};
use crate::domain::user::UserId;
use crate::error::AppError;

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
    // whole-row-save-ok: insert of a fresh UUID row built in place — there is no prior row to clobber.
    // The course row is *referenced* (a real foreign key the store now
    // enforces), so a note that races `Course::delete`'s cascade is refused
    // as 23503 instead of outliving its course — a note whose course is gone
    // is unreachable forever, every route to it going through the course.
    // That refusal is the parent-gone 404 the touch trick used to produce.
    let created = sqlx::query_as!(
        CourseNote,
        r#"INSERT INTO course_note (id, course, author, title, content)
           VALUES ($1, $2, $3, $4, $5)
           RETURNING id AS "id: CourseNoteId", course AS "course: CourseId",
               author AS "author: UserId", title AS "title: CourseNoteTitle",
               content AS "content: CourseNoteContent""#,
        note.id.uuid(),
        note.course.uuid(),
        note.author.uuid(),
        note.title.as_str(),
        note.content.as_str()
    )
    .fetch_one(db)
    .await;
    match created {
        Ok(row) => Ok(row),
        Err(err) if foreign_key_violation(&err) => Err(AppError::NotFound),
        Err(err) => Err(err.into()),
    }
}

pub async fn read(db: &Database, id: &CourseNoteId) -> Result<Option<CourseNote>, AppError> {
    let note = sqlx::query_as!(
        CourseNote,
        r#"SELECT id AS "id: CourseNoteId", course AS "course: CourseId",
               author AS "author: UserId", title AS "title: CourseNoteTitle",
               content AS "content: CourseNoteContent" FROM course_note WHERE id = $1"#,
        id.uuid()
    )
    .fetch_optional(db)
    .await?;
    Ok(note)
}

pub async fn list_for_course(
    db: &Database,
    course: &CourseId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<CourseNote>, i64), AppError> {
    PagedList::new("course_note WHERE course = $1", "ORDER BY id DESC")
        .bind(course.uuid())
        .run::<CourseNote>(limit, offset, db)
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
    FieldUpdate::new("course_note", note.id.uuid())
        .set("title", title.map(|title| title.as_str().to_string()))
        .set(
            "content",
            content.map(|content| content.as_str().to_string()),
        )
        .run::<CourseNote>(db)
        .await
}

/// Delete the note and cascade-remove its attachment rows, returning both:
/// the note, and the attachment rows this transaction actually removed.
/// Blob files on disk are the web layer's to remove, but only for *these*
/// rows — a row uploaded after the caller listed the note's files is
/// deleted here too, and a pre-read snapshot would strand its blob.
///
/// The `rag_output` sweep rides in the same transaction: `rag_output`
/// references this table through a real foreign key (`ON DELETE NO
/// ACTION`), so a derived row left behind would refuse the delete — and a
/// derived row outliving its note is exactly what the cascade exists to
/// prevent. The web layer's separate `delete_for_note` call stays, as a
/// harmless idempotent repeat.
pub async fn delete(
    db: &Database,
    note: CourseNote,
) -> Result<(CourseNote, Vec<CourseNoteFile>), AppError> {
    tx_with_retry(db, true, async move |conn| {
        sqlx::query!("DELETE FROM rag_output WHERE course_note = $1", note.id.uuid())
            .execute(&mut *conn)
            .await?;
        let files = sqlx::query_as!(
            CourseNoteFile,
            r#"DELETE FROM course_note_file WHERE course_note = $1
               RETURNING id AS "id: CourseNoteFileId",
                     course_note AS "course_note: CourseNoteId", name AS "name: FileName",
                     content_type AS "content_type: FileContentType", size"#,
            note.id.uuid()
        )
        .fetch_all(&mut *conn)
        .await?;
        let gone = sqlx::query_as!(
            CourseNote,
            r#"DELETE FROM course_note WHERE id = $1 RETURNING id AS "id: CourseNoteId", course AS "course: CourseId",
               author AS "author: UserId", title AS "title: CourseNoteTitle",
               content AS "content: CourseNoteContent""#,
            note.id.uuid()
        )
        .fetch_optional(&mut *conn)
        .await?;
        let note = gone.ok_or(AppError::NotFound)?;
        Ok((note, files))
    })
    .await
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
        let (db, _leases) = crate::database::init_test_db().await;
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
