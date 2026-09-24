//! A child must not survive the parent row it names.
//!
//! Since the K12 remodel a child names one of two parents: a `subject` and a
//! `course_note` hang off the catalog `course`, while an `exam` and a
//! `course_session` hang off the class×course **instance** a şube teaches. The
//! tie is a real foreign key now, so a create whose parent is already gone is a
//! refusal that writes nothing — but a create written as a bare insert that
//! *reads* its parent first could still write the orphan, and every route to
//! such a row goes through the parent it names:
//!
//! - an orphan `exam` is unreachable through its instance, while the list still
//!   hands it to every manager+ — undeletable,
//! - an orphan `course_session` 404s forever through the instance it names,
//! - an orphan `subject` 404s through its course, but its id still resolves as
//!   a parent, so a bank template can be tagged with a subject nobody can
//!   reach.
//!
//! **This file is the sequential half**: it pins the existence contract — a
//! create against a parent that is already gone must be a 404, not an orphan —
//! and that such a create leaves the counters its parent's delete guard reads
//! where they were.
//!
//! The **race** half drives the real interleaving and lives in-crate, one pin
//! per child beside the create it pins —
//! `db::exam::tests::an_exam_never_outlives_its_course`,
//! `db::course_session::tests::a_session_never_outlives_its_course`,
//! `db::subject::tests::a_subject_never_outlives_its_course` — over the
//! shared harness `db::course::assert_no_child_outlives_a_course_delete`,
//! against the same per-test Postgres this file runs on.

mod common;

use common::{FIXTURE_YEAR_ENDS_AT, FIXTURE_YEAR_STARTS_AT};
use hezarfen_backend::database::{self, Database};
use hezarfen_backend::db::{
    academic_year, class_group, course, course_session as db_course_session, term,
};
use hezarfen_backend::domain::academic_year::AcademicYearName;
use hezarfen_backend::domain::class_course::ClassCourseId;
use hezarfen_backend::domain::class_group::{ClassGroupId, ClassName};
use hezarfen_backend::domain::grade::GradeLevel;
use hezarfen_backend::domain::course::{CourseDescription, CourseId, CourseKind, CourseTitle};
use hezarfen_backend::domain::course_note::{CourseNoteContent, CourseNoteTitle};
use hezarfen_backend::domain::course_note_file::{CourseNoteFile, FileContentType, FileName};
use hezarfen_backend::domain::course_session::SessionTopic;
use hezarfen_backend::domain::exam::{
    ExamAttemptLimit, ExamDescription, ExamKind, ExamSchedule, ExamTitle,
};
use hezarfen_backend::domain::settings::Settings;
use hezarfen_backend::domain::subject::{SubjectDescription, SubjectName};
use hezarfen_backend::domain::term::{TermId, TermName};
use hezarfen_backend::domain::timestamp::Timestamp;
use hezarfen_backend::domain::user::UserId;
use hezarfen_backend::error::AppError;
use hezarfen_backend::service;
use sqlx::Row as _;
use tokio::task::JoinHandle;

async fn teacher(db: &Database) -> UserId {
    // Creator columns are foreign keys now, so the fixture teacher is a row,
    // not a fabricated id. It never logs in, so the hash is a stub.
    sqlx::query(
        "INSERT INTO app_user (id, username, created_at, role) \
         VALUES ($1, 'doktor', 0, 'teacher') ON CONFLICT DO NOTHING",
    )
    .bind(UserId::generate().uuid())
    .execute(db)
    .await
    .unwrap();
    let id: uuid::Uuid = sqlx::query("SELECT id FROM app_user WHERE username = 'doktor'")
        .fetch_one(db)
        .await
        .unwrap()
        .try_get(0)
        .unwrap();
    UserId::from_key(&id.to_string())
}

async fn a_course(db: &Database) -> CourseId {
    let teacher = teacher(db).await;
    course::create(
        db,
        &teacher,
        CourseTitle::try_new("Fizik").unwrap(),
        CourseDescription::try_new("").unwrap(),
        CourseKind::course(),
    )
    .await
    .unwrap()
    .get_id()
    .clone()
}

/// The two parents a child can name: the catalog `course` for a subject and a
/// course note, the class×course **instance** for an exam and a lesson session.
/// Which one a child names decides which row has to go before its create can be
/// asked the question, and [`Fixture`] mints both so either half can be staged
/// out of one fixture.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Parent {
    Course,
    Instance,
}

impl Parent {
    /// The column the child names its parent by.
    fn column(self) -> &'static str {
        match self {
            Parent::Course => "course",
            Parent::Instance => "class_course",
        }
    }
}

/// A catalog course taught by one şube: the course, the class, the instance the
/// pair forms, and the dönem an exam is filed under — everything a child needs
/// to exist, and everything that has to be gone before the child's create can
/// be staged against a missing parent.
#[derive(Clone)]
struct Fixture {
    course: CourseId,
    class: ClassGroupId,
    instance: ClassCourseId,
    term: TermId,
}

/// Mint the whole stack: a year (so the class has a calendar), a dönem inside
/// it, the catalog course, a şube, and the instance the pair forms.
async fn fixture(db: &Database) -> Fixture {
    let teacher = teacher(db).await;
    let year = *academic_year::create(
        db,
        &teacher,
        AcademicYearName::try_new("2025-2026").unwrap(),
        Timestamp::from_millis(FIXTURE_YEAR_STARTS_AT),
        Timestamp::from_millis(FIXTURE_YEAR_ENDS_AT),
        Vec::new(),
    )
    .await
    .unwrap()
    .get_id();
    let term = *term::create(
        db,
        TermName::try_new("1. Dönem").unwrap(),
        year,
        Timestamp::from_millis(FIXTURE_YEAR_STARTS_AT),
        Timestamp::from_millis(FIXTURE_YEAR_ENDS_AT),
    )
    .await
    .unwrap()
    .get_id();
    let course = a_course(db).await;
    // The şube names the year it sits in — the link every instance-scoped
    // archive gate reads (instance → şube → year).
    let class = class_group::create(
        db,
        &teacher,
        ClassName::try_new("9-A").unwrap(),
        GradeLevel::new(9).unwrap(),
        Some(year),
        None,
    )
    .await
    .unwrap()
    .get_id()
    .clone();
    let instance = service::class_course::attach(db, &class, &course, &teacher)
        .await
        .unwrap()
        .get_id()
        .clone();
    Fixture {
        course,
        class,
        instance,
        term,
    }
}

/// Detach the fixture's instance — the shipped way an instance (and everything
/// taught under it) goes. A catalog course is refused while any instance still
/// teaches it, so this is also the move that makes the course deletable.
async fn detach(fixture: &Fixture, db: &Database) -> Vec<String> {
    service::class_course::detach(db, &fixture.class, &fixture.course)
        .await
        .expect("the fixture's own instance detaches")
}

async fn drop_course(course: &CourseId, db: &Database) -> Result<bool, AppError> {
    course::delete(
        db,
        course::read(db, course)
            .await
            .unwrap()
            .expect("the course is there"),
    )
    .await
}

// --- the four creates, each as one spawnable unit --------------------------
// Fn pointers rather than a generic closure: the four take different argument
// types and the race harness only ever needs "start it, tell me if it 500s".
// Two of them name the catalog course (a subject, a note), two name the
// instance (an exam, a lesson session) — the split the K12 remodel made.

fn make_exam(fixture: Fixture, db: Database) -> JoinHandle<Result<(), AppError>> {
    tokio::spawn(async move {
        let teacher = teacher(&db).await;
        let kinds = Settings::defaults().get_exam_kinds().to_vec();
        hezarfen_backend::db::exam::create(
            &db,
            &teacher,
            &fixture.instance,
            &fixture.term,
            ExamTitle::try_new("Yazılı").unwrap(),
            ExamDescription::try_new("").unwrap(),
            ExamKind::try_new("yazili", &kinds).unwrap(),
            ExamSchedule::try_new(None, None, None, None).unwrap(),
            ExamAttemptLimit::try_new(1).unwrap(),
            true,
            false,
            false,
        )
        .await
        .map(|_| ())
    })
}

fn make_session(fixture: Fixture, db: Database) -> JoinHandle<Result<(), AppError>> {
    tokio::spawn(async move {
        let teacher = teacher(&db).await;
        db_course_session::create(
            &db,
            &fixture.instance,
            &teacher,
            SessionTopic::try_new("limits").unwrap(),
            Timestamp::from_millis(1),
            None,
        )
        .await
        .map(|_| ())
    })
}

fn make_subject(fixture: Fixture, db: Database) -> JoinHandle<Result<(), AppError>> {
    tokio::spawn(async move {
        hezarfen_backend::db::subject::create(
            &db,
            &fixture.course,
            SubjectName::try_new("Limits").unwrap(),
            SubjectDescription::try_new("").unwrap(),
        )
        .await
        .map(|_| ())
    })
}

fn make_note(fixture: Fixture, db: Database) -> JoinHandle<Result<(), AppError>> {
    tokio::spawn(async move {
        let teacher = teacher(&db).await;
        hezarfen_backend::db::course_note::create(
            &db,
            &fixture.course,
            &teacher,
            CourseNoteTitle::try_new("plan").unwrap(),
            CourseNoteContent::try_new("").unwrap(),
        )
        .await
        .map(|_| ())
    })
}

/// How many rows of `table` name `parent` in `column`. Stored state, never a
/// return value: the whole point is what the store kept. The id is bound as a
/// uuid — the column is one, and a text bind would not compare against it.
async fn children(table: &str, column: &str, parent: uuid::Uuid, db: &Database) -> usize {
    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM {table} WHERE {column} = $1"
    )))
    .bind(parent)
    .fetch_one(db)
    .await
    .unwrap() as usize
}

/// The pair of counters a catalog course's delete guard reads.
async fn guard_counters(course: &CourseId, db: &Database) -> (i64, i64) {
    sqlx::query_as::<_, (i64, i64)>(
        "SELECT class_course_count, course_membership_count FROM course WHERE id = $1",
    )
    .bind(course.uuid())
    .fetch_one(db)
    .await
    .unwrap()
}

// --- the sequential half: the existence contract ---------------------------

/// A create against a parent that is *already* gone must refuse, having written
/// nothing. This is the half of the fix a race cannot show: a create that reads
/// its parent and then inserts would happily write a row naming a row that does
/// not exist.
///
/// The staging is the shipped shape of "the parent is gone": the fixture's own
/// instance is detached for both parents (a catalog course is refused while any
/// instance still teaches it), and the course is deleted on top for the two
/// children that name it.
async fn a_create_against_a_deleted_parent_refuses(
    table: &str,
    parent: Parent,
    make: fn(Fixture, Database) -> JoinHandle<Result<(), AppError>>,
) {
    let (db, _dbs) = database::init_test_db().await;
    let fixture = fixture(&db).await;
    detach(&fixture, &db).await;
    let (gone, id) = match parent {
        Parent::Course => ("course", {
            assert!(
                drop_course(&fixture.course, &db).await.unwrap(),
                "the course goes"
            );
            fixture.course.uuid()
        }),
        Parent::Instance => ("instance", fixture.instance.uuid()),
    };

    let answer = make(fixture.clone(), db.clone()).await.unwrap();
    assert!(
        matches!(answer, Err(AppError::NotFound)),
        "{table}: a create under a deleted {gone} must be a 404, not {answer:?}"
    );
    assert_eq!(
        children(table, parent.column(), id, &db).await,
        0,
        "{table}: a refused create left a row naming a {gone} that is gone"
    );
}

#[tokio::test]
async fn an_exam_under_a_deleted_instance_is_refused() {
    a_create_against_a_deleted_parent_refuses("exam", Parent::Instance, make_exam).await;
}

#[tokio::test]
async fn a_session_under_a_deleted_instance_is_refused() {
    a_create_against_a_deleted_parent_refuses("course_session", Parent::Instance, make_session)
        .await;
}

#[tokio::test]
async fn a_subject_under_a_deleted_course_is_refused() {
    a_create_against_a_deleted_parent_refuses("subject", Parent::Course, make_subject).await;
}

#[tokio::test]
async fn a_course_note_under_a_deleted_course_is_refused() {
    a_create_against_a_deleted_parent_refuses("course_note", Parent::Course, make_note).await;
}

/// A child create must leave the counters its parent's delete guard reads where
/// they were, and the parent must stay deletable afterwards. The guard on a
/// catalog course is the pair `class_course_count`/`course_membership_count`
/// (and on an instance, the class's own counter) — a create that bumped one as
/// a side effect and never gave it back would leave the parent undeletable for
/// good, which is a worse bug than the orphan it prevents.
#[tokio::test]
async fn the_creates_leave_their_parents_delete_guards_untouched() {
    let (db, _dbs) = database::init_test_db().await;
    let fixture = fixture(&db).await;
    let before = guard_counters(&fixture.course, &db).await;

    for make in [make_exam, make_session, make_subject, make_note] {
        make(fixture.clone(), db.clone()).await.unwrap().unwrap();
    }
    assert_eq!(
        guard_counters(&fixture.course, &db).await,
        before,
        "a child create moved a counter the course's delete guard reads"
    );
    // The proof that matters to a user: once the instance is detached, the
    // course is still deletable.
    detach(&fixture, &db).await;
    assert!(
        drop_course(&fixture.course, &db).await.unwrap(),
        "a course whose children moved a guard counter can never be deleted"
    );
}

/// The cascade half of the same contract, for a child class with two tiers: a
/// `course_note` deleted through the course cascade must take its own
/// `course_note_file` children with it too, not just itself.
#[tokio::test]
async fn a_course_note_and_its_files_never_outlive_a_course_delete() {
    let (db, _dbs) = database::init_test_db().await;
    let course = a_course(&db).await;
    let teacher = teacher(&db).await;
    let note = hezarfen_backend::db::course_note::create(
        &db,
        &course,
        &teacher,
        CourseNoteTitle::try_new("plan").unwrap(),
        CourseNoteContent::try_new("body").unwrap(),
    )
    .await
    .unwrap();
    hezarfen_backend::db::course_note_file::insert(
        &db,
        CourseNoteFile::new(
            note.get_id(),
            FileName::try_new("plan.pdf").unwrap(),
            FileContentType::try_new("application/pdf").unwrap(),
            3,
        ),
    )
    .await
    .unwrap();

    assert!(drop_course(&course, &db).await.unwrap(), "the course goes");

    assert_eq!(
        children("course_note", "course", course.uuid(), &db).await,
        0,
        "a course_note survived its course's delete"
    );
    let files = sqlx::query_as::<_, (uuid::Uuid,)>(
        "SELECT id FROM course_note_file WHERE course_note = $1",
    )
    .bind(note.get_id().clone())
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(
        files.len(),
        0,
        "a course_note_file survived its note's course's delete"
    );
}

/// `CourseNoteFile::insert` already goes through `cap::claim_and_create` on
/// the note row (unlike a bare `db.create` `CourseNote::create` would) — this
/// pins that a file upload against an already-deleted note is refused rather
/// than left as an orphan, the same existence contract as the creates above.
#[tokio::test]
async fn a_course_note_file_under_a_deleted_note_is_refused() {
    let (db, _dbs) = database::init_test_db().await;
    let course = a_course(&db).await;
    let teacher = teacher(&db).await;
    let note = hezarfen_backend::db::course_note::create(
        &db,
        &course,
        &teacher,
        CourseNoteTitle::try_new("plan").unwrap(),
        CourseNoteContent::try_new("").unwrap(),
    )
    .await
    .unwrap();
    assert!(
        course::delete(&db, course::read(&db, &course).await.unwrap().unwrap())
            .await
            .unwrap(),
        "the course, and its note with it, goes"
    );

    let file = hezarfen_backend::db::course_note_file::insert(
        &db,
        CourseNoteFile::new(
            note.get_id(),
            FileName::try_new("plan.pdf").unwrap(),
            FileContentType::try_new("application/pdf").unwrap(),
            3,
        ),
    )
    .await;
    assert!(
        matches!(file, Err(AppError::Conflict(_))),
        "a file upload under a deleted note must refuse, not {file:?}"
    );
    let files = sqlx::query_as::<_, (uuid::Uuid,)>(
        "SELECT id FROM course_note_file WHERE course_note = $1",
    )
    .bind(note.get_id().clone())
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(
        files.len(),
        0,
        "a refused upload left a row naming a note that is gone"
    );
}
