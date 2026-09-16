//! Course workflows: the catalog's CRUD — the template a şube attaches, with
//! no term, capacity or teacher list of its own any more (D1/D5/D6: staffing
//! and roster live on the instances, [`crate::service::class_course`]) — and
//! the delete that collects the image/homework/note-file blob keys before the
//! guarded cascade sweeps those rows. The queries live in
//! [`crate::db::course`].

use crate::database::Database;
use crate::db::course;
use crate::domain::course::{Course, CourseDescription, CourseId, CourseKind, CourseTitle};
use crate::domain::user::UserId;
use crate::error::AppError;

/// Mint a catalog course. The catalog is not bound to a dönem (D1): exams are,
/// through their instance, and no capacity is stored — the roster counter
/// lives on the instance and gates nothing.
pub async fn create(
    db: &Database,
    creator: &UserId,
    title: CourseTitle,
    description: CourseDescription,
    kind: CourseKind,
) -> Result<Course, AppError> {
    course::create(db, creator, title, description, kind).await
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
) -> Result<Course, AppError> {
    course::update(db, course, title, description, kind).await
}

/// What [`delete`] did: whether the course went, and the blob keys of the
/// rows its cascade removed — exactly whose files the web layer may unlink.
#[derive(Debug)]
pub struct DeleteOutcome {
    /// `false` = refused, nothing was written: a şube still teaches this
    /// course, or a student still holds an individual membership in it.
    pub deleted: bool,
    pub image_files: Vec<String>,
    pub answer_image_files: Vec<String>,
    pub homework_files: Vec<String>,
    pub course_note_files: Vec<String>,
}

/// Delete the course: collect the image/homework/note-file blob keys, then
/// run the cascading delete — which takes every instance the catalog is taught
/// in (with everything under them), every individual membership and every
/// teacher link, all in one guarded transaction.
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
