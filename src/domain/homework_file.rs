//! A file attached to a homework submission. Like a note file, the row carries
//! metadata only (original filename, MIME type, byte size) and the bytes live
//! on disk under [`crate::config::Config::files_path`] — but named by this
//! row's own `file` field (a fresh server-generated ULID per upload, like a
//! question image), never by user input, so nothing a client sends shapes a
//! disk path. Files are immutable: created and deleted, never updated, so the
//! `READONLY` `submission`/`file`/`created_at` columns are only ever set once.
//! The web layer owns the blob I/O and its ordering (blob before row on
//! upload, row before blob on delete); this module owns the rows.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::{
    HOMEWORK_FILE_TABLE, MAX_HOMEWORK_FILES_PER_SUBMISSION, SUBMISSION_FILE_COUNT_FIELD,
    SUBMISSION_OPEN_GUARD,
};
use crate::database::{Database, transaction_with_retry};
use crate::db::cap;
use crate::domain::course::CourseId;
use crate::domain::homework::HomeworkId;
use crate::domain::homework_submission::{HomeworkSubmission, HomeworkSubmissionId};
use crate::domain::monotonic_id::next_ulid;
use crate::domain::note_file::{FileContentType, FileName};
use crate::domain::timestamp::Timestamp;
use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct HomeworkFileId(RecordId);

impl HomeworkFileId {
    /// Minted from the process-wide monotonic generator, not `Ulid::new()`:
    /// a submission's files list `id DESC` (newest first, [`HomeworkFile::list_for_submission`]),
    /// and a random low half scrambles rows minted in the same millisecond.
    pub fn generate() -> Self {
        Self(RecordId::new(HOMEWORK_FILE_TABLE, next_ulid().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(HOMEWORK_FILE_TABLE, key))
    }

    pub fn record(&self) -> RecordId {
        self.0.clone()
    }

    pub fn key(&self) -> &str {
        match &self.0.key {
            RecordIdKey::String(key) => key,
            _ => "",
        }
    }
}

/// An attachment row. `file` is the blob's on-disk name (a fresh ULID per
/// upload), independent of the row id and reused as the GC key on cascade.
#[derive(Debug, Clone, SurrealValue)]
pub struct HomeworkFile {
    id: HomeworkFileId,
    submission: HomeworkSubmissionId,
    name: FileName,
    content_type: FileContentType,
    size: i64,
    file: String,
    created_at: Timestamp,
}

/// What [`HomeworkFile::delete`]'s transaction reports: whether the submission
/// was still open (`open` = 1, the gate bit) and the row it then removed. Two
/// answers in one object because an empty `gone` alone cannot say whether the
/// delete was refused or the file had simply vanished.
#[derive(Debug, SurrealValue)]
struct DeleteOutcome {
    open: i64,
    gone: Vec<HomeworkFile>,
}

impl HomeworkFile {
    /// Assemble a row (fresh blob name generated here) without persisting it.
    /// The caller writes the blob under [`Self::get_file`] first, then calls
    /// [`Self::insert`] — so a stored row always points at a real blob.
    pub fn new(
        submission: &HomeworkSubmissionId,
        name: FileName,
        content_type: FileContentType,
        size: i64,
    ) -> Self {
        Self {
            id: HomeworkFileId::generate(),
            submission: submission.clone(),
            name,
            content_type,
            size,
            file: Ulid::new().to_string(),
            created_at: Timestamp::now(),
        }
    }

    pub fn get_id(&self) -> &HomeworkFileId {
        &self.id
    }

    pub fn get_submission(&self) -> &HomeworkSubmissionId {
        &self.submission
    }

    pub fn get_name(&self) -> &FileName {
        &self.name
    }

    pub fn get_content_type(&self) -> &FileContentType {
        &self.content_type
    }

    pub fn get_size(&self) -> i64 {
        self.size
    }

    /// The blob's on-disk name — a fresh ULID, so no user input shapes a path.
    pub fn get_file(&self) -> &str {
        &self.file
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }

    /// Persist the row assembled by [`Self::new`], refusing once its submission
    /// already holds [`MAX_HOMEWORK_FILES_PER_SUBMISSION`] (an `Err(Conflict)`)
    /// or once a grade has frozen it (`Ok(None)`, so the web layer keeps its own
    /// wording). Both are decided by one [`cap::claim_when_and_create`] on the
    /// submission row — the seat and the file row commit together, so a crash
    /// between them can no longer leave a slot claimed by a file that does not
    /// exist, and a losing upload never has to be un-counted. Which of the two
    /// conditions refused it is read back afterwards, off the losing path only,
    /// and only to pick the message: `Claimed::Full` says "full *or* graded *or*
    /// the submission is gone".
    pub async fn insert(self, db: &Database) -> Result<Option<HomeworkFile>, AppError> {
        // whole-row-save-ok: create of a fresh ULID row built in place by `new` — there is no prior row to clobber
        match cap::claim_when_and_create(
            &self.submission.record(),
            SUBMISSION_FILE_COUNT_FIELD,
            MAX_HOMEWORK_FILES_PER_SUBMISSION as i64,
            SUBMISSION_OPEN_GUARD,
            &self.id.record(),
            &self,
            db,
        )
        .await?
        {
            cap::Claimed::Made(created) => Ok(Some(created)),
            cap::Claimed::Full => {
                if HomeworkSubmission::is_graded(&self.submission, db).await? {
                    return Ok(None);
                }
                Err(AppError::Conflict(
                    "the submission already holds the maximum of 10 files — delete one first",
                ))
            }
            // The id is a fresh ULID minted by `new`, so a row already holding it
            // is a collision, not a re-upload.
            cap::Claimed::Duplicate => Err(AppError::Internal("homework file id collided".into())),
        }
    }

    /// Read a file's row only if it belongs to `submission` — callers have
    /// already checked the submission belongs to the requesting user.
    pub async fn read_for(
        id: &HomeworkFileId,
        submission: &HomeworkSubmissionId,
        db: &Database,
    ) -> Result<Option<HomeworkFile>, AppError> {
        let file: Option<HomeworkFile> = db.select(id.record()).await?;
        Ok(file.filter(|file| &file.submission == submission))
    }

    /// Read a file by id only if it hangs off a submission to `homework` — the
    /// grader's download scoping. The teacher download path carries the homework
    /// id but not the owning student (unlike `read_for`, which needs the
    /// submission), so this both finds the file and confirms it belongs under
    /// `homework`: a teacher can't pass a homework they manage to read a file
    /// from a different (perhaps unmanaged) one.
    pub async fn read_in_homework(
        id: &HomeworkFileId,
        homework: &HomeworkId,
        db: &Database,
    ) -> Result<Option<HomeworkFile>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM homework_file WHERE id = $id AND submission IN \
                 (SELECT VALUE id FROM homework_submission WHERE homework = $hw)",
            )
            .bind(("id", id.record()))
            .bind(("hw", homework.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<HomeworkFile>>(0)?.into_iter().next())
    }

    /// All of `submission`'s files, newest first.
    pub async fn list_for_submission(
        submission: &HomeworkSubmissionId,
        db: &Database,
    ) -> Result<Vec<HomeworkFile>, AppError> {
        let mut result = db
            .query("SELECT * FROM homework_file WHERE submission = $sub ORDER BY id DESC")
            .bind(("sub", submission.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<HomeworkFile>>(0)?)
    }

    /// How many files `submission` holds — the cap check reads this under the
    /// lock. Counting via the rows (not a `count()` query) keeps it identical
    /// to `NoteFile`'s proven cap check; at a ceiling of 10 the cost is nil.
    pub async fn count_for_submission(
        submission: &HomeworkSubmissionId,
        db: &Database,
    ) -> Result<usize, AppError> {
        Ok(Self::list_for_submission(submission, db).await?.len())
    }

    /// The blob names behind every file of every submission to `homework` —
    /// collected *before* the homework-delete cascade wipes the rows, so the
    /// web layer can unlink them once the rows are gone.
    pub async fn file_keys_for_homework(
        homework: &HomeworkId,
        db: &Database,
    ) -> Result<Vec<String>, AppError> {
        let mut result = db
            .query(
                "SELECT VALUE file FROM homework_file \
                 WHERE submission IN (SELECT VALUE id FROM homework_submission WHERE homework = $hw)",
            )
            .bind(("hw", homework.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<String>>(0)?)
    }

    /// The blob names behind every homework file of `course` — collected
    /// *before* the course-delete cascade wipes the rows.
    pub async fn file_keys_for_course(
        course: &CourseId,
        db: &Database,
    ) -> Result<Vec<String>, AppError> {
        let mut result = db
            .query(
                "SELECT VALUE file FROM homework_file WHERE submission IN ( \
                   SELECT VALUE id FROM homework_submission \
                   WHERE homework IN (SELECT VALUE id FROM homework WHERE course = $course) \
                 )",
            )
            .bind(("course", course.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<String>>(0)?)
    }

    /// Delete the row and give its slot back in the same transaction. The
    /// submission survives, so its counter has to be corrected; the cascades
    /// that delete the submission itself take the counter with it.
    ///
    /// The gate is the first statement: a conditional write on the *submission*
    /// row — the "last touched" re-stamp a file delete owes the late flag
    /// anyway — carrying [`SUBMISSION_OPEN_GUARD`]. Nothing else in the
    /// transaction runs unless it bit, so a grade landing concurrently either
    /// stamps first (this delete is refused, `Ok(None)`) or stamps after (the
    /// file was already gone when it graded). `Err(NotFound)` still means the
    /// file row itself had vanished.
    ///
    /// Sound to re-send while the store answers "conflict, retry", and it has
    /// to be: the gate writes the *submission* row, the very record an upload's
    /// [`HomeworkFile::insert`] claims its seat on, and both handlers hold only
    /// `HOMEWORK_LOCK.read()` — so a concurrent add and delete of two files of
    /// one submission contend by design, and a lost round used to come back as
    /// a 500 on a request that had written nothing. Only `UPDATE` and `DELETE`
    /// are in the batch, and neither can legitimately answer "already exists",
    /// which is what makes the whole of it re-sendable.
    pub async fn delete(self, db: &Database) -> Result<Option<HomeworkFile>, AppError> {
        let (mut result, mut errors) = transaction_with_retry(
            db,
            &format!(
                "BEGIN TRANSACTION;
                 LET $open = (UPDATE $sub SET updated_at = $now \
                     WHERE {SUBMISSION_OPEN_GUARD} RETURN VALUE id);
                 LET $gone = IF array::len($open) > 0 {{ (DELETE $id RETURN BEFORE) }} ELSE {{ [] }};
                 UPDATE $sub SET file_count = math::max([(file_count ?? 0) - array::len($gone), 0]);
                 RETURN {{ open: array::len($open), gone: $gone }};
                 COMMIT TRANSACTION;"
            ),
            &[
                ("id".into(), self.id.record().into_value()),
                ("sub".into(), self.submission.record().into_value()),
                ("now".into(), Timestamp::now().into_value()),
            ],
            &[],
        )
        .await?;
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        // BEGIN is slot 0, the two LETs slots 1-2 and the counter fix slot 3;
        // the RETURN is slot 4.
        let outcome: Option<DeleteOutcome> =
            result.take::<Vec<DeleteOutcome>>(4)?.into_iter().next();
        let outcome =
            outcome.ok_or_else(|| AppError::Internal("failed to delete homework file".into()))?;
        if outcome.open == 0 {
            return Ok(None);
        }
        outcome
            .gone
            .into_iter()
            .next()
            .map(Some)
            .ok_or(AppError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::homework_submission::HomeworkSubmission;
    use crate::domain::user::UserId;

    /// A real homework row (with the subject it references): `upsert` reads the
    /// deadline off the entity now, so a bare id no longer does.
    async fn a_homework(db: &Database) -> crate::domain::homework::Homework {
        use crate::domain::homework::{Homework, HomeworkTitle};
        use crate::domain::subject::{Subject, SubjectDescription, SubjectName};

        let course = crate::db::course::a_test_course(db).await;
        let subject = Subject::create(
            &course,
            SubjectName::try_new("topic").unwrap(),
            SubjectDescription::try_new("").unwrap(),
            db,
        )
        .await
        .unwrap();
        Homework::create(
            &course,
            subject.get_id(),
            HomeworkTitle::try_new("essay").unwrap(),
            None,
            crate::domain::timestamp::Timestamp::from_millis(1),
            None,
            &UserId::from_key("teacher"),
            db,
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
        let submission = HomeworkSubmission::upsert(&hw, &user, None, &db)
            .await
            .unwrap()
            .unwrap();
        let sub_a = submission.get_id().clone();
        let sub_b = HomeworkSubmissionId::composite(
            &homework,
            &UserId::from_key("01TESTUSERBBBBBBBBBBBBBBBB"),
        );

        let stored = a_file(&sub_a).insert(&db).await.unwrap().unwrap();
        // Readable under its own submission, invisible under another.
        assert!(
            HomeworkFile::read_for(stored.get_id(), &sub_a, &db)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            HomeworkFile::read_for(stored.get_id(), &sub_b, &db)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            HomeworkFile::count_for_submission(&sub_a, &db)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            stored_count(&sub_a, &db).await,
            1,
            "the seat rode with the row"
        );

        // The grader's download scoping: found under its own homework, invisible
        // under another — so a teacher can't read it by naming a homework they
        // happen to manage.
        assert!(
            HomeworkFile::read_in_homework(stored.get_id(), &homework, &db)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            HomeworkFile::read_in_homework(
                stored.get_id(),
                &HomeworkId::from_key("01TESTHWBBBBBBBBBBBBBBBBBB"),
                &db,
            )
            .await
            .unwrap()
            .is_none()
        );

        // The blob name is collectable for GC before a cascade wipes the rows.
        let keys = HomeworkFile::file_keys_for_homework(&homework, &db)
            .await
            .unwrap();
        assert_eq!(keys, vec![stored.get_file().to_string()]);

        // Fill to the cap, then the 11th is refused.
        for _ in 1..MAX_HOMEWORK_FILES_PER_SUBMISSION {
            a_file(&sub_a).insert(&db).await.unwrap().unwrap();
        }
        assert!(matches!(
            a_file(&sub_a).insert(&db).await,
            Err(AppError::Conflict(_))
        ));
        // The refusal wrote nothing at all: neither a row nor a seat.
        assert_eq!(
            HomeworkFile::count_for_submission(&sub_a, &db)
                .await
                .unwrap() as i64,
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
    /// Bite check: drop [`SUBMISSION_OPEN_GUARD`] from `insert`'s
    /// [`cap::claim_when_and_create`] and the add below lands; drop it from
    /// `delete`'s gate and the delete below succeeds.
    #[tokio::test]
    async fn a_grade_freezes_the_files_too() {
        use crate::domain::homework_result::{HomeworkResult, HomeworkStatus};

        let db = crate::database::init_mem().await.unwrap();
        let hw = a_homework(&db).await;
        let homework = hw.get_id().clone();
        let user = UserId::from_key("01TESTUSERAAAAAAAAAAAAAAAA");
        let submission = HomeworkSubmission::upsert(&hw, &user, None, &db)
            .await
            .unwrap()
            .unwrap();
        let sub = submission.get_id().clone();
        let stored = a_file(&sub).insert(&db).await.unwrap().unwrap();

        HomeworkResult::grade(
            &homework,
            &user,
            HomeworkStatus::try_new("done").unwrap(),
            None,
            &UserId::from_key("01TESTTEACHERAAAAAAAAAAAAA"),
            &db,
        )
        .await
        .unwrap();

        // Neither adding nor removing an attachment: `None` is the freeze, and
        // the cap's own `Err(Conflict)` stays distinct from it.
        assert!(a_file(&sub).insert(&db).await.unwrap().is_none());
        assert!(stored.clone().delete(&db).await.unwrap().is_none());
        assert_eq!(
            HomeworkFile::count_for_submission(&sub, &db).await.unwrap(),
            1,
            "the graded submission keeps exactly the files it was graded on"
        );
        // Refused by the *guard*, not the cap (nine seats free) — and still no
        // seat moved, so the counter matches the rows.
        assert_eq!(stored_count(&sub, &db).await, 1);
        // The refused add took no slot either, so un-grading gives back a
        // submission with room, not one that silently lost nine.
        HomeworkResult::remove(&homework, &user, &db).await.unwrap();
        for _ in 1..MAX_HOMEWORK_FILES_PER_SUBMISSION {
            a_file(&sub).insert(&db).await.unwrap().unwrap();
        }
        assert!(stored.delete(&db).await.unwrap().is_some());
    }
}
