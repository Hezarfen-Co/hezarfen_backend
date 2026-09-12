//! The `subject` table: the count-and-create that pins a topic to a live
//! course row, the bulk id listing, the field-scoped PATCH, and the guarded
//! delete whose guard reads the row's own reference counters. The type and
//! its validation live in [`crate::domain::subject`].

use crate::constant::SUBJECT_TABLE;
use crate::database::Database;
use crate::db::field_update::FieldUpdate;
use crate::db::page::PagedList;
use crate::domain::course::CourseId;
use crate::domain::subject::{Subject, SubjectDescription, SubjectId, SubjectName};
use crate::error::AppError;

pub async fn create(
    db: &Database,
    course: &CourseId,
    name: SubjectName,
    description: SubjectDescription,
) -> Result<Subject, AppError> {
    let subject = Subject {
        id: SubjectId::generate(),
        course: course.clone(),
        name,
        description,
    };
    // The course foreign key is the existence proof the old bump-and-restore
    // "touch" trick faked: a topic whose course is already gone — or which is
    // deleted while this insert is in flight — is refused here, so a subject
    // can never outlive its course and a bank template can never be tagged
    // with a subject nobody can reach. `23503` is the parent-gone refusal,
    // the same answer the touch produced.
    let created = sqlx::query_as!(
        Subject,
        r#"INSERT INTO subject (id, course, name, description, exam_question_count, homework_count)
           VALUES ($1, $2, $3, $4, 0, 0)
           RETURNING id, course, name, description"#,
        subject.id,
        subject.course,
        subject.name,
        subject.description,
    )
    .fetch_one(db)
    .await;
    match created {
        Ok(created) => Ok(created),
        Err(err) if crate::database::foreign_key_violation(&err) => Err(AppError::NotFound),
        Err(err) => Err(err.into()),
    }
}

pub async fn read(db: &Database, id: &SubjectId) -> Result<Option<Subject>, AppError> {
    let subject = sqlx::query_as!(
        Subject,
        r#"SELECT id, course, name, description FROM subject WHERE id = $1"#,
        id,
    )
    .fetch_optional(db)
    .await?;
    Ok(subject)
}

/// Several subjects in one query — the bulk half of a list endpoint that
/// names each row's subject (a read per row would be an N+1).
pub async fn list_by_ids(db: &Database, ids: &[&SubjectId]) -> Result<Vec<Subject>, AppError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let keys: Vec<SubjectId> = ids.iter().map(|id| **id).collect();
    let rows = sqlx::query_as!(
        Subject,
        r#"SELECT id, course, name, description FROM subject WHERE id = ANY($1)"#,
        keys,
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// The course's subjects in curriculum order (uuid ids sort by creation).
pub async fn list_for_course(
    db: &Database,
    course: &CourseId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Subject>, i64), AppError> {
    PagedList::new("subject WHERE course = $1", "ORDER BY id ASC")
        .bind(course.uuid())
        .run::<Subject>(limit, offset, db)
        .await
}

/// Request-scoped: no lock spans the handler's read and this write, so an
/// omitted field (`None`) is not written at all. Passing the snapshot's
/// value back instead would revert a concurrent edit of that field —
/// scoping the `SET` alone does not prevent that, the values have to come
/// from the request. Neither column is nullable, so plain `Option` per
/// field says everything there is to say.
pub async fn update(
    db: &Database,
    subject: Subject,
    name: Option<SubjectName>,
    description: Option<SubjectDescription>,
) -> Result<Subject, AppError> {
    FieldUpdate::new(SUBJECT_TABLE, subject.id.uuid())
        .set("name", name.map(|name| name.as_str().to_owned()))
        .set("description", description.map(|d| d.as_str().to_owned()))
        .run::<Subject>(db)
        .await
}

/// Delete the subject and clear it off every bank template that carried it
/// as origin metadata — one transaction, so a template can't be left
/// pointing at a subject that no longer exists.
///
/// Exam questions and homework are *not* cascaded: their `subject` is a
/// required field that may not be orphaned, so either one still refuses the
/// delete with a 409. That refusal is this transaction's own check, read
/// off the two reference counters this row carries
/// (`exam_question_count` and its homework twin) under a `FOR UPDATE` row
/// lock — the lock *is* the guard, because every counter writer must
/// update this same row, so no question can land between the check and the
/// delete. The old store could not conflict-check the cross-table shape,
/// which is why the check had to be the `DELETE`'s own `WHERE`; here the
/// row lock does it and the `WHERE` reads off the same columns.
///
/// Which of the two blocked is read off the counters *before* the delete,
/// purely to pick the message; the decision itself was already made by the
/// check.
///
/// The bank's subject is optional metadata, and blocking on it was a dead
/// end — only the template's owner may re-tag it, so a manager could never
/// clear their own 409, and a private template raising it leaked its
/// existence. Its cascade runs *after* the guarded delete, so a refused
/// delete leaves every template's subject where it was.
pub async fn delete(db: &Database, subject: Subject) -> Result<Subject, AppError> {
    crate::database::tx_with_retry(db, false, async |tx| {
        // The pre-image read and the guard in one locked statement: the row
        // (and its counters) cannot change under this transaction.
        let held = sqlx::query!(
            r#"SELECT exam_question_count, homework_count
               FROM subject WHERE id = $1 FOR UPDATE"#,
            subject.id,
        )
        .fetch_optional(&mut *tx)
        .await?;
        let held = held.ok_or(AppError::NotFound)?;
        if held.exam_question_count > 0 {
            return Err(AppError::Conflict(
                "exam questions still reference this subject — re-tag or delete them first",
            ));
        }
        if held.homework_count > 0 {
            return Err(AppError::Conflict(
                "homework still references this subject — re-tag or delete it first",
            ));
        }
        let deleted = sqlx::query_as!(
            Subject,
            r#"DELETE FROM subject WHERE id = $1
               RETURNING id, course, name, description"#,
            subject.id,
        )
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query!(
            r#"UPDATE bank_question SET subject = NULL WHERE subject = $1"#,
            subject.id,
        )
        .execute(&mut *tx)
        .await?;
        Ok(deleted)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::SUBJECT_TABLE;

    /// A curriculum topic must not outlive its course: an orphan 404s through
    /// `subject_with_course`, and worse than the sibling cases,
    /// `must_exist` still *accepts* its id — so a bank template can be
    /// tagged with a subject nobody can reach.
    ///
    /// [`create`] therefore *writes* the course row rather than
    /// reading it ([`cap::touch_and_create`]); the harness and the window it
    /// races in are documented on
    /// [`crate::db::course::assert_no_child_outlives_a_course_delete`].
    /// Mutation-tested: with the bare `db.create` this shipped with, all four
    /// rounds orphan.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs a real SurrealDB server: podman start hezarfen-surrealdb && cargo test -- --ignored"]
    async fn a_subject_never_outlives_its_course() {
        fn make(course: CourseId, db: Database) -> tokio::task::JoinHandle<Result<(), AppError>> {
            tokio::spawn(async move {
                create(
                    &db,
                    &course,
                    SubjectName::try_new("Limits").unwrap(),
                    SubjectDescription::try_new("").unwrap(),
                )
                .await
                .map(|_| ())
            })
        }
        crate::db::course::assert_no_child_outlives_a_course_delete(
            "subject_orphan_race",
            SUBJECT_TABLE,
            make,
        )
        .await;
    }

    /// GUARD, not a retry measurement — read the last paragraph before
    /// trusting this test with the retry. See
    /// [`crate::db::course::delete`]'s race test for why the rate
    /// is counted rather than asserted per round, and why this needs the real
    /// server and a multi-threaded runtime.
    ///
    /// The racer is [`crate::db::exam_question::create`], which claims the subject's
    /// question reference *before* it writes the row — a conditional write on
    /// the same record the delete's `WHERE` reads. `Err(Conflict)` (still
    /// referenced), `Err(NotFound)` and the claim's `Validation` miss are all
    /// correct answers; only `AppError::Db` is the defect. What the two stored
    /// counters below assert is that the sweep genuinely straddled the site:
    /// some rounds the claims won, some rounds the delete did.
    ///
    /// What it does *not* prove is the retry. The 500 window here is the delete
    /// passing its `WHERE` and then committing while a claim is in flight, and
    /// this cascade is three statements long — measured at 0 conflicts in 100
    /// raced rounds, and the whole test stays green with
    /// [`crate::database::transaction_with_retry`]'s loop cut to a single
    /// attempt. Widening the sweep, staggering the burst and doubling it to 12
    /// racers all failed to open the window (they only move which side wins).
    /// So this is a status-code-and-cascade guard: a raced delete answers 409 or
    /// 404 and never 500, and a landed question survives it. The retry itself is
    /// measured on [`crate::db::course::delete`], whose cascade is
    /// long enough to lose a round (1-2 of 20, red under the same mutation).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs a real SurrealDB server: podman start hezarfen-surrealdb && cargo test -- --ignored"]
    async fn a_delete_racing_a_question_never_answers_500() {
        use crate::domain::exam_question::{
            QuestionKind, QuestionPoints, QuestionSpec, QuestionText,
        };
        let (db, _serialized) = crate::database::init_test_server("subject_delete_race").await;
        let (mut delete_500, mut question_500) = (0, 0);
        let (mut landed, mut wiped) = (0, 0);
        let (mut last_delete, mut last_question) = (String::new(), String::new());
        for round in 0..20 {
            let course = crate::db::course::a_test_course(&db).await;
            // A real exam row per round: a question write moves its exam's
            // counter, so a minted id nothing wrote is a 404 and no round would
            // reach the subject race this test is about.
            let exam = crate::db::exam::published_exam(&db).await.get_id().clone();
            let subject = create(
                &db,
                &course,
                SubjectName::try_new("Limits").unwrap(),
                SubjectDescription::try_new("").unwrap(),
            )
            .await
            .unwrap();

            let separated = round % 4 == 0;
            let drop_it = {
                let (subject, db) = (subject.clone(), db.clone());
                // One round in four holds the racers back by a clear 2ms so the
                // delete wins outright: the sub-millisecond sweep alone leaves
                // them ahead of it nearly every round (measured 20 to 0), and
                // both counters below have to see a side. The other three keep
                // the sub-ms beat, which is the only spacing that overlaps at
                // all — a whole millisecond either way separates them.
                let beat = if separated {
                    std::time::Duration::ZERO
                } else {
                    std::time::Duration::from_micros(round * 53 % 300)
                };
                tokio::spawn(async move {
                    tokio::time::sleep(beat).await;
                    delete(&db, subject).await
                })
            };
            let asks: Vec<_> = (0..6)
                .map(|_| {
                    let (id, db, exam) = (subject.get_id().clone(), db.clone(), exam.clone());
                    let head_start = if separated {
                        std::time::Duration::from_millis(2)
                    } else {
                        std::time::Duration::from_micros(round * 37 % 300)
                    };
                    tokio::spawn(async move {
                        tokio::time::sleep(head_start).await;
                        crate::db::exam_question::create(
                            &db,
                            &exam,
                            id,
                            QuestionText::try_new("why").unwrap(),
                            QuestionPoints::try_new(1).unwrap(),
                            QuestionSpec::try_new(
                                QuestionKind::try_new("text").unwrap(),
                                None,
                                None,
                                &[],
                            )
                            .unwrap(),
                        )
                        .await
                    })
                })
                .collect();
            let drop_it = drop_it.await.unwrap();
            if matches!(drop_it, Err(AppError::Db(_))) {
                delete_500 += 1;
                last_delete = format!("{drop_it:?}");
            }
            for ask in asks {
                let ask = ask.await.unwrap();
                if matches!(ask, Err(AppError::Db(_))) {
                    question_500 += 1;
                    last_question = format!("{ask:?}");
                }
            }
            // Stored state, and both sides are needed: a question landing means
            // the claim beat the guard, the subject being gone means the delete
            // did. Seeing only one is a run that never swept across the window.
            if read(&db, subject.get_id()).await.unwrap().is_none() {
                wiped += 1;
            }
            if !crate::db::exam_question::list_for_exam(&db, &exam, None, 0)
                .await
                .unwrap()
                .0
                .is_empty()
            {
                landed += 1;
            }
        }
        eprintln!(
            "Subject::delete raced: {delete_500}/20 delete 500s, {question_500} question 500s, \
             {landed} rounds with a question landed / {wiped} wiped"
        );
        assert!(
            landed > 0 && wiped > 0,
            "the sweep never crossed the window ({landed} landed / {wiped} wiped)"
        );
        assert_eq!(
            delete_500, 0,
            "a raced delete must be refused, not 500: {delete_500}/20 rounds, last {last_delete}"
        );
        assert_eq!(
            question_500, 0,
            "a raced question create must retry, not 500: {question_500}/20 rounds, last {last_question}"
        );
    }
}
