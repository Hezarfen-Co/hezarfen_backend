//! Payload contract for the `rag.summarize` and `rag.questions` capabilities.
//!
//! Same transport as every other capability: JSON inside a
//! [`Request::payload`](crate::ai::protocol::Request::payload) and coming back
//! inside [`Response::Ok`](crate::ai::protocol::Response::Ok)`::payload`. What
//! differs from [`super::rag_chat`] is the shape of a turn: those are
//! thread-bound and asynchronous (a `202`, a stored row, a poll), while these
//! two are **one-shot** — the caller names exactly one corpus and a range
//! inside it and waits for the artifact itself (a summary, or a set of
//! practice questions). Nothing is stored on either side of the bridge, so the
//! reply *is* the answer.
//!
//! The scope is one `(sinif, ders)` pair plus the page/span range inside it.
//! Which pairs the asker may name is the backend's own derivation
//! ([`crate::service::rag_scope`]) and the caller's body only picks a target
//! within them, so the pair that travels here is already the authorized one.
//! The asker rides along for the same reason it does on `rag.chat`: the service
//! may read that person's own data back through the bridge's api reads
//! (`on_behalf_of`).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::ai::AiBridge;
use crate::constant::{
    AI_RAG_QUESTIONS_CAPABILITY, AI_RAG_QUESTIONS_TIMEOUT_SECS, AI_RAG_SUMMARIZE_CAPABILITY,
    AI_RAG_SUMMARIZE_TIMEOUT_SECS,
};
use crate::tenant::Slug;

/// The one corpus a study request is addressed to, and where inside it.
///
/// One scope, never a list: a summary or a question set is *about* the
/// material the caller selected, and a range spanning several corpora has no
/// single thing to be about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RagScope {
    /// The class/grade the corpus belongs to (`"10"`), or `null` for a
    /// school-wide corpus (club/etüt), which belongs to no section and so
    /// names no grade.
    pub sinif: Option<String>,
    /// The subject/course name the corpus is routed by — the one field a scope
    /// cannot go without, since a subject-less scope retrieves nothing.
    pub ders: String,
    /// The pages of the corpus to work over, when its source is paginated.
    /// Empty is legal: a scope with neither pages nor spans is answered with an
    /// `abstained` reply (`empty_scope`), not refused here.
    #[serde(default)]
    pub pages: Vec<i64>,
    /// Opaque retrieval span ids to work over, for a range the caller already
    /// holds from an earlier retrieval. Normally exactly one of
    /// `pages`/`span_ids` is set.
    #[serde(default)]
    pub span_ids: Vec<String>,
    /// A human label for the range (the heading the caller selected), used by
    /// the service in its own prose. May be empty.
    #[serde(default)]
    pub scope_label: String,
}

/// What the backend asks a RAG service to summarize (`rag.summarize`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RagSummarizePayload {
    /// The corpus and range to summarize.
    pub scope: RagScope,
    /// The id of the person who asked. The service reads that person's own
    /// data back through the bridge's api reads (`on_behalf_of`). Read live
    /// from the authenticated session, never taken from a request body.
    pub asker: String,
    /// The asker's **school role** — the same lowercase strings the rest of the
    /// API uses, read live from the session, so a service may refuse a scope a
    /// student's role may not have.
    pub asker_role: String,
}

/// What the backend asks a RAG service for (`rag.questions`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RagQuestionsPayload {
    /// The corpus and range the questions are generated from — also the bound
    /// on their answers: a question whose answer is not in this range must not
    /// be generated.
    pub scope: RagScope,
    /// The id of the person who asked, exactly as [`RagSummarizePayload`]'s.
    pub asker: String,
    /// The asker's live school role.
    pub asker_role: String,
    /// How many questions to generate. The door holds it to a small bounded
    /// set (1..=20) before dispatching.
    pub n: u32,
    /// The difficulty the caller asked for (`kolay`/`orta`/`zor` and whatever
    /// else the service knows). The backend only forwards it — the vocabulary
    /// is the service's.
    pub difficulty: String,
    /// A question to base the set on, when the caller wants variations of one
    /// specific exercise. `null` means "any question within the scope".
    #[serde(default)]
    pub seed_question: Option<String>,
}

/// One passage a summary drew on, inside the reply's `citations`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RagSummaryCitation {
    /// The marker the summary text uses to point at this passage: `[N]` in
    /// `text` resolves to the citation whose `n` is `N`.
    pub n: u32,
    /// The page numbers within the document the passage spans.
    #[serde(default)]
    pub pages: Vec<i64>,
    /// Opaque retrieval span ids within the document.
    #[serde(default)]
    pub span_ids: Vec<String>,
}

/// The service's summary, inside `Response::Ok { payload }`.
///
/// Handled failures do *not* come back here — they use the existing
/// [`Response::Err`](crate::ai::protocol::Response::Err) `{ code, message }`,
/// so there is exactly one failure shape on the bridge for every capability.
/// A refusal that is part of the answer (nothing in the named range to
/// summarize, a guard) rides here with `abstained: true` and a `reason`: the
/// request completes, and nothing is reported as an error.
///
/// Unlike `rag.chat`'s citations, these carry **no corpus `doc_id`** — a
/// summary is addressed to a range the caller selected, not scoped to one
/// document — so nothing here resolves to a course-note file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RagSummarizeReply {
    /// The summary text. Required: a reply without it is unreadable, not an
    /// empty summary.
    pub text: String,
    /// Whether the service declined to summarize rather than summarizing.
    #[serde(default)]
    pub abstained: bool,
    /// Why it abstained, as a short machine code (`empty_scope`,
    /// `insufficient_data`, `guard_*`, `model_abstained`, …); empty on an
    /// ordinary answer.
    #[serde(default)]
    pub reason: String,
    /// The passages the summary drew on, oldest first.
    #[serde(default)]
    pub citations: Vec<RagSummaryCitation>,
    /// The pages the summary actually covered, when the service resolved the
    /// requested range against the corpus.
    #[serde(default)]
    pub scope_pages: Vec<i64>,
    /// Whether the service summarized the range hierarchically (per section,
    /// then rolled up) rather than flat.
    #[serde(default)]
    pub hierarchical: bool,
}

/// One generated practice question, inside the reply's `items`.
///
/// The keys are the **service's** own (`soru`/`cevap`/`zorluk`) — the wire
/// mirrors the service's vocabulary exactly as `rag.chat` mirrors its own, and
/// the HTTP door is where they become this API's English field names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RagQuestion {
    /// The question itself.
    pub soru: String,
    /// The answer, generated from the named range and never from outside it —
    /// the range is the bound that keeps a practice question answerable from
    /// the material the student holds.
    pub cevap: String,
    /// The question's difficulty, in the service's own vocabulary.
    pub zorluk: String,
}

/// The service's generated question set, inside `Response::Ok { payload }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RagQuestionsReply {
    /// The generated questions, in the order they should be shown. Required: a
    /// reply without `items` is unreadable; a set that could not be generated
    /// is `abstained: true` with a `reason` instead.
    pub items: Vec<RagQuestion>,
    /// Whether the service declined to generate rather than generating.
    #[serde(default)]
    pub abstained: bool,
    /// Why it abstained, as a short machine code; empty on an ordinary answer.
    #[serde(default)]
    pub reason: String,
    /// The retrieval spans the whole set is bounded to.
    #[serde(default)]
    pub span_ids: Vec<String>,
    /// The pages those spans sit on.
    #[serde(default)]
    pub pages: Vec<i64>,
}

/// Ask a RAG service to summarize one range of one corpus.
///
/// `Err` is a short, stable code for the door's refusal table — a transport
/// failure, the service's own `Response::Err` verdict, or `bad_reply` when the
/// reply was unreadable. Two codes are special: `role_required` and
/// `scope_mismatch` mean the **backend** composed a request the service may not
/// answer (the pair or the role it sent), so they are logged as bugs here and
/// reach the caller as a `502` — the same posture `rag.chat` takes.
pub async fn summarize(
    bridge: &AiBridge,
    slug: &Slug,
    payload: RagSummarizePayload,
) -> Result<RagSummarizeReply, String> {
    let asker = payload.asker.clone();
    let encoded = encode(&payload, AI_RAG_SUMMARIZE_CAPABILITY)?;
    let raw = bridge
        .dispatch_with_timeout(
            slug,
            AI_RAG_SUMMARIZE_CAPABILITY,
            encoded,
            Duration::from_secs(AI_RAG_SUMMARIZE_TIMEOUT_SECS),
        )
        .await
        .map_err(failure_code)?;
    let reply: RagSummarizeReply = decode(raw, AI_RAG_SUMMARIZE_CAPABILITY)?;
    reject_backend_bug(&reply.reason, &asker, AI_RAG_SUMMARIZE_CAPABILITY)?;
    Ok(reply)
}

/// Ask a RAG service to generate practice questions over one range of one
/// corpus. The refusal vocabulary is [`summarize`]'s, code for code.
pub async fn questions(
    bridge: &AiBridge,
    slug: &Slug,
    payload: RagQuestionsPayload,
) -> Result<RagQuestionsReply, String> {
    let asker = payload.asker.clone();
    let encoded = encode(&payload, AI_RAG_QUESTIONS_CAPABILITY)?;
    let raw = bridge
        .dispatch_with_timeout(
            slug,
            AI_RAG_QUESTIONS_CAPABILITY,
            encoded,
            Duration::from_secs(AI_RAG_QUESTIONS_TIMEOUT_SECS),
        )
        .await
        .map_err(failure_code)?;
    let reply: RagQuestionsReply = decode(raw, AI_RAG_QUESTIONS_CAPABILITY)?;
    reject_backend_bug(&reply.reason, &asker, AI_RAG_QUESTIONS_CAPABILITY)?;
    Ok(reply)
}

/// Encode one payload; a payload that cannot be encoded is this side's bug, so
/// it is logged and reported as `internal` rather than as a service verdict.
fn encode<P: Serialize>(payload: &P, capability: &str) -> Result<serde_json::Value, String> {
    serde_json::to_value(payload).map_err(|err| {
        tracing::error!("could not encode a {capability} request: {err}");
        "internal".to_string()
    })
}

/// Decode one service reply, wrapped in the capability's name. An unreadable
/// payload is `bad_reply`: the answer is not safe to guess at, and a service
/// that changed its shape must be seen failing rather than half-read.
fn decode<R: serde::de::DeserializeOwned>(
    raw: serde_json::Value,
    capability: &str,
) -> Result<R, String> {
    serde_json::from_value(raw).map_err(|err| {
        tracing::warn!("AI service answered with an unreadable {capability} payload: {err}");
        "bad_reply".to_string()
    })
}

/// The two verdicts that mean the **backend** built a bad request: the scope or
/// the role it sent is not something the service may answer. The request still
/// fails — an artifact is not safe to invent — but this side of the bridge is
/// the bug, so it is logged as one rather than as an ordinary service refusal.
fn reject_backend_bug(reason: &str, asker: &str, capability: &str) -> Result<(), String> {
    if matches!(reason, "role_required" | "scope_mismatch") {
        tracing::error!(
            "{capability} refused a request the backend built: {reason} (asker {asker})"
        );
        return Err(reason.to_string());
    }
    Ok(())
}

/// A dispatch failure as a short, stable code the frontend can branch on. The
/// same mapping the chatbot nest applies to its own round trip.
fn failure_code(err: crate::ai::AiError) -> String {
    use crate::ai::AiError;
    match err {
        AiError::NoWorker(_) | AiError::Setup(_) => "unavailable".to_string(),
        AiError::Busy(_) => "busy".to_string(),
        AiError::Timeout(_) => "timed_out".to_string(),
        AiError::Transport(_) => "transport".to_string(),
        AiError::Protocol(_) | AiError::IdMismatch { .. } => "protocol".to_string(),
        // The service's own verdict. Kept verbatim so a service can define
        // codes the backend has never heard of.
        AiError::Remote { code, message } => {
            tracing::warn!("AI RAG service refused the request: {code}: {message}");
            if code.trim().is_empty() {
                "service_error".to_string()
            } else {
                code
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn study_payload_keys_are_the_documented_wire_names() {
        // Services in other languages match on these literals — a rename here
        // silently breaks every one of them, so pin the encoding, key names
        // included (the exact wire key sets are the contract).
        let summarize = serde_json::to_value(RagSummarizePayload {
            scope: RagScope {
                sinif: Some("10".into()),
                ders: "biyoloji".into(),
                pages: vec![16, 17],
                span_ids: vec![],
                scope_label: "DNA".into(),
            },
            asker: "01ASKER".into(),
            asker_role: "teacher".into(),
        })
        .unwrap();
        assert_eq!(
            summarize,
            json!({
                "scope": {
                    "sinif": "10",
                    "ders": "biyoloji",
                    "pages": [16, 17],
                    "span_ids": [],
                    "scope_label": "DNA",
                },
                "asker": "01ASKER",
                "asker_role": "teacher",
            })
        );

        // A grade-less (school-wide) corpus sends `sinif: null`, exactly as
        // `rag.chat`'s pairs do, and a span-addressed range sends no pages.
        let questions = serde_json::to_value(RagQuestionsPayload {
            scope: RagScope {
                sinif: None,
                ders: "Satranç Kulübü".into(),
                pages: vec![],
                span_ids: vec!["s-7".into()],
                scope_label: String::new(),
            },
            asker: "01ASKER".into(),
            asker_role: "student".into(),
            n: 5,
            difficulty: "orta".into(),
            seed_question: None,
        })
        .unwrap();
        assert_eq!(
            questions,
            json!({
                "scope": {
                    "sinif": null,
                    "ders": "Satranç Kulübü",
                    "pages": [],
                    "span_ids": ["s-7"],
                    "scope_label": "",
                },
                "asker": "01ASKER",
                "asker_role": "student",
                "n": 5,
                "difficulty": "orta",
                "seed_question": null,
            })
        );

        // Every optional reply field defaults on a minimal service answer, and
        // the one required field of each stays required: a reply without text
        // (or without items) is unreadable, never an empty artifact.
        let bare: RagSummarizeReply =
            serde_json::from_value(json!({ "text": "özet" })).unwrap();
        assert!(!bare.abstained);
        assert_eq!(bare.reason, "");
        assert!(bare.citations.is_empty());
        assert!(bare.scope_pages.is_empty());
        assert!(!bare.hierarchical);
        assert!(serde_json::from_value::<RagSummarizeReply>(json!({ "abstained": true })).is_err());

        let bare: RagQuestionsReply = serde_json::from_value(json!({ "items": [] })).unwrap();
        assert!(!bare.abstained);
        assert_eq!(bare.reason, "");
        assert!(bare.span_ids.is_empty());
        assert!(bare.pages.is_empty());
        assert!(serde_json::from_value::<RagQuestionsReply>(json!({ "abstained": true })).is_err());

        // The question row keeps the service's own keys.
        let row: RagQuestion =
            serde_json::from_value(json!({ "soru": "DNA nedir?", "cevap": "…", "zorluk": "orta" }))
                .unwrap();
        assert_eq!(row.soru, "DNA nedir?");
        assert_eq!(row.cevap, "…");
        assert_eq!(row.zorluk, "orta");
    }
}
