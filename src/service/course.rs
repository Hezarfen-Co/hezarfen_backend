//! Course workflows: the archived-term gate every course-scoped write pays,
//! the staffing changes a manager makes, and the delete that collects the
//! image/homework/note-file blob keys before the guarded cascade sweeps
//! those rows. The queries live in
//! [`crate::db::course`].

use crate::database::Database;
use crate::db::course;
use crate::domain::course::{Course, CourseDescription, CourseId, CourseKind, CourseTitle};
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
/// The old writer leases on the exam-attempt and homework state are gone
/// with the store that needed them: this cascade is one guarded
/// transaction whose retry answers a child write that raced it mid-sweep
/// (`is_retryable_cascade` — a foreign key naming a row the racing write
/// just landed), so an attempt started or a grade written beside the sweep
/// is either swept by the re-sent cascade or refused by the very foreign
/// key it would have orphaned. No lock can out-guard that.
///
/// The keys are read before the delete because it takes their rows with it;
/// a refused delete just drops them unused. Unlinking the blobs stays the
/// web layer's job — a crash in between strands at worst an unreachable
/// blob.
pub async fn delete(db: &Database, course: &Course) -> Result<DeleteOutcome, AppError> {
    require_open(db, course).await?;
    let image_files = crate::db::question_image::file_keys_for_course(db, course.get_id()).await?;
    let answer_image_files =
        crate::db::answer_image::file_keys_for_course(db, course.get_id()).await?;
    let homework_files =
        crate::db::homework_file::file_keys_for_course(db, course.get_id()).await?;
    let course_note_files =
        crate::db::course_note_file::file_keys_for_course(db, course.get_id()).await?;
    let deleted = course::delete(db, course.clone()).await?;
    Ok(DeleteOutcome {
        deleted,
        image_files,
        answer_image_files,
        homework_files,
        course_note_files,
    })
}
