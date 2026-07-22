//! Homework entity endpoints: the cross-course "my homework" list plus lookup,
//! edit, and delete of one homework by id. Creation and the per-course listing
//! live under `/courses/{id}/homework` (see [`super::courses`]); student
//! submissions, files, grading, the roster, and the observer report arrive in
//! later steps and reuse the visibility rule ([`Homework::student_sees`]), the
//! [`HOMEWORK_LOCK`], and [`resolve_assigned`] from here.

use axum::Json;
use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::constant::{MAX_HOMEWORK_ASSIGNED, MAX_MAX_FILE_BYTES, UPLOAD_BODY_OVERHEAD_BYTES};
use crate::database::Database;
use crate::domain::course::{Course, CourseId};
use crate::domain::enrollment::Enrollment;
use crate::domain::homework::{Homework, HomeworkDescription, HomeworkId, HomeworkTitle};
use crate::domain::homework_file::{HomeworkFile, HomeworkFileId};
use crate::domain::homework_result::HomeworkResult;
use crate::domain::homework_submission::{HomeworkSubmission, HomeworkSubmissionId, SubmissionText};
use crate::domain::note_file::{FileContentType, FileName};
use crate::domain::role::Role;
use crate::domain::settings::Settings;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::courses::{can_manage_course, can_view_course, visible_courses};
use super::notes::content_disposition;
use super::subjects::subject_in_course;
use super::{
    CurrentUser, HomeworkResponse, Page, PageParams, RequireTeacher, UploadFileForm, blob_path,
    check_not_past, paginate, read_upload, remove_blob, set_or_clear,
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
    // The file-upload route carries a larger HTTP body cap than axum's 2 MB
    // default — the server-wide hard ceiling plus multipart framing headroom;
    // the school's actual, usually smaller, limit is enforced while the stream
    // is read. Same split as notes' file routes (the download/delete siblings
    // ride along under the cap harmlessly, having no body).
    let files = OpenApiRouter::new()
        .routes(routes!(upload_submission_file))
        .routes(routes!(download_submission_file, delete_submission_file))
        .layer(DefaultBodyLimit::max(
            MAX_MAX_FILE_BYTES as usize + UPLOAD_BODY_OVERHEAD_BYTES,
        ));
    OpenApiRouter::new()
        .routes(routes!(list_homework))
        .routes(routes!(get_homework, update_homework, delete_homework))
        .routes(routes!(submit, get_submission, delete_submission))
        .merge(files)
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

/// A 403 unless `user` is enrolled in `course` — the homework twin of the exam
/// sitting wall ([`super::exams::ensure_enrolled`], which is `Exam`-shaped).
/// Submitting is course content, so leaving the course closes it; re-checked on
/// every submission and file write, so an unenrollment mid-task bites the next.
async fn ensure_enrolled(course: &CourseId, user: &UserId, db: &Database) -> Result<(), AppError> {
    if Enrollment::read_for_user(course, user, db).await?.is_none() {
        return Err(AppError::Forbidden(
            "you are not enrolled in this homework's course",
        ));
    }
    Ok(())
}

/// Read a homework and clear `user` to act on their own submission to it — the
/// shared wall of the submission, file, and (student side of the) download
/// paths. Three gates, in order:
///
/// 1. Exact `Student` on the *live* role. Teachers assign homework, they never
///    hand it in; the `RequireStudent` extractor's ≥Student would wave a
///    promoted teacher through, so this checks the role `CurrentUser` read for
///    this very request — the same reasoning as [`super::exams::ensure_student`].
/// 2. Current enrollment in the course.
/// 3. The audience check: a student a subset homework does not name gets a 404,
///    never a 403, so a subset assignment never leaks to those left out (the
///    no-leak idiom an unseen exam draft uses).
async fn gate_own_submission(id: &str, user: &User, db: &Database) -> Result<Homework, AppError> {
    let homework = Homework::read(&HomeworkId::from_key(id), db)
        .await?
        .ok_or(AppError::NotFound)?;
    if user.get_role() != Role::Student {
        return Err(AppError::Forbidden(
            "only students have homework submissions",
        ));
    }
    ensure_enrolled(homework.get_course(), user.get_id(), db).await?;
    if !homework.student_sees(user.get_id()) {
        return Err(AppError::NotFound);
    }
    Ok(homework)
}

/// One file attached to a submission — metadata only; the bytes download
/// separately from `GET /homework/{id}/submission/files/{fid}`.
#[derive(Serialize, ToSchema)]
struct HomeworkFileResponse {
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    id: String,
    /// The uploader's original filename.
    #[schema(example = "odev.pdf")]
    name: String,
    /// MIME type as declared on upload.
    #[schema(example = "application/pdf")]
    content_type: String,
    /// File size in bytes.
    #[schema(example = 24_576)]
    size: i64,
}

impl HomeworkFileResponse {
    fn new(file: &HomeworkFile) -> Self {
        Self {
            id: file.get_id().key().to_string(),
            name: file.get_name().as_str().to_string(),
            content_type: file.get_content_type().as_str().to_string(),
            size: file.get_size(),
        }
    }
}

/// The grade on a submission as the student sees it — present only once the
/// teacher has recorded one, which is also what freezes the submission.
#[derive(Serialize, ToSchema)]
struct HomeworkResultResponse {
    /// `done`, `incomplete`, or `missing` (a teacher's deliberate verdict).
    #[schema(example = "done")]
    status: String,
    /// The optional 0–100 mark; `null` when graded on status alone.
    mark: Option<i64>,
    /// Who graded it.
    graded_by: String,
    /// When it was graded, UTC unix-milliseconds.
    created_at: i64,
}

impl HomeworkResultResponse {
    fn new(result: &HomeworkResult) -> Self {
        Self {
            status: result.get_status().as_str().to_string(),
            mark: result.get_mark().map(|mark| mark.as_i64()),
            graded_by: result.get_graded_by().key().to_string(),
            created_at: result.get_created_at().as_millis(),
        }
    }
}

/// A student's own submission: their text and stamps, the computed late flag,
/// every attached file, and the grade once one exists. `late` is derived here,
/// never stored — `updated_at` (the last text edit or file add/delete) falling
/// after the homework's `due_at`.
#[derive(Serialize, ToSchema)]
struct SubmissionResponse {
    /// The submitting student.
    user: String,
    /// The homework this answers.
    homework: String,
    /// The optional free-text note; `null` when none was given.
    text: Option<String>,
    /// First hand-in, UTC unix-milliseconds (immutable across re-submits).
    submitted_at: i64,
    /// Last touched — a text edit or a file add/delete — UTC unix-milliseconds.
    updated_at: i64,
    /// Whether the submission was last touched after the homework's `due_at`.
    late: bool,
    /// The attached files, newest first (metadata only).
    files: Vec<HomeworkFileResponse>,
    /// The grade, once the teacher has recorded one (freezes the submission).
    result: Option<HomeworkResultResponse>,
}

impl SubmissionResponse {
    fn new(
        homework: &Homework,
        submission: &HomeworkSubmission,
        files: &[HomeworkFile],
        result: Option<&HomeworkResult>,
    ) -> Self {
        Self {
            user: submission.get_user().key().to_string(),
            homework: homework.get_id().key().to_string(),
            text: submission.get_text().map(|text| text.as_str().to_string()),
            submitted_at: submission.get_submitted_at().as_millis(),
            updated_at: submission.get_updated_at().as_millis(),
            late: submission.get_updated_at() > homework.get_due_at(),
            files: files.iter().map(HomeworkFileResponse::new).collect(),
            result: result.map(HomeworkResultResponse::new),
        }
    }
}

#[derive(Deserialize, ToSchema)]
struct SubmitHomework {
    /// The submission's free-text note (optional). Sent in full each time: this
    /// replaces any previous text, and omitting it (or sending `""`) clears it.
    /// Files are managed separately via `.../submission/files` and are never
    /// touched here.
    #[schema(example = "Answers to questions 1–4 are in the attached photo.")]
    text: Option<String>,
}

/// Submit (or re-submit) the caller's own work for a homework: optional text,
/// files added separately. Requires the student role, enrollment in the course,
/// and that the homework is assigned to the caller (a subset it doesn't name
/// 404s, never leaking the assignment). Text replaces the previous text; the
/// first-submit stamp is pinned once and `updated_at` moves to now. `201` on the
/// first submit, `200` on a later edit. Refused (409) once the work is graded —
/// ask the teacher to remove the grade to reopen it.
#[utoipa::path(
    post,
    path = "/{id}/submission",
    tag = "homework",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Homework id")),
    request_body = SubmitHomework,
    responses(
        (status = 200, description = "Submission updated", body = SubmissionResponse),
        (status = 201, description = "Submission created", body = SubmissionResponse),
        (status = 400, description = "Text exceeds its length limit", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not a student, or not enrolled in the homework's course", body = ErrorResponse),
        (status = 404, description = "No such homework (or a subset assignment the caller is not part of)", body = ErrorResponse),
        (status = 409, description = "The homework has been graded — the submission is frozen until the grade is removed", body = ErrorResponse),
    ),
)]
async fn submit(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Json(req): Json<SubmitHomework>,
) -> Result<(StatusCode, Json<SubmissionResponse>), AppError> {
    let homework = gate_own_submission(&id, &user, &st.db).await?;
    // Empty text stores as "no note", so a present-but-blank row is never left.
    let text = match req.text {
        Some(ref text) if !text.is_empty() => Some(SubmissionText::try_new(text)?),
        _ => None,
    };
    // Reader lease of HOMEWORK_LOCK, held from the graded gate through the write:
    // the "no grade yet" read and the upsert that depends on it are one unit, or
    // a grade landing between them lets an edit slip onto a frozen submission.
    let _guard = HOMEWORK_LOCK.read().await;
    if HomeworkResult::read_for(homework.get_id(), user.get_id(), &st.db)
        .await?
        .is_some()
    {
        return Err(AppError::Conflict(
            "this homework has been graded — ask the teacher to remove the grade before editing your submission",
        ));
    }
    // 201-vs-200: a prior read under the lock is exact, where comparing the
    // returned stamps would misreport a same-millisecond re-submit as a create.
    let existed = HomeworkSubmission::read_for(homework.get_id(), user.get_id(), &st.db)
        .await?
        .is_some();
    let submission =
        HomeworkSubmission::upsert(homework.get_id(), user.get_id(), text, &st.db).await?;
    let files = HomeworkFile::list_for_submission(submission.get_id(), &st.db).await?;
    let status = if existed {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    // A result can't exist here — the gate above would have 409'd.
    Ok((
        status,
        Json(SubmissionResponse::new(&homework, &submission, &files, None)),
    ))
}

/// Read the caller's own submission to a homework: their text, files, the
/// computed late flag, and the grade if one exists. Same visibility gates as
/// submitting (student, enrolled, assigned). `404` until they have submitted.
#[utoipa::path(
    get,
    path = "/{id}/submission",
    tag = "homework",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Homework id")),
    responses(
        (status = 200, description = "The caller's submission", body = SubmissionResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not a student, or not enrolled in the homework's course", body = ErrorResponse),
        (status = 404, description = "No such homework, a subset assignment the caller is not part of, or nothing submitted yet", body = ErrorResponse),
    ),
)]
async fn get_submission(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<SubmissionResponse>, AppError> {
    let homework = gate_own_submission(&id, &user, &st.db).await?;
    let submission = HomeworkSubmission::read_for(homework.get_id(), user.get_id(), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let files = HomeworkFile::list_for_submission(submission.get_id(), &st.db).await?;
    let result = HomeworkResult::read_for(homework.get_id(), user.get_id(), &st.db).await?;
    Ok(Json(SubmissionResponse::new(
        &homework,
        &submission,
        &files,
        result.as_ref(),
    )))
}

/// Withdraw the caller's own submission — its text, its file rows, and their
/// blobs. Same visibility gates as submitting. Refused (409) once the work is
/// graded. The file rows fall in one transaction with the submission (children
/// first); their blob names are collected before the wipe and unlinked after.
#[utoipa::path(
    delete,
    path = "/{id}/submission",
    tag = "homework",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Homework id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not a student, or not enrolled in the homework's course", body = ErrorResponse),
        (status = 404, description = "No such homework, a subset assignment the caller is not part of, or nothing submitted yet", body = ErrorResponse),
        (status = 409, description = "The homework has been graded — the submission is frozen until the grade is removed", body = ErrorResponse),
    ),
)]
async fn delete_submission(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let homework = gate_own_submission(&id, &user, &st.db).await?;
    let _guard = HOMEWORK_LOCK.read().await;
    if HomeworkResult::read_for(homework.get_id(), user.get_id(), &st.db)
        .await?
        .is_some()
    {
        return Err(AppError::Conflict(
            "this homework has been graded — ask the teacher to remove the grade before deleting your submission",
        ));
    }
    let submission = HomeworkSubmission::read_for(homework.get_id(), user.get_id(), &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    // Collect blob names before the cascade (submission.delete wipes the file
    // rows in the same transaction), then unlink after the rows are gone.
    let files = HomeworkFile::list_for_submission(submission.get_id(), &st.db).await?;
    submission.delete(&st.db).await?;
    for file in &files {
        remove_blob(&st.files_path, file.get_file()).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Serve a submission file as a download — `Content-Disposition: attachment`,
/// never inline. Homework files take *any* content type (documents, photos —
/// and so a `text/html` or `image/svg+xml` too), so, unlike the raster-only,
/// allowlisted exam images [`super::serve_inline_blob`] renders inline, they
/// must download: an inline HTML/SVG upload would run as stored XSS in a
/// viewer's session. This is exactly the note-file download idiom.
async fn serve_download(st: &AppState, file: &HomeworkFile) -> Result<Response, AppError> {
    let bytes = tokio::fs::read(blob_path(&st.files_path, file.get_file()))
        .await
        .map_err(|err| {
            // The row exists but its blob doesn't — server-side damage (a lost
            // volume path), not a client 404.
            AppError::Internal(format!(
                "missing blob for homework file {}: {err}",
                file.get_id().key()
            ))
        })?;
    // The stored content type is validated ASCII without control characters, so
    // it always parses; the fallback is belt and braces.
    let content_type = HeaderValue::from_str(file.get_content_type().as_str())
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    let disposition = HeaderValue::from_str(&content_disposition(file.get_name().as_str()))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment"));
    Ok((
        [
            (CONTENT_TYPE, content_type),
            (CONTENT_DISPOSITION, disposition),
        ],
        bytes,
    )
        .into_response())
}

/// Attach a file to the caller's own submission. `multipart/form-data` with the
/// bytes under a `file` field (its `filename` required); any content type, at
/// most the school's `max_file_bytes`, up to 10 files per submission. Same
/// visibility gates as submitting. A submission need not exist first — a
/// photo-only homework never types text, so this auto-creates an empty
/// submission to hang the file off (an existing one's text is preserved). Adding
/// a file re-stamps the submission's `updated_at`. Refused (409) once graded, or
/// once the 10-file cap is reached.
#[utoipa::path(
    post,
    path = "/{id}/submission/files",
    tag = "homework",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Homework id")),
    request_body(content = UploadFileForm, content_type = "multipart/form-data"),
    responses(
        (status = 201, description = "File stored", body = HomeworkFileResponse),
        (status = 400, description = "Missing file field, empty file, or an invalid filename or content type", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not a student, or not enrolled in the homework's course", body = ErrorResponse),
        (status = 404, description = "No such homework (or a subset assignment the caller is not part of)", body = ErrorResponse),
        (status = 409, description = "The homework has been graded, or the submission already holds the maximum of 10 files", body = ErrorResponse),
        (status = 413, description = "File exceeds the school's size limit", body = ErrorResponse),
    ),
)]
async fn upload_submission_file(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<HomeworkFileResponse>), AppError> {
    let homework = gate_own_submission(&id, &user, &st.db).await?;
    let limit = Settings::load(&st.db).await?.get_max_file_bytes();
    // Consume the body before taking the lock — a slow upload must not stall the
    // homework subsystem (mirrors the exam/note image uploads).
    let upload = read_upload(&mut multipart, limit).await?;
    let name = FileName::try_new(&upload.name.unwrap_or_default())?;
    let content_type = FileContentType::try_new(&upload.content_type.unwrap_or_default())?;

    // Reader lease of HOMEWORK_LOCK, held from the graded gate through the write.
    let _guard = HOMEWORK_LOCK.read().await;
    if HomeworkResult::read_for(homework.get_id(), user.get_id(), &st.db)
        .await?
        .is_some()
    {
        return Err(AppError::Conflict(
            "this homework has been graded — ask the teacher to remove the grade before adding files",
        ));
    }
    // A submission row must exist to hang the file off; auto-create an empty one
    // for the photo-only case rather than force a separate text submit first.
    let submission = match HomeworkSubmission::read_for(homework.get_id(), user.get_id(), &st.db)
        .await?
    {
        Some(existing) => existing,
        None => HomeworkSubmission::upsert(homework.get_id(), user.get_id(), None, &st.db).await?,
    };

    // Blob first, row second — a stored row always points at a real blob. The
    // 10-file cap is enforced inside `insert` under its own lock (order:
    // HOMEWORK_LOCK then the cap Mutex, never reversed). Unlink the fresh blob if
    // the row insert loses the cap race.
    let file = HomeworkFile::new(
        submission.get_id(),
        name,
        content_type,
        upload.data.len() as i64,
    );
    let path = blob_path(&st.files_path, file.get_file());
    tokio::fs::write(&path, &upload.data)
        .await
        .map_err(|err| AppError::Internal(format!("failed to store the file blob: {err}")))?;
    let stored = match file.insert(&st.db).await {
        Ok(stored) => stored,
        Err(err) => {
            let _ = tokio::fs::remove_file(&path).await;
            return Err(err);
        }
    };
    // A landed file moves the submission's "last touched" clock (the late flag).
    HomeworkSubmission::touch(submission.get_id(), &st.db).await?;
    Ok((
        StatusCode::CREATED,
        Json(HomeworkFileResponse::new(&stored)),
    ))
}

/// Download a submission file's bytes. Two callers, one handler: a student reads
/// their own file (behind the submission gate), or a teacher who manages the
/// homework's course reads any file under it. A parent never reaches here —
/// observers get the report, never the bytes. The file is scoped to the homework
/// in the path, so a managed homework's id can't be used to pull a file from
/// another one.
#[utoipa::path(
    get,
    path = "/{id}/submission/files/{fid}",
    tag = "homework",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Homework id"),
        ("fid" = String, Path, description = "File id"),
    ),
    responses(
        (status = 200, description = "The file bytes", content_type = "application/octet-stream"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "A student not enrolled, or a teacher without management rights over the course", body = ErrorResponse),
        (status = 404, description = "No such homework or file (or a subset assignment the caller is not part of)", body = ErrorResponse),
    ),
)]
async fn download_submission_file(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, fid)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let file_id = HomeworkFileId::from_key(&fid);
    let file = if user.get_role() == Role::Student {
        // Student: only their own file, behind the full submission gate.
        let homework = gate_own_submission(&id, &user, &st.db).await?;
        let submission = HomeworkSubmissionId::composite(homework.get_id(), user.get_id());
        HomeworkFile::read_for(&file_id, &submission, &st.db)
            .await?
            .ok_or(AppError::NotFound)?
    } else {
        // Teacher+: any file under a homework they manage.
        let (homework, course) = homework_with_course(&id, &st.db).await?;
        if !can_manage_course(&course, &user) {
            return Err(AppError::Forbidden(
                "only the course creator, an assigned teacher, or a manager/admin can read submission files",
            ));
        }
        HomeworkFile::read_in_homework(&file_id, homework.get_id(), &st.db)
            .await?
            .ok_or(AppError::NotFound)?
    };
    serve_download(&st, &file).await
}

/// Remove a file from the caller's own submission — row first, then its blob.
/// Same visibility gates as submitting. Refused (409) once graded. Removing a
/// file re-stamps the submission's `updated_at`.
#[utoipa::path(
    delete,
    path = "/{id}/submission/files/{fid}",
    tag = "homework",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Homework id"),
        ("fid" = String, Path, description = "File id"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not a student, or not enrolled in the homework's course", body = ErrorResponse),
        (status = 404, description = "No such homework or file (or a subset assignment the caller is not part of)", body = ErrorResponse),
        (status = 409, description = "The homework has been graded — the submission is frozen until the grade is removed", body = ErrorResponse),
    ),
)]
async fn delete_submission_file(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((id, fid)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let homework = gate_own_submission(&id, &user, &st.db).await?;
    let _guard = HOMEWORK_LOCK.read().await;
    if HomeworkResult::read_for(homework.get_id(), user.get_id(), &st.db)
        .await?
        .is_some()
    {
        return Err(AppError::Conflict(
            "this homework has been graded — ask the teacher to remove the grade before deleting files",
        ));
    }
    let submission = HomeworkSubmissionId::composite(homework.get_id(), user.get_id());
    let file = HomeworkFile::read_for(&HomeworkFileId::from_key(&fid), &submission, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let blob = file.get_file().to_string();
    file.delete(&st.db).await?;
    remove_blob(&st.files_path, &blob).await;
    // A removed file moves the submission's "last touched" clock (the late flag).
    HomeworkSubmission::touch(&submission, &st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}
