//! Payload contract for the `insight.*` capabilities, and the dispatch behind
//! the two the backend can route today.
//!
//! ZEKA (`hezarfen_zeka`) computes student and class insights from a school's
//! own data. Unlike the chatbot and the RAG nests, whose answers the backend
//! stores, ZEKA **writes its own rows**: the `zeka_*` tables in each school
//! database, created by the school migrator
//! (`migrations/school/20260917000002_zeka.sql`). This module only carries the
//! frames; [`crate::web::insights`] is the reader of what lands.
//!
//! That split is why the dispatches here return the service's answer rather
//! than store it — the answer is a receipt, and the rows are the product. A
//! dispatch that times out, or a service that refuses, leaves the previously
//! computed rows exactly as they are: a stale insight beats a hole. The two
//! capabilities this module dispatches:
//!
//! * [`AI_INSIGHT_STUDENT_CAPABILITY`] — one student, on demand;
//! * [`AI_INSIGHT_REFRESH_CAPABILITY`] — a school-wide sweep, on demand.
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
//! backend error.

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ai::AiBridge;
use crate::ai::error::AiError;
use crate::ai::protocol::FrameError;
use crate::constant::{AI_INSIGHT_REFRESH_TIMEOUT_SECS, AI_INSIGHT_STUDENT_TIMEOUT_SECS};
use crate::tenant::Slug;

pub use crate::constant::{
    AI_INSIGHT_CLASS_CAPABILITY, AI_INSIGHT_REFRESH_CAPABILITY, AI_INSIGHT_STUDENT_CAPABILITY,
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
/// as one şube teaches it). Defined for the contract; nothing dispatches it
/// yet — see the module docs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassRequest {
    /// The course or class the analysis is about.
    pub course_id: String,
    /// The dönem to read; omitted, the service uses the current one.
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

/// What the backend asks a service to recompute for a whole school.
///
/// A batch job: with `user_ids` omitted the service works through its own
/// configured student list — the backend does not enumerate one, because a
/// school-wide roster is not something the bridge's read scope hands a
/// service (same doc, item 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_ids: Option<Vec<String>>,
    /// Recompute even where a cached result is still valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force: Option<bool>,
}

/// The receipt for a refresh: what the sweep did, once it did it. The run's
/// own ledger is `zeka_run` in the school database, which is what a reader
/// polls while this answer is still on its way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// One request, one typed answer — the two dispatches differ only in
/// capability, deadline and payload type.
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
            since: Some("2026-09-01".into()),
            sections: None,
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({ "user_id": "user-1", "since": "2026-09-01" }),
            "an omitted section list stays off the wire, not null"
        );

        let refresh = RefreshRequest {
            user_ids: Some(vec!["user-1".into()]),
            force: Some(true),
        };
        assert_eq!(
            serde_json::to_value(&refresh).unwrap(),
            serde_json::json!({ "user_ids": ["user-1"], "force": true })
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
}
