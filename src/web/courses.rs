use crate::web::tenant_state::State;
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use std::collections::{HashMap, HashSet};

use crate::constant::MAX_COURSE_SECTIONS;
use crate::database::Database;
use crate::domain::class_course::{ClassCourse, DersSaati};
use crate::domain::class_group::{ClassGroup, ClassGroupId};
use crate::domain::course::{Course, CourseDescription, CourseId, CourseKind, CourseTitle};
use crate::domain::course_offering::CourseOfferingId;
use crate::domain::role::Role;
use crate::domain::subject::{SubjectDescription, SubjectName};
use crate::domain::text_fold::search_fold;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse};
use crate::service;
use crate::service::course::{can_manage_course, can_view_course};
use crate::state::AppState;

use super::dto::CourseSectionRef;
use super::{
    CourseResponse, CurrentUser, Page, PageParams, PersonRef, RequireTeacher, SubjectResponse,
    course_people, paginate, person_map, remove_blob,
};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_course, list_courses))
        .routes(routes!(my_courses))
        .routes(routes!(get_course, update_course, delete_course))
        .routes(routes!(join_member, list_members))
        .routes(routes!(leave_member))
}

// The route pair below is mounted under `/courses` but *belongs* to another
// module, so it is split out to carry that module's gate as well as the course
// one — see `crate::web::module_gate`. It is merged back in `build_router`, so
// the URL space is unchanged. The exam, session and homework pairs that used to
// sit here moved to `/instances/{id}/...` (see [`super::instances`]), because
// they hang off the instance, not off the catalog row.

pub fn subject_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(create_subject_in_course, list_course_subjects))
}

#[derive(Deserialize, ToSchema)]
struct CreateCourse {
    #[schema(max_length = 200, example = "Algebra")]
    title: String,
    #[schema(max_length = 2000)]
    description: Option<String>,
    /// `course` (a regular class — the default), `study` (a supervised study
    /// session), or `club` (a student club). Only a `course` is taught through
    /// class-section instances (and carries exams, sessions and homework); a
    /// `study`/`club` is joined school-wide.
    #[schema(example = "course")]
    kind: Option<String>,
}

#[derive(Deserialize, ToSchema)]
struct JoinMember {
    /// The user to add to the club or study.
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    user_id: String,
}

/// Public shape of one individual membership in a club or study — the
/// school-scoped tier, distinct from a class instance's roster.
#[derive(Serialize, ToSchema)]
struct MembershipResponse {
    #[schema(
        example = "019732e3-7b00-7000-8000-00000000dead_019732e3-7b00-7000-8000-00000000dead"
    )]
    id: String,
    /// The course joined (`GET /courses/{id}`).
    course: String,
    user: PersonRef,
    /// Who placed the membership.
    added_by: PersonRef,
    /// When, UTC unix-milliseconds.
    created_at: i64,
}

impl MembershipResponse {
    fn new(
        membership: &crate::domain::course_membership::CourseMembership,
        people: &std::collections::HashMap<String, PersonRef>,
    ) -> Self {
        Self {
            id: membership.get_id().key(),
            course: membership.get_course().key().to_string(),
            user: PersonRef::resolve(people, membership.get_user()),
            added_by: PersonRef::resolve(people, membership.get_added_by()),
            created_at: membership.get_created_at().as_millis(),
        }
    }
}

#[derive(Deserialize, ToSchema)]
struct UpdateCourse {
    #[schema(max_length = 200)]
    title: Option<String>,
    #[schema(max_length = 2000)]
    description: Option<String>,
    /// `course`, `study` (supervised study), or `club`. Omit to keep the current
    /// kind.
    kind: Option<String>,
}

/// Who may destroy a catalog row: its creator, or anyone `manager` and above.
/// Carries the same live-`teacher` floor as
/// [`crate::service::course::can_manage_course`], and for the same reason: a
/// demoted creator owns nothing.
fn owns_course(course: &Course, user: &User) -> bool {
    can_manage_course(course, user)
}

/// The catalog as one user sees it: every course for manager+, otherwise the
/// courses they are assigned to teach somewhere plus the ones they're enrolled
/// in, newest first.
///
/// The taught half carries the same live-`teacher` floor as
/// [`crate::service::course::can_manage_course`], and for the same reason:
/// `creator` is a historical column no demotion sweeps, so without it a
/// demoted creator kept seeing the course — and, through the `/exams` and
/// `/homework` catalogs that build on this list, its published exams and
/// homework. Below `teacher` a course is visible only the way it is to any
/// other student: by enrollment.
pub(crate) async fn visible_courses(user: &User, db: &Database) -> Result<Vec<Course>, AppError> {
    visible_courses_filtered(user, db, None, None, None).await
}

/// [`visible_courses`] narrowed by the `GET /courses` filters. The filters
/// ride *after* the visibility union and *before* any paging, so a filter can
/// only ever narrow what the caller already reaches and `total` is the
/// filtered length. For a manager the filters go into the catalog query
/// itself ([`crate::service::course::list_filtered`]); below manager the
/// union is Rust, so the same text fold filters the merged list in memory.
pub(crate) async fn visible_courses_filtered(
    user: &User,
    db: &Database,
    kind: Option<CourseKind>,
    q: Option<&str>,
    taught: Option<bool>,
) -> Result<Vec<Course>, AppError> {
    if user.get_role().at_least(Role::Manager) {
        return service::course::list_filtered(db, kind, q, taught).await;
    }
    let mut courses = if user.get_role().at_least(Role::Teacher) {
        service::course::list_for_teacher(db, user.get_id()).await?
    } else {
        Vec::new()
    };
    for course in service::course::list_enrolled(db, user.get_id(), None, 0)
        .await?
        .0
    {
        if !courses
            .iter()
            .any(|known| known.get_id() == course.get_id())
        {
            courses.push(course);
        }
    }
    // Both sources come newest-first; re-sort so the merged list is too.
    courses.sort_by_key(|course| std::cmp::Reverse(course.get_id().key()));
    // The same fold both sides of the manager arm's SQL uses, so a `q`
    // matches identically above and below manager; blank means no filter.
    let needle = q.map(|q| search_fold(q.trim())).filter(|q| !q.is_empty());
    courses.retain(|course| {
        kind.as_ref().is_none_or(|want| course.get_kind() == want)
            && taught.is_none_or(|taught| (course.get_class_course_count() > 0) == taught)
            && needle.as_deref().is_none_or(|needle| {
                search_fold(course.get_title().as_str()).contains(needle)
                    || search_fold(course.get_description().as_str()).contains(needle)
            })
    });
    Ok(courses)
}

/// The class sections teaching each of `courses`, keyed by course id — the
/// builder every catalogue row and every `/courses/me` row rides.
///
/// Batched end to end: one query per resource for the *whole* page, never one
/// per row and never one per section — the instance read
/// ([`crate::db::class_course::list_for_course_ids`]), the teacher links
/// ([`crate::db::class_course_teacher::into_instances`]), the classes
/// ([`crate::db::class_group::list_by_ids`]), the offerings behind the resolved
/// hours ([`crate::db::course_offering::list_by_ids`]), the resolved titles
/// ([`crate::service::instance_resolve::resolved_content`], itself two queries)
/// and the teachers' `PersonRef`s ([`person_map`], one query).
///
/// `viewer` is the narrowing rule: `Some(user)` for a non-manager keeps only
/// the sections that caller already reaches — [`reached_instances`], the same
/// set `GET /instances/me` serves plus their own enrollments — so a section
/// they cannot reach, and its roster size, teacher and class, never leak here.
/// `None` (manager+) sees every section. Sections per course are truncated to
/// [`MAX_COURSE_SECTIONS`]; the course row's own `class_course_count` carries
/// the untruncated total.
pub(crate) async fn sections_of(
    db: &Database,
    courses: &[Course],
    viewer: Option<&User>,
) -> Result<HashMap<String, Vec<CourseSectionRef>>, AppError> {
    let mut by_course: HashMap<String, Vec<CourseSectionRef>> = HashMap::new();
    if courses.is_empty() {
        return Ok(by_course);
    }
    let ids: Vec<CourseId> = courses.iter().map(|course| course.get_id().clone()).collect();
    let mut instances = crate::db::class_course::list_for_course_ids(db, &ids).await?;
    if let Some(viewer) = viewer {
        let reachable: HashSet<String> = reached_instances(db, viewer)
            .await?
            .into_iter()
            .map(|instance| instance.get_id().key())
            .collect();
        instances.retain(|instance| reachable.contains(&instance.get_id().key()));
    }
    let with_teachers = crate::db::class_course_teacher::into_instances(db, instances).await?;
    for section in section_refs(db, with_teachers).await? {
        by_course
            .entry(section.course.clone())
            .or_default()
            .push(section);
    }
    for sections in by_course.values_mut() {
        sections.truncate(MAX_COURSE_SECTIONS);
    }
    Ok(by_course)
}

/// The class sections the caller is reached by, one flat list — the shape
/// `/courses/me` serves (it lists sections, not catalog rows, so there is no
/// grouping to do). Rides [`reached_instances`] (class roster, homeroom,
/// taught, or their own enrollment) and [`section_refs`] like [`sections_of`]
/// does, so the surfaces cannot disagree about a section's resolved title or
/// hours.
pub(crate) async fn reached_sections(
    db: &Database,
    user: &User,
) -> Result<Vec<CourseSectionRef>, AppError> {
    let instances = reached_instances(db, user).await?;
    let with_teachers = crate::db::class_course_teacher::into_instances(db, instances).await?;
    section_refs(db, with_teachers).await
}

/// The instances `user` reaches, as rows: the class-roster / homeroom / taught
/// set ([`crate::service::instance::visible_instances`]) unioned with the
/// instances their own **enrollment** rows place them on, de-duplicated by
/// instance id and kept newest-first.
///
/// The second half is not redundant. An enrollment row is the *instance*
/// roster, and a hand-placed one carries no class membership at all (a class
/// row an operator later disowned is the same shape), so the class-keyed
/// visibility set never sees it — yet that section is theirs, and both
/// `/courses/me` and the profile course block listed it before the section
/// split. Two reads for the union (the enrollment list plus one batched
/// instance read of whatever it added), never one per row.
pub(crate) async fn reached_instances(
    db: &Database,
    user: &User,
) -> Result<Vec<ClassCourse>, AppError> {
    let mut instances: Vec<ClassCourse> = crate::service::instance::visible_instances(user, db)
        .await?
        .into_iter()
        .map(|(instance, _)| instance)
        .collect();
    let known: HashSet<String> = instances
        .iter()
        .map(|instance| instance.get_id().key())
        .collect();
    let missing: Vec<crate::domain::class_course::ClassCourseId> =
        crate::db::enrollment::list_for_user(db, user.get_id())
            .await?
            .iter()
            .map(|enrollment| enrollment.get_class_course().clone())
            .filter(|id| !known.contains(&id.key()))
            .collect();
    // `list_by_ids` orders `attached_at DESC, id DESC` — the same order the
    // class arm of `visible_instances` produces, so the union reads newest-first
    // end to end.
    instances.extend(crate::db::class_course::list_by_ids(db, &missing).await?);
    Ok(instances)
}

/// Build one [`CourseSectionRef`] per `(instance, teachers)` pair — the shared
/// tail of [`sections_of`] and [`reached_sections`]: the classes, offerings,
/// resolved titles and people are read once for the whole batch, then each
/// section is assembled in memory.
pub(crate) async fn section_refs(
    db: &Database,
    rows: Vec<(ClassCourse, Vec<UserId>)>,
) -> Result<Vec<CourseSectionRef>, AppError> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let mut class_ids: Vec<ClassGroupId> = Vec::new();
    let mut offering_ids: Vec<CourseOfferingId> = Vec::new();
    for (instance, _) in &rows {
        if !class_ids.contains(instance.get_class()) {
            class_ids.push(instance.get_class().clone());
        }
        if !offering_ids.contains(instance.get_offering()) {
            offering_ids.push(instance.get_offering().clone());
        }
    }
    let classes = crate::db::class_group::list_by_ids(db, &class_ids).await?;
    let class_by_key: HashMap<String, &ClassGroup> = classes
        .iter()
        .map(|class| (class.get_id().key(), class))
        .collect();
    // The resolved weekly hours chain is `resolve_policy`'s — instance
    // override, else the offering's default, else the floor — but its offering
    // half is read once for the batch instead of once per section.
    let offerings = crate::db::course_offering::list_by_ids(db, &offering_ids).await?;
    let hours_by_offering: HashMap<String, Option<DersSaati>> = offerings
        .iter()
        .map(|offering| {
            (
                offering.get_id().key(),
                offering.get_default_ders_saati(),
            )
        })
        .collect();
    let refs: Vec<&ClassCourse> =
        rows.iter().map(|(instance, _)| instance).collect();
    let content = crate::service::instance_resolve::resolved_content(db, &refs).await?;
    let people = person_map(
        rows.iter()
            .flat_map(|(_, teachers)| teachers.iter().cloned()),
        db,
    )
    .await?;
    let mut sections = Vec::with_capacity(rows.len());
    for (instance, teachers) in &rows {
        let key = instance.get_id().key();
        let resolved = content
            .get(&key)
            .expect("resolved_content covers every instance it is given");
        let class = class_by_key
            .get(&instance.get_class().key())
            .expect("every section's class is in the batch read");
        let ders_saati = instance
            .get_ders_saati()
            .or_else(|| {
                hours_by_offering
                    .get(&instance.get_offering().key())
                    .copied()
                    .flatten()
            })
            .unwrap_or_else(|| {
                DersSaati::try_new(crate::constant::MIN_DERS_SAATI)
                    .expect("the floor is a valid weekly-hours count")
            });
        sections.push(CourseSectionRef {
            id: key,
            course: instance.get_course().key(),
            class: instance.get_class().key(),
            class_name: class.get_name().as_str().to_string(),
            title: resolved.title.clone(),
            grade_level: class.get_grade_level().get(),
            ders_saati: ders_saati.as_i64(),
            teachers: teachers
                .iter()
                .map(|teacher| PersonRef::resolve(&people, teacher))
                .collect(),
            enrollment_count: instance.get_enrollment_count(),
        });
    }
    Ok(sections)
}

// ---- courses ------------------------------------------------------------

/// Create a catalog course owned by the current user. Requires the `teacher`
/// role or higher. `kind` picks the flavor — `course` (a regular class, the
/// default), `study` (a supervised study session), or `club` (a student
/// club). A catalog row teaches nobody by itself: a class section attaches it
/// into an instance (`POST /classes/{id}/instances`), and a `study`/`club` is
/// joined school-wide (`POST /courses/{id}/members`).
#[utoipa::path(
    post,
    path = "/",
    tag = "courses",
    security(("session_cookie" = [])),
    request_body = CreateCourse,
    responses(
        (status = 201, description = "Course created", body = CourseResponse),
        (status = 400, description = "Invalid fields or kind", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create_course(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Json(req): Json<CreateCourse>,
) -> Result<(StatusCode, Json<CourseResponse>), AppError> {
    let title = CourseTitle::try_new(&req.title)?;
    let description = CourseDescription::try_new(&req.description.unwrap_or_default())?;
    let kind = match req.kind {
        Some(ref kind) => CourseKind::try_new(kind)?,
        None => CourseKind::course(),
    };
    let course = service::course::create(&st.db, user.get_id(), title, description, kind).await?;
    // The creator is the caller — already loaded, no extra lookup.
    let people = PersonRef::map_of(&[&user]);
    Ok((
        StatusCode::CREATED,
        Json(CourseResponse::new(&course, &people)),
    ))
}

/// The `GET /courses` filters, both optional and combinable. They narrow the
/// caller's visible set — they never widen it: a student filtering by `kind`
/// still sees only the courses they reach.
#[derive(Debug, Deserialize, IntoParams)]
struct CourseListFilter {
    /// Narrow to one kind: `course` (a regular class), `study` (a supervised
    /// study session), or `club`. Omit for every kind.
    #[param(example = "course")]
    kind: Option<String>,
    /// Case- and diacritic-insensitive fragment of the title or description.
    /// Omit — or leave blank — to filter nothing; `%` and `_` are literal.
    #[param(example = "matematik")]
    q: Option<String>,
    /// Keep only courses taught somewhere (`true`, `class_course_count > 0`)
    /// or nowhere yet (`false`, `= 0`). Omit for both.
    #[param(example = true)]
    taught: Option<bool>,
}

/// The parsed [`CourseListFilter`] — one named field per query parameter,
/// ready for [`visible_courses_filtered`].
#[derive(Debug)]
struct ResolvedCourseFilter {
    kind: Option<CourseKind>,
    q: Option<String>,
    taught: Option<bool>,
}

impl CourseListFilter {
    /// `kind` parses through the write-path validator, so an unknown or empty
    /// spelling is the same 400 a create/PATCH gets; `taught` needs no parse —
    /// the query extractor itself 400s a `?taught=` that is not `true`/`false`,
    /// naming the field; `q` passes through raw — each arm trims, folds and
    /// blanks it identically.
    fn resolve(self) -> Result<ResolvedCourseFilter, AppError> {
        let kind = match self.kind {
            Some(kind) => Some(CourseKind::try_new(&kind)?),
            None => None,
        };
        Ok(ResolvedCourseFilter {
            kind,
            q: self.q,
            taught: self.taught,
        })
    }
}

/// List the catalog courses visible to the caller: every course for manager+,
/// otherwise the courses they teach somewhere plus the ones they're enrolled
/// in. Paged via `?limit=&offset=` (omit `limit` for the full list); returns a
/// `{items, total, limit, offset}` envelope.
///
/// `kind` narrows to one flavor, `q` filters by a case- and
/// diacritic-insensitive fragment of the title or description (blank =
/// unfiltered), and `taught` keeps only the courses a class section teaches
/// (`true`) or none yet (`false`). All apply to the caller's visible set
/// only — after the visibility gate and before the window is cut — so
/// `total` is the filtered length.
#[utoipa::path(
    get,
    path = "/",
    tag = "courses",
    security(("session_cookie" = [])),
    params(CourseListFilter, PageParams),
    responses(
        (status = 200, description = "A page of the caller's visible courses (all of them when unpaged)", body = Page<CourseResponse>),
        (status = 400, description = "Unknown or empty kind, a `taught` that is not true/false, or invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn list_courses(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(filter): Query<CourseListFilter>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<CourseResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let filters = filter.resolve()?;
    // The filters ride inside the visible set — SQL for a manager, the
    // union's tail in Rust below one — so total is the filtered length
    // before the window is cut.
    let courses =
        visible_courses_filtered(&user, &st.db, filters.kind, filters.q.as_deref(), filters.taught)
            .await?;
    let total = courses.len() as i64;
    // Paged in the web layer: the visible set is a Rust union of two lists.
    let window = paginate(&courses, limit, offset);
    // Join creators onto the page alone — the lookup shrinks with the window.
    let people = person_map(window.iter().flat_map(course_people), &st.db).await?;
    // The sections of every row on the page, batched: one round of queries for
    // the whole page, never one per row. A non-manager sees only the sections
    // they already reach (their şubeler, what they teach, their enrollments).
    let viewer = (!user.get_role().at_least(Role::Manager)).then_some(&user);
    let mut sections = sections_of(&st.db, window, viewer).await?;
    // Deliberate exception to the instance-resolved display rule: this
    // surface IS the ders catalog, so each *row* shows its own catalog
    // title/description — but every row now carries the class sections
    // teaching it, each with the section's own resolved title and hours (the
    // grade templates (`/offerings`) and the per-class instances
    // (`/instances`) resolve from it; they never edit it).
    let items = window
        .iter()
        .map(|course| {
            let own = sections.remove(&course.get_id().key()).unwrap_or_default();
            CourseResponse::with_sections(course, &people, own)
        })
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// The catalog courses the current user is reached by, paged via
/// `?limit=&offset=` (omit `limit` for all of them); returns a
/// `{items, total, limit, offset}` envelope.
///
/// One row per **class section** the caller is reached by — the resolved
/// title, hours, class and teachers of that section, with the *instance* id.
/// The set is `GET /instances/me`'s (class roster, homeroom, taught) unioned
/// with the caller's own enrollment rows, so a section is listed exactly when
/// they reach it at all: by their şube, by running it, or by being placed on
/// its roster by hand.
#[utoipa::path(
    get,
    path = "/me",
    tag = "courses",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "A page of the caller's sections (all of them when unpaged)", body = Page<CourseSectionRef>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn my_courses(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<CourseSectionRef>>, AppError> {
    let (limit, offset) = page.resolve()?;
    // The section set is a live Rust list (the visibility union), so it is
    // paged here — the `visible_courses` shape.
    let sections = reached_sections(&st.db, &user).await?;
    let total = sections.len() as i64;
    let window = paginate(&sections, limit, offset);
    Ok(Json(Page::new(window.to_vec(), total, limit, offset)))
}

/// Fetch a single catalog course by id. Visible to the people it reaches —
/// students enrolled in any of its instances, members of the course itself —
/// and to its creator and managers/admins while those accounts are still
/// `teacher`+.
#[utoipa::path(
    get,
    path = "/{id}",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    responses(
        (status = 200, description = "The course", body = CourseResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not reached by this course, and not a still-`teacher`+ course creator or manager/admin", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
    ),
)]
async fn get_course(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<CourseResponse>, AppError> {
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_view_course(&course, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only a student enrolled in one of this course's instances, a member of it, its creator, or a manager/admin can view this course",
        ));
    }
    let people = person_map(course_people(&course), &st.db).await?;
    let viewer = (!user.get_role().at_least(Role::Manager)).then_some(&user);
    let mut sections = sections_of(&st.db, std::slice::from_ref(&course), viewer).await?;
    let own = sections
        .remove(&course.get_id().key())
        .unwrap_or_default();
    Ok(Json(CourseResponse::with_sections(&course, &people, own)))
}

/// Update a catalog course. Requires teacher+ and catalog rights — its
/// creator, or a manager/admin. Omitted fields keep their value.
#[utoipa::path(
    patch,
    path = "/{id}",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    request_body = UpdateCourse,
    responses(
        (status = 200, description = "Updated course", body = CourseResponse),
        (status = 400, description = "Invalid fields or kind", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn update_course(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<UpdateCourse>,
) -> Result<Json<CourseResponse>, AppError> {
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can edit this course",
        ));
    }

    // Only what the request actually carried is validated and written — an
    // omitted field stays `None` so the save never re-sends this snapshot's
    // value over a concurrent PATCH of that field.
    let title = req.title.as_deref().map(CourseTitle::try_new).transpose()?;
    let description = req
        .description
        .as_deref()
        .map(CourseDescription::try_new)
        .transpose()?;
    let kind = req.kind.as_deref().map(CourseKind::try_new).transpose()?;

    let updated = service::course::update(&st.db, course, title, description, kind).await?;
    let people = person_map(course_people(&updated), &st.db).await?;
    Ok(Json(CourseResponse::new(&updated, &people)))
}

/// Delete a catalog course. Requires teacher+; only its creator or a
/// manager/admin may delete it. Refused with a 409 while the course is still
/// taught anywhere — detach it from every class section (`DELETE
/// /classes/{id}/instances/{instance}`) and remove its individual members
/// first, so a course that carries teaching is never dropped by accident.
/// Once free, it cascades the instances' exams (with their results, questions,
/// answers, and question images), homework (with submissions, submission
/// files, and grades), sessions and roll call, its individual memberships, its
/// subjects, and the teacher links. It also strikes its id out of every class
/// blueprint that named it — a template holding a course nothing can resolve
/// is a stocking run that skips it and a `PATCH` that refuses the very list
/// the template already holds.
#[utoipa::path(
    delete,
    path = "/{id}",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 409, description = "A class still teaches this course, or students still hold an individual membership in it", body = ErrorResponse),
    ),
)]
async fn delete_course(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !owns_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can delete this course",
        ));
    }
    // The workflow — blob-key collection and the guarded cascade — is
    // [`service::course::delete`]'s.
    // Blob unlinking stays here because only the web layer knows `files_path`.
    let outcome = service::course::delete(&st.db, &course).await?;
    if !outcome.deleted {
        return Err(AppError::Conflict(
            "a class still teaches this course, or students still hold an individual \
             membership in it — detach the classes and remove the members first",
        ));
    }
    for file in outcome
        .image_files
        .iter()
        .chain(&outcome.answer_image_files)
        .chain(&outcome.homework_files)
        .chain(&outcome.course_note_files)
    {
        remove_blob(&st.files_path, file).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---- club/study membership -------------------------------------------------

/// Add a user to a club or study — the **school-scoped** membership tier.
/// Requires teacher+ and catalog rights (its creator, or a manager/admin).
/// Only students can be added, and only to a `study` (supervised study) or
/// `club`: a regular course (`kind` `course`) has no school-wide roster — its
/// students come from the class sections that teach it, and that join is
/// `POST /instances/{id}/enrollments` (400 here). Idempotent: a pair that
/// already holds a membership is returned as-is.
#[utoipa::path(
    post,
    path = "/{id}/members",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    request_body = JoinMember,
    responses(
        (status = 200, description = "Member added (or already a member)", body = MembershipResponse),
        (status = 400, description = "Unknown user, user is not a student, or the course is a class-delivered ders", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn join_member(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<JoinMember>,
) -> Result<Json<MembershipResponse>, AppError> {
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can add members to this course",
        ));
    }
    let target = UserId::from_key(&req.user_id);
    let membership =
        service::enrollment::join_activity(&st.db, course.get_id(), &target, user.get_id()).await?;
    let people = person_map([target, *user.get_id()], &st.db).await?;
    Ok(Json(MembershipResponse::new(&membership, &people)))
}

/// List a club/study's members, newest first, paged via `?limit=&offset=`
/// (omit `limit` for the whole list). Requires teacher+ and catalog rights.
/// Returns a `{items, total, limit, offset}` envelope. An instance's roster
/// is `GET /instances/{id}/enrollments`.
#[utoipa::path(
    get,
    path = "/{id}/members",
    tag = "courses",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id"), PageParams),
    responses(
        (status = 200, description = "A page of members (the whole list when unpaged)", body = Page<MembershipResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
    ),
)]
async fn list_members(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<MembershipResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can list this course's members",
        ));
    }
    let (rows, total) =
        crate::db::course_membership::list_for_course(&st.db, course.get_id(), limit, offset)
            .await?;
    // Join people onto the page alone — the lookup shrinks with the window.
    let people = person_map(
        rows.iter()
            .flat_map(|row| [*row.get_user(), *row.get_added_by()]),
        &st.db,
    )
    .await?;
    let items = rows
        .iter()
        .map(|row| MembershipResponse::new(row, &people))
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// Remove a user from a club or study. Requires teacher+ and catalog rights.
/// Existing exam results and badges are untouched — the membership is a door,
/// not a record of what happened inside. A pair holding no membership is a
/// 404.
#[utoipa::path(
    delete,
    path = "/{id}/members/{user}",
    tag = "courses",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Course id"),
        ("user" = String, Path, description = "User id"),
    ),
    responses(
        (status = 204, description = "Removed"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Course not found, or that user held no membership", body = ErrorResponse),
    ),
)]
async fn leave_member(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, target)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can remove members from this course",
        ));
    }
    service::enrollment::leave_activity(&st.db, course.get_id(), &UserId::from_key(&target))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- subjects in a course ---------------------------------------------------
// The course's curriculum topics. Every exam question links to one, so the
// list doubles as the tag picker when authoring questions.

#[derive(Deserialize, ToSchema)]
struct CreateSubject {
    #[schema(max_length = 200, example = "Limits and continuity")]
    name: String,
    #[schema(max_length = 2000)]
    description: Option<String>,
}

/// Create a subject inside a course. Requires teacher+ and catalog rights
/// (its creator, or a manager/admin). Subjects are the curriculum topics of the
/// *catalog* row — every exam of every instance teaching it tags its questions
/// with one of them.
#[utoipa::path(
    post,
    path = "/{id}/subjects",
    tag = "subjects",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id")),
    request_body = CreateSubject,
    responses(
        (status = 201, description = "Subject created", body = SubjectResponse),
        (status = 400, description = "Invalid name or description", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not the course creator (and not a manager/admin)", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create_subject_in_course(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<CreateSubject>,
) -> Result<(StatusCode, Json<SubjectResponse>), AppError> {
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_manage_course(&course, &user) {
        return Err(AppError::Forbidden(
            "only the course creator or a manager/admin can add subjects to this course",
        ));
    }

    let name = SubjectName::try_new(&req.name)?;
    let description = SubjectDescription::try_new(&req.description.unwrap_or_default())?;
    let subject = service::subject::create(&st.db, course.get_id(), name, description).await?;
    Ok((StatusCode::CREATED, Json(SubjectResponse::new(&subject))))
}

/// List a course's subjects in creation order, paged via `?limit=&offset=`
/// (omit `limit` for all of them). Visible to its creator, to managers/admins,
/// and to anyone the course reaches (a student enrolled in one of its
/// instances, or a member of it). Returns a `{items, total, limit, offset}`
/// envelope.
#[utoipa::path(
    get,
    path = "/{id}/subjects",
    tag = "subjects",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Course id"), PageParams),
    responses(
        (status = 200, description = "A page of the course's subjects (all of them when unpaged)", body = Page<SubjectResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not reached by the course, and not its creator or a manager/admin", body = ErrorResponse),
        (status = 404, description = "Course not found", body = ErrorResponse),
    ),
)]
async fn list_course_subjects(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<SubjectResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    // Course must exist — a missing course is a 404, not an empty list.
    let course = service::course::read(&st.db, &CourseId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !can_view_course(&course, &user, &st.db).await? {
        return Err(AppError::Forbidden(
            "only a user this course reaches, its creator, or a manager/admin can view this course",
        ));
    }
    let (subjects, total) =
        service::subject::list_for_course(&st.db, course.get_id(), limit, offset).await?;
    let items = subjects.iter().map(SubjectResponse::new).collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_test_db;
    use crate::domain::user::Username;

    /// A user at `role`, minted through the real create path.
    async fn user(username: &str, role: Role, db: &Database) -> User {
        let user = crate::service::user::create(db, Username::try_new(username).unwrap(), None)
            .await
            .unwrap();
        crate::service::user::set_role(db, user.get_id(), role)
            .await
            .unwrap()
            .0
    }

    /// A catalog course `creator` made.
    async fn course(creator: &User, db: &Database) -> Course {
        service::course::create(
            db,
            creator.get_id(),
            CourseTitle::try_new("Matematik").unwrap(),
            CourseDescription::try_new("").unwrap(),
            CourseKind::try_new("course").unwrap(),
        )
        .await
        .unwrap()
    }

    /// A catalog course with caller-chosen fields, minted through the real
    /// create path.
    async fn shaped_course(
        creator: &User,
        db: &Database,
        title: &str,
        description: &str,
        kind: &str,
    ) -> Course {
        service::course::create(
            db,
            creator.get_id(),
            CourseTitle::try_new(title).unwrap(),
            CourseDescription::try_new(description).unwrap(),
            CourseKind::try_new(kind).unwrap(),
        )
        .await
        .unwrap()
    }

    fn titles(courses: &[Course]) -> Vec<&str> {
        courses
            .iter()
            .map(|course| course.get_title().as_str())
            .collect()
    }

    fn a_kind(kind: &str) -> CourseKind {
        CourseKind::try_new(kind).unwrap()
    }

    /// An unknown or empty `kind` is the write path's own 400, naming the
    /// field; `q` passes through verbatim (each arm folds it).
    #[tokio::test]
    async fn the_kind_filter_parses_through_the_write_path() {
        for kind in ["nope", ""] {
            let err = CourseListFilter {
                kind: Some(kind.to_string()),
                q: None,
                taught: None,
            }
            .resolve()
            .unwrap_err();
            assert!(
                matches!(
                    &err,
                    AppError::Validation(crate::error::ValidationError::Invalid { field, .. })
                        if *field == "kind"
                ),
                "{kind:?}: {err:?}"
            );
        }
        let resolved = CourseListFilter {
            kind: Some("club".to_string()),
            q: Some("  ".to_string()),
            taught: None,
        }
        .resolve()
        .unwrap();
        assert_eq!(resolved.kind.as_ref().map(CourseKind::as_str), Some("club"));
        assert_eq!(resolved.q.as_deref(), Some("  "));
        assert_eq!(resolved.taught, None);
    }

    /// Manager arm: kind and q narrow the catalog, total is the filtered
    /// length, a miss is an empty list, and `%`/`_` are literal.
    #[tokio::test]
    async fn kind_and_q_narrow_the_manager_catalog() {
        let (db, _leases) = init_test_db().await;
        let boss = user("boss", Role::Manager, &db).await;
        shaped_course(&boss, &db, "Matematik", "Cebir ve Geometri", "course").await;
        shaped_course(&boss, &db, "Satranç Kulübü", "", "club").await;
        shaped_course(&boss, &db, "Biyoloji Etüdü", "canlı yayın", "study").await;
        // "Matematik" is taught by a real class section; the other two are
        // attached nowhere, so the taught axis has both values in play.
        let class = crate::db::class_group::create(
            &db,
            boss.get_id(),
            crate::domain::class_group::ClassName::try_new("9-A").unwrap(),
            crate::domain::grade::GradeLevel::new(9).unwrap(),
            None,
            None,
        )
        .await
        .unwrap();
        let matematik = visible_courses_filtered(&boss, &db, None, None, None)
            .await
            .unwrap()
            .into_iter()
            .find(|course| course.get_title().as_str() == "Matematik")
            .unwrap();
        service::class_course::attach(&db, class.get_id(), matematik.get_id(), boss.get_id())
            .await
            .unwrap();

        // No filters: the whole catalog, newest first — today's response,
        // taught and untaught alike.
        let all = visible_courses_filtered(&boss, &db, None, None, None)
            .await
            .unwrap();
        assert_eq!(
            titles(&all),
            vec!["Biyoloji Etüdü", "Satranç Kulübü", "Matematik"]
        );

        // kind: exact flavor only.
        let clubs = visible_courses_filtered(&boss, &db, Some(a_kind("club")), None, None)
            .await
            .unwrap();
        assert_eq!(titles(&clubs), vec!["Satranç Kulübü"]);

        // q: title fragment, case- and diacritic-insensitive.
        let q = visible_courses_filtered(&boss, &db, None, Some("SATRANÇ"), None)
            .await
            .unwrap();
        assert_eq!(titles(&q), vec!["Satranç Kulübü"]);
        // q: description fragment too.
        let q = visible_courses_filtered(&boss, &db, None, Some("canlı"), None)
            .await
            .unwrap();
        assert_eq!(titles(&q), vec!["Biyoloji Etüdü"]);
        // q: `position`, not `LIKE` — no wildcard surprises.
        for wildcard in ["%", "_"] {
            let q = visible_courses_filtered(&boss, &db, None, Some(wildcard), None)
                .await
                .unwrap();
            assert!(q.is_empty(), "{wildcard:?} acted as a wildcard");
        }

        // Combined filters AND; a miss is 200 empty, not an error.
        let miss =
            visible_courses_filtered(&boss, &db, Some(a_kind("course")), Some("satranç"), None)
                .await
                .unwrap();
        assert!(miss.is_empty());
        let hit = visible_courses_filtered(&boss, &db, Some(a_kind("study")), Some("CANLI"), None)
            .await
            .unwrap();
        assert_eq!(titles(&hit), vec!["Biyoloji Etüdü"]);

        // Blank q is no filter — the unfiltered catalog again.
        let blank = visible_courses_filtered(&boss, &db, None, Some("   "), None)
            .await
            .unwrap();
        assert_eq!(titles(&blank), titles(&all));

        // taught: true keeps the attached one, false the unattached two, and
        // it composes with kind.
        let taught = visible_courses_filtered(&boss, &db, None, None, Some(true))
            .await
            .unwrap();
        assert_eq!(titles(&taught), vec!["Matematik"]);
        let untaught = visible_courses_filtered(&boss, &db, None, None, Some(false))
            .await
            .unwrap();
        assert_eq!(titles(&untaught), vec!["Biyoloji Etüdü", "Satranç Kulübü"]);
        let clubs = visible_courses_filtered(&boss, &db, Some(a_kind("club")), None, Some(false))
            .await
            .unwrap();
        assert_eq!(titles(&clubs), vec!["Satranç Kulübü"]);
        let miss = visible_courses_filtered(&boss, &db, Some(a_kind("club")), None, Some(true))
            .await
            .unwrap();
        assert!(miss.is_empty());

        // The window is cut after the filter: one course per page while the
        // total stays the filtered length (the handler's `total` is
        // `courses.len()` taken *before* `paginate`).
        let dersler = visible_courses_filtered(&boss, &db, Some(a_kind("course")), None, None)
            .await
            .unwrap();
        assert_eq!(dersler.len(), 1);
        assert_eq!(paginate(&dersler, Some(1), 0).len(), 1);
    }

    /// Below manager the filters ride the Rust union: they can narrow what a
    /// student reaches but never widen it.
    #[tokio::test]
    async fn filters_narrow_but_never_widen_a_students_visible_set() {
        let (db, _leases) = init_test_db().await;
        let boss = user("boss2", Role::Manager, &db).await;
        let student = user("ogrenci", Role::Student, &db).await;
        let (instance, _ders) = crate::db::course::a_test_instance(&db).await;
        shaped_course(&boss, &db, "Satranç Kulübü", "zeka oyunu", "club").await;
        service::enrollment::enroll(&db, &instance, student.get_id(), boss.get_id())
            .await
            .unwrap();

        // Unfiltered: only the enrolled ders — the club is real but not theirs.
        let mine = visible_courses_filtered(&student, &db, None, None, None)
            .await
            .unwrap();
        assert_eq!(titles(&mine), vec!["test course"]);

        // kind=club: the club exists in the catalog, the student still sees none.
        let clubs = visible_courses_filtered(&student, &db, Some(a_kind("club")), None, None)
            .await
            .unwrap();
        assert!(clubs.is_empty());
        // q over the club's own title finds nothing they cannot reach either.
        let q = visible_courses_filtered(&student, &db, None, Some("satranç"), None)
            .await
            .unwrap();
        assert!(q.is_empty());

        // taught: their ders is taught (the attached instance), the club is
        // not — the axis narrows both ways without widening anything.
        let taught = visible_courses_filtered(&student, &db, None, None, Some(true))
            .await
            .unwrap();
        assert_eq!(titles(&taught), vec!["test course"]);
        let untaught = visible_courses_filtered(&student, &db, None, None, Some(false))
            .await
            .unwrap();
        assert!(untaught.is_empty());

        // Their own ders survives both filters.
        let dersler = visible_courses_filtered(&student, &db, Some(a_kind("course")), None, None)
            .await
            .unwrap();
        assert_eq!(titles(&dersler), vec!["test course"]);
        let q = visible_courses_filtered(&student, &db, None, Some("TEST"), None)
            .await
            .unwrap();
        assert_eq!(titles(&q), vec!["test course"]);
    }

    /// `?taught=` present but empty is refused by the query extractor itself —
    /// a 400 naming the field; only `true`/`false` parse.
    #[test]
    fn an_empty_taught_is_rejected_naming_the_field() {
        let uri = axum::http::Uri::from_static("/courses?taught=");
        let err = Query::<CourseListFilter>::try_from_uri(&uri).unwrap_err();
        assert!(err.to_string().contains("taught"), "{err}");
        for ok in [
            "/courses?taught=true",
            "/courses?taught=false",
            "/courses?q=x",
            "/courses",
        ] {
            let uri = axum::http::Uri::from_static(ok);
            assert!(
                Query::<CourseListFilter>::try_from_uri(&uri).is_ok(),
                "{ok:?} refused"
            );
        }
    }

    /// The leak: `creator` is a historical column that demotion never sweeps,
    /// so the grant itself has to re-read the live role on every call.
    #[tokio::test]
    async fn demoted_creator_loses_management_and_ownership() {
        let (db, _leases) = init_test_db().await;
        let creator = user("teacher", Role::Teacher, &db).await;
        let course = course(&creator, &db).await;
        assert!(can_manage_course(&course, &creator));
        assert!(owns_course(&course, &creator));

        for role in [Role::Student, Role::Parent] {
            let demoted = crate::service::user::set_role(&db, creator.get_id(), role)
                .await
                .unwrap()
                .0;
            assert!(
                !can_manage_course(&course, &demoted),
                "{role:?} creator still manages the course"
            );
            assert!(
                !owns_course(&course, &demoted),
                "{role:?} creator still owns the course"
            );
        }
    }

    /// No course is left orphaned by the floor: manager+ reaches a course whose
    /// creator was demoted.
    #[tokio::test]
    async fn manager_still_manages_a_demoted_creators_course() {
        let (db, _leases) = init_test_db().await;
        let creator = user("teacher", Role::Teacher, &db).await;
        let course = course(&creator, &db).await;
        crate::service::user::set_role(&db, creator.get_id(), Role::Student)
            .await
            .unwrap();

        for role in [Role::Manager, Role::Admin] {
            let boss = user(&format!("boss{}", role.as_str()), role, &db).await;
            assert!(can_manage_course(&course, &boss), "{role:?} locked out");
            assert!(owns_course(&course, &boss), "{role:?} cannot delete");
        }
    }

    /// The assignment list is off the catalog (D6): a teacher assigned to an
    /// instance runs that instance, but holds no rights over the catalog row
    /// itself — the gate that grants them the instance is
    /// [`super::instances::can_manage_instance`].
    #[tokio::test]
    async fn an_instance_teacher_holds_no_catalog_rights() {
        let (db, _leases) = init_test_db().await;
        let creator = user("owner", Role::Manager, &db).await;
        let assigned = user("assigned", Role::Teacher, &db).await;
        let course = course(&creator, &db).await;
        assert!(!can_manage_course(&course, &assigned));
        assert!(!owns_course(&course, &assigned));
    }
}
