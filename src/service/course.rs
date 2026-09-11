//! Course workflows: the archived-term gate every course-scoped write pays,
//! the staffing changes a manager makes, and the delete that collects the
//! image/homework/note-file blob keys under the exam and homework locks
//! before the cascade sweeps those rows. The queries live in
//! [`crate::db::course`].

use crate::database::Database;
use crate::db::course;
use crate::domain::answer_image::AnswerImage;
use crate::domain::course::{Course, CourseDescription, CourseId, CourseKind, CourseTitle};
use crate::domain::course_note_file::CourseNoteFile;
use crate::domain::question_image::QuestionImage;
use crate::domain::role::Role;
use crate::domain::term::TermId;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::service::term;

/// Refuse the write when this course's term is archived — a pre-flight guard,
/// accepted race (see README concurrency model): a term archived after this
/// read still lets the write through.
pub async fn require_open(db: &Database, course: &Course) -> Result<(), AppError> {
    match course.get_term() {
        None => Ok(()),
        Some(term) => term::require_open(db, term).await,
    }
}

pub async fn create(
    db: &Database,
    creator: &UserId,
    title: CourseTitle,
    description: CourseDescription,
    kind: CourseKind,
    term: Option<TermId>,
    capacity: Option<i64>,
) -> Result<Course, AppError> {
    course::create(db, creator, title, description, kind, term, capacity).await
}

pub async fn read(db: &Database, id: &CourseId) -> Result<Option<Course>, AppError> {
    course::read(db, id).await
}

pub async fn list_all(db: &Database) -> Result<Vec<Course>, AppError> {
    course::list_all(db).await
}

pub async fn list_enrolled(
    db: &Database,
    user: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Course>, i64), AppError> {
    course::list_enrolled(db, user, limit, offset).await
}

pub async fn list_for_teacher(db: &Database, user: &UserId) -> Result<Vec<Course>, AppError> {
    course::list_for_teacher(db, user).await
}

pub async fn list_by_ids(db: &Database, ids: &[CourseId]) -> Result<Vec<Course>, AppError> {
    course::list_by_ids(db, ids).await
}

/// Only what the request carried is written: an omitted field (`None`) is
/// not stored at all, so a concurrent PATCH of that field survives.
pub async fn update(
    db: &Database,
    course: Course,
    title: Option<CourseTitle>,
    description: Option<CourseDescription>,
    kind: Option<CourseKind>,
    term: Option<Option<TermId>>,
    capacity: Option<Option<i64>>,
) -> Result<Course, AppError> {
    course::update(db, course, title, description, kind, term, capacity).await
}

/// Assign `target` to run `course`, or return the course untouched if they
/// already run it — assignment is idempotent, like enrollment.
///
/// The assignee must already hold the `teacher` role or higher: assignment
/// hands out course-management rights, which every gate behind it re-checks
/// against the `teacher` bar — assigning anyone below it would write a row
/// that can never be used.
pub async fn assign_teacher(
    db: &Database,
    course: &Course,
    target: &UserId,
) -> Result<Course, AppError> {
    require_open(db, course).await?;
    let Some(target_user) = crate::db::user::read(db, target).await? else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user does not exist",
        }));
    };
    if !target_user.get_role().at_least(Role::Teacher) {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "assigned teacher must hold the teacher role or higher",
        }));
    }
    course::assign_teacher(db, course.clone(), target).await
}

/// Drop `target` from this course's assigned teachers. `None` when they
/// weren't assigned, so the web layer can answer 404 instead of pretending
/// it removed someone.
pub async fn unassign_teacher(
    db: &Database,
    course: &Course,
    target: &UserId,
) -> Result<Option<Course>, AppError> {
    require_open(db, course).await?;
    course::unassign_teacher(db, course.clone(), target).await
}

/// Strip `user` from every course they were assigned to — the sweep for a
/// user demoted below `teacher`, who may no longer run anything.
pub async fn unassign_everywhere(db: &Database, user: &UserId) -> Result<(), AppError> {
    course::unassign_everywhere(db, user).await
}

/// What [`delete`] did: whether the course went, and the blob keys of the
/// rows its cascade removed — exactly whose files the web layer may unlink.
#[derive(Debug)]
pub struct DeleteOutcome {
    /// `false` = refused, nothing was written: someone is still enrolled.
    pub deleted: bool,
    pub image_files: Vec<String>,
    pub answer_image_files: Vec<String>,
    pub homework_files: Vec<String>,
    pub course_note_files: Vec<String>,
}

/// Delete the course: collect the image/homework/note-file blob keys, then
/// run the cascading delete.
///
/// Writer lease of [`crate::service::exam_attempt::EXAM_LOCK`], for
/// `delete_exam`'s reason: this cascade sweeps the course's exams *and their
/// attempts*, and an attempt is the one exam child whose write cannot
/// collide with the sweep (its claim lands on the student's row, never the
/// exam's). Narrower here — the delete is refused while anyone is enrolled,
/// so a start would have to pass its enrollment gate and then have that
/// enrollment removed under it — but the hole is the same one and so is the
/// lease.
///
/// And the homework half of the same cascade, for
/// [`crate::web::homework::delete_homework`]'s reason: it sweeps the
/// course's homework with its submissions, files and results, and grading
/// ([`crate::db::homework_result::grade`]) writes a
/// result row against a homework it only *read*, which a delete committing
/// alongside is invisible to. Without this lease the grade lands behind the
/// sweep: an orphan `homework_result` under a vanished homework, plus a
/// `marks_given_total` on the grader no ungrade can reach. Lock order here
/// is EXAM_LOCK then HOMEWORK_LOCK, the only path that takes both.
///
/// The keys are read before the delete because it takes their rows with it;
/// a refused delete just drops them unused. Unlinking the blobs stays the
/// web layer's job — a crash in between strands at worst an unreachable
/// blob.
pub async fn delete(db: &Database, course: &Course) -> Result<DeleteOutcome, AppError> {
    require_open(db, course).await?;
    let _guard = crate::service::exam_attempt::EXAM_LOCK.write().await;
    let _homework_guard = crate::service::homework::HOMEWORK_LOCK.write().await;
    let image_files = QuestionImage::file_keys_for_course(course.get_id(), db).await?;
    let answer_image_files = AnswerImage::file_keys_for_course(course.get_id(), db).await?;
    let homework_files =
        crate::db::homework_file::file_keys_for_course(db, course.get_id()).await?;
    let course_note_files = CourseNoteFile::file_keys_for_course(course.get_id(), db).await?;
    let deleted = course::delete(db, course.clone()).await?;
    Ok(DeleteOutcome {
        deleted,
        image_files,
        answer_image_files,
        homework_files,
        course_note_files,
    })
}
