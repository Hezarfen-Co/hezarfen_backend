//! Homework grading workflows: the policy gates around the atomic grade —
//! nobody grades themselves, the target must be a live enrolled student in
//! the homework's audience — and the un-grade. The freeze-stamping
//! transaction lives in [`crate::db::homework_result`]; it locks the
//! homework row across the stamp, which is what orders a grade against every
//! student-side write — there is no subsystem lock here any more.

use crate::database::Database;
use crate::db::homework;
use crate::db::homework_result;
use crate::domain::class_course::ClassCourseId;
use crate::domain::exam_result::Mark;
use crate::domain::homework::HomeworkId;
use crate::domain::homework_result::{HomeworkResult, HomeworkStatus};
use crate::domain::role::Role;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ValidationError};

/// Record (or overwrite) `target`'s grade for the homework. The web layer
/// has already read the homework, checked course-management rights, walled
/// the archived term, and validated the status and mark; everything here is
/// re-run or decided against the row the grade transaction locks:
///
/// - the homework is re-read (a homework delete is a fellow writer — both
///   contend on the homework row's lock, so a grade cannot resurrect a
///   result row under a vanished homework; the vanished row answers 404);
/// - grading never targets oneself — no grader, whatever their role, may
///   write their own grade;
/// - the target user must exist, and only students carry homework grades —
///   the live role, so a stale enrollment left behind by a promotion can't
///   reopen grading for staff;
/// - the target must be enrolled in the homework's course, and in the
///   homework's audience — a subset assignment is also the grading roster.
pub async fn grade(
    db: &Database,
    id: &HomeworkId,
    teacher: &User,
    status: HomeworkStatus,
    mark: Option<Mark>,
    target: &UserId,
) -> Result<HomeworkResult, AppError> {
    let homework = homework::read(db, id).await?.ok_or(AppError::NotFound)?;

    // Grading never targets oneself.
    if target == teacher.get_id() {
        return Err(AppError::Forbidden("grading yourself is not allowed"));
    }

    // Target user must exist.
    let Some(target_user) = crate::service::user::read(db, target).await? else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user",
            reason: "target user does not exist",
        }));
    };

    // Only students carry homework grades.
    if target_user.get_role() != Role::Student {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user",
            reason: "only students can be graded",
        }));
    }

    // ... enrolled in the instance the homework belongs to ...
    if crate::service::enrollment::read_for_user(db, homework.get_class_course(), target)
        .await?
        .is_none()
    {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user",
            reason: "target user is not enrolled in this course",
        }));
    }

    // ... and in the homework's audience — a subset assignment is also the
    // grading roster, so a grade can't land on a student the homework never
    // named.
    if !homework.student_sees(target) {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user",
            reason: "target user is not in this homework's audience",
        }));
    }

    let result = homework_result::grade(db, id, target, status, mark, teacher.get_id()).await?;
    // The grade credited the *grader*'s `marks_given`, not the student's — a
    // homework grade moves no counter of the student's at all.
    crate::service::homework::award_badges(teacher.get_id(), db).await;
    Ok(result)
}

/// Remove a student's grade from a homework — un-grading, which unfreezes
/// the student's submission and files for further edits. `None` means there
/// was no grade (the web layer answers 404).
pub async fn ungrade(
    db: &Database,
    id: &HomeworkId,
    target: &UserId,
) -> Result<Option<HomeworkResult>, AppError> {
    homework_result::remove(db, id, target).await
}

/// `user`'s grade for `homework`, if graded — the web layer's read paths go
/// through here.
pub async fn read_for(
    db: &Database,
    homework_id: &HomeworkId,
    user: &UserId,
) -> Result<Option<HomeworkResult>, AppError> {
    homework_result::read_for(db, homework_id, user).await
}

/// Every grade for `homework` — the roster joins these onto the submissions.
pub async fn list_for_homework(
    db: &Database,
    homework_id: &HomeworkId,
) -> Result<Vec<HomeworkResult>, AppError> {
    homework_result::list_for_homework(db, homework_id).await
}

/// `user`'s homework grades across one instance.
pub async fn list_for_user_in_class_course(
    db: &Database,
    class_course: &ClassCourseId,
    user: &UserId,
) -> Result<Vec<HomeworkResult>, AppError> {
    homework_result::list_for_user_in_course(db, class_course, user).await
}
