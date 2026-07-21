//! A student's drawn answer to an exam question — their own illustration for
//! one (exam, user, question), mirroring [`crate::domain::question_image`] but
//! keyed by the answering student instead of a choice slot. The row carries
//! metadata; the bytes live on disk under [`crate::config::Config::files_path`]
//! in a file named by `file` — a fresh server-generated ULID per upload, so no
//! user input ever shapes a disk path and a replace never overwrites bytes in
//! place. The row id is *deterministic* per (question, user) — the same shape
//! as [`crate::domain::exam_answer::ExamAnswerId::composite`] — so "one drawing
//! per student per question" holds by construction and a replace is a plain
//! UPSERT. The web layer owns the blob I/O and its ordering (new blob before
//! row, row before old blob); this module owns the rows.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::database::{ANSWER_IMAGE_TABLE, Database};
use crate::domain::course::CourseId;
use crate::domain::exam::ExamId;
use crate::domain::exam_question::ExamQuestionId;
use crate::domain::note_file::FileContentType;
use crate::domain::user::UserId;
use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct AnswerImageId(RecordId);

impl AnswerImageId {
    /// The one id a (question, user) pair can have: `{question}_{user}` — the
    /// same deterministic shape as [`crate::domain::exam_answer::ExamAnswerId::composite`],
    /// so "one drawing per student per question" needs no index. ULID keys are
    /// alphanumeric, so `_` is an unambiguous joiner.
    pub fn composite(question: &ExamQuestionId, user: &UserId) -> Self {
        Self(RecordId::new(
            ANSWER_IMAGE_TABLE,
            format!("{}_{}", question.key(), user.key()),
        ))
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

/// One student's drawing for one question. `exam` is denormalized (like
/// [`crate::domain::exam_answer::ExamAnswer`]) so per-exam reads (cascade) and
/// per-(exam, user) reads (sitting/grading views, retake blob-GC) don't fan out
/// through `exam_question`.
#[derive(Debug, Clone, SurrealValue)]
pub struct AnswerImage {
    id: AnswerImageId,
    exam: ExamId,
    question: ExamQuestionId,
    user: UserId,
    /// The blob's on-disk name — a fresh ULID every upload.
    file: String,
    content_type: FileContentType,
    size: i64,
}

impl AnswerImage {
    /// Assemble a row (fresh blob name generated here) without persisting it.
    /// The caller writes the blob under [`Self::get_file`] first, then calls
    /// [`Self::upsert`] — so a stored row always points at a real blob.
    pub fn new(
        exam: &ExamId,
        question: &ExamQuestionId,
        user: &UserId,
        content_type: FileContentType,
        size: i64,
    ) -> Self {
        Self {
            id: AnswerImageId::composite(question, user),
            exam: exam.clone(),
            question: question.clone(),
            user: user.clone(),
            file: Ulid::new().to_string(),
            content_type,
            size,
        }
    }

    pub fn get_question(&self) -> &ExamQuestionId {
        &self.question
    }

    pub fn get_file(&self) -> &str {
        &self.file
    }

    pub fn get_content_type(&self) -> &FileContentType {
        &self.content_type
    }

    pub fn get_size(&self) -> i64 {
        self.size
    }

    /// Create or replace the student's drawing for the question — the
    /// deterministic id makes this the whole "one drawing per student per
    /// question" story.
    pub async fn upsert(self, db: &Database) -> Result<AnswerImage, AppError> {
        let written: Option<AnswerImage> = db.upsert(self.id.record()).content(self).await?;
        written.ok_or_else(|| AppError::Internal("failed to store answer image".into()))
    }

    /// The student's drawing for one question, if any.
    pub async fn read(
        question: &ExamQuestionId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<AnswerImage>, AppError> {
        Ok(db
            .select(AnswerImageId::composite(question, user).record())
            .await?)
    }

    /// Every answer drawing of the exam — one query for the exam-delete cascade.
    pub async fn list_for_exam(exam: &ExamId, db: &Database) -> Result<Vec<AnswerImage>, AppError> {
        let mut result = db
            .query("SELECT * FROM answer_image WHERE exam = $ex")
            .bind(("ex", exam.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<AnswerImage>>(0)?)
    }

    /// One student's answer drawings across an exam — the parallel to
    /// [`crate::domain::exam_answer::ExamAnswer::list_for_exam_user`], feeding
    /// the sitting/grading answer-image maps and the retake blob-GC.
    pub async fn list_for_exam_user(
        exam: &ExamId,
        user: &UserId,
        db: &Database,
    ) -> Result<Vec<AnswerImage>, AppError> {
        let mut result = db
            .query("SELECT * FROM answer_image WHERE exam = $ex AND user = $usr")
            .bind(("ex", exam.record()))
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<AnswerImage>>(0)?)
    }

    /// The blob names behind every answer drawing of every exam of `course` —
    /// collected *before* the course-delete cascade wipes the rows.
    pub async fn file_keys_for_course(
        course: &CourseId,
        db: &Database,
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

    pub async fn delete(self, db: &Database) -> Result<AnswerImage, AppError> {
        let deleted: Option<AnswerImage> = db.delete(self.id.record()).await?;
        deleted.ok_or(AppError::NotFound)
    }

    /// Drop one student's answer drawings across an exam — a retake starts from
    /// a blank sheet. Rows only; the caller GCs the blobs. The retake path itself
    /// wipes rows inside `ExamAttempt::wipe_and_create`'s transaction; this is
    /// the standalone mirror of [`crate::domain::exam_answer::ExamAnswer::delete_for_exam_user`].
    pub async fn delete_for_exam_user(
        exam: &ExamId,
        user: &UserId,
        db: &Database,
    ) -> Result<(), AppError> {
        db.query("DELETE answer_image WHERE exam = $ex AND user = $usr")
            .bind(("ex", exam.record()))
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> FileContentType {
        FileContentType::try_new("image/png").unwrap()
    }

    fn student() -> UserId {
        UserId::from_key("01TESTSTUDENTAAAAAAAAAAAAA")
    }

    #[tokio::test]
    async fn upsert_replaces_per_student_and_question() {
        let db = crate::database::init_mem().await.unwrap();
        let exam = ExamId::generate();
        let question = ExamQuestionId::generate();
        let user = student();

        let first = AnswerImage::new(&exam, &question, &user, png(), 3)
            .upsert(&db)
            .await
            .unwrap();
        let second = AnswerImage::new(&exam, &question, &user, png(), 5)
            .upsert(&db)
            .await
            .unwrap();
        // Same (question, user), same row — the replace swapped the blob pointer.
        assert_ne!(first.get_file(), second.get_file());
        let rows = AnswerImage::list_for_exam_user(&exam, &user, &db)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_size(), 5);

        // Another student's drawing for the same question is its own row.
        let other = UserId::from_key("01TESTSTUDENTBBBBBBBBBBBBB");
        AnswerImage::new(&exam, &question, &other, png(), 7)
            .upsert(&db)
            .await
            .unwrap();
        assert_eq!(AnswerImage::list_for_exam(&exam, &db).await.unwrap().len(), 2);
        assert!(
            AnswerImage::read(&question, &user, &db)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn listing_scopes_by_exam_and_user() {
        let db = crate::database::init_mem().await.unwrap();
        let exam_a = ExamId::generate();
        let exam_b = ExamId::generate();
        let user = student();
        AnswerImage::new(&exam_a, &ExamQuestionId::generate(), &user, png(), 1)
            .upsert(&db)
            .await
            .unwrap();
        assert_eq!(
            AnswerImage::list_for_exam_user(&exam_a, &user, &db)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            AnswerImage::list_for_exam(&exam_b, &db)
                .await
                .unwrap()
                .is_empty()
        );

        // delete_for_exam_user clears the student's sheet.
        AnswerImage::delete_for_exam_user(&exam_a, &user, &db)
            .await
            .unwrap();
        assert!(
            AnswerImage::list_for_exam_user(&exam_a, &user, &db)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
