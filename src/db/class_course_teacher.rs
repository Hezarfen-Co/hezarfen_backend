//! The `class_course_teacher` junction: who teaches one instance.
//!
//! D6 moved teacher assignment off the catalog course and onto the instance —
//! two şubeler teaching the same course may legitimately have different
//! teachers — so this is the table every instance-membership gate reads
//! ([`crate::service::class_course::ensure_instance_teacher`]) and the one the
//! demotion sweep strips ([`crate::db::user::set_role_cascade`]).
//!
//! The shape is the `course_teacher` junction's, keyed on the instance:
//! assignment is an idempotent `INSERT` whose `(class_course, teacher)` key
//! answers a duplicate with `DO NOTHING`, so two requests naming the same
//! teacher at once land one row.

use crate::database::{Database, foreign_key_violation};
use crate::domain::class_course::{ClassCourse, ClassCourseId};
use crate::domain::user::UserId;
use crate::error::AppError;

use std::collections::HashMap;

/// Assign `teacher` to teach this instance. `false` when they already did
/// (assignment is idempotent, so a repeat is a no-op rather than an error);
/// `Err(NotFound)` when the instance is gone — the same answer the caller's
/// own read gives one instant earlier.
pub async fn assign(
    db: &Database,
    instance: &ClassCourseId,
    teacher: &UserId,
) -> Result<bool, AppError> {
    let written = match sqlx::query!(
        r#"INSERT INTO class_course_teacher (class_course, teacher)
           SELECT $1, $2 WHERE EXISTS (SELECT 1 FROM class_course WHERE id = $1)
           ON CONFLICT DO NOTHING"#,
        instance.uuid(),
        teacher.uuid(),
    )
    .execute(db)
    .await
    {
        Ok(result) => result.rows_affected(),
        // The teacher row (or the instance) vanished mid-write: a foreign key
        // answers, and a gone target is the `NotFound` the read below gives.
        Err(e) if foreign_key_violation(&e) => return Err(AppError::NotFound),
        Err(e) => return Err(e.into()),
    };
    if written == 0
        && crate::db::class_course::class_of(db, instance)
            .await?
            .is_none()
    {
        // Zero rows and no instance: the conditional select matched nothing,
        // rather than the pair already holding its row.
        return Err(AppError::NotFound);
    }
    Ok(written > 0)
}

/// Drop `teacher` from this instance. `false` when they were not assigned to
/// it, so the web layer can answer 404 instead of pretending it removed
/// someone.
pub async fn unassign(
    db: &Database,
    instance: &ClassCourseId,
    teacher: &UserId,
) -> Result<bool, AppError> {
    let deleted = sqlx::query!(
        r#"DELETE FROM class_course_teacher WHERE class_course = $1 AND teacher = $2"#,
        instance.uuid(),
        teacher.uuid(),
    )
    .execute(db)
    .await?;
    Ok(deleted.rows_affected() > 0)
}

/// Strip `user` from every instance they were assigned to — the sweep for a
/// user demoted below `teacher`, who may no longer run anything.
pub async fn unassign_everywhere(db: &Database, user: &UserId) -> Result<(), AppError> {
    sqlx::query!(
        r#"DELETE FROM class_course_teacher WHERE teacher = $1"#,
        user.uuid(),
    )
    .execute(db)
    .await?;
    Ok(())
}

/// The teachers of one instance, by uuid — the read behind a single
/// `/instances/{id}` response.
pub async fn list_for_instance(
    db: &Database,
    instance: &ClassCourseId,
) -> Result<Vec<UserId>, AppError> {
    let rows = sqlx::query!(
        r#"SELECT teacher AS "teacher: UserId" FROM class_course_teacher
           WHERE class_course = $1 ORDER BY teacher"#,
        instance.uuid(),
    )
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|row| row.teacher).collect())
}

/// Assemble `(instance, teachers)` pairs out of a batch of instance reads:
/// one `class_course_teacher` query for the whole batch, so a page of `n`
/// instances costs one extra statement rather than `n`. An instance nobody is
/// assigned to reads an empty list, the shape
/// [`crate::db::course`]'s old `into_courses` gave a course.
pub async fn into_instances(
    db: &Database,
    rows: Vec<ClassCourse>,
) -> Result<Vec<(ClassCourse, Vec<UserId>)>, AppError> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<uuid::Uuid> = rows.iter().map(|row| row.get_id().uuid()).collect();
    let links = sqlx::query!(
        r#"SELECT class_course AS "class_course: ClassCourseId", teacher AS "teacher: UserId"
           FROM class_course_teacher WHERE class_course = ANY($1)
           ORDER BY class_course, teacher"#,
        &ids,
    )
    .fetch_all(db)
    .await?;
    let mut by_instance: HashMap<uuid::Uuid, Vec<UserId>> = HashMap::new();
    for link in links {
        by_instance
            .entry(link.class_course.uuid())
            .or_default()
            .push(link.teacher);
    }
    Ok(rows
        .into_iter()
        .map(|row| {
            let teachers = by_instance.remove(&row.get_id().uuid()).unwrap_or_default();
            (row, teachers)
        })
        .collect())
}
