//! The `course_note_file` table: attachment rows for a course note, listed
//! newest first, with the note's file cap claimed and released in the same
//! write as the row.

use surrealdb::types::{RecordId, RecordIdKey};

use crate::constant::{COURSE_NOTE_FILE_COUNT_FIELD, MAX_COURSE_NOTE_FILES};
use crate::database::Database;
use crate::db::cap;
use crate::db::page::PagedList;
use crate::domain::course_note::CourseNoteId;
use crate::domain::course_note_file::{CourseNoteFile, CourseNoteFileId};
use crate::error::AppError;

/// Persist the row assembled by [`CourseNoteFile::new`], refusing once its
/// note already holds [`MAX_COURSE_NOTE_FILES`]. The slot and the row are
/// taken together by [`cap::claim_and_create`] on the note row — see
/// [`crate::db::note_file::insert`] for why that is the only guard a
/// concurrent insert cannot outrun.
pub async fn insert(db: &Database, file: CourseNoteFile) -> Result<CourseNoteFile, AppError> {
    // whole-row-save-ok: create of a fresh ULID row built in place by `new` — there is no prior row to clobber
    match cap::claim_and_create(
        &file.course_note.record(),
        COURSE_NOTE_FILE_COUNT_FIELD,
        MAX_COURSE_NOTE_FILES as i64,
        &file.id.record(),
        &file,
        db,
    )
    .await?
    {
        cap::Claimed::Made(created) => Ok(created),
        // Also how a missing note reads: no note row means no slot to take.
        cap::Claimed::Full => Err(AppError::Conflict(
            "the course note already holds the maximum of 10 files — delete one first",
        )),
        // Unreachable: the id is a ULID this call just generated.
        cap::Claimed::Duplicate => Err(AppError::Internal(
            "failed to create course note file".into(),
        )),
    }
}

/// Read a file's row by id alone, for the callers that have no note in
/// hand yet and reach the note *through* the file (the AI bridge's blob
/// stream). A key from any other table simply reads as `None`, which is
/// what keeps a personal note's file id unreachable here.
pub async fn read(
    db: &Database,
    id: &CourseNoteFileId,
) -> Result<Option<CourseNoteFile>, AppError> {
    Ok(db.select(id.record()).await?)
}

/// Read a file's row only if it belongs to `note` — callers have already
/// checked the note belongs to a course the caller may act on.
pub async fn read_for(
    db: &Database,
    id: &CourseNoteFileId,
    note: &CourseNoteId,
) -> Result<Option<CourseNoteFile>, AppError> {
    let file: Option<CourseNoteFile> = db.select(id.record()).await?;
    Ok(file.filter(|file| &file.course_note == note))
}

/// All of `note`'s attachment rows, newest first.
pub async fn list_for(
    db: &Database,
    note: &CourseNoteId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<CourseNoteFile>, i64), AppError> {
    PagedList::new(
        "course_note_file WHERE course_note = $note",
        "ORDER BY id DESC",
    )
    .bind("note", note.record())
    .run(limit, offset, db)
    .await
}

/// The blob names (record keys, since a file's blob is named by its own
/// id) behind every note attachment of `course` — collected *before* the
/// course-delete cascade wipes the rows.
pub async fn file_keys_for_course(
    db: &Database,
    course: &crate::domain::course::CourseId,
) -> Result<Vec<String>, AppError> {
    let mut result = db
        .query(
            "SELECT VALUE id FROM course_note_file \
             WHERE course_note IN (SELECT VALUE id FROM course_note WHERE course = $course)",
        )
        .bind(("course", course.record()))
        .await?
        .check()?;
    Ok(result
        .take::<Vec<RecordId>>(0)?
        .into_iter()
        .map(|id| match id.key {
            RecordIdKey::String(key) => key,
            _ => String::new(),
        })
        .collect())
}

/// Delete the row and give its slot back in the same transaction — the
/// note itself is untouched, so unlike the note-delete cascade this one
/// has a counter to correct.
pub async fn delete(db: &Database, file: CourseNoteFile) -> Result<CourseNoteFile, AppError> {
    let mut result = db
        .query(
            "BEGIN TRANSACTION;
             LET $gone = (DELETE $id RETURN BEFORE);
             UPDATE $note SET file_count = math::max([(file_count ?? 0) - array::len($gone), 0]);
             RETURN $gone;
             COMMIT TRANSACTION;",
        )
        .bind(("id", file.id.record()))
        .bind(("note", file.course_note.record()))
        .await?
        .check()?;
    result
        .take::<Vec<CourseNoteFile>>(3)?
        .into_iter()
        .next()
        .ok_or(AppError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::domain::course::{CourseDescription, CourseKind, CourseTitle};
    use crate::domain::course_note::{CourseNote, CourseNoteContent, CourseNoteTitle};
    use crate::domain::course_note_file::{FileContentType, FileName};

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

    #[tokio::test]
    async fn rows_scope_to_their_note() {
        let db = crate::database::init_mem().await.unwrap();
        let note_a = note_of(&db, "a").await.get_id().clone();
        let note_b = note_of(&db, "b").await.get_id().clone();
        let file = insert(
            &db,
            CourseNoteFile::new(
                &note_a,
                FileName::try_new("plan.pdf").unwrap(),
                FileContentType::try_new("application/pdf").unwrap(),
                3,
            ),
        )
        .await
        .unwrap();

        let found = read_for(&db, file.get_id(), &note_a).await.unwrap();
        assert_eq!(found.unwrap().get_name().as_str(), "plan.pdf");
        assert!(
            read_for(&db, file.get_id(), &note_b)
                .await
                .unwrap()
                .is_none()
        );

        let listed = async |note: &CourseNoteId| list_for(&db, note, None, 0).await.unwrap().0;
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
        let note = note_of(&db, "a").await.get_id().clone();
        let stored_count = async |note: &CourseNoteId| -> i64 {
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
        let add = async |note: &CourseNoteId| {
            insert(
                &db,
                CourseNoteFile::new(
                    note,
                    FileName::try_new("plan.pdf").unwrap(),
                    FileContentType::try_new("application/pdf").unwrap(),
                    3,
                ),
            )
            .await
        };

        for filled in 1..=MAX_COURSE_NOTE_FILES {
            add(&note).await.unwrap();
            assert_eq!(stored_count(&note).await, filled as i64);
            assert_eq!(
                list_for(&db, &note, None, 0).await.unwrap().1,
                filled as i64
            );
        }

        assert!(matches!(add(&note).await, Err(AppError::Conflict(_))));
        assert_eq!(stored_count(&note).await, MAX_COURSE_NOTE_FILES as i64);
        assert_eq!(
            list_for(&db, &note, None, 0).await.unwrap().1,
            MAX_COURSE_NOTE_FILES as i64
        );
    }
}
