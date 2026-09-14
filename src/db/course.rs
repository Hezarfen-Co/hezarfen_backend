//! The `course` table: the unit exams, sessions, subjects and enrollments
//! hang off. Listed newest first; deleted with its whole subtree in one
//! guarded cascade.

use crate::constant::COURSE_TABLE;
use crate::database::{Database, tx_with_retry, unique_violation};
use crate::db::cap;
use crate::db::field_update::{FieldUpdate, Refcount};
use crate::db::page::PagedList;
use crate::domain::course::{Course, CourseDescription, CourseId, CourseKind, CourseTitle};
use crate::domain::term::{self, TermId};
use crate::domain::user::UserId;
use crate::error::AppError;

pub async fn create(
    db: &Database,
    creator: &UserId,
    title: CourseTitle,
    description: CourseDescription,
    kind: CourseKind,
    term: Option<TermId>,
    capacity: Option<i64>,
) -> Result<Course, AppError> {
    let id = CourseId::generate();
    let Some(term) = term else {
        let created = sqlx::query_as!(
            Course,
            r#"INSERT INTO course (id, creator, teachers, title, description, kind, term, capacity)
               VALUES ($1, $2, $3, $4, $5, $6, NULL, $7)
               RETURNING id AS "id: CourseId", creator AS "creator: UserId",
                     teachers AS "teachers: Vec<UserId>", title AS "title: CourseTitle",
                     description AS "description: CourseDescription",
                     kind AS "kind: CourseKind", term AS "term: TermId", capacity"#,
            id.uuid(),
            creator.uuid(),
            &Vec::<uuid::Uuid>::new(),
            title.as_str(),
            description.as_str(),
            kind.as_str(),
            capacity,
        )
        .fetch_one(db)
        .await?;
        return Ok(created);
    };
    // Claim a reference on the term in the *same statement* as the row: the
    // claim is a conditional write on the term row, so it fails when the
    // term is already gone and it makes the term undeletable the instant
    // this link exists — and a crash can never leave one without the other,
    // which a claim sent as its own query could (the count would strand and
    // the term be undeletable forever).
    let created = sqlx::query_as!(
        Course,
        r#"WITH seat AS (
             UPDATE term
                SET course_count = course_count + 1
              WHERE id = $8 AND course_count < $9
              RETURNING 1)
           INSERT INTO course (id, creator, teachers, title, description, kind, term, capacity)
           SELECT $1, $2, $3, $4, $5, $6, $8, $7 WHERE EXISTS (SELECT 1 FROM seat)
           RETURNING id AS "id: CourseId", creator AS "creator: UserId",
                     teachers AS "teachers: Vec<UserId>", title AS "title: CourseTitle",
                     description AS "description: CourseDescription",
                     kind AS "kind: CourseKind", term AS "term: TermId", capacity"#,
        id.uuid(),
        creator.uuid(),
        &Vec::<uuid::Uuid>::new(),
        title.as_str(),
        description.as_str(),
        kind.as_str(),
        capacity,
        term.uuid(),
        cap::UNLIMITED,
    )
    .fetch_one(db)
    .await;
    match created {
        Ok(created) => Ok(created),
        // Uncapped, so "full" can only mean the conditional write matched no
        // term row at all — the existence check the pre-flight lookup makes.
        Err(sqlx::Error::RowNotFound) => Err(term::gone_error()),
        // Unreachable: the id was minted above.
        Err(err) if unique_violation(&err).is_some() => {
            Err(AppError::Internal("failed to create course".into()))
        }
        Err(err) => Err(err.into()),
    }
}

pub async fn read(db: &Database, id: &CourseId) -> Result<Option<Course>, AppError> {
    let course = sqlx::query_as!(
        Course,
        r#"SELECT id AS "id: CourseId", creator AS "creator: UserId",
                  teachers AS "teachers: Vec<UserId>", title AS "title: CourseTitle",
                  description AS "description: CourseDescription",
                  kind AS "kind: CourseKind", term AS "term: TermId", capacity
           FROM course WHERE id = $1"#,
        id.uuid(),
    )
    .fetch_optional(db)
    .await?;
    Ok(course)
}

pub async fn list_all(db: &Database) -> Result<Vec<Course>, AppError> {
    let rows = sqlx::query_as!(
        Course,
        r#"SELECT id AS "id: CourseId", creator AS "creator: UserId",
                  teachers AS "teachers: Vec<UserId>", title AS "title: CourseTitle",
                  description AS "description: CourseDescription",
                  kind AS "kind: CourseKind", term AS "term: TermId", capacity
           FROM course ORDER BY id DESC"#,
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// The courses `user` is enrolled in — the spine of `/courses/me` and the
/// marks report.
pub async fn list_enrolled(
    db: &Database,
    user: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Course>, i64), AppError> {
    PagedList::new(
        "course WHERE id IN (SELECT course FROM enrollment WHERE app_user = $1)",
        "ORDER BY id DESC",
    )
    .bind(user.uuid())
    .run::<Course>(limit, offset, db)
    .await
}

/// The courses `user` runs — the ones they created plus the ones a manager
/// assigned them to. A teacher's slice of the catalog.
///
/// corner-cut: unpaged full table scan, and it stays one — every profile
/// read of a teacher pays it, so the ceiling is the course table's size.
/// Under Postgres the `OR` halves could each use an index on `creator` /
/// `teachers` (a GIN), but the old per-element index the membership half
/// would have needed was *wrong*, not merely useless — `$usr IN teachers`
/// returned **no rows at all** with it, which is what the integration test
/// `assigned_teacher_manages_course_without_owning_it` catches. The upgrade
/// path is structural: a `course_teacher` link table indexed on `user`, the
/// shape `enrollment` already has, turning this into two index-backed
/// reads. No schema carries it yet, so the scan stays.
pub async fn list_for_teacher(db: &Database, user: &UserId) -> Result<Vec<Course>, AppError> {
    let rows = sqlx::query_as!(
        Course,
        r#"SELECT id AS "id: CourseId", creator AS "creator: UserId",
                  teachers AS "teachers: Vec<UserId>", title AS "title: CourseTitle",
                  description AS "description: CourseDescription",
                  kind AS "kind: CourseKind", term AS "term: TermId", capacity
           FROM course WHERE creator = $1 OR $2 = ANY(teachers) ORDER BY id DESC"#,
        user.uuid(),
        user.uuid(),
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// Load every course behind `ids` (one query) — the join half of the
/// attendance report's per-course blocks.
pub async fn list_by_ids(db: &Database, ids: &[CourseId]) -> Result<Vec<Course>, AppError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let keys: Vec<uuid::Uuid> = ids.iter().map(CourseId::uuid).collect();
    let rows = sqlx::query_as!(
        Course,
        r#"SELECT id AS "id: CourseId", creator AS "creator: UserId",
                  teachers AS "teachers: Vec<UserId>", title AS "title: CourseTitle",
                  description AS "description: CourseDescription",
                  kind AS "kind: CourseKind", term AS "term: TermId", capacity
           FROM course WHERE id = ANY($1)"#,
        &keys,
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// Request-scoped: `teachers` is written by [`assign_teacher`],
/// [`unassign_teacher`] and the demotion sweep, and nothing guards
/// the course row across the handler's read and this write (its `TERM_LOCK`
/// window guards the *term* it links, and the assign path takes no lock at
/// all) — so a field the request omitted (`None`) is not written at all.
/// Sending the snapshot's value back instead would revert a concurrent
/// edit of that field; scoping the `SET` alone does not stop that, the
/// values have to come from the request. `term` and `capacity` are
/// nullable, so they take the outer/inner `Option<Option<_>>`: `None` =
/// omitted (keep), `Some(None)` = clear.
pub async fn update(
    db: &Database,
    course: Course,
    title: Option<CourseTitle>,
    description: Option<CourseDescription>,
    kind: Option<CourseKind>,
    term: Option<Option<TermId>>,
    capacity: Option<Option<i64>>,
) -> Result<Course, AppError> {
    // A term move claims the new term and releases the old one inside the
    // very transaction that moves the link, so no crash can leave a count
    // without its link (the term would be undeletable forever) or a link
    // without its count. A PATCH that carried no `term_id`, or re-stated the
    // link it already had, moves neither counter.
    let (claim, release) = term::ref_move(course.term.as_ref(), &term);
    FieldUpdate::new(COURSE_TABLE, course.id.uuid())
        .set("title", title.map(|title| title.as_str().to_owned()))
        .set(
            "description",
            description.map(|description| description.as_str().to_owned()),
        )
        .set("kind", kind.map(|kind| kind.as_str().to_owned()))
        .set(
            "term",
            term.map(|term| crate::db::page::Param::OptUuid(term.map(|term| term.uuid()))),
        )
        .set("capacity", capacity.map(crate::db::page::Param::OptI64))
        .refcount(Refcount {
            counter_table: "term",
            counter_field: "course_count",
            link: "term",
            expected: course.term.as_ref().map(|term| term.uuid()),
            claim: claim.map(|term| term.uuid()),
            release: release.map(|term| term.uuid()),
            refused: term::gone_error(),
        })
        .run::<Course>(db)
        .await
}

/// Assign `teacher` to run this course, or return the course untouched if
/// they already run it — assignment is idempotent, like enrollment.
/// Field-scoped, and the new list is folded server-side out of the *stored*
/// one: a course PATCH awaits a term lookup between its read and its write,
/// so a whole-row save from either side would revert the other. The
/// `array_agg(DISTINCT …)` keeps the assignment idempotent even when two
/// requests name the same teacher at once (the early return only sees a
/// stale row).
pub async fn assign_teacher(
    db: &Database,
    course: Course,
    teacher: &UserId,
) -> Result<Course, AppError> {
    if course.is_assigned(teacher) {
        return Ok(course);
    }
    let updated = sqlx::query_as!(
        Course,
        r#"UPDATE course
             SET teachers = (SELECT array_agg(DISTINCT x)
                               FROM unnest(course.teachers || $2::uuid) AS x)
           WHERE id = $1
           RETURNING id AS "id: CourseId", creator AS "creator: UserId",
                     teachers AS "teachers: Vec<UserId>", title AS "title: CourseTitle",
                     description AS "description: CourseDescription",
                     kind AS "kind: CourseKind", term AS "term: TermId", capacity"#,
        course.id.uuid(),
        teacher.uuid(),
    )
    .fetch_optional(db)
    .await?;
    updated.ok_or(AppError::NotFound)
}

/// Drop `teacher` from this course. `None` when they weren't assigned, so
/// the web layer can answer 404 instead of pretending it removed someone.
pub async fn unassign_teacher(
    db: &Database,
    course: Course,
    teacher: &UserId,
) -> Result<Option<Course>, AppError> {
    if !course.is_assigned(teacher) {
        return Ok(None);
    }
    // Same field-scoped story as [`assign_teacher`]; `array_remove` drops
    // the one link off the stored list without touching the course's own
    // text.
    let updated = sqlx::query_as!(
        Course,
        r#"UPDATE course SET teachers = array_remove(teachers, $2)
           WHERE id = $1
           RETURNING id AS "id: CourseId", creator AS "creator: UserId",
                     teachers AS "teachers: Vec<UserId>", title AS "title: CourseTitle",
                     description AS "description: CourseDescription",
                     kind AS "kind: CourseKind", term AS "term: TermId", capacity"#,
        course.id.uuid(),
        teacher.uuid(),
    )
    .fetch_optional(db)
    .await?;
    Ok(updated)
}

/// Strip `user` from every course they were assigned to — the sweep for a
/// user demoted below `teacher`, who may no longer run anything.
pub async fn unassign_everywhere(db: &Database, user: &UserId) -> Result<(), AppError> {
    sqlx::query!(
        r#"UPDATE course SET teachers = array_remove(teachers, $1)
           WHERE $1 = ANY(teachers)"#,
        user.uuid(),
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Delete the course and cascade-remove everything inside it: results,
/// attempts, answers, and question/answer images of its exams, its
/// homework with their submissions, submission files, and grades, its
/// enrollments, the class attachments that pumped some of them (each class
/// gets its count back, or it would be undeletable over rows pointing at
/// nothing), its sessions with their roll call, its subjects, and the
/// exams themselves. The children go in one transaction so a crash can't
/// leave an exam pointing at a deleted course. The image and
/// homework-file *blobs* are the web layer's to remove — it collects
/// their names before calling this.
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
/// The **grade blueprints** naming it are swept in that same transaction.
/// Nothing else can reach them — a blueprint holds its courses as a list on
/// its own row, not as link rows this cascade could delete — and an id left
/// behind is permanent rather than merely stale: the pump's own
/// [`crate::db::class_blueprint::prune`] fires only while walking a
/// section, so a grade with no sections can never drop one, and every
/// `PATCH` of that template is refused for naming a course that does not
/// exist, which is precisely the call documented as the repair. A dozen-row
/// table scanned unindexed, deliberately: a course delete is rare.
///
/// The **derived AI rows** (`rag_output` carries a denormalized `course`
/// besides its note link) and the event **audience links** are FK children
/// the old store let dangle: `rag_output` is disposable and dies here, and
/// an event whose audience was "this course's students" keeps standing with
/// `audience_course` cleared — an empty roster, exactly the documented
/// outcome of the class twin (`Course::delete` never touched `event`; it
/// still doesn't delete one).
///
/// The marks going with it give their exam kinds' references back, counted
/// per kind inside this same transaction — the mirror of `Exam::delete`.
/// Skipping it would leave every kind the course graded under counted
/// forever, and a counted kind can never leave the school's settings.
///
/// **Order is the foreign-key graph, children first** (the old store had
/// no FKs, so its order was free): exam results/attempts/answers/images
/// and question images before `exam_question`; `exam_question` before
/// `exam`; homework files before submissions before results before
/// `homework`; roll-call rows before `course_session`; note files and
/// rag rows before `course_note`; every one of them before the `course`
/// row itself, whose `NO ACTION` FKs are checked the moment the final
/// `DELETE` runs.
///
/// `false` = refused, nothing was written: someone is still enrolled. The
/// guard locks the course row (`FOR UPDATE`) and reads its own
/// `enrollment_count`, so an enroll racing this either takes its seat
/// first (and the delete is refused) or finds the row gone (and is refused
/// itself). `Err(NotFound)` keeps the answer a concurrent *delete* used
/// to get — and unlike the old one-statement guard, this one tells the two
/// apart without a second read.
pub async fn delete(db: &Database, course: Course) -> Result<bool, AppError> {
    tx_with_retry(db, true, async move |tx| {
        // The guard and the lock: the row (and its roster count) cannot
        // change under this transaction, and every enroll claim contends on
        // this very row, so check and cascade are one decision.
        let guard = sqlx::query!(
            r#"SELECT enrollment_count, term FROM course WHERE id = $1 FOR UPDATE"#,
            course.id.uuid(),
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(guard) = guard else {
            return Err(AppError::NotFound);
        };
        if guard.enrollment_count != 0 {
            return Ok(false);
        }
        // The exam ids are needed twice — the kind_ref refund and the bank's
        // source_exam clear — and must be collected before the sweeps take
        // the rows they name.
        let exams: Vec<uuid::Uuid> =
            sqlx::query!(r#"SELECT id FROM exam WHERE course = $1"#, course.id.uuid(),)
                .fetch_all(&mut *tx)
                .await?
                .into_iter()
                .map(|row| row.id)
                .collect();
        // Every mark the course ever gave gives its exam kind's reference
        // back, counted per kind (mirrors `Exam::delete`). The kind rides
        // the exam row — `exam_result` names its exam, not its kind.
        sqlx::query!(
            r#"UPDATE kind_ref k
                 SET count = GREATEST(k.count - s.n, 0)
                 FROM (SELECT e.kind, count(*) AS n FROM exam_result r
                        JOIN exam e ON r.exam = e.id
                        WHERE r.exam = ANY($1) GROUP BY e.kind) s
                WHERE k.name = s.kind"#,
            &exams,
        )
        .execute(&mut *tx)
        .await?;
        // The term gets its reference back inside the same transaction.
        if let Some(term) = guard.term {
            sqlx::query!(
                r#"UPDATE term SET course_count = GREATEST(course_count - 1, 0)
                   WHERE id = $1"#,
                term,
            )
            .execute(&mut *tx)
            .await?;
        }
        // Events aimed at this course keep standing, audience cleared.
        sqlx::query!(
            r#"UPDATE event SET audience_course = NULL WHERE audience_course = $1"#,
            course.id.uuid(),
        )
        .execute(&mut *tx)
        .await?;
        // The exam subtree, deepest children first.
        sqlx::query!(r#"DELETE FROM exam_result WHERE exam = ANY($1)"#, &exams)
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
        sqlx::query!(r#"DELETE FROM exam_question WHERE exam = ANY($1)"#, &exams)
            .execute(&mut *tx)
            .await?;
        // The homework subtree: files, then submissions (whose graded_by
        // link names a result deleted after them), then results.
        sqlx::query!(
            r#"DELETE FROM homework_file WHERE submission IN (
                 SELECT id FROM homework_submission
                  WHERE homework IN (SELECT id FROM homework WHERE course = $1))"#,
            course.id.uuid(),
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            r#"DELETE FROM homework_submission WHERE homework IN (
                 SELECT id FROM homework WHERE course = $1)"#,
            course.id.uuid(),
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            r#"DELETE FROM homework_result WHERE homework IN (
                 SELECT id FROM homework WHERE course = $1)"#,
            course.id.uuid(),
        )
        .execute(&mut *tx)
        .await?;
        // Derived AI rows for this course's notes.
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
        // Class attachments: detach and hand each class its count back in
        // one statement, then strike the course out of every blueprint.
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
            r#"UPDATE class_blueprint SET courses = array_remove(courses, $1)
               WHERE $1 = ANY(courses)"#,
            course.id.uuid(),
        )
        .execute(&mut *tx)
        .await?;
        // Sessions with their roll call, then the roster itself.
        sqlx::query!(
            r#"DELETE FROM session_attendance WHERE course = $1"#,
            course.id.uuid(),
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            r#"DELETE FROM course_session WHERE course = $1"#,
            course.id.uuid()
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            r#"DELETE FROM enrollment WHERE course = $1"#,
            course.id.uuid()
        )
        .execute(&mut *tx)
        .await?;
        // The bank keeps its templates; their links to this course's
        // content go first, so the subject/exam deletes below cannot trip
        // a foreign key behind them.
        sqlx::query!(
            r#"UPDATE bank_question SET subject = NULL
               WHERE subject IN (SELECT id FROM subject WHERE course = $1)"#,
            course.id.uuid(),
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            r#"UPDATE bank_question SET source_exam = NULL WHERE source_exam = ANY($1)"#,
            &exams,
        )
        .execute(&mut *tx)
        .await?;
        // Subjects before exams would trip `exam.subject` — exams go first.
        sqlx::query!(r#"DELETE FROM exam WHERE course = $1"#, course.id.uuid())
            .execute(&mut *tx)
            .await?;
        sqlx::query!(
            r#"DELETE FROM homework WHERE course = $1"#,
            course.id.uuid()
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

/// A real course row, for the tests of every child that must now prove its
/// parent exists (`Exam`, `CourseSession`, `Subject` — see
/// [`cap::touch_and_create`]). A minted id nothing wrote is a 404 there, the
/// way a minted subject id already is for an exam question.
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
        None,
        None,
    )
    .await
    .unwrap()
    .id
}

/// The race half of "a child of a course must not survive its deletion", run
/// for one child table: [`delete`] is held open by the schema and
/// `make` fires its create inside that window, four rounds.
///
/// The harness lives here, beside the cascade being raced, and each child names
/// its own pin in its own module (`exam`, `course_session`, `subject`) — the
/// loop, the window and the verdict are one fact about *this* delete, and three
/// copies of it would drift into three different tests of three different
/// things. The sequential half — a create against a course that is already gone
/// — is `tests/regress_course_child_orphans.rs`, which needs no server.
///
/// The window is opened by the database rather than by a lucky interleaving: a
/// `DEFINE EVENT` on `course` fires *inside* the delete's own transaction the
/// instant the row goes, so the `SLEEP` lands between the delete and its
/// `DELETE <child> WHERE course = $course` sweep every time — and a create
/// fired into it reads a course that is still there (removed, uncommitted)
/// while the sweep already ran on a snapshot without its row.
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
            orphans += sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) FROM {table} WHERE course = $1"
            )))
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
    use crate::domain::term::{Term, TermName};

    /// A real `app_user` row: creators, students and enrollers are foreign
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

    async fn course_on(term: Option<TermId>, db: &Database) -> Course {
        create(
            db,
            &a_person(db, "teacher", "teacher").await,
            CourseTitle::try_new("algebra").unwrap(),
            CourseDescription::try_new("").unwrap(),
            CourseKind::course(),
            term,
            None,
        )
        .await
        .unwrap()
    }

    /// The bite test for the delete guard that replaced `ENROLL_LOCK`: the
    /// roster check is now the `WHERE` on the delete itself, so only the guard
    /// can refuse — and it must refuse having written nothing, cascade
    /// included. A `>= 0` guard passes the roster branch and fails here.
    #[tokio::test]
    async fn a_course_with_a_roster_refuses_to_delete() {
        let (db, _leases) = crate::database::init_test_db().await;
        let teacher = a_person(&db, "teacher", "teacher").await;
        let student = a_person(&db, "student", "student").await;
        let course = course_on(None, &db).await;
        crate::db::enrollment::enroll(&db, course.get_id(), &student, &teacher)
            .await
            .unwrap();

        assert!(
            !delete(&db, course.clone()).await.unwrap(),
            "a non-empty roster must refuse the delete"
        );
        assert!(
            read(&db, course.get_id()).await.unwrap().is_some(),
            "a refused delete may write nothing"
        );
        assert!(
            crate::db::enrollment::read_for_user(&db, course.get_id(), &student)
                .await
                .unwrap()
                .is_some(),
            "…the cascade least of all"
        );

        crate::db::enrollment::remove(&db, course.get_id(), &student)
            .await
            .unwrap();
        assert!(delete(&db, course.clone()).await.unwrap());
        assert!(read(&db, course.get_id()).await.unwrap().is_none());

        // The other half of the same guard: once the course row is gone the
        // seat claim matches nothing, so a late enroll is a 404 rather than a
        // roster row that outlived its course.
        let late = crate::db::enrollment::enroll(&db, course.get_id(), &student, &teacher)
            .await
            .expect_err("enrolling into a deleted course must fail");
        assert!(matches!(late, AppError::NotFound), "got {late:?}");
    }

    /// The bite test for the refcount that replaced `TERM_LOCK`: a term is
    /// undeletable exactly while a course links it, and every way a link can
    /// end — PATCH away, and the course's own delete — gives the reference
    /// back. Dropping the release in `delete` leaves the term deletable never.
    #[tokio::test]
    async fn a_term_is_deletable_only_once_no_course_links_it() {
        let (db, _leases) = crate::database::init_test_db().await;
        let at = crate::domain::timestamp::Timestamp::from_millis;
        let term =
            crate::db::term::create(&db, TermName::try_new("2026").unwrap(), at(100), at(200))
                .await
                .unwrap();

        let linked = course_on(Some(*term.get_id()), &db).await;
        let patched = course_on(Some(*term.get_id()), &db).await;
        assert!(
            !crate::db::term::delete(&db, term.clone()).await.unwrap(),
            "two linked courses must refuse the delete"
        );

        update(&db, patched, None, None, None, Some(None), None)
            .await
            .unwrap();
        assert!(
            !crate::db::term::delete(&db, term.clone()).await.unwrap(),
            "one link is still one link"
        );

        assert!(delete(&db, linked).await.unwrap());
        assert!(
            crate::db::term::delete(&db, term.clone()).await.unwrap(),
            "the last link gone, the term may go"
        );
        let again = crate::db::term::delete(&db, term).await;
        assert!(
            matches!(again, Err(AppError::NotFound)),
            "a second delete is a 404, not a refusal: {again:?}"
        );
    }

    /// The claim doubles as the existence check the lock used to make safe:
    /// a term that is already gone cannot be linked, with the same 400 the
    /// web layer's pre-flight lookup gives.
    #[tokio::test]
    async fn a_course_cannot_link_a_term_that_is_gone() {
        let (db, _leases) = crate::database::init_test_db().await;
        let error = create(
            &db,
            &a_person(&db, "teacher", "teacher").await,
            CourseTitle::try_new("algebra").unwrap(),
            CourseDescription::try_new("").unwrap(),
            CourseKind::course(),
            Some(TermId::from_key("gone")),
            None,
        )
        .await
        .expect_err("a missing term must not be linkable");
        assert!(error.to_string().contains("term does not exist"));
    }

    /// The stored `course_count` on one term, absent counting as zero.
    async fn count_on(term: &TermId, db: &Database) -> i64 {
        sqlx::query("SELECT COALESCE(course_count, 0) FROM term WHERE id = $1")
            .bind(term.uuid())
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

    async fn a_term(name: &str, db: &Database) -> Term {
        let at = crate::domain::timestamp::Timestamp::from_millis;
        crate::db::term::create(db, TermName::try_new(name).unwrap(), at(100), at(200))
            .await
            .unwrap()
    }

    /// The invariant, on the create path: a claim and the row it accounts for
    /// commit together or not at all. A refused create must therefore leave
    /// *neither* — no course row, and no count stranded on a term (which is
    /// worse than it sounds: the term's delete guard reads that count, so a
    /// stray one makes the term undeletable forever).
    #[tokio::test]
    async fn a_refused_create_writes_neither_row_nor_count() {
        let (db, _leases) = crate::database::init_test_db().await;
        let term = a_term("2026", &db).await;
        let id = *term.get_id();
        assert!(crate::db::term::delete(&db, term).await.unwrap());

        let error = create(
            &db,
            &a_person(&db, "teacher", "teacher").await,
            CourseTitle::try_new("algebra").unwrap(),
            CourseDescription::try_new("").unwrap(),
            CourseKind::course(),
            Some(id),
            None,
        )
        .await
        .expect_err("a term that is gone must not be linkable");
        assert!(error.to_string().contains("term does not exist"));
        assert_eq!(
            rows("course", &db).await,
            0,
            "a refused create may write no row"
        );
        assert_eq!(
            rows("term", &db).await,
            0,
            "…and least of all a count on a term it just brought back"
        );
    }

    /// The invariant on the PATCH path, both directions: a move carries the new
    /// term's claim and the old term's release with the link itself.
    #[tokio::test]
    async fn a_term_move_moves_the_count() {
        let (db, _leases) = crate::database::init_test_db().await;
        let from = a_term("2026", &db).await;
        let to = a_term("2027", &db).await;
        let course = course_on(Some(*from.get_id()), &db).await;
        assert_eq!(count_on(from.get_id(), &db).await, 1);

        let moved = update(
            &db,
            course,
            None,
            None,
            None,
            Some(Some(*to.get_id())),
            None,
        )
        .await
        .unwrap();
        assert_eq!(moved.get_term(), Some(to.get_id()));
        assert_eq!(
            count_on(from.get_id(), &db).await,
            0,
            "the old term is free"
        );
        assert_eq!(count_on(to.get_id(), &db).await, 1, "the new one is not");
        assert!(crate::db::term::delete(&db, from.clone()).await.unwrap());
        assert!(!crate::db::term::delete(&db, to.clone()).await.unwrap());
    }

    /// The double-claim guard. Both movers compute their claim and release from
    /// the row as *they* read it, so two PATCHes moving the same course off the
    /// same term both release it and both claim their target — two counts for
    /// one link, and the loser's target is undeletable forever. The second call
    /// here runs on the struct read before the first one landed, which is that
    /// race with the interleaving pinned: it must be refused outright, and the
    /// counts must read as if it never ran.
    #[tokio::test]
    async fn a_stale_mover_is_refused_and_claims_nothing() {
        let (db, _leases) = crate::database::init_test_db().await;
        let from = a_term("2026", &db).await;
        let to = a_term("2027", &db).await;
        let other = a_term("2028", &db).await;
        let course = course_on(Some(*from.get_id()), &db).await;
        let stale = course.clone();
        update(
            &db,
            course,
            None,
            None,
            None,
            Some(Some(*to.get_id())),
            None,
        )
        .await
        .unwrap();

        let error = update(
            &db,
            stale.clone(),
            None,
            None,
            None,
            Some(Some(*other.get_id())),
            None,
        )
        .await
        .expect_err("a mover that read a link it no longer holds must be refused");
        assert!(
            matches!(error, AppError::Conflict(_)),
            "a lost CAS is a conflict, not a 404 or a 500: {error:?}"
        );
        let stored = read(&db, stale.get_id()).await.unwrap().unwrap();
        assert_eq!(stored.get_term(), Some(to.get_id()), "the winner's link");
        assert_eq!(
            count_on(from.get_id(), &db).await,
            0,
            "released once, not twice"
        );
        assert_eq!(count_on(to.get_id(), &db).await, 1, "claimed once");
        assert_eq!(count_on(other.get_id(), &db).await, 0, "never claimed");

        // Same race from the other end: the snapshot says *no* link, so the CAS
        // is against an absent column — the shape a bound `NONE` has to match.
        let unlinked = course_on(None, &db).await;
        let stale = unlinked.clone();
        update(
            &db,
            unlinked,
            None,
            None,
            None,
            Some(Some(*to.get_id())),
            None,
        )
        .await
        .unwrap();
        let error = update(
            &db,
            stale.clone(),
            None,
            None,
            None,
            Some(Some(*other.get_id())),
            None,
        )
        .await
        .expect_err("a mover that read an absent link someone else filled must be refused");
        assert!(matches!(error, AppError::Conflict(_)), "{error:?}");
        assert_eq!(count_on(to.get_id(), &db).await, 2, "one claim per link");
        assert_eq!(
            count_on(other.get_id(), &db).await,
            0,
            "still never claimed"
        );
        // …and the unraced set still lands, so the CAS did not just break moves.
        let stored = read(&db, stale.get_id()).await.unwrap().unwrap();
        assert_eq!(stored.get_term(), Some(to.get_id()));
    }

    /// The same race with the counters taken out of it. A PATCH that re-states
    /// the term its snapshot showed shifts *nothing* — [`term::ref_move`] hands
    /// back no claim and no release — so if the guard were armed off the counter
    /// move it would not be armed here at all, and the write would land: the
    /// winner's link silently dragged back, its claim stranded on a term nothing
    /// points at (undeletable forever) and the reverted-to term linked at a
    /// count of zero (deletable while linked). The guard is armed by the request
    /// *carrying* the column instead, which is why this is refused.
    #[tokio::test]
    async fn a_stale_re_stater_is_refused_and_reverts_nothing() {
        let (db, _leases) = crate::database::init_test_db().await;
        let from = a_term("2026", &db).await;
        let to = a_term("2027", &db).await;
        let course = course_on(Some(*from.get_id()), &db).await;
        let stale = course.clone();
        update(
            &db,
            course,
            None,
            None,
            None,
            Some(Some(*to.get_id())),
            None,
        )
        .await
        .unwrap();

        let error = update(
            &db,
            stale.clone(),
            None,
            None,
            None,
            Some(Some(*from.get_id())),
            None,
        )
        .await
        .expect_err("re-stating a link someone else moved must be refused");
        assert!(
            matches!(error, AppError::Conflict(_)),
            "a lost CAS is a conflict, not a silent 200: {error:?}"
        );
        let stored = read(&db, stale.get_id()).await.unwrap().unwrap();
        assert_eq!(stored.get_term(), Some(to.get_id()), "the winner's link");
        assert_eq!(
            count_on(from.get_id(), &db).await,
            0,
            "the reverted-to term must not end up linked at zero"
        );
        assert_eq!(
            count_on(to.get_id(), &db).await,
            1,
            "…nor the winner's term counted with nothing pointing at it"
        );

        // A *genuine* no-op re-state — nobody moved underneath it — still lands,
        // and still moves no counter: the CAS passes trivially.
        let fresh = read(&db, stale.get_id()).await.unwrap().unwrap();
        let same = update(
            &db,
            fresh,
            None,
            None,
            None,
            Some(Some(*to.get_id())),
            None,
        )
        .await
        .expect("re-stating the link the row really holds is not a conflict");
        assert_eq!(same.get_term(), Some(to.get_id()));
        assert_eq!(count_on(from.get_id(), &db).await, 0, "nothing moved");
        assert_eq!(count_on(to.get_id(), &db).await, 1, "…in either direction");
    }

    /// The rollback proof. The transaction releases the old term *before* it
    /// claims the new one, so a move to a term that is gone has already
    /// decremented when the claim throws — the old count still being 1 is the
    /// abort undoing a write that really happened, not a branch that never ran.
    /// The title moves in the same PATCH, and must not stick either.
    #[tokio::test]
    async fn a_term_move_to_a_dead_term_leaves_everything_untouched() {
        let (db, _leases) = crate::database::init_test_db().await;
        let from = a_term("2026", &db).await;
        let dead = a_term("2027", &db).await;
        let dead_id = *dead.get_id();
        assert!(crate::db::term::delete(&db, dead).await.unwrap());
        let course = course_on(Some(*from.get_id()), &db).await;

        let error = update(
            &db,
            course.clone(),
            Some(CourseTitle::try_new("moved").unwrap()),
            None,
            None,
            Some(Some(dead_id)),
            None,
        )
        .await
        .expect_err("a term that is gone must not be linkable");
        assert!(error.to_string().contains("term does not exist"));

        let stored = read(&db, course.get_id()).await.unwrap().unwrap();
        assert_eq!(stored.get_term(), Some(from.get_id()), "the link stays put");
        assert_eq!(
            stored.get_title().as_str(),
            "algebra",
            "…and so does the row"
        );
        assert_eq!(
            count_on(from.get_id(), &db).await,
            1,
            "the release must roll back with the abort"
        );
        assert_eq!(
            rows("term", &db).await,
            1,
            "the dead term must not be resurrected by the claim"
        );
    }

    /// MEASUREMENT — the one race test that actually measures the retry, and
    /// the only one of the four that does. The other three (subject, term,
    /// exam) are status-code guards; each says so on itself.
    ///
    /// [`delete`] is one `BEGIN…COMMIT` whose guard reads
    /// `enrollment_count` off the very record a concurrent enroll increments,
    /// so the two contend by design, and its cascade is long enough that a
    /// rival's write lands mid-transaction. Losing that round writes nothing,
    /// which is what makes re-sending it the recovery. Without
    /// [`crate::database::transaction_with_retry`] a lost round comes out as a
    /// 500: mutation-tested by cutting that retry loop to one attempt, which
    /// turns this test red at 1-2 of 20 rounds (3 runs in 5 — the window is
    /// real but narrow, so a single green run under the mutation means nothing).
    ///
    /// A refusal (`Ok(false)`) or an `Err(NotFound)` is *correct* here and
    /// must not fail this test: the only defect is `AppError::Db`.
    ///
    /// The rate is counted over the whole loop instead of asserted per round,
    /// because a per-round `assert!` aborts at the first hit and would report
    /// "1 of 1" for a bug the point of this test is to *quantify*.
    ///
    /// Multi-threaded and on a real server for the reasons spelled out on
    /// [`crate::domain::fee_plan_assignment`]'s pair of race tests: the
    /// current-thread runtime never interleaves the two, and the embedded
    /// engine does not conflict-check concurrent writes to one record at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_delete_racing_an_enroll_never_answers_500() {
        let (db, _leases) = crate::database::init_test_db().await;
        // The roster rows are foreign keys now: the burst's six racers and
        // their enroller are real people, reused every round (each round's
        // delete cascade frees the seats back).
        let mgr = a_person(&db, "mgr", "manager").await;
        let mut students = Vec::new();
        for seat in 0..6 {
            students.push(a_person(&db, &format!("stu{seat}"), "student").await);
        }
        let (mut delete_500, mut enroll_500, mut enrolled) = (0, 0, 0);
        let (mut last_delete, mut last_enroll) = (String::new(), String::new());
        for round in 0..20 {
            let course = course_on(None, &db).await;

            // A *burst* of enrolls, and a delete held back by a sweeping beat.
            // Released together the delete is one statement while an enroll
            // spends two round trips reading before it claims, so it wins every
            // round and the guard is never contended at all (measured: 0/20
            // seats placed). Six racers over a 0-3ms sweep put the single
            // statement somewhere inside the counter writes instead.
            let drop_it = {
                let (course, db) = (course.clone(), db.clone());
                let beat = std::time::Duration::from_millis(round % 4);
                tokio::spawn(async move {
                    tokio::time::sleep(beat).await;
                    delete(&db, course).await
                })
            };
            let joins: Vec<_> = (0..6)
                .map(|seat| {
                    let (id, db, mgr) = (course.get_id().clone(), db.clone(), mgr);
                    let student = students[seat];
                    tokio::spawn(async move {
                        crate::db::enrollment::enroll(&db, &id, &student, &mgr).await
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
                    enroll_500 += 1;
                    last_enroll = format!("{join:?}");
                }
            }
            // Stored state, not the return values: a seat that landed is what
            // the guard had to see.
            if !crate::db::enrollment::list_for_course(&db, course.get_id(), None, 0)
                .await
                .unwrap()
                .0
                .is_empty()
            {
                enrolled += 1;
            }
        }
        eprintln!(
            "Course::delete raced: {delete_500}/20 delete 500s, {enroll_500} enroll 500s, \
             {enrolled}/20 rounds with a seat placed"
        );
        assert!(
            enrolled > 0,
            "no round ever placed an enrollment, so the delete guard was never contended"
        );
        assert_eq!(
            delete_500, 0,
            "a raced delete must be refused, not 500: {delete_500}/20 rounds, last {last_delete}"
        );
        assert_eq!(
            enroll_500, 0,
            "a raced enroll must retry, not 500: {enroll_500}/20 rounds, last {last_enroll}"
        );
    }
}
