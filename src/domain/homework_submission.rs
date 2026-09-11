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
    HOMEWORK_ON_TIME_TOTAL_FIELD, HOMEWORK_SUBMISSION_TABLE, HOMEWORK_SUBMITTED_TOTAL_FIELD,
    MAX_HOMEWORK_TEXT_LEN, SUBMISSION_GRADED_FIELD, SUBMISSION_OPEN_GUARD,
};
use crate::database::{Database, transaction_with_retry};
use crate::domain::homework::{Homework, HomeworkId};
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
    /// yet" a condition of this very write instead of a read a concurrent grade
    /// can land behind. An absent row satisfies it (the stamp is `NONE` there
    /// too), so a first submit still creates.
    ///
    /// The badge counters on the student's user row move in this very
    /// transaction, and the order is load-bearing: the increment sits *after*
    /// the UPSERT and is conditional on it having matched a row, because a
    /// frozen row makes that `WHERE` match nothing **silently** (unlike
    /// [`crate::db::exam_result::grade`], which `THROW`s) — put
    /// first, it would count a submission the freeze refused. `$before = NONE`
    /// keeps it to a genuine first create, so an edit moves neither counter,
    /// which is what makes the live count mean the same thing the one-time
    /// backfill seeded (one per row; on time judged against `due_at`, equal
    /// counting as on time).
    ///
    /// `counted_on_time` is stamped on the row from the *same* `$on_time`
    /// expression the increment adds, in the same statement block: the deadline
    /// is mutable, so a withdrawal that re-judged it against the live `due_at`
    /// gave back something other than what was taken. Writing the verdict beside
    /// the counter is what keeps the two from ever disagreeing — read back by
    /// [`HomeworkSubmission::delete`], never re-derived.
    ///
    /// That deadline is `$was_due`, the one the gate below already read *inside
    /// this transaction* — never the caller's [`Homework`] snapshot, which was
    /// read before the lease and is exactly one `PATCH due_at` old in the
    /// "teacher extends the deadline at 23:59 while the class submits" moment.
    /// Judging on the snapshot stored a verdict the web layer's own `late` flag
    /// (`updated_at > due_at`, re-derived live on every read) then contradicted
    /// forever, in both directions. An in-process lock cannot fix this — it
    /// orders two handlers, not two store transactions — so the comparison has
    /// to live where the write does. The whole entity is still taken (both
    /// callers hold the row, and the id comes off it); its `due_at` must not be.
    ///
    /// The first three statements are the parent gate, and they are why this
    /// write cannot outlive its homework: the id is deterministic, so nothing
    /// else here would fail against a homework a cascade already removed — it
    /// would simply re-create the row, badge counters and all, unreachable ever
    /// after (every route to a submission goes through its homework). A *read*
    /// of the homework does not close that, on either side of the call: the
    /// store does no read-set conflict detection, so a delete committing
    /// alongside is invisible to it. The gate therefore *moves* a value on the
    /// homework row — `due_at` up by one and straight back to the captured
    /// value, so the row is byte-identical afterwards — because only a write
    /// collides, and `SET x = x` is elided and never reaches the write set. It
    /// is [`crate::db::exam_answer::save`]'s shape exactly.
    /// `Err(NotFound)` means the homework is gone, which is the 404 the web
    /// layer's own lookup would have answered.
    pub async fn upsert(
        homework: &Homework,
        user: &UserId,
        text: Option<SubmissionText>,
        db: &Database,
    ) -> Result<Option<HomeworkSubmission>, AppError> {
        let id = HomeworkSubmissionId::composite(homework.get_id(), user);
        let now = Timestamp::now();
        let text = text.map(|text| text.as_str().to_string());
        // Sound to re-send: the UPSERT is on a deterministic id on a table with
        // no unique index, so it can never legitimately answer "already exists",
        // and the counter UPDATEs never can either.
        let (mut saved, mut errors) = transaction_with_retry(
            db,
            &format!(
                "BEGIN TRANSACTION;
                 LET $was_due = (SELECT VALUE due_at FROM ONLY $hw);
                 LET $alive = (UPDATE $hw SET due_at = due_at + 1 RETURN VALUE id);
                 IF array::len($alive) = 0 {{ THROW 'no_homework' }};
                 UPDATE $hw SET due_at = $was_due;
                 LET $before = (SELECT VALUE id FROM ONLY $id);
                 LET $on_time = $now <= $was_due;
                 LET $after = (UPSERT $id SET homework = $hw, user = $usr, text = $text,
                     updated_at = $now, submitted_at = submitted_at ?? $now
                     WHERE {SUBMISSION_OPEN_GUARD} RETURN AFTER);
                 IF $before = NONE AND array::len($after) > 0 {{
                     UPDATE $id SET counted_on_time = $on_time;
                     UPDATE $usr SET
                         {HOMEWORK_SUBMITTED_TOTAL_FIELD} = ({HOMEWORK_SUBMITTED_TOTAL_FIELD} ?? 0) + 1,
                         {HOMEWORK_ON_TIME_TOTAL_FIELD} = ({HOMEWORK_ON_TIME_TOTAL_FIELD} ?? 0)
                             + IF $on_time {{ 1 }} ELSE {{ 0 }}
                 }};
                 RETURN $after;
                 COMMIT TRANSACTION;"
            ),
            &[
                ("id".into(), id.record().into_value()),
                ("hw".into(), homework.get_id().record().into_value()),
                ("usr".into(), user.record().into_value()),
                ("text".into(), text.into_value()),
                ("now".into(), now.into_value()),
            ],
            &["no_homework"],
        )
        .await?;
        // An aborted transaction errors *every* slot, most with a generic "not
        // executed" — only the THROW's own slot names the reason.
        if errors
            .values()
            .any(|error| error.to_string().contains("no_homework"))
        {
            return Err(AppError::NotFound);
        }
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        // The trailing `RETURN` is always the last statement before `COMMIT`, so
        // its slot follows the statement count rather than a hand-kept number.
        // It returns the whole array, never `$after[0]`: a refused write makes
        // that `NONE`, which fails to deserialize ("expected object, got none")
        // instead of reading as the "frozen" the caller answers 409 to.
        let slot = saved.num_statements().saturating_sub(2);
        Ok(saved
            .take::<Vec<HomeworkSubmission>>(slot)?
            .into_iter()
            .next())
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
    ///
    /// The one place either badge counter comes back down, and deliberately the
    /// only one: this route is student-callable on their own row, so without it
    /// a student farms "handed in 50 homeworks" by submit/delete/submit on a
    /// single homework. A teacher's homework delete and the course cascade
    /// leave the counters alone — history a teacher erased is still history the
    /// student lived.
    ///
    /// The on-time half is given back by the verdict [`HomeworkSubmission::upsert`]
    /// *stored* on the row, never by re-judging the deadline: `due_at` is
    /// mutable, so re-deriving read whatever the teacher had moved it to since
    /// and gave back something other than what was taken — extend it after a
    /// late hand-in and this debited a credit that was never given; pull it back
    /// after a punctual one and it debited nothing, leaving `on_time` above
    /// `submitted`. A row from before the column exists carries no verdict, and
    /// cannot be given one for a credit that already happened, so it falls back
    /// to the old cut (`submitted_at`, the readonly stamp the increment judged,
    /// against the live deadline); a dangling `homework` link leaves `$due`
    /// `NONE` there, which compares false and so counts as late, matching the
    /// backfill. Floored at zero: a row that predates the columns has none.
    ///
    /// Badges already earned are never taken away — [`crate::domain::badge`] is
    /// add-only, which is where that permanence lives.
    pub async fn delete(self, db: &Database) -> Result<Option<HomeworkSubmission>, AppError> {
        // Sound to re-send: DELETE and UPDATE can never answer "already exists".
        let (mut result, mut errors) = transaction_with_retry(
            db,
            &format!(
                "BEGIN TRANSACTION;
                 LET $due = (SELECT VALUE homework.due_at FROM ONLY $sub);
                 LET $gone = (DELETE $sub WHERE {SUBMISSION_OPEN_GUARD} RETURN BEFORE);
                 IF array::len($gone) > 0 {{
                     DELETE homework_file WHERE submission = $sub;
                     LET $on_time = IF ($gone[0].counted_on_time
                         ?? ($gone[0].submitted_at <= $due)) {{ 1 }} ELSE {{ 0 }};
                     UPDATE $usr SET
                         {HOMEWORK_SUBMITTED_TOTAL_FIELD} =
                             math::max([({HOMEWORK_SUBMITTED_TOTAL_FIELD} ?? 0) - 1, 0]),
                         {HOMEWORK_ON_TIME_TOTAL_FIELD} =
                             math::max([({HOMEWORK_ON_TIME_TOTAL_FIELD} ?? 0) - $on_time, 0])
                 }};
                 RETURN $gone;
                 COMMIT TRANSACTION;"
            ),
            &[
                ("sub".into(), self.id.record().into_value()),
                ("usr".into(), self.user.record().into_value()),
            ],
            &[],
        )
        .await?;
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        // The trailing `RETURN` is always the last statement before `COMMIT`.
        let slot = result.num_statements().saturating_sub(2);
        Ok(result
            .take::<Vec<HomeworkSubmission>>(slot)?
            .into_iter()
            .next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::domain::homework::HomeworkTitle;
    use crate::domain::subject::{Subject, SubjectDescription, SubjectName};

    /// A real homework row (and the subject it must reference) due at `due_at` —
    /// `upsert` now reads the deadline off the entity, so the tests need one.
    async fn a_homework(due_at: Timestamp, db: &Database) -> Homework {
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
            due_at,
            None,
            &UserId::from_key("teacher"),
            db,
        )
        .await
        .unwrap()
    }

    /// A deadline no test run can reach, so a submission is unambiguously on
    /// time; `Timestamp::from_millis(1)` is its late twin.
    fn far_future() -> Timestamp {
        Timestamp::from_millis(Timestamp::now().as_millis() + 3_600_000)
    }

    /// A real user row: the counters live on it, and an `UPDATE` has nothing to
    /// write to without one.
    async fn a_student(username: &str, db: &Database) -> UserId {
        let hash = crate::domain::user::Password::try_new("secret1")
            .unwrap()
            .hash_async()
            .await
            .unwrap();
        crate::db::user::create(
            db,
            crate::domain::user::Username::try_new(username).unwrap(),
            hash,
        )
        .await
        .unwrap()
        .get_id()
        .clone()
    }

    /// Move the stored deadline the way a teacher's PATCH does, leaving the
    /// caller's `homework` snapshot untouched — the stale-snapshot shape the
    /// handler produces when a `PATCH due_at` commits between its gate read and
    /// its write.
    async fn deadline_moves_to(homework: &Homework, due_at: Timestamp, db: &Database) {
        Homework::read(homework.get_id(), db)
            .await
            .unwrap()
            .expect("the homework exists")
            .update(None, None, None, Some(due_at), None, db)
            .await
            .unwrap();
    }

    /// The verdict `upsert` stored on the row — what `delete` debits off.
    /// `None` is a row from before the column existed.
    async fn stored_verdict(id: &HomeworkSubmissionId, db: &Database) -> Option<bool> {
        let mut result = db
            .query("SELECT VALUE counted_on_time FROM $sub")
            .bind(("sub", id.record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        result
            .take::<Vec<Option<bool>>>(0)
            .unwrap()
            .into_iter()
            .flatten()
            .next()
    }

    /// The web layer's computed flag, exactly as `SubmissionResponse::new`
    /// derives it (`src/web/homework.rs`): `updated_at` against the *live*
    /// deadline, re-read here because that is what a later GET reads.
    async fn late_flag(submission: &HomeworkSubmission, db: &Database) -> bool {
        let live = Homework::read(submission.get_homework(), db)
            .await
            .unwrap()
            .expect("the homework exists");
        submission.get_updated_at() > live.get_due_at()
    }

    /// The two badge counters on a user row, absent counting as zero.
    async fn counters(user: &UserId, db: &Database) -> (i64, i64) {
        let mut result = db
            .query(format!(
                "SELECT VALUE [({HOMEWORK_SUBMITTED_TOTAL_FIELD} ?? 0),
                               ({HOMEWORK_ON_TIME_TOTAL_FIELD} ?? 0)] FROM $usr"
            ))
            .bind(("usr", user.record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let rows = result.take::<Vec<Vec<i64>>>(0).unwrap();
        let row = rows.first().cloned().unwrap_or_default();
        (
            row.first().copied().unwrap_or(0),
            row.get(1).copied().unwrap_or(0),
        )
    }

    #[tokio::test]
    async fn text_is_optional_but_bounded() {
        assert!(SubmissionText::try_new("").is_ok());
        assert!(SubmissionText::try_new(&"x".repeat(5_000)).is_ok());
        assert!(SubmissionText::try_new(&"x".repeat(5_001)).is_err());
    }

    #[tokio::test]
    async fn resubmit_keeps_submitted_at_and_restamps_updated_at() {
        let db = crate::database::init_mem().await.unwrap();
        let homework = a_homework(far_future(), &db).await;
        let user = a_student("ogrenci", &db).await;

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
        // ... and it is one submission, not two: the edit moves no counter.
        assert_eq!(counters(&user, &db).await, (1, 1));
        assert_eq!(first.get_id(), second.get_id());
        assert_eq!(first.get_submitted_at(), second.get_submitted_at());
        assert!(second.get_updated_at() >= first.get_updated_at());
        assert!(second.get_text().is_none());
        // One row per (homework, user), whatever the re-submit count.
        assert_eq!(
            HomeworkSubmission::list_for_homework(homework.get_id(), &db)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    /// The freeze, as stored state rather than as a race: the grade stamps the
    /// submission row, and from then on the submission's own writes fail their
    /// condition — no read of `homework_result` involved, which is what makes it
    /// hold when the grade lands mid-request. Un-grading clears the stamp
    /// and the row is writable again.
    ///
    /// Bite check: drop `WHERE graded_by_result = NONE` from
    /// [`HomeworkSubmission::upsert`] and the "version B" upsert below comes
    /// back `Some` — the exact swap this guards against.
    #[tokio::test]
    async fn a_grade_freezes_the_submission_row_itself() {
        let db = crate::database::init_mem().await.unwrap();
        let homework = a_homework(far_future(), &db).await;
        let user = a_student("ogrenci", &db).await;
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

        assert_eq!(counters(&user, &db).await, (1, 1));

        crate::domain::homework_result::HomeworkResult::grade(
            homework.get_id(),
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
            HomeworkSubmission::read_for(homework.get_id(), &user, &db)
                .await
                .unwrap()
                .and_then(|row| row.get_text().map(|text| text.as_str().to_string())),
            Some("version A".to_string())
        );
        // The refusal is silent — the UPSERT simply matches nothing — so the
        // counters are the only thing that can catch an increment placed in
        // front of it: a submission the freeze refused must count for nothing.
        assert_eq!(counters(&user, &db).await, (1, 1));
        // ... and so is withdrawing it wholesale.
        let frozen = HomeworkSubmission::read_for(homework.get_id(), &user, &db)
            .await
            .unwrap()
            .unwrap();
        assert!(frozen.delete(&db).await.unwrap().is_none());
        assert!(
            HomeworkSubmission::read_for(homework.get_id(), &user, &db)
                .await
                .unwrap()
                .is_some()
        );
        // A refused delete decrements nothing either.
        assert_eq!(counters(&user, &db).await, (1, 1));

        // Un-grading unfreezes it, stamp and all.
        crate::domain::homework_result::HomeworkResult::remove(homework.get_id(), &user, &db)
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
        // Still an edit to the row that was already counted, un-freeze or not.
        assert_eq!(counters(&user, &db).await, (1, 1));
        // The two-stamp lateness rule is untouched by any of it: the first
        // hand-in is still the first hand-in.
        assert_eq!(reopened.get_submitted_at(), version_a.get_submitted_at());
        assert!(reopened.get_updated_at() >= version_a.get_updated_at());
    }

    #[tokio::test]
    async fn touch_moves_updated_at_but_not_submitted_at_or_text() {
        let db = crate::database::init_mem().await.unwrap();
        let homework = a_homework(far_future(), &db).await;
        let user = a_student("ogrenci", &db).await;

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

    /// The farm, closed: `delete_submission` is the student's own route, so a
    /// counter that only ever went up would let one homework be handed in fifty
    /// times. Submit → withdraw → submit is worth exactly one submission.
    ///
    /// Bite check: drop the `UPDATE $usr` from [`HomeworkSubmission::delete`]
    /// and the last assertion reads `(2, 2)` — the farm.
    #[tokio::test]
    async fn submit_delete_submit_is_worth_one_submission() {
        let db = crate::database::init_mem().await.unwrap();
        let homework = a_homework(far_future(), &db).await;
        let user = a_student("ogrenci", &db).await;

        HomeworkSubmission::upsert(&homework, &user, None, &db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(counters(&user, &db).await, (1, 1));

        let mine = HomeworkSubmission::read_for(homework.get_id(), &user, &db)
            .await
            .unwrap()
            .unwrap();
        mine.delete(&db).await.unwrap().unwrap();
        assert_eq!(counters(&user, &db).await, (0, 0));
        // Floored: a second withdrawal (or a row from before the columns
        // existed) can never push either counter negative.
        assert!(
            HomeworkSubmission::read_for(homework.get_id(), &user, &db)
                .await
                .unwrap()
                .is_none()
        );

        HomeworkSubmission::upsert(&homework, &user, None, &db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(counters(&user, &db).await, (1, 1));
    }

    /// Lateness is judged against the homework's own deadline at hand-in, the
    /// same cut the one-time backfill used, and only the on-time counter sees
    /// it: a late hand-in is still a hand-in. Its withdrawal gives back only
    /// what it took.
    #[tokio::test]
    async fn a_late_submission_counts_as_submitted_but_not_on_time() {
        let db = crate::database::init_mem().await.unwrap();
        let punctual = a_homework(far_future(), &db).await;
        let overdue = a_homework(Timestamp::from_millis(1), &db).await;
        let user = a_student("ogrenci", &db).await;

        HomeworkSubmission::upsert(&punctual, &user, None, &db)
            .await
            .unwrap()
            .unwrap();
        HomeworkSubmission::upsert(&overdue, &user, None, &db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(counters(&user, &db).await, (2, 1));

        // Withdrawing the *late* one takes back only the submission, never the
        // on-time credit the punctual one earned — the zero floor would hide a
        // wrong subtraction here if the on-time count were sitting at zero.
        let late = HomeworkSubmission::read_for(overdue.get_id(), &user, &db)
            .await
            .unwrap()
            .unwrap();
        late.delete(&db).await.unwrap().unwrap();
        assert_eq!(counters(&user, &db).await, (1, 1));

        // ... and withdrawing the punctual one gives back exactly that credit.
        let kept = HomeworkSubmission::read_for(punctual.get_id(), &user, &db)
            .await
            .unwrap()
            .unwrap();
        kept.delete(&db).await.unwrap().unwrap();
        assert_eq!(counters(&user, &db).await, (0, 0));
    }

    /// A row from before `counted_on_time` existed carries no verdict — it was
    /// credited by a comparison that left no trace, and cannot be given one
    /// retroactively. Withdrawing it falls back to that same comparison, so the
    /// pre-2026-08-04 rows on a live volume keep behaving exactly as they did.
    ///
    /// Bite check: an absent verdict read as `false` rather than falling through
    /// (`??` binding the wrong side of the `<=`) leaves the last assertion at
    /// `(0, 1)` — on time above submitted, the shape the stored verdict exists
    /// to prevent.
    #[tokio::test]
    async fn a_row_with_no_stored_verdict_falls_back_to_the_deadline() {
        let db = crate::database::init_mem().await.unwrap();
        let punctual = a_homework(far_future(), &db).await;
        let overdue = a_homework(Timestamp::from_millis(1), &db).await;
        let user = a_student("ogrenci", &db).await;

        HomeworkSubmission::upsert(&punctual, &user, None, &db)
            .await
            .unwrap()
            .unwrap();
        HomeworkSubmission::upsert(&overdue, &user, None, &db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(counters(&user, &db).await, (2, 1));
        // Aged into legacy rows: the counters keep the credit, the rows lose the
        // verdict — the exact state of every submission on an existing volume.
        db.query("UPDATE homework_submission UNSET counted_on_time")
            .await
            .unwrap()
            .check()
            .unwrap();

        let late = HomeworkSubmission::read_for(overdue.get_id(), &user, &db)
            .await
            .unwrap()
            .unwrap();
        late.delete(&db).await.unwrap().unwrap();
        assert_eq!(counters(&user, &db).await, (1, 1));

        let kept = HomeworkSubmission::read_for(punctual.get_id(), &user, &db)
            .await
            .unwrap()
            .unwrap();
        kept.delete(&db).await.unwrap().unwrap();
        assert_eq!(counters(&user, &db).await, (0, 0));
    }

    /// The stale snapshot, both directions: the teacher's `PATCH due_at` commits
    /// between the handler's gate read and its write, so the caller's entity
    /// carries a deadline the store no longer has. The verdict must follow the
    /// *stored* deadline the write itself reads, because the API's `late` flag
    /// is re-derived from that same stored value on every later read — a verdict
    /// judged on the snapshot disagrees with it permanently, and `delete` then
    /// gives back the wrong credit.
    ///
    /// Bite check: judge `$on_time` against the caller's `$due` instead of
    /// `$was_due` and both halves fail — extended reads `(1, 0)` with a stored
    /// `false` against a live `late = false`, pulled reads `(1, 1)` with a
    /// stored `true` against a live `late = true`.
    #[tokio::test]
    async fn a_deadline_moved_under_the_caller_is_judged_by_the_stored_value() {
        let db = crate::database::init_mem().await.unwrap();
        let user = a_student("ogrenci", &db).await;

        // Extended at 23:59: the snapshot says the deadline has passed, the
        // store says it has not. A hand-in now is on time.
        let extended = a_homework(Timestamp::from_millis(1), &db).await;
        deadline_moves_to(&extended, far_future(), &db).await;
        let landed = HomeworkSubmission::upsert(&extended, &user, None, &db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(counters(&user, &db).await, (1, 1));
        assert_eq!(stored_verdict(landed.get_id(), &db).await, Some(true));
        assert!(!late_flag(&landed, &db).await);

        // Pulled earlier: the snapshot says there is time left, the store says
        // the deadline is gone. The same hand-in is late.
        let pulled = a_homework(far_future(), &db).await;
        deadline_moves_to(&pulled, Timestamp::from_millis(1), &db).await;
        let missed = HomeworkSubmission::upsert(&pulled, &user, None, &db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(counters(&user, &db).await, (2, 1));
        assert_eq!(stored_verdict(missed.get_id(), &db).await, Some(false));
        assert!(late_flag(&missed, &db).await);

        // The invariant behind both: the stored verdict is the negation of the
        // flag the web layer computes for the same submission, never its twin.
        for submission in [&landed, &missed] {
            assert_eq!(
                stored_verdict(submission.get_id(), &db).await,
                Some(!late_flag(submission, &db).await),
                "stored verdict and the API's late flag must never disagree"
            );
        }

        // ... and the withdrawal, which debits off the stamp, gives back exactly
        // what each one took.
        missed.delete(&db).await.unwrap().unwrap();
        assert_eq!(counters(&user, &db).await, (1, 1));
        landed.delete(&db).await.unwrap().unwrap();
        assert_eq!(counters(&user, &db).await, (0, 0));
    }
}
