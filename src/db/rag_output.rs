//! What an AI service produced for a course note, stored on this side: the
//! write paths the api-read bridge serves (`create`/`replace_for_note` run
//! when this backend answers a request over the bridge, and this side stores
//! the answer), which is what keeps the api-read bridge GET-only. Derived,
//! disposable data: the row is dropped and regenerated whenever its input
//! changes, so the cascades below — [`delete_for_note`] when the note goes,
//! [`delete_with_source`] when one of the attachments it was built from goes
//! — keep a stale output from outliving its input, even in a deployment with
//! no AI service connected. The row shape lives in
//! [`crate::domain::rag_output`].

use crate::database::Database;
use crate::db::page::PagedList;
use crate::domain::course::CourseId;
use crate::domain::course_note::CourseNoteId;
use crate::domain::course_note_file::CourseNoteFileId;
use crate::domain::rag_output::{RagOutput, RagOutputId};
use crate::domain::timestamp::Timestamp;
use crate::error::AppError;
use serde_json::Value;
use sqlx::types::Json;

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
        payload: sqlx::types::Json(payload),
        generated_at: Timestamp::now(),
    };
    // whole-row-save-ok: insert of a fresh UUID row built in place — there is no prior row to clobber
    let created = sqlx::query_as!(
        RagOutput,
        r#"INSERT INTO rag_output (id, course_note, course, sources, payload, generated_at)
           VALUES ($1, $2, $3, $4, $5, $6)
           RETURNING id AS "id: RagOutputId", course_note AS "course_note: CourseNoteId",
                     course AS "course: CourseId",
                     sources AS "sources: Vec<CourseNoteFileId>",
                     payload AS "payload: Json<Value>",
                     generated_at AS "generated_at: Timestamp""#,
        row.id.uuid(),
        row.course_note.uuid(),
        row.course.uuid(),
        &row.sources
            .iter()
            .map(CourseNoteFileId::uuid)
            .collect::<Vec<uuid::Uuid>>(),
        row.payload.0,
        row.generated_at.as_millis()
    )
    .fetch_one(db)
    .await?;
    Ok(created)
}

pub async fn read(db: &Database, id: &RagOutputId) -> Result<Option<RagOutput>, AppError> {
    let row = sqlx::query_as!(
        RagOutput,
        r#"SELECT id AS "id: RagOutputId", course_note AS "course_note: CourseNoteId",
                  course AS "course: CourseId",
                  sources AS "sources: Vec<CourseNoteFileId>",
                  payload AS "payload: Json<Value>",
                  generated_at AS "generated_at: Timestamp"
           FROM rag_output WHERE id = $1"#,
        id.uuid()
    )
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// A note's outputs, newest first.
pub async fn list_for(
    db: &Database,
    note: &CourseNoteId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<RagOutput>, i64), AppError> {
    PagedList::new("rag_output WHERE course_note = $1", "ORDER BY id DESC")
        .bind(note.uuid())
        .run::<RagOutput>(limit, offset, db)
        .await
}

pub async fn delete(db: &Database, id: &RagOutputId) -> Result<RagOutput, AppError> {
    let deleted = sqlx::query_as!(
        RagOutput,
        r#"DELETE FROM rag_output WHERE id = $1
           RETURNING id AS "id: RagOutputId", course_note AS "course_note: CourseNoteId",
                     course AS "course: CourseId",
                     sources AS "sources: Vec<CourseNoteFileId>",
                     payload AS "payload: Json<Value>",
                     generated_at AS "generated_at: Timestamp""#,
        id.uuid()
    )
    .fetch_optional(db)
    .await?;
    deleted.ok_or(AppError::NotFound)
}

/// Cascade: every output of `note`. Deleting none is a success — a note
/// no service ever indexed has nothing to drop.
pub async fn delete_for_note(db: &Database, note: &CourseNoteId) -> Result<(), AppError> {
    sqlx::query!("DELETE FROM rag_output WHERE course_note = $1", note.uuid())
        .execute(db)
        .await?;
    Ok(())
}

/// Newest-wins replace, without a lock: store `payload` first, then drop
/// this note's older rows. Two concurrent index tasks can interleave in any
/// order and still leave exactly one row — the newest — because ids come
/// from the process-wide monotonic generator, so "older" is `id <` the row
/// just written and the loser's row is always below the winner's. (The
/// uuid column compares in byte order, which for UUIDv7 is mint order —
/// the same property the old record ids got from the ULID.)
pub async fn replace_for_note(
    db: &Database,
    note: &CourseNoteId,
    course: &CourseId,
    sources: Vec<CourseNoteFileId>,
    payload: Value,
) -> Result<RagOutput, AppError> {
    let created = create(db, note, course, sources, payload).await?;
    sqlx::query!(
        "DELETE FROM rag_output WHERE course_note = $1 AND id < $2",
        note.uuid(),
        created.id.uuid()
    )
    .execute(db)
    .await?;
    Ok(created)
}

/// Cascade: every output built from `file`. Runs on a file delete even
/// with no AI service connected, so a stale output cannot survive its
/// source. The GIN index on `sources` turns the containment test into a
/// lookup.
pub async fn delete_with_source(db: &Database, file: &CourseNoteFileId) -> Result<(), AppError> {
    sqlx::query!(
        "DELETE FROM rag_output WHERE $1 = ANY(sources)",
        file.uuid()
    )
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::course::{CourseDescription, CourseKind, CourseTitle};
    use crate::domain::course_note::CourseNote;
    use crate::domain::course_note::CourseNoteContent;
    use crate::domain::course_note::CourseNoteTitle;
    use crate::domain::course_note_file::CourseNoteFile;
    use crate::domain::course_note_file::FileContentType;
    use crate::domain::course_note_file::FileName;
    use serde_json::json;

    /// A real `app_user` row: the course's creator is a foreign key now.
    async fn a_person(db: &Database) -> crate::domain::user::UserId {
        let user = crate::domain::user::UserId::generate();
        sqlx::query("INSERT INTO app_user (id, username, created_at) VALUES ($1, $2, 0)")
            .bind(user.uuid())
            .bind(format!("rag-{}", &user.key()[30..]))
            .execute(db)
            .await
            .unwrap();
        user
    }

    async fn note_of(db: &Database, title: &str) -> CourseNote {
        // The course's creator is a foreign key now: a real `app_user` row.
        let creator = a_person(db).await;
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

    async fn a_file(db: &Database, note: &CourseNote) -> CourseNoteFile {
        crate::db::course_note_file::insert(
            db,
            CourseNoteFile::new(
                note.get_id(),
                FileName::try_new("f").unwrap(),
                FileContentType::try_new("text/plain").unwrap(),
                1,
            ),
        )
        .await
        .unwrap()
    }

    /// A stored round-trips through JSONB with its sources list intact, and
    /// the note cascade takes every row of the note with it.
    #[tokio::test]
    async fn create_reads_back_and_cascades_from_the_note() {
        let (db, _leases) = crate::database::init_test_db().await;
        let note = note_of(&db, "n").await;
        let file = a_file(&db, &note).await;
        let row = create(
            &db,
            note.get_id(),
            note.get_course(),
            vec![file.get_id().clone()],
            json!({"answer": "x"}),
        )
        .await
        .unwrap();
        let read_back = read(&db, row.get_id()).await.unwrap().unwrap();
        assert_eq!(read_back.get_payload(), &json!({"answer": "x"}));
        assert_eq!(read_back.get_sources(), &[file.get_id().clone()]);

        delete_for_note(&db, note.get_id()).await.unwrap();
        assert!(read(&db, row.get_id()).await.unwrap().is_none());
    }

    /// Replace leaves exactly one row — the newest — and the source cascade
    /// drops a row built from a file that is gone.
    #[tokio::test]
    async fn replace_is_newest_wins_and_source_cascade_drops() {
        let (db, _leases) = crate::database::init_test_db().await;
        let note = note_of(&db, "n").await;
        let file = a_file(&db, &note).await;
        let first = create(
            &db,
            note.get_id(),
            note.get_course(),
            vec![file.get_id().clone()],
            json!({"v": 1}),
        )
        .await
        .unwrap();
        let second = replace_for_note(
            &db,
            note.get_id(),
            note.get_course(),
            vec![file.get_id().clone()],
            json!({"v": 2}),
        )
        .await
        .unwrap();
        let (rows, total) = list_for(&db, note.get_id(), None, 0).await.unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].get_id(), second.get_id());
        assert!(first.get_id() != second.get_id());

        delete_with_source(&db, file.get_id()).await.unwrap();
        let (_, total) = list_for(&db, note.get_id(), None, 0).await.unwrap();
        assert_eq!(total, 0);
    }

    /// Deleting one row returns it; deleting it again is a 404.
    #[tokio::test]
    async fn delete_returns_the_row_then_refuses() {
        let (db, _leases) = crate::database::init_test_db().await;
        let note = note_of(&db, "n").await;
        let row = create(&db, note.get_id(), note.get_course(), Vec::new(), json!({}))
            .await
            .unwrap();
        let gone = delete(&db, row.get_id()).await.unwrap();
        assert_eq!(gone.get_id(), row.get_id());
        assert!(matches!(
            delete(&db, row.get_id()).await,
            Err(AppError::NotFound)
        ));
    }
}
