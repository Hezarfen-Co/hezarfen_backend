//! Subject workflows: the id resolvers every question/homework tag goes
//! through — the same-course rule for curriculum content, existence-only for
//! the bank's cross-course origin metadata — over the row writes in
//! [`crate::db::subject`]. Since the course-template system, the tag gate for
//! instance-scoped content is [`in_instance`]: the same-course rule *plus*
//! the section's resolved subject set (override-or-inherit), because a
//! section teaches a selection of the course, not the whole course.

use crate::database::Database;
use crate::db::subject;
use crate::domain::class_course::ClassCourse;
use crate::domain::course::CourseId;
use crate::domain::subject::{Subject, SubjectDescription, SubjectId, SubjectName};
use crate::error::{AppError, ValidationError};

pub async fn create(
    db: &Database,
    course: &CourseId,
    name: SubjectName,
    description: SubjectDescription,
) -> Result<Subject, AppError> {
    subject::create(db, course, name, description).await
}

/// The subject row, for callers that only inspect it — the web layer's gates
/// read through here.
pub async fn read(db: &Database, id: &SubjectId) -> Result<Option<Subject>, AppError> {
    subject::read(db, id).await
}

/// Several subjects in one query — the bulk half of a list endpoint that
/// names each row's subject (a read per row would be an N+1).
pub async fn list_by_ids(db: &Database, ids: &[&SubjectId]) -> Result<Vec<Subject>, AppError> {
    subject::list_by_ids(db, ids).await
}

pub async fn list_for_course(
    db: &Database,
    course: &CourseId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Subject>, i64), AppError> {
    subject::list_for_course(db, course, limit, offset).await
}

pub async fn update(
    db: &Database,
    target: Subject,
    name: Option<SubjectName>,
    description: Option<SubjectDescription>,
) -> Result<Subject, AppError> {
    subject::update(db, target, name, description).await
}

pub async fn delete(db: &Database, target: Subject) -> Result<Subject, AppError> {
    subject::delete(db, target).await
}

/// Turn a request-supplied subject id into a validated reference, provided the
/// subject belongs to `course`. The *course half* of the instance tag gate —
/// a question may only be tagged with a subject of its own exam's course.
/// Instance-scoped writers use [`in_instance`], which adds the section's
/// resolved subject set on top; this fn stays for the callers that have only
/// the course at hand. Unknown or foreign subjects are a `400` naming the
/// field. Shared by the question create/update handlers.
pub async fn in_course(db: &Database, id: &str, course: &CourseId) -> Result<SubjectId, AppError> {
    let subject =
        subject::read(db, &SubjectId::from_key(id))
            .await?
            .ok_or(AppError::Validation(ValidationError::Invalid {
                field: "subject_id",
                reason: "subject does not exist",
            }))?;
    if subject.get_course() != course {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "subject_id",
            reason: "subject belongs to a different course",
        }));
    }
    Ok(*subject.get_id())
}

/// The instance-scoped tag gate: the same two 400s [`in_course`] answers for
/// the instance's course — same order, so the first 4xx a client sees is
/// unchanged — then the course-template half: the subject must sit in what
/// this section actually *teaches*, its resolved set
/// ([`crate::service::offering_subject::resolved_for_instance`]), not merely
/// in the catalog course. A right-course-but-off-syllabus subject is a `400`
/// naming the field. The resolved-set probe is one SQL statement that reads
/// the section's flag and the matching table as of one snapshot
/// ([`crate::db::offering_subject::resolves_for_class_course`]).
pub async fn in_instance(
    db: &Database,
    id: &str,
    instance: &ClassCourse,
) -> Result<SubjectId, AppError> {
    let subject_id = in_course(db, id, instance.get_course()).await?;
    crate::service::offering_subject::ensure_resolved_member(db, instance, &subject_id).await?;
    Ok(subject_id)
}

/// Turn a request-supplied subject id into a validated reference, checking only
/// that the subject exists — no course tie. For the question bank, whose subject
/// is cross-course origin metadata: the same-course rule applies at instantiate
/// time, not here. An unknown subject is a `400` naming the field.
pub async fn must_exist(db: &Database, id: &str) -> Result<SubjectId, AppError> {
    let subject =
        subject::read(db, &SubjectId::from_key(id))
            .await?
            .ok_or(AppError::Validation(ValidationError::Invalid {
                field: "subject_id",
                reason: "subject does not exist",
            }))?;
    Ok(*subject.get_id())
}
