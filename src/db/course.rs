//! The `course` table: the school-level catalog exams, sessions, subjects and
//! enrollments hang off *through their instances* ([`crate::db::class_course`]).
//! Listed newest first; deleted with its whole subtree in one guarded cascade.
//!
//! The catalog is a template since the K12 remodel (D1): the academic work —
//! roster, exams, timetable, teachers — lives on the instances a section attaches,
//! so this row carries only what every instance shares (title, description,
//! kind) and two counters. The catalog is Manager+-owned (D6): there is no
//! per-teacher ownership list, and the two counters are the delete guard.

use crate::constant::COURSE_TABLE;
use crate::database::{Database, tx_with_retry, unique_violation};
use crate::db::page::PagedList;
use crate::domain::course::{Course, CourseDescription, CourseId, CourseKind, CourseTitle};
use crate::domain::user::UserId;
use crate::error::AppError;
use sqlx::PgConnection;

/// The stored columns of a `course` row — the whole row. The assigned-teacher
/// list this struct used to join on moved to the instances
/// ([`crate::db::class_course_teacher`]), so a course read is one statement
/// again.
#[derive(Debug, sqlx::FromRow)]
struct CourseRow {
    id: CourseId,
    creator: UserId,
    title: CourseTitle,
    description: CourseDescription,
    kind: CourseKind,
    class_course_count: i64,
    course_membership_count: i64,
}

impl CourseRow {
    fn into_course(self) -> Course {
        Course {
            id: self.id,
            creator: self.creator,
            title: self.title,
            description: self.description,
            kind: self.kind,
            class_course_count: self.class_course_count,
            course_membership_count: self.course_membership_count,
        }
    }
}

/// Mint a catalog course. No term is claimed any more: the catalog is not
/// bound to a term (exams are, through their instance), and no capacity is
/// stored — the roster counter lives on the instance and gates nothing.
pub async fn create(
    db: &Database,
    creator: &UserId,
    title: CourseTitle,
    description: CourseDescription,
    kind: CourseKind,
) -> Result<Course, AppError> {
    let id = CourseId::generate();
    let created = sqlx::query_as!(
        CourseRow,
        r#"INSERT INTO course (id, creator, title, description, kind)
           VALUES ($1, $2, $3, $4, $5)
           RETURNING id AS "id: CourseId", creator AS "creator: UserId",
                 title AS "title: CourseTitle",
                 description AS "description: CourseDescription",
                 kind AS "kind: CourseKind", class_course_count, course_membership_count"#,
        id.uuid(),
        creator.uuid(),
        title.as_str(),
        description.as_str(),
        kind.as_str(),
    )
    .fetch_one(db)
    .await;
    match created {
        Ok(created) => Ok(created.into_course()),
        // The creator is a foreign key: a gone account is the same `NotFound`
        // the web layer's own read answers with, never an orphan row.
        Err(err) if crate::database::foreign_key_violation(&err) => Err(AppError::NotFound),
        // Unreachable: the id was minted above.
        Err(err) if unique_violation(&err).is_some() => {
            Err(AppError::Internal("failed to create course".into()))
        }
        Err(err) => Err(err.into()),
    }
}

async fn read_row(db: &Database, id: &CourseId) -> Result<Option<CourseRow>, AppError> {
    sqlx::query_as!(
        CourseRow,
        r#"SELECT id AS "id: CourseId", creator AS "creator: UserId",
                  title AS "title: CourseTitle",
                  description AS "description: CourseDescription",
                  kind AS "kind: CourseKind", class_course_count, course_membership_count
           FROM course WHERE id = $1"#,
        id.uuid(),
    )
    .fetch_optional(db)
    .await
    .map_err(Into::into)
}

pub async fn read(db: &Database, id: &CourseId) -> Result<Option<Course>, AppError> {
    Ok(read_row(db, id).await?.map(CourseRow::into_course))
}

pub async fn list_all(db: &Database) -> Result<Vec<Course>, AppError> {
    let rows = sqlx::query_as!(
        CourseRow,
        r#"SELECT id AS "id: CourseId", creator AS "creator: UserId",
                  title AS "title: CourseTitle",
                  description AS "description: CourseDescription",
                  kind AS "kind: CourseKind", class_course_count, course_membership_count
           FROM course ORDER BY id DESC"#,
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(CourseRow::into_course).collect())
}

/// The courses `user` is enrolled in — read through the *instances* their
/// enrollments name, since an enrollment keys on the class×course instance.
/// The spine of `/courses/me` and the marks report.
pub async fn list_enrolled(
    db: &Database,
    user: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Course>, i64), AppError> {
    let (rows, total) = PagedList::new(
        "course WHERE id IN (
             SELECT cc.course FROM enrollment e
             JOIN class_course cc ON cc.id = e.class_course
             WHERE e.app_user = $1)",
        "ORDER BY id DESC",
    )
    .bind(user.uuid())
    .run::<CourseRow>(limit, offset, db)
    .await?;
    Ok((
        rows.into_iter().map(CourseRow::into_course).collect(),
        total,
    ))
}

/// The courses `user` teaches somewhere — the ones a manager assigned them to
/// on some instance. A teacher's slice of the catalog.
///
/// It reads `class_course_teacher` by its `teacher` index and follows each
/// assignment to the catalog course its instance teaches: a teacher who runs
/// 5-A's Matematik sees Matematik. The old creator-membership half is gone
/// with D6 — the catalog is Manager+ owned.
pub async fn list_for_teacher(db: &Database, user: &UserId) -> Result<Vec<Course>, AppError> {
    let rows = sqlx::query_as!(
        CourseRow,
        r#"SELECT id AS "id: CourseId", creator AS "creator: UserId",
                  title AS "title: CourseTitle",
                  description AS "description: CourseDescription",
                  kind AS "kind: CourseKind", class_course_count, course_membership_count
           FROM course WHERE id IN (
               SELECT cc.course FROM class_course_teacher t
               JOIN class_course cc ON cc.id = t.class_course
               WHERE t.teacher = $1)
           ORDER BY id DESC"#,
        user.uuid(),
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(CourseRow::into_course).collect())
}

/// Load every course behind `ids` (one query) — the join half of the
/// attendance and marks reports' per-course blocks.
pub async fn list_by_ids(db: &Database, ids: &[CourseId]) -> Result<Vec<Course>, AppError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let keys: Vec<uuid::Uuid> = ids.iter().map(CourseId::uuid).collect();
    let rows = sqlx::query_as!(
        CourseRow,
        r#"SELECT id AS "id: CourseId", creator AS "creator: UserId",
                  title AS "title: CourseTitle",
                  description AS "description: CourseDescription",
                  kind AS "kind: CourseKind", class_course_count, course_membership_count
           FROM course WHERE id = ANY($1)"#,
        &keys,
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(CourseRow::into_course).collect())
}

/// Field-scoped: a field the request omitted (`None`) is not written at all.
/// Sending the snapshot's value back instead would revert a concurrent edit of
/// that field; scoping the `SET` alone does not stop that, the values have to
/// come from the request.
///
/// The two counters and `creator` are not editable: the counters are what the
/// delete guard reads (a PATCH is not a roster change), and the creator is the
/// catalog's of record.
pub async fn update(
    db: &Database,
    course: Course,
    title: Option<CourseTitle>,
    description: Option<CourseDescription>,
    kind: Option<CourseKind>,
) -> Result<Course, AppError> {
    let row = crate::db::field_update::FieldUpdate::new(COURSE_TABLE, course.id.uuid())
        .set("title", title.map(|title| title.as_str().to_owned()))
        .set(
            "description",
            description.map(|description| description.as_str().to_owned()),
        )
        .set("kind", kind.map(|kind| kind.as_str().to_owned()))
        .run::<CourseRow>(db)
        .await?;
    Ok(row.into_course())
}

/// Delete the course and cascade-remove everything inside it *through its
/// instances*: results, attempts, answers, and question/answer images of
/// their exams, their homework with submissions, submission files, and
/// grades, their enrollments, their sessions with their roll call, their
/// teachers, and — last — the `class_course` rows themselves (each class
/// getting its attachment counter back, or it would be undeletable over rows
/// pointing at nothing). The children go in one transaction so a crash can't
/// leave an exam pointing at a deleted course. The image and homework-file
/// *blobs* are the web layer's to remove — it collects their names before
/// calling this.
///
/// The guard is the two counters on the locked row (D5/D6): an instance still
/// teaching this course refuses the delete, and so does an individual club
/// membership, because nothing here unlinks either. Detach the instances and
/// empty the memberships first — a catalog course whose last instance goes is
/// then deletable, which is the whole point of the catalog being a template.
///
/// **Bank templates** survive it — they are a separate, reusable library
/// spanning every course — so their `source_exam` and `subject` links are
/// cleared instead, in this same transaction, exactly as
/// [`crate::db::exam::delete`] and [`crate::db::subject::delete`]
/// clear them one level down. Deleting a course must leave the bank where
/// deleting each of its exams and subjects by hand would have left it, or
/// a template is left pointing at a dead exam (permanently: nothing else
/// ever visits that column) and at a dead subject the next `PATCH`
/// omitting `subject_id` writes straight back.
///
/// The **grade blueprints** naming it are swept in that same transaction —
/// their `blueprint_course` link rows are FK children now, deleted right
/// here like any other link, so a template stops naming a course the
/// instant that course goes (the old `teachers`-style array made the id a
/// permanent dangling value only the pump's [`crate::db::class_blueprint::
/// prune`] could chase). The same transaction takes the instances'
/// `class_course_teacher` assignment rows: an assigned teacher loses the
/// instance with it.
///
/// The **derived AI rows** (`rag_output` carries a denormalized `course`
/// besides its note link) and the event **audience links** are FK children
/// the old store let dangle: `rag_output` is disposable and dies here, and
/// an event whose audience was "this course's students" keeps standing with
/// `audience_course` cleared — an empty roster, exactly the documented
/// outcome of the class twin (`ClassGroup::delete` never touched `event`;
/// it still doesn't delete one).
///
/// The marks going with it give their exam kinds' references back, counted
/// per kind inside this same transaction — the mirror of `Exam::delete`.
/// Skipping it would leave every kind the course graded under counted
/// forever, and a counted kind can never leave the school's settings.
///
/// **Order is the foreign-key graph, children first**: exam results/attempts/
/// answers/audiences/images and question images before `exam_question`;
/// `exam_question` before `exam`; homework files before submissions before
/// results before `homework`; roll-call rows before `course_session`;
/// enrollment / teacher / audience rows before `class_course`; note files and
/// rag rows before `course_note`; `blueprint_course`, `course_membership` and
/// `class_course` before the `course` row itself, whose `NO ACTION` FKs are
/// checked the moment the final `DELETE` runs.
///
/// `false` = refused, nothing was written: a class still teaches it, or
/// someone still holds a membership. The guard locks the course row
/// (`FOR UPDATE`) and reads its own counters, so an attach or a join racing
/// this either claims first (and the delete is refused) or finds the row gone
/// (and is refused itself). `Err(NotFound)` keeps the answer a concurrent
/// *delete* used to get — and unlike the old one-statement guard, this one
/// tells the two apart without a second read.
pub async fn delete(db: &Database, course: Course) -> Result<bool, AppError> {
    tx_with_retry(db, true, async move |tx| {
        // The guard and the lock: the row (and its two counters) cannot
        // change under this transaction, and every attach or membership
        // claim contends on this very row, so check and cascade are one
        // decision.
        let guard = sqlx::query!(
            r#"SELECT class_course_count, course_membership_count FROM course
               WHERE id = $1 FOR UPDATE"#,
            course.id.uuid(),
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(guard) = guard else {
            return Err(AppError::NotFound);
        };
        if guard.class_course_count != 0 || guard.course_membership_count != 0 {
            return Ok(false);
        }
        // Events aimed at this course keep standing, audience cleared.
        sqlx::query!(
            r#"UPDATE event SET audience_course = NULL WHERE audience_course = $1"#,
            course.id.uuid(),
        )
        .execute(&mut *tx)
        .await?;
        // The instances and everything under them. The guard above refuses a
        // course that still carries one, so this sweep is defensive: a counter
        // is the guard's *input*, and a guard reading a stale number is
        // exactly the shape this exists to survive. The blob keys it returns
        // are already collected by the caller (`file_keys_for_course`), which
        // reads them before the rows go, so they are dropped here.
        let instances: Vec<uuid::Uuid> = sqlx::query_scalar!(
            r#"SELECT id AS "id: uuid::Uuid" FROM class_course WHERE course = $1"#,
            course.id.uuid(),
        )
        .fetch_all(&mut *tx)
        .await?;
        for instance in &instances {
            let _blobs = sweep_instance_subtree(&mut *tx, *instance).await?;
        }
        // Derived AI rows for this course's notes — links before the rows
        // (`ON DELETE NO ACTION`).
        sqlx::query!(
            r#"DELETE FROM rag_output_source WHERE output IN (
                 SELECT id FROM rag_output WHERE course = $1)"#,
            course.id.uuid(),
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            r#"DELETE FROM rag_output WHERE course = $1"#,
            course.id.uuid()
        )
        .execute(&mut *tx)
        .await?;
        // Notes: files before their notes.
        sqlx::query!(
            r#"DELETE FROM course_note_file WHERE course_note IN (
                 SELECT id FROM course_note WHERE course = $1)"#,
            course.id.uuid(),
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            r#"DELETE FROM course_note WHERE course = $1"#,
            course.id.uuid()
        )
        .execute(&mut *tx)
        .await?;
        // The instances, each class handed its attachment counter back in
        // the same statement.
        sqlx::query!(
            r#"WITH detached AS (
                 DELETE FROM class_course WHERE course = $1 RETURNING class)
               UPDATE class_group g
                  SET class_course_count = GREATEST(g.class_course_count - 1, 0)
                 FROM detached WHERE g.id = detached.class"#,
            course.id.uuid(),
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            r#"DELETE FROM course_membership WHERE course = $1"#,
            course.id.uuid(),
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            r#"DELETE FROM blueprint_course WHERE course = $1"#,
            course.id.uuid(),
        )
        .execute(&mut *tx)
        .await?;
        // The bank keeps its templates; their links to this course's
        // content go first, so the subject delete below cannot trip a
        // foreign key behind them.
        sqlx::query!(
            r#"UPDATE bank_question SET subject = NULL
               WHERE subject IN (SELECT id FROM subject WHERE course = $1)"#,
            course.id.uuid(),
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(r#"DELETE FROM subject WHERE course = $1"#, course.id.uuid())
            .execute(&mut *tx)
            .await?;
        // Last: the course itself, now FK-silent.
        sqlx::query!(r#"DELETE FROM course WHERE id = $1"#, course.id.uuid())
            .execute(&mut *tx)
            .await?;
        Ok(true)
    })
    .await
}

/// Everything hanging off one instance, deleted children-first with the
/// counters it holds refunded — the sweep both [`crate::db::class_pump::
/// detach_course`] and [`delete`] run, and the only place the instance's
/// foreign-key graph is spelled.
///
/// The order is the graph: exam results/attempts/answers/audiences and
/// question/answer images before `exam_question`; the question rows give their
/// subjects' `exam_question_count` back and the results their kinds'
/// `kind_ref` count; bank templates keep standing with `source_exam` cleared;
/// homework files before submissions (whose `grad_by_result` stamp points at a
/// result row) before results before assignments before `homework`; roll-call
/// rows before `course_session`; then the roster, the teacher links and the
/// audience rows, and the caller takes the instance row itself last.
///
/// Returns the blob keys of the image and homework-file rows it removed, so a
/// caller whose web route unlinks bytes can do it after the commit; a caller
/// that collected them beforehand ([`crate::service::course::delete`]) drops
/// them.
///
/// **Every reference counter it gives back is driven by the rows a
/// `DELETE … RETURNING` actually removed** — `exam`→`term.exam_count`,
/// `exam_result`→`kind_ref.count`, `exam_question`→`subject.exam_question_count`,
/// `homework`→`subject.homework_count` — never by a count taken off a
/// statement-start snapshot. The difference is a race: `Exam::delete` and
/// `remove_result` release some of these counters themselves, so a
/// count-then-subtract pair would hand one row's reference back twice (a
/// counter one low, permanently — `GREATEST` only floors at zero), and a kind
/// still carrying marks could then be retired from settings. Fused this way an
/// interleaving is exact from either side: the loser's `DELETE` simply does
/// not return the row the winner took.
///
/// The row locks it takes are the ones the children's own writers take (the
/// instance row `FOR UPDATE` is the caller's, before this runs), so a grade or
/// an upload racing the sweep either lands before it (and is swept) or finds
/// its parent gone and is refused.
pub(crate) async fn sweep_instance_subtree(
    tx: &mut PgConnection,
    instance: uuid::Uuid,
) -> Result<Vec<String>, AppError> {
    let exams: Vec<uuid::Uuid> = sqlx::query_scalar!(
        r#"SELECT id AS "id: uuid::Uuid" FROM exam WHERE class_course = $1"#,
        instance,
    )
    .fetch_all(&mut *tx)
    .await?;
    // Every release below is driven by the rows a `DELETE … RETURNING`
    // actually removed, in the shape [`crate::db::exam_result::remove`] uses,
    // and never by a count taken off a statement-start snapshot: a concurrent
    // `Exam::delete` or `remove_result` releases the same counters itself, so
    // a count-then-subtract pair would release its row twice — a `kind_ref`
    // one low permanently (GREATEST only floors at zero), which then lets a
    // kind still carrying marks be retired from settings.
    let image_files: Vec<String> = sqlx::query_scalar!(
        r#"SELECT file FROM question_image WHERE exam = ANY($1)"#,
        &exams,
    )
    .fetch_all(&mut *tx)
    .await?;
    let answer_image_files: Vec<String> = sqlx::query_scalar!(
        r#"SELECT file FROM answer_image WHERE exam = ANY($1)"#,
        &exams,
    )
    .fetch_all(&mut *tx)
    .await?;
    // Both directions of the audience table, and each has to go before the
    // row it names. The rows *addressed to* this instance (an exam owned by
    // another instance may be announced here — a shared exam) reference the
    // `class_course` row the caller deletes right after this sweep and would
    // otherwise refuse it with a `23503`. The rows *of* the exams this sweep
    // deletes reference *those* exams the same way (`exam_audience.exam` is
    // `NO ACTION` too), so they block the `DELETE FROM exam` below wherever
    // they point — an audience of a swept exam in a section that is *not* being
    // detached included.
    //
    // The instance row is held `FOR UPDATE` by the caller from before this
    // sweep, and an insert into `exam_audience` takes that row's
    // `FOR KEY SHARE` through its foreign key, so no audience *addressed to*
    // this instance can land under the sweep: the `add` blocks behind the
    // detach and, on the settled state, finds the instance gone (`404`). An
    // audience of one of the swept *exams* rides that exam's own key-share
    // lock instead; if it commits between this delete and the exam delete
    // below, the `exam_audience_exam_fkey` refusal that follows is what the
    // detach's `cascade` retry re-runs on — the second round sees the
    // settled row and takes it.
    sqlx::query!(
        r#"DELETE FROM exam_audience WHERE class_course = $1 OR exam = ANY($2)"#,
        instance,
        &exams,
    )
    .execute(&mut *tx)
    .await?;
    // Every mark this sweep takes gives its exam kind's reference back, per
    // kind, off the rows this statement removed. The kind rides the exam row —
    // `exam_result` names its exam, not its kind — so the deleted rows are
    // joined back to the exam they named *before* it goes (the exam row is
    // still there: this runs ahead of the `DELETE FROM exam` at the end).
    sqlx::query!(
        r#"WITH gone AS (
               DELETE FROM exam_result WHERE exam = ANY($1)
               RETURNING exam),
           kinds AS (
               SELECT e.kind AS kind, count(*) AS n
               FROM gone g JOIN exam e ON e.id = g.exam
               GROUP BY e.kind)
           UPDATE kind_ref k SET count = GREATEST(k.count - c.n, 0)
           FROM kinds c WHERE k.name = c.kind"#,
        &exams,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(r#"DELETE FROM exam_attempt WHERE exam = ANY($1)"#, &exams)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(r#"DELETE FROM exam_answer WHERE exam = ANY($1)"#, &exams)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(r#"DELETE FROM answer_image WHERE exam = ANY($1)"#, &exams)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(r#"DELETE FROM question_image WHERE exam = ANY($1)"#, &exams)
        .execute(&mut *tx)
        .await?;
    // The cascaded questions give their subject references back off the rows
    // this statement removed, per subject — released with the delete, never
    // counted ahead of it.
    sqlx::query!(
        r#"WITH gone AS (
               DELETE FROM exam_question WHERE exam = ANY($1)
               RETURNING subject),
           subs AS (
               SELECT subject, count(*) AS n FROM gone GROUP BY subject)
           UPDATE subject s SET exam_question_count = GREATEST(s.exam_question_count - c.n, 0)
           FROM subs c WHERE s.id = c.subject"#,
        &exams,
    )
    .execute(&mut *tx)
    .await?;
    // Bank templates survive the exam they were saved from — only the
    // provenance link is cleared.
    sqlx::query!(
        r#"UPDATE bank_question SET source_exam = NULL WHERE source_exam = ANY($1)"#,
        &exams,
    )
    .execute(&mut *tx)
    .await?;
    // The swept exams give their term's reference back off the rows this
    // statement removed, per term (the mirror of `Exam::delete`'s
    // single-exam release): `db::exam::create` claimed `term.exam_count` for
    // each of them, and a claim nothing releases leaves the term's
    // `exam_count = 0` delete guard permanently failing — which takes the
    // term, and the academic year whose own guard counts its terms, down
    // with it. Fused with the delete for the same reason the two releases
    // above are: a concurrent single-exam delete releases its own claim, so a
    // count taken before this statement could give one back twice.
    sqlx::query!(
        r#"WITH gone AS (
               DELETE FROM exam WHERE class_course = $1
               RETURNING term),
           terms AS (
               SELECT term, count(*) AS n FROM gone GROUP BY term)
           UPDATE term t SET exam_count = GREATEST(t.exam_count - c.n, 0)
           FROM terms c WHERE t.id = c.term"#,
        instance,
    )
    .execute(&mut *tx)
    .await?;
    let homework_files: Vec<String> = sqlx::query_scalar!(
        r#"SELECT file FROM homework_file
           WHERE submission IN (SELECT id FROM homework_submission WHERE homework IN (
               SELECT id FROM homework WHERE class_course = $1))"#,
        instance,
    )
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query!(
        r#"DELETE FROM homework_file
           WHERE submission IN (SELECT id FROM homework_submission WHERE homework IN (
               SELECT id FROM homework WHERE class_course = $1))"#,
        instance,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        r#"DELETE FROM homework_submission WHERE homework IN (
             SELECT id FROM homework WHERE class_course = $1)"#,
        instance,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        r#"DELETE FROM homework_result WHERE homework IN (
             SELECT id FROM homework WHERE class_course = $1)"#,
        instance,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        r#"DELETE FROM homework_assignment WHERE homework IN (
             SELECT id FROM homework WHERE class_course = $1)"#,
        instance,
    )
    .execute(&mut *tx)
    .await?;
    // The homework rows give their subjects' counters back off the rows this
    // statement removed, per subject — the same fused shape as the two above.
    sqlx::query!(
        r#"WITH gone AS (
               DELETE FROM homework WHERE class_course = $1
               RETURNING subject),
           subs AS (
               SELECT subject, count(*) AS n FROM gone GROUP BY subject)
           UPDATE subject s SET homework_count = GREATEST(s.homework_count - c.n, 0)
           FROM subs c WHERE s.id = c.subject"#,
        instance,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        r#"DELETE FROM session_attendance WHERE class_course = $1"#,
        instance
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        r#"DELETE FROM course_session WHERE class_course = $1"#,
        instance
    )
    .execute(&mut *tx)
    .await?;
    // The roster: every enrollment row of the instance, hand-placed included —
    // the instance is the row's own parent key, so none can outlive it.
    sqlx::query!(
        r#"DELETE FROM enrollment WHERE class_course = $1"#,
        instance
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        r#"DELETE FROM class_course_teacher WHERE class_course = $1"#,
        instance
    )
    .execute(&mut *tx)
    .await?;
    let mut blobs = image_files;
    blobs.extend(answer_image_files);
    blobs.extend(homework_files);
    Ok(blobs)
}

/// A real course row, for the tests of every child that must now prove its
/// parent exists. A minted id nothing wrote is a 404 there, the way a minted
/// subject id already is for an exam question.
#[cfg(test)]
pub(crate) async fn a_test_course(db: &Database) -> CourseId {
    // The creator is a foreign key now: a real row, minted per call so
    // repeated calls are new people, not the same one.
    let creator = UserId::generate();
    sqlx::query(
        "INSERT INTO app_user (id, username, created_at, role) \
         VALUES ($1, $2, 0, 'teacher')",
    )
    .bind(creator.uuid())
    .bind(format!("course-fixture-{}", &creator.key()[30..]))
    .execute(db)
    .await
    .unwrap();
    create(
        db,
        &creator,
        CourseTitle::try_new("test course").unwrap(),
        CourseDescription::try_new("").unwrap(),
        CourseKind::course(),
    )
    .await
    .unwrap()
    .id
}

/// A real *instance* of that course, attached to a fresh class — the parent
/// every re-keyed child (exam, session, homework, enrollment) now hangs off,
/// for the tests of those children.
///
/// Returned as a pair because a child write that names the instance usually
/// also wants the catalog course (a subject's `course`, a marks read's
/// course grouping); both are real rows, minted per call.
#[cfg(test)]
pub(crate) async fn a_test_instance(
    db: &Database,
) -> (crate::domain::class_course::ClassCourseId, CourseId) {
    use crate::domain::class_group::ClassName;

    let manager = UserId::generate();
    sqlx::query(
        "INSERT INTO app_user (id, username, created_at, role) \
         VALUES ($1, $2, 0, 'manager')",
    )
    .bind(manager.uuid())
    .bind(format!("instance-fixture-{}", &manager.key()[30..]))
    .execute(db)
    .await
    .unwrap();
    let class = crate::db::class_group::create(
        db,
        &manager,
        ClassName::try_new("9-A").unwrap(),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let course = a_test_course(db).await;
    let attached = crate::service::class_course::attach(db, class.get_id(), &course, &manager)
        .await
        .unwrap();
    (attached.get_id().clone(), course)
}

/// The race half of "a child of a course must not survive its deletion", run
/// for one child table: [`delete`] is held open by the schema and
/// `make` fires its create inside that window, eight rounds.
///
/// The harness lives here, beside the cascade being raced, and each child names
/// its own pin in its own module (`exam`, `course_session`, `subject`) — the
/// loop, the window and the verdict are one fact about *this* delete, and three
/// copies of it would drift into three different tests of three different
/// things. The sequential half — a create against a course that is already gone
/// — is `tests/regress_course_child_orphans.rs`, which needs no server.
///
/// The orphan query is the caller's, because the child's key to the course is
/// not: `subject` still names the course directly, while an exam or a session
/// hangs off an *instance* ([`crate::db::class_course`]) and reaches the course
/// through it. Either way it takes the course uuid as `$1`.
///
/// The window is opened by the database rather than by a lucky interleaving:
/// Postgres puts the racing create and delete on the same rows (the FK claims
/// and the cascade), so a barrier start covers every interleaving — and the
/// child may never outlive the course in any of them.
///
/// One child per round, for the reason the menu and `class_course` twins
/// document: a second writer makes the delete lose and re-send, and the re-sent
/// sweep clears the evidence.
///
/// Real server, and every caller is `#[ignore]`d for it: the subject *is* the
/// store's conflict detection, which `init_mem`'s embedded engine does not
/// have — it commits both sides and answers `Ok` to each, so this passes there
/// on broken code.
#[cfg(test)]
pub(crate) async fn assert_no_child_outlives_a_course_delete(
    table: &str,
    orphan_count_sql: &str,
    make: fn(CourseId, Database) -> tokio::task::JoinHandle<Result<(), AppError>>,
) {
    use sqlx::Row as _;

    let (db, _leases) = crate::database::init_test_db().await;

    let (mut swept, mut orphans) = (0, 0);
    for round in 0..8 {
        let course = a_test_course(&db).await;
        // The old engine needed a schema event to hold the delete's window
        // open; Postgres puts the racing create and delete on the same rows
        // (the FK claims and the cascade), so a barrier start covers every
        // interleaving — and the child may never outlive the course in any
        // of them.
        let gate = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let drop_it = {
            let (course, db, gate) = (course.clone(), db.clone(), gate.clone());
            tokio::spawn(async move {
                gate.wait().await;
                delete(
                    &db,
                    read(&db, &course)
                        .await
                        .unwrap()
                        .expect("the course is there"),
                )
                .await
            })
        };
        let child = {
            let (course, db, gate) = (course.clone(), db.clone(), gate);
            tokio::spawn(async move {
                gate.wait().await;
                // The inner task joins here: a `Db` answer stays `Err`, only a
                // panic inside `make` (or the outer join) would panic.
                make(course, db).await.unwrap()
            })
        }
        .await
        .unwrap();
        let dropped = drop_it.await.unwrap();

        // A 404 for the create, or a refusal for the delete, is a correct
        // answer — the only defect is stored state. Nothing may 500.
        assert!(
            !matches!(child, Err(AppError::Db(_))),
            "{table} round {round}: a raced create must be answered, not 500: {child:?}"
        );
        assert!(
            !matches!(dropped, Err(AppError::Db(_))),
            "{table} round {round}: a raced delete must be answered, not 500: {dropped:?}"
        );

        // Stored state is the whole verdict; a return value is not evidence.
        if read(&db, &course).await.unwrap().is_none() {
            swept += 1;
            orphans += sqlx::query(sqlx::AssertSqlSafe(orphan_count_sql.to_string()))
                .bind(course.uuid())
                .fetch_one(&db)
                .await
                .unwrap()
                .try_get::<i64, _>(0)
                .unwrap() as usize;
        } else if matches!(dropped, Ok(true)) {
            panic!("{table} round {round}: the delete reported success, the course is still there");
        }
    }
    eprintln!("Course::delete raced by a {table} create: {swept}/8 rounds deleted the course");
    assert!(
        swept > 0,
        "{table}: no round ever deleted the course, so the race never actually ran"
    );
    assert_eq!(orphans, 0, "{table}: a child outlived its course");
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row as _;

    /// A real `app_user` row: creators, attached-bys and managers are foreign
    /// keys now. The label names the row's username; the id is minted, so
    /// repeated calls are new people, not the same row.
    async fn a_person(db: &Database, label: &str, role: &str) -> UserId {
        let user = UserId::generate();
        sqlx::query(
            "INSERT INTO app_user (id, username, created_at, role) \
             VALUES ($1, $2, 0, $3)",
        )
        .bind(user.uuid())
        .bind(format!("{label}-{}", &user.key()[30..]))
        .bind(role)
        .execute(db)
        .await
        .unwrap();
        user
    }

    /// A catalog course, minted per call. The catalog is a school-wide
    /// template since the remodel: no term to link, no capacity to store.
    async fn a_course(db: &Database) -> Course {
        create(
            db,
            &a_person(db, "teacher", "teacher").await,
            CourseTitle::try_new("algebra").unwrap(),
            CourseDescription::try_new("").unwrap(),
            CourseKind::course(),
        )
        .await
        .unwrap()
    }

    /// A section, for the tests that need an instance attached to it.
    async fn a_class(
        on: &UserId,
        name: &str,
        db: &Database,
    ) -> crate::domain::class_group::ClassGroup {
        crate::db::class_group::create(
            db,
            on,
            crate::domain::class_group::ClassName::try_new(name).unwrap(),
            None,
            None,
            None,
        )
        .await
        .unwrap()
    }

    /// The instances a course is attached through, re-read from storage —
    /// never off a return value.
    async fn instance_rows(course: &CourseId, db: &Database) -> i64 {
        sqlx::query("SELECT count(*) FROM class_course WHERE course = $1")
            .bind(course.uuid())
            .fetch_one(db)
            .await
            .unwrap()
            .try_get::<i64, _>(0)
            .unwrap()
    }

    /// How many rows a table holds (the table name is a fixture literal).
    async fn rows(table: &str, db: &Database) -> usize {
        sqlx::query(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
            .fetch_one(db)
            .await
            .unwrap()
            .try_get::<i64, _>(0)
            .unwrap() as usize
    }

    /// The bite test for the delete guard that replaced `ENROLL_LOCK`: the
    /// guard is the two counters on the locked course row, so the anchor's
    /// refusal is a live instance — and it must refuse having written
    /// nothing, the instance least of all. A `>= 0` guard passes the guard
    /// branch and fails here by deleting the course out from under its class.
    #[tokio::test]
    async fn a_course_an_instance_still_teaches_refuses_to_delete() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = a_person(&db, "manager", "manager").await;
        let course = a_course(&db).await;
        let class = a_class(&manager, "9-A", &db).await;
        let instance =
            crate::service::class_course::attach(&db, class.get_id(), course.get_id(), &manager)
                .await
                .unwrap();

        assert!(
            !delete(&db, course.clone()).await.unwrap(),
            "a course a class still teaches must refuse the delete"
        );
        assert!(
            read(&db, course.get_id()).await.unwrap().is_some(),
            "a refused delete may write nothing"
        );
        assert!(
            crate::db::class_course::read(&db, instance.get_id())
                .await
                .unwrap()
                .is_some(),
            "…the instance least of all"
        );

        // Detaching the last instance frees the row: the catalog is a
        // template, and the template whose last class let it go is deletable.
        crate::service::class_course::detach(&db, class.get_id(), course.get_id())
            .await
            .unwrap();
        assert!(delete(&db, course.clone()).await.unwrap());
        assert!(read(&db, course.get_id()).await.unwrap().is_none());

        // …and the counter the guard read is back to zero with it, not left
        // stranded by the cascade.
        assert_eq!(
            instance_rows(course.get_id(), &db).await,
            0,
            "the instance rows must go with the course"
        );
        assert_eq!(
            rows("course", &db).await,
            0,
            "no row may outlive the delete"
        );
    }

    /// MEASUREMENT — the one race test for `Course::delete`, and the only
    /// test that measures `tx_with_retry` on this path at all.
    ///
    /// [`delete`] is one `BEGIN…COMMIT` whose guard locks the course row the
    /// very counter an attach claims, so the two contend by design: the
    /// attach is several statements long (the early verdicts, the counter
    /// claim, the roster pump) and the delete's cascade is longer, so a
    /// rival's write lands mid-transaction. Losing that round writes nothing,
    /// which is what makes re-sending it the recovery. Without
    /// [`crate::database::transaction_with_retry`] a lost round comes out as
    /// a 500: mutation-tested by cutting that retry loop to one attempt, which
    /// turns this test red at 1-2 of 20 rounds (3 runs in 5 — the window is
    /// real but narrow, so a single green run under the mutation means
    /// nothing).
    ///
    /// A refusal (`Ok(false)`) or an `Err(NotFound)` is *correct* here and
    /// must not fail this test: the only defect is `AppError::Db`. The stored
    /// half of the verdict is the orphan check — an instance may never name a
    /// course that is gone, whichever side of the race it was on.
    ///
    /// The rate is counted over the whole loop instead of asserted per round,
    /// because a per-round `assert!` aborts at the first hit and would report
    /// "1 of 1" for a bug the point of this test is to *quantify*.
    ///
    /// Each side gets a clear head start on every other round — 2ms is a
    /// separation the several-statement rival cannot cross (the same spacing
    /// [`crate::db::term`]'s race test settled on). Without it the two sides
    /// compete for the same course row and whichever one is marginally faster
    /// takes every round, leaving one half of the verdict (`swept > 0`, or
    /// `attached > 0`) a coin toss under load.
    ///
    /// Multi-threaded and on a real server: the current-thread runtime never
    /// interleaves the two, and an embedded engine does not conflict-check
    /// concurrent writes at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_delete_racing_an_attach_never_answers_500() {
        let (db, _leases) = crate::database::init_test_db().await;
        let manager = a_person(&db, "mgr", "manager").await;
        // One section per racer; each round attaches them all into one fresh
        // catalog course, so the round's delete has six rival claims to meet.
        let mut classes = Vec::new();
        for seat in 0..6 {
            classes.push(a_class(&manager, &format!("9-{seat}"), &db).await);
        }
        let (mut delete_500, mut attach_500, mut attached, mut swept) = (0, 0, 0, 0);
        let (mut last_delete, mut last_attach) = (String::new(), String::new());
        for round in 0..20 {
            let course = a_course(&db).await;

            // Every other round the attaches are held back so the delete wins
            // outright (the course goes, and nothing may survive it); the rest
            // release the attaches first, which is the round where the guard
            // has to read their claim and refuse the delete.
            let clear = std::time::Duration::from_millis(2);
            let (delete_beat, attach_beat) = if round % 2 == 0 {
                (std::time::Duration::ZERO, clear)
            } else {
                (clear, std::time::Duration::ZERO)
            };
            let drop_it = {
                let (course, db) = (course.clone(), db.clone());
                tokio::spawn(async move {
                    tokio::time::sleep(delete_beat).await;
                    delete(&db, course).await
                })
            };
            let joins: Vec<_> = classes
                .iter()
                .map(|class| {
                    let (class, course, db, mgr) = (
                        class.get_id().clone(),
                        course.get_id().clone(),
                        db.clone(),
                        manager,
                    );
                    tokio::spawn(async move {
                        tokio::time::sleep(attach_beat).await;
                        crate::service::class_course::attach(&db, &class, &course, &mgr).await
                    })
                })
                .collect();
            let drop_it = drop_it.await.unwrap();
            if matches!(drop_it, Err(AppError::Db(_))) {
                delete_500 += 1;
                last_delete = format!("{drop_it:?}");
            }
            for join in joins {
                let join = join.await.unwrap();
                if matches!(join, Err(AppError::Db(_))) {
                    attach_500 += 1;
                    last_attach = format!("{join:?}");
                }
            }
            // Stored state, not the return values: an instance that landed is
            // what the guard had to see, and one that outlived a deleted
            // course is the defect this whole shape exists to catch.
            let live = instance_rows(course.get_id(), &db).await;
            if live > 0 {
                attached += 1;
            }
            if read(&db, course.get_id()).await.unwrap().is_none() {
                swept += 1;
                assert_eq!(
                    live, 0,
                    "round {round}: an instance outlived the course it names"
                );
            }
        }
        eprintln!(
            "Course::delete raced: {delete_500}/20 delete 500s, {attach_500} attach 500s, \
             {attached} rounds with an instance attached / {swept} rounds the course went"
        );
        assert!(
            attached > 0 && swept > 0,
            "the sweep never crossed the window ({attached} attached / {swept} swept), \
             so at least one verdict was never exercised"
        );
        assert_eq!(
            delete_500, 0,
            "a raced delete must be refused, not 500: {delete_500}/20 rounds, last {last_delete}"
        );
        assert_eq!(
            attach_500, 0,
            "a raced attach must be answered, not 500: {attach_500}/20 rounds, last {last_attach}"
        );
    }
}
