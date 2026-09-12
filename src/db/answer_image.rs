//! The `answer_image` table: the drawing upsert that names the blob it
//! replaces from inside its own transaction, the per-sitting and per-exam
//! reads, and the sweeps the retake and cascades use. The row's pure half —
//! ids and the fresh-blob-name constructor — lives in
//! [`crate::domain::answer_image`]; the blob bytes stay the web layer's.

use sqlx::PgConnection;

use crate::database::{Database, tx_with_retry};
use crate::domain::answer_image::AnswerImage;
use crate::domain::course::CourseId;
use crate::domain::exam::ExamId;
use crate::domain::exam_question::ExamQuestionId;
use crate::domain::note_file::FileContentType;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Create or replace the student's drawing for the question — the natural
/// (question, user, seq) primary key makes this the whole "one drawing per
/// student per question per sitting" story — handing back what it stored
/// plus the blob name it replaced, for the caller to take off disk.
///
/// The replaced name is read *inside this transaction*, not by the caller
/// in front of it: two uploads to one (question, user, seq) both write this
/// row, so they contend and the loser re-reads the winner's blob name,
/// where two pre-reads both saw the *old* blob and left the loser's fresh
/// one on disk with nothing pointing at it. Same shape as
/// [`crate::db::question_image::upsert`].
///
/// `NotFound` = the exam is gone, and the drawing was *not* written. The
/// write locks the exam row before upserting, in this one transaction, so
/// the exam's existence is something this write *locks* rather than
/// something a gate read a moment earlier: a bare upsert landing after
/// [`delete`](crate::db::exam::delete) removed the exam but before it
/// committed was swept by nothing — its `DELETE answer_image WHERE exam`
/// ran on a snapshot predating this row — and both sides reported success.
/// That stranded the blob as well as the row: `delete_exam` collects the
/// names to unlink *before* it calls the delete, so bytes written after
/// that snapshot stay on disk forever. Locking the key the delete removes
/// serializes the pair — this is the shape
/// [`crate::db::exam_answer::save`] takes for the text half of the same
/// answer sheet, and for the same reason.
pub async fn upsert(
    db: &Database,
    image: AnswerImage,
) -> Result<(AnswerImage, Option<String>), AppError> {
    tx_with_retry(db, false, async move |conn| upsert_in(conn, &image).await).await
}

/// The locked exam-row probe plus the upsert, on one connection.
pub(crate) async fn upsert_in(
    conn: &mut PgConnection,
    image: &AnswerImage,
) -> Result<(AnswerImage, Option<String>), AppError> {
    let touched = sqlx::query!(
        r#"SELECT id AS "id: ExamId" FROM exam WHERE id = $1 FOR UPDATE"#,
        image.exam.uuid(),
    )
    .fetch_optional(&mut *conn)
    .await?;
    if touched.is_none() {
        return Err(AppError::NotFound);
    }
    let replaced = sqlx::query!(
        r#"SELECT file FROM answer_image
           WHERE question = $1 AND app_user = $2 AND seq = $3"#,
        image.question.uuid(),
        image.user.uuid(),
        image.seq,
    )
    .fetch_optional(&mut *conn)
    .await?
    .map(|row| row.file);
    let stored = sqlx::query_as!(
        AnswerImage,
        r#"INSERT INTO answer_image (exam, question, app_user, file, content_type, size, seq)
           VALUES ($1, $2, $3, $4, $5, $6, $7)
           ON CONFLICT (question, app_user, seq) DO UPDATE
               SET file = EXCLUDED.file, content_type = EXCLUDED.content_type,
                   size = EXCLUDED.size
           RETURNING exam AS "exam: ExamId", question AS "question: ExamQuestionId",
                     app_user AS "user: UserId", seq, file,
                     content_type AS "content_type: FileContentType", size"#,
        image.exam.uuid(),
        image.question.uuid(),
        image.user.uuid(),
        image.file,
        image.content_type.as_str(),
        image.size,
        image.seq,
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok((stored, replaced))
}

/// The student's drawing for one question in one sitting, if any.
pub async fn read(
    db: &Database,
    question: &ExamQuestionId,
    user: &UserId,
    seq: i64,
) -> Result<Option<AnswerImage>, AppError> {
    Ok(sqlx::query_as!(
        AnswerImage,
        r#"SELECT exam AS "exam: ExamId", question AS "question: ExamQuestionId",
                  app_user AS "user: UserId", seq, file,
                  content_type AS "content_type: FileContentType", size
           FROM answer_image
           WHERE question = $1 AND app_user = $2 AND seq = $3"#,
        question.uuid(),
        user.uuid(),
        seq,
    )
    .fetch_optional(db)
    .await?)
}

/// Every answer drawing of the exam — one query for the exam-delete cascade.
pub async fn list_for_exam(db: &Database, exam: &ExamId) -> Result<Vec<AnswerImage>, AppError> {
    Ok(sqlx::query_as!(
        AnswerImage,
        r#"SELECT exam AS "exam: ExamId", question AS "question: ExamQuestionId",
                  app_user AS "user: UserId", seq, file,
                  content_type AS "content_type: FileContentType", size
           FROM answer_image WHERE exam = $1"#,
        exam.uuid(),
    )
    .fetch_all(db)
    .await?)
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
    Ok(sqlx::query_as!(
        AnswerImage,
        r#"SELECT exam AS "exam: ExamId", question AS "question: ExamQuestionId",
                  app_user AS "user: UserId", seq, file,
                  content_type AS "content_type: FileContentType", size
           FROM answer_image WHERE exam = $1 AND app_user = $2 AND seq = $3"#,
        exam.uuid(),
        user.uuid(),
        seq,
    )
    .fetch_all(db)
    .await?)
}

/// The distinct sittings (`seq`, ascending) this student has any answer
/// drawing for at this exam — the history index behind a per-attempt view.
pub async fn list_seqs_for_user(
    db: &Database,
    exam: &ExamId,
    user: &UserId,
) -> Result<Vec<i64>, AppError> {
    let rows = sqlx::query!(
        r#"SELECT DISTINCT seq FROM answer_image
           WHERE exam = $1 AND app_user = $2 ORDER BY seq ASC"#,
        exam.uuid(),
        user.uuid(),
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|row| row.seq).collect())
}

/// The blob names behind every answer drawing of every exam of `course` —
/// collected *before* the course-delete cascade wipes the rows.
pub async fn file_keys_for_course(
    db: &Database,
    course: &CourseId,
) -> Result<Vec<String>, AppError> {
    let rows = sqlx::query!(
        r#"SELECT ai.file FROM answer_image ai
           JOIN exam e ON e.id = ai.exam WHERE e.course = $1"#,
        course.uuid(),
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|row| row.file).collect())
}

pub async fn delete(db: &Database, image: AnswerImage) -> Result<AnswerImage, AppError> {
    let deleted = sqlx::query_as!(
        AnswerImage,
        r#"DELETE FROM answer_image WHERE question = $1 AND app_user = $2 AND seq = $3
           RETURNING exam AS "exam: ExamId", question AS "question: ExamQuestionId",
                     app_user AS "user: UserId", seq, file,
                     content_type AS "content_type: FileContentType", size"#,
        image.question.uuid(),
        image.user.uuid(),
        image.seq,
    )
    .fetch_optional(db)
    .await?;
    deleted.ok_or(AppError::NotFound)
}

/// Drop one student's answer drawings across an exam — a retake starts from
/// a blank sheet. Rows only; the caller GCs the blobs. The retake path
/// itself wipes nothing: a retake's rows live at their own `seq`, and this
/// is the standalone mirror of
/// [`crate::db::exam_answer::delete_for_exam_user`].
pub async fn delete_for_exam_user(
    db: &Database,
    exam: &ExamId,
    user: &UserId,
) -> Result<(), AppError> {
    sqlx::query!(
        r#"DELETE FROM answer_image WHERE exam = $1 AND app_user = $2"#,
        exam.uuid(),
        user.uuid(),
    )
    .execute(db)
    .await?;
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

    /// A real `app_user` row under a fixed key: the sheet's owner is a
    /// foreign key now.
    async fn a_student(db: &Database) -> UserId {
        let user = UserId::from_key("019732e3-7b00-7000-8000-00000000a11a");
        sqlx::query("INSERT INTO app_user (id, username, password_hash) VALUES ($1, 'a11a', 'x')")
            .bind(user.uuid())
            .execute(db)
            .await
            .unwrap();
        user
    }

    /// A real question row on `exam`: a drawing hangs off it by foreign key.
    async fn a_question(db: &Database, exam: &ExamId) -> ExamQuestionId {
        let subject = crate::db::subject::create(
            db,
            &crate::db::course::a_test_course(db).await,
            crate::domain::subject::SubjectName::try_new("topic").unwrap(),
            crate::domain::subject::SubjectDescription::try_new("").unwrap(),
        )
        .await
        .unwrap();
        let spec = crate::domain::exam_question::QuestionSpec::try_new(
            crate::domain::exam_question::QuestionKind::try_new("text").unwrap(),
            None,
            None,
            &[],
        )
        .unwrap();
        crate::db::exam_question::create(
            db,
            exam,
            subject.get_id().clone(),
            crate::domain::exam_question::QuestionText::try_new("3 + 3?").unwrap(),
            crate::domain::exam_question::QuestionPoints::try_new(1).unwrap(),
            spec,
        )
        .await
        .unwrap()
        .get_id()
        .clone()
    }

    #[tokio::test]
    async fn upsert_replaces_within_a_sitting_but_not_across_them() {
        let (db, _leases) = crate::database::init_test_db().await;
        let exam = exam_row(&db).await;
        let question = a_question(&db, &exam).await;
        let user = a_student(&db).await;

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
        let other = UserId::generate();
        sqlx::query("INSERT INTO app_user (id, username, password_hash) VALUES ($1, 'b22b', 'x')")
            .bind(other.uuid())
            .execute(&db)
            .await
            .unwrap();
        upsert(&db, AnswerImage::new(&exam, &question, &other, 1, png(), 7))
            .await
            .unwrap();
        assert_eq!(list_for_exam(&db, &exam).await.unwrap().len(), 3);
        assert!(read(&db, &question, &user, 2).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn listing_scopes_by_exam_and_user() {
        let (db, _leases) = crate::database::init_test_db().await;
        let exam_a = exam_row(&db).await;
        let exam_b = exam_row(&db).await;
        let user = a_student(&db).await;
        let question = a_question(&db, &exam_a).await;
        upsert(&db, AnswerImage::new(&exam_a, &question, &user, 1, png(), 1))
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
