use super::*;

use crate::domain::class_course::ClassCourseId;

// ---- audience: the shared exam ----------------------------------------------
// An exam is owned by the instance it was created on, but it can be announced
// to other instances of the same course in the same academic year: one
// sitting, one mark, standing in every addressed instance's marks and report card.
// The gate is the owner instance's (a manager+, its assigned teachers, its
// class section's homeroom teacher) — the instance that runs the exam decides
// who else sits it.

/// One instance an exam is announced to.
#[derive(Serialize, ToSchema)]
pub(crate) struct ExamAudienceResponse {
    /// The class×course instance the exam is announced to.
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    instance: String,
    /// The class section that instance belongs to.
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    class: String,
    /// The catalog course that instance teaches — the exam's own course, by
    /// the announcement's rule.
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    course: String,
}

impl ExamAudienceResponse {
    fn new(audience: &crate::db::exam_audience::Audience) -> Self {
        Self {
            instance: audience.get_instance().key().to_string(),
            class: audience.get_class().key().to_string(),
            course: audience.get_course().key().to_string(),
        }
    }

    fn list(audience: &[crate::db::exam_audience::Audience]) -> Vec<Self> {
        audience.iter().map(Self::new).collect()
    }
}

/// `(may_view, may_manage)` over the instances an exam is addressed to — the
/// pair of questions every exam-level gate asks (D2).
///
/// A caller *sees* the exam when any addressed instance admits them as a
/// viewer (its enrolled students, its teachers, its class section's homeroom
/// teacher, manager+), and *acts* on it — grading, the results readers, the live
/// monitor — when they manage one of them. Both are exactly the predicates
/// that instance's own routes apply
/// ([`super::instances::list_instance_exams`], `POST /instances/{id}/exams`),
/// so an announced-to section's teacher runs the exam there like the owner's.
/// One walk: an audience is one or two rows.
pub(crate) async fn audience_rights(
    db: &Database,
    exam: &Exam,
    user: &User,
) -> Result<(bool, bool), AppError> {
    let mut may_view = false;
    let mut may_manage = false;
    for row in crate::service::exam::list_audience(db, exam.get_id()).await? {
        if !may_view && can_view_instance(db, row.get_instance(), user).await? {
            may_view = true;
        }
        if !may_manage && can_manage_instance(db, row.get_instance(), user).await? {
            may_manage = true;
        }
        if may_view && may_manage {
            break;
        }
    }
    Ok((may_view, may_manage))
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct AddAudience {
    /// The class×course instance to announce the exam to (`GET /instances/me`,
    /// `GET /classes/{id}/instances`). It must teach the exam's own catalog
    /// course, sit under the same academic year, and be a *different*
    /// instance — the owner is refused with a `400`.
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    instance: String,
}

/// Announce an exam to another instance — the **shared exam** write: one exam
/// addressed to a sibling class section, so it and its marks stand in that
/// section's exam list, marks report and report card. Requires teacher+ and
/// management rights over the exam's **owner** instance (an assigned teacher,
/// its class section's homeroom teacher, or a manager/admin) — the target
/// instance's teachers have no say.
/// The target must teach the exam's own catalog course and sit under the same
/// academic year (`400` otherwise), the owner itself is refused (`400`), and
/// an archived target year is a `409`. Announcing a pair that already stands
/// is a no-op answering `200` with the audience unchanged.
#[utoipa::path(
    post,
    path = "/{id}/audience",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    request_body = AddAudience,
    responses(
        (status = 200, description = "The exam's audience after the announcement", body = [ExamAudienceResponse]),
        (status = 400, description = "The instance is the exam's own, teaches another course, or belongs to another academic year", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not this exam's owner instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
        (status = 404, description = "The exam or the instance does not exist", body = ErrorResponse),
        (status = 409, description = "The instance's academic year is archived (read-only)", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
pub(crate) async fn add_audience(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path(id): Path<String>,
    Json(req): Json<AddAudience>,
) -> Result<Json<Vec<ExamAudienceResponse>>, AppError> {
    let audience = service::exam::add_audience(
        &st.db,
        &user,
        &ExamId::from_key(&id),
        &ClassCourseId::from_key(&req.instance),
    )
    .await?;
    Ok(Json(ExamAudienceResponse::list(&audience)))
}

/// List the instances an exam is announced to, its owner included — the read
/// behind the announce routes' answer, useful on its own to a client that
/// wants to show where else an exam is sat. Visible to the exam's own
/// audience: an addressed instance's enrolled students, its teachers (or its
/// class section's homeroom teacher), and managers/admins — except drafts,
/// which stay a `404` to everyone but an addressed instance's managers.
#[utoipa::path(
    get,
    path = "/{id}/audience",
    tag = "exams",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Exam id")),
    responses(
        (status = 200, description = "The instances the exam is announced to (the owner first, then in announcement order)", body = [ExamAudienceResponse]),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not enrolled and not a teacher of any instance the exam is addressed to, nor its şube's homeroom teacher, nor a manager/admin", body = ErrorResponse),
        (status = 404, description = "Not found (or a draft the caller may not see)", body = ErrorResponse),
    ),
)]
pub(crate) async fn list_audience(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<Vec<ExamAudienceResponse>>, AppError> {
    let exam = service::exam::read(&st.db, &ExamId::from_key(&id))
        .await?
        .ok_or(AppError::NotFound)?;
    let (may_view, may_manage) = audience_rights(&st.db, &exam, &user).await?;
    if !may_view {
        return Err(AppError::Forbidden(
            "only an addressed instance's enrolled students, its teachers, its class's homeroom teacher, or a manager/admin can view this exam",
        ));
    }
    // A draft doesn't exist for anyone but an addressed instance's managers —
    // 404, not 403, so its existence never leaks to the students it's hidden
    // from.
    if exam.is_draft() && !may_manage {
        return Err(AppError::NotFound);
    }
    let audience = service::exam::list_audience(&st.db, exam.get_id()).await?;
    Ok(Json(ExamAudienceResponse::list(&audience)))
}

/// Withdraw an exam from one instance's audience. Requires teacher+ and
/// management rights over the exam's **owner** instance, like the announce
/// itself. The owner's own pair is not withdrawable (`400`) — that is the
/// instance the exam belongs to, and deleting the exam is what ends it; an
/// archived year refuses the withdrawal with the same `409` its other writes
/// answer. A pair that holds no audience row is a `404`.
#[utoipa::path(
    delete,
    path = "/{id}/audience/{instance}",
    tag = "exams",
    security(("session_cookie" = [])),
    params(
        ("id" = String, Path, description = "Exam id"),
        ("instance" = String, Path, description = "Instance id"),
    ),
    responses(
        (status = 200, description = "The exam's audience after the withdrawal", body = [ExamAudienceResponse]),
        (status = 400, description = "The pair names the exam's owner instance", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Not this exam's owner instance's teacher, its şube's homeroom teacher, or a manager/admin", body = ErrorResponse),
        (status = 404, description = "The exam does not exist, or the pair holds no audience row", body = ErrorResponse),
        (status = 409, description = "The exam's academic year is archived (read-only)", body = ErrorResponse),
    ),
)]
pub(crate) async fn remove_audience(
    State(st): State<AppState>,
    RequireTeacher(user): RequireTeacher,
    Path((id, instance)): Path<(String, String)>,
) -> Result<Json<Vec<ExamAudienceResponse>>, AppError> {
    let audience = service::exam::remove_audience(
        &st.db,
        &user,
        &ExamId::from_key(&id),
        &ClassCourseId::from_key(&instance),
    )
    .await?;
    Ok(Json(ExamAudienceResponse::list(&audience)))
}
