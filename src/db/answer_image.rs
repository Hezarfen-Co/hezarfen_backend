//! The `answer_image` table: the drawing upsert that names the blob it
//! replaces from inside its own transaction, the per-sitting and per-exam
//! reads, and the sweeps the retake and cascades use. The row's pure half —
//! ids and the fresh-blob-name constructor — lives in
//! [`crate::domain::answer_image`]; the blob bytes stay the web layer's.

use surrealdb::types::SurrealValue;

use crate::constant::EXAM_RESULT_COUNT_FIELD;
use crate::database::{Database, transaction_with_retry};
use crate::domain::answer_image::{AnswerImage, AnswerImageId};
use crate::domain::course::CourseId;
use crate::domain::exam::ExamId;
use crate::domain::exam_question::ExamQuestionId;
use crate::domain::user::UserId;
use crate::error::AppError;

/// What one [`upsert`] transaction returns: the row it stored and
/// the blob name it replaced. Both are arrays because SurrealDB drops an object
/// key valued `NONE` on the way out, while an empty array survives — "nothing
/// was replaced" has to be readable, not missing.
#[derive(SurrealValue)]
struct UpsertOutcome {
    stored: Vec<AnswerImage>,
    replaced: Vec<String>,
}

/// Create or replace the student's drawing for the question — the
/// deterministic id makes this the whole "one drawing per student per
/// question" story — handing back what it stored plus the blob name it
/// replaced, for the caller to take off disk.
///
/// The replaced name is read *here*, inside this transaction, not by the
/// caller in front of it: two uploads to one (question, user, seq) both
/// write this row, so they contend and the loser re-reads the winner's blob
/// name, where two pre-reads both saw the *old* blob and left the loser's
/// fresh one on disk with nothing pointing at it. Same shape as
/// [`crate::db::question_image::upsert`].
///
/// `NotFound` = the exam is gone, and the drawing was *not* written. The
/// write moves the exam's mark counter and puts it straight back, in this
/// one transaction, so the exam's existence is something this write
/// *writes* rather than something a gate read a moment earlier: a bare
/// upsert landing after [`delete`](crate::db::exam::delete)
/// removed the exam but before it committed was swept by nothing — its
/// `DELETE answer_image WHERE exam = $ex` ran on a snapshot predating this
/// row — and both sides reported success. That stranded the blob as well as
/// the row: `delete_exam` collects the names to unlink *before* it calls
/// the delete, so bytes written after that snapshot stay on disk forever.
/// This is the shape [`crate::db::exam_answer::save`] takes
/// for the text half of the same answer sheet, and for the same reason.
///
/// The restore is by captured value, `NONE` included, so the row is
/// byte-identical afterwards and a teacher's PATCH — which pins that
/// counter — is not refused because a student drew. Writing the same value
/// back would buy nothing: an `UPDATE` that leaves the document unchanged
/// is elided and never reaches the store's write set.
///
/// Admissible for [`transaction_with_retry`]: the `UPDATE`s, `SELECT`,
/// `IF`/`THROW` and `RETURN` can never answer "already exists", and the
/// `UPSERT`'s id is bijective with the (question, user, seq) triple this
/// table keys — a lost round wrote nothing, and re-sending resolves onto
/// the same row rather than colliding with it.
pub async fn upsert(
    db: &Database,
    image: AnswerImage,
) -> Result<(AnswerImage, Option<String>), AppError> {
    // whole-row-save-ok: image is built in place from the request, never read back, and the (question, user, seq) id is deterministic — replacing the row *is* the operation
    let (exam, id) = (image.exam.record(), image.id.record());
    let (mut result, mut errors) = transaction_with_retry(
        db,
        &format!(
            "BEGIN TRANSACTION;
             LET $was = (SELECT VALUE {EXAM_RESULT_COUNT_FIELD} FROM ONLY $ex);
             LET $touched = (UPDATE $ex SET {EXAM_RESULT_COUNT_FIELD} = \
                 ({EXAM_RESULT_COUNT_FIELD} ?? 0) + 1 RETURN VALUE id);
             IF array::len($touched) = 0 {{ THROW 'no_exam' }};
             UPDATE $ex SET {EXAM_RESULT_COUNT_FIELD} = $was;
             LET $replaced = (SELECT VALUE file FROM $id);
             LET $row = (UPSERT $id CONTENT $image RETURN AFTER);
             RETURN {{ stored: $row, replaced: $replaced }};
             COMMIT TRANSACTION;"
        ),
        &[
            ("ex".into(), exam.into_value()),
            ("id".into(), id.into_value()),
            ("image".into(), image.into_value()),
        ],
        &["no_exam"],
    )
    .await?;
    // An aborted transaction errors *every* slot, most with a generic "not
    // executed" — only the THROW's own slot names the reason.
    if errors
        .values()
        .any(|error| error.to_string().contains("no_exam"))
    {
        return Err(AppError::NotFound);
    }
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    // The trailing `RETURN` is the last statement before `COMMIT`, so its
    // slot follows the statement count rather than a hand-kept number;
    // `num_statements` counts BEGIN and COMMIT.
    let slot = result.num_statements().saturating_sub(2);
    let failed = || AppError::Internal("failed to store answer image".into());
    let outcome = result
        .take::<Vec<UpsertOutcome>>(slot)?
        .into_iter()
        .next()
        .ok_or_else(failed)?;
    let stored = outcome.stored.into_iter().next().ok_or_else(failed)?;
    Ok((stored, outcome.replaced.into_iter().next()))
}

/// The student's drawing for one question in one sitting, if any.
pub async fn read(
    db: &Database,
    question: &ExamQuestionId,
    user: &UserId,
    seq: i64,
) -> Result<Option<AnswerImage>, AppError> {
    Ok(db
        .select(AnswerImageId::composite(question, user, seq).record())
        .await?)
}

/// Every answer drawing of the exam — one query for the exam-delete cascade.
pub async fn list_for_exam(db: &Database, exam: &ExamId) -> Result<Vec<AnswerImage>, AppError> {
    let mut result = db
        .query("SELECT * FROM answer_image WHERE exam = $ex")
        .bind(("ex", exam.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<AnswerImage>>(0)?)
}

/// One student's answer drawings for a single sitting — the parallel to
/// [`crate::db::exam_answer::list_for_exam_user`], feeding
/// the sitting/grading answer-image maps for that attempt's `seq`.
pub async fn list_for_exam_user(
    db: &Database,
    exam: &ExamId,
    user: &UserId,
    seq: i64,
) -> Result<Vec<AnswerImage>, AppError> {
    let mut result = db
        .query("SELECT * FROM answer_image WHERE exam = $ex AND user = $usr AND seq = $seq")
        .bind(("ex", exam.record()))
        .bind(("usr", user.record()))
        .bind(("seq", seq))
        .await?
        .check()?;
    Ok(result.take::<Vec<AnswerImage>>(0)?)
}

/// The distinct sittings (`seq`, ascending) this student has any answer
/// drawing for at this exam — the history index behind a per-attempt view.
pub async fn list_seqs_for_user(
    db: &Database,
    exam: &ExamId,
    user: &UserId,
) -> Result<Vec<i64>, AppError> {
    let mut result = db
        .query(
            "SELECT VALUE seq FROM answer_image \
             WHERE exam = $ex AND user = $usr ORDER BY seq",
        )
        .bind(("ex", exam.record()))
        .bind(("usr", user.record()))
        .await?
        .check()?;
    let mut seqs = result.take::<Vec<i64>>(0)?;
    seqs.dedup();
    Ok(seqs)
}

/// The blob names behind every answer drawing of every exam of `course` —
/// collected *before* the course-delete cascade wipes the rows.
pub async fn file_keys_for_course(
    db: &Database,
    course: &CourseId,
) -> Result<Vec<String>, AppError> {
    let mut result = db
        .query(
            "SELECT VALUE file FROM answer_image \
             WHERE exam IN (SELECT VALUE id FROM exam WHERE course = $course)",
        )
        .bind(("course", course.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<String>>(0)?)
}

pub async fn delete(db: &Database, image: AnswerImage) -> Result<AnswerImage, AppError> {
    let deleted: Option<AnswerImage> = db.delete(image.id.record()).await?;
    deleted.ok_or(AppError::NotFound)
}

/// Drop one student's answer drawings across an exam — a retake starts from
/// a blank sheet. Rows only; the caller GCs the blobs. The retake path itself
/// wipes rows inside `ExamAttempt::wipe_and_create`'s transaction; this is
/// the standalone mirror of [`crate::db::exam_answer::delete_for_exam_user`].
pub async fn delete_for_exam_user(
    db: &Database,
    exam: &ExamId,
    user: &UserId,
) -> Result<(), AppError> {
    db.query("DELETE answer_image WHERE exam = $ex AND user = $usr")
        .bind(("ex", exam.record()))
        .bind(("usr", user.record()))
        .await?
        .check()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::note_file::FileContentType;

    fn png() -> FileContentType {
        FileContentType::try_new("image/png").unwrap()
    }

    /// A real exam row: a drawing's write moves its exam's counter (that is
    /// what keeps it from outliving the exam), so a minted id nothing wrote is
    /// a 404.
    async fn exam_row(db: &Database) -> ExamId {
        crate::db::exam::published_exam(db).await.get_id().clone()
    }

    fn student() -> UserId {
        UserId::from_key("01TESTSTUDENTAAAAAAAAAAAAA")
    }

    #[tokio::test]
    async fn upsert_replaces_within_a_sitting_but_not_across_them() {
        let db = crate::database::init_mem().await.unwrap();
        let exam = exam_row(&db).await;
        let question = ExamQuestionId::generate();
        let user = student();

        let (first, retired) = upsert(&db, AnswerImage::new(&exam, &question, &user, 1, png(), 3))
            .await
            .unwrap();
        assert_eq!(retired, None, "a first upload retires no blob");
        let (second, retired) = upsert(&db, AnswerImage::new(&exam, &question, &user, 1, png(), 5))
            .await
            .unwrap();
        // Same (question, user, seq), same row — the replace swapped the blob,
        // and the write itself names the blob the caller must unlink.
        assert_ne!(first.get_file(), second.get_file());
        assert_eq!(retired.as_deref(), Some(first.get_file()));
        let rows = list_for_exam_user(&db, &exam, &user, 1).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_size(), 5);

        // A retake (seq 2) accumulates: it is its own row, not an overwrite.
        upsert(&db, AnswerImage::new(&exam, &question, &user, 2, png(), 9))
            .await
            .unwrap();
        assert_eq!(
            list_for_exam_user(&db, &exam, &user, 1)
                .await
                .unwrap()
                .len(),
            1,
            "the seq-2 image must not overwrite seq-1"
        );
        assert_eq!(
            list_for_exam_user(&db, &exam, &user, 2).await.unwrap()[0].get_size(),
            9
        );
        assert_eq!(
            list_seqs_for_user(&db, &exam, &user).await.unwrap(),
            vec![1, 2]
        );

        // Another student's drawing for the same question/sitting is its own row.
        let other = UserId::from_key("01TESTSTUDENTBBBBBBBBBBBBB");
        upsert(&db, AnswerImage::new(&exam, &question, &other, 1, png(), 7))
            .await
            .unwrap();
        assert_eq!(list_for_exam(&db, &exam).await.unwrap().len(), 3);
        assert!(read(&db, &question, &user, 2).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn listing_scopes_by_exam_and_user() {
        let db = crate::database::init_mem().await.unwrap();
        let exam_a = exam_row(&db).await;
        let exam_b = exam_row(&db).await;
        let user = student();
        upsert(
            &db,
            AnswerImage::new(&exam_a, &ExamQuestionId::generate(), &user, 1, png(), 1),
        )
        .await
        .unwrap();
        assert_eq!(
            list_for_exam_user(&db, &exam_a, &user, 1)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(list_for_exam(&db, &exam_b).await.unwrap().is_empty());

        // delete_for_exam_user clears the student's sheet across all sittings.
        delete_for_exam_user(&db, &exam_a, &user).await.unwrap();
        assert!(
            list_for_exam_user(&db, &exam_a, &user, 1)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
