//! Payload contract for the `insight.*` capabilities, in both directions.
//!
//! ZEKA (`hezarfen_zeka`) computes student and class insights from a school's
//! own data. The family has two halves and they travel opposite ways:
//!
//! * **Outbound** (the backend dispatches to the service):
//!   [`AI_INSIGHT_STUDENT_CAPABILITY`] — one student, on demand;
//!   [`AI_INSIGHT_REFRESH_CAPABILITY`] — a school-wide sweep, on demand;
//!   [`AI_INSIGHT_REPORT_CAPABILITY`] — a school-level report document, on
//!   demand (its request carries the rows the backend already holds). The
//!   dispatches below carry them ([`compute_student`], [`refresh`],
//!   [`report`]).
//! * **Inbound** (the service calls the backend): the nine operations of
//!   [`AI_INSIGHT_SUMMARY_UPSERT_CAPABILITY`]'s family — the storage surface
//!   ZEKA's `zeka_*` rows are written and read through, because **no AI
//!   service may touch a school database**. [`serve`] runs them; the SQL
//!   lives in [`crate::db::insight`], and [`crate::web::insights`] mounts the
//!   same functions as HTTP doors. This is the shape the whole family exists
//!   for: an operation named on the wire, scoped to the frame's school, with
//!   no path, no table and no query any caller can name.
//!
//! [`AI_INSIGHT_CLASS_CAPABILITY`] is declared (the service announces it) but
//! deliberately not dispatched: its request names a course the service would
//! have to enumerate a roster for, and the bridge's read allowlist carries no
//! member listing yet — the backend half of that contract is still open
//! (`hezarfen_zeka/service/docs/BACKEND-GEREKSINIMLERI.md`, item 3). Routing
//! it early would only hand the service a request it cannot answer.
//!
//! Every payload member is optional on the response side on purpose: the
//! service's TypedDicts are `total=False`, so a partial answer is a legal
//! answer, and a strict struct here would turn a service update into a
//! backend error. The inbound rows are the opposite: they are a *write*
//! contract, so a missing field is refused rather than defaulted — except
//! where this module says an omission is meaningful.

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ai::AiBridge;
use crate::ai::error::AiError;
use crate::ai::protocol::FrameError;
use crate::constant::{
    AI_INSIGHT_REFRESH_TIMEOUT_SECS, AI_INSIGHT_REPORT_TIMEOUT_SECS,
    AI_INSIGHT_STUDENT_TIMEOUT_SECS,
};
use crate::tenant::Slug;

pub use crate::constant::{
    AI_INSIGHT_CLASS_CAPABILITY, AI_INSIGHT_REFRESH_CAPABILITY, AI_INSIGHT_REPORT_CAPABILITY,
    AI_INSIGHT_STUDENT_CAPABILITY,
};

/// What the backend asks a service to compute for one student.
///
/// `school` is **not** a payload member: it rides the frame's own
/// [`Request.school`](crate::ai::protocol::Request::school), and a second
/// copy here would be one more place the two could disagree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StudentRequest {
    /// The student's user id.
    pub user_id: String,
    /// Who asked for this compute: the caller the door authenticated, and the
    /// principal every bridge read for this dispatch runs as. The service
    /// passes it as `on_behalf_of`, because the synthetic `ai` principal a
    /// read with nobody named would run as is refused `403` on a per-student
    /// report (`/marks/{user}` and the rest) — the report answers a teacher or
    /// a manager, never the service itself.
    pub requested_by: String,
    /// ISO-8601 date to compute from; omitted, the service starts at the
    /// term's beginning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// Sections to compute; omitted, the service computes all of them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sections: Option<Vec<String>>,
}

/// What the service computed for one student.
///
/// `signals` and `recommendations` stay JSON values: their shape belongs to
/// the service's rule catalogue, which is versioned far faster than this
/// crate, and the stored form of the same data is JSONB for exactly that
/// reason. A reader that needs a card's fields reads them by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StudentResponse {
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub generated_at: Option<String>,
    /// The study profile the service detected.
    #[serde(default)]
    pub archetype: Option<String>,
    #[serde(default)]
    pub signals: Vec<Value>,
    #[serde(default)]
    pub recommendations: Vec<Value>,
    /// Which data source contributed how many records.
    #[serde(default)]
    pub coverage: Option<Value>,
}

/// What the backend would ask a service to compute for one class (a course
/// as one section teaches it). Defined for the contract; nothing dispatches it
/// yet — see the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassRequest {
    /// The course or class the analysis is about.
    pub course_id: String,
    /// Who asked for this compute — see [`StudentRequest::requested_by`].
    pub requested_by: String,
    /// The term to read; omitted, the service uses the current one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term: Option<String>,
    /// Length of the attention list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_n: Option<u32>,
}

/// What the service computed for one class. Same leniency as
/// [`StudentResponse`]: the loose members are the service's own shapes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassResponse {
    #[serde(default)]
    pub course_id: Option<String>,
    #[serde(default)]
    pub generated_at: Option<String>,
    #[serde(default)]
    pub cohort_size: Option<u64>,
    #[serde(default)]
    pub topic_gaps: Vec<Value>,
    #[serde(default)]
    pub attention_list: Vec<Value>,
    #[serde(default)]
    pub coverage: Option<Value>,
}

/// `roster_source` when the caller named the students themselves: the list is
/// passed through exactly as given.
pub const ROSTER_SOURCE_EXPLICIT: &str = "explicit";

/// `roster_source` when the door filled `user_ids` from the school's own
/// student roster (an empty request body).
pub const ROSTER_SOURCE_SCHOOL: &str = "school";

/// What the backend asks a service to recompute for a whole school.
///
/// A batch job, and the door always fills `user_ids`: a caller-named list
/// passes through exactly as given ([`ROSTER_SOURCE_EXPLICIT`]), while an
/// empty one is filled from the school's own student roster
/// ([`ROSTER_SOURCE_SCHOOL`]) — the backend enumerates one now because the
/// service's own roster discovery reads homework `assigned` lists, which a
/// live school rarely fills, so an empty body used to sweep nobody.
/// `roster_source` records which of the two the list was, so the run's counts
/// read honestly: a `school` sweep that computed few students is a bounded
/// run, not a caller who named few.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshRequest {
    /// Who asked for this sweep — see [`StudentRequest::requested_by`].
    pub requested_by: String,
    /// The students to recompute, in the order the door resolved them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_ids: Option<Vec<String>>,
    /// Which roster [`Self::user_ids`] came from: [`ROSTER_SOURCE_EXPLICIT`]
    /// or [`ROSTER_SOURCE_SCHOOL`].
    pub roster_source: String,
    /// Recompute even where a cached result is still valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force: Option<bool>,
}

/// The receipt for a refresh: what the sweep did, once it did it. The run's
/// own ledger is `zeka_run` in the school database, which is what a reader
/// polls while this answer is still on its way.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefreshResponse {
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub requested: Option<u64>,
    #[serde(default)]
    pub computed: Option<u64>,
    #[serde(default)]
    pub skipped: Option<u64>,
    /// `{user_id, code, message}` per student the sweep could not compute.
    #[serde(default)]
    pub failed: Vec<Value>,
}

/// `kind` on a school-level report request — the one document kind this
/// dispatch asks for today. Spelled as the service's own vocabulary spells it
/// (`okul`), because it selects a builder there.
pub const REPORT_KIND_SCHOOL: &str = "okul";

/// What the backend asks a service to render for a whole school, for one run
/// day.
///
/// Unlike the other two outbound requests, this one **carries the data**: the
/// service computes nothing here, and it may not read a school's database, so
/// the backend sends the rows it already holds from its own `zeka_*` tables —
/// the same serde rows the storage surface writes ([`SummaryRow`],
/// [`RecommendationRow`], [`ProfileRow`], [`RunRow`]) — and the service
/// renders them into one self-contained document.
///
/// A run day with no `zeka_run` row, or one whose tables are empty, is a
/// legal request: the lists are then empty (or missing the absent day) and
/// the document's own `notes` say so. Nothing here refuses a day the ledger
/// does not know.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportRequest {
    /// [`REPORT_KIND_SCHOOL`] — which builder the service runs.
    pub kind: String,
    /// The run day the document is about, `YYYY-MM-DD`.
    pub run_day: String,
    /// Who asked for the document — see [`StudentRequest::requested_by`]. The
    /// rendering reads nothing, but the service's logs name the requester.
    pub requested_by: String,
    /// The school's own identity, so the document prints a display name
    /// rather than a slug.
    pub school: ReportSchool,
    /// Every class (şube) of the school, id and display name: the summaries'
    /// evidence carries class **ids**, and a document that groups by class
    /// must print the name a reader recognizes. A sibling key rather than a
    /// member of [`Self::school`]: it is a lookup for the rows, not part of
    /// the school's identity. A class a row names but this list does not
    /// carry (a deleted one) is absent on purpose — the renderer falls back
    /// to an honest label, never a name invented here.
    pub classes: Vec<ClassLabel>,
    /// Every stored summary (with its attention items) in the school.
    pub summaries: Vec<SummaryRow>,
    pub recommendations: Vec<RecommendationRow>,
    pub profiles: Vec<ProfileRow>,
    /// The ledger rows the document may show: the named day first when it
    /// exists, then the newest days, at most a handful.
    pub runs: Vec<RunRow>,
}

/// What the service rendered. `html` is the one required member: a response
/// without a document is not a partial answer, it is a dispatch that produced
/// nothing to store, and [`crate::ai::insight::report`]'s caller would rather
/// refuse it than persist an empty file. The rest is the service's own
/// bookkeeping, optional for the same reason every response member here is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportResponse {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub run_day: Option<String>,
    /// `html` today — the format the stored artifact is served as. Recorded
    /// rather than assumed, so a future format is visible on the wire.
    #[serde(default)]
    pub format: Option<String>,
    /// The self-contained document, `<!doctype html>` first.
    pub html: String,
    #[serde(default)]
    pub byte_size: Option<u64>,
    /// Whether the service clipped the document (a row set past its own
    /// ceiling). Stored and reported verbatim: a clipped document must not
    /// read as a complete one.
    #[serde(default)]
    pub truncated: bool,
    /// The service's own remarks about the document — e.g. that the run day
    /// has no ledger row. Shown to the manager beside the artifact.
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Compute one student's insight, awaited. No capability check here: the
/// caller decides what "no service" means, and every caller today is an HTTP
/// door that has already answered `503` for it ([`crate::web::insights`]).
pub async fn compute_student(
    bridge: &AiBridge,
    school: &Slug,
    request: &StudentRequest,
) -> Result<StudentResponse, AiError> {
    dispatch(
        bridge,
        school,
        AI_INSIGHT_STUDENT_CAPABILITY,
        AI_INSIGHT_STUDENT_TIMEOUT_SECS,
        request,
    )
    .await
}

/// Ask for a school-wide recompute, awaited. The service answers when its
/// batch is over; the deadline is [`AI_INSIGHT_REFRESH_TIMEOUT_SECS`].
pub async fn refresh(
    bridge: &AiBridge,
    school: &Slug,
    request: &RefreshRequest,
) -> Result<RefreshResponse, AiError> {
    dispatch(
        bridge,
        school,
        AI_INSIGHT_REFRESH_CAPABILITY,
        AI_INSIGHT_REFRESH_TIMEOUT_SECS,
        request,
    )
    .await
}

/// Ask for one school-level report document, awaited. The request carries
/// every row the document needs, and the answer carries the rendered HTML
/// back; the deadline is [`AI_INSIGHT_REPORT_TIMEOUT_SECS`] — short on
/// purpose, so the caller's own error path wins the race against the request
/// envelope ([`crate::constant::REQUEST_TIMEOUT_SECS`]). The caller stores the
/// document — this function only moves it.
pub async fn report(
    bridge: &AiBridge,
    school: &Slug,
    request: &ReportRequest,
) -> Result<ReportResponse, AiError> {
    dispatch(
        bridge,
        school,
        AI_INSIGHT_REPORT_CAPABILITY,
        AI_INSIGHT_REPORT_TIMEOUT_SECS,
        request,
    )
    .await
}

/// One request, one typed answer — the dispatches differ only in capability,
/// deadline and payload type.
///
/// A reply that does not parse as the expected type is a
/// [`FrameError::Malformed`]: the bytes were JSON, but not this contract's
/// JSON, and handing back a half-understood answer would be worse than
/// reporting the service out of step.
async fn dispatch<T>(
    bridge: &AiBridge,
    school: &Slug,
    capability: &str,
    timeout_secs: u64,
    request: &impl Serialize,
) -> Result<T, AiError>
where
    T: DeserializeOwned,
{
    let payload = serde_json::to_value(request).map_err(malformed)?;
    let answer = bridge
        .dispatch_with_timeout(
            school,
            capability,
            payload,
            Duration::from_secs(timeout_secs),
        )
        .await?;
    serde_json::from_value(answer).map_err(malformed)
}

fn malformed(err: serde_json::Error) -> AiError {
    AiError::Protocol(FrameError::Malformed(err))
}

// ---- the operations the backend serves -------------------------------------
//
// The half of this module that runs *towards* the backend. ZEKA computes, but
// it may not write: no AI service holds a school database credential on this
// deployment, so its `zeka_*` rows are written and read here, by name, over
// the same bridge — one operation per call, each scoped to the school the
// frame named. [`crate::db::insight`] is the SQL; this is the contract: which
// names exist, what each payload is, and how a refusal reads.

pub use crate::db::insight::{
    AttentionRow, ClassLabel, PendingList, ProfileRow, ProfileWriteRequest, PurgeRequest,
    RecommendationRow, RecommendationWriteRequest, ReportSchool, RunRow, RunWriteRequest,
    SchoolDirectory, SegmentConfidences, SegmentLabels, SegmentRow, SegmentWriteRequest, SummaryRow,
    SummaryWriteRequest, TableVerdicts, WriteReceipt,
};

use crate::constant::{
    AI_INSIGHT_DEPARTED_PURGE_CAPABILITY, AI_INSIGHT_PENDING_LIST_CAPABILITY,
    AI_INSIGHT_PROFILE_UPSERT_CAPABILITY, AI_INSIGHT_RECOMMENDATION_UPSERT_CAPABILITY,
    AI_INSIGHT_RETENTION_SWEEP_CAPABILITY, AI_INSIGHT_RUN_UPSERT_CAPABILITY,
    AI_INSIGHT_SCHOOLS_LIST_CAPABILITY, AI_INSIGHT_SEGMENT_UPSERT_CAPABILITY,
    AI_INSIGHT_SUMMARY_UPSERT_CAPABILITY,
};
use crate::database::Database;
use crate::db;
use crate::domain::timestamp::Timestamp;
use crate::error::AppError;
use crate::module::Module;
use crate::tenant::ResolvedTenant;

/// A refusal headed for a [`CapabilityResponse::Err`](crate::ai::protocol::CapabilityResponse::Err):
/// the documented code, and a message that names the field a caller can fix.
pub type Refusal = (&'static str, String);

/// What a served capability is scoped to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// It runs against the school the frame named, and nothing else.
    School,
    /// Its answer does not come from a school's database at all — the
    /// directory the fleet schedules over. The frame sends no school.
    Deployment,
}

/// Every capability the backend serves, and its scope. Matched exactly: a
/// name that is not here is `unknown_capability`, with no prefix match and no
/// fallback — an operation a caller cannot name is one it cannot run.
pub const SERVED: &[(&str, Scope)] = &[
    (AI_INSIGHT_SUMMARY_UPSERT_CAPABILITY, Scope::School),
    (
        AI_INSIGHT_RECOMMENDATION_UPSERT_CAPABILITY,
        Scope::School,
    ),
    (AI_INSIGHT_SEGMENT_UPSERT_CAPABILITY, Scope::School),
    (AI_INSIGHT_PROFILE_UPSERT_CAPABILITY, Scope::School),
    (AI_INSIGHT_RUN_UPSERT_CAPABILITY, Scope::School),
    (AI_INSIGHT_PENDING_LIST_CAPABILITY, Scope::School),
    (AI_INSIGHT_RETENTION_SWEEP_CAPABILITY, Scope::School),
    (AI_INSIGHT_DEPARTED_PURGE_CAPABILITY, Scope::School),
    (AI_INSIGHT_SCHOOLS_LIST_CAPABILITY, Scope::Deployment),
];

/// The scope of one served capability, or `None` if the backend serves no
/// operation by that name.
pub fn scope_of(capability: &str) -> Option<Scope> {
    SERVED
        .iter()
        .find(|(name, _)| *name == capability)
        .map(|(_, scope)| *scope)
}

/// Run one capability call and hand back its payload.
///
/// `school` is the tenant the bridge resolved for the frame — `None` only for
/// a [`Scope::Deployment`] capability, whose frame names no school. The
/// school's own feature set is checked here as well as at the HTTP nest: a
/// school that is not being served insights should not be accumulating rows
/// for them either.
pub async fn serve(
    capability: &str,
    school: Option<&ResolvedTenant>,
    control: &Database,
    payload: &Value,
) -> Result<Value, Refusal> {
    if scope_of(capability).is_none() {
        return Err((
            "unknown_capability",
            format!("the backend serves no `{capability}` operation"),
        ));
    }
    if capability == AI_INSIGHT_SCHOOLS_LIST_CAPABILITY {
        let directory = db::insight::active_schools(control).await.map_err(refusal)?;
        return encode(&directory);
    }
    let Some(tenant) = school else {
        return Err((
            "unknown_school",
            "this operation runs against a school and the frame named none".to_string(),
        ));
    };
    if !tenant.modules.contains(Module::Chatbot) {
        return Err((
            "not_permitted",
            "this school does not have the insights module enabled".to_string(),
        ));
    }
    let db = &tenant.db;
    match capability {
        AI_INSIGHT_SUMMARY_UPSERT_CAPABILITY => {
            let request: SummaryWriteRequest = decode(payload)?;
            encode(&db::insight::write_summaries(db, request.rows).await.map_err(refusal)?)
        }
        AI_INSIGHT_RECOMMENDATION_UPSERT_CAPABILITY => {
            let request: RecommendationWriteRequest = decode(payload)?;
            encode(
                &db::insight::write_recommendations(db, request.rows)
                    .await
                    .map_err(refusal)?,
            )
        }
        AI_INSIGHT_SEGMENT_UPSERT_CAPABILITY => {
            let request: SegmentWriteRequest = decode(payload)?;
            encode(&db::insight::write_segments(db, request.rows).await.map_err(refusal)?)
        }
        AI_INSIGHT_PROFILE_UPSERT_CAPABILITY => {
            let request: ProfileWriteRequest = decode(payload)?;
            encode(&db::insight::write_profiles(db, request.rows).await.map_err(refusal)?)
        }
        AI_INSIGHT_RUN_UPSERT_CAPABILITY => {
            let request: RunWriteRequest = decode(payload)?;
            encode(&db::insight::write_run(db, request.run).await.map_err(refusal)?)
        }
        AI_INSIGHT_PENDING_LIST_CAPABILITY => {
            encode(&db::insight::last_pending(db).await.map_err(refusal)?)
        }
        AI_INSIGHT_RETENTION_SWEEP_CAPABILITY => {
            // The backend's own clock decides expiry — a service's clock is
            // not a second authority on when a row's retention has closed.
            let now_ms = Timestamp::now().as_millis();
            encode(&db::insight::sweep(db, now_ms).await.map_err(refusal)?)
        }
        AI_INSIGHT_DEPARTED_PURGE_CAPABILITY => {
            let request: PurgeRequest = decode(payload)?;
            encode(
                &db::insight::purge_departed(db, request.students)
                    .await
                    .map_err(refusal)?,
            )
        }
        // `scope_of` matched above, so the table and this match agree; the
        // arm exists so a table entry without an operation is a compile-time
        // miss instead of a runtime surprise.
        other => Err((
            "unknown_capability",
            format!("the backend serves no `{other}` operation"),
        )),
    }
}

/// Decode one payload into the operation's own type. A payload that does not
/// fit is `invalid_payload`, never `malformed`: the frame was a capability
/// call, the body was not this operation's body, and the caller can fix it.
fn decode<T: DeserializeOwned>(payload: &Value) -> Result<T, Refusal> {
    serde_json::from_value(payload.clone())
        .map_err(|err| ("invalid_payload", format!("payload: {err}")))
}

fn encode<T: Serialize>(value: &T) -> Result<Value, Refusal> {
    serde_json::to_value(value).map_err(|err| ("internal", format!("could not encode the answer: {err}")))
}

/// [`AppError`] as the protocol's refusal vocabulary. The mapping is the
/// whole reason the db layer returns `AppError` rather than its own type:
/// HTTP answers these through [`crate::error`], and a capability call answers
/// the same verdict with the code a service branches on.
fn refusal(err: AppError) -> Refusal {
    match err {
        AppError::Validation(err) => ("invalid_payload", err.to_string()),
        AppError::PayloadTooLarge(message) => ("too_many_rows", message),
        AppError::ModuleDisabled(module) => (
            "not_permitted",
            format!("this school does not have the `{module}` module enabled"),
        ),
        AppError::NotFound => ("not_found", "the named row does not exist".to_string()),
        AppError::DbUnavailable | AppError::DbTimeout => (
            "unavailable",
            "this school's database could not be reached".to_string(),
        ),
        other => ("internal", other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire names ARE this contract — a Python service reads them by
    /// name, and a Rust-side rename that slipped past a struct literal would
    /// break the service silently. Pinned on the encoded JSON.
    #[test]
    fn the_request_names_are_the_contracts() {
        let request = StudentRequest {
            user_id: "user-1".into(),
            requested_by: "user-9".into(),
            since: Some("2026-09-01".into()),
            sections: None,
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({
                "user_id": "user-1",
                "requested_by": "user-9",
                "since": "2026-09-01",
            }),
            "a requested_by always rides the wire; an omitted section list stays off it, not null"
        );

        let refresh = RefreshRequest {
            requested_by: "user-9".into(),
            user_ids: Some(vec!["user-1".into()]),
            roster_source: ROSTER_SOURCE_EXPLICIT.into(),
            force: Some(true),
        };
        assert_eq!(
            serde_json::to_value(&refresh).unwrap(),
            serde_json::json!({
                "requested_by": "user-9",
                "user_ids": ["user-1"],
                "roster_source": "explicit",
                "force": true,
            })
        );
    }

    /// A partial answer is a legal answer: the service's TypedDicts are
    /// `total=False`, so parsing must not demand every member — a strict
    /// struct here would turn a service update into a backend error.
    #[test]
    fn a_partial_answer_parses() {
        let answer: StudentResponse =
            serde_json::from_value(serde_json::json!({ "user_id": "user-1" }))
                .expect("a partial answer is still an answer");
        assert_eq!(answer.user_id.as_deref(), Some("user-1"));
        assert!(answer.signals.is_empty());
        assert!(answer.coverage.is_none());
    }

    /// The report payload's own names, pinned on the encoded JSON for the same
    /// reason as the request test above: `hezarfen_zeka` reads `kind`,
    /// `run_day`, `school` and the four row lists by name.
    #[test]
    fn the_report_request_names_are_the_contracts() {
        let request = ReportRequest {
            kind: REPORT_KIND_SCHOOL.into(),
            run_day: "2026-09-17".into(),
            requested_by: "user-9".into(),
            school: ReportSchool {
                id: "school-1".into(),
                slug: "demo".into(),
                name: "Demo Okulu".into(),
            },
            classes: vec![ClassLabel {
                id: "class-1".into(),
                name: "8-A".into(),
            }],
            summaries: Vec::new(),
            recommendations: Vec::new(),
            profiles: Vec::new(),
            runs: Vec::new(),
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({
                "kind": "okul",
                "run_day": "2026-09-17",
                "requested_by": "user-9",
                "school": { "id": "school-1", "slug": "demo", "name": "Demo Okulu" },
                "classes": [{ "id": "class-1", "name": "8-A" }],
                "summaries": [],
                "recommendations": [],
                "profiles": [],
                "runs": [],
            })
        );
    }

    /// A report answer without a document is not a partial answer: `html` is
    /// the one member the door cannot do without, and a service that omits it
    /// is out of step, not empty-handed — the door would rather refuse the
    /// dispatch than store a blank artifact. Every other member stays optional.
    #[test]
    fn a_report_answer_without_html_does_not_parse() {
        let missing = serde_json::from_value::<ReportResponse>(serde_json::json!({
            "kind": "okul",
            "run_day": "2026-09-17"
        }));
        assert!(
            missing.is_err(),
            "a documentless answer is Malformed, never an empty file"
        );

        let answer: ReportResponse =
            serde_json::from_value(serde_json::json!({ "html": "<!doctype html><html></html>" }))
                .expect("the rest is optional");
        assert!(answer.notes.is_empty());
        assert!(!answer.truncated);
        assert!(answer.byte_size.is_none());
    }

    /// The report door waits on its dispatch inside ONE HTTP request, so its
    /// deadline must stay under the request envelope
    /// ([`crate::constant::REQUEST_TIMEOUT_SECS`]): the middleware drops the
    /// handler future at that ceiling — mid-write included — and answers its
    /// own `503` whose "the write may or may not have applied" prose is wrong
    /// for a door that knows nothing was stored. Raising the dispatch deadline
    /// without the ceiling silently re-breaks exactly that, so the invariant
    /// is checked when this module compiles, not when a test runs.
    const _: () = assert!(
        AI_INSIGHT_REPORT_TIMEOUT_SECS < crate::constant::REQUEST_TIMEOUT_SECS,
        "the report dispatch must lose the race to the middleware on purpose"
    );
}
