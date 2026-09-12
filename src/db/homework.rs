//! The `homework` table: row reads and listings, the subject-counter claim
//! that pins a homework to a live subject row, the reference-counting PATCH —
//! whose audience-narrowing orphan guard is one transaction with the write,
//! serialized on the homework row's `FOR UPDATE` lock — and the cascading
//! delete.

use crate::database::{Database, tx_with_retry};
use crate::domain::course::CourseId;
use crate::domain::homework::{Homework, HomeworkDescription, HomeworkId, HomeworkTitle};
use crate::domain::subject::SubjectId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use sqlx::PgConnection;

/// The answer a link to a subject that is not there gets, on create and on
/// re-tag alike — the conditional claim on the subject row matches nothing,
/// and the caller says exactly what the web layer's pre-flight lookup would
/// have.
fn subject_gone() -> AppError {
    AppError::Validation(ValidationError::Invalid {
        field: "subject_id",
        reason: "subject does not exist",
    })
}

/// What a stale re-tag answers. The shared PATCH builder
/// ([`crate::db::field_update`]) carries the same refusal; the homework PATCH
/// keeps its own copy because its orphan guard needs the row lock the generic
/// builder cannot take.
const STALE_MOVE: &str = "the link this update moves changed since it was read; re-read and retry";

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
    // The subject's reference is taken in the very statement that writes the
    // row — the exam question's twin ([`crate::db::exam_question`]): the
    // claim (`UPDATE subject … +1`) and the insert are one CTE, so a subject
    // a delete already removed matches nothing and nothing is written, which
    // is the 400 the web layer's pre-flight check answers with. The subject
    // delete is refused while this counter is non-zero, so the two writers
    // contend on the subject row itself, and a crash cannot strand a claim
    // that would make the subject undeletable forever.
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
    let assigned_values: Vec<uuid::Uuid> = homework
        .assigned
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(UserId::uuid)
        .collect();
    // The fresh v7 id cannot collide, so the pair-unique answer has no rival
    // here; the mapping is kept for symmetry with the other claim sites.
    let created = sqlx::query_as!(
        Homework,
        r#"WITH seat AS (
               UPDATE subject SET homework_count = homework_count + 1
               WHERE id = $1
               RETURNING 1
           )
           INSERT INTO homework (id, course, subject, title, description, due_at, assigned, created_by, created_at)
           SELECT $2, $3, $1, $4, $5, $6, $7, $8, $9 WHERE EXISTS (SELECT 1 FROM seat)
           RETURNING id AS "id: HomeworkId",
                     course AS "course: CourseId",
                     subject AS "subject: SubjectId",
                     title AS "title: HomeworkTitle",
                     description AS "description: HomeworkDescription",
                     due_at AS "due_at: Timestamp",
                     CASE WHEN cardinality(assigned) = 0 THEN NULL ELSE assigned END
                         AS "assigned: Vec<UserId>",
                     created_by AS "created_by: UserId",
                     created_at AS "created_at: Timestamp""#,
        subject.uuid(),
        homework.id.uuid(),
        course.uuid(),
        homework.title.as_str(),
        homework.description.as_ref().map(HomeworkDescription::as_str),
        homework.due_at.as_millis(),
        &assigned_values,
        homework.created_by.uuid(),
        homework.created_at.as_millis(),
    )
    .fetch_optional(db)
    .await
    .map_err(|err| {
        if crate::database::unique_violation(&err).is_some() {
            AppError::Internal("failed to create homework".into())
        } else {
            AppError::from(err)
        }
    })?;
    created.ok_or_else(subject_gone)
}

pub async fn read(db: &Database, id: &HomeworkId) -> Result<Option<Homework>, AppError> {
    Ok(sqlx::query_as!(
        Homework,
        r#"SELECT id AS "id: HomeworkId",
                  course AS "course: CourseId",
                  subject AS "subject: SubjectId",
                  title AS "title: HomeworkTitle",
                  description AS "description: HomeworkDescription",
                  due_at AS "due_at: Timestamp",
                  CASE WHEN cardinality(assigned) = 0 THEN NULL ELSE assigned END
                      AS "assigned: Vec<UserId>",
                  created_by AS "created_by: UserId",
                  created_at AS "created_at: Timestamp"
           FROM homework WHERE id = $1"#,
        id.uuid()
    )
    .fetch_optional(db)
    .await?)
}

/// The course's homework, newest first (v7 ids sort by creation). The web
/// layer retains only the rows a given student `student_sees`.
pub async fn list_for_course(db: &Database, course: &CourseId) -> Result<Vec<Homework>, AppError> {
    Ok(sqlx::query_as!(
        Homework,
        r#"SELECT id AS "id: HomeworkId",
                  course AS "course: CourseId",
                  subject AS "subject: SubjectId",
                  title AS "title: HomeworkTitle",
                  description AS "description: HomeworkDescription",
                  due_at AS "due_at: Timestamp",
                  CASE WHEN cardinality(assigned) = 0 THEN NULL ELSE assigned END
                      AS "assigned: Vec<UserId>",
                  created_by AS "created_by: UserId",
                  created_at AS "created_at: Timestamp"
           FROM homework WHERE course = $1 ORDER BY id DESC"#,
        course.uuid()
    )
    .fetch_all(db)
    .await?)
}

/// Every homework in the system, newest first — the manager+ view of the
/// cross-course "my homework" list.
pub async fn list_all(db: &Database) -> Result<Vec<Homework>, AppError> {
    Ok(sqlx::query_as!(
        Homework,
        r#"SELECT id AS "id: HomeworkId",
                  course AS "course: CourseId",
                  subject AS "subject: SubjectId",
                  title AS "title: HomeworkTitle",
                  description AS "description: HomeworkDescription",
                  due_at AS "due_at: Timestamp",
                  CASE WHEN cardinality(assigned) = 0 THEN NULL ELSE assigned END
                      AS "assigned: Vec<UserId>",
                  created_by AS "created_by: UserId",
                  created_at AS "created_at: Timestamp"
           FROM homework ORDER BY id DESC"#,
    )
    .fetch_all(db)
    .await?)
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
    let ids: Vec<uuid::Uuid> = courses.iter().map(CourseId::uuid).collect();
    Ok(sqlx::query_as!(
        Homework,
        r#"SELECT id AS "id: HomeworkId",
                  course AS "course: CourseId",
                  subject AS "subject: SubjectId",
                  title AS "title: HomeworkTitle",
                  description AS "description: HomeworkDescription",
                  due_at AS "due_at: Timestamp",
                  CASE WHEN cardinality(assigned) = 0 THEN NULL ELSE assigned END
                      AS "assigned: Vec<UserId>",
                  created_by AS "created_by: UserId",
                  created_at AS "created_at: Timestamp"
           FROM homework WHERE course = ANY($1) ORDER BY id DESC"#,
        &ids
    )
    .fetch_all(db)
    .await?)
}

/// The homework of `course` that `user` is meant to see — whole-course ones
/// plus any subset that names them — newest first. Backs a student's (or an
/// observer's) per-course homework report; mirrors
/// [`Homework::student_sees`](crate::domain::homework::Homework::student_sees)
/// in SQL so the filter runs in the database.
pub async fn list_for_user_in_course(
    db: &Database,
    course: &CourseId,
    user: &UserId,
) -> Result<Vec<Homework>, AppError> {
    Ok(sqlx::query_as!(
        Homework,
        r#"SELECT id AS "id: HomeworkId",
                  course AS "course: CourseId",
                  subject AS "subject: SubjectId",
                  title AS "title: HomeworkTitle",
                  description AS "description: HomeworkDescription",
                  due_at AS "due_at: Timestamp",
                  CASE WHEN cardinality(assigned) = 0 THEN NULL ELSE assigned END
                      AS "assigned: Vec<UserId>",
                  created_by AS "created_by: UserId",
                  created_at AS "created_at: Timestamp"
           FROM homework
           WHERE course = $1 AND (cardinality(assigned) = 0 OR $2 = ANY(assigned))
           ORDER BY id DESC"#,
        course.uuid(),
        user.uuid()
    )
    .fetch_all(db)
    .await?)
}

/// Re-tag, re-title, re-describe, re-schedule, or re-scope the homework.
/// Request-scoped: every parameter is `Option`, `None` meaning the PATCH
/// did not carry that field, so it is not written at all. Handing the
/// snapshot's value back instead would revert a concurrent edit of that
/// field — under the row lock below, an uncarried field keeps the value read
/// *under that lock*, never the handler's snapshot. `course`, `created_by`,
/// and `created_at` are readonly and never appear in the write.
///
/// `description` takes `Some(None)` to clear; `assigned` takes `Some(None)`
/// to widen back to the whole course (stored as `'{}'`, read back as `None`).
/// The web layer has already re-checked a new `due_at` against now, a new
/// `subject` against the course, and — inside the transaction here — the
/// narrowing against the work that would be orphaned by it.
pub async fn update(
    db: &Database,
    homework: Homework,
    subject: Option<SubjectId>,
    title: Option<HomeworkTitle>,
    description: Option<Option<HomeworkDescription>>,
    due_at: Option<Timestamp>,
    assigned: Option<Option<Vec<UserId>>>,
) -> Result<Homework, AppError> {
    tx_with_retry(db, false, async move |tx| {
        // The homework row's lock. The orphan guard's reads and the write
        // below are one transaction with it, so a submission — whose own
        // transaction locks this very row before writing — cannot land
        // between the check and the narrowing, orphaned by an audience change
        // that just missed it. The lock also replaces the old CAS's row read:
        // the re-tag check below compares the handler's snapshot against the
        // row as it is *locked*, not as it was first read.
        let current = sqlx::query_as!(
            Homework,
            r#"SELECT id AS "id: HomeworkId",
                      course AS "course: CourseId",
                      subject AS "subject: SubjectId",
                      title AS "title: HomeworkTitle",
                      description AS "description: HomeworkDescription",
                      due_at AS "due_at: Timestamp",
                      CASE WHEN cardinality(assigned) = 0 THEN NULL ELSE assigned END
                          AS "assigned: Vec<UserId>",
                      created_by AS "created_by: UserId",
                      created_at AS "created_at: Timestamp"
               FROM homework WHERE id = $1 FOR UPDATE"#,
            homework.get_id().uuid()
        )
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;

        // The orphan guard runs on exactly the requests that re-scope the
        // audience. An absent `assigned` writes nothing, so the stored subset
        // is untouched and no narrowing can happen behind the guard's back.
        if let Some(resolved) = assigned.as_ref() {
            ensure_no_orphans(&current, resolved.as_deref(), tx).await?;
        }

        // A re-tag moves a reference: the new subject's claim and the old
        // one's release ride the same transaction as the link write, so no
        // crash can leave a count without its link (the subject would be
        // undeletable forever) or a link without its count. The release goes
        // first and the claim refuses on a gone row, so a move to a deleted
        // subject is a rolled-back no-op — every abort takes the whole move
        // back with it.
        let moving = subject.as_ref().filter(|next| **next != homework.subject);
        if let Some(next) = moving {
            sqlx::query!(
                "UPDATE subject SET homework_count = GREATEST(homework_count - 1, 0) WHERE id = $1",
                current.subject.uuid()
            )
            .execute(&mut *tx)
            .await?;
            let seat = sqlx::query!(
                r#"UPDATE subject SET homework_count = homework_count + 1
                   WHERE id = $1 RETURNING 1 AS "seat: i32""#,
                next.uuid()
            )
            .fetch_optional(&mut *tx)
            .await?;
            if seat.is_none() {
                return Err(subject_gone());
            }
        }

        // The stale-re-tag check. Armed by the request *carrying* the subject
        // — even when it re-states the tag its snapshot showed, shifting no
        // counter — so two PATCHes moving the same homework off the same
        // subject cannot both claim their target: the loser's snapshot says
        // A, the locked row says B, and the refusal is the 409 the generic
        // PATCH builder has always answered with.
        if subject.is_some() && current.subject != homework.subject {
            return Err(AppError::Conflict(STALE_MOVE));
        }

        // What the PATCH carried is written; what it did not carry keeps the
        // value just read under the lock. Under that lock this resolved
        // full-row write is exactly the request-scoped SET the old builder
        // emitted — except it is one static statement instead of a runtime
        // build.
        let new_subject = subject.clone().unwrap_or_else(|| current.subject.clone());
        let new_title = title.clone().unwrap_or_else(|| current.title.clone());
        let new_description = match description.clone() {
            None => current.description.clone(),
            Some(None) => None,
            Some(Some(text)) => Some(text),
        };
        let new_due_at = due_at.unwrap_or(current.due_at);
        let new_assigned = match assigned.clone() {
            None => current.assigned.clone(),
            // Clear = the whole course, stored as `'{}'`.
            Some(None) => None,
            Some(Some(subset)) => Some(subset),
        };
        let assigned_values: Vec<uuid::Uuid> = new_assigned
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(UserId::uuid)
            .collect();
        sqlx::query_as!(
            Homework,
            r#"UPDATE homework SET subject = $2, title = $3, description = $4,
                   due_at = $5, assigned = $6
               WHERE id = $1
               RETURNING id AS "id: HomeworkId",
                         course AS "course: CourseId",
                         subject AS "subject: SubjectId",
                         title AS "title: HomeworkTitle",
                         description AS "description: HomeworkDescription",
                         due_at AS "due_at: Timestamp",
                         CASE WHEN cardinality(assigned) = 0 THEN NULL ELSE assigned END
                             AS "assigned: Vec<UserId>",
                         created_by AS "created_by: UserId",
                         created_at AS "created_at: Timestamp""#,
            homework.get_id().uuid(),
            new_subject.uuid(),
            new_title.as_str(),
            new_description.as_ref().map(HomeworkDescription::as_str),
            new_due_at.as_millis(),
            &assigned_values,
        )
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)
    })
    .await
}

/// Refuse (409) a PATCH that would narrow `homework`'s audience so a student
/// who already submitted or was graded falls outside it — their work would be
/// stranded. `new_assigned` is the proposed subset (`None` = whole course, in
/// which case no one can be orphaned). The blocking students are named in the
/// message so the teacher knows whose work to clear (or whom to keep assigned)
/// first.
///
/// Runs inside the caller's transaction, under the homework row's lock, so
/// the check and the narrowing cannot be driven through by a submission.
async fn ensure_no_orphans(
    homework: &Homework,
    new_assigned: Option<&[UserId]>,
    tx: &mut PgConnection,
) -> Result<(), AppError> {
    // Whole-course covers everyone — no narrowing, no orphans.
    let Some(subset) = new_assigned else {
        return Ok(());
    };
    let submitted: Vec<UserId> = sqlx::query!(
        r#"SELECT app_user AS "user: UserId" FROM homework_submission WHERE homework = $1"#,
        homework.get_id().uuid()
    )
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|row| row.user)
    .collect();
    let graded: Vec<UserId> = sqlx::query!(
        r#"SELECT app_user AS "user: UserId" FROM homework_result WHERE homework = $1"#,
        homework.get_id().uuid()
    )
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|row| row.user)
    .collect();
    let mut blocked: Vec<String> = Vec::new();
    for user in submitted.iter().chain(graded.iter()) {
        let key = user.key().to_string();
        if !subset.contains(user) && !blocked.contains(&key) {
            blocked.push(key);
        }
    }
    if blocked.is_empty() {
        Ok(())
    } else {
        Err(AppError::ConflictOwned(format!(
            "narrowing the assigned list would orphan existing work by {} student(s): {}",
            blocked.len(),
            blocked.join(", ")
        )))
    }
}

/// Delete the homework and cascade its submissions, their files, and its
/// results — one transaction, so a crash can't orphan a submission under a
/// vanished homework. The submission file *blobs* are the web layer's to
/// unlink: their names are returned (collected inside the transaction,
/// before the wipes — a file row inserted after an out-of-transaction
/// collection would be deleted here while its key was already gone from the
/// list, stranding the blob) and removed after the rows are gone.
pub async fn delete(
    db: &Database,
    homework: Homework,
) -> Result<(Homework, Vec<String>), AppError> {
    tx_with_retry(db, true, async move |tx| {
        let blob_keys: Vec<String> = sqlx::query!(
            r#"SELECT file FROM homework_file
               WHERE submission IN (SELECT id FROM homework_submission WHERE homework = $1)"#,
            homework.get_id().uuid()
        )
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(|row| row.file)
        .collect();
        sqlx::query!(
            r#"DELETE FROM homework_file
               WHERE submission IN (SELECT id FROM homework_submission WHERE homework = $1)"#,
            homework.get_id().uuid()
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "DELETE FROM homework_result WHERE homework = $1",
            homework.get_id().uuid()
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "DELETE FROM homework_submission WHERE homework = $1",
            homework.get_id().uuid()
        )
        .execute(&mut *tx)
        .await?;
        let deleted = sqlx::query_as!(
            Homework,
            r#"DELETE FROM homework WHERE id = $1
               RETURNING id AS "id: HomeworkId",
                         course AS "course: CourseId",
                         subject AS "subject: SubjectId",
                         title AS "title: HomeworkTitle",
                         description AS "description: HomeworkDescription",
                         due_at AS "due_at: Timestamp",
                         CASE WHEN cardinality(assigned) = 0 THEN NULL ELSE assigned END
                             AS "assigned: Vec<UserId>",
                         created_by AS "created_by: UserId",
                         created_at AS "created_at: Timestamp""#,
            homework.get_id().uuid()
        )
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
        // The subject's reference is given back in this same transaction, off
        // the row the delete actually removed.
        sqlx::query!(
            "UPDATE subject SET homework_count = GREATEST(homework_count - 1, 0) WHERE id = $1",
            deleted.subject.uuid()
        )
        .execute(&mut *tx)
        .await?;
        Ok((deleted, blob_keys))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::subject::{Subject, SubjectDescription, SubjectName};

    async fn a_subject(name: &str, db: &Database) -> Subject {
        crate::db::subject::create(
            db,
            &crate::db::course::a_test_course(db).await,
            SubjectName::try_new(name).unwrap(),
            SubjectDescription::try_new("").unwrap(),
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
        crate::db::subject::delete(&db, subject).await.unwrap();

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
        assert!(
            crate::db::subject::delete(&db, from).await.is_ok(),
            "no reference left"
        );
        assert!(
            crate::db::subject::delete(&db, to).await.is_err(),
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
        crate::db::subject::delete(&db, dead).await.unwrap();
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
