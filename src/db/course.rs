//! The `course` table: the unit exams, sessions, subjects and enrollments
//! hang off. Listed newest first; deleted with its whole subtree in one
use surrealdb::types::{RecordId, SurrealValue};

use crate::constant::{
    CLASS_COURSE_COUNT_FIELD, COURSE_COUNT_FIELD, ENROLLMENT_COUNT_FIELD, REF_COUNT_FIELD,
};
use crate::database::{Database, transaction_with_retry};
use crate::db::cap;
use crate::db::field_update::FieldUpdate;
use crate::db::page::PagedList;
use crate::domain::course::{Course, CourseDescription, CourseId, CourseKind, CourseTitle};
use crate::domain::term::{self, TermId};
use crate::domain::user::UserId;
use crate::error::AppError;

/// The `THROW` marker the delete guard aborts with — a roster that is not
/// empty, or a course row that is no longer there.
const ROSTER_MARK: &str = "course_roster";

/// Create the course, claiming a reference on the term it links (if any) in
/// the *same transaction* as the row: the claim is a conditional write on
/// the term row, so it fails when the term is already gone and it makes the
/// term undeletable the instant this link exists — and a crash can never
/// leave one without the other, which a claim sent as its own query could
/// (the count would strand and the term be undeletable forever).
pub async fn create(
    db: &Database,
    creator: &UserId,
    title: CourseTitle,
    description: CourseDescription,
    kind: CourseKind,
    term: Option<TermId>,
    capacity: Option<i64>,
) -> Result<Course, AppError> {
    let course = Course {
        id: CourseId::generate(),
        creator: creator.clone(),
        teachers: Vec::new(),
        term,
        capacity,
        title,
        description,
        kind,
    };
    let id = course.id.record();
    let Some(term) = course.term.clone() else {
        let created: Option<Course> = db.create(id).content(course).await?;
        return created.ok_or_else(|| AppError::Internal("failed to create course".into()));
    };
    match cap::claim_and_create(
        &term.record(),
        COURSE_COUNT_FIELD,
        cap::UNLIMITED,
        &id,
        &course,
        db,
    )
    .await?
    {
        cap::Claimed::Made(created) => Ok(created),
        // Uncapped, so "full" can only mean the conditional write matched no
        // term row at all — the existence check the pre-flight lookup makes.
        cap::Claimed::Full => Err(term::gone_error()),
        // Unreachable: the id is a ULID this call just generated.
        cap::Claimed::Duplicate => Err(AppError::Internal("failed to create course".into())),
    }
}

pub async fn read(db: &Database, id: &CourseId) -> Result<Option<Course>, AppError> {
    Ok(db.select(id.record()).await?)
}

pub async fn list_all(db: &Database) -> Result<Vec<Course>, AppError> {
    let mut result = db
        .query("SELECT * FROM course ORDER BY id DESC")
        .await?
        .check()?;
    Ok(result.take::<Vec<Course>>(0)?)
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
        "course WHERE id IN (SELECT VALUE course FROM enrollment WHERE user = $usr)",
        "ORDER BY id DESC",
    )
    .bind("usr", user.record())
    .run(limit, offset, db)
    .await
}

/// The courses `user` runs — the ones they created plus the ones a manager
/// assigned them to. A teacher's slice of the catalog.
///
/// corner-cut: unpaged full table scan, and it stays one — every profile read
/// of a teacher pays it, so the ceiling is the course table's size. It
/// cannot be indexed away on SurrealDB 3.2.3: an index on `creator` alone
/// leaves the `OR` a `TableScan` (EXPLAIN), and the per-element index the
/// membership half would need (`DEFINE INDEX ... FIELDS teachers[*]`) is
/// *wrong*, not merely useless — with it, `$usr IN teachers` and
/// `teachers CONTAINS $usr` return **no rows at all**, which is what the
/// integration test `assigned_teacher_manages_course_without_owning_it`
/// catches. A plain `FIELDS teachers` index is correct but unused. The
/// upgrade path is structural: a `course_teacher` link table indexed on
/// `user`, the shape `enrollment` already has, turning this into two
/// index-backed reads.
pub async fn list_for_teacher(db: &Database, user: &UserId) -> Result<Vec<Course>, AppError> {
    let mut result = db
        .query("SELECT * FROM course WHERE creator = $usr OR $usr IN teachers ORDER BY id DESC")
        .bind(("usr", user.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<Course>>(0)?)
}

/// Load every course behind `ids` (one query) — the join half of the
/// attendance report's per-course blocks.
pub async fn list_by_ids(db: &Database, ids: &[CourseId]) -> Result<Vec<Course>, AppError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let records: Vec<RecordId> = ids.iter().map(CourseId::record).collect();
    let mut result = db
        .query("SELECT * FROM course WHERE id IN $ids")
        .bind(("ids", records))
        .await?
        .check()?;
    Ok(result.take::<Vec<Course>>(0)?)
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
    let expected = course.term.as_ref().map(TermId::record);
    FieldUpdate::new(course.id.record())
        .set("title", title)
        .set("description", description)
        .set("kind", kind)
        .set("term", term.map(|term| term.map(|term| term.record())))
        .set("capacity", capacity)
        .refcount(
            COURSE_COUNT_FIELD,
            "term",
            expected,
            claim,
            release,
            term::gone_error(),
        )
        .run::<Course>(db)
        .await
}

/// Assign `teacher` to run this course, or return the course untouched if
/// they already run it — assignment is idempotent, like enrollment.
/// Field-scoped, and the new list is folded server-side out of the *stored*
/// one: a course PATCH awaits a term lookup between its read and its write,
/// so a whole-row save from either side would revert the other. The
/// `array::distinct` keeps the assignment idempotent even when two requests
/// name the same teacher at once (the early return only sees a stale row).
pub async fn assign_teacher(
    db: &Database,
    course: Course,
    teacher: &UserId,
) -> Result<Course, AppError> {
    if course.is_assigned(teacher) {
        return Ok(course);
    }
    let mut result = db
        .query(
            "UPDATE $id SET teachers = array::distinct(array::append(teachers, $usr))
             RETURN AFTER",
        )
        .bind(("id", course.id.record()))
        .bind(("usr", teacher.record()))
        .await?
        .check()?;
    result
        .take::<Vec<Course>>(0)?
        .into_iter()
        .next()
        .ok_or(AppError::NotFound)
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
    // Same field-scoped story as [`assign_teacher`]; `-=` drops the
    // one link off the stored list without touching the course's own text.
    let mut result = db
        .query("UPDATE $id SET teachers -= $usr RETURN AFTER")
        .bind(("id", course.id.record()))
        .bind(("usr", teacher.record()))
        .await?
        .check()?;
    Ok(Some(
        result
            .take::<Vec<Course>>(0)?
            .into_iter()
            .next()
            .ok_or(AppError::NotFound)?,
    ))
}

/// Strip `user` from every course they were assigned to — the sweep for a
/// user demoted below `teacher`, who may no longer run anything.
pub async fn unassign_everywhere(db: &Database, user: &UserId) -> Result<(), AppError> {
    db.query("UPDATE course SET teachers -= $usr WHERE $usr IN teachers")
        .bind(("usr", user.record()))
        .await?
        .check()?;
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
/// [`crate::db::exam::delete`] and
/// [`crate::domain::subject::Subject::delete`] clear them one level down.
/// Deleting a course must leave the bank where deleting each of its exams
/// and subjects by hand would have left it, or a template is left pointing
/// at a dead exam (permanently: nothing else ever visits that column) and
/// at a dead subject the next `PATCH` omitting `subject_id` writes straight
/// back.
///
/// The **grade blueprints** naming it are swept in that same transaction.
/// Nothing else can reach them — a blueprint holds its courses as a list on
/// its own row, not as link rows this cascade could delete — and an id left
/// behind is permanent rather than merely stale: the pump's own
/// [`crate::db::class_blueprint::prune`] fires only
/// while walking a section, so a grade with no sections can never drop one,
/// and every `PATCH` of that template is refused for naming a course that
/// does not exist, which is precisely the call documented as the repair.
///
/// It is a table scan, deliberately unindexed: `class_blueprint` holds one
/// row per grade label the school uses (a dozen), a course delete is rare,
/// and the per-element index the `WHERE` would want
/// (`FIELDS courses[*]`) is the one this store answers with **no rows at
/// all** — the trap [`list_for_teacher`] documents.
///
/// The marks going with it give their exam kinds' references back, counted
/// per kind inside this same transaction — the mirror of `Exam::delete`.
/// Skipping it would leave every kind the course graded under counted
/// forever, and a counted kind can never leave the school's settings.
///
/// `false` = refused, nothing was written: someone is still enrolled. The
/// roster is read off the course's own `enrollment_count`, so the check and
/// the delete are one conditional write on one record — an enroll racing
/// this either takes its seat first (and the delete is refused) or finds
/// the row gone (and is refused itself). `Err(NotFound)`
/// keeps the answer a concurrent *delete* used to get.
pub async fn delete(db: &Database, course: Course) -> Result<bool, AppError> {
    let sql = format!(
        "BEGIN TRANSACTION;
         LET $gone = (DELETE $course WHERE ({ENROLLMENT_COUNT_FIELD} ?? 0) = 0 RETURN BEFORE);
         IF array::len($gone) = 0 {{ THROW '{ROSTER_MARK}' }};
         FOR $row IN $gone {{
             IF $row.term != NONE {{
                 UPDATE $row.term SET {COURSE_COUNT_FIELD} = \
                     math::max([({COURSE_COUNT_FIELD} ?? 0) - 1, 0]);
             }};
         }};
         FOR $row IN ((SELECT exam.kind AS kind, count() AS n FROM exam_result
             WHERE exam IN (SELECT VALUE id FROM exam WHERE course = $course)
             GROUP BY kind) ?? []) {{
             UPDATE type::record('kind_ref', $row.kind) SET {REF_COUNT_FIELD} = \
                 math::max([({REF_COUNT_FIELD} ?? 0) - $row.n, 0])
         }};
         DELETE exam_result WHERE exam IN (SELECT VALUE id FROM exam WHERE course = $course);
         DELETE exam_attempt WHERE exam IN (SELECT VALUE id FROM exam WHERE course = $course);
         DELETE exam_answer WHERE exam IN (SELECT VALUE id FROM exam WHERE course = $course);
         DELETE answer_image WHERE exam IN (SELECT VALUE id FROM exam WHERE course = $course);
         DELETE question_image WHERE exam IN (SELECT VALUE id FROM exam WHERE course = $course);
         DELETE exam_question WHERE exam IN (SELECT VALUE id FROM exam WHERE course = $course);
         DELETE homework_file WHERE submission IN (SELECT VALUE id FROM homework_submission WHERE homework IN (SELECT VALUE id FROM homework WHERE course = $course));
         DELETE homework_submission WHERE homework IN (SELECT VALUE id FROM homework WHERE course = $course);
         DELETE homework_result WHERE homework IN (SELECT VALUE id FROM homework WHERE course = $course);
         DELETE course_note_file WHERE course_note IN (SELECT VALUE id FROM course_note WHERE course = $course);
         DELETE course_note WHERE course = $course;
         LET $detached = (DELETE class_course WHERE course = $course RETURN BEFORE);
         FOR $row IN ($detached ?? []) {{
             UPDATE $row.class SET {CLASS_COURSE_COUNT_FIELD} = \
                 math::max([({CLASS_COURSE_COUNT_FIELD} ?? 0) - 1, 0]);
         }};
         UPDATE class_blueprint SET courses -= $course WHERE $course IN courses;
         DELETE session_attendance WHERE course = $course;
         DELETE course_session WHERE course = $course;
         DELETE enrollment WHERE course = $course;
         UPDATE bank_question SET subject = NONE
             WHERE subject IN (SELECT VALUE id FROM subject WHERE course = $course);
         DELETE subject WHERE course = $course;
         DELETE homework WHERE course = $course;
         UPDATE bank_question SET source_exam = NONE
             WHERE source_exam IN (SELECT VALUE id FROM exam WHERE course = $course);
         DELETE exam WHERE course = $course;
         COMMIT TRANSACTION;"
    );
    // An aborted transaction errors *every* slot, most with a generic "not
    // executed" — only the THROW's own slot names the marker, and a lost
    // round is re-sent rather than reported (see [`transaction_with_retry`]).
    let (_, mut errors) = transaction_with_retry(
        db,
        &sql,
        &[("course".into(), course.id.record().into_value())],
        &[ROSTER_MARK],
    )
    .await?;
    if errors
        .values()
        .any(|error| error.to_string().contains(ROSTER_MARK))
    {
        // Full stop or already gone: the guard cannot tell those apart, and
        // only the refusal path pays for the extra read that can.
        return match read(db, &course.id).await? {
            Some(_) => Ok(false),
            None => Err(AppError::NotFound),
        };
    }
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    Ok(true)
}

/// A real course row, for the tests of every child that must now prove its
/// parent exists (`Exam`, `CourseSession`, `Subject` — see
/// [`cap::touch_and_create`]). A minted id nothing wrote is a 404 there, the
/// way a minted subject id already is for an exam question.
#[cfg(test)]
pub(crate) async fn a_test_course(db: &Database) -> CourseId {
    create(
        db,
        &UserId::generate(),
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
    scratch: &str,
    table: &str,
    make: fn(CourseId, Database) -> tokio::task::JoinHandle<Result<(), AppError>>,
) {
    // The guard is held for the whole test: these bursts are sub-millisecond,
    // and a sibling race test's burst pushes a delete clean out of its window.
    let (db, _serialized) = crate::database::init_test_server(scratch).await;
    db.query(
        "DEFINE EVENT hold_the_window ON TABLE course WHEN $event = 'DELETE' \
         THEN { SLEEP 1s; };",
    )
    .await
    .expect("define the window event")
    .check()
    .expect("check the window event");

    let (mut swept, mut orphans) = (0, 0);
    for round in 0..4 {
        let course = a_test_course(&db).await;
        let drop_it = {
            let (course, db) = (course.clone(), db.clone());
            tokio::spawn(async move {
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
        // The create starts inside the held window.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let child = make(course.clone(), db.clone()).await.unwrap();
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
            let mut left = db
                .query(format!(
                    "SELECT VALUE id FROM {table} WHERE course = $course"
                ))
                .bind(("course", course.record()))
                .await
                .unwrap()
                .check()
                .unwrap();
            orphans += left.take::<Vec<RecordId>>(0).unwrap().len();
        } else if matches!(dropped, Ok(true)) {
            panic!("{table} round {round}: the delete reported success, the course is still there");
        }
    }
    eprintln!("Course::delete raced by a {table} create: {swept}/4 rounds deleted the course");
    assert!(
        swept > 0,
        "{table}: no round ever deleted the course, so the window was never reached"
    );
    assert_eq!(orphans, 0, "{table}: a child outlived its course");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::term::{Term, TermName};

    async fn course_on(term: Option<TermId>, db: &Database) -> Course {
        create(
            db,
            &UserId::from_key("teacher"),
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
        let db = crate::database::init_mem().await.unwrap();
        let teacher = UserId::from_key("teacher");
        let student = UserId::from_key("student");
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
        let db = crate::database::init_mem().await.unwrap();
        let at = crate::domain::timestamp::Timestamp::from_millis;
        let term = Term::create(TermName::try_new("2026").unwrap(), at(100), at(200), &db)
            .await
            .unwrap();

        let linked = course_on(Some(term.get_id().clone()), &db).await;
        let patched = course_on(Some(term.get_id().clone()), &db).await;
        assert!(
            !term.clone().delete(&db).await.unwrap(),
            "two linked courses must refuse the delete"
        );

        update(&db, patched, None, None, None, Some(None), None)
            .await
            .unwrap();
        assert!(
            !term.clone().delete(&db).await.unwrap(),
            "one link is still one link"
        );

        assert!(delete(&db, linked).await.unwrap());
        assert!(
            term.clone().delete(&db).await.unwrap(),
            "the last link gone, the term may go"
        );
        let again = term.delete(&db).await;
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
        let db = crate::database::init_mem().await.unwrap();
        let error = create(
            &db,
            &UserId::from_key("teacher"),
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
        let mut result = db
            .query(format!(
                "SELECT VALUE ({COURSE_COUNT_FIELD} ?? 0) FROM $term"
            ))
            .bind(("term", term.record()))
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

    async fn a_term(name: &str, db: &Database) -> Term {
        let at = crate::domain::timestamp::Timestamp::from_millis;
        Term::create(TermName::try_new(name).unwrap(), at(100), at(200), db)
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
        let db = crate::database::init_mem().await.unwrap();
        let term = a_term("2026", &db).await;
        let id = term.get_id().clone();
        assert!(term.delete(&db).await.unwrap());

        let error = create(
            &db,
            &UserId::from_key("teacher"),
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
            rows("SELECT VALUE id FROM course", &db).await,
            0,
            "a refused create may write no row"
        );
        assert_eq!(
            rows("SELECT VALUE id FROM term", &db).await,
            0,
            "…and least of all a count on a term it just brought back"
        );
    }

    /// The invariant on the PATCH path, both directions: a move carries the new
    /// term's claim and the old term's release with the link itself.
    #[tokio::test]
    async fn a_term_move_moves_the_count() {
        let db = crate::database::init_mem().await.unwrap();
        let from = a_term("2026", &db).await;
        let to = a_term("2027", &db).await;
        let course = course_on(Some(from.get_id().clone()), &db).await;
        assert_eq!(count_on(from.get_id(), &db).await, 1);

        let moved = update(
            &db,
            course,
            None,
            None,
            None,
            Some(Some(to.get_id().clone())),
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
        assert!(from.clone().delete(&db).await.unwrap());
        assert!(!to.clone().delete(&db).await.unwrap());
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
        let db = crate::database::init_mem().await.unwrap();
        let from = a_term("2026", &db).await;
        let to = a_term("2027", &db).await;
        let other = a_term("2028", &db).await;
        let course = course_on(Some(from.get_id().clone()), &db).await;
        let stale = course.clone();
        update(
            &db,
            course,
            None,
            None,
            None,
            Some(Some(to.get_id().clone())),
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
            Some(Some(other.get_id().clone())),
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
            Some(Some(to.get_id().clone())),
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
            Some(Some(other.get_id().clone())),
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
        let db = crate::database::init_mem().await.unwrap();
        let from = a_term("2026", &db).await;
        let to = a_term("2027", &db).await;
        let course = course_on(Some(from.get_id().clone()), &db).await;
        let stale = course.clone();
        update(
            &db,
            course,
            None,
            None,
            None,
            Some(Some(to.get_id().clone())),
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
            Some(Some(from.get_id().clone())),
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
            Some(Some(to.get_id().clone())),
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
        let db = crate::database::init_mem().await.unwrap();
        let from = a_term("2026", &db).await;
        let dead = a_term("2027", &db).await;
        let dead_id = dead.get_id().clone();
        assert!(dead.delete(&db).await.unwrap());
        let course = course_on(Some(from.get_id().clone()), &db).await;

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
            rows("SELECT VALUE id FROM term", &db).await,
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
    #[ignore = "needs a real SurrealDB server: podman start hezarfen-surrealdb && cargo test -- --ignored"]
    async fn a_delete_racing_an_enroll_never_answers_500() {
        let (db, _serialized) = crate::database::init_test_server("course_delete_race").await;
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
                    let (id, db) = (course.get_id().clone(), db.clone());
                    let student = UserId::from_key(&format!("stu{round}_{seat}"));
                    tokio::spawn(async move {
                        crate::db::enrollment::enroll(&db, &id, &student, &UserId::from_key("mgr"))
                            .await
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
