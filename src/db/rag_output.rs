//! What an AI service produced for a course note, stored on this side: the
//! write paths the api-read bridge serves (`create`/`replace_for_note` run
//! when this backend answers a request over the bridge, and this side stores
//! the answer), which is what keeps the api-read bridge GET-only. Derived,
//! disposable data: the row is dropped and regenerated whenever its input
//! changes, so the cascades below — [`delete_for_note`] when the note goes,
//! [`delete_with_source`] dropping the citation links a file delete strands
//! — keep the stored shape from outliving its input, even in a deployment
//! with no AI service connected. The row shape lives in
//! [`crate::domain::rag_output`].

use std::collections::HashMap;

use crate::database::{Database, tx_with_retry};
use crate::db::page::PagedList;
use crate::domain::course::CourseId;
use crate::domain::course_note::CourseNoteId;
use crate::domain::course_note_file::CourseNoteFileId;
use crate::domain::rag_output::{RagOutput, RagOutputId};
use crate::domain::timestamp::Timestamp;
use crate::error::AppError;
use serde_json::Value;
use sqlx::types::Json;
use uuid::Uuid;

/// The plain `rag_output` columns. A citation list is not a column on the
/// row — the links live in `rag_output_source`, and the `query!` macros
/// cannot join an aggregate into a row — so every read decodes this shape
/// and one batch junction statement supplies the links that
/// [`RagOutputRow::into_rag_output`](RagOutputRow::into_rag_output) attaches.
#[derive(Debug, sqlx::FromRow)]
struct RagOutputRow {
    id: RagOutputId,
    course_note: CourseNoteId,
    course: CourseId,
    payload: Json<Value>,
    generated_at: Timestamp,
}

impl RagOutputRow {
    fn into_rag_output(self, sources: Vec<CourseNoteFileId>) -> RagOutput {
        RagOutput {
            id: self.id,
            course_note: self.course_note,
            course: self.course,
            sources,
            payload: self.payload,
            generated_at: self.generated_at,
        }
    }
}

/// The citation links of many outputs in one statement, grouped by output
/// uuid (the domain id newtype is not `Hash`), every list in `source` order —
/// the one order all reads rebuild, so a create's answer and any later read
/// hand back equal `Vec`s.
async fn sources_for_many(
    db: &Database,
    outputs: &[RagOutputId],
) -> Result<HashMap<Uuid, Vec<CourseNoteFileId>>, AppError> {
    let keys: Vec<Uuid> = outputs.iter().map(|output| output.uuid()).collect();
    let links = sqlx::query!(
        r#"SELECT output, source AS "source: CourseNoteFileId"
           FROM rag_output_source WHERE output = ANY($1) ORDER BY source"#,
        &keys
    )
    .fetch_all(db)
    .await?;
    let mut grouped: HashMap<Uuid, Vec<CourseNoteFileId>> = HashMap::new();
    for link in links {
        grouped.entry(link.output).or_default().push(link.source);
    }
    Ok(grouped)
}

/// The citation links of one output, in `source` order.
async fn sources_for(
    db: &Database,
    output: &RagOutputId,
) -> Result<Vec<CourseNoteFileId>, AppError> {
    let mut grouped = sources_for_many(db, std::slice::from_ref(output)).await?;
    Ok(grouped.remove(&output.uuid()).unwrap_or_default())
}

pub async fn create(
    db: &Database,
    note: &CourseNoteId,
    course: &CourseId,
    mut sources: Vec<CourseNoteFileId>,
    payload: Value,
) -> Result<RagOutput, AppError> {
    // Link rows carry no order of their own; the source id is the order.
    // Sorting here makes `create`'s answer equal to any later read's.
    sources.sort_by_key(|file| file.uuid());
    let keys: Vec<Uuid> = sources.iter().map(|file| file.uuid()).collect();
    let id = RagOutputId::generate();
    let generated_at = Timestamp::now();
    // Owned binds: capturing `&CourseNoteId` / `&CourseId` across the
    // `tx_with_retry` await poisons Send at every caller (ai/rag, course_notes).
    let id_u = id.uuid();
    let note_u = note.uuid();
    let course_u = course.uuid();
    let at = generated_at.as_millis();
    let row = tx_with_retry(db, false, async move |tx| {
        let created = sqlx::query_as!(
            RagOutputRow,
            r#"INSERT INTO rag_output (id, course_note, course, payload, generated_at)
               VALUES ($1, $2, $3, $4, $5)
               RETURNING id AS "id: RagOutputId", course_note AS "course_note: CourseNoteId",
                         course AS "course: CourseId",
                         payload AS "payload: Json<Value>",
                         generated_at AS "generated_at: Timestamp""#,
            id_u,
            note_u,
            course_u,
            &payload,
            at
        )
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query!(
            "INSERT INTO rag_output_source (output, source)
             SELECT $1, s FROM unnest($2::uuid[]) AS t(s)",
            created.id.uuid(),
            &keys
        )
        .execute(&mut *tx)
        .await?;
        Ok(created)
    })
    .await?;
    Ok(row.into_rag_output(sources))
}

pub async fn read(db: &Database, id: &RagOutputId) -> Result<Option<RagOutput>, AppError> {
    let row = sqlx::query_as!(
        RagOutputRow,
        r#"SELECT id AS "id: RagOutputId", course_note AS "course_note: CourseNoteId",
                  course AS "course: CourseId",
                  payload AS "payload: Json<Value>",
                  generated_at AS "generated_at: Timestamp"
           FROM rag_output WHERE id = $1"#,
        id.uuid()
    )
    .fetch_optional(db)
    .await?;
    match row {
        Some(row) => {
            let sources = sources_for(db, &row.id).await?;
            Ok(Some(row.into_rag_output(sources)))
        }
        None => Ok(None),
    }
}

/// A note's outputs, newest first.
pub async fn list_for(
    db: &Database,
    note: &CourseNoteId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<RagOutput>, i64), AppError> {
    let (rows, total) = PagedList::new("rag_output WHERE course_note = $1", "ORDER BY id DESC")
        .bind(note.uuid())
        .run::<RagOutputRow>(limit, offset, db)
        .await?;
    let ids: Vec<RagOutputId> = rows.iter().map(|row| row.id).collect();
    let grouped = sources_for_many(db, &ids).await?;
    let items = rows
        .into_iter()
        .map(|row| {
            let sources = grouped.get(&row.id.uuid()).cloned().unwrap_or_default();
            row.into_rag_output(sources)
        })
        .collect();
    Ok((items, total))
}

pub async fn delete(db: &Database, id: &RagOutputId) -> Result<RagOutput, AppError> {
    let id_u = id.uuid();
    let (row, sources) = tx_with_retry(db, false, async move |tx| {
        let sources = sqlx::query!(
            r#"SELECT source AS "source: CourseNoteFileId"
               FROM rag_output_source WHERE output = $1 ORDER BY source"#,
            id_u
        )
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(|link| link.source)
        .collect::<Vec<_>>();
        sqlx::query!("DELETE FROM rag_output_source WHERE output = $1", id_u)
            .execute(&mut *tx)
            .await?;
        sqlx::query_as!(
            RagOutputRow,
            r#"DELETE FROM rag_output WHERE id = $1
               RETURNING id AS "id: RagOutputId", course_note AS "course_note: CourseNoteId",
                         course AS "course: CourseId",
                         payload AS "payload: Json<Value>",
                         generated_at AS "generated_at: Timestamp""#,
            id_u
        )
        .fetch_optional(&mut *tx)
        .await?
        .map(|row| (row, sources))
        .ok_or(AppError::NotFound)
    })
    .await?;
    Ok(row.into_rag_output(sources))
}

/// Cascade: every output of `note`. Deleting none is a success — a note
/// no service ever indexed has nothing to drop. The links go before the
/// rows: they are the rows' children (`ON DELETE NO ACTION`).
pub async fn delete_for_note(db: &Database, note: &CourseNoteId) -> Result<(), AppError> {
    sqlx::query!(
        "DELETE FROM rag_output_source WHERE output IN
         (SELECT id FROM rag_output WHERE course_note = $1)",
        note.uuid()
    )
    .execute(db)
    .await?;
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
    // The superseded rows' links go first (`ON DELETE NO ACTION`), then the
    // rows themselves. Neither delete can touch the row just written: its id
    // is above every id the generator has minted before it.
    sqlx::query!(
        "DELETE FROM rag_output_source WHERE output IN
         (SELECT id FROM rag_output WHERE course_note = $1 AND id < $2)",
        note.uuid(),
        created.id.uuid()
    )
    .execute(db)
    .await?;
    sqlx::query!(
        "DELETE FROM rag_output WHERE course_note = $1 AND id < $2",
        note.uuid(),
        created.id.uuid()
    )
    .execute(db)
    .await?;
    Ok(created)
}

/// Cascade: the citation links that point at `file`. Runs on a file delete
/// even with no AI service connected — `ON DELETE NO ACTION` refuses the
/// file row while any link still cites it. An output a link belonged to
/// stops being true the moment its source is gone; the re-index a file
/// delete triggers regenerates it from what is left, and until then it
/// reads as a shorter — possibly empty — citation list.
pub async fn delete_with_source(db: &Database, file: &CourseNoteFileId) -> Result<(), AppError> {
    sqlx::query!(
        "DELETE FROM rag_output_source WHERE source = $1",
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

    /// A stored round-trips through JSONB with its citations intact, the
    /// file row refuses to go while a citation stands (23503), and the note
    /// cascade sweeps the links with the rows so the file can go after.
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

        let refused = sqlx::query("DELETE FROM course_note_file WHERE id = $1")
            .bind(file.get_id().uuid())
            .execute(&db)
            .await;
        let err = refused.unwrap_err();
        assert!(crate::database::foreign_key_violation(&err), "{err}");

        delete_for_note(&db, note.get_id()).await.unwrap();
        assert!(read(&db, row.get_id()).await.unwrap().is_none());
        // The cascade swept the links with the outputs: the file row goes now.
        sqlx::query("DELETE FROM course_note_file WHERE id = $1")
            .bind(file.get_id().uuid())
            .execute(&db)
            .await
            .unwrap();
    }

    /// Replace leaves exactly one row — the newest — and the source cascade
    /// drops the link a file delete strands: the row survives reading as an
    /// empty citation list until the re-index replaces it, and the file row
    /// can go.
    #[tokio::test]
    async fn replace_is_newest_wins_and_source_cascade_empties_the_citations() {
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
        assert_eq!(total, 1);
        let survivor = read(&db, second.get_id()).await.unwrap().unwrap();
        assert!(survivor.get_sources().is_empty());
        // Links first, file after: with the link gone the file row deletes.
        sqlx::query("DELETE FROM course_note_file WHERE id = $1")
            .bind(file.get_id().uuid())
            .execute(&db)
            .await
            .unwrap();
    }

    /// Deleting one row returns it, citations included; deleting it again is
    /// a 404.
    #[tokio::test]
    async fn delete_returns_the_row_then_refuses() {
        let (db, _leases) = crate::database::init_test_db().await;
        let note = note_of(&db, "n").await;
        let file = a_file(&db, &note).await;
        let row = create(
            &db,
            note.get_id(),
            note.get_course(),
            vec![file.get_id().clone()],
            json!({}),
        )
        .await
        .unwrap();
        let gone = delete(&db, row.get_id()).await.unwrap();
        assert_eq!(gone.get_id(), row.get_id());
        assert_eq!(gone.get_sources(), &[file.get_id().clone()]);
        assert!(matches!(
            delete(&db, row.get_id()).await,
            Err(AppError::NotFound)
        ));
    }
}
