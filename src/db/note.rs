//! The `note` table: one user's rows, listed newest first, deleted together
//! with their attachment rows.

use surrealdb::types::SurrealValue;

use crate::database::Database;
use crate::db::field_update::FieldUpdate;
use crate::db::page::PagedList;
use crate::domain::note::{Note, NoteContent, NoteId, NoteTitle};
use crate::domain::note_file::NoteFile;
use crate::domain::user::UserId;
use crate::error::AppError;

/// What [`delete`]'s transaction removed: the note row (empty if it had
/// already vanished) and every attachment row the cascade took with it — the
/// only set whose blobs are safe to unlink.
#[derive(Debug, SurrealValue)]
struct DeleteOutcome {
    note: Vec<Note>,
    files: Vec<NoteFile>,
}

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
    let created: Option<Note> = db.create(note.id.record()).content(note).await?;
    created.ok_or_else(|| AppError::Internal("failed to create note".into()))
}

/// Read a note only if it belongs to `owner`.
pub async fn read_owned(
    db: &Database,
    id: &NoteId,
    owner: &UserId,
) -> Result<Option<Note>, AppError> {
    let note: Option<Note> = db.select(id.record()).await?;
    Ok(note.filter(|note| &note.user == owner))
}

pub async fn list_for(
    db: &Database,
    owner: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Note>, i64), AppError> {
    PagedList::new("note WHERE user = $usr", "ORDER BY id DESC")
        .bind("usr", owner.record())
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
    note: Note,
    title: Option<NoteTitle>,
    content: Option<NoteContent>,
) -> Result<Note, AppError> {
    FieldUpdate::new(note.id.record())
        .set("title", title)
        .set("content", content)
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
/// nothing could ever list or delete it again.
pub async fn delete(db: &Database, note: Note) -> Result<(Note, Vec<NoteFile>), AppError> {
    let mut result = db
        .query(
            "BEGIN TRANSACTION;
             LET $files = (DELETE note_file WHERE note = $note RETURN BEFORE);
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
        .ok_or_else(|| AppError::Internal("failed to delete note".into()))?;
    let note = outcome.note.into_iter().next().ok_or(AppError::NotFound)?;
    Ok((note, outcome.files))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The blobs the handler unlinks are exactly the rows this transaction
    /// removed — including one uploaded after any pre-read snapshot would have
    /// been taken, which is the row whose blob used to leak.
    #[tokio::test]
    async fn delete_returns_the_attachment_rows_it_removed() {
        use crate::domain::note_file::{FileContentType, FileName};

        let db = crate::database::init_mem().await.unwrap();
        let owner = crate::domain::user::UserId::generate();
        let note = create(
            &db,
            &owner,
            NoteTitle::try_new("a").unwrap(),
            NoteContent::try_new("body").unwrap(),
        )
        .await
        .unwrap();
        // What a handler snapshot would have seen...
        let early = NoteFile::new(
            note.get_id(),
            FileName::try_new("early.pdf").unwrap(),
            FileContentType::try_new("application/pdf").unwrap(),
            3,
        )
        .insert(&db)
        .await
        .unwrap();
        let (snapshot, _) = NoteFile::list_for(note.get_id(), None, 0, &db)
            .await
            .unwrap();
        assert_eq!(snapshot.len(), 1);
        // ...and the upload that races in after it.
        let late = NoteFile::new(
            note.get_id(),
            FileName::try_new("late.pdf").unwrap(),
            FileContentType::try_new("application/pdf").unwrap(),
            3,
        )
        .insert(&db)
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
            NoteFile::list_for(gone.get_id(), None, 0, &db)
                .await
                .unwrap()
                .0
                .is_empty()
        );
    }
}
