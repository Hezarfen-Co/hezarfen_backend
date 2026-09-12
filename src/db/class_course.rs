//! The `class_course` table: the course-axis attach entry point and the
//! attachment listing. The refusals-to-errors policy and the detach that turns
//! a zero-row sweep into a 404 live in [`crate::service::class_course`]; the
//! transaction itself is the pump's, [`crate::db::class_pump`].

use crate::constant::CLASS_COURSE_TABLE;
use crate::database::Database;
use crate::db::class_pump::Attached;
use crate::db::page::PagedList;
use crate::domain::class_blueprint::ClassBlueprintId;
use crate::domain::class_course::ClassCourse;
use crate::domain::class_group::ClassGroupId;
use crate::domain::course::CourseId;
use crate::domain::user::UserId;
use crate::error::AppError;

/// The attach itself, with the refusals left *unmapped*.
///
/// A hand attach ([`crate::service::class_course::attach`]) turns each of them
/// into the error the route answers with, because one call is one course and a
/// refusal is that call's whole answer. A blueprint pump cannot: it runs
/// one of these per (class, course), and a course that does not fit one
/// section must not abort the other eleven — so it needs to *read* the
/// refusal and carry on ([`crate::domain::class_blueprint`]).
///
/// `source` is the provenance tag, and it is written by the same statement
/// that writes the link, so no attachment can exist without the answer to
/// "may a blueprint take this back". It is also *claimed* in that
/// transaction ([`Attached::SourceGone`]): a blueprint deleted while this
/// pump ran has already swept by that tag, so a row landing afterwards
/// would carry a name nothing can reach.
pub(crate) async fn attach_sourced(
    db: &Database,
    class: &ClassGroupId,
    course: &CourseId,
    attached_by: &UserId,
    source: Option<&ClassBlueprintId>,
) -> Result<Attached<ClassCourse>, AppError> {
    crate::db::class_pump::attach_course(db, class, course, attached_by, source).await
}

/// The courses a class is attached to, newest first — by when they were
/// attached. The composite primary key is the tiebreaker (class, then course:
/// the two halves of the id this order used to sort), which makes the order
/// total.
pub async fn list_for_class(
    db: &Database,
    class: &ClassGroupId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<ClassCourse>, i64), AppError> {
    PagedList::new(
        format!("{CLASS_COURSE_TABLE} WHERE class = $1"),
        "ORDER BY attached_at DESC, class DESC, course DESC",
    )
    .bind(class.uuid())
    .run(limit, offset, db)
    .await
}
