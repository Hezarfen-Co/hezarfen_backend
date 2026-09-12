//! The `course_note_file` table: attachment rows for a course note, listed
//! newest first, with the note's file cap claimed and released in the same
//! write as the row.

use crate::constant::MAX_COURSE_NOTE_FILES;
use crate::database::{Database, tx_with_retry, unique_violation};
use crate::db::page::PagedList;
use crate::domain::course::CourseId;
use crate::domain::course_note::CourseNoteId;
use crate::domain::course_note_file::{CourseNoteFile, CourseNoteFileId};
use crate::error::AppError;

/// Persist the row assembled by [`CourseNoteFile::new`], refusing once its
/// note already holds [`MAX_COURSE_NOTE_FILES`]. The slot and the row are
/// taken together by one conditional statement on the note row (the claim
/// CTE of `cap::claim_and_create`, spelled out at its call site — see
/// `crate::db::cap`) — see [`crate::db::note_file::insert`] for why that is
/// the only guard a concurrent insert cannot outrun.
pub async fn insert(db: &Database, file: CourseNoteFile) -> Result<CourseNoteFile, AppError> {
    // whole-row-save-ok: insert of a fresh UUID row built in place by `new` — there is no prior row to clobber
    let created = sqlx::query_as!(
        CourseNoteFile,
        r#"WITH seat AS (
               UPDATE course_note SET file_count = file_count + 1
               WHERE id = $1 AND file_count < $2
               RETURNING 1)
           INSERT INTO course_note_file (id, course_note, name, content_type, size)
           SELECT $3, $1, $4, $5, $6 WHERE EXISTS (SELECT 1 FROM seat)
           RETURNING id, course_note, name, content_type, size"#,
        file.course_note,
        MAX_COURSE_NOTE_FILES as i64,
        file.id,
        file.name,
        file.content_type,
        file.size
    )
    .fetch_optional(db)
    .await;
    match created {
        Ok(Some(row)) => Ok(row),
        // Also how a missing note reads: no note row means no slot to take.
        Ok(None) => Err(AppError::Conflict(
            "the course note already holds the maximum of 10 files — delete one first",
        )),
        // A duplicate would be a 23505 on the row's own key, which this path
        // cannot produce — the id is a UUID this call just generated.
        Err(err) if unique_violation(&err).is_some() => Err(AppError::Internal(
            "failed to create course note file".into(),
        )),
        Err(err) => Err(err.into()),
    }
}

/// Read a file's row by id alone, for the callers that have no note in
/// hand yet and reach the note *through* the file (the AI bridge's blob
/// stream). An id from any other table simply reads as `None`, which is
/// what keeps a personal note's file id unreachable here.
pub async fn read(
    db: &Database,
    id: &CourseNoteFileId,
) -> Result<Option<CourseNoteFile>, AppError> {
    let file = sqlx::query_as!(
        CourseNoteFile,
        r#"SELECT id, course_note, name, content_type, size FROM course_note_file WHERE id = $1"#,
        id
    )
    .fetch_optional(db)
    .await?;
    Ok(file)
}

/// Read a file's row only if it belongs to `note` — callers have already
/// checked the note belongs to a course the caller may act on.
pub async fn read_for(
    db: &Database,
    id: &CourseNoteFileId,
    note: &CourseNoteId,
) -> Result<Option<CourseNoteFile>, AppError> {
    let file = sqlx::query_as!(
        CourseNoteFile,
        r#"SELECT id, course_note, name, content_type, size FROM course_note_file
           WHERE id = $1 AND course_note = $2"#,
        id,
        note
    )
    .fetch_optional(db)
    .await?;
    Ok(file)
}

/// All of `note`'s attachment rows, newest first.
pub async fn list_for(
    db: &Database,
    note: &CourseNoteId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<CourseNoteFile>, i64), AppError> {
    PagedList::new(
        "course_note_file WHERE course_note = $1",
        "ORDER BY id DESC",
    )
    .bind(note.uuid())
    .run::<CourseNoteFile>(limit, offset, db)
    .await
}

/// The blob names (the rows' own ids, since a file's blob is named by its
/// id) behind every note attachment of `course` — collected *before* the
/// course-delete cascade wipes the rows.
pub async fn file_keys_for_course(
    db: &Database,
    course: &CourseId,
) -> Result<Vec<String>, AppError> {
    let rows = sqlx::query!(
        r#"SELECT course_note_file.id AS "id: CourseNoteFileId" FROM course_note_file
           WHERE course_note IN (SELECT id FROM course_note WHERE course = $1)"#,
        course.0
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|row| row.id.key()).collect())
}

/// Delete the row and give its slot back in the same transaction — the
/// note itself is untouched, so unlike the note-delete cascade this one
/// has a counter to correct.
pub async fn delete(db: &Database, file: CourseNoteFile) -> Result<CourseNoteFile, AppError> {
    tx_with_retry(db, true, async |conn| {
        let gone = sqlx::query_as!(
            CourseNoteFile,
            r#"DELETE FROM course_note_file WHERE id = $1
               RETURNING id, course_note, name, content_type, size"#,
            file.id
        )
        .fetch_optional(&mut *conn)
        .await?;
        let file = gone.ok_or(AppError::NotFound)?;
        sqlx::query!(
            "UPDATE course_note SET file_count = GREATEST(file_count - 1, 0) WHERE id = $1",
            file.course_note
        )
        .execute(&mut *conn)
        .await?;
        Ok(file)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::course::{CourseDescription, CourseKind, CourseTitle};
    use crate::domain::course_note::{CourseNoteContent, CourseNoteTitle};
    use crate::domain::course_note_file::FileContentType;
    use crate::domain::course_note_file::FileName;

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
        crate::db::course_note::create(
            db,
            course.get_id(),
            &creator,
            CourseNoteTitle::try_new(title).unwrap(),
            CourseNoteContent::try_new("body").unwrap(),
        )
        .await
        .unwrap()
    }

    /// The cap answers with a conflict and the refused upload wrote nothing:
    /// the count stays at the accepted rows.
    #[tokio::test]
    async fn insert_refuses_at_the_cap() {
        let db = crate::database::init_mem().await.unwrap();
        let note = note_of(&db, "n").await;
        for index in 0..MAX_COURSE_NOTE_FILES {
            let file = CourseNoteFile::new(
                note.get_id(),
                FileName::try_new(&format!("f{index}")).unwrap(),
                FileContentType::try_new("text/plain").unwrap(),
                1,
            );
            insert(&db, file).await.unwrap();
        }
        let over = CourseNoteFile::new(
            note.get_id(),
            FileName::try_new("over").unwrap(),
            FileContentType::try_new("text/plain").unwrap(),
            1,
        );
        assert!(matches!(
            insert(&db, over).await,
            Err(AppError::Conflict(_))
        ));
        let (_, total) = list_for(&db, note.get_id(), None, 0).await.unwrap();
        assert_eq!(total, MAX_COURSE_NOTE_FILES as i64);
    }

    /// The blob-key collection sees attachments through the course, and a
    /// delete hands its slot back to the note.
    #[tokio::test]
    async fn file_keys_and_delete_slot_release() {
        let db = crate::database::init_mem().await.unwrap();
        let note = note_of(&db, "n").await;
        let course = note.get_course().clone();
        let file = CourseNoteFile::new(
            note.get_id(),
            FileName::try_new("f").unwrap(),
            FileContentType::try_new("text/plain").unwrap(),
            1,
        );
        let file = insert(&db, file).await.unwrap();
        let keys = file_keys_for_course(&db, &course).await.unwrap();
        assert_eq!(keys, vec![file.get_id().key().to_string()]);
        let gone = delete(&db, file.clone()).await.unwrap();
        assert_eq!(gone.get_id(), file.get_id());
        assert!(matches!(delete(&db, file).await, Err(AppError::NotFound)));
        assert!(file_keys_for_course(&db, &course).await.unwrap().is_empty());
    }
}
