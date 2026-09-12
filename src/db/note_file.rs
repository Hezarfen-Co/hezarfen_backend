//! The `note_file` table: attachment rows for a personal note, listed newest
//! first, with the note's file cap claimed and released in the same write as
//! the row.

use crate::constant::MAX_NOTE_FILES;
use crate::database::{Database, tx_with_retry, unique_violation};
use crate::db::page::PagedList;
use crate::domain::note::NoteId;
use crate::domain::note_file::{FileContentType, FileName, NoteFile, NoteFileId};
use crate::error::AppError;

/// Persist the row assembled by [`NoteFile::new`], refusing once its note
/// already holds [`MAX_NOTE_FILES`]. The slot and the row are taken together
/// by one conditional statement on the note row (the claim CTE of
/// `cap::claim_and_create`, spelled out at its call site — see
/// `crate::db::cap`): a `BEGIN…COMMIT` around a count can't enforce the cap
/// and a process-wide mutex can't either, since it is released around the
/// very round trip the insert races — a single guarded statement can.
/// Claiming in a *separate* query would enforce the cap but leak a slot on
/// a crash between the two.
pub async fn insert(db: &Database, file: NoteFile) -> Result<NoteFile, AppError> {
    // whole-row-save-ok: insert of a fresh UUID row built in place by `new` — there is no prior row to clobber
    let created = sqlx::query_as!(
        NoteFile,
        r#"WITH seat AS (
               UPDATE note SET file_count = file_count + 1
               WHERE id = $1 AND file_count < $2
               RETURNING 1)
           INSERT INTO note_file (id, note, name, content_type, size)
           SELECT $3, $1, $4, $5, $6 WHERE EXISTS (SELECT 1 FROM seat)
           RETURNING id AS "id: NoteFileId", note AS "note: NoteId", name AS "name: FileName",
               content_type AS "content_type: FileContentType", size"#,
        file.note.uuid(),
        MAX_NOTE_FILES as i64,
        file.id.uuid(),
        file.name.as_str(),
        file.content_type.as_str(),
        file.size
    )
    .fetch_optional(db)
    .await;
    match created {
        Ok(Some(row)) => Ok(row),
        // Also how a missing note reads: no note row means no slot to take.
        Ok(None) => Err(AppError::Conflict(
            "the note already holds the maximum of 10 files — delete one first",
        )),
        // A duplicate would be a 23505 on the row's own key, which this path
        // cannot produce — the id is a UUID this call just generated.
        Err(err) if unique_violation(&err).is_some() => {
            Err(AppError::Internal("failed to create note file".into()))
        }
        Err(err) => Err(err.into()),
    }
}

/// Read a file's row only if it belongs to `note` — callers have already
/// checked the note belongs to the requesting user.
pub async fn read_for(
    db: &Database,
    id: &NoteFileId,
    note: &NoteId,
) -> Result<Option<NoteFile>, AppError> {
    let file = sqlx::query_as!(
        NoteFile,
        r#"SELECT id AS "id: NoteFileId", note AS "note: NoteId", name AS "name: FileName",
               content_type AS "content_type: FileContentType", size FROM note_file WHERE id = $1 AND note = $2"#,
        id.uuid(),
        note.uuid()
    )
    .fetch_optional(db)
    .await?;
    Ok(file)
}

/// All of `note`'s attachment rows, newest first.
pub async fn list_for(
    db: &Database,
    note: &NoteId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<NoteFile>, i64), AppError> {
    PagedList::new("note_file WHERE note = $1", "ORDER BY id DESC")
        .bind(note.uuid())
        .run::<NoteFile>(limit, offset, db)
        .await
}

/// Delete the row and give its slot back in the same transaction — the note
/// itself is untouched, so unlike the note-delete cascade this one has a
/// counter to correct. (Deleting a *note* takes its counter with it.)
pub async fn delete(db: &Database, file: NoteFile) -> Result<NoteFile, AppError> {
    tx_with_retry(db, true, async move |conn| {
        let gone = sqlx::query_as!(
            NoteFile,
            r#"DELETE FROM note_file WHERE id = $1 RETURNING id AS "id: NoteFileId", note AS "note: NoteId", name AS "name: FileName",
               content_type AS "content_type: FileContentType", size"#,
            file.id.uuid()
        )
        .fetch_optional(&mut *conn)
        .await?;
        let file = gone.ok_or(AppError::NotFound)?;
        sqlx::query!(
            "UPDATE note SET file_count = GREATEST(file_count - 1, 0) WHERE id = $1",
            file.note.uuid()
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
    use crate::domain::note::NoteContent;
    use crate::domain::note::NoteTitle;
    use crate::domain::note_file::FileContentType;
    use crate::domain::note_file::FileName;

    async fn a_note(db: &Database) -> Note {
        let owner = crate::domain::user::UserId::generate();
        crate::db::note::create(
            db,
            &owner,
            NoteTitle::try_new("n").unwrap(),
            NoteContent::try_new("b").unwrap(),
        )
        .await
        .unwrap()
    }

    /// The cap answers with a conflict, and neither the refused row nor its
    /// slot was written: the count is still what the last accepted upload
    /// left.
    #[tokio::test]
    async fn insert_refuses_at_the_cap_without_leaking_a_slot() {
        let db = crate::database::init_mem().await.unwrap();
        let note = a_note(&db).await;
        for index in 0..MAX_NOTE_FILES {
            let file = NoteFile::new(
                note.get_id(),
                FileName::try_new(&format!("f{index}")).unwrap(),
                FileContentType::try_new("text/plain").unwrap(),
                1,
            );
            insert(&db, file).await.unwrap();
        }
        let over = NoteFile::new(
            note.get_id(),
            FileName::try_new("over").unwrap(),
            FileContentType::try_new("text/plain").unwrap(),
            1,
        );
        assert!(matches!(
            insert(&db, over).await,
            Err(AppError::Conflict(_))
        ));
        let (files, total) = list_for(&db, note.get_id(), None, 0).await.unwrap();
        assert_eq!(total, MAX_NOTE_FILES as i64);
        assert_eq!(files.len(), MAX_NOTE_FILES);
    }

    /// Deleting a row hands its slot back: after one delete a new upload is
    /// accepted again, and the count never dips below zero.
    #[tokio::test]
    async fn delete_returns_the_slot_to_the_note() {
        let db = crate::database::init_mem().await.unwrap();
        let note = a_note(&db).await;
        let file = NoteFile::new(
            note.get_id(),
            FileName::try_new("f").unwrap(),
            FileContentType::try_new("text/plain").unwrap(),
            1,
        );
        let file = insert(&db, file).await.unwrap();
        let gone = delete(&db, file.clone()).await.unwrap();
        assert_eq!(gone.get_id(), file.get_id());
        // Deleting it a second time is a 404, not a second decrement.
        assert!(matches!(delete(&db, file).await, Err(AppError::NotFound)));
        let next = NoteFile::new(
            note.get_id(),
            FileName::try_new("next").unwrap(),
            FileContentType::try_new("text/plain").unwrap(),
            1,
        );
        insert(&db, next).await.unwrap();
        let (_, total) = list_for(&db, note.get_id(), None, 0).await.unwrap();
        assert_eq!(total, 1);
    }
}
