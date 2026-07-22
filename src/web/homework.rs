//! Homework entity endpoints: the cross-course "my homework" list plus lookup,
//! edit, and delete of one homework by id. Creation and the per-course listing
//! live under `/courses/{id}/homework` (see [`super::courses`]); student
//! submissions, files, grading, the roster, and the observer report arrive in
//! later steps and reuse the visibility rule ([`Homework::student_sees`]), the
//! [`HOMEWORK_LOCK`], and [`resolve_assigned`] from here.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::constant::MAX_HOMEWORK_ASSIGNED;
use crate::database::Database;
use crate::domain::course::{Course, CourseId};
use crate::domain::enrollment::Enrollment;
use crate::domain::homework::{Homework, HomeworkDescription, HomeworkId, HomeworkTitle};
use crate::domain::homework_file::HomeworkFile;
use crate::domain::homework_result::HomeworkResult;
use crate::domain::homework_submission::HomeworkSubmission;
use crate::domain::role::Role;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::courses::{can_manage_course, can_view_course, visible_courses};
use super::subjects::subject_in_course;
use super::{
    CurrentUser, HomeworkResponse, Page, PageParams, RequireTeacher, check_not_past, paginate,
    remove_blob, set_or_clear,
};

/// Serializes the homework subsystem's cross-record check-then-writes, which
/// `BEGIN…COMMIT` cannot (write skew) — the same reasoning as
/// [`crate::web::exams::EXAM_LOCK`]. The class of bug: a submission stays
/// editable only *until a result exists*, so the "no grade yet" read and the
/// submission write that depends on it must not straddle a concurrent grade,
/// and a homework delete must not race a submission landing under it. Read side
/// (steps 3/4): the student's submission and file writes, held from the
/// ungraded gate through the upsert, concurrent with each other. Write side:
/// grade/ungrade (steps 3/4) and the homework-delete cascade here. Lock order,
/// where both are taken: `HOMEWORK_LOCK` before the file-cap `Mutex`, never the
/// reverse.
// ponytail: global RwLock, shard per-homework if write latency ever matters.
pub(crate) static HOMEWORK_LOCK: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_homework))
        .routes(routes!(get_homework, update_homework, delete_homework))
}

/// Validate a request's `assigned` list into a stored student subset. `None`,
/// an explicit `null`, and an empty list all mean "the whole enrolled course"
/// (stored as `None`); a non-empty list must name at most
/// [`MAX_HOMEWORK_ASSIGNED`] students, each currently enrolled in `course`.
/// Deduped so a repeated id can't inflate the cap or double a roster row.
/// Shared by the create ([`super::courses`]) and PATCH handlers.
pub(crate) async fn resolve_assigned(
    assigned: Option<Vec<String>>,
    course: &CourseId,
    db: &Database,
) -> Result<Option<Vec<UserId>>, AppError> {
    let Some(mut keys) = assigned.filter(|keys| !keys.is_empty()) else {
        return Ok(None);
    };
    keys.sort();
    keys.dedup();
    if keys.len() > MAX_HOMEWORK_ASSIGNED {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "assigned",
            reason: "may name at most 200 students",
        }));
    }
    let mut users = Vec::with_capacity(keys.len());
    for key in keys {
        let user = UserId::from_key(&key);
        if Enrollment::read_for_user(course, &user, db).await?.is_none() {
            return Err(AppError::Validation(ValidationError::Invalid {
                field: "assigned",
                reason: "every assigned student must be enrolled in the course",
            }));
        }
        users.push(user);
    }
    Ok(Some(users))
}

/// A homework description off the wire: an empty string means "none", anything
/// else is length-validated. Shared by create and PATCH so both treat `""` the
/// same (a stored empty description would be a needless present-but-blank row).
pub(crate) fn description_or_none(text: &str) -> Result<Option<HomeworkDescription>, AppError> {
    if text.is_empty() {
        Ok(None)
    } else {
        Ok(Some(HomeworkDescription::try_new(text)?))
    }
}

/// The homework plus its course, or a 404 — every entity handler here gates on
/// the parent course, so they always travel together.
async fn homework_with_course(id: &str, db: &Database) -> Result<(Homework, Course), AppError> {
    let homework = Homework::read(&HomeworkId::from_key(id), db)
        .await?
        .ok_or(AppError::NotFound)?;
    let course = Course::read(homework.get_course(), db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok((homework, course))
}

/// List the homework across the caller's courses — their "my homework" view —
/// paged via `?limit=&offset=` (omit `limit` for all of it). Manager+ see every
/// course's homework; a teacher sees the homework of courses they run; a
/// student sees only the homework they are assigned (whole-course ones plus any
/// subset that names them). Returns a `{items, total, limit, offset}` envelope.
#[utoipa::path(
    get,
    path = "/",
    tag = "homework",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "A page of the caller's visible homework (all of it when unpaged)", body = Page<HomeworkResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn list_homework(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<HomeworkResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let homework = if user.get_role().at_least(Role::Manager) {
        Homework::list_all(&st.db).await?
    } else {
        let courses = visible_courses(&user, &st.db).await?;
        let ids: Vec<_> = courses.iter().map(|course| course.get_id().clone()).collect();
        // A student sees only the homework they are assigned; a teacher who
        // manages a course sees all of its homework (the manager+ path above
        // already saw everything).
        let managed: Vec<&str> = courses
            .iter()
            .filter(|course| can_manage_course(course, &user))
            .map(|course| course.get_id().key())
            .collect();
        let mut homework = Homework::list_for_courses(&ids, &st.db).await?;
        homework.retain(|hw| {
            managed.contains(&hw.get_course().key()) || hw.student_sees(user.get_id())
        });
        homework
    };
    let total = homework.len() as i64;
    let items = paginate(&homework, limit, offset)
        .iter()
        .map(HomeworkResponse::new)
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Fetch a single homework by id. Visible to whoever can view its course (its
/// enrolled users, creator, assigned teachers, and managers/admins). A student
/// the homework is *not* assigned to gets a 404 — the same no-leak an unseen
/// exam draft gets, so a subset assignment never reveals itself to the students
/// left out of it.
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "homework",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Homework id")),
    responses(
        (status = 200, description = "The homework", body = HomeworkResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled in the homework's course, not its creator or an assigned teacher, and not a manager/admin", body = ErrorResponse),
        (status = 404, description = "Not found (or a subset assignment the caller is not part of)", body = ErrorResponse),
    ),
)]
async fn get_homework(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<HomeworkResponse>, AppError> {
    let (homework, course) = homework_with_course(&id, &st.db).await?;
    if !can_view_course(&course, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only enrolled users, the course creator, an assigned teacher, or a manager/admin can view this homework",
        ));
    }
    // A student the homework is not assigned to must not even learn it exists —
    // 404, not 403, exactly like an exam draft hidden from non-managers.
    if !can_manage_course(&course, &user) && !homework.student_sees(user.get_id()) {
        return Err(AppError::NotFound);
    }
    Ok(Json(HomeworkResponse::new(&homework)))
}

#[derive(Deserialize, ToSchema)]
struct UpdateHomework {
    /// Re-title. Omit to keep the current title.
    title: Option<String>,
    /// Re-describe. Omit to keep; send `null` (or `""`) to clear.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<String>)]
    description: Option<Option<String>>,
    /// New due date, UTC unix-milliseconds. Omit to keep; a newly set value
    /// must not be in the past (a kept one may already be past).
    #[schema(example = 1_900_000_000_000_i64)]
    due_at: Option<i64>,
    /// Re-tag with another of the course's subjects
    /// (`GET /courses/{id}/subjects`). Omit to keep — a homework always has a
    /// subject, so there is no clear.
    subject_id: Option<String>,
    /// Re-scope the audience: a list of enrolled student ids, or an empty list
    /// / `null` for the whole course. Omit to keep. Narrowing is refused (409)
    /// while it would orphan a student who already submitted or was graded.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<Vec<String>>)]
    assigned: Option<Option<Vec<String>>>,
}

/// Edit a homework's title, description, due date, subject, or assigned subset.
/// Requires teacher+ and management rights over its course. Omitted fields keep
/// their value; a newly set `due_at` is re-checked against now and a new
/// `subject_id` re-checked against the course. Narrowing `assigned` is refused
/// (409) while it would orphan an existing submission or result.
#[utoipa::path(
    patch,
    path = "/{id}",
    tag = "homework",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Homework id")),
    request_body = UpdateHomework,
    responses(
        (status = 200, description = "Updated homework", body = HomeworkResponse),
        (status = 400, description = "Invalid title, description, due date, subject (unknown or from another course), or assigned list", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "Narrowing the assigned list would orphan an existing submission or result", body = ErrorResponse),
    ),
)]
async fn update_homework(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<UpdateHomework>,
) -> Result<Json<HomeworkResponse>, AppError> {
    let (homework, course) = homework_with_course(&id, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can edit this homework",
        ));
    }

    let title = match req.title {
        Some(ref title) => HomeworkTitle::try_new(title)?,
        None => homework.get_title().clone(),
    };
    let description = match req.description {
        Some(Some(ref text)) => description_or_none(text)?,
        Some(None) => None,
        None => homework.get_description().cloned(),
    };
    let due_at = match req.due_at {
        Some(millis) => {
            let due_at = Timestamp::from_millis(millis);
            check_not_past("due_at", Some(due_at))?;
            due_at
        }
        None => homework.get_due_at(),
    };
    let subject = match req.subject_id {
        Some(ref subject_id) => subject_in_course(subject_id, course.get_id(), &st.db).await?,
        None => homework.get_subject().clone(),
    };
    let assigned = match req.assigned {
        Some(assigned) => {
            let resolved = resolve_assigned(assigned, course.get_id(), &st.db).await?;
            ensure_no_orphans(&homework, resolved.as_deref(), &st.db).await?;
            resolved
        }
        None => homework.get_assigned().map(<[UserId]>::to_vec),
    };

    let updated = homework
        .update(&subject, title, description, due_at, assigned, &st.db)
        .await?;
    Ok(Json(HomeworkResponse::new(&updated)))
}

/// Refuse (409) a PATCH that would narrow `homework`'s audience so a student
/// who already submitted or was graded falls outside it — their work would be
/// stranded. `new_assigned` is the proposed subset (`None` = whole course, in
/// which case no one can be orphaned). The blocking students are named in the
/// message so the teacher knows whose work to clear (or whom to keep assigned)
/// first.
async fn ensure_no_orphans(
    homework: &Homework,
    new_assigned: Option<&[UserId]>,
    db: &Database,
) -> Result<(), AppError> {
    // Whole-course covers everyone — no narrowing, no orphans.
    let Some(subset) = new_assigned else {
        return Ok(());
    };
    let submissions = HomeworkSubmission::list_for_homework(homework.get_id(), db).await?;
    let results = HomeworkResult::list_for_homework(homework.get_id(), db).await?;
    let mut blocked: Vec<String> = Vec::new();
    for user in submissions
        .iter()
        .map(HomeworkSubmission::get_user)
        .chain(results.iter().map(HomeworkResult::get_user))
    {
        let key = user.key().to_string();
        if !subset.contains(user) && !blocked.contains(&key) {
            blocked.push(key);
        }
    }
    if blocked.is_empty() {
        Ok(())
    } else {
        Err(AppError::ConflictOwned(format!(
            "narrowing the assigned list would orphan existing work by {} student(s): {}",
            blocked.len(),
            blocked.join(", ")
        )))
    }
}

/// Delete a homework and everything under it — submissions, their files, and
/// results — then unlink the file blobs from disk. Requires teacher+ and
/// management rights over its course. Held under [`HOMEWORK_LOCK`]'s write lease
/// so no submission can land under the homework mid-delete; the blob names are
/// collected before the rows are wiped (the cascade is one transaction, children
/// first) and removed after, so a crash in between strands at worst an
/// unreachable file.
#[utoipa::path(
    delete,
    path = "/{id}",
    tag = "homework",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Homework id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn delete_homework(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let (homework, course) = homework_with_course(&id, &st.db).await?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can delete this homework",
        ));
    }
    let _guard = HOMEWORK_LOCK.write().await;
    let blob_keys = HomeworkFile::file_keys_for_homework(homework.get_id(), &st.db).await?;
    homework.delete(&st.db).await?;
    for key in &blob_keys {
        remove_blob(&st.files_path, key).await;
    }
    Ok(StatusCode::NO_CONTENT)
}
