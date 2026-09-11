//! School fees: the plans a manager writes, the students they are placed on,
//! and the append-only ledger the placement bills into.
//!
//! Every write here is manager+ — money is school administration, and a teacher
//! has no business in it. The read gate is deliberately *narrower* than the
//! per-student reports elsewhere
//! ([`crate::service::parent_link::ensure_can_observe`]): a student sees
//! their own record, a parent a linked student's, manager+ everyone's, and a
//! teacher nothing at all.
//!
//! No route edits or deletes a ledger line, and none exists to write: a mistake
//! is corrected by appending its opposite. Balances and statements are folded
//! from the lines on every read and stored nowhere.

use std::collections::HashMap;

use crate::web::tenant_state::State;
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::constant::{MAX_FEE_PLAN_ASSIGN_STUDENTS, MAX_FEE_PLAN_ASSIGN_WRITES};
use crate::database::Database;
use crate::domain::fee_plan::{FeePlan, FeePlanId, FeePlanName, Installment};
use crate::domain::fee_plan_assignment::FeePlanAssignment;
use crate::domain::payment_ledger::{
    LedgerAmount, LedgerMethod, LedgerNote, PaymentLedger, PaymentLedgerId, PaymentLedgerKind,
    PaymentRequestKey,
};
use crate::domain::role::Role;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ErrorResponse, ValidationError};
use crate::service;
use crate::state::AppState;

use super::{CurrentUser, Page, PageParams, PersonRef, RequireManager, paginate, person_map};

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_plan, list_plans))
        .routes(routes!(get_plan, update_plan, delete_plan))
        .routes(routes!(assign_plan, list_plan_assignments))
        .routes(routes!(record_payment))
        .routes(routes!(record_refund))
        .routes(routes!(record_reversal))
        .routes(routes!(user_ledger))
        .routes(routes!(my_statement))
        .routes(routes!(user_statement))
        .routes(routes!(my_balance))
        .routes(routes!(user_balance))
}

// ---- fee plans --------------------------------------------------------------

/// One installment as a client sends it. Both fields are required: a plan is a
/// schedule, and an installment with no date is not part of one.
#[derive(Debug, Deserialize, ToSchema)]
struct InstallmentBody {
    /// What this installment bills, **minor units** (kuruş), positive. Integer
    /// only: this API never speaks decimals or floats about money.
    #[schema(minimum = 1, maximum = 10000000, example = 150000_i64)]
    amount_minor: i64,
    /// When it falls due, unix milliseconds. **May be in the past** — a school
    /// adopting the app mid-year assigns plans whose first installments were
    /// already due. Negative is a `400`: that is not an instant.
    #[schema(minimum = 0, example = 1_760_000_000_000_i64)]
    due_at: i64,
}

impl InstallmentBody {
    fn into_domain(self) -> Result<Installment, AppError> {
        // A past due date is legal on purpose; a *negative* one is not an
        // instant at all, and it would leave the charge it bills permanently
        // `overdue` with no date a client could have meant.
        if self.due_at < 0 {
            return Err(AppError::Validation(ValidationError::Invalid {
                field: "due_at",
                reason: "must not be negative",
            }));
        }
        Ok(Installment::new(
            LedgerAmount::try_new(self.amount_minor)?,
            Timestamp::from_millis(self.due_at),
        ))
    }

    fn list(body: Vec<Self>) -> Result<Vec<Installment>, AppError> {
        body.into_iter().map(Self::into_domain).collect()
    }
}

#[derive(Deserialize, ToSchema)]
struct CreateFeePlan {
    #[schema(example = "2026-2027 Yearly", max_length = 120)]
    name: String,
    /// The schedule, 1 to 60 entries. Assigning the plan bills every one of
    /// them at once, so this is what a student ends up owing.
    #[schema(min_items = 1, max_items = 60)]
    installments: Vec<InstallmentBody>,
}

#[derive(Deserialize, ToSchema)]
struct UpdateFeePlan {
    #[schema(max_length = 120)]
    name: Option<String>,
    /// Replaces the whole schedule. Refused (`409`) once anyone is on the plan
    /// — their charges are frozen copies, so an edit would only make the plan
    /// and the money disagree.
    #[schema(min_items = 1, max_items = 60)]
    installments: Option<Vec<InstallmentBody>>,
}

#[derive(Serialize, ToSchema)]
struct InstallmentResponse {
    /// Minor units (kuruş).
    amount_minor: i64,
    due_at: i64,
}

#[derive(Serialize, ToSchema)]
struct FeePlanResponse {
    id: String,
    name: String,
    installments: Vec<InstallmentResponse>,
    created_by: PersonRef,
    created_at: i64,
}

impl FeePlanResponse {
    fn new(plan: &FeePlan, people: &HashMap<String, PersonRef>) -> Self {
        Self {
            id: plan.get_id().key().to_string(),
            name: plan.get_name().as_str().to_string(),
            installments: plan
                .get_installments()
                .iter()
                .map(|installment| InstallmentResponse {
                    amount_minor: installment.get_amount_minor().as_minor(),
                    due_at: installment.get_due_at().as_millis(),
                })
                .collect(),
            created_by: PersonRef::resolve(people, plan.get_created_by()),
            created_at: plan.get_created_at().as_millis(),
        }
    }
}

/// Join the authors onto a page of plans — one query for the whole page.
async fn plan_responses(
    plans: &[FeePlan],
    db: &Database,
) -> Result<Vec<FeePlanResponse>, AppError> {
    let people = person_map(plans.iter().map(|plan| plan.get_created_by().clone()), db).await?;
    Ok(plans
        .iter()
        .map(|plan| FeePlanResponse::new(plan, &people))
        .collect())
}

async fn require_plan(id: &str, db: &Database) -> Result<FeePlan, AppError> {
    FeePlan::read(&FeePlanId::from_key(id), db)
        .await?
        .ok_or(AppError::NotFound)
}

/// Write a fee plan: a name and the installments it is paid in. Creating one
/// bills nobody — assigning it does.
#[utoipa::path(
    post,
    path = "/plans",
    tag = "payments",
    security(("session_cookie" = [])),
    request_body = CreateFeePlan,
    responses(
        (status = 201, description = "Plan created", body = FeePlanResponse),
        (status = 400, description = "Invalid name, amount, due date, or installment count", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn create_plan(
    State(st): State<AppState>,
    RequireManager(manager): RequireManager,
    Json(req): Json<CreateFeePlan>,
) -> Result<(StatusCode, Json<FeePlanResponse>), AppError> {
    let plan = FeePlan::create(
        FeePlanName::try_new(&req.name)?,
        InstallmentBody::list(req.installments)?,
        manager.get_id(),
        &st.db,
    )
    .await?;
    let people = PersonRef::map_of(&[&manager]);
    Ok((
        StatusCode::CREATED,
        Json(FeePlanResponse::new(&plan, &people)),
    ))
}

/// Every fee plan, newest first. Paged via `?limit=&offset=` (omit `limit` for
/// all of them); returns a `{items, total, limit, offset}` envelope.
#[utoipa::path(
    get,
    path = "/plans",
    tag = "payments",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "A page of plans (all of them when unpaged)", body = Page<FeePlanResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
    ),
)]
async fn list_plans(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<FeePlanResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let (rows, total) = FeePlan::list_all(limit, offset, &st.db).await?;
    let items = plan_responses(&rows, &st.db).await?;
    Ok(Json(Page::new(items, total, limit, offset)))
}

/// One fee plan with its schedule.
#[utoipa::path(
    get,
    path = "/plans/{id}",
    tag = "payments",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Fee plan id")),
    responses(
        (status = 200, description = "The plan", body = FeePlanResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "No such plan", body = ErrorResponse),
    ),
)]
async fn get_plan(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(id): Path<String>,
) -> Result<Json<FeePlanResponse>, AppError> {
    let plan = require_plan(&id, &st.db).await?;
    let items = plan_responses(std::slice::from_ref(&plan), &st.db).await?;
    Ok(Json(
        items.into_iter().next().expect("one plan in, one out"),
    ))
}

/// Edit a plan's name and/or its schedule. Refused (`409`) once the plan has
/// been assigned to anyone: those charges are frozen copies of the installments
/// as they stood, so editing afterwards would leave the plan and the money
/// telling different stories. Write a new plan instead.
///
/// The refusal is decided by the database as the edit is written, not by a
/// read taken before it, so an assign arriving at the same instant cannot end
/// up on either side of the edit: it either freezes the plan first (and this is
/// a `409`) or bills the edited schedule. A PATCH carrying no field at all
/// writes nothing and returns the plan unchanged, assigned or not.
#[utoipa::path(
    patch,
    path = "/plans/{id}",
    tag = "payments",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Fee plan id")),
    request_body = UpdateFeePlan,
    responses(
        (status = 200, description = "The updated plan", body = FeePlanResponse),
        (status = 400, description = "Invalid name, amount, due date, or installment count", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "No such plan", body = ErrorResponse),
        (status = 409, description = "The plan is already assigned to a student", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn update_plan(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(id): Path<String>,
    Json(req): Json<UpdateFeePlan>,
) -> Result<Json<FeePlanResponse>, AppError> {
    let plan = require_plan(&id, &st.db).await?;
    let name = req.name.as_deref().map(FeePlanName::try_new).transpose()?;
    let installments = req.installments.map(InstallmentBody::list).transpose()?;
    let plan = plan.update(name, installments, &st.db).await?;
    let items = plan_responses(std::slice::from_ref(&plan), &st.db).await?;
    Ok(Json(
        items.into_iter().next().expect("one plan in, one out"),
    ))
}

/// Delete a plan. Refused (`409`) once it has been assigned to anyone — the
/// charges it raised name it, and a school's financial history keeps its
/// references. Decided as the delete is written, the same way the edit is, so a
/// simultaneous assign can never leave a live assignment pointing at a plan
/// that is gone.
#[utoipa::path(
    delete,
    path = "/plans/{id}",
    tag = "payments",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Fee plan id")),
    responses(
        (status = 204, description = "Plan deleted"),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "No such plan", body = ErrorResponse),
        (status = 409, description = "The plan is already assigned to a student", body = ErrorResponse),
    ),
)]
async fn delete_plan(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let plan = require_plan(&id, &st.db).await?;
    if !plan.delete(&st.db).await? {
        return Err(AppError::Conflict("an assigned plan cannot be deleted"));
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---- assignments ------------------------------------------------------------

#[derive(Deserialize, ToSchema)]
struct AssignFeePlan {
    /// The students to place on the plan, at most 200 per call. Each is
    /// reported on individually — one bad id does not lose the rest.
    #[schema(max_items = 200, example = json!(["01JC0Z0Z0Z0Z0Z0Z0Z0Z0Z0Z0Z"]))]
    student_ids: Vec<String>,
}

#[derive(Serialize, ToSchema)]
struct AssignmentOutcome {
    student_id: String,
    /// `assigned` (placed and billed now), `already_assigned` (a replay — the
    /// student was already on this plan, and nothing was billed again), or
    /// `rejected`.
    #[schema(example = "assigned")]
    status: &'static str,
    /// Why it was rejected; absent otherwise.
    reason: Option<&'static str>,
}

#[derive(Serialize, ToSchema)]
struct FeePlanAssignmentResponse {
    id: String,
    plan: String,
    student: PersonRef,
    assigned_by: PersonRef,
    created_at: i64,
}

/// Place a plan on students, which is what turns it into money owed: every
/// installment is appended as a charge line right away, each with its own due
/// date. **Replay-safe** — a student already on the plan is reported
/// `already_assigned` and is not billed a second time, and an assignment whose
/// charges landed only in part heals when the call is repeated.
///
/// Only students carry a fee record, so any other target is rejected. One bad
/// id never loses the rest of the batch: the response reports each student
/// separately.
///
/// Placing the first student **freezes** the plan: it can no longer be edited
/// or deleted. The installments billed are read back from the stored plan at
/// that instant, so an edit that landed a moment earlier is the one billed,
/// never the version this request first looked at. A plan deleted while the
/// batch is running is `rejected` from that student on — a per-student outcome
/// like any other, so the students it already billed stay in the report.
///
/// One request is bounded by the **charges it would raise**, not by the head
/// count alone: `student_ids × installments` may not exceed 3 000 (200
/// students up to a 15-installment plan; 50 at a time on a 60-installment
/// one). Past that the whole call is a `400` telling the caller to split the
/// batch — nothing is written, because a money route that billed half a batch
/// and gave up would leave a bursar guessing which families were charged.
#[utoipa::path(
    post,
    path = "/plans/{id}/assignments",
    tag = "payments",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Fee plan id")),
    request_body = AssignFeePlan,
    responses(
        (status = 200, description = "Per-student outcome, in the order sent", body = Vec<AssignmentOutcome>),
        (status = 400, description = "Too many students named, or too many charges for one request", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "No such plan", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn assign_plan(
    State(st): State<AppState>,
    RequireManager(manager): RequireManager,
    Path(id): Path<String>,
    Json(req): Json<AssignFeePlan>,
) -> Result<Json<Vec<AssignmentOutcome>>, AppError> {
    if req.student_ids.len() > MAX_FEE_PLAN_ASSIGN_STUDENTS {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "student_ids",
            reason: "may name at most 200 students",
        }));
    }
    let plan = require_plan(&id, &st.db).await?;
    // The student cap alone cannot see the schedule: every student named
    // appends *every* installment, so what really bounds this request is the
    // product. Refused whole and before anything is written — a batch this API
    // billed only part of would leave a bursar guessing which families were
    // charged.
    if req
        .student_ids
        .len()
        .saturating_mul(plan.get_installments().len())
        > MAX_FEE_PLAN_ASSIGN_WRITES
    {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "student_ids",
            reason: "too many charges for one request: students × installments \
                     may not exceed 3000, so split the batch",
        }));
    }
    let mut outcomes = Vec::with_capacity(req.student_ids.len());
    for student_id in req.student_ids {
        let student = UserId::from_key(&student_id);
        // A fee record belongs to a student; billing anyone else is a typo, and
        // a typo here is money against the wrong person.
        let is_student = crate::service::user::read(&st.db, &student)
            .await?
            .is_some_and(|user| user.get_role() == Role::Student);
        let (status, reason) = if is_student {
            match FeePlanAssignment::assign(&plan, &student, manager.get_id(), &st.db).await {
                Ok((_, true)) => ("already_assigned", None),
                Ok((_, false)) => ("assigned", None),
                // The plan was deleted mid-batch. That is a per-student outcome
                // like any other, not a reason to throw away the report for the
                // students this batch already billed — their charges are
                // written and the caller has to be told about them.
                Err(AppError::NotFound) => ("rejected", Some("no such plan")),
                Err(err) => return Err(err),
            }
        } else {
            ("rejected", Some("no such student"))
        };
        outcomes.push(AssignmentOutcome {
            student_id,
            status,
            reason,
        });
    }
    Ok(Json(outcomes))
}

/// Who is on this plan, newest first. Paged via `?limit=&offset=`; returns a
/// `{items, total, limit, offset}` envelope.
#[utoipa::path(
    get,
    path = "/plans/{id}/assignments",
    tag = "payments",
    security(("session_cookie" = [])),
    params(("id" = String, Path, description = "Fee plan id"), PageParams),
    responses(
        (status = 200, description = "A page of assignments (all of them when unpaged)", body = Page<FeePlanAssignmentResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "No such plan", body = ErrorResponse),
    ),
)]
async fn list_plan_assignments(
    State(st): State<AppState>,
    RequireManager(_user): RequireManager,
    Path(id): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<FeePlanAssignmentResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let plan = require_plan(&id, &st.db).await?;
    let (rows, total) =
        FeePlanAssignment::list_for_plan(plan.get_id(), limit, offset, &st.db).await?;
    let people = person_map(
        rows.iter()
            .flat_map(|row| [row.get_student().clone(), row.get_assigned_by().clone()]),
        &st.db,
    )
    .await?;
    let items = rows
        .iter()
        .map(|row| FeePlanAssignmentResponse {
            id: row.get_id().key().to_string(),
            plan: row.get_plan().key().to_string(),
            student: PersonRef::resolve(&people, row.get_student()),
            assigned_by: PersonRef::resolve(&people, row.get_assigned_by()),
            created_at: row.get_created_at().as_millis(),
        })
        .collect();
    Ok(Json(Page::new(items, total, limit, offset)))
}

// ---- the ledger -------------------------------------------------------------

#[derive(Serialize, ToSchema)]
struct PaymentLineResponse {
    id: String,
    student: PersonRef,
    /// `charge`, `credit`, `refund`, or `reversal`.
    #[schema(example = "charge")]
    kind: String,
    /// Always positive — the sign is the `kind`'s business.
    #[schema(example = 150000)]
    amount_minor: i64,
    /// What caused the line: the assignment on a `charge`, the paid charge on a
    /// `credit`, the returned credit on a `refund`, the undone line on a
    /// `reversal`.
    source: Option<String>,
    /// When this installment falls due. Charges only.
    due_at: Option<i64>,
    method: Option<String>,
    note: Option<String>,
    recorded_by: PersonRef,
    created_at: i64,
}

impl PaymentLineResponse {
    fn new(line: &PaymentLedger, people: &HashMap<String, PersonRef>) -> Self {
        Self {
            id: line.get_id().key().to_string(),
            student: PersonRef::resolve(people, line.get_student()),
            kind: line.get_kind().as_str().to_string(),
            amount_minor: line.get_amount_minor().as_minor(),
            source: line.get_source_key().map(str::to_string),
            due_at: line.get_due_at().map(|due| due.as_millis()),
            method: line.get_method().map(|method| method.as_str().to_string()),
            note: line.get_note().map(|note| note.as_str().to_string()),
            recorded_by: PersonRef::resolve(people, line.get_recorded_by()),
            created_at: line.get_created_at().as_millis(),
        }
    }
}

/// Join the people onto a page of ledger lines — one query for the whole page.
async fn line_responses(
    lines: &[PaymentLedger],
    db: &Database,
) -> Result<Vec<PaymentLineResponse>, AppError> {
    let people = person_map(
        lines
            .iter()
            .flat_map(|line| [line.get_student().clone(), line.get_recorded_by().clone()]),
        db,
    )
    .await?;
    Ok(lines
        .iter()
        .map(|line| PaymentLineResponse::new(line, &people))
        .collect())
}

/// The single appended line, as a `201`. Shared by all three money routes.
async fn appended(
    line: PaymentLedger,
    db: &Database,
) -> Result<(StatusCode, Json<PaymentLineResponse>), AppError> {
    let items = line_responses(std::slice::from_ref(&line), db).await?;
    Ok((
        StatusCode::CREATED,
        Json(items.into_iter().next().expect("one line in, one out")),
    ))
}

async fn require_line(id: &str, db: &Database) -> Result<PaymentLedger, AppError> {
    service::payment_ledger::read(db, &PaymentLedgerId::from_key(id))
        .await?
        .ok_or(AppError::NotFound)
}

#[derive(Deserialize, ToSchema)]
struct RecordPayment {
    /// The charge line this money pays.
    charge_id: String,
    /// How much came in, **minor units** (kuruş), positive. Partial payments
    /// are the norm; together they may not exceed the charge.
    #[schema(minimum = 1, maximum = 10000000, example = 50000)]
    amount_minor: i64,
    /// How it arrived ("cash", "havale", …). Free text — the backend speaks to
    /// no payment gateway and stores no card data.
    #[schema(max_length = 50, example = "havale")]
    method: Option<String>,
    #[schema(max_length = 500, example = "receipt 2026-114")]
    note: Option<String>,
    /// Optional client-chosen idempotence key, `[A-Za-z0-9-]` (no `_`: it is the
    /// separator inside a ledger line's id). Send one and a
    /// retry after a timeout returns the **same** line instead of recording the
    /// money twice; omit it and two identical calls are two payments. The same
    /// key sent with a different `amount_minor` or `charge_id` is a `409`.
    #[schema(min_length = 1, max_length = 64, example = "receipt-2026-114")]
    request_key: Option<String>,
}

#[derive(Deserialize, ToSchema)]
struct RecordRefund {
    /// The payment being handed back.
    credit_id: String,
    /// How much goes back out, **minor units** (kuruş), positive. Partials
    /// allowed, up to what the credit was worth.
    #[schema(minimum = 1, maximum = 10000000, example = 50000)]
    amount_minor: i64,
    #[schema(max_length = 50, example = "cash")]
    method: Option<String>,
    #[schema(max_length = 500)]
    note: Option<String>,
    /// Optional client-chosen idempotence key, `[A-Za-z0-9-]` — the same
    /// retry-safety a payment gets, keyed by the credit it returns. The same
    /// key with a different `amount_minor` or `credit_id` is a `409`.
    #[schema(min_length = 1, max_length = 64, example = "refund-2026-114")]
    request_key: Option<String>,
}

#[derive(Deserialize, ToSchema)]
struct RecordReversal {
    /// The `charge` or `refund` to undo, for its exact amount.
    line_id: String,
    #[schema(max_length = 500, example = "billed in error")]
    note: Option<String>,
}

fn method_of(raw: Option<&str>) -> Result<Option<LedgerMethod>, AppError> {
    Ok(raw.map(LedgerMethod::try_new).transpose()?.flatten())
}

fn note_of(raw: Option<&str>) -> Result<Option<LedgerNote>, AppError> {
    Ok(raw.map(LedgerNote::try_new).transpose()?.flatten())
}

fn request_key_of(raw: Option<&str>) -> Result<Option<PaymentRequestKey>, AppError> {
    Ok(raw.map(PaymentRequestKey::try_new).transpose()?)
}

/// Record money received against one named charge. Allocation is recorded, not
/// inferred: a payment always says which installment it settles. Partial
/// payments accumulate; one that would take the charge past what it is worth is
/// a `409`. A charge that was **reversed** takes no payment either — it is no
/// longer owed — and that `409` says so, rather than reporting money that never
/// arrived as "paid in full".
///
/// **Retry-safe on request** — send a `request_key` and a repeat of the call
/// (a client retry after a network timeout) returns the line the first attempt
/// wrote rather than recording the money a second time, even when that payment
/// filled the charge exactly. The same key with a different `amount_minor` or
/// `charge_id` is a `409`: that is a client bug, not a replay. Without a key
/// two identical calls are two payments, as before.
#[utoipa::path(
    post,
    path = "/credits",
    tag = "payments",
    security(("session_cookie" = [])),
    request_body = RecordPayment,
    responses(
        (status = 201, description = "Payment recorded", body = PaymentLineResponse),
        (status = 400, description = "Invalid amount, method, note, or a target that is not a charge", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "No such ledger line", body = ErrorResponse),
        (status = 409, description = "The charge is already paid in full, the charge was reversed (so it is no longer owed), it already carries the most lines that may be applied to it, or the request_key was used for a different amount or charge", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn record_payment(
    State(st): State<AppState>,
    RequireManager(manager): RequireManager,
    Json(req): Json<RecordPayment>,
) -> Result<(StatusCode, Json<PaymentLineResponse>), AppError> {
    let charge = require_line(&req.charge_id, &st.db).await?;
    let line = service::payment_ledger::credit(
        &st.db,
        &charge,
        LedgerAmount::try_new(req.amount_minor)?,
        method_of(req.method.as_deref())?,
        note_of(req.note.as_deref())?,
        request_key_of(req.request_key.as_deref())?.as_ref(),
        manager.get_id(),
    )
    .await?;
    appended(line, &st.db).await
}

/// Hand money back, against one named payment — how an over-payment or a
/// payment recorded in error is returned, and the only way a mistaken *credit*
/// is corrected (a credit is never reversed). Capped by that credit's amount.
///
/// **Retry-safe on request** the same way a payment is: a `request_key` makes a
/// repeat return the existing refund, and the same key with a different
/// `amount_minor` or `credit_id` is a `409`.
#[utoipa::path(
    post,
    path = "/refunds",
    tag = "payments",
    security(("session_cookie" = [])),
    request_body = RecordRefund,
    responses(
        (status = 201, description = "Refund recorded", body = PaymentLineResponse),
        (status = 400, description = "Invalid amount, method, note, or a target that is not a payment", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "No such ledger line", body = ErrorResponse),
        (status = 409, description = "The payment is already refunded in full, it already carries the most lines that may be applied to it, or the request_key was used for a different amount or payment", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn record_refund(
    State(st): State<AppState>,
    RequireManager(manager): RequireManager,
    Json(req): Json<RecordRefund>,
) -> Result<(StatusCode, Json<PaymentLineResponse>), AppError> {
    let credit = require_line(&req.credit_id, &st.db).await?;
    let line = service::payment_ledger::refund(
        &st.db,
        &credit,
        LedgerAmount::try_new(req.amount_minor)?,
        method_of(req.method.as_deref())?,
        note_of(req.note.as_deref())?,
        request_key_of(req.request_key.as_deref())?.as_ref(),
        manager.get_id(),
    )
    .await?;
    appended(line, &st.db).await
}

/// Undo a line entered by mistake, for its exact amount — the line itself stays,
/// with an opposing one appended beside it. Only a `charge` or a `refund` may be
/// reversed (`400` otherwise): a mistaken payment is corrected with a refund, so
/// money leaving the school is always spelled the one way. **Idempotent** — a
/// line has at most one reversal, however often the call is retried.
#[utoipa::path(
    post,
    path = "/reversals",
    tag = "payments",
    security(("session_cookie" = [])),
    request_body = RecordReversal,
    responses(
        (status = 201, description = "Reversal recorded (or the existing one replayed)", body = PaymentLineResponse),
        (status = 400, description = "Invalid note, or a target that is not a charge or a refund", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher", body = ErrorResponse),
        (status = 404, description = "No such ledger line", body = ErrorResponse),
        (status = 422, description = "The body does not fit this request: a field has the wrong type, or a required field is missing"),
    ),
)]
async fn record_reversal(
    State(st): State<AppState>,
    RequireManager(manager): RequireManager,
    Json(req): Json<RecordReversal>,
) -> Result<(StatusCode, Json<PaymentLineResponse>), AppError> {
    let target = require_line(&req.line_id, &st.db).await?;
    let line = service::payment_ledger::reversal(
        &st.db,
        &target,
        note_of(req.note.as_deref())?,
        manager.get_id(),
    )
    .await?;
    appended(line, &st.db).await
}

// ---- reads ------------------------------------------------------------------

/// May `caller` read `target`'s payment record? Own always; a parent only for a
/// student they hold a live link to; manager+ for anyone.
///
/// Deliberately narrower than
/// [`crate::service::parent_link::ensure_can_observe`]: **a teacher sees no
/// money**. What a family owes the school is not classroom information.
async fn ensure_can_read_payments(
    caller: &User,
    target: &UserId,
    db: &Database,
) -> Result<(), AppError> {
    if caller.get_id() == target || caller.get_role().at_least(Role::Manager) {
        return Ok(());
    }
    // The link row alone is not the grant: a link whose student side changed
    // role must be inert, so the target's live role is re-read. A missing or
    // non-student target falls through to the same 403 — a parent never gets an
    // existence oracle.
    if caller.get_role() == Role::Parent
        && crate::service::parent_link::links_live(db, caller.get_id(), target).await?
    {
        return Ok(());
    }
    Err(AppError::Forbidden(
        "requires manager role or higher, or a parent link to this student",
    ))
}

/// One student's raw ledger, newest line first: every charge, payment, refund,
/// and reversal. Nothing here is ever edited or deleted — a correction is
/// another line. Own record always; otherwise manager+ or a parent link (a
/// teacher gets a `403`). Paged via `?limit=&offset=`.
#[utoipa::path(
    get,
    path = "/ledger/{user}",
    tag = "payments",
    security(("session_cookie" = [])),
    params(("user" = String, Path, description = "Student id"), PageParams),
    responses(
        (status = 200, description = "A page of ledger lines (all of them when unpaged)", body = Page<PaymentLineResponse>),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher, or a parent link", body = ErrorResponse),
    ),
)]
async fn user_ledger(
    State(st): State<AppState>,
    CurrentUser(caller): CurrentUser,
    Path(user): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<Page<PaymentLineResponse>>, AppError> {
    let (limit, offset) = page.resolve()?;
    let target = UserId::from_key(&user);
    ensure_can_read_payments(&caller, &target, &st.db).await?;
    let (rows, total) =
        service::payment_ledger::list_for_student(&st.db, &target, limit, offset).await?;
    let items = line_responses(&rows, &st.db).await?;
    Ok(Json(Page::new(items, total, limit, offset)))
}

#[derive(Serialize, ToSchema)]
struct StatementEntry {
    /// The charge line this row rolls up.
    charge_id: String,
    /// The plan it came from, and its name when the plan still exists.
    plan: Option<String>,
    plan_name: Option<String>,
    /// What the installment bills, minor units (kuruş).
    #[schema(example = 150000)]
    amount_minor: i64,
    /// When it falls due, unix milliseconds.
    due_at: Option<i64>,
    /// Payments recorded against it.
    credited_minor: i64,
    /// Of those payments, how much was handed back (a reversed refund does not
    /// count — it never left).
    refunded_minor: i64,
    /// `amount - credited + refunded`, or `0` for a reversed charge. Negative
    /// means the charge was over-paid.
    outstanding_minor: i64,
    /// Was the charge itself undone? Such a charge owes nothing.
    reversed: bool,
    /// Still owed, and its due date has passed. Derived on every read, never
    /// stored — there is no scheduler and no overdue sweep.
    overdue: bool,
}

#[derive(Serialize, ToSchema)]
struct StatementResponse {
    student: PersonRef,
    /// One row per charge, newest first, in the standard
    /// `{items, total, limit, offset}` envelope. Paging windows these rows
    /// only — `balance_minor` is folded from every line either way.
    entries: Page<StatementEntry>,
    /// `credits + reversals - charges - refunds`, minor units. Negative means
    /// the family owes the school.
    #[schema(example = -100000)]
    balance_minor: i64,
}

/// The per-charge rollup, folded from the student's lines on every read.
///
/// Everything derived here — what a charge collected, what went back out,
/// what is still owed, and whether it is late — is computed from the raw lines
/// at request time and stored nowhere: a stored rollup is a second version of
/// the truth, and the ledger is the first.
///
/// `limit`/`offset` window the returned rows **after** the whole fold, never
/// the lines it folds: a page of a statement must still report the same
/// `balance_minor` (and the same overdue arithmetic) as every other page.
async fn statement_response(
    student: &UserId,
    limit: Option<i64>,
    offset: i64,
    db: &Database,
) -> Result<Json<StatementResponse>, AppError> {
    let (lines, _) = service::payment_ledger::list_for_student(db, student, None, 0).await?;
    // Index every line by what it points at, so the rollup is one pass over the
    // student's history rather than a query per charge.
    let mut children: HashMap<&str, Vec<&PaymentLedger>> = HashMap::new();
    for line in &lines {
        if let Some(source) = line.get_source_key() {
            children.entry(source).or_default().push(line);
        }
    }
    let reversed = |key: &str| {
        children.get(key).is_some_and(|kids| {
            kids.iter()
                .any(|kid| kid.get_kind() == PaymentLedgerKind::Reversal)
        })
    };

    // A charge names its assignment, and the assignment names the plan.
    let (assignments, _) = FeePlanAssignment::list_for_student(student, None, 0, db).await?;
    let mut plan_names: HashMap<String, Option<String>> = HashMap::new();
    let mut plan_of: HashMap<&str, &FeePlanId> = HashMap::new();
    for assignment in &assignments {
        let plan = assignment.get_plan();
        if !plan_names.contains_key(plan.key()) {
            let name = FeePlan::read(plan, db)
                .await?
                .map(|plan| plan.get_name().as_str().to_string());
            plan_names.insert(plan.key().to_string(), name);
        }
        plan_of.insert(assignment.get_id().key(), plan);
    }

    let now = Timestamp::now().as_millis();
    // The window is taken over the charges, not over the lines: the rollup
    // above (and the balance below) still reads every line, so a page reports
    // exactly the arithmetic the unpaged document does.
    let charges: Vec<&PaymentLedger> = lines
        .iter()
        .filter(|line| line.get_kind() == PaymentLedgerKind::Charge)
        .collect();
    let total = charges.len() as i64;
    let entries: Vec<StatementEntry> = paginate(&charges, limit, offset)
        .iter()
        .map(|charge| {
            let paid: Vec<&&PaymentLedger> = children
                .get(charge.get_id().key())
                .map(|kids| {
                    kids.iter()
                        .filter(|kid| kid.get_kind() == PaymentLedgerKind::Credit)
                        .collect()
                })
                .unwrap_or_default();
            let credited: i64 = paid
                .iter()
                .map(|credit| credit.get_amount_minor().as_minor())
                .sum();
            // A refund hangs off the credit it returns, one level below the
            // charge — and a refund that was itself reversed never left.
            let refunded: i64 = paid
                .iter()
                .filter_map(|credit| children.get(credit.get_id().key()))
                .flatten()
                .filter(|kid| {
                    kid.get_kind() == PaymentLedgerKind::Refund && !reversed(kid.get_id().key())
                })
                .map(|refund| refund.get_amount_minor().as_minor())
                .sum();
            let charge_reversed = reversed(charge.get_id().key());
            let outstanding = if charge_reversed {
                0
            } else {
                charge.get_amount_minor().as_minor() - credited + refunded
            };
            let due_at = charge.get_due_at().map(|due| due.as_millis());
            let plan = plan_of.get(charge.get_source_key().unwrap_or_default());
            StatementEntry {
                charge_id: charge.get_id().key().to_string(),
                plan: plan.map(|plan| plan.key().to_string()),
                plan_name: plan.and_then(|plan| plan_names.get(plan.key()).cloned().flatten()),
                amount_minor: charge.get_amount_minor().as_minor(),
                due_at,
                credited_minor: credited,
                refunded_minor: refunded,
                outstanding_minor: outstanding,
                reversed: charge_reversed,
                overdue: outstanding > 0 && due_at.is_some_and(|due| due < now),
            }
        })
        .collect();

    let people = person_map(std::iter::once(student.clone()), db).await?;
    Ok(Json(StatementResponse {
        student: PersonRef::resolve(&people, student),
        entries: Page::new(entries, total, limit, offset),
        // Folded from the very lines the rollup above walked, never re-read: a
        // payment landing between two reads would leave one document saying
        // both "still owed" and "already settled" about the same money.
        balance_minor: PaymentLedger::fold_balance(lines.iter().map(PaymentLedger::folded)),
    }))
}

/// The caller's own statement. The per-charge rows are paged via
/// `?limit=&offset=` (omit `limit` for all of them); `balance_minor` is the
/// same on every page, because the fold behind it reads every line.
#[utoipa::path(
    get,
    path = "/statement/me",
    tag = "payments",
    security(("session_cookie" = [])),
    params(PageParams),
    responses(
        (status = 200, description = "The caller's statement", body = StatementResponse),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn my_statement(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(page): Query<PageParams>,
) -> Result<Json<StatementResponse>, AppError> {
    let (limit, offset) = page.resolve()?;
    statement_response(user.get_id(), limit, offset, &st.db).await
}

/// One student's statement: a row per charge with what it collected, what went
/// back out, what is still owed, and whether it is overdue. Own record always;
/// otherwise manager+ or a parent link (a teacher gets a `403`). The rows are
/// paged via `?limit=&offset=`; `balance_minor` is the same on every page.
#[utoipa::path(
    get,
    path = "/statement/{user}",
    tag = "payments",
    security(("session_cookie" = [])),
    params(("user" = String, Path, description = "Student id"), PageParams),
    responses(
        (status = 200, description = "The student's statement", body = StatementResponse),
        (status = 400, description = "Invalid limit or offset", body = ErrorResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher, or a parent link", body = ErrorResponse),
    ),
)]
async fn user_statement(
    State(st): State<AppState>,
    CurrentUser(caller): CurrentUser,
    Path(user): Path<String>,
    Query(page): Query<PageParams>,
) -> Result<Json<StatementResponse>, AppError> {
    let (limit, offset) = page.resolve()?;
    let target = UserId::from_key(&user);
    ensure_can_read_payments(&caller, &target, &st.db).await?;
    statement_response(&target, limit, offset, &st.db).await
}

#[derive(Serialize, ToSchema)]
struct PaymentBalanceResponse {
    student: PersonRef,
    /// `credits + reversals - charges - refunds`, in **minor units** (kuruş).
    /// Negative means the family owes the school. Derived on every read, never
    /// stored.
    #[schema(example = -100000)]
    balance_minor: i64,
}

async fn balance_response(
    student: &UserId,
    db: &Database,
) -> Result<Json<PaymentBalanceResponse>, AppError> {
    let people = person_map(std::iter::once(student.clone()), db).await?;
    Ok(Json(PaymentBalanceResponse {
        student: PersonRef::resolve(&people, student),
        balance_minor: service::payment_ledger::balance_of(db, student).await?,
    }))
}

/// What the caller owes the school (or has on account).
#[utoipa::path(
    get,
    path = "/balance/me",
    tag = "payments",
    security(("session_cookie" = [])),
    responses(
        (status = 200, description = "The caller's fee balance", body = PaymentBalanceResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
    ),
)]
async fn my_balance(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Result<Json<PaymentBalanceResponse>, AppError> {
    balance_response(user.get_id(), &st.db).await
}

/// One student's fee balance. Own record always; otherwise manager+ or a parent
/// link (a teacher gets a `403`).
#[utoipa::path(
    get,
    path = "/balance/{user}",
    tag = "payments",
    security(("session_cookie" = [])),
    params(("user" = String, Path, description = "Student id")),
    responses(
        (status = 200, description = "The student's fee balance", body = PaymentBalanceResponse),
        (status = 401, description = "Not authenticated", body = ErrorResponse),
        (status = 403, description = "Requires manager role or higher, or a parent link", body = ErrorResponse),
    ),
)]
async fn user_balance(
    State(st): State<AppState>,
    CurrentUser(caller): CurrentUser,
    Path(user): Path<String>,
) -> Result<Json<PaymentBalanceResponse>, AppError> {
    let target = UserId::from_key(&user);
    ensure_can_read_payments(&caller, &target, &st.db).await?;
    balance_response(&target, &st.db).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The statement is the one piece of arithmetic this layer owns, and it is
    /// arithmetic about money: a charge's payments, the refunds hanging a level
    /// below them, a reversal that zeroes the whole row, and `overdue` derived
    /// from a due date that has passed. Folded against the engine, because the
    /// shape of `source` links is exactly what the fold walks.
    #[tokio::test]
    async fn the_statement_rolls_each_charge_up_from_its_own_lines() {
        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("mgr1");
        let student = UserId::from_key("stu1");
        let future = Timestamp::now().as_millis() + 30 * 24 * 60 * 60 * 1000;
        let plan = FeePlan::create(
            FeePlanName::try_new("Yearly").unwrap(),
            vec![
                // Already due, and only part paid — the overdue row.
                Installment::new(
                    LedgerAmount::try_new(10_000).unwrap(),
                    Timestamp::from_millis(1_000),
                ),
                Installment::new(
                    LedgerAmount::try_new(20_000).unwrap(),
                    Timestamp::from_millis(future),
                ),
            ],
            &manager,
            &db,
        )
        .await
        .unwrap();
        FeePlanAssignment::assign(&plan, &student, &manager, &db)
            .await
            .unwrap();

        let (lines, _) = service::payment_ledger::list_for_student(&db, &student, None, 0)
            .await
            .unwrap();
        let charge = |amount: i64| {
            lines
                .iter()
                .find(|line| line.get_amount_minor().as_minor() == amount)
                .expect("the installment was billed")
                .clone()
        };
        let paid = service::payment_ledger::credit(
            &db,
            &charge(10_000),
            LedgerAmount::try_new(6_000).unwrap(),
            None,
            None,
            None,
            &manager,
        )
        .await
        .unwrap();
        service::payment_ledger::refund(
            &db,
            &paid,
            LedgerAmount::try_new(1_000).unwrap(),
            None,
            None,
            None,
            &manager,
        )
        .await
        .unwrap();
        service::payment_ledger::reversal(&db, &charge(20_000), None, &manager)
            .await
            .unwrap();

        let statement = statement_response(&student, None, 0, &db).await.unwrap().0;
        let entry = |n: &str| {
            statement
                .entries
                .items
                .iter()
                .find(|entry| entry.charge_id.ends_with(n))
                .expect("a row per charge")
        };
        let first = entry("_c1");
        assert_eq!(first.plan_name.as_deref(), Some("Yearly"));
        assert_eq!(first.credited_minor, 6_000);
        assert_eq!(first.refunded_minor, 1_000);
        // 10 000 billed, 6 000 paid, 1 000 of that handed back.
        assert_eq!(first.outstanding_minor, 5_000);
        assert!(!first.reversed);
        assert!(first.overdue, "unpaid and its due date has passed");

        let second = entry("_c2");
        assert!(second.reversed);
        assert_eq!(
            second.outstanding_minor, 0,
            "a reversed charge owes nothing"
        );
        assert!(!second.overdue);

        assert_eq!(
            statement.balance_minor, -5_000,
            "the fold and the rollup must agree about what is owed"
        );
    }
}
