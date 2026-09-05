//! Class sections (şube): a named set of students the school moves as one.
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
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::database::Database;
use crate::domain::class_blueprint::{ClassBlueprint, ClassBlueprintId, Pumped, Skip};
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
    CurrentUser, Page, PageParams, PersonRef, RequireManager, RequireTeacher, ensure_can_observe,
    person_map, set_or_clear, undo_if_demoted,
};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_class, list_classes))
        // Registration order is irrelevant here, and saying otherwise would be
        // a comment pinning nothing: axum routes on `matchit`, which prefers a
        // static segment over a parameter no matter when either was added, so
        // `/classes/me` reaches `my_classes` and never `get_class` with an id
        // of `"me"`. `a_student_reads_exactly_their_own_classes` is what
        // actually checks it, by asserting the body is a page envelope.
        .routes(routes!(my_classes))
        .routes(routes!(user_classes))
        .routes(routes!(get_class, update_class, delete_class))
        .routes(routes!(add_member, list_members))
        .routes(routes!(remove_member))
        .routes(routes!(attach_course, list_class_courses))
        .routes(routes!(detach_course))
        // Static before parameter is a matchit rule, not a registration order —
        // `/classes/blueprints` reaches the template list and never `get_class`
        // with an id of `"blueprints"`, exactly as `/classes/me` does.
        .routes(routes!(create_blueprint, list_blueprints))
        .routes(routes!(get_blueprint, update_blueprint, delete_blueprint))
        .routes(routes!(blueprint_status))
        .routes(routes!(apply_blueprint))
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
    /// The class's homeroom teacher (sınıf öğretmeni) — a teacher, manager or
    /// admin account. Optional; omit (or send `""`) for a class with none.
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    teacher_id: Option<String>,
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
    /// Omit to keep the current homeroom teacher, send `null` (or `""`) to
    /// clear it, or send a teacher+ user id to (re)assign.
    #[serde(default, deserialize_with = "set_or_clear")]
    #[schema(value_type = Option<String>)]
    teacher_id: Option<Option<String>>,
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
    /// Who created the class — always a manager or admin, so it is `null` for
    /// a caller below teacher+. `GET /classes/me` and a parent's
    /// `GET /classes/user/{user}` are the only reads a non-staff account can
    /// make here, and the office's account names are not theirs to learn:
    /// `GET /users` is admin-only and `/users/search` is teacher+.
    creator: Option<PersonRef>,
    #[schema(example = "9-A")]
    name: String,
    /// The school's own free-text grade label; `null` when the class has none.
    #[schema(example = "9")]
    grade: Option<String>,
    /// The academic term this class belongs to (`GET /terms`); `null` when
    /// unassigned.
    term: Option<String>,
    /// The class's homeroom teacher (sınıf öğretmeni); `null` when none is
    /// assigned — as it is for every class after the account was demoted below
    /// `teacher`.
    teacher: Option<PersonRef>,
}

impl ClassResponse {
    /// `with_creator` is the caller's clearance: every route but the two
    /// membership reads is teacher+ and passes `true`; those two pass it only
    /// for a staff caller. The homeroom teacher is never hidden — naming them
    /// is the whole point of a student's own section read.
    fn new(
        class: &ClassGroup,
        people: &std::collections::HashMap<String, PersonRef>,
        with_creator: bool,
    ) -> Self {
        Self {
            id: class.get_id().key().to_string(),
            creator: with_creator.then(|| PersonRef::resolve(people, class.get_creator())),
            name: class.get_name().as_str().to_string(),
            grade: class.get_grade().map(|g| g.as_str().to_string()),
            term: class.get_term().map(|term| term.key().to_string()),
            teacher: class
                .get_teacher()
                .map(|teacher| PersonRef::resolve(people, teacher)),
        }
    }
}

/// A created class plus what its grade's blueprint did to it — the shape
/// [`BlueprintPumpResponse`] already has on the other write that pumps, with the
/// class in place of the template.
#[derive(Serialize, ToSchema)]
struct CreateClassResponse {
    class: ClassResponse,
    /// The courses the template could not put on this section, empty when it
    /// took them all (and always empty when no template covered its grade).
    skipped: Vec<SkipResponse>,
    /// The grade label of the blueprint that stocked this section, or `null`
    /// when none did. `null` with an empty `skipped` is what tells "no template
    /// covers this grade" apart from "the template applied cleanly" — the
    /// distinction `matched` makes on the grade-wide pumps.
    ///
    /// Stocking is best-effort to the end: a template that could not be read,
    /// or a pump that faulted part-way, also reads `null` rather than turning a
    /// section that exists into a `500` whose caller never learns its id. The
    /// remedy is the same either way — `POST /classes/{id}/blueprint`, which is
    /// idempotent and `404`s when there is genuinely no template.
    #[schema(example = "9")]
    stocked_from: Option<String>,
}

/// Every person a [`ClassResponse`] names: its homeroom teacher, plus its
/// creator when the caller is cleared to see one. Feed this into `person_map` —
/// an id the map is missing renders as a bare ULID, so a creator left out here
/// must also be left out of the response.
fn class_people(class: &ClassGroup, with_creator: bool) -> impl Iterator<Item = UserId> + '_ {
    with_creator
        .then(|| class.get_creator().clone())
        .into_iter()
        .chain(class.get_teacher().cloned())
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

/// The homeroom teacher a request names, as the row itself — an empty string is
/// *no* teacher, exactly like `grade`, so omitting the field and clearing it
/// with `""` land on the same stored row. The named account must exist and hold
/// teacher-or-higher: a class's sınıf öğretmeni is staff, and the check is the
/// same shape `add_member` uses for a non-student. The `User` comes back so the
/// caller can name them in the response without a second read.
async fn teacher_or_none(text: Option<&str>, db: &Database) -> Result<Option<User>, AppError> {
    let (None | Some("")) = text else {
        return resolve_teacher(text.unwrap_or_default(), db)
            .await
            .map(Some);
    };
    Ok(None)
}

/// The teacher-or-higher account `key` names, or the 400 both write paths give.
async fn resolve_teacher(key: &str, db: &Database) -> Result<User, AppError> {
    let Some(teacher) = User::read(&UserId::from_key(key), db).await? else {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "teacher_id",
            reason: "target user does not exist",
        }));
    };
    if !teacher.get_role().at_least(Role::Teacher) {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "teacher_id",
            reason: "only a teacher, manager or admin can be a class's homeroom teacher",
        }));
    }
    Ok(teacher)
}

/// One page of classes with their people joined on — the shared tail of the two
/// membership reads. Ordering follows `rows`, so the page stays newest-first.
///
/// A membership whose class id resolves to nothing is *skipped*: class deletion
/// is refused while members exist, so it is unreachable, and if a repair ever
/// left one behind a student's own section list must degrade to one row short
/// rather than 500. `total` still counts the membership rows, which is what the
/// window was cut from.
async fn classes_page(
    ids: &[ClassGroupId],
    total: i64,
    limit: Option<i64>,
    offset: i64,
    with_creator: bool,
    db: &Database,
) -> Result<Page<ClassResponse>, AppError> {
    let classes = ClassGroup::list_by_ids(ids, db).await?;
    let by_id: std::collections::HashMap<&str, &ClassGroup> = classes
        .iter()
        .map(|class| (class.get_id().key(), class))
        .collect();
    let people = person_map(
        classes
            .iter()
            .flat_map(|class| class_people(class, with_creator)),
        db,
    )
    .await?;
    let items = ids
        .iter()
        .filter_map(|id| by_id.get(id.key()))
        .map(|class| ClassResponse::new(class, &people, with_creator))
        .collect();
    Ok(Page::new(items, total, limit, offset))
}

/// Create a class. Requires manager+ — a class is school structure, not a
/// teacher's own room. `grade` is a free-text label for the year ("9", "10-A"),
/// `term_id` links the school calendar, `teacher_id` names the homeroom teacher
/// (sınıf öğretmeni, a teacher+ account); all optional.
///
/// If a blueprint covers the new class's grade, the class is **stocked from it
/// at once** — the same best-effort pump `POST /classes/{id}/blueprint` runs, so
/// a course that does not fit comes back in `skipped` and the class is created
/// either way. `stocked_from` names the template that did it, and is `null` when
/// none covered the grade.
#[utoipa::path(
    post,
    path = "/",
    tag = "classes",
    security(("session_cookie" = [])),
    request_body = CreateClass,
    responses(
        (status = 201, description = "Class created, stocked from its grade's blueprint when one covers it", body = CreateClassResponse),
        (status = 400, description = "Invalid name or grade, an unknown term, or a teacher_id naming nobody or a non-teacher", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 409, description = "The named homeroom teacher was demoted below teacher while the request ran (the class was rolled back, nothing was created), or the named term is archived — past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create_class(
    State(st): State<AppState>,
    RequireManager(user): RequireManager,
    Json(req): Json<CreateClass>,
) -> Result<(StatusCode, Json<CreateClassResponse>), AppError> {
    let name = ClassName::try_new(&req.name)?;
    let grade = grade_or_none(req.grade.as_deref())?;
    // Pre-flight only: `ClassGroup::create` claims a reference on the term
    // before it writes the link, and a term deleted in between fails that claim
    // with this very error — so an unknown id reads the same whichever side wins.
    let term = resolve_term(req.term_id.as_deref(), &st.db).await?;
    let teacher = teacher_or_none(req.teacher_id.as_deref(), &st.db).await?;
    let class = ClassGroup::create(
        user.get_id(),
        name,
        grade,
        term,
        teacher.as_ref().map(|t| t.get_id().clone()),
        &st.db,
    )
    .await?;
    // The row is written; a demotion that raced this request's role check swept
    // it too early to see it, so the live role is re-read now (see
    // [`undo_if_demoted`]).
    if let Some(teacher) = teacher.as_ref()
        && let Err(demoted) = undo_if_demoted(teacher.get_id(), &st.db).await
    {
        // Roll the whole create back, not just the column: a `409` that left a
        // teacherless class standing would have the caller either retrying into
        // a duplicate or never learning it was there.
        //
        // The guard cannot refuse this delete — the class is one statement old
        // and both of its counters are still absent — so a refusal is a broken
        // invariant, not a client error, and it must not be reported as the
        // `409` whose text promises nothing was created.
        if !class.clone().delete(&st.db).await? {
            return Err(AppError::Internal(format!(
                "class {} took a member or a course between its create and the \
                 rollback of a demoted homeroom teacher; it is still there, \
                 without a teacher",
                class.get_id().key()
            )));
        }
        return Err(demoted);
    }
    // Stocking runs **after** that rollback, never before: `ClassGroup::delete`
    // refuses a class holding courses, so a section pumped first could not be
    // rolled back and the `409` above would leave a half-created class standing
    // behind a promise that nothing was created.
    let stocked = stock_from_blueprint(&class, user.get_id(), &st.db).await;
    // The creator is the caller and the teacher was just read — no extra lookup.
    let mut named = vec![&user];
    named.extend(teacher.as_ref());
    let people = PersonRef::map_of(&named);
    Ok((
        StatusCode::CREATED,
        Json(CreateClassResponse {
            class: ClassResponse::new(&class, &people, true),
            skipped: stocked
                .iter()
                .flat_map(|(_, skipped)| skipped.iter().map(SkipResponse::new))
                .collect(),
            stocked_from: stocked.map(|(grade, _)| grade),
        }),
    ))
}

/// Stock a freshly created class from its grade's blueprint, if one covers it:
/// the template's grade label and the pairs it could not place, or `None` when
/// the class has no grade, no blueprint holds that grade, or the stocking could
/// not be carried out.
///
/// Swallowing that last case is the deliberate one. Every other pump here may
/// fail its whole request, because the caller can name what it asked for again —
/// a blueprint's id *is* the grade label they sent. A class's id is a ULID this
/// request is the only place it is ever returned from, so a `500` after the row
/// is written loses a section that exists. Best-effort therefore runs to the end
/// of this route: the class is reported, `stocked_from` stays `null`, and the
/// retry is the idempotent `POST /classes/{id}/blueprint` — which is also the
/// remedy when no template covers the grade, so the two need not be told apart.
async fn stock_from_blueprint(
    class: &ClassGroup,
    by: &UserId,
    db: &Database,
) -> Option<(String, Vec<Skip>)> {
    let grade = class.get_grade()?;
    let blueprint = ClassBlueprint::read(&ClassBlueprintId::for_grade(grade), db)
        .await
        .ok()??;
    let skipped = blueprint.apply_to(class, by, db).await.ok()?;
    Some((blueprint.get_grade().as_str().to_string(), skipped))
}

/// Filter for the class index.
#[derive(Deserialize, IntoParams)]
struct ClassFilter {
    /// Narrow to the sections carrying this exact grade label — the same
    /// spelling a blueprint is keyed by, matched verbatim (no trimming, no case
    /// folding: a filter that normalized would disagree with the pump it exists
    /// to debug). `?grade=` (empty) asks for the sections carrying *no* grade,
    /// exactly as an empty `grade` on a write means no grade. Omit for every
    /// class.
    #[param(example = "9")]
    grade: Option<String>,
}

/// List every class, newest first. Requires teacher+. `?grade=` narrows to one
/// grade label, matched exactly as written — the label a blueprint is keyed by,
/// so this is the read that shows which sections a `POST /classes/blueprints`
/// pump covered (and `?grade=` on its own lists the sections with no grade at
/// all). An unknown label is an empty page, not a `404`. Paged via
/// `?limit=&offset=` (omit `limit` for the full list); returns a
/// `{items, total, limit, offset}` envelope whose `total` counts every class
/// under the same filter, not just this page.
#[utoipa::path(
    get,
    path = "/",
    tag = "classes",
    security(("session_cookie" = [])),
    params(ClassFilter, PageParams),
    responses(
        (status = 200, description = "A page of classes (the full list when unpaged)", body = Page<ClassResponse>),
        (status = 400, description = "Invalid grade, limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
    ),
)]
async fn list_classes(
    State(st): State<AppState>,
    RequireTeacher(_user): RequireTeacher,
    Query(filter): Query<ClassFilter>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<ClassResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    // The same newtype the write paths validate against, so an over-long label
    // is the same `400` here as on a create. `grade_or_none` folds `""` into
    // "no grade"; the outer `Option` is what keeps *omitted* apart from it.
    let grade = filter
        .grade
        .as_deref()
        .map(|grade| grade_or_none(Some(grade)))
        .transpose()?;
    let (classes, total) = ClassGroup::list_all(grade, limit, offset, &st.db).await?;
    // Join people onto the page alone — the lookup shrinks with the window.
    let people = person_map(
        classes.iter().flat_map(|class| class_people(class, true)),
        &st.db,
    )
    .await?;
    let items = classes
        .iter()
        .map(|class| ClassResponse::new(class, &people, true))
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
    let people = person_map(class_people(&class, true), &st.db).await?;
    Ok(Json(ClassResponse::new(&class, &people, true)))
}

/// Update a class. Requires manager+. Omitted fields keep their value; `grade`,
/// `term_id` and `teacher_id` are nullable, so an explicit `null` clears them.
#[utoipa::path(
    patch,
    path = "/{id}",
    tag = "classes",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Class id")),
    request_body = UpdateClass,
    responses(
        (status = 200, description = "Updated class", body = ClassResponse),
        (status = 400, description = "Invalid name or grade, an unknown term, or a teacher_id naming nobody or a non-teacher", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "The term this update moves the class off changed since the caller read it (nothing was written, re-read and retry), the named homeroom teacher was demoted below teacher while the request ran (the assignment was undone), or the class's own term — or the named one — is archived: past years are read-only", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn update_class(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(id): Path<String>,
    Json(req): Json<UpdateClass>,
) -> Result<Json<ClassResponse>, AppError> {
    let class = class_or_404(&id, &st.db).await?;
    // The class's *current* term, so a move off an archived year is refused
    // too; `resolve_term` below holds the other end (the term moved onto).
    class.require_open(&st.db).await?;

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
    let teacher = match req.teacher_id {
        // Explicit `null` (or `""`) clears it; a value must name a teacher+.
        Some(ref update) => Some(teacher_or_none(update.as_deref(), &st.db).await?),
        None => None,
    };

    let assigned = teacher
        .as_ref()
        .and_then(|teacher| teacher.as_ref().map(|teacher| teacher.get_id().clone()));
    let updated = class
        .update(
            name,
            grade,
            term,
            teacher.map(|teacher| teacher.map(|teacher| teacher.get_id().clone())),
            &st.db,
        )
        .await?;
    // Only when this request named a teacher: a PATCH that left the column
    // alone raced nobody's demotion (see [`undo_if_demoted`]). The undo clears
    // the column, which is the whole of what this request wrote to it.
    if let Some(teacher) = assigned.as_ref() {
        undo_if_demoted(teacher, &st.db).await?;
    }
    let people = person_map(class_people(&updated, true), &st.db).await?;
    Ok(Json(ClassResponse::new(&updated, &people, true)))
}

// ---- a student's own class -------------------------------------------------

/// The classes the caller is a member of, newest membership first. Any
/// authenticated role — this is the one class read a student (or a parent, for
/// themselves) can make, since every other `/classes` route is teacher+. Paged
/// via `?limit=&offset=` (omit `limit` for all of them); returns a
/// `{items, total, limit, offset}` envelope. Staff, who are never class
/// members, simply get an empty page.
#[utoipa::path(
    get,
    path = "/me",
    tag = "classes",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "A page of the caller's classes (all of them when unpaged)", body = Page<ClassResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn my_classes(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<ClassResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    // A student may not learn who in the office created their class; staff may.
    let with_creator = user.get_role().at_least(Role::Teacher);
    Ok(Json(
        classes_of(user.get_id(), limit, offset, with_creator, &st.db).await?,
    ))
}

/// Another user's classes. Requires teacher+, or a parent tied to the target
/// student — the same bar the per-student reports hold, and the same 403 for
/// everyone else (a student reads their own at `GET /classes/me`).
#[utoipa::path(
    get,
    path = "/user/{user}",
    tag = "classes",
    security(("session_cookie" = [])),
    params(("user" = String, Path, description = "User id"), PageParams),
    responses(
        (status = 200, description = "A page of that user's classes (all of them when unpaged)", body = Page<ClassResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher, or a parent link to this student", body = ErrorResponse),
        (status = 404, description = "User not found", body = ErrorResponse),
    ),
)]
async fn user_classes(
    State(st): State<AppState>,
    CurrentUser(caller): CurrentUser,
    Path(user): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<ClassResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let target = UserId::from_key(&user);
    ensure_can_observe(&caller, &target, &st.db).await?;
    // User must exist — a missing user is a 404, not an empty page.
    User::read(&target, &st.db)
        .await?
        .ok_or(AppError::NotFound)?;
    // A linked parent reads this too, and is no more entitled to the office's
    // account names than their child is.
    let with_creator = caller.get_role().at_least(Role::Teacher);
    Ok(Json(
        classes_of(&target, limit, offset, with_creator, &st.db).await?,
    ))
}

/// One user's class page: the membership rows are what the window is cut from,
/// and the class rows are joined onto that page alone.
async fn classes_of(
    user: &UserId,
    limit: Option<i64>,
    offset: i64,
    with_creator: bool,
    db: &Database,
) -> Result<Page<ClassResponse>, AppError> {
    let (rows, total) = ClassMember::list_for_user(user, limit, offset, db).await?;
    let ids: Vec<ClassGroupId> = rows.iter().map(|row| row.get_class().clone()).collect();
    classes_page(&ids, total, limit, offset, with_creator, db).await
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
        (status = 409, description = "Students or courses are still on this class, or its term is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn delete_class(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let class = class_or_404(&id, &st.db).await?;
    class.require_open(&st.db).await?;
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
        (status = 409, description = "Already in this class, the class is at its student ceiling (max_class_members), it carries more courses than one add may enroll at once, one of its courses is full, or one of them no longer exists (a stale attachment — detach it). The body carries a machine `code` beside the prose, and this route answers exactly these: `duplicate`, `class_at_roster_ceiling`, `class_course_list_too_large`, `course_full`, `linked_course_missing`, `term_archived` (the class sits on an archived term — past years are read-only)", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn add_member(
    State(st): State<AppState>,
    RequireManager(user): RequireManager,
    Path(id): Path<String>,
    Json(req): Json<AddMember>,
) -> Result<(StatusCode, Json<ClassMemberResponse>), AppError> {
    let class = class_or_404(&id, &st.db).await?;
    class.require_open(&st.db).await?;

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
        (status = 409, description = "The class's (or course's) term is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn remove_member(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path((id, target)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let class = class_or_404(&id, &st.db).await?;
    class.require_open(&st.db).await?;
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
        (status = 409, description = "Already attached, the class is at its course ceiling (max_class_courses), it holds more students than one attach may enroll at once, or the course cannot hold the whole class. The body carries a machine `code` beside the prose, and this route answers exactly these: `duplicate`, `class_at_course_ceiling`, `class_roster_too_large`, `course_full`, `term_archived` (the class's or the course's term is archived — past years are read-only)", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
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
    // Both ends: neither a class nor a course on a past year takes a new link.
    class.require_open(&st.db).await?;
    course.require_open(&st.db).await?;

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
        (status = 409, description = "The class's (or course's) term is archived — past years are read-only", body = ErrorResponse),
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
    let row = Course::read(&course, &st.db).await?;
    if let Some(row) = row.as_ref()
        && !can_manage_course(row, &user)
    {
        return Err(AppError::Forbidden(
            "only the course creator, an assigned teacher, or a manager/admin can detach this course from a class",
        ));
    }
    // Both ends, and only what is still there: a link whose course row is gone
    // has no term to read, and sweeping it is the whole point of the route.
    class.require_open(&st.db).await?;
    if let Some(row) = row.as_ref() {
        row.require_open(&st.db).await?;
    }
    ClassCourse::detach(class.get_id(), &course, &st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- grade blueprints ------------------------------------------------------

#[derive(Deserialize, ToSchema)]
struct CreateBlueprint {
    /// The grade label this template stocks — the same free text a class
    /// carries in `grade` ("9", "10-A"). It is the blueprint's own id, so it
    /// must be non-empty and free of `/ \ ? # %`.
    #[schema(max_length = 20, example = "9")]
    grade: String,
    /// The courses every class section at that grade takes.
    course_ids: Vec<String>,
}

#[derive(Deserialize, ToSchema)]
struct UpdateBlueprint {
    /// The whole new course list — a *set*, not a delta: courses missing from
    /// it are dropped from the blueprint and detached from the classes it
    /// attached them to.
    course_ids: Vec<String>,
}

#[derive(Serialize, ToSchema)]
struct BlueprintResponse {
    /// The grade label, which is also the blueprint's id in every path here.
    #[schema(example = "9")]
    grade: String,
    /// The course ids this grade's sections take.
    courses: Vec<String>,
    /// Who wrote the template.
    creator: PersonRef,
}

impl BlueprintResponse {
    fn new(
        blueprint: &ClassBlueprint,
        people: &std::collections::HashMap<String, PersonRef>,
    ) -> Self {
        Self {
            grade: blueprint.get_grade().as_str().to_string(),
            courses: blueprint
                .get_courses()
                .iter()
                .map(|course| course.key().to_string())
                .collect(),
            creator: PersonRef::resolve(people, blueprint.get_creator()),
        }
    }
}

/// One class a pump did *not* stock, and why. The class is named as well as
/// identified: a manager reading a skip list has to know which section is short
/// a course, and a bare ULID is not that.
#[derive(Serialize, ToSchema)]
struct SkipResponse {
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    class: String,
    #[schema(example = "9-C")]
    class_name: String,
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    course: String,
    /// Why that course could not be attached to that class, as a machine code
    /// the client words itself — the same id-plus-client-label shape roles and
    /// course kinds have. The set is closed: `class_deleted` (the section was
    /// deleted while the pump ran), `course_deleted` (the course itself was
    /// deleted while the pump ran — a course deleted before it started is
    /// already out of the template; the pump drops that id too, so this is
    /// reported once for the whole grade and never again),
    /// `class_at_course_ceiling` (the
    /// section already holds `max_class_courses`), `class_roster_too_large`
    /// (the section holds more students than one attach may enroll at once),
    /// `course_full` (the course has no free seat for the whole section), and
    /// `blueprint_deleted` (the blueprint itself was deleted while the pump
    /// ran; nothing was attached, and there is nothing left to retry — it ends
    /// the run, so it appears once however many sections were left).
    ///
    /// The **same vocabulary** answers the manual routes as their `code`
    /// field. `POST /classes/{id}/courses` attaches along this very axis and so
    /// answers this list's ceilings, plus `duplicate` — a refusal there (the
    /// call asked for a row and did not get it) but not here (the course is
    /// already on the class, which is what the blueprint asks for).
    /// `POST /classes/{id}/members` attaches along the *other* axis, so its two
    /// ceilings are the mirror ones, named for the ceiling actually hit:
    /// `class_at_roster_ceiling` (the section already holds
    /// `max_class_members`) and `class_course_list_too_large` (it carries more
    /// courses than one member add may enroll at once). It also owns
    /// `linked_course_missing` (another course already attached to that section
    /// no longer exists — detach it first): it is the one attach that walks the
    /// section's existing course links, while a pump attaches a course it has
    /// just proved alive.
    #[schema(example = "course_full")]
    reason: String,
}

impl SkipResponse {
    fn new(skip: &Skip) -> Self {
        Self {
            class: skip.class.key().to_string(),
            class_name: skip.class_name.clone(),
            course: skip.course.key().to_string(),
            reason: skip.reason.to_string(),
        }
    }
}

#[derive(Serialize, ToSchema)]
struct BlueprintPumpResponse {
    blueprint: BlueprintResponse,
    /// How many class sections this pump reached. **`0` means no section
    /// carries this grade label** — the template was saved and stocked
    /// nothing. A grade is free text and matched exactly, so `"9 "`, `"9-A"`
    /// and `"9"` are three different grades: check the label against the one
    /// the sections actually carry, since an empty `skipped` alone reads the
    /// same whether every section took the list or none was found.
    ///
    /// The sections *reached*, not the ones the grade holds: a
    /// `blueprint_deleted` skip ends the run, and this then counts the ones
    /// walked before it.
    #[schema(example = 12)]
    matched: i64,
    /// Every (class, course) pair this write could not place. Empty when every
    /// section it reached took the whole list. The blueprint itself was still
    /// saved — a pump is best-effort by design.
    skipped: Vec<SkipResponse>,
}

/// One section measured against its grade's template.
#[derive(Serialize, ToSchema)]
struct SectionStatusResponse {
    #[schema(example = "01J8XZ0K3Q8G7X2M4N5P6R7S8T")]
    class: String,
    #[schema(example = "9-C")]
    class_name: String,
    /// The template's courses this section does not carry, empty when it is in
    /// sync. A course attached **by hand** counts as carried, exactly as it
    /// does for the pump.
    missing: Vec<String>,
}

#[derive(Serialize, ToSchema)]
struct BlueprintStatusResponse {
    #[schema(example = "9")]
    grade: String,
    /// The template every section below is measured against.
    courses: Vec<String>,
    /// How many sections carry this grade label — the same count a pump
    /// reports, and `0` still means the label matches nothing rather than that
    /// every section is stocked.
    #[schema(example = 12)]
    matched: i64,
    /// Every section at the grade, in sync or not. Unpaged: it is the şube one
    /// school runs at one grade.
    sections: Vec<SectionStatusResponse>,
}

#[derive(Serialize, ToSchema)]
struct ApplyResponse {
    /// The courses this class could not take, empty when it took them all.
    skipped: Vec<SkipResponse>,
}

/// The courses a request names, each of which must exist. Order and duplicates
/// are the blueprint's to settle ([`ClassBlueprint`] deduplicates).
async fn resolve_courses(ids: &[String], db: &Database) -> Result<Vec<CourseId>, AppError> {
    let mut courses = Vec::with_capacity(ids.len());
    for id in ids {
        let course = CourseId::from_key(id);
        if Course::read(&course, db).await?.is_none() {
            return Err(AppError::Validation(ValidationError::Invalid {
                field: "course_ids",
                reason: "one of these courses does not exist",
            }));
        }
        courses.push(course);
    }
    Ok(courses)
}

/// The blueprint a path grade names, or a 404.
async fn blueprint_or_404(grade: &str, db: &Database) -> Result<ClassBlueprint, AppError> {
    ClassBlueprint::read(&ClassBlueprintId::from_key(grade), db)
        .await?
        .ok_or(AppError::NotFound)
}

async fn blueprint_body(
    blueprint: &ClassBlueprint,
    pumped: &Pumped,
    db: &Database,
) -> Result<BlueprintPumpResponse, AppError> {
    let people = person_map([blueprint.get_creator().clone()], db).await?;
    Ok(BlueprintPumpResponse {
        blueprint: BlueprintResponse::new(blueprint, &people),
        matched: pumped.matched,
        skipped: pumped.skipped.iter().map(SkipResponse::new).collect(),
    })
}

/// Create a grade's course blueprint and stock every class section already at
/// that grade with it. Requires manager+.
///
/// The pump is **best-effort**: a class that cannot take one of the courses (it
/// is at its own course ceiling, or the course has no free seat for the whole
/// section) is skipped and reported in `skipped`, while every other class is
/// still stocked. The blueprint is saved either way.
///
/// `matched` says how many sections it actually reached — `0` with an empty
/// `skipped` is a saved template that stocked nothing, which almost always
/// means the grade label does not match the one those sections carry.
#[utoipa::path(
    post,
    path = "/blueprints",
    tag = "classes",
    security(("session_cookie" = [])),
    request_body = CreateBlueprint,
    responses(
        (status = 201, description = "Blueprint created, with the number of sections it reached and the classes it could not stock", body = BlueprintPumpResponse),
        (status = 400, description = "Invalid or unaddressable grade, too many courses, or a course that does not exist", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 409, description = "A blueprint already exists for that grade", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create_blueprint(
    State(st): State<AppState>,
    RequireManager(user): RequireManager,
    Json(req): Json<CreateBlueprint>,
) -> Result<(StatusCode, Json<BlueprintPumpResponse>), AppError> {
    let grade = ClassBlueprint::grade_key(&req.grade)?;
    let courses = resolve_courses(&req.course_ids, &st.db).await?;
    let mut blueprint = ClassBlueprint::create(user.get_id(), grade, courses, &st.db).await?;
    let pumped = blueprint.pump(user.get_id(), &st.db).await?;
    let body = blueprint_body(&blueprint, &pumped, &st.db).await?;
    Ok((StatusCode::CREATED, Json(body)))
}

/// List every grade blueprint, by grade label. Requires manager+. Paged via
/// `?limit=&offset=` (omit `limit` for all of them).
#[utoipa::path(
    get,
    path = "/blueprints",
    tag = "classes",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "A page of blueprints (all of them when unpaged)", body = Page<BlueprintResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
    ),
)]
async fn list_blueprints(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<BlueprintResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let (blueprints, total) = ClassBlueprint::list_all(limit, offset, &st.db).await?;
    let people = person_map(
        blueprints
            .iter()
            .map(|blueprint| blueprint.get_creator().clone()),
        &st.db,
    )
    .await?;
    let items = blueprints
        .iter()
        .map(|blueprint| BlueprintResponse::new(blueprint, &people))
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Fetch one grade's blueprint. Requires manager+.
#[utoipa::path(
    get,
    path = "/blueprints/{grade}",
    tag = "classes",
    security(("session_cookie" = [])),
    params(("grade" = String, Path, description = "Grade label")),
    responses(
        (status = 200, description = "The blueprint", body = BlueprintResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "No blueprint for that grade", body = ErrorResponse),
    ),
)]
async fn get_blueprint(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(grade): Path<String>,
) -> Result<Json<BlueprintResponse>, AppError> {
    let blueprint = blueprint_or_404(&grade, &st.db).await?;
    let people = person_map([blueprint.get_creator().clone()], &st.db).await?;
    Ok(Json(BlueprintResponse::new(&blueprint, &people)))
}

/// Replace a blueprint's course list and reconcile every class section at that
/// grade with it. Requires manager+.
///
/// `course_ids` is the whole list, not a delta. A course dropped from it is
/// **detached** from the classes this blueprint attached it to (their pumped
/// enrollments swept the usual way) — but a course a human attached to a class
/// by hand carries no blueprint tag and is left exactly where it is. Courses
/// still in the list are pumped into every class at the grade that does not
/// already carry them.
///
/// The pump is **best-effort**: a class that cannot take a course is skipped
/// and reported in `skipped`, and the rest are still stocked. `matched` says
/// how many sections it reached, so a `0` tells a grade label nothing carries
/// apart from a list every section already had. A `409` means the list changed
/// since you read it — nothing was written; re-read and retry.
#[utoipa::path(
    patch,
    path = "/blueprints/{grade}",
    tag = "classes",
    security(("session_cookie" = [])),
    params(("grade" = String, Path, description = "Grade label")),
    request_body = UpdateBlueprint,
    responses(
        (status = 200, description = "Updated blueprint, with the number of sections it reached and the classes it could not stock", body = BlueprintPumpResponse),
        (status = 400, description = "Too many courses, or a course that does not exist", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "No blueprint for that grade", body = ErrorResponse),
        (status = 409, description = "The course list changed since the caller read it — nothing was written, re-read and retry", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn update_blueprint(
    State(st): State<AppState>,
    RequireManager(user): RequireManager,
    Path(grade): Path<String>,
    Json(req): Json<UpdateBlueprint>,
) -> Result<Json<BlueprintPumpResponse>, AppError> {
    let blueprint = blueprint_or_404(&grade, &st.db).await?;
    let courses = resolve_courses(&req.course_ids, &st.db).await?;
    let (saved, pumped) = blueprint
        .set_courses(courses, user.get_id(), &st.db)
        .await?;
    Ok(Json(blueprint_body(&saved, &pumped, &st.db).await?))
}

/// Delete a grade's blueprint. Requires manager+. Every attachment the
/// blueprint made is detached with it (their pumped enrollments swept the usual
/// way); a course a human attached to one of those classes by hand carries no
/// blueprint tag and survives.
///
/// A `409` means the course list changed between this call reading the
/// blueprint and deleting it — nothing was written; re-read and retry.
#[utoipa::path(
    delete,
    path = "/blueprints/{grade}",
    tag = "classes",
    security(("session_cookie" = [])),
    params(("grade" = String, Path, description = "Grade label")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "No blueprint for that grade", body = ErrorResponse),
        (status = 409, description = "The course list changed since this call read it — nothing was written, re-read and retry", body = ErrorResponse),
    ),
)]
async fn delete_blueprint(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(grade): Path<String>,
) -> Result<StatusCode, AppError> {
    blueprint_or_404(&grade, &st.db)
        .await?
        .delete(&st.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Which sections at a grade are out of sync with its blueprint. Requires
/// manager+. Reads only — nothing is attached, detached or pruned.
///
/// A pump's `skipped` list lives only in the response that reported it; this is
/// how a manager asks the same question afterwards. Every section carrying the
/// grade label is listed with the template courses it does not carry, so an
/// empty `missing` everywhere is a grade fully stocked.
///
/// A course a human attached by hand counts as carried, exactly as it does for
/// the pump — the template asks for the course, not for the pump's tag.
/// Deleting a course takes it out of every template naming it, so `missing`
/// names courses that still exist — only a row written before that cascade
/// existed can still show a dangling id.
///
/// The fix for anything listed is one of two idempotent calls:
/// `POST /classes/{id}/blueprint` for one section, or `PATCH` the blueprint
/// with the list it already holds to re-run the whole grade's pump.
#[utoipa::path(
    get,
    path = "/blueprints/{grade}/status",
    tag = "classes",
    security(("session_cookie" = [])),
    params(("grade" = String, Path, description = "Grade label")),
    responses(
        (status = 200, description = "Every section at the grade, with the template courses it is missing", body = BlueprintStatusResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "No blueprint for that grade", body = ErrorResponse),
    ),
)]
async fn blueprint_status(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(grade): Path<String>,
) -> Result<Json<BlueprintStatusResponse>, AppError> {
    let blueprint = blueprint_or_404(&grade, &st.db).await?;
    let sections = blueprint.status(&st.db).await?;
    Ok(Json(BlueprintStatusResponse {
        grade: blueprint.get_grade().as_str().to_string(),
        courses: blueprint
            .get_courses()
            .iter()
            .map(|course| course.key().to_string())
            .collect(),
        matched: sections.len() as i64,
        sections: sections
            .iter()
            .map(|section| SectionStatusResponse {
                class: section.class.key().to_string(),
                class_name: section.class_name.clone(),
                missing: section
                    .missing
                    .iter()
                    .map(|course| course.key().to_string())
                    .collect(),
            })
            .collect(),
    }))
}

/// Stock one class section from its grade's blueprint. Requires manager+ — this
/// writes the roster of every course in the template, which is the office's
/// call, not one course owner's.
///
/// Idempotent: a course the class already carries is left alone, whoever
/// attached it. Best-effort like every other pump — the courses that did not
/// fit come back in `skipped` and the rest are attached. A class with no grade,
/// or a grade no blueprint covers, is a 404.
#[utoipa::path(
    post,
    path = "/{id}/blueprint",
    tag = "classes",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Class id")),
    responses(
        (status = 200, description = "The class was stocked, minus the courses it could not take", body = ApplyResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "Class not found, or no blueprint covers its grade", body = ErrorResponse),
        (status = 409, description = "The class's (or course's) term is archived — past years are read-only", body = ErrorResponse),
    ),
)]
async fn apply_blueprint(
    State(st): State<AppState>,
    RequireManager(user): RequireManager,
    Path(id): Path<String>,
) -> Result<Json<ApplyResponse>, AppError> {
    let class = class_or_404(&id, &st.db).await?;
    let grade = class.get_grade().ok_or(AppError::NotFound)?;
    let blueprint = blueprint_or_404(grade.as_str(), &st.db).await?;
    // The pump writes both ends, so both are guarded — the class, and every
    // course the template would attach. A course the template names that is
    // already gone is the pump's own `skipped` business, not a term refusal.
    class.require_open(&st.db).await?;
    for id in blueprint.get_courses() {
        if let Some(course) = Course::read(id, &st.db).await? {
            course.require_open(&st.db).await?;
        }
    }
    let skipped = blueprint.apply_to(&class, user.get_id(), &st.db).await?;
    Ok(Json(ApplyResponse {
        skipped: skipped.iter().map(SkipResponse::new).collect(),
    }))
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

    /// The same nullable rule on the homeroom teacher, plus the bar it holds:
    /// absent and `""` are the same *no teacher* (and never touch the store),
    /// an id naming nobody is a `400`, and so is one naming a student — a
    /// class's sınıf öğretmeni is staff.
    #[tokio::test]
    async fn an_empty_teacher_is_no_teacher_and_a_student_is_never_one() {
        use crate::domain::role::Role;
        use crate::domain::user::{Password, User, Username};

        let db = crate::database::init_mem().await.unwrap();
        assert!(teacher_or_none(None, &db).await.unwrap().is_none());
        assert!(teacher_or_none(Some(""), &db).await.unwrap().is_none());

        let make = async |name: &str, role: Role| {
            let user = User::create(
                Username::try_new(name).unwrap(),
                Password::try_new("secret1")
                    .unwrap()
                    .hash_async()
                    .await
                    .unwrap(),
                &db,
            )
            .await
            .unwrap();
            user.set_role(role, &db).await.unwrap().0
        };
        let student = make("ali", Role::Student).await;
        let teacher = make("ada", Role::Teacher).await;

        for (field, id) in [
            ("gone", "01J8XZ0K3Q8G7X2M4N5P6R7S8T"),
            ("student", student.get_id().key()),
        ] {
            let refused = teacher_or_none(Some(id), &db).await;
            assert!(
                matches!(
                    refused,
                    Err(AppError::Validation(ValidationError::Invalid {
                        field: "teacher_id",
                        ..
                    }))
                ),
                "a {field} id must be a 400 naming teacher_id: {refused:?}"
            );
        }
        assert_eq!(
            teacher_or_none(Some(teacher.get_id().key()), &db)
                .await
                .unwrap()
                .map(|found| found.get_id().clone()),
            Some(teacher.get_id().clone()),
            "a teacher account is the one thing that resolves"
        );
    }
}
