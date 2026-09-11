//! The `note_file` table: attachment rows for a personal note, listed newest
//! first, with the note's file cap claimed and released in the same write as
//! the row.

use crate::constant::{MAX_NOTE_FILES, NOTE_FILE_COUNT_FIELD};
use crate::database::Database;
use crate::db::cap;
use crate::db::page::PagedList;
use crate::domain::note::NoteId;
use crate::domain::note_file::{NoteFile, NoteFileId};
use crate::error::AppError;

/// Persist the row assembled by [`NoteFile::new`], refusing once its note
/// already holds [`MAX_NOTE_FILES`]. The slot and the row are taken together
/// by [`cap::claim_and_create`] on the note row: a `BEGIN…COMMIT` around a
/// count can't enforce the cap (SurrealDB doesn't conflict-check a
/// cross-record count against a concurrent insert) and a process-wide mutex
/// can't either, since it is released around the very round trip the insert
/// races — a conditional single-record write can. Claiming in a *separate*
/// query would enforce the cap but leak a slot on a crash between the two.
pub async fn insert(db: &Database, file: NoteFile) -> Result<NoteFile, AppError> {
    // whole-row-save-ok: create of a fresh ULID row built in place by `new` — there is no prior row to clobber
    match cap::claim_and_create(
        &file.note.record(),
        NOTE_FILE_COUNT_FIELD,
        MAX_NOTE_FILES as i64,
        &file.id.record(),
        &file,
        db,
    )
    .await?
    {
        cap::Claimed::Made(created) => Ok(created),
        // Also how a missing note reads: no note row means no slot to take.
        cap::Claimed::Full => Err(AppError::Conflict(
            "the note already holds the maximum of 10 files — delete one first",
        )),
        // Unreachable: the id is a ULID this call just generated.
        cap::Claimed::Duplicate => Err(AppError::Internal("failed to create note file".into())),
    }
}

/// Read a file's row only if it belongs to `note` — callers have already
/// checked the note belongs to the requesting user.
pub async fn read_for(
    db: &Database,
    id: &NoteFileId,
    note: &NoteId,
) -> Result<Option<NoteFile>, AppError> {
    let file: Option<NoteFile> = db.select(id.record()).await?;
    Ok(file.filter(|file| &file.note == note))
}

/// All of `note`'s attachment rows, newest first.
pub async fn list_for(
    db: &Database,
    note: &NoteId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<NoteFile>, i64), AppError> {
    PagedList::new("note_file WHERE note = $note", "ORDER BY id DESC")
        .bind("note", note.record())
        .run(limit, offset, db)
        .await
}

/// Delete the row and give its slot back in the same transaction — the note
/// itself is untouched, so unlike the note-delete cascade this one has a
/// counter to correct. (Deleting a *note* takes its counter with it.)
pub async fn delete(db: &Database, file: NoteFile) -> Result<NoteFile, AppError> {
    let mut result = db
        .query(
            "BEGIN TRANSACTION;
             LET $gone = (DELETE $id RETURN BEFORE);
             UPDATE $note SET file_count = math::max([(file_count ?? 0) - array::len($gone), 0]);
             RETURN $gone;
             COMMIT TRANSACTION;",
        )
        .bind(("id", file.id.record()))
        .bind(("note", file.note.record()))
        .await?
        .check()?;
    result
        .take::<Vec<NoteFile>>(3)?
        .into_iter()
        .next()
        .ok_or(AppError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::note_file::{FileContentType, FileName};

    #[tokio::test]
    async fn rows_scope_to_their_note() {
        let db = crate::database::init_mem().await.unwrap();
        // Real note rows: the cap counter lives on the note, so an insert whose
        // note does not exist has no slot to take (a 409, like a full note).
        let owner = crate::domain::user::UserId::generate();
        let note_of = async |title: &str| {
            crate::db::note::create(
                &db,
                &owner,
                crate::domain::note::NoteTitle::try_new(title).unwrap(),
                crate::domain::note::NoteContent::try_new("body").unwrap(),
            )
            .await
            .unwrap()
        };
        let note_a = note_of("a").await.get_id().clone();
        let note_b = note_of("b").await.get_id().clone();
        let file = insert(
            &db,
            NoteFile::new(
                &note_a,
                FileName::try_new("plan.pdf").unwrap(),
                FileContentType::try_new("application/pdf").unwrap(),
                3,
            ),
        )
        .await
        .unwrap();

        // Readable under its own note, invisible under another.
        let found = read_for(&db, file.get_id(), &note_a).await.unwrap();
        assert_eq!(found.unwrap().get_name().as_str(), "plan.pdf");
        assert!(
            read_for(&db, file.get_id(), &note_b)
                .await
                .unwrap()
                .is_none()
        );

        let listed = async |note: &NoteId| list_for(&db, note, None, 0).await.unwrap().0;
        assert_eq!(listed(&note_a).await.len(), 1);
        assert!(listed(&note_b).await.is_empty());

        delete(&db, file).await.unwrap();
        assert!(listed(&note_a).await.is_empty());
    }

    /// The counter and the rows are written in one transaction, so the stored
    /// count must equal the stored rows — after a success *and* after the
    /// refusal that fills the cap, which must move neither.
    #[tokio::test]
    async fn counter_tracks_stored_rows() {
        let db = crate::database::init_mem().await.unwrap();
        let note = crate::db::note::create(
            &db,
            &crate::domain::user::UserId::generate(),
            crate::domain::note::NoteTitle::try_new("a").unwrap(),
            crate::domain::note::NoteContent::try_new("body").unwrap(),
        )
        .await
        .unwrap()
        .get_id()
        .clone();
        let stored_count = async |note: &NoteId| -> i64 {
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
        let add = async |note: &NoteId| {
            insert(
                &db,
                NoteFile::new(
                    note,
                    FileName::try_new("plan.pdf").unwrap(),
                    FileContentType::try_new("application/pdf").unwrap(),
                    3,
                ),
            )
            .await
        };

        for filled in 1..=MAX_NOTE_FILES {
            add(&note).await.unwrap();
            assert_eq!(stored_count(&note).await, filled as i64);
            assert_eq!(
                list_for(&db, &note, None, 0).await.unwrap().1,
                filled as i64
            );
        }

        // At the cap: the refusal writes nothing, counter included.
        assert!(matches!(add(&note).await, Err(AppError::Conflict(_))));
        assert_eq!(stored_count(&note).await, MAX_NOTE_FILES as i64);
        assert_eq!(
            list_for(&db, &note, None, 0).await.unwrap().1,
            MAX_NOTE_FILES as i64
        );
    }
}
