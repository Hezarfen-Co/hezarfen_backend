//! The `homework` table: row reads and listings, the count-and-create that
//! pins a homework to a live subject row, the reference-counting PATCH, and
//! the cascading delete. The PATCH's orphan guard and the audience gate live
//! in [`crate::service::homework`].

use surrealdb::types::RecordId;

use crate::constant::SUBJECT_HOMEWORK_COUNT_FIELD;
use crate::database::Database;
use crate::db::cap;
use crate::db::field_update::FieldUpdate;
use crate::domain::course::CourseId;
use crate::domain::homework::{Homework, HomeworkDescription, HomeworkId, HomeworkTitle};
use crate::domain::subject::SubjectId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// The answer a link to a subject that is not there gets, on create and on
/// re-tag alike — both claims are conditional writes on the subject row, so a
/// subject a delete already removed matches nothing and the caller says exactly
/// what the web layer's pre-flight lookup would have.
fn subject_gone() -> AppError {
    AppError::Validation(ValidationError::Invalid {
        field: "subject_id",
        reason: "subject does not exist",
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the sibling entities' create(field, field, ..) shape"
)]
pub async fn create(
    db: &Database,
    course: &CourseId,
    subject: &SubjectId,
    title: HomeworkTitle,
    description: Option<HomeworkDescription>,
    due_at: Timestamp,
    assigned: Option<Vec<UserId>>,
    created_by: &UserId,
) -> Result<Homework, AppError> {
    // The subject's reference is taken in the very transaction that writes
    // the row — the exam question's twin
    // ([`crate::db::exam_question`]): the subject delete
    // is refused while this counter is non-zero, so the create and the
    // delete contend on the subject record rather than on a cross-table
    // count neither of them sees the other move, and a crash can no longer
    // strand a claim that would make the subject undeletable forever. A
    // refused claim means the subject is already gone, which is the 400 the
    // web layer's pre-flight check answers with.
    let counted = subject.record();
    let homework = Homework {
        id: HomeworkId::generate(),
        course: course.clone(),
        subject: subject.clone(),
        title,
        description,
        due_at,
        assigned,
        created_by: created_by.clone(),
        created_at: Timestamp::now(),
    };
    let id = homework.id.record();
    match cap::claim_and_create(
        &counted,
        SUBJECT_HOMEWORK_COUNT_FIELD,
        cap::UNLIMITED,
        &id,
        &homework,
        db,
    )
    .await?
    {
        cap::Claimed::Made(created) => Ok(created),
        cap::Claimed::Full => Err(subject_gone()),
        cap::Claimed::Duplicate => Err(AppError::Internal("failed to create homework".into())),
    }
}

pub async fn read(db: &Database, id: &HomeworkId) -> Result<Option<Homework>, AppError> {
    Ok(db.select(id.record()).await?)
}

/// The course's homework, newest first (ULID ids sort by creation). The web
/// layer retains only the rows a given student `student_sees`.
pub async fn list_for_course(db: &Database, course: &CourseId) -> Result<Vec<Homework>, AppError> {
    let mut result = db
        .query("SELECT * FROM homework WHERE course = $course ORDER BY id DESC")
        .bind(("course", course.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<Homework>>(0)?)
}

/// Every homework in the system, newest first — the manager+ view of the
/// cross-course "my homework" list.
pub async fn list_all(db: &Database) -> Result<Vec<Homework>, AppError> {
    let mut result = db
        .query("SELECT * FROM homework ORDER BY id DESC")
        .await?
        .check()?;
    Ok(result.take::<Vec<Homework>>(0)?)
}

/// Every homework of every course in `courses`, newest first (one query) —
/// the cross-course list over a caller's visible courses. The web layer
/// still trims each course's rows to what the caller may see (a student to
/// the ones they `student_sees`).
pub async fn list_for_courses(
    db: &Database,
    courses: &[CourseId],
) -> Result<Vec<Homework>, AppError> {
    if courses.is_empty() {
        return Ok(Vec::new());
    }
    let records: Vec<RecordId> = courses.iter().map(CourseId::record).collect();
    let mut result = db
        .query("SELECT * FROM homework WHERE course IN $courses ORDER BY id DESC")
        .bind(("courses", records))
        .await?
        .check()?;
    Ok(result.take::<Vec<Homework>>(0)?)
}

/// The homework of `course` that `user` is meant to see — whole-course ones
/// plus any subset that names them — newest first. Backs a student's (or an
/// observer's) per-course homework report; mirrors
/// [`Homework::student_sees`](crate::domain::homework::Homework::student_sees)
/// in SurQL so the filter runs in the database.
pub async fn list_for_user_in_course(
    db: &Database,
    course: &CourseId,
    user: &UserId,
) -> Result<Vec<Homework>, AppError> {
    let mut result = db
        .query(
            "SELECT * FROM homework
             WHERE course = $course
               AND (assigned = NONE OR assigned = [] OR $usr IN assigned)
             ORDER BY id DESC",
        )
        .bind(("course", course.record()))
        .bind(("usr", user.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<Homework>>(0)?)
}

/// Re-tag, re-title, re-describe, re-schedule, or re-scope the homework.
/// Request-scoped: every parameter is `Option`, `None` meaning the PATCH
/// did not carry that field, so it is not written at all. Handing the
/// snapshot's value back instead would revert a concurrent edit of that
/// field — scoping the `SET` alone does not prevent that, the *values* must
/// come from the request. `course`, `created_by`, and `created_at` are
/// `READONLY` and never appear in the write.
///
/// `description` and `assigned` are nullable columns, so they take a
/// `NONE` (clear the description / widen back to the whole course). The web
/// layer has already re-checked a new `due_at` against now, a new `subject`
/// against the course, and refused a narrowing that would orphan work.
pub async fn update(
    db: &Database,
    homework: Homework,
    subject: Option<SubjectId>,
    title: Option<HomeworkTitle>,
    description: Option<Option<HomeworkDescription>>,
    due_at: Option<Timestamp>,
    assigned: Option<Option<Vec<UserId>>>,
) -> Result<Homework, AppError> {
    let assigned = assigned
        .map(|subset| subset.map(|users| users.iter().map(UserId::record).collect::<Vec<_>>()));
    // A re-tag moves a reference: the new subject's claim and the old one's
    // release ride the same transaction as the link write, so no crash can
    // leave a count without its link (the subject would be undeletable
    // forever) or a link without its count. `subject` is required, so the
    // snapshot's value is always the CAS expectation — two PATCHes moving
    // the same homework off the same subject would otherwise both claim
    // their target, and the loser is refused with a 409 instead. The
    // `.refcount` call is unconditional: the CAS is armed by the request
    // *carrying* `subject_id`, not by a counter moving, so a PATCH that
    // re-states the tag its snapshot showed — shifting no counter at all —
    // is still refused when a rival moved the tag in between. A PATCH that
    // carried no `subject_id` arms nothing and writes what it always did.
    let (claim, release) = subject
        .as_ref()
        .filter(|next| **next != homework.subject)
        .map(|next| (next.record(), homework.subject.record()))
        .unzip();
    FieldUpdate::new(homework.id.record())
        .set("subject", subject.map(|subject| subject.record()))
        .set("title", title)
        .set("description", description)
        .set("due_at", due_at)
        .set("assigned", assigned)
        .refcount(
            SUBJECT_HOMEWORK_COUNT_FIELD,
            "subject",
            Some(homework.subject.record()),
            claim,
            release,
            subject_gone(),
        )
        .run::<Homework>(db)
        .await
}

/// Delete the homework and cascade its submissions, their files, and its
/// results — one transaction, so a crash can't orphan a submission under a
/// vanished homework. The submission file *blobs* are the web layer's to
/// unlink: it collects their names via
/// [`crate::db::homework_file::file_keys_for_homework`]
/// before calling this, and removes them after the rows are gone.
pub async fn delete(db: &Database, homework: Homework) -> Result<Homework, AppError> {
    let mut result = db
        .query(
            format!(
                "BEGIN TRANSACTION;
                 DELETE homework_file WHERE submission IN (SELECT VALUE id FROM homework_submission WHERE homework = $hw);
                 DELETE homework_submission WHERE homework = $hw;
                 DELETE homework_result WHERE homework = $hw;
                 LET $gone = (DELETE $hw RETURN BEFORE);
                 FOR $sub IN ($gone.subject ?? []) {{
                     UPDATE $sub SET {SUBJECT_HOMEWORK_COUNT_FIELD} =
                         math::max([({SUBJECT_HOMEWORK_COUNT_FIELD} ?? 0) - 1, 0])
                 }};
                 RETURN $gone;
                 COMMIT TRANSACTION;"
            ),
        )
        .bind(("hw", homework.id.record()))
        .await?
        .check()?;
    // The subject's reference is given back in this same transaction, off
    // what the delete actually removed. Read through the trailing `RETURN`,
    // not a hand-counted slot — see [`crate::db::exam::delete`].
    let slot = result.num_statements().saturating_sub(2);
    let deleted: Option<Homework> = result.take::<Vec<Homework>>(slot)?.into_iter().next();
    deleted.ok_or(AppError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::subject::{Subject, SubjectDescription, SubjectName};

    async fn a_subject(name: &str, db: &Database) -> Subject {
        Subject::create(
            &crate::db::course::a_test_course(db).await,
            SubjectName::try_new(name).unwrap(),
            SubjectDescription::try_new("").unwrap(),
            db,
        )
        .await
        .unwrap()
    }

    async fn homework_on(subject: &SubjectId, db: &Database) -> Homework {
        create(
            db,
            &CourseId::from_key("course"),
            subject,
            HomeworkTitle::try_new("essay").unwrap(),
            None,
            Timestamp::from_millis(1),
            None,
            &UserId::from_key("teacher"),
        )
        .await
        .unwrap()
    }

    /// The stored `homework_count` on one subject, absent counting as zero.
    async fn count_on(subject: &SubjectId, db: &Database) -> i64 {
        let mut result = db
            .query(format!(
                "SELECT VALUE ({SUBJECT_HOMEWORK_COUNT_FIELD} ?? 0) FROM $sub"
            ))
            .bind(("sub", subject.record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        result
            .take::<Vec<i64>>(0)
            .unwrap()
            .first()
            .copied()
            .unwrap_or(0)
    }

    /// How many rows `sql` selects ids for.
    async fn rows(sql: &str, db: &Database) -> usize {
        let mut result = db.query(sql).await.unwrap().check().unwrap();
        result.take::<Vec<RecordId>>(0).unwrap().len()
    }

    /// The invariant on the create path: the claim and the row it accounts for
    /// commit together or not at all. A refused create leaves *neither* — no
    /// homework row, and no count stranded on a subject (the subject's delete
    /// guard reads that count, so a stray one makes it undeletable forever).
    #[tokio::test]
    async fn a_refused_create_writes_neither_row_nor_count() {
        let db = crate::database::init_mem().await.unwrap();
        let subject = a_subject("algebra", &db).await;
        let id = subject.get_id().clone();
        subject.delete(&db).await.unwrap();

        let error = create(
            &db,
            &CourseId::from_key("course"),
            &id,
            HomeworkTitle::try_new("essay").unwrap(),
            None,
            Timestamp::from_millis(1),
            None,
            &UserId::from_key("teacher"),
        )
        .await
        .expect_err("a subject that is gone must not be taggable");
        assert!(error.to_string().contains("subject does not exist"));
        assert_eq!(
            rows("SELECT VALUE id FROM homework", &db).await,
            0,
            "a refused create may write no row"
        );
        assert_eq!(
            rows("SELECT VALUE id FROM subject", &db).await,
            0,
            "…and least of all a count on a subject it just brought back"
        );
    }

    /// The invariant on the PATCH path: a re-tag carries the new subject's
    /// claim and the old one's release with the link itself.
    #[tokio::test]
    async fn a_subject_move_moves_the_count() {
        let db = crate::database::init_mem().await.unwrap();
        let from = a_subject("algebra", &db).await;
        let to = a_subject("geometry", &db).await;
        let homework = homework_on(from.get_id(), &db).await;
        assert_eq!(count_on(from.get_id(), &db).await, 1);

        let moved = update(
            &db,
            homework,
            Some(to.get_id().clone()),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(moved.get_subject(), to.get_id());
        assert_eq!(
            count_on(from.get_id(), &db).await,
            0,
            "the old subject is free"
        );
        assert_eq!(count_on(to.get_id(), &db).await, 1, "the new one is not");
        assert!(from.delete(&db).await.is_ok(), "no reference left");
        assert!(
            to.delete(&db).await.is_err(),
            "the reference moved here refuses the delete"
        );
    }

    /// The claim throws inside the same transaction as the link write, so a
    /// move onto a subject that is gone rolls the release back with it: the row
    /// keeps its tag and both counters read as if nothing ran.
    #[tokio::test]
    async fn a_move_to_a_dead_subject_leaves_everything_untouched() {
        let db = crate::database::init_mem().await.unwrap();
        let from = a_subject("algebra", &db).await;
        let dead = a_subject("geometry", &db).await;
        let gone = dead.get_id().clone();
        dead.delete(&db).await.unwrap();
        let homework = homework_on(from.get_id(), &db).await;

        let error = update(
            &db,
            homework.clone(),
            Some(gone.clone()),
            None,
            None,
            None,
            None,
        )
        .await
        .expect_err("a subject that is gone must not be taggable");
        assert!(error.to_string().contains("subject does not exist"));
        let stored = read(&db, homework.get_id()).await.unwrap().unwrap();
        assert_eq!(stored.get_subject(), from.get_id(), "the tag never moved");
        assert_eq!(
            count_on(from.get_id(), &db).await,
            1,
            "the release rolled back with the claim"
        );
        assert_eq!(
            rows("SELECT VALUE id FROM subject", &db).await,
            1,
            "the dead subject was not brought back by a count"
        );
        assert_eq!(count_on(&gone, &db).await, 0);
    }

    /// The double-claim guard. Both movers compute their claim and release from
    /// the row as *they* read it, so two PATCHes re-tagging the same homework
    /// off the same subject both release it and both claim their target — two
    /// counts for one link, and the loser's target is undeletable forever. The
    /// second call here runs on the struct read before the first one landed,
    /// which is that race with the interleaving pinned: it must be refused
    /// outright, and the counts must read as if it never ran.
    #[tokio::test]
    async fn a_stale_mover_is_refused_and_claims_nothing() {
        let db = crate::database::init_mem().await.unwrap();
        let from = a_subject("algebra", &db).await;
        let to = a_subject("geometry", &db).await;
        let other = a_subject("calculus", &db).await;
        let homework = homework_on(from.get_id(), &db).await;
        let stale = homework.clone();
        update(
            &db,
            homework,
            Some(to.get_id().clone()),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let error = update(
            &db,
            stale.clone(),
            Some(other.get_id().clone()),
            None,
            None,
            None,
            None,
        )
        .await
        .expect_err("a mover that read a tag it no longer holds must be refused");
        assert!(
            matches!(error, AppError::Conflict(_)),
            "a lost CAS is a conflict, not a 404 or a 500: {error:?}"
        );
        let stored = read(&db, stale.get_id()).await.unwrap().unwrap();
        assert_eq!(stored.get_subject(), to.get_id(), "the winner's tag");
        assert_eq!(
            count_on(from.get_id(), &db).await,
            0,
            "released once, not twice"
        );
        assert_eq!(count_on(to.get_id(), &db).await, 1, "claimed once");
        assert_eq!(count_on(other.get_id(), &db).await, 0, "never claimed");
    }

    /// The same race with the counters taken out of it. A PATCH that re-states
    /// the subject its snapshot showed shifts *nothing* — no claim and no
    /// release — so if the guard were armed off the counter move it would not be
    /// armed here at all, and the write would land: the winner's tag silently
    /// dragged back, its claim stranded on a subject nothing points at
    /// (undeletable forever) and the reverted-to subject tagged at a count of
    /// zero (deletable while tagged). The guard is armed by the request
    /// *carrying* the column instead, which is why this is refused.
    #[tokio::test]
    async fn a_stale_re_stater_is_refused_and_reverts_nothing() {
        let db = crate::database::init_mem().await.unwrap();
        let from = a_subject("algebra", &db).await;
        let to = a_subject("geometry", &db).await;
        let homework = homework_on(from.get_id(), &db).await;
        let stale = homework.clone();
        update(
            &db,
            homework,
            Some(to.get_id().clone()),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let error = update(
            &db,
            stale.clone(),
            Some(from.get_id().clone()),
            None,
            None,
            None,
            None,
        )
        .await
        .expect_err("re-stating a tag someone else moved must be refused");
        assert!(
            matches!(error, AppError::Conflict(_)),
            "a lost CAS is a conflict, not a silent 200: {error:?}"
        );
        let stored = read(&db, stale.get_id()).await.unwrap().unwrap();
        assert_eq!(stored.get_subject(), to.get_id(), "the winner's tag");
        assert_eq!(
            count_on(from.get_id(), &db).await,
            0,
            "the reverted-to subject must not end up tagged at zero"
        );
        assert_eq!(
            count_on(to.get_id(), &db).await,
            1,
            "…nor the winner's subject counted with nothing pointing at it"
        );

        // A *genuine* no-op re-state — nobody moved underneath it — still lands,
        // and still moves no counter: the CAS passes trivially.
        let fresh = read(&db, stale.get_id()).await.unwrap().unwrap();
        let same = update(
            &db,
            fresh,
            Some(to.get_id().clone()),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("re-stating the tag actually held is not a race");
        assert_eq!(same.get_subject(), to.get_id());
        assert_eq!(
            count_on(from.get_id(), &db).await,
            0,
            "still no counter move"
        );
        assert_eq!(count_on(to.get_id(), &db).await, 1, "claimed once, still");
    }
}
