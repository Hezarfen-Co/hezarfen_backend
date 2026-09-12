//! The `question_image` table: the slot-keyed upsert (and delete) behind the
//! question-freeze gate, the listing reads, and the choice-cleanup sweep a
//! question PATCH drives. The row's pure half — ids and the fresh-blob-name
//! constructor — lives in [`crate::domain::question_image`]; the blob bytes
//! stay the web layer's.

use sqlx::PgConnection;

use crate::database::{Database, tx_with_retry};
use crate::db::exam_attempt::freeze_gate;
use crate::domain::course::CourseId;
use crate::domain::exam::ExamId;
use crate::domain::exam_question::{ChoiceId, ExamQuestionId};
use crate::domain::note_file::FileContentType;
use crate::domain::question_image::QuestionImage;
use crate::error::AppError;

/// Create or replace the slot's image row — the `UNIQUE NULLS NOT DISTINCT`
/// (question, slot) constraint makes this the whole "one image per slot"
/// story — handing back what it stored plus the blob name it replaced, for
/// the caller to take off disk. Refused once the exam has an attempt:
/// pictures are part of the question, so they freeze with it, and the gate
/// is in this transaction rather than in a lock the caller held.
///
/// The replaced name is read *inside the same transaction*, not by the
/// caller before it: two uploads to one slot both write this row, so they
/// contend on it and the loser re-reads the winner's blob name, where two
/// pre-reads both saw the *old* blob and left the loser's fresh one
/// orphaned on disk.
pub async fn upsert(
    db: &Database,
    image: QuestionImage,
) -> Result<(QuestionImage, Option<String>), AppError> {
    tx_with_retry(db, false, async move |conn| {
        upsert_in(conn, image.clone()).await
    })
    .await
}

/// The gated read-and-replace, on one connection.
pub(crate) async fn upsert_in(
    conn: &mut PgConnection,
    image: QuestionImage,
) -> Result<(QuestionImage, Option<String>), AppError> {
    freeze_gate(conn, &image.exam).await?;
    let replaced = replaced_file(conn, &image.question, image.slot.as_ref()).await?;
    let stored = sqlx::query_as!(
        QuestionImage,
        r#"INSERT INTO question_image (exam, question, slot, file, content_type, size)
           VALUES ($1, $2, $3, $4, $5, $6)
           ON CONFLICT (question, slot) DO UPDATE
               SET file = EXCLUDED.file, content_type = EXCLUDED.content_type,
                   size = EXCLUDED.size
           RETURNING exam AS "exam: ExamId", question AS "question: ExamQuestionId",
                     slot AS "slot: ChoiceId", file,
                     content_type AS "content_type: FileContentType", size"#,
        image.exam.uuid(),
        image.question.uuid(),
        image.slot.as_ref().map(ChoiceId::as_str),
        image.file,
        image.content_type.as_str(),
        image.size,
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok((stored, replaced))
}

/// The blob name a slot's row currently names — the file a replace takes
/// off disk. `IS NOT DISTINCT FROM` is what makes the illustration slot's
/// `NULL` match itself.
pub(crate) async fn replaced_file(
    conn: &mut PgConnection,
    question: &ExamQuestionId,
    slot: Option<&ChoiceId>,
) -> Result<Option<String>, AppError> {
    let row = sqlx::query!(
        r#"SELECT file FROM question_image
           WHERE question = $1 AND slot IS NOT DISTINCT FROM $2"#,
        question.uuid(),
        slot.map(ChoiceId::as_str),
    )
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|row| row.file))
}

pub async fn read_slot(
    db: &Database,
    question: &ExamQuestionId,
    slot: Option<&ChoiceId>,
) -> Result<Option<QuestionImage>, AppError> {
    Ok(sqlx::query_as!(
        QuestionImage,
        r#"SELECT exam AS "exam: ExamId", question AS "question: ExamQuestionId",
                  slot AS "slot: ChoiceId", file,
                  content_type AS "content_type: FileContentType", size
           FROM question_image
           WHERE question = $1 AND slot IS NOT DISTINCT FROM $2"#,
        question.uuid(),
        slot.map(ChoiceId::as_str),
    )
    .fetch_optional(db)
    .await?)
}

/// Every image of the exam's questions — one query for the list views.
pub async fn list_for_exam(db: &Database, exam: &ExamId) -> Result<Vec<QuestionImage>, AppError> {
    Ok(sqlx::query_as!(
        QuestionImage,
        r#"SELECT exam AS "exam: ExamId", question AS "question: ExamQuestionId",
                  slot AS "slot: ChoiceId", file,
                  content_type AS "content_type: FileContentType", size
           FROM question_image WHERE exam = $1"#,
        exam.uuid(),
    )
    .fetch_all(db)
    .await?)
}

pub async fn list_for_question(
    db: &Database,
    question: &ExamQuestionId,
) -> Result<Vec<QuestionImage>, AppError> {
    Ok(sqlx::query_as!(
        QuestionImage,
        r#"SELECT exam AS "exam: ExamId", question AS "question: ExamQuestionId",
                  slot AS "slot: ChoiceId", file,
                  content_type AS "content_type: FileContentType", size
           FROM question_image WHERE question = $1"#,
        question.uuid(),
    )
    .fetch_all(db)
    .await?)
}

/// Drop the option pictures whose choice is gone — every choice image of
/// the question whose `slot` is *not* in `keep` (the question's own
/// illustration always stays), returning the removed rows so the caller can
/// take their blobs off disk.
///
/// This is what makes an edit non-destructive: a PATCH that reorders,
/// renames, or drops options passes the surviving choice ids as `keep`, so
/// only the pictures of genuinely removed options go. `keep = &[]` (a text
/// question, or an all-new choice list) still clears the lot.
pub async fn delete_choices_not_in(
    db: &Database,
    question: &ExamQuestionId,
    keep: &[ChoiceId],
) -> Result<Vec<QuestionImage>, AppError> {
    let keep = keep
        .iter()
        .map(|id| id.as_str().to_string())
        .collect::<Vec<_>>();
    Ok(sqlx::query_as!(
        QuestionImage,
        r#"DELETE FROM question_image
           WHERE question = $1 AND slot IS NOT NULL AND NOT (slot = ANY($2))
           RETURNING exam AS "exam: ExamId", question AS "question: ExamQuestionId",
                     slot AS "slot: ChoiceId", file,
                     content_type AS "content_type: FileContentType", size"#,
        question.uuid(),
        &keep,
    )
    .fetch_all(db)
    .await?)
}

/// The blob names behind every image of every exam of `course` — collected
/// *before* the course-delete cascade wipes the rows.
pub async fn file_keys_for_course(
    db: &Database,
    course: &CourseId,
) -> Result<Vec<String>, AppError> {
    let rows = sqlx::query!(
        r#"SELECT qi.file FROM question_image qi
           JOIN exam e ON e.id = qi.exam WHERE e.course = $1"#,
        course.uuid(),
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|row| row.file).collect())
}

/// Refused once the exam has an attempt, in the same transaction — same
/// gate, same reason as [`upsert`].
pub async fn delete(db: &Database, image: QuestionImage) -> Result<QuestionImage, AppError> {
    tx_with_retry(db, false, async move |conn| {
        freeze_gate(conn, &image.exam).await?;
        let deleted = sqlx::query_as!(
            QuestionImage,
            r#"DELETE FROM question_image
               WHERE question = $1 AND slot IS NOT DISTINCT FROM $2
               RETURNING exam AS "exam: ExamId", question AS "question: ExamQuestionId",
                         slot AS "slot: ChoiceId", file,
                         content_type AS "content_type: FileContentType", size"#,
            image.question.uuid(),
            image.slot.as_ref().map(ChoiceId::as_str),
        )
        .fetch_optional(&mut *conn)
        .await?;
        deleted.ok_or(AppError::NotFound)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::exam_question::{ChoiceInput, QuestionKind, QuestionSpec};
    use crate::domain::note_file::FileContentType;

    /// A real exam row: an image write moves its exam's counter (that is what
    /// keeps a picture from outliving its exam), so a minted id nothing wrote
    /// is a 404.
    async fn exam_row(db: &Database) -> ExamId {
        crate::db::exam::published_exam(db).await.get_id().clone()
    }

    fn png() -> FileContentType {
        FileContentType::try_new("image/png").unwrap()
    }

    /// Three minted choice ids to slot pictures against.
    fn choice_ids() -> Vec<ChoiceId> {
        QuestionSpec::try_new(
            QuestionKind::try_new("choice").unwrap(),
            Some(
                ["a", "b", "c"]
                    .iter()
                    .map(|l| ChoiceInput {
                        id: Some((*l).into()),
                        text: (*l).into(),
                    })
                    .collect(),
            ),
            Some("a".into()),
            &[],
        )
        .unwrap()
        .into_parts()
        .1
        .unwrap()
        .iter()
        .map(|c| c.get_id().clone())
        .collect()
    }

    /// A real question row on `exam`: images are FK children of their
    /// question, so a minted id is refused outright.
    async fn a_question(db: &Database, exam: &ExamId) -> ExamQuestionId {
        use crate::domain::exam_question::{QuestionPoints, QuestionText};
        use crate::domain::subject::{SubjectDescription, SubjectName};
        let spec = QuestionSpec::try_new(
            QuestionKind::try_new("choice").unwrap(),
            Some(
                ["a", "b", "c"]
                    .iter()
                    .map(|l| ChoiceInput {
                        id: Some((*l).into()),
                        text: (*l).into(),
                    })
                    .collect(),
            ),
            Some("a".into()),
            &[],
        )
        .unwrap();
        let subject = crate::db::subject::create(
            db,
            &crate::db::course::a_test_course(db).await,
            SubjectName::try_new("pictures").unwrap(),
            SubjectDescription::try_new("").unwrap(),
        )
        .await
        .unwrap();
        crate::db::exam_question::create(
            db,
            exam,
            subject.get_id().clone(),
            QuestionText::try_new("pick one").unwrap(),
            QuestionPoints::try_new(1).unwrap(),
            spec,
        )
        .await
        .unwrap()
        .get_id()
        .clone()
    }

    #[tokio::test]
    async fn upsert_replaces_per_slot() {
        let (db, _leases) = crate::database::init_test_db().await;
        let exam = exam_row(&db).await;
        let question = a_question(&db, &exam).await;
        let ids = choice_ids();

        let (first, retired) = upsert(&db, QuestionImage::new(&exam, &question, None, png(), 3))
            .await
            .unwrap();
        assert_eq!(retired, None, "a first upload retires no blob");
        let (second, retired) = upsert(&db, QuestionImage::new(&exam, &question, None, png(), 5))
            .await
            .unwrap();
        // Same slot, same row — the replace swapped the blob pointer, and the
        // write itself names the blob the caller must unlink.
        assert_ne!(first.get_file(), second.get_file());
        assert_eq!(retired.as_deref(), Some(first.get_file()));
        let rows = list_for_question(&db, &question).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_size(), 5);

        // A choice slot is its own row, keyed by the option's id.
        upsert(
            &db,
            QuestionImage::new(&exam, &question, Some(&ids[0]), png(), 7),
        )
        .await
        .unwrap();
        assert_eq!(list_for_question(&db, &question).await.unwrap().len(), 2);
        let choice = read_slot(&db, &question, Some(&ids[0]))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(choice.get_slot(), Some(&ids[0]));
        assert!(
            read_slot(&db, &question, Some(&ids[1]))
                .await
                .unwrap()
                .is_none()
        );
    }

    /// The behaviour the whole remodel exists for: an edit that keeps some
    /// options keeps exactly their pictures, and drops only the removed one's.
    #[tokio::test]
    async fn only_the_dropped_options_lose_their_pictures() {
        let (db, _leases) = crate::database::init_test_db().await;
        let exam = exam_row(&db).await;
        let question = a_question(&db, &exam).await;
        let ids = choice_ids();
        upsert(&db, QuestionImage::new(&exam, &question, None, png(), 1))
            .await
            .unwrap();
        for id in &ids {
            upsert(
                &db,
                QuestionImage::new(&exam, &question, Some(id), png(), 1),
            )
            .await
            .unwrap();
        }

        // Keep the first and last option (reordered — order is irrelevant now).
        let keep = vec![ids[2].clone(), ids[0].clone()];
        let dropped = delete_choices_not_in(&db, &question, &keep).await.unwrap();
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].get_slot(), Some(&ids[1]));

        let left = list_for_question(&db, &question).await.unwrap();
        // The question illustration plus the two surviving option pictures.
        assert_eq!(left.len(), 3);
        assert!(
            read_slot(&db, &question, Some(&ids[0]))
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            read_slot(&db, &question, Some(&ids[2]))
                .await
                .unwrap()
                .is_some()
        );
        assert!(read_slot(&db, &question, None).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn an_empty_keep_set_clears_every_option_picture_but_not_the_illustration() {
        let (db, _leases) = crate::database::init_test_db().await;
        let exam = exam_row(&db).await;
        let question = a_question(&db, &exam).await;
        let ids = choice_ids();
        upsert(&db, QuestionImage::new(&exam, &question, None, png(), 1))
            .await
            .unwrap();
        for id in &ids[..2] {
            upsert(
                &db,
                QuestionImage::new(&exam, &question, Some(id), png(), 1),
            )
            .await
            .unwrap();
        }

        let dropped = delete_choices_not_in(&db, &question, &[]).await.unwrap();
        assert_eq!(dropped.len(), 2);
        assert!(dropped.iter().all(|image| image.get_slot().is_some()));

        let left = list_for_question(&db, &question).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].get_slot(), None);
    }

    #[tokio::test]
    async fn exam_listing_scopes_by_exam() {
        let (db, _leases) = crate::database::init_test_db().await;
        let exam_a = exam_row(&db).await;
        let exam_b = exam_row(&db).await;
        upsert(
            &db,
            QuestionImage::new(&exam_a, &a_question(&db, &exam_a).await, None, png(), 1),
        )
        .await
        .unwrap();
        assert_eq!(list_for_exam(&db, &exam_a).await.unwrap().len(), 1);
        assert!(list_for_exam(&db, &exam_b).await.unwrap().is_empty());
    }
}
