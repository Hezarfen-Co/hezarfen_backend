//! Classes (şube): a named set of students the school moves as one.
//!
//! Nothing here is a second kind of membership — adding a student to a class
//! enrolls them into every course the class carries, and attaching a course
//! enrolls the whole roster into it, as real `enrollment` rows. So the two
//! write axes are gated exactly like the writes they stand in for: the class
//! and its roster are the office's (manager+), while attaching a course writes
//! *that course's* roster and takes the same right enrolling into it does.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;
use crate::domain::class_course::ClassCourse;
use crate::domain::class_group::{ClassGrade, ClassGroup, ClassGroupId, ClassName};
use crate::domain::class_member::ClassMember;
use crate::domain::course::{Course, CourseId};
use crate::domain::role::Role;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::state::AppState;

use super::courses::can_manage_course;
use super::terms::resolve_term;
use super::{
    Page, PageParams, PersonRef, RequireManager, RequireTeacher, person_map, set_or_clear,
};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_class, list_classes))
        .routes(routes!(get_class, update_class, delete_class))
        .routes(routes!(add_member, list_members))
        .routes(routes!(remove_member))
        .routes(routes!(attach_course, list_class_courses))
        .routes(routes!(detach_course))
}

#[derive(Deserialize, ToSchema)]
struct CreateClass {
    #[schema(max_length = 200, example = "9-A")]
    name: String,
    /// The school's own label for the year this class sits in ("9", "10-A",
    /// "anaokulu"). Free text; omit (or send `""`) for a class with no grade.
    #[schema(max_length = 20, example = "9")]
    grade: Option<String>,
    /// The academic term this class belongs to (`GET /terms`). Optional.
    term_id: Option<String>,
}

#[derive(Deserialize, ToSchema)]
struct UpdateClass {
    #[schema(max_length = 200)]
    name: Option<String>,
    /// Omit to keep the current grade, send `null` (or `""`) to clear it, or
    /// send a label to (re)set it.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<String>, max_length = 20)]
    grade: Option<Option<String>>,
    /// Omit to keep the current term, send `null` to unlink, or send a term id
    /// to (re)assign.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<String>)]
    term_id: Option<Option<String>>,
}

#[derive(Deserialize, ToSchema)]
struct AddMember {
    /// The student to put in the class.
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    user_id: String,
}

#[derive(Deserialize, ToSchema)]
struct AttachCourse {
    /// The course the whole class takes.
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    course_id: String,
}

#[derive(Serialize, ToSchema)]
struct ClassResponse {
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    id: String,
    /// Who created the class.
    creator: PersonRef,
    #[schema(example = "9-A")]
    name: String,
    /// The school's own free-text grade label; `null` when the class has none.
    #[schema(example = "9")]
    grade: Option<String>,
    /// The academic term this class belongs to (`GET /terms`); `null` when
    /// unassigned.
    term: Option<String>,
}

impl ClassResponse {
    fn new(class: &ClassGroup, people: &std::collections::HashMap<String, PersonRef>) -> Self {
        Self {
            id: class.get_id().key().to_string(),
            creator: PersonRef::resolve(people, class.get_creator()),
            name: class.get_name().as_str().to_string(),
            grade: class.get_grade().map(|g| g.as_str().to_string()),
            term: class.get_term().map(|term| term.key().to_string()),
        }
    }
}

#[derive(Serialize, ToSchema)]
struct ClassMemberResponse {
    id: String,
    class: String,
    /// The student in the class.
    user: PersonRef,
    /// Who put them there.
    added_by: PersonRef,
}

impl ClassMemberResponse {
    fn new(member: &ClassMember, people: &std::collections::HashMap<String, PersonRef>) -> Self {
        Self {
            id: member.get_id().key().to_string(),
            class: member.get_class().key().to_string(),
            user: PersonRef::resolve(people, member.get_user()),
            added_by: PersonRef::resolve(people, member.get_added_by()),
        }
    }
}

#[derive(Serialize, ToSchema)]
struct ClassCourseResponse {
    id: String,
    class: String,
    /// The course the class takes.
    course: String,
    /// Who attached it.
    attached_by: PersonRef,
}

impl ClassCourseResponse {
    fn new(link: &ClassCourse, people: &std::collections::HashMap<String, PersonRef>) -> Self {
        Self {
            id: link.get_id().key().to_string(),
            class: link.get_class().key().to_string(),
            course: link.get_course().key().to_string(),
            attached_by: PersonRef::resolve(people, link.get_attached_by()),
        }
    }
}

/// The class a path id names, or a 404 — every route under `/classes/{id}`
/// gates on it, so a missing class never reads as an empty roster.
async fn class_or_404(id: &str, db: &Database) -> Result<ClassGroup, AppError> {
    ClassGroup::read(&ClassGroupId::from_key(id), db)
        .await?
        .ok_or(AppError::NotFound)
}

/// An empty grade is *no* grade: the column is nullable, so a client that omits
/// it and one that sends `""` must land on the same stored row.
fn grade_or_none(text: Option<&str>) -> Result<Option<ClassGrade>, AppError> {
    match text {
        None | Some("") => Ok(None),
        Some(text) => Ok(Some(ClassGrade::try_new(text)?)),
    }
}

/// Create a class. Requires manager+ — a class is school structure, not a
/// teacher's own room. `grade` is a free-text label for the year ("9", "10-A"),
/// `term_id` links the school calendar; both optional.
#[utoipa::path(
    post,
    path = "/",
    tag = "classes",
    security(("session_cookie" = [])),
    request_body = CreateClass,
    responses(
        (status = 201, description = "Class created", body = ClassResponse),
        (status = 400, description = "Invalid name or grade, or an unknown term", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
    ),
)]
async fn create_class(
    State(st): State<AppState>,
    RequireManager(user): RequireManager,
    Json(req): Json<CreateClass>,
) -> Result<(StatusCode, Json<ClassResponse>), AppError> {
    let name = ClassName::try_new(&req.name)?;
    let grade = grade_or_none(req.grade.as_deref())?;
    // Pre-flight only: `ClassGroup::create` claims a reference on the term
    // before it writes the link, and a term deleted in between fails that claim
    // with this very error — so an unknown id reads the same whichever side wins.
    let term = resolve_term(req.term_id.as_deref(), &st.db).await?;
    let class = ClassGroup::create(user.get_id(), name, grade, term, &st.db).await?;
    // The creator is the caller — already loaded, no extra lookup.
    let people = PersonRef::map_of(&[&user]);
    Ok((
        StatusCode::CREATED,
        Json(ClassResponse::new(&class, &people)),
    ))
}

/// List every class, newest first. Requires teacher+. Paged via `?limit=&offset=`
/// (omit `limit` for the full list); returns a `{items, total, limit, offset}`
/// envelope.
#[utoipa::path(
    get,
    path = "/",
    tag = "classes",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "A page of classes (the full list when unpaged)", body = Page<ClassResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
    ),
)]
async fn list_classes(
    State(st): State<AppState>,
    RequireTeacher(_user): RequireTeacher,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<ClassResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let (classes, total) = ClassGroup::list_all(limit, offset, &st.db).await?;
    // Join creators onto the page alone — the lookup shrinks with the window.
    let people = person_map(
        classes.iter().map(|class| class.get_creator().clone()),
        &st.db,
    )
    .await?;
    let items = classes
        .iter()
        .map(|class| ClassResponse::new(class, &people))
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Fetch a single class by id. Requires teacher+.
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "classes",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Class id")),
    responses(
        (status = 200, description = "The class", body = ClassResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn get_class(
    State(st): State<AppState>,
    RequireTeacher(_user): RequireTeacher,
    Path(id): Path<String>,
) -> Result<Json<ClassResponse>, AppError> {
    let class = class_or_404(&id, &st.db).await?;
    let people = person_map(std::iter::once(class.get_creator().clone()), &st.db).await?;
    Ok(Json(ClassResponse::new(&class, &people)))
}

/// Update a class. Requires manager+. Omitted fields keep their value; `grade`
/// and `term_id` are nullable, so an explicit `null` clears them.
#[utoipa::path(
    patch,
    path = "/{id}",
    tag = "classes",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Class id")),
    request_body = UpdateClass,
    responses(
        (status = 200, description = "Updated class", body = ClassResponse),
        (status = 400, description = "Invalid name or grade, or an unknown term", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "The term this update moves the class off changed since the caller read it — nothing was written, re-read and retry", body = ErrorResponse),
    ),
)]
async fn update_class(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(id): Path<String>,
    Json(req): Json<UpdateClass>,
) -> Result<Json<ClassResponse>, AppError> {
    let class = class_or_404(&id, &st.db).await?;

    // Only what the request actually carried is validated and written — an
    // omitted field stays `None` so the save never re-sends this snapshot's
    // value over a concurrent PATCH of that field.
    let name = req.name.as_deref().map(ClassName::try_new).transpose()?;
    let grade = match req.grade {
        Some(ref update) => Some(grade_or_none(update.as_deref())?),
        None => None,
    };
    let term = match req.term_id {
        // Explicit `null` clears the link; a value must name a real term.
        Some(ref update) => Some(resolve_term(update.as_deref(), &st.db).await?),
        None => None,
    };

    let updated = class.update(name, grade, term, &st.db).await?;
    let people = person_map(std::iter::once(updated.get_creator().clone()), &st.db).await?;
    Ok(Json(ClassResponse::new(&updated, &people)))
}

/// Delete a class. Requires manager+. Refused with a 409 while it still holds
/// students or courses — nothing cascades, because dropping the class silently
/// would leave the enrollments it pumped with nothing left to sweep them.
#[utoipa::path(
    delete,
    path = "/{id}",
    tag = "classes",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Class id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "Students or courses are still on this class", body = ErrorResponse),
    ),
)]
async fn delete_class(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let class = class_or_404(&id, &st.db).await?;
    if !class.delete(&st.db).await? {
        return Err(AppError::Conflict(
            "this class still holds students or courses — remove its members and detach its courses first",
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---- members ---------------------------------------------------------------

/// Put a student in a class. Requires manager+. They are enrolled into every
/// course the class already carries, in one go: a course with no free seat
/// refuses the whole join with a 409 naming it, and a student already enrolled
/// by hand keeps the row they have (no second seat, and it survives their
/// removal from the class). Adding the same student twice is a 409.
#[utoipa::path(
    post,
    path = "/{id}/members",
    tag = "classes",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Class id")),
    request_body = AddMember,
    responses(
        (status = 201, description = "Student added to the class", body = ClassMemberResponse),
        (status = 400, description = "Unknown user, or user is not a student", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Class not found", body = ErrorResponse),
        (status = 409, description = "Already in this class, the class is at its student ceiling (max_class_members), one of its courses is full, or one of them no longer exists (a stale attachment — detach it)", body = ErrorResponse),
    ),
)]
async fn add_member(
    State(st): State<AppState>,
    RequireManager(user): RequireManager,
    Path(id): Path<String>,
    Json(req): Json<AddMember>,
) -> Result<(StatusCode, Json<ClassMemberResponse>), AppError> {
    let class = class_or_404(&id, &st.db).await?;

    let target = UserId::from_key(&req.user_id);
    let Some(target_user) = User::read(&target, &st.db).await? else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "target user does not exist",
        }));
    };
    // A class membership is enrollment in bulk, and enrollment is student
    // membership — the same bar `POST /courses/{id}/enrollments` holds.
    if target_user.get_role() != Role::Student {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_id",
            reason: "only students can be added to a class",
        }));
    }

    let member = ClassMember::add(class.get_id(), &target, user.get_id(), &st.db).await?;
    let people = PersonRef::map_of(&[&target_user, &user]);
    Ok((
        StatusCode::CREATED,
        Json(ClassMemberResponse::new(&member, &people)),
    ))
}

/// List a class's roster, newest first, paged via `?limit=&offset=` (omit
/// `limit` for the whole roster). Requires teacher+. Returns a
/// `{items, total, limit, offset}` envelope.
#[utoipa::path(
    get,
    path = "/{id}/members",
    tag = "classes",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Class id"), PageParams),
    responses(
        (status = 200, description = "A page of class members (the whole roster when unpaged)", body = Page<ClassMemberResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 404, description = "Class not found", body = ErrorResponse),
    ),
)]
async fn list_members(
    State(st): State<AppState>,
    RequireTeacher(_user): RequireTeacher,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<ClassMemberResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let class = class_or_404(&id, &st.db).await?;
    let (rows, total) = ClassMember::list_for_class(class.get_id(), limit, offset, &st.db).await?;
    // Join people onto the page alone — the lookup shrinks with the window.
    let people = person_map(
        rows.iter()
            .flat_map(|row| [row.get_user().clone(), row.get_added_by().clone()]),
        &st.db,
    )
    .await?;
    let items = rows
        .iter()
        .map(|row| ClassMemberResponse::new(row, &people))
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Take a student out of a class. Requires manager+. The enrollments the class
/// pumped for them are swept with it — except rows another attached class still
/// claims (re-tagged to it) and rows placed by hand (left standing). A student
/// who was not in the class is a 404.
#[utoipa::path(
    delete,
    path = "/{id}/members/{user}",
    tag = "classes",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Class id"),
        ("user" = String, Path, description = "User id"),
    ),
    responses(
        (status = 204, description = "Removed"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Class not found, or that student was not in it", body = ErrorResponse),
    ),
)]
async fn remove_member(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path((id, target)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let class = class_or_404(&id, &st.db).await?;
    ClassMember::remove(class.get_id(), &UserId::from_key(&target), &st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- attached courses ------------------------------------------------------

/// Attach a course to a class. Requires teacher+ and management rights **on
/// that course** — attaching writes its roster, so it takes exactly the right
/// enrolling into it does. The class's whole roster is enrolled in one go: a
/// course that cannot hold all of them takes none (409), and students already
/// enrolled by hand keep their own rows. Attaching the same course twice is a
/// 409.
#[utoipa::path(
    post,
    path = "/{id}/courses",
    tag = "classes",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Class id")),
    request_body = AttachCourse,
    responses(
        (status = 201, description = "Course attached to the class", body = ClassCourseResponse),
        (status = 400, description = "Unknown course", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Class not found", body = ErrorResponse),
        (status = 409, description = "Already attached, the class is at its course ceiling (max_class_courses), or the course cannot hold the whole class", body = ErrorResponse),
    ),
)]
async fn attach_course(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<AttachCourse>,
) -> Result<(StatusCode, Json<ClassCourseResponse>), AppError> {
    let class = class_or_404(&id, &st.db).await?;
    let Some(course) = Course::read(&CourseId::from_key(&req.course_id), &st.db).await? else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "course_id",
            reason: "course does not exist",
        }));
    };
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can attach this course to a class",
        ));
    }

    let link = ClassCourse::attach(class.get_id(), course.get_id(), user.get_id(), &st.db).await?;
    let people = PersonRef::map_of(&[&user]);
    Ok((
        StatusCode::CREATED,
        Json(ClassCourseResponse::new(&link, &people)),
    ))
}

/// List the courses a class is attached to, newest first, paged via
/// `?limit=&offset=` (omit `limit` for all of them). Requires teacher+. Returns
/// a `{items, total, limit, offset}` envelope.
#[utoipa::path(
    get,
    path = "/{id}/courses",
    tag = "classes",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Class id"), PageParams),
    responses(
        (status = 200, description = "A page of the class's courses (all of them when unpaged)", body = Page<ClassCourseResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 404, description = "Class not found", body = ErrorResponse),
    ),
)]
async fn list_class_courses(
    State(st): State<AppState>,
    RequireTeacher(_user): RequireTeacher,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<ClassCourseResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let class = class_or_404(&id, &st.db).await?;
    let (rows, total) = ClassCourse::list_for_class(class.get_id(), limit, offset, &st.db).await?;
    let people = person_map(rows.iter().map(|row| row.get_attached_by().clone()), &st.db).await?;
    let items = rows
        .iter()
        .map(|row| ClassCourseResponse::new(row, &people))
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Detach a course from a class. Requires teacher+ and management rights on
/// that course, like attaching. The enrollments the class pumped into it are
/// swept — except rows another attached class still claims (re-tagged to it)
/// and rows placed by hand (left standing). A course that was not attached is a
/// 404 — but a course row that is *gone* is not: the link a deleted course left
/// behind detaches (the rights check has nothing left to read, and no roster
/// left to protect), or the class holding it could never be deleted.
#[utoipa::path(
    delete,
    path = "/{id}/courses/{course}",
    tag = "classes",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Class id"),
        ("course" = String, Path, description = "Course id"),
    ),
    responses(
        (status = 204, description = "Detached"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator or an assigned teacher (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Class not found, or that course was not attached — a course row that is gone does not refuse the detach, it is the reason for it", body = ErrorResponse),
    ),
)]
async fn detach_course(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, course)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let class = class_or_404(&id, &st.db).await?;
    let course = CourseId::from_key(&course);
    // The rights check is skipped when the course row is gone, rather than the
    // whole detach refused: management rights are read *off* the course, so a
    // link left pointing at a deleted course had no readable owner and this
    // route answered 404 forever — which also left the class undeletable, its
    // attachment counter counting a row nothing could sweep. There is no roster
    // left to protect, and the caller is already teacher+.
    if let Some(course) = Course::read(&course, &st.db).await?
        && !can_manage_course(&course, &user)
    {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can detach this course from a class",
        ));
    }
    ClassCourse::detach(class.get_id(), &course, &st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The nullable-grade rule both write paths share: absent and `""` are the
    /// same *no grade*, so a PATCH clearing with `""` cannot store a blank
    /// label a create could never make.
    #[test]
    fn an_empty_grade_is_no_grade() {
        assert!(grade_or_none(None).unwrap().is_none());
        assert!(grade_or_none(Some("")).unwrap().is_none());
        assert_eq!(
            grade_or_none(Some("9"))
                .unwrap()
                .map(|g| g.as_str().to_string()),
            Some("9".to_string())
        );
        assert!(grade_or_none(Some(&"x".repeat(1000))).is_err());
    }
}
