//! One student's submission to a homework: an optional free-text note plus any
//! number of files ([`crate::domain::homework_file`]). The row id is the
//! deterministic `{homework}_{user}` composite, so a student has exactly one
//! submission per homework by construction and a re-submit is a single atomic
//! UPSERT.
//!
//! `submitted_at` — the first-submit stamp — is `READONLY` and survives every
//! re-submit; [`HomeworkSubmission::upsert`] preserves it inside one statement
//! rather than through `.content()`, see there. `updated_at` moves to now on
//! every (re-)submit (and, later, on a file add/delete) and drives the computed
//! "late" flag (`updated_at > homework.due_at`, judged in the web layer, never
//! stored): `submitted_at` answers "was the first hand-in on time", `updated_at`
//! "was it touched after the deadline".

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{
    HOMEWORK_SUBMISSION_TABLE, MAX_HOMEWORK_TEXT_LEN, SUBMISSION_GRADED_FIELD,
    SUBMISSION_OPEN_GUARD,
};
use crate::database::Database;
use crate::domain::homework::HomeworkId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_optional;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct HomeworkSubmissionId(RecordId);

impl HomeworkSubmissionId {
    /// The one id a (homework, user) pair can have. A deterministic composite
    /// means one submission per pair with no unique index and no
    /// find-then-insert race — a re-submit UPSERTs the same row. ULID keys are
    /// alphanumeric, so `_` is an unambiguous joiner.
    pub fn composite(homework: &HomeworkId, user: &UserId) -> Self {
        Self(RecordId::new(
            HOMEWORK_SUBMISSION_TABLE,
            format!("{}_{}", homework.key(), user.key()),
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

/// A submission's optional free-text note: may be empty, at most
/// `MAX_HOMEWORK_TEXT_LEN` characters. Files ride separately.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct SubmissionText(String);

impl SubmissionText {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_optional("text", value, MAX_HOMEWORK_TEXT_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct HomeworkSubmission {
    id: HomeworkSubmissionId,
    homework: HomeworkId,
    user: UserId,
    text: Option<SubmissionText>,
    submitted_at: Timestamp,
    updated_at: Timestamp,
}

impl HomeworkSubmission {
    pub fn get_id(&self) -> &HomeworkSubmissionId {
        &self.id
    }

    pub fn get_homework(&self) -> &HomeworkId {
        &self.homework
    }

    pub fn get_user(&self) -> &UserId {
        &self.user
    }

    pub fn get_text(&self) -> Option<&SubmissionText> {
        self.text.as_ref()
    }

    pub fn get_submitted_at(&self) -> Timestamp {
        self.submitted_at
    }

    pub fn get_updated_at(&self) -> Timestamp {
        self.updated_at
    }

    /// Create or re-stamp `user`'s submission to `homework`, unless a grade has
    /// frozen it — `None` means frozen, and the caller answers 409. `updated_at`
    /// moves to now every time; `submitted_at` (readonly, the first-submit stamp)
    /// is kept on an existing row and set only on a fresh one.
    ///
    /// Hand-written rather than `.content()` on purpose: `.content()` re-sends
    /// the whole row, so it would carry a `submitted_at`, and any value differing
    /// from the stored one trips the readonly guard. The
    /// `submitted_at = submitted_at ?? $now` expression preserves it inside the
    /// one UPSERT statement — race-free without a lock, where a read-then-content
    /// could let two concurrent first-submits pick different stamps and 500.
    ///
    /// The `WHERE` is the freeze: [`SUBMISSION_OPEN_GUARD`] makes "not graded
    /// yet" a condition of this very write instead of a read the grading replica
    /// can land behind. An absent row satisfies it (the stamp is `NONE` there
    /// too), so a first submit still creates.
    pub async fn upsert(
        homework: &HomeworkId,
        user: &UserId,
        text: Option<SubmissionText>,
        db: &Database,
    ) -> Result<Option<HomeworkSubmission>, AppError> {
        let id = HomeworkSubmissionId::composite(homework, user);
        let now = Timestamp::now();
        let text = text.map(|text| text.as_str().to_string());
        let mut saved = db
            .query(format!(
                "UPSERT $id SET homework = $hw, user = $usr, text = $text, \
                 updated_at = $now, submitted_at = submitted_at ?? $now \
                 WHERE {SUBMISSION_OPEN_GUARD} RETURN AFTER"
            ))
            .bind(("id", id.record()))
            .bind(("hw", homework.record()))
            .bind(("usr", user.record()))
            .bind(("text", text))
            .bind(("now", now))
            .await?
            .check()?;
        Ok(saved.take::<Vec<HomeworkSubmission>>(0)?.into_iter().next())
    }

    /// Re-stamp a submission's `updated_at` to now, leaving its text and files
    /// alone. A file add or delete modifies the submission as a whole, so its
    /// "last touched" clock — which drives the computed late flag — must move
    /// even though the text row is unchanged. Static because the file paths hold
    /// the composite id, not always the row. A targeted single-field UPDATE, so
    /// the `READONLY` `submitted_at` is never re-sent (a `.content()` would trip
    /// its guard). Returns the re-stamped row, or a 404 if it has since vanished.
    pub async fn touch(
        id: &HomeworkSubmissionId,
        db: &Database,
    ) -> Result<HomeworkSubmission, AppError> {
        let now = Timestamp::now();
        let mut result = db
            .query("UPDATE $id SET updated_at = $now RETURN AFTER")
            .bind(("id", id.record()))
            .bind(("now", now))
            .await?
            .check()?;
        result
            .take::<Vec<HomeworkSubmission>>(0)?
            .into_iter()
            .next()
            .ok_or(AppError::NotFound)
    }

    /// Whether a grade has frozen this submission. Only ever read to tell two
    /// refusals apart *after* a conditional write has already refused one — the
    /// stamp on the row, never this read, is what decides.
    pub async fn is_graded(id: &HomeworkSubmissionId, db: &Database) -> Result<bool, AppError> {
        let mut result = db
            .query(format!(
                "SELECT VALUE id FROM $id WHERE {SUBMISSION_GRADED_FIELD} != NONE"
            ))
            .bind(("id", id.record()))
            .await?
            .check()?;
        Ok(!result.take::<Vec<HomeworkSubmissionId>>(0)?.is_empty())
    }

    /// `user`'s submission to `homework`, if they have one.
    pub async fn read_for(
        homework: &HomeworkId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<HomeworkSubmission>, AppError> {
        Ok(db
            .select(HomeworkSubmissionId::composite(homework, user).record())
            .await?)
    }

    /// Every submission to `homework`, in student (composite-id) order — the
    /// roster's raw rows.
    pub async fn list_for_homework(
        homework: &HomeworkId,
        db: &Database,
    ) -> Result<Vec<HomeworkSubmission>, AppError> {
        let mut result = db
            .query("SELECT * FROM homework_submission WHERE homework = $hw ORDER BY id ASC")
            .bind(("hw", homework.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<HomeworkSubmission>>(0)?)
    }

    /// Delete this submission and its files in one transaction — so a crash
    /// can't leave a file pointing at a vanished submission. The file *blobs*
    /// are the web layer's to unlink: it lists them
    /// ([`crate::domain::homework_file::HomeworkFile::list_for_submission`])
    /// before calling this. `None` means a grade froze the row (the caller
    /// answers 409): the submission's own delete carries
    /// [`SUBMISSION_OPEN_GUARD`], and the file wipe is conditional on it having
    /// bitten, so a refused delete leaves the children standing too.
    pub async fn delete(self, db: &Database) -> Result<Option<HomeworkSubmission>, AppError> {
        let mut result = db
            .query(format!(
                "BEGIN TRANSACTION;
                 LET $gone = (DELETE $sub WHERE {SUBMISSION_OPEN_GUARD} RETURN BEFORE);
                 IF array::len($gone) > 0 {{ DELETE homework_file WHERE submission = $sub }};
                 RETURN $gone;
                 COMMIT TRANSACTION;"
            ))
            .bind(("sub", self.id.record()))
            .await?
            .check()?;
        // BEGIN is slot 0, the LET slot 1 and the IF slot 2; the RETURN is slot 3.
        Ok(result
            .take::<Vec<HomeworkSubmission>>(3)?
            .into_iter()
            .next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn text_is_optional_but_bounded() {
        assert!(SubmissionText::try_new("").is_ok());
        assert!(SubmissionText::try_new(&"x".repeat(5_000)).is_ok());
        assert!(SubmissionText::try_new(&"x".repeat(5_001)).is_err());
    }

    #[tokio::test]
    async fn resubmit_keeps_submitted_at_and_restamps_updated_at() {
        let db = crate::database::init_mem().await.unwrap();
        let homework = HomeworkId::generate();
        let user = UserId::from_key("01TESTUSERAAAAAAAAAAAAAAAA");

        let first = HomeworkSubmission::upsert(
            &homework,
            &user,
            Some(SubmissionText::try_new("draft").unwrap()),
            &db,
        )
        .await
        .unwrap()
        .unwrap();
        // A re-submit lands on the same row: submitted_at (the first hand-in)
        // pinned, updated_at moves forward, and the text can be cleared.
        let second = HomeworkSubmission::upsert(&homework, &user, None, &db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.get_id(), second.get_id());
        assert_eq!(first.get_submitted_at(), second.get_submitted_at());
        assert!(second.get_updated_at() >= first.get_updated_at());
        assert!(second.get_text().is_none());
        // One row per (homework, user), whatever the re-submit count.
        assert_eq!(
            HomeworkSubmission::list_for_homework(&homework, &db)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    /// The freeze, as stored state rather than as a race: the grade stamps the
    /// submission row, and from then on the submission's own writes fail their
    /// condition — no read of `homework_result` involved, which is what makes it
    /// hold when the grader is in the other replica. Un-grading clears the stamp
    /// and the row is writable again.
    ///
    /// Bite check: drop `WHERE graded_by_result = NONE` from
    /// [`HomeworkSubmission::upsert`] and the "version B" upsert below comes
    /// back `Some` — the exact swap this guards against.
    #[tokio::test]
    async fn a_grade_freezes_the_submission_row_itself() {
        let db = crate::database::init_mem().await.unwrap();
        let homework = HomeworkId::from_key("01TESTHWAAAAAAAAAAAAAAAAAA");
        let user = UserId::from_key("01TESTUSERAAAAAAAAAAAAAAAA");
        let teacher = UserId::from_key("01TESTTEACHERAAAAAAAAAAAAA");
        let graded = |value| {
            let text = SubmissionText::try_new(value).unwrap();
            HomeworkSubmission::upsert(&homework, &user, Some(text), &db)
        };

        let version_a = graded("version A").await.unwrap().unwrap();
        assert!(
            !HomeworkSubmission::is_graded(version_a.get_id(), &db)
                .await
                .unwrap()
        );

        crate::domain::homework_result::HomeworkResult::grade(
            &homework,
            &user,
            crate::domain::homework_result::HomeworkStatus::try_new("done").unwrap(),
            None,
            &teacher,
            &db,
        )
        .await
        .unwrap();
        assert!(
            HomeworkSubmission::is_graded(version_a.get_id(), &db)
                .await
                .unwrap()
        );

        // The swap the teacher would never see: refused, and the stored text is
        // still the version that was graded.
        assert!(graded("version B").await.unwrap().is_none());
        assert_eq!(
            HomeworkSubmission::read_for(&homework, &user, &db)
                .await
                .unwrap()
                .and_then(|row| row.get_text().map(|text| text.as_str().to_string())),
            Some("version A".to_string())
        );
        // ... and so is withdrawing it wholesale.
        let frozen = HomeworkSubmission::read_for(&homework, &user, &db)
            .await
            .unwrap()
            .unwrap();
        assert!(frozen.delete(&db).await.unwrap().is_none());
        assert!(
            HomeworkSubmission::read_for(&homework, &user, &db)
                .await
                .unwrap()
                .is_some()
        );

        // Un-grading unfreezes it, stamp and all.
        crate::domain::homework_result::HomeworkResult::remove(&homework, &user, &db)
            .await
            .unwrap();
        assert!(
            !HomeworkSubmission::is_graded(version_a.get_id(), &db)
                .await
                .unwrap()
        );
        let reopened = graded("version B").await.unwrap().unwrap();
        assert_eq!(
            reopened.get_text().map(SubmissionText::as_str),
            Some("version B")
        );
        // The two-stamp lateness rule is untouched by any of it: the first
        // hand-in is still the first hand-in.
        assert_eq!(reopened.get_submitted_at(), version_a.get_submitted_at());
        assert!(reopened.get_updated_at() >= version_a.get_updated_at());
    }

    #[tokio::test]
    async fn touch_moves_updated_at_but_not_submitted_at_or_text() {
        let db = crate::database::init_mem().await.unwrap();
        let homework = HomeworkId::generate();
        let user = UserId::from_key("01TESTUSERAAAAAAAAAAAAAAAA");

        let original = HomeworkSubmission::upsert(
            &homework,
            &user,
            Some(SubmissionText::try_new("photo answer").unwrap()),
            &db,
        )
        .await
        .unwrap()
        .unwrap();
        // A file add/delete touches the submission: updated_at moves (never
        // backwards), while the first-submit stamp and the text stay put.
        let touched = HomeworkSubmission::touch(original.get_id(), &db)
            .await
            .unwrap();
        assert_eq!(touched.get_submitted_at(), original.get_submitted_at());
        assert!(touched.get_updated_at() >= original.get_updated_at());
        assert_eq!(
            touched.get_text().map(SubmissionText::as_str),
            Some("photo answer")
        );
    }
}
