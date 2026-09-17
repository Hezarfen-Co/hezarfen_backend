//! Payload contract for the `rag.chat` capability.
//!
//! Same transport as every other capability: this is JSON riding inside a
//! [`Request::payload`](crate::ai::protocol::Request::payload) and coming back
//! inside [`Response::Ok`](crate::ai::protocol::Response::Ok)`::payload`. One
//! request, one answer — retrieval-then-answer is unary here even though it is
//! two steps inside the service.
//!
//! The service answers *about* a school's course-note corpus, so the request
//! carries both who is asking (so the service can read that person's own data
//! back through the bridge's api reads, as `on_behalf_of`) and the
//! `(sınıf, ders)` scope the corpus is routed by. The reply returns the answer
//! text plus the citations behind it, so the backend can resolve each cited
//! passage to the course-note file it came from.

use serde::{Deserialize, Serialize};

use crate::ai::chat::ChatRole;

/// One `(sınıf, ders)` pair the question is scoped to.
///
/// The corpus is routed by the **pair**: a grade and a subject list sent
/// separately would cross-product into combinations the asker never named, so
/// each pair is one scope the service may retrieve from. `sinif` is optional —
/// a question about a subject across every grade names `ders` alone — and
/// `ders` is required, since a subject-less scope retrieves nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RagScopePair {
    /// The class/grade the subject is taught in (`"9"`, `"10"`), or omitted to
    /// scope the subject across grades.
    #[serde(default)]
    pub sinif: Option<String>,
    /// The subject/course name the pair is scoped to (Matematik, Fizik). The
    /// one field a pair cannot go without.
    pub ders: String,
}

/// One previous message in the RAG conversation.
///
/// The same `{role, content}` shape the chatbot's history uses — a model's
/// `messages` array takes it with no remapping — so the two histories are
/// interchangeable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RagTurn {
    pub role: ChatRole,
    pub content: String,
}

/// What the backend asks a RAG service to answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RagChatRequestPayload {
    /// The user's new question. `history` does *not* contain it.
    pub message: String,
    /// The id of the user who asked. The service reads that person's own data
    /// back through the bridge's api reads (`on_behalf_of`), so a question
    /// about "my marks" is answered from *their* marks and never from another
    /// student's. Read live from the authenticated session, never taken from a
    /// request body.
    pub asker: String,
    /// The asker's **school role** — `"parent"`, `"student"`, `"teacher"`,
    /// `"manager"` or `"admin"`, the same lowercase strings the rest of the
    /// API uses. Read live from the session on every request, like the chatbot
    /// request's copy, so a service may answer on it: a student must not be
    /// handed an answer scoped for a manager.
    pub asker_role: String,
    /// The `(sınıf, ders)` pairs the question is scoped to — one entry per pair,
    /// because the RAG routes corpora by the pair. At most
    /// [`MAX_RAG_SCOPE_PAIRS`](crate::constant::MAX_RAG_SCOPE_PAIRS); a request
    /// carrying more is refused rather than narrowed.
    pub scope: Vec<RagScopePair>,
    /// The last N turns of the conversation, **oldest first** — index 0 the
    /// furthest back, the final element the turn immediately before `message`.
    /// Absent or `[]` means a fresh conversation.
    #[serde(default)]
    pub history: Vec<RagTurn>,
}

/// One passage the answer drew on, inside the reply's `citations`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RagCitation {
    /// The marker the answer text uses to point at this passage: `[N]` in
    /// `text` resolves to the citation whose `n` is `N`.
    pub n: u32,
    /// The corpus document id the passage came from. The backend maps it to
    /// the course-note file that owns that PDF (the `rag.index` reply echoes
    /// this id back per file), so a citation resolves to something a reader can
    /// open.
    pub doc_id: String,
    /// The page numbers within the document the passage spans, when the source
    /// is paginated.
    #[serde(default)]
    pub pages: Vec<i64>,
    /// Opaque retrieval span ids within the document, for a client that wants
    /// to highlight the exact passage rather than the whole page.
    #[serde(default)]
    pub span_ids: Vec<String>,
    /// The subject the passage belongs to, when the corpus records one.
    #[serde(default)]
    pub ders: Option<String>,
}

/// The service's answer, inside `Response::Ok { payload }`.
///
/// Handled failures do *not* come back here — they use the existing
/// [`Response::Err`](crate::ai::protocol::Response::Err) `{ code, message }`,
/// so there is exactly one failure shape on the bridge for every capability.
/// A refusal that is part of the answer (the service chose not to answer, or
/// could not) rides here instead, because it is still an answer: the turn
/// completes, no error surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RagChatReplyPayload {
    /// The complete answer text, with `[N]` markers resolving to `citations`.
    pub text: String,
    /// Whether the service declined to answer rather than answering. A true
    /// value is a complete, successful turn — not a failure — and `reason`
    /// says why.
    #[serde(default)]
    pub abstained: bool,
    /// Why the service abstained, as a short machine code (`""` when it did
    /// not): `guard_*`, `insufficient_data`, `model_abstained`,
    /// `scope_mismatch`, `role_required`, or a service-specific code. Empty on
    /// an ordinary answer.
    #[serde(default)]
    pub reason: String,
    /// The passages the answer drew on, oldest first.
    #[serde(default)]
    pub citations: Vec<RagCitation>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rag_chat_payload_keys_are_the_documented_wire_names() {
        // Services in other languages match on these literals — a rename here
        // silently breaks every one of them, so pin the encoding, key names
        // included (the exact wire key sets are the contract).
        let request = serde_json::to_value(RagChatRequestPayload {
            message: "ikinci yasa nedir?".into(),
            asker: "01ASKER".into(),
            asker_role: "student".into(),
            scope: vec![RagScopePair {
                sinif: Some("11".into()),
                ders: "Fizik".into(),
            }],
            history: vec![RagTurn {
                role: ChatRole::User,
                content: "merhaba".into(),
            }],
        })
        .unwrap();
        assert_eq!(
            request,
            json!({
                "message": "ikinci yasa nedir?",
                "asker": "01ASKER",
                "asker_role": "student",
                "scope": [{ "sinif": "11", "ders": "Fizik" }],
                "history": [{ "role": "user", "content": "merhaba" }],
            })
        );

        let reply = serde_json::to_value(RagChatReplyPayload {
            text: "F = m·a [1]".into(),
            abstained: false,
            reason: String::new(),
            citations: vec![RagCitation {
                n: 1,
                doc_id: "01DOC".into(),
                pages: vec![3],
                span_ids: vec!["s-7".into()],
                ders: Some("Fizik".into()),
            }],
        })
        .unwrap();
        assert_eq!(
            reply,
            json!({
                "text": "F = m·a [1]",
                "abstained": false,
                "reason": "",
                "citations": [{
                    "n": 1,
                    "doc_id": "01DOC",
                    "pages": [3],
                    "span_ids": ["s-7"],
                    "ders": "Fizik",
                }],
            })
        );

        // The three optional fields default to empty on a minimal service
        // reply, and a scope pair may omit `sinif`.
        let bare: RagChatReplyPayload =
            serde_json::from_value(json!({ "text": "bilmiyorum", "abstained": true })).unwrap();
        assert_eq!(bare.reason, "");
        assert!(bare.citations.is_empty());
        let pair: RagScopePair = serde_json::from_value(json!({ "ders": "Matematik" })).unwrap();
        assert_eq!(pair.sinif, None);
    }
}
