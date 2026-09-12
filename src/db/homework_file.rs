//! The `homework_file` table: the attachment rows behind a submission's
//! files — the seat-claiming insert, the scoped reads, the GC key
//! collectors, and the gate-guarded delete. The rows' assembly (`new`) and
//! their validated fields live in [`crate::domain::homework_file`]; the
//! upload workflow around them in [`crate::service::homework_file`].

use crate::constant::MAX_HOMEWORK_FILES_PER_SUBMISSION;
use crate::database::{Database, tx_with_retry};
use crate::domain::course::CourseId;
use crate::domain::homework::HomeworkId;
use crate::domain::homework_file::HomeworkFile;
use crate::domain::homework_file::HomeworkFileId;
use crate::domain::homework_submission::HomeworkSubmissionId;
use crate::domain::note_file::{FileContentType, FileName};
use crate::domain::timestamp::Timestamp;
use crate::error::AppError;

/// Persist the row assembled by
/// [`HomeworkFile::new`](crate::domain::homework_file::HomeworkFile::new),
/// refusing once its submission already holds
/// [`MAX_HOMEWORK_FILES_PER_SUBMISSION`] (an `Err(Conflict)`)
/// or once a grade has frozen it (`Ok(None)`, so the web layer keeps its own
/// wording). Both are decided by one guarded insert on the submission row —
/// the seat (`UPDATE … file_count + 1 WHERE file_count < $cap AND
/// graded_by_result IS NULL`) and the file row commit together in one CTE, so
/// a crash between them can no longer leave a slot claimed by a file that
/// does not exist, and a losing upload never has to be un-counted. Which of
/// the two conditions refused it is read back afterwards, off the losing
/// path only, and only to pick the message: zero rows says "full *or* graded
/// *or* the submission is gone".
pub async fn insert(db: &Database, file: HomeworkFile) -> Result<Option<HomeworkFile>, AppError> {
    let created = sqlx::query_as!(
        HomeworkFile,
        r#"WITH seat AS (
               UPDATE homework_submission SET file_count = file_count + 1
               WHERE id = $2 AND file_count < $1 AND graded_by_result IS NULL
               RETURNING 1
           )
           INSERT INTO homework_file (id, submission, name, content_type, size, file, created_at)
           SELECT $3, $2, $4, $5, $6, $7, $8 WHERE EXISTS (SELECT 1 FROM seat)
           RETURNING id AS "id: HomeworkFileId",
                     submission AS "submission: HomeworkSubmissionId",
                     name AS "name: FileName",
                     content_type AS "content_type: FileContentType",
                     size,
                     file,
                     created_at AS "created_at: Timestamp""#,
        MAX_HOMEWORK_FILES_PER_SUBMISSION as i64,
        file.submission,
        file.id,
        file.name,
        file.content_type,
        file.size,
        file.file,
        file.created_at,
    )
    .fetch_optional(db)
    .await
    .map_err(|err| {
        // The id is a fresh v7 minted by `new`, so a row already holding it
        // is a collision, not a re-upload.
        if crate::database::unique_violation(&err).is_some() {
            AppError::Internal("homework file id collided".into())
        } else {
            AppError::from(err)
        }
    })?;
    match created {
        Some(row) => Ok(Some(row)),
        None => {
            if super::homework_submission::is_graded(db, &file.submission).await? {
                return Ok(None);
            }
            Err(AppError::Conflict(
                "the submission already holds the maximum of 10 files — delete one first",
            ))
        }
    }
}

/// Read a file's row only if it belongs to `submission` — callers have
/// already checked the submission belongs to the requesting user.
pub async fn read_for(
    db: &Database,
    id: &crate::domain::homework_file::HomeworkFileId,
    submission: &crate::domain::homework_submission::HomeworkSubmissionId,
) -> Result<Option<HomeworkFile>, AppError> {
    Ok(sqlx::query_as!(
        HomeworkFile,
        r#"SELECT id AS "id: HomeworkFileId",
                  submission AS "submission: HomeworkSubmissionId",
                  name AS "name: FileName",
                  content_type AS "content_type: FileContentType",
                  size,
                  file,
                  created_at AS "created_at: Timestamp"
           FROM homework_file WHERE id = $1 AND submission = $2"#,
        id,
        submission
    )
    .fetch_optional(db)
    .await?)
}

/// Read a file by id only if it hangs off a submission to `homework` — the
/// grader's download scoping. The teacher download path carries the homework
/// id but not the owning student (unlike `read_for`, which needs the
/// submission), so this both finds the file and confirms it belongs under
/// `homework`: a teacher can't pass a homework they manage to read a file
/// from a different (perhaps unmanaged) one.
pub async fn read_in_homework(
    db: &Database,
    id: &crate::domain::homework_file::HomeworkFileId,
    homework: &HomeworkId,
) -> Result<Option<HomeworkFile>, AppError> {
    Ok(sqlx::query_as!(
        HomeworkFile,
        r#"SELECT id AS "id: HomeworkFileId",
                  submission AS "submission: HomeworkSubmissionId",
                  name AS "name: FileName",
                  content_type AS "content_type: FileContentType",
                  size,
                  file,
                  created_at AS "created_at: Timestamp"
           FROM homework_file
           WHERE id = $1
             AND submission IN (SELECT id FROM homework_submission WHERE homework = $2)"#,
        id,
        homework
    )
    .fetch_optional(db)
    .await?)
}

/// All of `submission`'s files, newest first.
pub async fn list_for_submission(
    db: &Database,
    submission: &crate::domain::homework_submission::HomeworkSubmissionId,
) -> Result<Vec<HomeworkFile>, AppError> {
    Ok(sqlx::query_as!(
        HomeworkFile,
        r#"SELECT id AS "id: HomeworkFileId",
                  submission AS "submission: HomeworkSubmissionId",
                  name AS "name: FileName",
                  content_type AS "content_type: FileContentType",
                  size,
                  file,
                  created_at AS "created_at: Timestamp"
           FROM homework_file WHERE submission = $1 ORDER BY id DESC"#,
        submission
    )
    .fetch_all(db)
    .await?)
}

/// How many files `submission` holds. Counting the rows (not the counter
/// column) keeps it identical to `NoteFile`'s proven cap check; at a ceiling
/// of 10 the cost is nil.
pub async fn count_for_submission(
    db: &Database,
    submission: &crate::domain::homework_submission::HomeworkSubmissionId,
) -> Result<usize, AppError> {
    let row = sqlx::query!(
        r#"SELECT count(*) AS "count: i64" FROM homework_file WHERE submission = $1"#,
        submission
    )
    .fetch_one(db)
    .await?;
    Ok(row.count as usize)
}

/// The blob names behind every file of every submission to `homework` —
/// the GC keys of a homework delete (collected inside the delete's own
/// transaction by [`crate::db::homework::delete`]; this read backs it and
/// the tests).
pub async fn file_keys_for_homework(
    db: &Database,
    homework: &HomeworkId,
) -> Result<Vec<String>, AppError> {
    Ok(sqlx::query!(
        r#"SELECT file FROM homework_file
           WHERE submission IN (SELECT id FROM homework_submission WHERE homework = $1)"#,
        homework
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|row| row.file)
    .collect())
}

/// The blob names behind every homework file of `course` — collected
/// *before* the course-delete cascade wipes the rows.
pub async fn file_keys_for_course(
    db: &Database,
    course: &CourseId,
) -> Result<Vec<String>, AppError> {
    Ok(sqlx::query!(
        r#"SELECT file FROM homework_file
           WHERE submission IN (
               SELECT id FROM homework_submission
               WHERE homework IN (SELECT id FROM homework WHERE course = $1)
           )"#,
        course
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|row| row.file)
    .collect())
}

/// Delete the row and give its slot back in the same transaction. The
/// submission survives, so its counter has to be corrected; the cascades
/// that delete the submission itself take the counter with it.
///
/// The gate is the first statement: a conditional write on the *submission*
/// row — the "last touched" re-stamp a file delete owes the late flag
/// anyway. Its row lock is also the serialization point against an upload's
/// seat claim on the same row: one of the two waits, and the winner re-reads
/// the counter before moving it, so no lost round and no 500 — a grade
/// landing concurrently either stamps first (this delete is refused,
/// `Ok(None)`) or stamps after (the file was already gone when it graded).
/// `Err(NotFound)` still means the file row itself had vanished.
pub async fn delete(db: &Database, file: HomeworkFile) -> Result<Option<HomeworkFile>, AppError> {
    let now = Timestamp::now();
    tx_with_retry(db, false, async |tx| {
        let open = sqlx::query!(
            r#"UPDATE homework_submission SET updated_at = $2
               WHERE id = $1 AND graded_by_result IS NULL
               RETURNING 1 AS "open: i32""#,
            file.submission,
            now
        )
        .fetch_optional(&mut *tx)
        .await?;
        if open.is_none() {
            return Ok(None);
        }
        let gone = sqlx::query_as!(
            HomeworkFile,
            r#"DELETE FROM homework_file WHERE id = $1 AND submission = $2
               RETURNING id AS "id: HomeworkFileId",
                         submission AS "submission: HomeworkSubmissionId",
                         name AS "name: FileName",
                         content_type AS "content_type: FileContentType",
                         size,
                         file,
                         created_at AS "created_at: Timestamp""#,
            file.id,
            file.submission
        )
        .fetch_optional(&mut *tx)
        .await?;
        match gone {
            Some(row) => {
                sqlx::query!(
                    "UPDATE homework_submission SET file_count = GREATEST(file_count - 1, 0)
                     WHERE id = $1",
                    file.submission
                )
                .execute(&mut *tx)
                .await?;
                Ok(Some(row))
            }
            // The submission was open but the file had already vanished.
            None => Err(AppError::NotFound),
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::homework_submission::HomeworkSubmissionId;
    use crate::domain::note_file::{FileContentType, FileName};
    use crate::domain::user::UserId;

    /// A real homework row (with the subject it references): `upsert` reads the
    /// deadline off the entity now, so a bare id no longer does.
    async fn a_homework(db: &Database) -> crate::domain::homework::Homework {
        use crate::db::homework as hw;
        use crate::domain::homework::HomeworkTitle;
        use crate::domain::subject::{SubjectDescription, SubjectName};

        let course = crate::db::course::a_test_course(db).await;
        let subject = crate::db::subject::create(
            db,
            &course,
            SubjectName::try_new("topic").unwrap(),
            SubjectDescription::try_new("").unwrap(),
        )
        .await
        .unwrap();
        hw::create(
            db,
            &course,
            subject.get_id(),
            HomeworkTitle::try_new("essay").unwrap(),
            None,
            crate::domain::timestamp::Timestamp::from_millis(1),
            None,
            &UserId::from_key("teacher"),
        )
        .await
        .unwrap()
    }

    fn a_file(submission: &HomeworkSubmissionId) -> HomeworkFile {
        HomeworkFile::new(
            submission,
            FileName::try_new("answer.pdf").unwrap(),
            FileContentType::try_new("application/pdf").unwrap(),
            3,
        )
    }

    /// The counter as *stored* — the only witness that the seat and the row
    /// moved together, since every other read counts the rows themselves.
    async fn stored_count(submission: &HomeworkSubmissionId, db: &Database) -> i64 {
        db.query("SELECT VALUE file_count FROM $sub")
            .bind(("sub", submission.record()))
            .await
            .unwrap()
            .take::<Vec<i64>>(0)
            .unwrap()
            .into_iter()
            .next()
            .unwrap_or(0)
    }
    #[tokio::test]
    async fn rows_scope_to_their_submission_gc_and_cap() {
        let db = crate::database::init_mem().await.unwrap();
        let hw = a_homework(&db).await;
        let homework = hw.get_id().clone();
        let user = UserId::from_key("01TESTUSERAAAAAAAAAAAAAAAA");
        // A real submission row, so the GC join through it resolves.
        let submission = crate::db::homework_submission::upsert(&db, &hw, &user, None)
            .await
            .unwrap()
            .unwrap();
        let sub_a = submission.get_id().clone();
        let sub_b = HomeworkSubmissionId::composite(
            &homework,
            &UserId::from_key("01TESTUSERBBBBBBBBBBBBBBBB"),
        );

        let stored = insert(&db, a_file(&sub_a)).await.unwrap().unwrap();
        // Readable under its own submission, invisible under another.
        assert!(
            read_for(&db, stored.get_id(), &sub_a)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            read_for(&db, stored.get_id(), &sub_b)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(count_for_submission(&db, &sub_a).await.unwrap(), 1);
        assert_eq!(
            stored_count(&sub_a, &db).await,
            1,
            "the seat rode with the row"
        );

        // The grader's download scoping: found under its own homework, invisible
        // under another — so a teacher can't read it by naming a homework they
        // happen to manage.
        assert!(
            read_in_homework(&db, stored.get_id(), &homework)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            read_in_homework(
                &db,
                stored.get_id(),
                &HomeworkId::from_key("01TESTHWBBBBBBBBBBBBBBBBBB"),
            )
            .await
            .unwrap()
            .is_none()
        );

        // The blob name is collectable for GC before a cascade wipes the rows.
        let keys = file_keys_for_homework(&db, &homework).await.unwrap();
        assert_eq!(keys, vec![stored.get_file().to_string()]);

        // Fill to the cap, then the 11th is refused.
        for _ in 1..MAX_HOMEWORK_FILES_PER_SUBMISSION {
            insert(&db, a_file(&sub_a)).await.unwrap().unwrap();
        }
        assert!(matches!(
            insert(&db, a_file(&sub_a)).await,
            Err(AppError::Conflict(_))
        ));
        // The refusal wrote nothing at all: neither a row nor a seat.
        assert_eq!(
            count_for_submission(&db, &sub_a).await.unwrap() as i64,
            stored_count(&sub_a, &db).await,
        );
        assert_eq!(
            stored_count(&sub_a, &db).await,
            MAX_HOMEWORK_FILES_PER_SUBMISSION as i64
        );
    }

    /// A graded submission takes no more files and gives none up — decided by
    /// the same conditional write that claims the file slot, so no
    /// `homework_result` read stands between the check and the write.
    ///
    /// Bite check: drop [`SUBMISSION_OPEN_GUARD`] from [`insert`]'s
    /// [`cap::claim_when_and_create`] and the add below lands; drop it from
    /// [`delete`]'s gate and the delete below succeeds.
    #[tokio::test]
    async fn a_grade_freezes_the_files_too() {
        use crate::db::homework_result;
        use crate::domain::homework_result::HomeworkStatus;

        let db = crate::database::init_mem().await.unwrap();
        let hw = a_homework(&db).await;
        let homework = hw.get_id().clone();
        let user = UserId::from_key("01TESTUSERAAAAAAAAAAAAAAAA");
        let submission = crate::db::homework_submission::upsert(&db, &hw, &user, None)
            .await
            .unwrap()
            .unwrap();
        let sub = submission.get_id().clone();
        let stored = insert(&db, a_file(&sub)).await.unwrap().unwrap();

        homework_result::grade(
            &db,
            &homework,
            &user,
            HomeworkStatus::try_new("done").unwrap(),
            None,
            &UserId::from_key("01TESTTEACHERAAAAAAAAAAAAA"),
        )
        .await
        .unwrap();

        // Neither adding nor removing an attachment: `None` is the freeze, and
        // the cap's own `Err(Conflict)` stays distinct from it.
        assert!(insert(&db, a_file(&sub)).await.unwrap().is_none());
        assert!(delete(&db, stored.clone()).await.unwrap().is_none());
        assert_eq!(
            count_for_submission(&db, &sub).await.unwrap(),
            1,
            "the graded submission keeps exactly the files it was graded on"
        );
        // Refused by the *guard*, not the cap (nine seats free) — and still no
        // seat moved, so the counter matches the rows.
        assert_eq!(stored_count(&sub, &db).await, 1);
        // The refused add took no slot either, so un-grading gives back a
        // submission with room, not one that silently lost nine.
        homework_result::remove(&db, &homework, &user)
            .await
            .unwrap();
        for _ in 1..MAX_HOMEWORK_FILES_PER_SUBMISSION {
            insert(&db, a_file(&sub)).await.unwrap().unwrap();
        }
        assert!(delete(&db, stored).await.unwrap().is_some());
    }
}
