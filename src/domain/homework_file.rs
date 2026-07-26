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
use tokio::sync::Mutex;
use ulid::Ulid;

use crate::constant::{HOMEWORK_FILE_TABLE, MAX_HOMEWORK_FILES_PER_SUBMISSION};
use crate::database::Database;
use crate::domain::course::CourseId;
use crate::domain::homework::HomeworkId;
use crate::domain::homework_submission::HomeworkSubmissionId;
use crate::domain::note_file::{FileContentType, FileName};
use crate::domain::timestamp::Timestamp;
use crate::error::AppError;

/// Serializes the files-per-submission cap check against the insert (see
/// [`HomeworkFile::insert`]). Its own lock, not note files' `FILE_CAP_LOCK`:
/// the two caps are independent counts and must not needlessly contend on one
/// mutex. This backend is the database's only writer, so one process-wide lock
/// suffices.
// ponytail: global lock, per-submission locks if uploads ever see real contention.
static SUBMISSION_FILE_LOCK: Mutex<()> = Mutex::const_new(());

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct HomeworkFileId(RecordId);

impl HomeworkFileId {
    pub fn generate() -> Self {
        Self(RecordId::new(HOMEWORK_FILE_TABLE, Ulid::new().to_string()))
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
    /// already holds [`MAX_HOMEWORK_FILES_PER_SUBMISSION`]. The
    /// count-then-create runs under [`SUBMISSION_FILE_LOCK`]: a `BEGIN…COMMIT`
    /// can't enforce the cap because SurrealDB doesn't conflict-check a
    /// cross-record count against a concurrent insert (write-skew), the same
    /// story as `NoteFile::insert`.
    pub async fn insert(self, db: &Database) -> Result<HomeworkFile, AppError> {
        let _guard = SUBMISSION_FILE_LOCK.lock().await;
        if Self::count_for_submission(&self.submission, db).await?
            >= MAX_HOMEWORK_FILES_PER_SUBMISSION
        {
            return Err(AppError::Conflict(
                "the submission already holds the maximum of 10 files — delete one first",
            ));
        }
        // whole-row-save-ok: create of a fresh ULID row built in place by `new` — there is no prior row to clobber
        let created: Option<HomeworkFile> = db.create(self.id.record()).content(self).await?;
        created.ok_or_else(|| AppError::Internal("failed to create homework file".into()))
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

    pub async fn delete(self, db: &Database) -> Result<HomeworkFile, AppError> {
        let deleted: Option<HomeworkFile> = db.delete(self.id.record()).await?;
        deleted.ok_or(AppError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::homework_submission::HomeworkSubmission;
    use crate::domain::user::UserId;

    fn a_file(submission: &HomeworkSubmissionId) -> HomeworkFile {
        HomeworkFile::new(
            submission,
            FileName::try_new("answer.pdf").unwrap(),
            FileContentType::try_new("application/pdf").unwrap(),
            3,
        )
    }

    #[tokio::test]
    async fn rows_scope_to_their_submission_gc_and_cap() {
        let db = crate::database::init_mem().await.unwrap();
        let homework = HomeworkId::from_key("01TESTHWAAAAAAAAAAAAAAAAAA");
        let user = UserId::from_key("01TESTUSERAAAAAAAAAAAAAAAA");
        // A real submission row, so the GC join through it resolves.
        let submission = HomeworkSubmission::upsert(&homework, &user, None, &db)
            .await
            .unwrap();
        let sub_a = submission.get_id().clone();
        let sub_b = HomeworkSubmissionId::composite(
            &homework,
            &UserId::from_key("01TESTUSERBBBBBBBBBBBBBBBB"),
        );

        let stored = a_file(&sub_a).insert(&db).await.unwrap();
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
            a_file(&sub_a).insert(&db).await.unwrap();
        }
        assert!(matches!(
            a_file(&sub_a).insert(&db).await,
            Err(AppError::Conflict(_))
        ));
    }
}
