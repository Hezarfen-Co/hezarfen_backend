//! The `homework_submission` table: one row per (homework, user) — the
//! freeze-guarded UPSERT with its badge counters, the touch, the reads the
//! roster and report join on, and the counter-returning delete. The
//! submit/withdraw workflows live in
//! [`crate::service::homework_submission`]; the validated text type in
//! [`crate::domain::homework_submission`].

use crate::database::{Database, tx_with_retry};
use crate::domain::homework::Homework;
use crate::domain::homework::HomeworkId;
use crate::domain::homework_submission::{
    HomeworkSubmission, HomeworkSubmissionId, SubmissionText,
};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Create or re-stamp `user`'s submission to `homework`, unless a grade has
/// frozen it — `None` means frozen, and the caller answers 409. `updated_at`
/// moves to now every time; `submitted_at` (the first-submit stamp) is kept
/// on an existing row and set only on a fresh one. Returns the row plus
/// whether it existed before this call (the web layer's 200-vs-201, and the
/// only-a-first-hand-in-earns-badges rule).
///
/// `preserve_text` is the photo-upload auto-create's switch: an existing row
/// keeps its text (and its stamp moves, as any touch), because the upload
/// never offered a text to replace it. A submit passes `false` — an absent
/// text clears, as on the wire.
///
/// The `graded_by_result IS NULL` predicate is the freeze: "not graded yet"
/// is a condition of the write itself, so a refused write answers `None`
/// instead of wiping graded work. An absent row satisfies it trivially, so a
/// first submit still creates.
///
/// The badge counters on the student's user row move in this very
/// transaction, and only on the create arm — an edit moves neither counter,
/// which is what makes the live count mean one per row.
///
/// `counted_on_time` is stamped on the row from the same `on_time` verdict
/// the increment adds, in the same transaction: the deadline is mutable, so
/// a withdrawal that re-judged it against the live `due_at` gave back
/// something other than what was taken. Writing the verdict beside the
/// counter is what keeps the two from ever disagreeing — read back by
/// [`delete`], never re-derived.
///
/// That deadline is `was_due`, the one read *inside this transaction* under
/// the homework row's lock — never the caller's [`Homework`] snapshot, which
/// was read before the request's gates and is exactly one `PATCH due_at` old
/// in the "teacher extends the deadline at 23:59 while the class submits"
/// moment. The lock is also the serialization point: grading locks the same
/// row before stamping, the audience-narrowing PATCH before its orphan
/// check, and a rival first hand-in before its own insert — so the
/// read-then-write below cannot interleave with any of them. (The pair's
/// UNIQUE constraint backs this up; under the lock it can never fire.)
pub async fn upsert(
    db: &Database,
    homework: &Homework,
    user: &UserId,
    text: Option<SubmissionText>,
    preserve_text: bool,
) -> Result<Option<(HomeworkSubmission, bool)>, AppError> {
    let now = Timestamp::now();
    // Owned captures (`Send` rule of `tx_with_retry` closures).
    let homework_id = homework.get_id().clone();
    let user = *user;
    tx_with_retry(db, false, async move |tx| {
        let text = text.clone();
        // The parent gate: the homework row is locked and its deadline read
        // in the very transaction that writes the submission. The id is
        // minted fresh, so nothing else here would fail against a homework a
        // cascade already removed — it would simply create a row under it,
        // badge counters and all, unreachable ever after (every route to a
        // submission goes through its homework). `Err(NotFound)` means the
        // homework is gone, which is the 404 the web layer's own lookup
        // would have answered.
        let was_due = sqlx::query!(
            // The audience re-checks UNDER the lock: a narrowing PATCH commits
            // between the web layer's pre-flight read and this write, and a
            // student the homework no longer names must read as gone — the
            // same 404 their own lookup answers.
            r#"SELECT due_at AS "due_at: Timestamp" FROM homework
               WHERE id = $1 AND (cardinality(assigned) = 0 OR $2 = ANY(assigned))
               FOR UPDATE"#,
            homework_id.uuid(),
            user.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?
        .map(|row| row.due_at);
        let Some(was_due) = was_due else {
            return Err(AppError::NotFound);
        };
        // Equal counts as on time, exactly as the deadline has always been
        // judged.
        let on_time = i64::from(now <= was_due);

        // The row, read under the homework's lock: its existence decides the
        // arm (and the counters).
        let existing = sqlx::query!(
            r#"SELECT 1 AS "row: i32" FROM homework_submission
               WHERE homework = $1 AND app_user = $2 FOR UPDATE"#,
            homework_id.uuid(),
            user.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?
        .is_some();

        if existing {
            // Re-submit: text is replaced (unless the upload auto-create is
            // asking — it has no text to replace), `updated_at` moves, and
            // `submitted_at` is left alone. A frozen row matches nothing and
            // answers `None`, the caller's 409.
            let updated = if preserve_text {
                sqlx::query_as!(
                    HomeworkSubmission,
                    r#"UPDATE homework_submission SET updated_at = $3
                       WHERE homework = $1 AND app_user = $2 AND graded_by_result IS NULL
                       RETURNING id AS "id: HomeworkSubmissionId",
                                 homework AS "homework: HomeworkId",
                                 app_user AS "user: UserId",
                                 text AS "text: SubmissionText",
                                 submitted_at AS "submitted_at: Timestamp",
                                 updated_at AS "updated_at: Timestamp""#,
                    homework_id.uuid(),
                    user.uuid(),
                    now.as_millis()
                )
                .fetch_optional(&mut *tx)
                .await?
            } else {
                sqlx::query_as!(
                    HomeworkSubmission,
                    r#"UPDATE homework_submission SET text = $3, updated_at = $4
                       WHERE homework = $1 AND app_user = $2 AND graded_by_result IS NULL
                       RETURNING id AS "id: HomeworkSubmissionId",
                                 homework AS "homework: HomeworkId",
                                 app_user AS "user: UserId",
                                 text AS "text: SubmissionText",
                                 submitted_at AS "submitted_at: Timestamp",
                                 updated_at AS "updated_at: Timestamp""#,
                    homework_id.uuid(),
                    user.uuid(),
                    text.as_ref().map(SubmissionText::as_str),
                    now.as_millis()
                )
                .fetch_optional(&mut *tx)
                .await?
            };
            return Ok(updated.map(|submission| (submission, existing)));
        }

        // First hand-in: the verdict is stored beside the counter it credits,
        // so the two can never disagree.
        let created = sqlx::query_as!(
            HomeworkSubmission,
            r#"INSERT INTO homework_submission
                   (id, homework, app_user, text, submitted_at, updated_at, file_count, counted_on_time)
               VALUES ($1, $2, $3, $4, $5, $5, 0, $6)
               RETURNING id AS "id: HomeworkSubmissionId",
                         homework AS "homework: HomeworkId",
                         app_user AS "user: UserId",
                         text AS "text: SubmissionText",
                         submitted_at AS "submitted_at: Timestamp",
                         updated_at AS "updated_at: Timestamp""#,
            HomeworkSubmissionId::generate().uuid(),
            homework_id.uuid(),
            user.uuid(),
            text.as_ref().map(SubmissionText::as_str),
            now.as_millis(),
            on_time != 0,
        )
        .fetch_optional(&mut *tx)
        .await?;
        sqlx::query!(
            r#"UPDATE app_user SET
                   homework_submitted_total = homework_submitted_total + 1,
                   homework_on_time_total = homework_on_time_total + $2
               WHERE id = $1"#,
            user.uuid(),
            on_time
        )
        .execute(&mut *tx)
        .await?;
        Ok(created.map(|submission| (submission, existing)))
    })
    .await
}

/// Re-stamp a submission's `updated_at` to now, leaving its text and files
/// alone. A file add or delete modifies the submission as a whole, so its
/// "last touched" clock — which drives the computed late flag — must move
/// even though the text row is unchanged. A targeted single-field UPDATE, so
/// the readonly `submitted_at` is never re-sent. Returns the re-stamped row,
/// or a 404 if it has since vanished.
pub async fn touch(
    db: &Database,
    id: &HomeworkSubmissionId,
) -> Result<HomeworkSubmission, AppError> {
    let now = Timestamp::now();
    sqlx::query_as!(
        HomeworkSubmission,
        r#"UPDATE homework_submission SET updated_at = $2 WHERE id = $1
           RETURNING id AS "id: HomeworkSubmissionId",
                     homework AS "homework: HomeworkId",
                     app_user AS "user: UserId",
                     text AS "text: SubmissionText",
                     submitted_at AS "submitted_at: Timestamp",
                     updated_at AS "updated_at: Timestamp""#,
        id.uuid(),
        now.as_millis()
    )
    .fetch_optional(db)
    .await?
    .ok_or(AppError::NotFound)
}

/// Whether a grade has frozen this submission. Only ever read to tell two
/// refusals apart *after* a conditional write has already refused one — the
/// stamp on the row, never this read, is what decides.
pub async fn is_graded(db: &Database, id: &HomeworkSubmissionId) -> Result<bool, AppError> {
    let row = sqlx::query!(
        r#"SELECT EXISTS(SELECT 1 FROM homework_submission
                         WHERE id = $1 AND graded_by_result IS NOT NULL) AS "graded!: bool""#,
        id.uuid()
    )
    .fetch_one(db)
    .await?;
    Ok(row.graded)
}

/// `user`'s submission to `homework`, if they have one.
pub async fn read_for(
    db: &Database,
    homework: &crate::domain::homework::HomeworkId,
    user: &UserId,
) -> Result<Option<HomeworkSubmission>, AppError> {
    Ok(sqlx::query_as!(
        HomeworkSubmission,
        r#"SELECT id AS "id: HomeworkSubmissionId",
                  homework AS "homework: HomeworkId",
                  app_user AS "user: UserId",
                  text AS "text: SubmissionText",
                  submitted_at AS "submitted_at: Timestamp",
                  updated_at AS "updated_at: Timestamp"
           FROM homework_submission WHERE homework = $1 AND app_user = $2"#,
        homework.uuid(),
        user.uuid()
    )
    .fetch_optional(db)
    .await?)
}

/// Every submission to `homework`, in creation order — the roster's raw rows.
pub async fn list_for_homework(
    db: &Database,
    homework: &crate::domain::homework::HomeworkId,
) -> Result<Vec<HomeworkSubmission>, AppError> {
    Ok(sqlx::query_as!(
        HomeworkSubmission,
        r#"SELECT id AS "id: HomeworkSubmissionId",
                  homework AS "homework: HomeworkId",
                  app_user AS "user: UserId",
                  text AS "text: SubmissionText",
                  submitted_at AS "submitted_at: Timestamp",
                  updated_at AS "updated_at: Timestamp"
           FROM homework_submission WHERE homework = $1 ORDER BY id ASC"#,
        homework.uuid()
    )
    .fetch_all(db)
    .await?)
}

/// Delete this submission and its files in one transaction — so a crash
/// can't leave a file pointing at a vanished submission. The file *blobs*
/// are the web layer's to unlink: it lists them
/// ([`crate::db::homework_file::list_for_submission`])
/// before calling this. `None` means a grade froze the row (the caller
/// answers 409): the delete carries `graded_by_result IS NULL` as its own
/// condition, so a refused delete leaves the files standing too.
///
/// The one place either badge counter comes back down, and deliberately the
/// only one: this route is student-callable on their own row, so without it
/// a student farms "handed in 50 homeworks" by submit/delete/submit on a
/// single homework. A teacher's homework delete and the course cascade
/// leave the counters alone — history a teacher erased is still history the
/// student lived.
///
/// The on-time half is given back by the verdict [`upsert`]
/// *stored* on the row, never by re-judging the deadline: `due_at` is
/// mutable, so re-deriving read whatever the teacher had moved it to since
/// and gave back something other than what was taken — extend it after a
/// late hand-in and this debited a credit that was never given; pull it back
/// after a punctual one and it debited nothing, leaving `on_time` above
/// `submitted`. A row with no stored verdict falls back to the old cut
/// (`submitted_at`, the stamp the increment judged, against the deadline
/// read here); a homework already gone counts as late, matching the
/// backfill. Floored at zero.
///
/// Badges already earned are never taken away — [`crate::domain::badge`] is
/// add-only, which is where that permanence lives.
pub async fn delete(
    db: &Database,
    submission: HomeworkSubmission,
) -> Result<Option<HomeworkSubmission>, AppError> {
    tx_with_retry(db, false, async move |tx| {
        // The homework row's lock is the serialization point against grading:
        // a grade locks the same row before it stamps, so either this delete
        // refuses off the stamp, or the grade landed on a row that is now
        // gone — grading absent work, which is allowed. Its `due_at` read is
        // also the replay fallback's deadline.
        let due = sqlx::query!(
            r#"SELECT due_at AS "due_at: Timestamp" FROM homework WHERE id = $1 FOR UPDATE"#,
            submission.get_homework().uuid()
        )
        .fetch_optional(&mut *tx)
        .await?
        .map(|row| row.due_at);
        // The gate runs as a lock-taking read, not a delete: the file rows
        // must die BEFORE the submission row (their `submission` FK is
        // `NO ACTION`), and that wipe is only honest if this delete has
        // already won — so the open-not-frozen verdict and the row lock come
        // first, files second, the submission row last. A rival withdrawal
        // that beat us removed the row under its own lock; a grade that
        // stamped it holds the homework lock first — either way `None`, one
        // refusal as before.
        let target = sqlx::query!(
            r#"SELECT id AS "id: HomeworkSubmissionId",
                      homework AS "homework: HomeworkId",
                      app_user AS "user: UserId",
                      text AS "text: SubmissionText",
                      submitted_at AS "submitted_at: Timestamp",
                      updated_at AS "updated_at: Timestamp",
                      counted_on_time AS "counted_on_time: bool"
               FROM homework_submission
               WHERE id = $1 AND graded_by_result IS NULL
               FOR UPDATE"#,
            submission.get_id().uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(removed) = target else {
            // Frozen, or already withdrawn by a rival — one refusal, as
            // before.
            return Ok(None);
        };
        sqlx::query!(
            "DELETE FROM homework_file WHERE submission = $1",
            removed.id.uuid()
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "DELETE FROM homework_submission WHERE id = $1",
            removed.id.uuid()
        )
        .execute(&mut *tx)
        .await?;
        let judged_on_time = due.map(|d| removed.submitted_at <= d).unwrap_or(false);
        let on_time = i64::from(removed.counted_on_time.unwrap_or(judged_on_time));
        sqlx::query!(
            r#"UPDATE app_user SET
                   homework_submitted_total = GREATEST(homework_submitted_total - 1, 0),
                   homework_on_time_total = GREATEST(homework_on_time_total - $2, 0)
               WHERE id = $1"#,
            removed.user.uuid(),
            on_time
        )
        .execute(&mut *tx)
        .await?;
        Ok(Some(HomeworkSubmission {
            id: removed.id,
            homework: removed.homework,
            user: removed.user,
            text: removed.text,
            submitted_at: removed.submitted_at,
            updated_at: removed.updated_at,
        }))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row as _;

    use crate::db::homework as homework_db;
    use crate::domain::homework::HomeworkTitle;
    use crate::domain::subject::{SubjectDescription, SubjectName};

    /// A real homework row (and the subject it must reference) due at `due_at` —
    /// `upsert` now reads the deadline off the entity, so the tests need one.
    async fn a_homework(due_at: Timestamp, db: &Database) -> Homework {
        let course = crate::db::course::a_test_course(db).await;
        let subject = crate::db::subject::create(
            db,
            &course,
            SubjectName::try_new("topic").unwrap(),
            SubjectDescription::try_new("").unwrap(),
        )
        .await
        .unwrap();
        homework_db::create(
            db,
            &course,
            subject.get_id(),
            HomeworkTitle::try_new("essay").unwrap(),
            None,
            due_at,
            None,
            &a_teacher(db).await,
        )
        .await
        .unwrap()
    }

    /// A real `app_user` teacher row: graders are foreign keys too.
    async fn a_teacher(db: &Database) -> UserId {
        let user = UserId::generate();
        sqlx::query(
            "INSERT INTO app_user (id, username, password_hash, role) \
             VALUES ($1, $2, 'x', 'teacher')",
        )
        .bind(user.uuid())
        .bind(format!("t-{}", &user.key()[30..]))
        .execute(db)
        .await
        .unwrap();
        user
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
        let stored = homework_db::read(db, homework.get_id())
            .await
            .unwrap()
            .expect("the homework exists");
        homework_db::update(db, stored, None, None, None, Some(due_at), None)
            .await
            .unwrap();
    }

    /// The verdict `upsert` stored on the row — what `delete` debits off.
    /// `None` is a row from before the column existed.
    async fn stored_verdict(id: &HomeworkSubmissionId, db: &Database) -> Option<bool> {
        let row = sqlx::query("SELECT counted_on_time FROM homework_submission WHERE id = $1")
            .bind(id.uuid())
            .fetch_optional(db)
            .await
            .unwrap()
            .expect("the submission row exists");
        row.try_get::<Option<bool>, _>(0).unwrap()
    }

    /// The web layer's computed flag, exactly as `SubmissionResponse::new`
    /// derives it (`src/web/homework.rs`): `updated_at` against the *live*
    /// deadline, re-read here because that is what a later GET reads.
    async fn late_flag(submission: &HomeworkSubmission, db: &Database) -> bool {
        let live = homework_db::read(db, submission.get_homework())
            .await
            .unwrap()
            .expect("the homework exists");
        submission.get_updated_at() > live.get_due_at()
    }

    /// The two badge counters on a user row, absent counting as zero.
    async fn counters(user: &UserId, db: &Database) -> (i64, i64) {
        let row = sqlx::query(
            "SELECT homework_submitted_total, homework_on_time_total \
             FROM app_user WHERE id = $1",
        )
        .bind(user.uuid())
        .fetch_one(db)
        .await
        .unwrap();
        (
            row.try_get::<i64, _>(0).unwrap(),
            row.try_get::<i64, _>(1).unwrap(),
        )
    }

    #[tokio::test]
    async fn resubmit_keeps_submitted_at_and_restamps_updated_at() {
        let (db, _leases) = crate::database::init_test_db().await;
        let homework = a_homework(far_future(), &db).await;
        let user = a_student("ogrenci", &db).await;

        let (first, _) = upsert(
            &db,
            &homework,
            &user,
            Some(SubmissionText::try_new("draft").unwrap()),
            false,
        )
        .await
        .unwrap()
        .unwrap();
        // A re-submit lands on the same row: submitted_at (the first hand-in)
        // pinned, updated_at moves forward, and the text can be cleared.
        let (second, _) = upsert(&db, &homework, &user, None, false)
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
            list_for_homework(&db, homework.get_id())
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
    /// [`upsert`] and the "version B" upsert below comes
    /// back `Some` — the exact swap this guards against.
    #[tokio::test]
    async fn a_grade_freezes_the_submission_row_itself() {
        use crate::db::homework_result;
        use crate::domain::homework_result::HomeworkStatus;

        let (db, _leases) = crate::database::init_test_db().await;
        let homework = a_homework(far_future(), &db).await;
        let user = a_student("ogrenci", &db).await;
        let teacher = a_teacher(&db).await;
        let graded = |value| {
            let text = SubmissionText::try_new(value).unwrap();
            upsert(&db, &homework, &user, Some(text), false)
        };

        let (version_a, _) = graded("version A").await.unwrap().unwrap();
        assert!(!is_graded(&db, version_a.get_id()).await.unwrap());

        assert_eq!(counters(&user, &db).await, (1, 1));

        homework_result::grade(
            &db,
            homework.get_id(),
            &user,
            HomeworkStatus::try_new("done").unwrap(),
            None,
            &teacher,
        )
        .await
        .unwrap();
        assert!(is_graded(&db, version_a.get_id()).await.unwrap());

        // The swap the teacher would never see: refused, and the stored text is
        // still the version that was graded.
        assert!(graded("version B").await.unwrap().is_none());
        assert_eq!(
            read_for(&db, homework.get_id(), &user)
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
        let frozen = read_for(&db, homework.get_id(), &user)
            .await
            .unwrap()
            .unwrap();
        assert!(delete(&db, frozen).await.unwrap().is_none());
        assert!(
            read_for(&db, homework.get_id(), &user)
                .await
                .unwrap()
                .is_some()
        );
        // A refused delete decrements nothing either.
        assert_eq!(counters(&user, &db).await, (1, 1));

        // Un-grading unfreezes it, stamp and all.
        homework_result::remove(&db, homework.get_id(), &user)
            .await
            .unwrap();
        assert!(!is_graded(&db, version_a.get_id()).await.unwrap());
        let (reopened, _) = graded("version B").await.unwrap().unwrap();
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
        let (db, _leases) = crate::database::init_test_db().await;
        let homework = a_homework(far_future(), &db).await;
        let user = a_student("ogrenci", &db).await;

        let (original, _) = upsert(
            &db,
            &homework,
            &user,
            Some(SubmissionText::try_new("photo answer").unwrap()),
            false,
        )
        .await
        .unwrap()
        .unwrap();
        // A file add/delete touches the submission: updated_at moves (never
        // backwards), while the first-submit stamp and the text stay put.
        let touched = touch(&db, original.get_id()).await.unwrap();
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
    /// Bite check: drop the `UPDATE $usr` from [`delete`]
    /// and the last assertion reads `(2, 2)` — the farm.
    #[tokio::test]
    async fn submit_delete_submit_is_worth_one_submission() {
        let (db, _leases) = crate::database::init_test_db().await;
        let homework = a_homework(far_future(), &db).await;
        let user = a_student("ogrenci", &db).await;

        upsert(&db, &homework, &user, None, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(counters(&user, &db).await, (1, 1));

        let mine = read_for(&db, homework.get_id(), &user)
            .await
            .unwrap()
            .unwrap();
        delete(&db, mine).await.unwrap().unwrap();
        assert_eq!(counters(&user, &db).await, (0, 0));
        // Floored: a second withdrawal (or a row from before the columns
        // existed) can never push either counter negative.
        assert!(
            read_for(&db, homework.get_id(), &user)
                .await
                .unwrap()
                .is_none()
        );

        upsert(&db, &homework, &user, None, false)
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
        let (db, _leases) = crate::database::init_test_db().await;
        let punctual = a_homework(far_future(), &db).await;
        let overdue = a_homework(Timestamp::from_millis(1), &db).await;
        let user = a_student("ogrenci", &db).await;

        upsert(&db, &punctual, &user, None, false)
            .await
            .unwrap()
            .unwrap();
        upsert(&db, &overdue, &user, None, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(counters(&user, &db).await, (2, 1));

        // Withdrawing the *late* one takes back only the submission, never the
        // on-time credit the punctual one earned — the zero floor would hide a
        // wrong subtraction here if the on-time count were sitting at zero.
        let late = read_for(&db, overdue.get_id(), &user)
            .await
            .unwrap()
            .unwrap();
        delete(&db, late).await.unwrap().unwrap();
        assert_eq!(counters(&user, &db).await, (1, 1));

        // ... and withdrawing the punctual one gives back exactly that credit.
        let kept = read_for(&db, punctual.get_id(), &user)
            .await
            .unwrap()
            .unwrap();
        delete(&db, kept).await.unwrap().unwrap();
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
        let (db, _leases) = crate::database::init_test_db().await;
        let punctual = a_homework(far_future(), &db).await;
        let overdue = a_homework(Timestamp::from_millis(1), &db).await;
        let user = a_student("ogrenci", &db).await;

        upsert(&db, &punctual, &user, None, false)
            .await
            .unwrap()
            .unwrap();
        upsert(&db, &overdue, &user, None, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(counters(&user, &db).await, (2, 1));
        // Aged into legacy rows: the counters keep the credit, the rows lose the
        // verdict — the exact state of every submission on an existing volume.
        sqlx::query("UPDATE homework_submission SET counted_on_time = NULL")
            .execute(&db)
            .await
            .unwrap();

        let late = read_for(&db, overdue.get_id(), &user)
            .await
            .unwrap()
            .unwrap();
        delete(&db, late).await.unwrap().unwrap();
        assert_eq!(counters(&user, &db).await, (1, 1));

        let kept = read_for(&db, punctual.get_id(), &user)
            .await
            .unwrap()
            .unwrap();
        delete(&db, kept).await.unwrap().unwrap();
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
        let (db, _leases) = crate::database::init_test_db().await;
        let user = a_student("ogrenci", &db).await;

        // Extended at 23:59: the snapshot says the deadline has passed, the
        // store says it has not. A hand-in now is on time.
        let extended = a_homework(Timestamp::from_millis(1), &db).await;
        deadline_moves_to(&extended, far_future(), &db).await;
        let (landed, _) = upsert(&db, &extended, &user, None, false)
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
        let (missed, _) = upsert(&db, &pulled, &user, None, false)
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
        delete(&db, missed).await.unwrap().unwrap();
        assert_eq!(counters(&user, &db).await, (1, 1));
        delete(&db, landed).await.unwrap().unwrap();
        assert_eq!(counters(&user, &db).await, (0, 0));
    }
}
