//! The insight nest: the read surface over ZEKA's tables, the doors that ask
//! ZEKA to compute, and the storage doors its rows are written through.
//!
//! ZEKA (`hezarfen_zeka`) is an AI service that dials in over the QUIC bridge.
//! It **does not touch a school database** — no AI service on this deployment
//! does — so the `zeka_*` tables, created by the school migrator, are written
//! and read only by this process. Two surfaces, one implementation each: the
//! service calls the backend over the bridge (`insight.summary.upsert` and
//! the rest of the family — the wire contract is [`crate::ai::insight`], the
//! statements are [`crate::db::insight`]), and the same operations are
//! mounted here as HTTP doors for tooling. The backend never composes an
//! insight row; it stores what the service computed and hands it back.
//!
//! Reading is authorized the way every per-student report in this API is:
//! [`service::parent_link::ensure_can_observe`] decides whether the caller
//! may read *that student* at all (teacher+ clears it, a parent needs a live
//! link), and a teacher is then narrowed to the students they actually reach
//! — a shared class×course instance, asked through
//! [`service::instance::visible_instances`]. A student who is not the subject
//! reads `404`, never `403`: a foreign id is indistinguishable from a missing
//! one, the same posture `/marks/{user}` and `/rag/threads/{id}` take.
//!
//! What each door deliberately does **not** return is as much of the contract
//! as what it does (ZEKA's output contract,
//! `hezarfen_zeka/service/docs/CIKTI-SOZLESMESI.md` §5):
//!
//! * The attention list is a teacher's tool. It never appears in
//!   [`my_insight`], and a linked parent reading their child does not get it
//!   either — only staff at `teacher` or above do.
//! * A card's `about` (another student's id) never reaches a student caller.
//! * Segment rows with `confidence = 'none'` (fewer than 30 answers) are
//!   filtered out: noise stays in the database, not on a screen.
//! * Dismissed and expired cards are filtered out of both card reads — the
//!   application closes a card by stamping `dismissed_at`, and a closed card
//!   must not come back. (The dismiss **write** is not built here: nothing in
//!   this batch asked for it, and it needs its own decision on who may stamp
//!   the audit trail. `deferred`, not half-done.)
//!
//! Sending is detached, like every other AI round trip in this API: the
//! compute doors answer `202` the moment the work is queued, because a
//! student's compute can outlast the request-timeout layer
//! ([`crate::constant::REQUEST_TIMEOUT_SECS`]). The client polls the stored
//! rows — the compute doors change nothing a reader cannot already see.
//!
//! The **report** doors are the one exception, and deliberately: building the
//! school's document is not a compute the service does on its own schedule —
//! it is one artifact the manager asked for, rendered from rows the backend
//! already holds, so `POST /runs/{run_day}/report` waits, stores the returned
//! HTML under the school's own blob directory, and answers `200` with what
//! was stored. `GET /runs/{run_day}/report` streams that document back.

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query};
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sqlx::types::Json as SqlJson;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::ai::error::AiError;
use crate::ai::insight::{
    self, PendingList, ProfileWriteRequest, PurgeRequest, REPORT_KIND_SCHOOL, ROSTER_SOURCE_EXPLICIT,
    ROSTER_SOURCE_SCHOOL, RecommendationWriteRequest, RefreshRequest, ReportRequest, RunWriteRequest,
    SchoolDirectory, SegmentWriteRequest, StudentRequest, SummaryWriteRequest, TableVerdicts,
    WriteReceipt,
};
use crate::constant::{
    AI_INSIGHT_REFRESH_CAPABILITY, AI_INSIGHT_REPORT_CAPABILITY, AI_INSIGHT_STUDENT_CAPABILITY,
    MAX_INSIGHT_REFRESH_STUDENTS,
};
use crate::database::Database;
use crate::db;
use crate::domain::class_group::ClassGroupId;
use crate::domain::course::CourseId;
use crate::domain::role::Role;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::service;
use crate::service::parent_link::ensure_can_observe;
use crate::state::AppState;
use crate::tenant::ResolvedTenant;
use crate::web::tenant_state::State;

use super::{
    CurrentUser, Page, PageParams, RequireBuilder, RequireManager, ai_unavailable, paginate, pump,
};

/// The single `now` every card read is filtered against. ZEKA stores unix
/// milliseconds, so "expired" is a comparison, not a projection.
fn now_millis() -> i64 {
    crate::domain::timestamp::Timestamp::now().as_millis()
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(refresh))
        .routes(routes!(my_insight))
        .routes(routes!(student_insight, compute_student))
        .routes(routes!(list_runs, write_run))
        .routes(routes!(generate_report, serve_report))
        .routes(routes!(write_summaries))
        .routes(routes!(write_recommendations))
        .routes(routes!(write_segments))
        .routes(routes!(write_profiles))
        .routes(routes!(pending_students))
        .routes(routes!(sweep_retention))
        .routes(routes!(purge_departed))
}

/// The one door of this nest that is **not** school-scoped: the deployment's
/// active-school directory.
///
/// ZEKA's storage operations are all scoped to the school a session belongs
/// to, and so are these doors — but the directory is not: it names every
/// customer. A school session must therefore never reach it, which is why
/// this route is mounted *outside* the school-scoped gate, on the builder
/// principal (the deployment operator), exactly like the vendor surface's own
/// `GET /schools`. A school cookie answers `401` here, as it does everywhere
/// the builder principal is required.
pub fn school_directory_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(active_schools))
}

// ---- the compute doors -----------------------------------------------------

/// The receipt for a queued dispatch. It carries no id: what was started is
/// not a row of ours, and the work's own record is ZEKA's (`zeka_run` for a
/// refresh, a student's `computed_at` for a compute).
#[derive(Serialize, ToSchema)]
struct AcceptedResponse {
    /// Always `accepted` — the field exists so a client can branch on the
    /// body rather than on the status code alone.
    #[schema(example = "accepted")]
    status: String,
}

#[derive(Deserialize, ToSchema, Default)]
struct ComputeStudentRequest {
    /// ISO-8601 date to compute from; omit for the term's beginning.
    #[schema(example = "2026-09-01")]
    since: Option<String>,
    /// Compute only these sections; omit for all of them.
    sections: Option<Vec<String>>,
}

#[derive(Deserialize, ToSchema, Default)]
struct RefreshInsightsRequest {
    /// Student user ids to recompute; omit (or send an empty list) to sweep
    /// the school's own student roster — every user holding the `student`
    /// role. A named list is used exactly as given.
    user_ids: Option<Vec<String>>,
    /// Recompute even where the service considers its cached result valid.
    force: Option<bool>,
}

/// Ask ZEKA to recompute one student's insight, detached. `202` means queued,
/// not computed: the service works through the student's data and writes its
/// `zeka_*` rows on its own, and `GET /insights/students/{id}` shows them as
/// soon as they land. `503` when no service offers `insight.student` — and
/// nothing is queued.
///
/// Authorization is [`student_insight`]'s, gate for gate: the caller must be
/// able to read the student before they may ask for a recompute, and the
/// refusal is a `404` (a foreign id and a missing one look the same).
///
/// The dispatch names the **caller** as `requested_by`, not the student: the
/// service reads the student's marks and the rest as that caller, so a
/// teacher's or manager's own reach is what the reads answer against.
#[utoipa::path(
    post,
    path = "/students/{user}",
    tag = "insights",
    security(("session_cookie" = [])),
    params(("user" = String, Path, description = "User id")),
    request_body = ComputeStudentRequest,
    responses(
        (status = 202, description = "Queued; the service writes the rows on its own", body = AcceptedResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher, or a parent link to this student", body = ErrorResponse),
        (status = 404, description = "Not found — no such student, or not one the caller may read", body = ErrorResponse),
        (status = 503, description = "No AI service offers `insight.student` right now; nothing was queued", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn compute_student(
    State(st): State<AppState>,
    tenant: ResolvedTenant,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    body: Option<Json<ComputeStudentRequest>>,
) -> Result<Response, AppError> {
    let target = UserId::from_key(&id);
    ensure_can_read(&user, &target, &st.db).await?;

    let body = body.map(|Json(body)| body).unwrap_or_default();
    let request = StudentRequest {
        user_id: target.key(),
        requested_by: user.get_id().key(),
        since: body.since,
        sections: body.sections,
    };
    let Some(bridge) = st.ai.clone() else {
        return Ok(ai_unavailable(
            "the AI service is not enabled on this deployment",
        ));
    };
    if !bridge.has_capability(AI_INSIGHT_STUDENT_CAPABILITY) {
        return Ok(ai_unavailable("no AI service is connected right now"));
    }
    let school = tenant.id;
    tokio::spawn(async move {
        match insight::compute_student(&bridge, &school, &request).await {
            Ok(answer) => tracing::info!(
                "insight.student answered for {}: {} signal(s), {} recommendation(s)",
                request.user_id,
                answer.signals.len(),
                answer.recommendations.len()
            ),
            Err(err) => tracing::warn!("insight.student failed for {}: {err}", request.user_id),
        }
    });

    Ok(accepted())
}

/// Ask ZEKA to recompute the whole school's insights, detached. `202` means
/// queued; the sweep's own record is `zeka_run`, which `GET /insights/runs`
/// reads — poll that to watch a run move from `running` to its verdict.
///
/// Manager+ only: this is a school-wide batch, and the run it starts spends
/// the service's whole time budget. `503` when no service offers
/// `insight.refresh` — nothing is queued.
///
/// The dispatch names the **caller** as `requested_by` — the principal the
/// service's own reads for the sweep run as — and always fills `user_ids`: a
/// named list passes through exactly as given, while an empty body is filled
/// from the school's own student roster (every user holding the `student`
/// role). The service's own roster discovery reads homework `assigned` lists,
/// so an empty body used to sweep nobody; the payload's `roster_source` records
/// which roster the list was. A school with no students dispatches an empty
/// list and answers `0` honestly.
#[utoipa::path(
    post,
    path = "/refresh",
    tag = "insights",
    security(("session_cookie" = [])),
    request_body = RefreshInsightsRequest,
    responses(
        (status = 202, description = "Queued; the service writes its run ledger on its own", body = AcceptedResponse),
        (status = 400, description = "`user_ids` names something that is not a user id, or the school's roster is larger than one refresh may carry", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 503, description = "No AI service offers `insight.refresh` right now; nothing was queued", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type"),
    ),
)]
async fn refresh(
    State(st): State<AppState>,
    tenant: ResolvedTenant,
    RequireManager(user): RequireManager,
    body: Option<Json<RefreshInsightsRequest>>,
) -> Result<Response, AppError> {
    let body = body.map(|Json(body)| body).unwrap_or_default();
    let (roster, roster_source) = match normalized_ids(body.user_ids)? {
        Some(ids) if !ids.is_empty() => (ids, ROSTER_SOURCE_EXPLICIT),
        _ => (school_roster(&st.db).await?, ROSTER_SOURCE_SCHOOL),
    };
    let request = RefreshRequest {
        requested_by: user.get_id().key(),
        user_ids: Some(roster),
        roster_source: roster_source.to_string(),
        force: body.force,
    };
    let Some(bridge) = st.ai.clone() else {
        return Ok(ai_unavailable(
            "the AI service is not enabled on this deployment",
        ));
    };
    if !bridge.has_capability(AI_INSIGHT_REFRESH_CAPABILITY) {
        return Ok(ai_unavailable("no AI service is connected right now"));
    }

    let school = tenant.id;
    tokio::spawn(async move {
        let source = request.roster_source.clone();
        match insight::refresh(&bridge, &school, &request).await {
            Ok(answer) => tracing::info!(
                "insight.refresh ({source} roster) answered: {} requested, {} computed, {} skipped, {} failed",
                answer.requested.unwrap_or(0),
                answer.computed.unwrap_or(0),
                answer.skipped.unwrap_or(0),
                answer.failed.len()
            ),
            Err(err) => tracing::warn!("insight.refresh failed: {err}"),
        }
    });

    Ok(accepted())
}

fn accepted() -> Response {
    (
        StatusCode::ACCEPTED,
        Json(AcceptedResponse {
            status: "accepted".to_string(),
        }),
    )
        .into_response()
}

// ---- the report doors ------------------------------------------------------

/// The receipt for a stored report. `generated_at` is this backend's clock the
/// moment the document was stored — the artifact's age is what a manager
/// re-clicking the button wants to see.
#[derive(Serialize, ToSchema)]
struct ReportReceipt {
    /// The run day the document is about, `YYYY-MM-DD`.
    #[schema(example = "2026-09-17")]
    run_day: String,
    /// The stored document's size in bytes.
    byte_size: u64,
    /// Whether the service clipped the document. A clipped report must not
    /// read as a complete one.
    truncated: bool,
    /// The service's own remarks about the document — e.g. that the run day
    /// has no ledger row. Shown to the manager beside the artifact.
    notes: Vec<String>,
    /// When this backend stored the document, unix milliseconds.
    generated_at: i64,
}

/// Where a school's generated report documents live under its blob directory:
/// one file per run day. The day is validated before it is joined (see
/// [`db::insight::run_day_ok`]), so the key can carry no separator and no
/// `..`.
const REPORT_DIR: &str = "zeka_report";

fn report_key(run_day: &str) -> String {
    format!("{REPORT_DIR}/{run_day}.html")
}

/// Render the school's report for one run day, and store it. Synchronous, and
/// deliberately so: the manager clicked one button, and the document is what
/// they asked for — there is no queue to poll and no partial state, and the
/// receipt says what was stored.
///
/// Manager+ only: the document is whole-school, like the refresh sweep and the
/// run ledger. The payload is read from this school's own `zeka_*` tables and
/// dispatched to the service that declares `insight.report`; the service may
/// not read a database, so every row the document needs travels in the
/// request — the same serde rows the storage surface writes.
///
/// A run day with no ledger row, or one whose tables are empty, is still a
/// legal request: the service renders what it was given and says so in
/// `notes`. Refusals: `503` when no service offers the capability, or it did
/// not answer — nothing was stored; `429` when every worker is at capacity;
/// and the service's own typed refusals, mapped per class by
/// [`report_refusal`].
#[utoipa::path(
    post,
    path = "/runs/{run_day}/report",
    tag = "insights",
    security(("session_cookie" = [])),
    params(("run_day" = String, Path, description = "The run day, `YYYY-MM-DD`")),
    responses(
        (status = 200, description = "The document was rendered and stored; the receipt says what was stored", body = ReportReceipt),
        (status = 400, description = "`run_day` is not a `YYYY-MM-DD` day, or the AI service refused the composed request", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 409, description = "The AI service answered, refusing — `report_refused`, or `report_empty` when it has no rows to render; nothing was stored", body = ErrorResponse),
        (status = 413, description = "The rendered document is over the AI service's size ceiling; nothing was stored", body = ErrorResponse),
        (status = 429, description = "Every AI service offering `insight.report` is at capacity", body = ErrorResponse),
        (status = 503, description = "No AI service offers `insight.report`, or it did not answer; nothing was stored", body = ErrorResponse),
    ),
)]
async fn generate_report(
    State(st): State<AppState>,
    tenant: ResolvedTenant,
    RequireManager(user): RequireManager,
    Path(run_day): Path<String>,
) -> Result<Response, AppError> {
    if !db::insight::run_day_ok(&run_day) {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "run_day",
            reason: "must be a `YYYY-MM-DD` day",
        }));
    }
    let Some(bridge) = st.ai.clone() else {
        return Ok(ai_unavailable(
            "the AI service is not enabled on this deployment",
        ));
    };
    if !bridge.has_capability(AI_INSIGHT_REPORT_CAPABILITY) {
        return Ok(ai_unavailable("no AI service is connected right now"));
    }
    let Some(school) =
        db::insight::school_identity(st.tenants.control(), &tenant.id).await?
    else {
        return Err(AppError::Internal(format!(
            "the resolved school `{}` has no control row",
            tenant.id.as_str()
        )));
    };
    let request = ReportRequest {
        kind: REPORT_KIND_SCHOOL.to_string(),
        run_day: run_day.clone(),
        requested_by: user.get_id().key(),
        school,
        classes: db::insight::all_classes(&st.db).await?,
        summaries: db::insight::all_summaries(&st.db).await?,
        recommendations: db::insight::all_recommendations(&st.db).await?,
        profiles: db::insight::all_profiles(&st.db).await?,
        runs: db::insight::runs_for_report(&st.db, &run_day).await?,
    };
    let answer = match insight::report(&bridge, &tenant.id, &request).await {
        Ok(answer) => answer,
        Err(AiError::Remote { code, message }) => {
            tracing::warn!("insight.report refused: {code}: {message}");
            return Err(report_refusal(code, message));
        }
        Err(AiError::Busy(_)) => {
            return Err(AppError::TooManyRequests { retry_after_secs: 5 });
        }
        // No worker (the capability was withdrawn between the check and the
        // dispatch), a timeout, a transport or protocol failure: the document
        // cannot be produced now, and claiming otherwise would store nothing.
        Err(err) => {
            tracing::warn!("insight.report failed: {err}");
            return Ok(ai_unavailable(
                "the AI service did not answer; nothing was stored",
            ));
        }
    };
    // An empty document is not a document: storing it would serve a blank
    // page as if it were the report, and a manager would read that as "the
    // school has nothing to say". Refused, never persisted.
    if answer.html.trim().is_empty() {
        tracing::error!("insight.report answered with an empty document for {run_day}");
        return Err(AppError::Internal(
            "the AI service answered with an empty document".to_string(),
        ));
    }
    store_report(&st.files_path, &run_day, &answer.html).await?;
    Ok(Json(ReportReceipt {
        run_day,
        byte_size: answer.html.len() as u64,
        truncated: answer.truncated,
        notes: answer.notes,
        generated_at: now_millis(),
    })
    .into_response())
}

/// The service's refusal vocabulary as this door's HTTP answer.
///
/// The report service closes four codes (`hezarfen_zeka`'s handler) and a
/// client branches on the class each becomes:
///
/// * `bad_request` — the service judged the composed request bad (an unserved
///   kind, a malformed day, a row field that is not a list, a school
///   disagreement). The payload is composed here, so this is a backend defect
///   and the caller can only be told the request was refused; the service's
///   own words go to the log.
/// * `insufficient_rows` — all four row lists were empty, so there is no
///   document to build. `409 report_empty`: no retry conjures data, a sweep
///   does.
/// * `document_too_large` — over the service's own HTML ceiling, which it
///   never truncates past. `413`.
/// * `internal` — the render failed on the service's side. `500`.
///
/// Anything else is `409 report_refused` with the service's code and message
/// kept verbatim: a service may define codes this backend has never heard of,
/// and swallowing them would hide the one explanation a caller needs.
fn report_refusal(code: String, message: String) -> AppError {
    match code.as_str() {
        "bad_request" => AppError::Validation(ValidationError::Invalid {
            field: "report",
            reason: "the AI service refused the composed request; the reason is in the backend log",
        }),
        "insufficient_rows" => AppError::ConflictCoded {
            code: "report_empty",
            message: format!(
                "the AI service has no rows to render a report from: {message}"
            ),
        },
        "document_too_large" => AppError::PayloadTooLarge(format!(
            "the rendered report is over the AI service's size ceiling, and it does not truncate: {message}"
        )),
        "internal" => AppError::Internal(format!(
            "the AI service failed to render the report: {message}"
        )),
        _ => AppError::ConflictCoded {
            code: "report_refused",
            message: format!("the AI service refused the report: {code}: {message}"),
        },
    }
}

/// The stored report document, streamed.
///
/// `409 report_missing` when nothing has been generated for the day — a
/// refusal that covers a day the ledger has never heard of too: generation
/// accepts such a day on purpose (the document's `notes` say it), so a `404`
/// here could refuse a day the POST door had just answered `200` for.
#[utoipa::path(
    get,
    path = "/runs/{run_day}/report",
    tag = "insights",
    security(("session_cookie" = [])),
    params(("run_day" = String, Path, description = "The run day, `YYYY-MM-DD`")),
    responses(
        (status = 200, description = "The stored document", content_type = "text/html"),
        (status = 400, description = "`run_day` is not a `YYYY-MM-DD` day", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 409, description = "No report has been generated for this day (`report_missing`)", body = ErrorResponse),
    ),
)]
async fn serve_report(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(run_day): Path<String>,
) -> Result<Response, AppError> {
    if !db::insight::run_day_ok(&run_day) {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "run_day",
            reason: "must be a `YYYY-MM-DD` day",
        }));
    }
    let key = report_key(&run_day);
    let path = crate::web::blob_path(&st.files_path, &key);
    let file = tokio::fs::File::open(&path).await.map_err(|err| {
        if err.kind() != std::io::ErrorKind::NotFound {
            tracing::error!("could not open the stored report {key}: {err}");
        }
        AppError::ConflictCoded {
            code: "report_missing",
            message: format!("no report has been generated for `{run_day}` yet"),
        }
    })?;
    Ok((
        [
            (
                CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            (X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
            (CACHE_CONTROL, HeaderValue::from_static("private, no-store")),
        ],
        Body::from_stream(pump(file)),
    )
        .into_response())
}

/// Store the rendered document under the school's own blob directory.
///
/// Written to a temp name and renamed into place: the artifact appears
/// atomically or not at all, so a reader — or a second POST racing this one —
/// can never open a half-written document. The temp file is removed if
/// anything fails before the rename.
async fn store_report(
    files_path: &std::path::Path,
    run_day: &str,
    html: &str,
) -> Result<(), AppError> {
    crate::web::ensure_files_dir(files_path).await?;
    let dir = crate::web::blob_path(files_path, REPORT_DIR);
    tokio::fs::create_dir_all(&dir).await.map_err(|err| {
        AppError::Internal(format!("failed to create the report directory: {err}"))
    })?;
    let temp = dir.join(format!(
        ".report-{}.tmp",
        crate::domain::monotonic_id::next_uuid()
    ));
    if let Err(err) = tokio::fs::write(&temp, html.as_bytes()).await {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(AppError::Internal(format!(
            "failed to write the report document: {err}"
        )));
    }
    let path = crate::web::blob_path(files_path, &report_key(run_day));
    if let Err(err) = tokio::fs::rename(&temp, &path).await {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(AppError::Internal(format!(
            "failed to store the report document: {err}"
        )));
    }
    Ok(())
}

// ---- the storage doors -----------------------------------------------------
//
// ZEKA's own tables are written and read by the backend, never by the service
// (`crate::db::insight` holds the statements, `src/ai/insight.rs` the bridge
// contract). These are the same nine operations over HTTP — one service
// function, two surfaces — so a deployment's tooling can seed, inspect and
// maintain what the service computed without holding a bridge connection.
//
// Manager+ on all of them: every one reads or writes rows about students
// school-wide, which is the same floor `GET /insights/runs` and the refresh
// door carry. The school is the caller's own, resolved by the nest's own
// extractors; no door here takes a school, a database or a query in its body.

/// Store a batch of ZEKA's student summaries (and their attention items).
///
/// This is what the AI service calls over the bridge as
/// `insight.summary.upsert`. One call is one batch and one transaction: the
/// batch lands whole or not at all, so a caller that retries cannot
/// half-apply it. Each student's attention items are **replaced**, never
/// merged — a fact the service stopped producing must not stay on screen.
/// At most [`MAX_INSIGHT_BATCH_ROWS`](crate::constant::MAX_INSIGHT_BATCH_ROWS)
/// rows per call.
#[utoipa::path(
    post,
    path = "/summaries",
    tag = "insights",
    security(("session_cookie" = [])),
    request_body = SummaryWriteRequest,
    responses(
        (status = 200, description = "The rows were written", body = WriteReceipt),
        (status = 400, description = "The payload does not fit the operation: a field is malformed, or a referenced row is not in this school", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 413, description = "More rows than one call may carry", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn write_summaries(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Json(body): Json<SummaryWriteRequest>,
) -> Result<Json<WriteReceipt>, AppError> {
    Ok(Json(db::insight::write_summaries(&st.db, body.rows).await?))
}

/// Store a batch of ZEKA's recommendation cards.
///
/// The bridge capability is `insight.recommendation.upsert`. The backend
/// mints each row's id. A row whose evidence carries nothing but a
/// `limitation` is **not written** and is counted `rejected` in the receipt —
/// the service's own rule, applied where the write happens instead of being
/// trusted to the writer.
#[utoipa::path(
    post,
    path = "/recommendations",
    tag = "insights",
    security(("session_cookie" = [])),
    request_body = RecommendationWriteRequest,
    responses(
        (status = 200, description = "The rows were written; `rejected` counts the cards the evidence rule kept out", body = WriteReceipt),
        (status = 400, description = "The payload does not fit the operation: a field is malformed, or a referenced row is not in this school", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 413, description = "More rows than one call may carry", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn write_recommendations(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Json(body): Json<RecommendationWriteRequest>,
) -> Result<Json<WriteReceipt>, AppError> {
    Ok(Json(
        db::insight::write_recommendations(&st.db, body.rows).await?,
    ))
}

/// Store a batch of ZEKA's question-segment labels (and each question's
/// dimension split).
///
/// The bridge capability is `insight.segment.upsert`; the dimension rows are
/// replaced with the payload, like the attention items above.
#[utoipa::path(
    post,
    path = "/segments",
    tag = "insights",
    security(("session_cookie" = [])),
    request_body = SegmentWriteRequest,
    responses(
        (status = 200, description = "The rows were written", body = WriteReceipt),
        (status = 400, description = "The payload does not fit the operation: a field is malformed, or a referenced row is not in this school", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 413, description = "More rows than one call may carry", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn write_segments(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Json(body): Json<SegmentWriteRequest>,
) -> Result<Json<WriteReceipt>, AppError> {
    Ok(Json(db::insight::write_segments(&st.db, body.rows).await?))
}

/// Store a batch of ZEKA's student segment profiles.
///
/// The bridge capability is `insight.profile.upsert`. `contrast` is the field
/// the service's rules fire on — the raw `accuracy` mostly measures a
/// student's general level and is stored for the evidence panel only.
#[utoipa::path(
    post,
    path = "/profiles",
    tag = "insights",
    security(("session_cookie" = [])),
    request_body = ProfileWriteRequest,
    responses(
        (status = 200, description = "The rows were written", body = WriteReceipt),
        (status = 400, description = "The payload does not fit the operation: a field is malformed, or a referenced row is not in this school", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 413, description = "More rows than one call may carry", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn write_profiles(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Json(body): Json<ProfileWriteRequest>,
) -> Result<Json<WriteReceipt>, AppError> {
    Ok(Json(db::insight::write_profiles(&st.db, body.rows).await?))
}

/// Store one compute run's ledger row (with its pending students and failed
/// modules, both replaced).
///
/// The bridge capability is `insight.run.upsert`; the run is keyed by
/// `run_day` (`YYYY-MM-DD`), so a re-run of the same night overwrites rather
/// than duplicating. `GET /insights/runs` reads the ledger back.
#[utoipa::path(
    post,
    path = "/runs",
    tag = "insights",
    security(("session_cookie" = [])),
    request_body = RunWriteRequest,
    responses(
        (status = 200, description = "The run's ledger row was written", body = WriteReceipt),
        (status = 400, description = "The payload does not fit the operation: `run_day` is not `YYYY-MM-DD`, or a referenced row is not in this school", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn write_run(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Json(body): Json<RunWriteRequest>,
) -> Result<Json<WriteReceipt>, AppError> {
    Ok(Json(db::insight::write_run(&st.db, body.run).await?))
}

/// The students the freshest compute run left pending — where the next run
/// starts.
///
/// Manager+ only, like `GET /insights/runs`: `pending_students` is a list of
/// people, and this is the same list unpaged. The bridge capability is
/// `insight.pending.list`; the list is answered whole or refused past
/// [`MAX_INSIGHT_PENDING_STUDENTS`](crate::constant::MAX_INSIGHT_PENDING_STUDENTS),
/// never clipped, because the service resumes its next run from it.
#[utoipa::path(
    get,
    path = "/pending",
    tag = "insights",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "The freshest run's pending students, in the run's own order", body = PendingList),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 413, description = "More pending students than one answer may carry", body = ErrorResponse),
    ),
)]
async fn pending_students(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
) -> Result<Json<PendingList>, AppError> {
    Ok(Json(db::insight::last_pending(&st.db).await?))
}

/// Delete every `zeka_*` row whose retention window has closed.
///
/// The bridge capability is `insight.retention.sweep`. Children go before
/// their parents (the foreign keys are `ON DELETE NO ACTION`), the backend's
/// own clock decides expiry, and each table answers its own verdict — a table
/// that could not be swept is reported `false` while the rest proceed, since
/// housekeeping must not lose a whole run over one locked table.
#[utoipa::path(
    post,
    path = "/sweep",
    tag = "insights",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "One verdict per table: whether its expired rows were deleted", body = TableVerdicts),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
    ),
)]
async fn sweep_retention(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
) -> Result<Json<TableVerdicts>, AppError> {
    let now_ms = crate::domain::timestamp::Timestamp::now().as_millis();
    Ok(Json(db::insight::sweep(&st.db, now_ms).await?))
}

/// Delete the derived rows of every student who is no longer on the roster.
///
/// The bridge capability is `insight.departed.purge`; `students` is the
/// **active** roster, and every student not named loses their summaries,
/// profiles, attention items, cards about them, and pending-run entries. One
/// transaction, so the verdicts move together. An empty list is refused: an
/// empty roster is a fetch that failed, and obeying it would delete the whole
/// school's derived data.
#[utoipa::path(
    post,
    path = "/purge",
    tag = "insights",
    security(("session_cookie" = [])),
    request_body = PurgeRequest,
    responses(
        (status = 200, description = "One verdict per table: whether the departed students' rows were deleted", body = TableVerdicts),
        (status = 400, description = "The payload does not fit the operation: the roster is empty", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 413, description = "More students than one call may name", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn purge_departed(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Json(body): Json<PurgeRequest>,
) -> Result<Json<TableVerdicts>, AppError> {
    Ok(Json(
        db::insight::purge_departed(&st.db, body.students).await?,
    ))
}

/// The deployment's active schools, by school — the directory a shared AI fleet
/// schedules over.
///
/// This is the one operation of the family that is not school-scoped, and the
/// one door of this nest that is not a school's: it names every customer, so
/// it runs on the **builder** principal (the deployment operator), mounted
/// outside the school gate exactly like the vendor surface's `GET /schools`.
/// A school session gets `401` here. The bridge capability is
/// `insight.schools.list`, which is the service's own deployment-scoped call:
/// the same function, two surfaces, and a school-scoped caller on neither.
#[utoipa::path(
    get,
    path = "/insights/schools",
    tag = "insights",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "Every active school on this deployment, by school", body = SchoolDirectory),
        (status = 401, description = "Not authenticated as a builder", body = ErrorResponse),
    ),
)]
async fn active_schools(
    axum::extract::State(st): axum::extract::State<AppState>,
    RequireBuilder(_builder): RequireBuilder,
) -> Result<Json<SchoolDirectory>, AppError> {
    Ok(Json(db::insight::active_schools(st.tenants.control()).await?))
}

/// Canonicalize the ids a refresh body names — through the same parser every
/// route uses for a wire id, so a client cannot hand the service a spelling
/// this backend itself would never produce. Anything that parses as no uuid
/// is a `400`: unlike a path segment (where garbage reads as the nil id and
/// matches no row), a batch naming students is a list this backend must not
/// forward half-understood.
fn normalized_ids(ids: Option<Vec<String>>) -> Result<Option<Vec<String>>, AppError> {
    ids.map(|ids| {
        ids.into_iter()
            .map(|id| {
                let parsed = UserId::from_key(&id);
                if parsed.uuid().is_nil() {
                    Err(AppError::Validation(ValidationError::Invalid {
                        field: "user_ids",
                        reason: "must be user ids",
                    }))
                } else {
                    Ok(parsed.key())
                }
            })
            .collect::<Result<Vec<String>, AppError>>()
    })
    .transpose()
}

/// The school's own student roster, as the refresh door fills an empty request
/// body with.
///
/// The same set the office's student table lists: every `app_user` holding the
/// `student` role exactly, newest first ([`db::user::list_by_role`]). It is
/// what the service cannot assemble itself — its own roster discovery reads
/// homework `assigned` lists, which a live school rarely fills — so an empty
/// body must name the school here or the sweep computes nobody.
///
/// Refused, never clipped, past
/// [`MAX_INSIGHT_REFRESH_STUDENTS`](crate::constant::MAX_INSIGHT_REFRESH_STUDENTS):
/// a clipped roster would leave students unanalysed while the run read as
/// complete. A school with no students answers an empty list — a sweep that
/// computed nobody is a true answer, not a failure.
async fn school_roster(db: &Database) -> Result<Vec<String>, AppError> {
    let students = db::user::list_by_role(db, Role::Student).await?;
    if students.len() > MAX_INSIGHT_REFRESH_STUDENTS {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "user_ids",
            reason: "the school's roster is larger than one refresh may carry; \
                     name the students to recompute explicitly",
        }));
    }
    Ok(students.iter().map(|user| user.get_id().key()).collect())
}

// ---- the reads -------------------------------------------------------------

/// One student's nightly summary. The four module objects are ZEKA's own
/// shapes (JSONB, by design): read a member by name, never by position. `null`
/// for a module the run could not compute — an empty object and a missing
/// module are different claims, and only one of them is true.
#[derive(Serialize, ToSchema)]
struct SummaryResponse {
    marks: Option<JsonValue>,
    attendance: Option<JsonValue>,
    submission: Option<JsonValue>,
    study: Option<JsonValue>,
    /// The weakest input's tier: `none` | `exploratory` | `stable`.
    #[schema(example = "stable")]
    confidence: String,
    /// UTC unix milliseconds the summary was computed at — the stamp a
    /// client polls for after asking for a recompute.
    computed_at: i64,
    /// UTC unix milliseconds after which the sweep deletes the row.
    retain_until: i64,
}

/// One entry of the attention list — a statement of fact about a window, not
/// a judgement. **Teacher-facing**: it is never returned to the student it is
/// about (see the module docs).
#[derive(Serialize, ToSchema)]
struct AttentionItemResponse {
    /// Which trigger fired: `attendance` | `homework` | `mark_trend` today —
    /// a fourth trigger is a rule change, so treat this as an open set.
    #[schema(example = "attendance")]
    trigger: String,
    /// The course the trigger is about, when it has one; `null` = school-wide.
    course: Option<String>,
    /// The sentence the teacher reads, written once at compute time.
    fact: String,
    /// UTC unix milliseconds — the window the fact is about.
    window_from: i64,
    window_to: i64,
    /// The numbers behind the fact, for the "neden?" panel. Never empty.
    evidence: JsonValue,
}

/// One recommendation card addressed to the caller. It carries no
/// `dismissed_at`/`dismiss_by`: a dismissed card is filtered out of both card
/// reads, so the field could only ever be null — and a face that never
/// arrives is worse than no face.
#[derive(Serialize, ToSchema)]
struct RecommendationResponse {
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    id: String,
    /// The product family: `O1` … `O4` for students, `T3`/`T4` for teachers.
    #[schema(example = "T4")]
    product: String,
    /// Which rule produced it, and which version of that rule.
    #[schema(example = "T4.attendance")]
    rule_id: String,
    rule_version: i64,
    /// The rule's optional scope segment; `null` when the rule has none.
    scope: Option<String>,
    /// Who the card is about; `null` on a card addressed to its subject
    /// (a student's own card), and never returned to a student caller.
    about: Option<String>,
    /// The role gate the card was written for: `student` | `teacher` |
    /// `parent` | `manager`.
    audience_role: String,
    course: Option<String>,
    /// The numbers behind the card, including the mandatory `limitation`
    /// line. Never empty.
    evidence: JsonValue,
    confidence: String,
    created_at: i64,
    /// After this instant the card is past; expired rows are filtered out of
    /// both reads.
    expires_at: i64,
}

/// One student × dimension × label row. `accuracy` is returned alongside
/// `contrast` and never alone: raw accuracy carries general ability, and
/// `contrast = accuracy − overall_accuracy` is the part specific to the
/// segment.
#[derive(Serialize, ToSchema)]
struct SegmentProfileResponse {
    /// A production dimension: `bilissel_talep` | `dikkat_tuzagi` |
    /// `okuma_yuku` (the experimental `adim_sayisi` never reaches a student).
    #[schema(example = "bilissel_talep")]
    dimension: String,
    #[schema(example = "analiz")]
    label: String,
    n_answers: i64,
    n_correct: i64,
    /// The segment's hit rate, `n_correct / n_answers`.
    accuracy: f64,
    /// The student's hit rate across all labelled items — the subtrahend, on
    /// the row so the evidence panel can show the comparison.
    overall_n_answers: i64,
    overall_accuracy: f64,
    /// The honest measure: `accuracy − overall_accuracy`. Negative = behind
    /// the student's own general level in this segment.
    contrast: f64,
    confidence: String,
    computed_at: i64,
}

/// One student's whole readable insight: the stored summary, the caller's own
/// cards about them, and the segment profile.
#[derive(Serialize, ToSchema)]
struct StudentInsightResponse {
    #[schema(example = "019732e3-7b00-7000-8000-00000000dead")]
    user_id: String,
    /// `null` until ZEKA has computed this student at least once.
    summary: Option<SummaryResponse>,
    /// Teacher-facing; always empty for the subject's own view and for a
    /// linked parent.
    attention: Vec<AttentionItemResponse>,
    /// Cards addressed to the caller about this student — nobody else's cards
    /// appear here, so two teachers reading one student each see their own.
    cards: Vec<RecommendationResponse>,
    segments: Vec<SegmentProfileResponse>,
}

/// The caller's own insight. Students get their summary, their own cards and
/// their segment profile — **never** the attention list about themselves.
/// Any other role gets the cards addressed to them (a teacher's `T4` cards
/// about their students), with `summary`/`segments` empty.
#[utoipa::path(
    get,
    path = "/me",
    tag = "insights",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "The caller's own insight", body = StudentInsightResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn my_insight(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Result<Json<StudentInsightResponse>, AppError> {
    let id = user.get_id();
    let student = user.get_role() == Role::Student;
    Ok(Json(StudentInsightResponse {
        user_id: id.key(),
        summary: if student {
            summary_of(&st.db, id).await?
        } else {
            None
        },
        attention: Vec::new(),
        cards: cards_of(&st.db, id, user.get_role(), None).await?,
        segments: if student {
            segments_of(&st.db, id).await?
        } else {
            Vec::new()
        },
    }))
}

/// One student's insight, as an observer reads it. Teacher+ (narrowed to the
/// students they reach) or a parent holding a live link to that student;
/// everyone else — other students included — gets the same `404` a missing id
/// gets.
#[utoipa::path(
    get,
    path = "/students/{user}",
    tag = "insights",
    security(("session_cookie" = [])),
    params(("user" = String, Path, description = "User id")),
    responses(
        (status = 200, description = "The student's insight, plus the caller's own cards about them", body = StudentInsightResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires teacher role or higher, or a parent link to this student", body = ErrorResponse),
        (status = 404, description = "Not found — no such student, or not one the caller may read", body = ErrorResponse),
    ),
)]
async fn student_insight(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<StudentInsightResponse>, AppError> {
    let target = UserId::from_key(&id);
    ensure_can_read(&user, &target, &st.db).await?;
    let staff = user.get_role().at_least(Role::Teacher);
    Ok(Json(StudentInsightResponse {
        user_id: target.key(),
        summary: summary_of(&st.db, &target).await?,
        attention: if staff {
            attention_of(&st.db, &target).await?
        } else {
            Vec::new()
        },
        cards: cards_of(&st.db, user.get_id(), user.get_role(), Some(&target)).await?,
        segments: segments_of(&st.db, &target).await?,
    }))
}

/// One compute run, as the ledger keeps it.
#[derive(Serialize, ToSchema)]
struct InsightRunResponse {
    /// The run's key: `YYYY-MM-DD`, so a re-run of the same night overwrites
    /// rather than duplicating.
    #[schema(example = "2026-09-17")]
    run_day: String,
    started_at: i64,
    finished_at: Option<i64>,
    /// `running` | `ok` | `partial` | `failed` | `skipped`. A `partial` run
    /// must be shown as such: some students were not processed.
    #[schema(example = "partial")]
    status: String,
    duration_ms: Option<i64>,
    students_total: i64,
    students_ok: i64,
    students_failed: i64,
    students_skipped: i64,
    rows_written: i64,
    /// Whether the run hit its time budget before finishing.
    budget_exceeded: bool,
    budget_ms: i64,
    /// The students the budget ran out on; the next run starts here. Kept for
    /// the management view, which is the only caller of this route.
    pending_students: Vec<String>,
    /// Modules that failed, one per row — so a section can be marked missing
    /// instead of shown as a hole.
    failed_modules: Vec<String>,
}

/// The run ledger, newest first. Manager+ only: `pending_students` is a list
/// of people, and ZEKA's output contract confines it to the management view.
/// Paged via `?limit=&offset=`.
#[utoipa::path(
    get,
    path = "/runs",
    tag = "insights",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "The school's compute runs, newest first", body = Page<InsightRunResponse>),
        (status = 400, description = "`limit` or `offset` out of range", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
    ),
)]
async fn list_runs(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<InsightRunResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let rows = sqlx::query!(
        r#"SELECT run_day, started_at, finished_at, status, duration_ms,
                  students_total, students_ok, students_failed, students_skipped,
                  rows_written, budget_exceeded, budget_ms
           FROM zeka_run
           ORDER BY started_at DESC, run_day DESC"#
    )
    .fetch_all(&st.db)
    .await?;
    let total = rows.len() as i64;
    let window = paginate(&rows, limit, offset);
    let days: Vec<String> = window.iter().map(|row| row.run_day.clone()).collect();
    let mut pending = children_of(&st.db, "pending", &days).await?;
    let mut failed = children_of(&st.db, "failed", &days).await?;
    let items = window
        .iter()
        .map(|row| InsightRunResponse {
            run_day: row.run_day.clone(),
            started_at: row.started_at,
            finished_at: row.finished_at,
            status: row.status.clone(),
            duration_ms: row.duration_ms,
            students_total: row.students_total,
            students_ok: row.students_ok,
            students_failed: row.students_failed,
            students_skipped: row.students_skipped,
            rows_written: row.rows_written,
            budget_exceeded: row.budget_exceeded,
            budget_ms: row.budget_ms,
            pending_students: pending.remove(&row.run_day).unwrap_or_default(),
            failed_modules: failed.remove(&row.run_day).unwrap_or_default(),
        })
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// The children of the runs on this page — one query for the whole page, not
/// one per run, grouped by run day in `ord` order.
async fn children_of(
    db: &Database,
    kind: &str,
    days: &[String],
) -> Result<std::collections::HashMap<String, Vec<String>>, AppError> {
    if days.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let mut grouped: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    match kind {
        "pending" => {
            for row in sqlx::query!(
                r#"SELECT run, student FROM zeka_run_pending
                   WHERE run = ANY($1) ORDER BY run, ord"#,
                days
            )
            .fetch_all(db)
            .await?
            {
                grouped
                    .entry(row.run)
                    .or_default()
                    .push(row.student.to_string());
            }
        }
        "failed" => {
            for row in sqlx::query!(
                r#"SELECT run, module FROM zeka_run_failed_module
                   WHERE run = ANY($1) ORDER BY run, ord"#,
                days
            )
            .fetch_all(db)
            .await?
            {
                grouped.entry(row.run).or_default().push(row.module);
            }
        }
        other => unreachable!("unknown run child table: {other}"),
    }
    Ok(grouped)
}

// ---- the queries -----------------------------------------------------------

/// The nightly summary, or `None` until ZEKA has computed this student once.
async fn summary_of(db: &Database, student: &UserId) -> Result<Option<SummaryResponse>, AppError> {
    let row = sqlx::query!(
        r#"SELECT marks AS "marks?: SqlJson<JsonValue>",
                  attendance AS "attendance?: SqlJson<JsonValue>",
                  submission AS "submission?: SqlJson<JsonValue>",
                  study AS "study?: SqlJson<JsonValue>",
                  confidence, computed_at, retain_until
           FROM zeka_student_summary WHERE student = $1"#,
        student.uuid(),
    )
    .fetch_optional(db)
    .await?;
    Ok(row.map(|row| SummaryResponse {
        marks: row.marks.map(|json| json.0),
        attendance: row.attendance.map(|json| json.0),
        submission: row.submission.map(|json| json.0),
        study: row.study.map(|json| json.0),
        confidence: row.confidence,
        computed_at: row.computed_at,
        retain_until: row.retain_until,
    }))
}

/// The attention list, in the writer's own order. Only staff at `teacher` or
/// above ever call this — see the module docs.
async fn attention_of(
    db: &Database,
    student: &UserId,
) -> Result<Vec<AttentionItemResponse>, AppError> {
    let rows = sqlx::query!(
        r#"SELECT trigger, course AS "course?: CourseId",
                  fact, window_from, window_to,
                  evidence AS "evidence: SqlJson<JsonValue>"
           FROM zeka_attention_item WHERE student = $1 ORDER BY ord"#,
        student.uuid(),
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| AttentionItemResponse {
            trigger: row.trigger,
            course: row.course.map(|course| course.key().to_string()),
            fact: row.fact,
            window_from: row.window_from,
            window_to: row.window_to,
            evidence: row.evidence.0,
        })
        .collect())
}

/// Cards addressed to `audience`, filtered to the label the caller's own role
/// reads (`audience_role`) and to cards that are still live: not expired, not
/// dismissed. `about` narrows to one student — `None` is the caller's own
/// card list, which for a teacher is the `T4` cards about their students.
async fn cards_of(
    db: &Database,
    audience: &UserId,
    role: Role,
    about: Option<&UserId>,
) -> Result<Vec<RecommendationResponse>, AppError> {
    let rows = sqlx::query!(
        r#"SELECT id, product, rule_id, rule_version, scope,
                  about AS "about?: UserId",
                  audience_role, course AS "course?: CourseId",
                  evidence AS "evidence: SqlJson<JsonValue>",
                  confidence, created_at, expires_at
           FROM zeka_recommendation
           WHERE audience = $1 AND audience_role = $2
             AND ($3::uuid IS NULL OR about = $3)
             AND expires_at > $4 AND dismissed_at IS NULL
           ORDER BY created_at DESC, id"#,
        audience.uuid(),
        audience_role(role),
        about.map(UserId::uuid),
        now_millis(),
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| RecommendationResponse {
            id: row.id.to_string(),
            product: row.product,
            rule_id: row.rule_id,
            rule_version: row.rule_version,
            scope: row.scope,
            // A student's own card has no `about` in the database, and the
            // contract forbids one reaching a student caller even if a rule
            // ever set it — so the value is dropped for a student caller, not
            // merely absent.
            about: (role != Role::Student)
                .then_some(row.about)
                .flatten()
                .map(|about| about.key().to_string()),
            audience_role: row.audience_role,
            course: row.course.map(|course| course.key().to_string()),
            evidence: row.evidence.0,
            confidence: row.confidence,
            created_at: row.created_at,
            expires_at: row.expires_at,
        })
        .collect())
}

/// The segment profile, noise filtered out: a row below 30 answers
/// (`confidence = 'none'`) stays in the database and off every screen.
async fn segments_of(
    db: &Database,
    student: &UserId,
) -> Result<Vec<SegmentProfileResponse>, AppError> {
    let rows = sqlx::query!(
        r#"SELECT dimension, label, n_answers, n_correct, accuracy,
                  overall_n_answers, overall_accuracy, contrast, confidence,
                  computed_at
           FROM zeka_student_segment_profile
           WHERE student = $1 AND confidence <> 'none'
           ORDER BY dimension, label"#,
        student.uuid(),
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| SegmentProfileResponse {
            dimension: row.dimension,
            label: row.label,
            n_answers: row.n_answers,
            n_correct: row.n_correct,
            accuracy: row.accuracy,
            overall_n_answers: row.overall_n_answers,
            overall_accuracy: row.overall_accuracy,
            contrast: row.contrast,
            confidence: row.confidence,
            computed_at: row.computed_at,
        })
        .collect())
}

// ---- authorization ---------------------------------------------------------

/// The one gate every per-student insight route shares, read and compute
/// alike.
///
/// [`ensure_can_observe`] settles the role question — teacher+ clears it, a
/// parent needs a live link — and the student a caller may *reach* is then
/// narrowed the same way `/marks/{user}` narrows a report: a teacher reads a
/// student only when they run an instance that student sits in. The narrowing
/// is a `404` rather than an empty body because the insight rows are not
/// per-instance: there is nothing honest to narrow *to*, and a foreign
/// student's id must be indistinguishable from a missing one.
async fn ensure_can_read(caller: &User, target: &UserId, db: &Database) -> Result<(), AppError> {
    ensure_can_observe(caller, target, db).await?;
    let target = service::user::read(db, target)
        .await?
        .ok_or(AppError::NotFound)?;
    // Only a student has insights: a colleague's id is not a narrower report,
    // it is a 404.
    if target.get_role() != Role::Student {
        return Err(AppError::NotFound);
    }
    if caller.get_role() == Role::Teacher && !teacher_reaches(db, caller, target.get_id()).await? {
        return Err(AppError::NotFound);
    }
    Ok(())
}

/// Does this teacher actually reach this student — a shared class×course
/// instance the teacher runs and the student sits in? The instances a teacher
/// runs are [`service::instance::visible_instances`]'s `manages` half, which
/// is the same D10 rule every instance-scoped route applies
/// ([`service::class_course::ensure_instance_teacher`]): an assigned teacher
/// or the class section's homeroom teacher, and nobody else.
async fn teacher_reaches(
    db: &Database,
    teacher: &User,
    student: &UserId,
) -> Result<bool, AppError> {
    let (members, _) = crate::db::class_member::list_for_user(db, student, None, 0).await?;
    let classes: Vec<ClassGroupId> = members
        .iter()
        .map(|member| member.get_class().clone())
        .collect();
    if classes.is_empty() {
        return Ok(false);
    }
    let runs: Vec<uuid::Uuid> = service::instance::visible_instances(teacher, db)
        .await?
        .into_iter()
        .filter(|(_, manages)| *manages)
        .map(|(instance, _)| instance.get_id().uuid())
        .collect();
    if runs.is_empty() {
        return Ok(false);
    }
    Ok(crate::db::class_course::list_for_class_ids(db, &classes)
        .await?
        .iter()
        .any(|instance| runs.contains(&instance.get_id().uuid())))
}

/// The `audience_role` label a caller reads cards under. ZEKA's catalogue
/// closes four labels (`student` | `teacher` | `parent` | `manager`); the two
/// staff ranks above teacher fold into `manager`, and the bridge's own
/// principal reads under a label no row can carry (the column's CHECK lists
/// the human four) — so a service reading as itself gets the empty list an
/// absent surface should give it.
fn audience_role(role: Role) -> &'static str {
    match role {
        Role::Student => "student",
        Role::Parent => "parent",
        Role::Teacher => "teacher",
        Role::Manager | Role::Admin => "manager",
        Role::Ai => "ai",
    }
}
