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
//! `(class, course)` scope the corpus is routed by. The reply returns the answer
//! text plus the citations behind it, so the backend can resolve each cited
//! passage to the course-note file it came from.

use serde::{Deserialize, Serialize};

use crate::ai::chat::ChatRole;
use crate::constant::{
    AI_RAG_CHAT_CAPABILITY, AI_RAG_CHAT_TIMEOUT_SECS, MAX_RAG_CITATIONS, MAX_RAG_CITATION_PAGES,
};

/// One `(class, course)` pair the question is scoped to.
///
/// The corpus is routed by the **pair**: a grade and a subject list sent
/// separately would cross-product into combinations the asker never named, so
/// each pair is one scope the service may retrieve from. The class is optional —
/// a question about a subject across every grade names the course alone — and
/// the course is required, since a subject-less scope retrieves nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RagScopePair {
    /// The class/grade the subject is taught in (`"9"`, `"10"`), or omitted to
    /// scope the subject across grades.
    #[serde(default)]
    pub sinif: Option<String>,
    /// The subject/course name the pair is scoped to (Mathematics, Physics). The
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
    /// The `(class, course)` pairs the question is scoped to — one entry per pair,
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

/// Ask a RAG service one question and resolve what came back.
///
/// The request is built from what the session already proves — `asker` and
/// `asker_role` are read from the live session by the caller, never taken from
/// a body, and `scope` from the asker's own memberships — and the reply is
/// mapped into what a message row can hold: the answer text, the abstention,
/// and the citations whose corpus `doc_id` resolves to a course-note file the
/// asker may view.
///
/// `Err` is a short, stable code for the turn's `error_code` — a transport
/// failure, the service's own `Response::Err` verdict, or `bad_reply` when the
/// reply was unreadable or exceeded a storage cap. Two codes are special:
/// `role_required` and `scope_mismatch` mean the **backend** built a request
/// the service may not answer. The turn still fails — an answer is not safe to
/// invent — but this side of the bridge is the bug, so it is logged as one.
#[allow(clippy::too_many_arguments)] // the whole turn context, spelled once
pub async fn answer(
    db: &crate::database::Database,
    bridge: &crate::ai::AiBridge,
    slug: &crate::tenant::Slug,
    thread: &crate::domain::rag_thread::RagThreadId,
    fresh: &[crate::domain::rag_message::RagMessageId; 2],
    prompt: String,
    asker: &crate::domain::user::UserId,
    asker_role: crate::domain::role::Role,
    scope: Vec<RagScopePair>,
    history: Vec<RagTurn>,
) -> Result<(String, crate::domain::rag_message::RagReply), String> {
    let scope_pairs = scope.len();
    let payload = serde_json::to_value(RagChatRequestPayload {
        message: prompt,
        asker: asker.key(),
        asker_role: asker_role.as_str().to_string(),
        scope,
        history,
    })
    .map_err(|err| {
        tracing::error!("could not encode a rag.chat request: {err}");
        "internal".to_string()
    })?;
    tracing::debug!(
        "dispatching rag.chat for thread {} (answer {}): {scope_pairs} scope pairs",
        thread.key(),
        fresh[1].key()
    );

    let raw = bridge
        .dispatch_with_timeout(
            slug,
            AI_RAG_CHAT_CAPABILITY,
            payload,
            std::time::Duration::from_secs(AI_RAG_CHAT_TIMEOUT_SECS),
        )
        .await
        .map_err(failure_code)?;
    let reply: RagChatReplyPayload = serde_json::from_value(raw).map_err(|err| {
        tracing::warn!("AI service answered with an unreadable rag.chat payload: {err}");
        "bad_reply".to_string()
    })?;

    // The service is a trust boundary: a reply past a storage cap is refused
    // whole rather than clipped. A half-cited answer reads as a complete one,
    // and an unbounded answer is bytes a service chose to write into a
    // school's database.
    if reply.citations.len() > MAX_RAG_CITATIONS {
        tracing::error!(
            "rag.chat returned {} citations, over the {MAX_RAG_CITATIONS} cap",
            reply.citations.len()
        );
        return Err("bad_reply".to_string());
    }
    if let Some(over) = reply
        .citations
        .iter()
        .find(|citation| citation.pages.len() > MAX_RAG_CITATION_PAGES)
    {
        tracing::error!(
            "rag.chat citation {} names {} pages, over the {MAX_RAG_CITATION_PAGES} cap",
            over.n,
            over.pages.len()
        );
        return Err("bad_reply".to_string());
    }

    // The two verdicts that mean the *backend* built a bad request: the scope
    // or the asker's role it sent is not something the service may answer. The
    // turn still fails, but this is a bug on this side of the bridge, so it is
    // logged as one rather than as an ordinary service refusal.
    if matches!(reply.reason.as_str(), "role_required" | "scope_mismatch") {
        tracing::error!(
            "rag.chat refused a request the backend built: {} (asker {})",
            reply.reason,
            asker.key()
        );
        return Err(reply.reason);
    }

    if reply.text.trim().is_empty() && !reply.abstained {
        // Nothing to show, and the domain would refuse to store it anyway. A
        // blank bubble is indistinguishable from a bug, so it is reported as
        // one.
        tracing::warn!("rag.chat answered with a blank, non-abstained reply");
        return Err("empty_reply".to_string());
    }

    let citations = resolve_citations(db, asker, &reply.citations).await;
    Ok((
        reply.text,
        crate::domain::rag_message::RagReply {
            abstained: reply.abstained,
            reason: reply.reason,
            citations,
        },
    ))
}

/// Resolve each citation's corpus `doc_id` to the course-note file that owns
/// it, keeping only a file whose course the asker may view.
///
/// One read per *distinct* `doc_id`: identical PDF bytes resolve to the same
/// document, so one document may be claimed by several files — across courses,
/// which is exactly why the visibility check is per candidate and not per
/// document. The first visible candidate wins, in the store's own order, so two
/// identical turns resolve a citation the same way. A citation whose document
/// no visible file claims keeps every other field and stores `file: null` —
/// the passage stays citable, only not openable.
async fn resolve_citations(
    db: &crate::database::Database,
    asker: &crate::domain::user::UserId,
    citations: &[RagCitation],
) -> Vec<crate::domain::rag_message::RagCitedFile> {
    let user = match crate::service::user::read(db, asker).await {
        Ok(Some(user)) => Some(user),
        // The asker vanished mid-turn (only reachable through a deleted
        // account): nothing can be visible to them, so nothing resolves.
        Ok(None) => {
            tracing::warn!("the asker is gone; citations stay unresolved");
            None
        }
        Err(err) => {
            tracing::warn!("could not read the asker for citation resolution: {err}");
            None
        }
    };

    let mut candidates: std::collections::HashMap<&str, Vec<crate::domain::course_note_file::CourseNoteFile>> =
        std::collections::HashMap::new();
    for citation in citations {
        if !candidates.contains_key(citation.doc_id.as_str()) {
            let files =
                match crate::db::course_note_file::find_by_rag_doc_id(db, &citation.doc_id).await {
                    Ok(files) => files,
                    Err(err) => {
                        tracing::warn!("could not resolve doc {}: {err}", citation.doc_id);
                        Vec::new()
                    }
                };
            candidates.insert(citation.doc_id.as_str(), files);
        }
    }

    let mut resolved = Vec::with_capacity(citations.len());
    for citation in citations {
        let file = match &user {
            Some(user) => {
                let files = candidates
                    .get(citation.doc_id.as_str())
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                first_visible_file(db, user, files).await
            }
            None => None,
        };
        resolved.push(crate::domain::rag_message::RagCitedFile {
            n: citation.n,
            file,
            pages: citation.pages.clone(),
            span_ids: citation.span_ids.clone(),
            ders: citation.ders.clone(),
        });
    }
    resolved
}

/// The first candidate file whose owning course `user` may view, in the
/// store's order; `None` when the document is claimed by no file they can
/// open. A read that fails mid-walk is treated like a file they cannot see —
/// the citation still lands, only unresolved.
async fn first_visible_file(
    db: &crate::database::Database,
    user: &crate::domain::user::User,
    candidates: &[crate::domain::course_note_file::CourseNoteFile],
) -> Option<String> {
    for candidate in candidates {
        let Some(note) = crate::db::course_note::read(db, candidate.get_course_note())
            .await
            .ok()
            .flatten()
        else {
            continue;
        };
        let Some(course) = crate::db::course::read(db, note.get_course()).await.ok().flatten()
        else {
            continue;
        };
        match crate::service::course::can_view_course(&course, user, db).await {
            Ok(true) => return Some(candidate.get_id().key().to_string()),
            Ok(false) => {}
            Err(err) => {
                tracing::warn!("could not check course visibility for a citation: {err}");
            }
        }
    }
    None
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
        // The service's own verdict. Kept verbatim (the domain trims it) so a
        // service can define codes the backend has never heard of.
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
        // reply, and a scope pair may omit the class.
        let bare: RagChatReplyPayload =
            serde_json::from_value(json!({ "text": "bilmiyorum", "abstained": true })).unwrap();
        assert_eq!(bare.reason, "");
        assert!(bare.citations.is_empty());
        let pair: RagScopePair = serde_json::from_value(json!({ "ders": "Matematik" })).unwrap();
        assert_eq!(pair.sinif, None);
    }
}
