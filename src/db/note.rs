//! The `note` table: one user's rows, listed newest first, deleted together
//! with their attachment rows.

use crate::database::{Database, tx_with_retry};
use crate::db::field_update::FieldUpdate;
use crate::db::page::PagedList;
use crate::domain::note::{Note, NoteContent, NoteId, NoteTitle};
use crate::domain::note_file::{FileContentType, FileName, NoteFile, NoteFileId};
use crate::domain::user::UserId;
use crate::error::AppError;

pub async fn create(
    db: &Database,
    owner: &UserId,
    title: NoteTitle,
    content: NoteContent,
) -> Result<Note, AppError> {
    let note = Note {
        id: NoteId::generate(),
        user: owner.clone(),
        title,
        content,
    };
    // whole-row-save-ok: insert of a fresh UUID row built in place by `new` — there is no prior row to clobber
    let created = sqlx::query_as!(
        Note,
        r#"INSERT INTO note (id, app_user, title, content)
           VALUES ($1, $2, $3, $4)
           RETURNING id AS "id: NoteId", app_user AS "user: UserId",
               title AS "title: NoteTitle", content AS "content: NoteContent""#,
        note.id.uuid(),
        note.user.uuid(),
        note.title.as_str(),
        note.content.as_str()
    )
    .fetch_one(db)
    .await?;
    Ok(created)
}

/// Read a note only if it belongs to `owner`.
pub async fn read_owned(
    db: &Database,
    id: &NoteId,
    owner: &UserId,
) -> Result<Option<Note>, AppError> {
    let note = sqlx::query_as!(
        Note,
        r#"SELECT id AS "id: NoteId", app_user AS "user: UserId",
               title AS "title: NoteTitle", content AS "content: NoteContent" FROM note WHERE id = $1 AND app_user = $2"#,
        id.uuid(),
        owner.uuid()
    )
    .fetch_optional(db)
    .await?;
    Ok(note)
}

pub async fn list_for(
    db: &Database,
    owner: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Note>, i64), AppError> {
    PagedList::new("note WHERE app_user = $1", "ORDER BY id DESC")
        .bind(owner.uuid())
        .run::<Note>(limit, offset, db)
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
    note: Note,
    title: Option<NoteTitle>,
    content: Option<NoteContent>,
) -> Result<Note, AppError> {
    FieldUpdate::new("note", note.id.uuid())
        .set("title", title.map(|title| title.as_str().to_string()))
        .set(
            "content",
            content.map(|content| content.as_str().to_string()),
        )
        .run::<Note>(db)
        .await
}

/// Delete the note and cascade-remove its attachment rows, returning both:
/// the note, and the attachment rows this transaction actually removed.
/// Blob files on disk are the web layer's to remove, but only for *these*
/// rows — a row uploaded after the caller listed the note's files is
/// deleted here too, and a pre-read snapshot would strand its blob. A crash
/// between commit and unlink leaves at worst an unreachable blob, never a
/// row pointing at nothing.
///
/// Children first, in one transaction, the way
/// [`crate::db::course::delete`] does it: as two queries, an
/// upload that committed in between kept its row while the note went, and
/// nothing could ever list or delete it again. The `rag_output` sweep rides
/// here too: under real foreign keys a `rag_output` row citing this note
/// would refuse the delete (`ON DELETE NO ACTION`), and the derived rows
/// are exactly the thing that may never outlive its input — the web layer's
/// separate `delete_for_note` call stays, as a harmless idempotent repeat.
pub async fn delete(db: &Database, note: Note) -> Result<(Note, Vec<NoteFile>), AppError> {
    tx_with_retry(db, true, async move |conn| {
        sqlx::query!("DELETE FROM rag_output WHERE course_note = $1", note.id.uuid())
            .execute(&mut *conn)
            .await?;
        let files = sqlx::query_as!(
            NoteFile,
            r#"DELETE FROM note_file WHERE note = $1
               RETURNING id AS "id: NoteFileId", note AS "note: NoteId", name AS "name: FileName",
                     content_type AS "content_type: FileContentType", size"#,
            note.id.uuid()
        )
        .fetch_all(&mut *conn)
        .await?;
        let gone = sqlx::query_as!(
            Note,
            r#"DELETE FROM note WHERE id = $1 RETURNING id AS "id: NoteId", app_user AS "user: UserId",
               title AS "title: NoteTitle", content AS "content: NoteContent""#,
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
    use crate::domain::note_file::{FileContentType, FileName};

    /// The blobs the handler unlinks are exactly the rows this transaction
    /// removed — including one uploaded after any pre-read snapshot would have
    /// been taken, which is the row whose blob used to leak.
    #[tokio::test]
    async fn delete_returns_the_attachment_rows_it_removed() {
        let (db, _leases) = crate::database::init_test_db().await;
        let owner = UserId::generate();
        let note = create(
            &db,
            &owner,
            NoteTitle::try_new("a").unwrap(),
            NoteContent::try_new("body").unwrap(),
        )
        .await
        .unwrap();
        // What a handler snapshot would have seen...
        let early = crate::db::note_file::insert(
            &db,
            NoteFile::new(
                note.get_id(),
                FileName::try_new("early.pdf").unwrap(),
                FileContentType::try_new("application/pdf").unwrap(),
                3,
            ),
        )
        .await
        .unwrap();
        let (snapshot, _) = crate::db::note_file::list_for(&db, note.get_id(), None, 0)
            .await
            .unwrap();
        assert_eq!(snapshot.len(), 1);
        // ...and the upload that races in after it.
        let late = crate::db::note_file::insert(
            &db,
            NoteFile::new(
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
            crate::db::note_file::list_for(&db, gone.get_id(), None, 0)
                .await
                .unwrap()
                .0
                .is_empty()
        );
    }
}
