//! Payload contract for the `rag.index` capability, and the dispatch behind
//! it.
//!
//! Unlike [`chat`](crate::ai::chat) — a user waiting on an answer — this one
//! is a background refresh: a course note changed, so whatever a service had
//! indexed for it is stale. [`spawn_index`] therefore returns immediately and
//! the round trip runs in its own task, because no course-note handler may
//! slow down, fail, or 503 on account of an AI service.
//!
//! Failure is silence by design: no worker, a timeout, a transport error or an
//! unreadable answer all leave the previously stored
//! [`RagOutput`](crate::domain::rag_output::RagOutput) rows exactly as they
//! are. A slightly stale index beats no index.
//!
//! A service restart wipes an in-memory index and does not replay it.
//! [`spawn_index_replay`] runs when a worker that offers `rag.index`
//! registers, and calls [`index_course_note`] for every course note that has
//! files, in every active school. The spawn returns immediately: the QUIC
//! handshake must not wait on it. A chatbot, zeka, or podcast registration
//! does not offer `rag.index`, so it does not replay.

use serde::{Deserialize, Serialize};

use crate::constant::AI_RAG_INDEX_TIMEOUT_SECS;
use crate::database::Database;
use crate::domain::course_note::CourseNote;
use crate::domain::course_note_file::CourseNoteFile;
use crate::module::Module;
use crate::state::AppState;
use crate::tenant::ResolvedTenant;

/// The capability string routed to an indexing service. Defined once, in
/// [`crate::constant`]; re-exported here so a reader of the payload contract
/// finds it next to the payloads.
pub use crate::constant::AI_RAG_INDEX_CAPABILITY;

/// One attachment of the note, metadata only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RagFile {
    /// The file's record key — the same id `GET /course-notes/{id}/files`
    /// publishes, so a service that wants the bytes fetches them itself.
    pub id: String,
    pub name: String,
    pub content_type: String,
    pub size: i64,
}

/// What the backend asks an indexing service to index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RagIndexPayload {
    /// The course note's record key. The service echoes nothing back about it
    /// — the backend keys the stored answer itself.
    pub course_note: String,
    pub course: String,
    /// The note author's user id — who a service reads the file bytes
    /// `on_behalf_of` on the blob stream, since the note's own teacher can
    /// always view its course and the `ai` principal never can.
    pub author: String,
    pub title: String,
    pub content: String,
    /// Attachment **metadata only** — the bytes ride their own QUIC stream
    /// (one `BlobRequest` per `files[].id`), never a JSON frame.
    #[serde(default)]
    pub files: Vec<RagFile>,
}

impl RagIndexPayload {
    fn new(note: &CourseNote, files: &[CourseNoteFile]) -> Self {
        Self {
            course_note: note.get_id().key().to_string(),
            course: note.get_course().key().to_string(),
            author: note.get_author().key().to_string(),
            title: note.get_title().as_str().to_string(),
            content: note.get_content().as_str().to_string(),
            files: files
                .iter()
                .map(|file| RagFile {
                    id: file.get_id().key().to_string(),
                    name: file.get_name().as_str().to_string(),
                    content_type: file.get_content_type().as_str().to_string(),
                    size: file.get_size(),
                })
                .collect(),
        }
    }
}

/// Re-index `note` and replace its stored outputs with what the service
/// answered. Silent no-op when the bridge is off or no worker offers
/// `rag.index`.
///
/// `tenant` is the caller's own school: its slug rides the `hab/2` frame, and
/// `state.db` — the same school's database, since the caller reached this
/// through the shadow `State` — is where the answer is stored.
///
/// A school that did not buy the `ai` package sends **nothing** to an AI
/// service: no dispatch, no `rag_output` row. That is `Module::Chatbot`, the
/// package's only module, so it gates every outbound dispatch and not just the
/// `/chatbot` nest.
///
/// The answer may also name the doc id the service minted per file — `files`
/// as `[{id, doc_id}]` — which is stamped onto the matching attachment rows,
/// and only onto this note's own files. Best-effort: a failed stamp is logged
/// and the output still lands, because the map is optional on the wire and a
/// service that omits it is simply not heard from on that front.
pub async fn index_course_note(state: &AppState, tenant: &ResolvedTenant, note: &CourseNote) {
    if !tenant.modules.contains(Module::Chatbot) {
        tracing::debug!(
            "skipping rag.index for course note {}: the `{}` school has no `chatbot` module",
            note.get_id().key(),
            tenant.slug
        );
        return;
    }
    let school = &tenant.slug;
    let Some(bridge) = state.ai.as_ref() else {
        return;
    };
    if !bridge.has_capability(AI_RAG_INDEX_CAPABILITY) {
        return;
    }
    // Read the attachments here rather than taking a caller's list: the
    // dispatch runs after the handler answered, so this is the freshest view
    // of what the note holds, and the ids it sends are the ids it stores.
    let files = match crate::db::course_note_file::list_for(&state.db, note.get_id(), None, 0).await
    {
        Ok((files, _)) => files,
        Err(err) => {
            tracing::warn!("could not load course note files for rag.index: {err}");
            return;
        }
    };
    let payload = match serde_json::to_value(RagIndexPayload::new(note, &files)) {
        Ok(payload) => payload,
        Err(err) => {
            tracing::error!("could not encode a rag.index request: {err}");
            return;
        }
    };

    let answer = match bridge
        .dispatch_with_timeout(
            school,
            AI_RAG_INDEX_CAPABILITY,
            payload,
            std::time::Duration::from_secs(AI_RAG_INDEX_TIMEOUT_SECS),
        )
        .await
    {
        Ok(answer) => answer,
        Err(err) => {
            tracing::warn!(
                "rag.index failed for course note {}: {err}",
                note.get_id().key()
            );
            return;
        }
    };
    // The service is a trust boundary and the column is an object: anything
    // else is a service bug, and storing it would only fail at the write.
    if !answer.is_object() {
        tracing::warn!("rag.index answered with a non-object payload — keeping the stored rows");
        return;
    }

    // A service may name the doc id it minted for each file — `files` as
    // `[{id, doc_id}]` — so a row remembers what its bytes were indexed as.
    // Only ids of *this* note's attachments are honoured: a service bug
    // naming another note's file must not stamp that row. Best-effort — a
    // failed write is logged and the stored answer still lands.
    if let Some(claims) = answer.get("files").and_then(serde_json::Value::as_array) {
        for claim in claims {
            let (Some(id), Some(doc_id)) = (
                claim.get("id").and_then(serde_json::Value::as_str),
                claim.get("doc_id").and_then(serde_json::Value::as_str),
            ) else {
                continue;
            };
            let Some(file) = files.iter().find(|file| file.get_id().key() == id) else {
                continue;
            };
            if let Err(err) =
                crate::db::course_note_file::set_rag_doc_id(&state.db, file.get_id(), doc_id).await
            {
                tracing::warn!(
                    "could not stamp doc id on course note file {}: {err}",
                    file.get_id().key()
                );
            }
        }
    } else {
        tracing::debug!(
            "the rag.index answer for course note {} names no doc ids",
            note.get_id().key()
        );
    }

    let sources = files.iter().map(|file| file.get_id().clone()).collect();
    if let Err(err) = replace(&state.db, note, sources, answer).await {
        tracing::warn!("could not store the rag.index output: {err}");
    }
}

/// Swap the note's outputs for the fresh one. Create-then-drop-older, not an
/// update: the previous output belongs to a previous version of the note, and
/// this order is what makes two concurrent index tasks converge on one row
/// ([`replace_for_note`](crate::db::rag_output::replace_for_note)).
async fn replace(
    db: &Database,
    note: &CourseNote,
    sources: Vec<crate::domain::course_note_file::CourseNoteFileId>,
    payload: serde_json::Value,
) -> Result<(), crate::error::AppError> {
    crate::db::rag_output::replace_for_note(db, note.get_id(), note.get_course(), sources, payload)
        .await?;
    Ok(())
}

/// [`index_course_note`] off the request path: the handler has already
/// answered by the time the service is asked.
pub fn spawn_index(state: &AppState, tenant: &ResolvedTenant, note: CourseNote) {
    // Cheap when AI is off — the spawned task returns on the first `let else`.
    let state = state.clone();
    let tenant = tenant.clone();
    tokio::spawn(async move { index_course_note(&state, &tenant, &note).await });
}

/// Whether a worker that just registered should have the course-note index
/// replayed. Only `rag.index` does. `rag.chat`, the chatbot, zeka, and the
/// podcast worker do not — their registration must not walk every school.
pub(crate) fn should_replay_index(capabilities: &[String]) -> bool {
    capabilities
        .iter()
        .any(|capability| capability == AI_RAG_INDEX_CAPABILITY)
}

/// [`replay_indexed_notes`] off the handshake path. Returns as soon as the
/// task is spawned. `state.ai` must already be the bridge that just
/// registered the worker; `state.db` may still be the control database —
/// each school is swapped in before [`index_course_note`].
pub(crate) fn spawn_index_replay(state: AppState) {
    tokio::spawn(async move {
        replay_indexed_notes(state).await;
    });
}

/// Re-dispatch [`index_course_note`] for every course note that has files, in
/// every active school. One note at a time: a replay is a catch-up, not a
/// stampede, and the worker's own concurrency cap is what bounds a single
/// dispatch.
async fn replay_indexed_notes(state: AppState) {
    let directory = match crate::db::insight::active_schools(state.tenants.control()).await {
        Ok(directory) => directory,
        Err(err) => {
            tracing::warn!("rag.index replay could not list active schools: {err}");
            return;
        }
    };
    for slug in directory.schools {
        replay_school(&state, &slug).await;
    }
}

async fn replay_school(state: &AppState, slug: &str) {
    let slug = match crate::tenant::Slug::try_new(slug) {
        Ok(slug) => slug,
        Err(err) => {
            tracing::warn!("rag.index replay skipped an unreadable school slug: {err}");
            return;
        }
    };
    let tenant = match state.tenants.resolve(&slug).await {
        Ok(tenant) => tenant,
        Err(err) => {
            tracing::warn!("rag.index replay skipped `{slug}`: {err}");
            return;
        }
    };
    if !tenant.modules.contains(Module::Chatbot) {
        tracing::debug!("rag.index replay skipped `{slug}`: the school has no chatbot module");
        return;
    }
    let notes = match crate::db::course_note::list_with_files(&tenant.db).await {
        Ok(notes) => notes,
        Err(err) => {
            tracing::warn!("rag.index replay could not list course notes in `{slug}`: {err}");
            return;
        }
    };
    if notes.is_empty() {
        tracing::debug!("rag.index replay: `{slug}` has no course notes with files");
        return;
    }
    tracing::info!(
        school = %slug,
        notes = notes.len(),
        "replaying rag.index for course notes with files"
    );
    // `index_course_note` writes through `state.db`, which on the request
    // path is the shadow school's database. The root handed in here is the
    // control database, so the school handle has to be swapped in first.
    let school_state = AppState {
        db: tenant.db.clone(),
        ..state.clone()
    };
    for note in &notes {
        index_course_note(&school_state, &tenant, note).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::{
        AI_CHAT_CAPABILITY, AI_INSIGHT_STUDENT_CAPABILITY, AI_PODCAST_SUBMIT_CAPABILITY,
        AI_RAG_CHAT_CAPABILITY,
    };
    use serde_json::json;

    #[test]
    fn rag_payload_keys_are_the_documented_wire_names() {
        // Services in other languages match on these literals — a rename here
        // silently breaks every one of them, so pin the encoding.
        let payload = serde_json::to_value(RagIndexPayload {
            course_note: "01NOTE".into(),
            course: "01COURSE".into(),
            author: "01AUTHOR".into(),
            title: "Chapter 3".into(),
            content: "quadratics".into(),
            files: vec![RagFile {
                id: "01FILE".into(),
                name: "recap.pdf".into(),
                content_type: "application/pdf".into(),
                size: 12,
            }],
        })
        .unwrap();
        assert_eq!(
            payload,
            json!({
                "course_note": "01NOTE",
                "course": "01COURSE",
                "author": "01AUTHOR",
                "title": "Chapter 3",
                "content": "quadratics",
                "files": [{
                    "id": "01FILE",
                    "name": "recap.pdf",
                    "content_type": "application/pdf",
                    "size": 12,
                }],
            })
        );
        assert_eq!(AI_RAG_INDEX_CAPABILITY, "rag.index");
        // A note with no attachments is the common case — `files` is optional
        // on the wire, so a minimal service need not send it back or expect it.
        let bare: RagIndexPayload = serde_json::from_value(json!({
            "course_note": "01NOTE", "course": "01COURSE", "author": "01AUTHOR",
            "title": "t", "content": "c",
        }))
        .unwrap();
        assert!(bare.files.is_empty());
    }

    /// Chatbot, zeka, podcast, and even `rag.chat` must not replay. A worker
    /// that offers `rag.index` alongside something else still does.
    #[test]
    fn index_replay_runs_only_when_the_worker_offers_rag_index() {
        assert!(should_replay_index(&[AI_RAG_INDEX_CAPABILITY.to_string()]));
        assert!(should_replay_index(&[
            AI_CHAT_CAPABILITY.to_string(),
            AI_RAG_INDEX_CAPABILITY.to_string(),
        ]));
        assert!(!should_replay_index(&[AI_CHAT_CAPABILITY.to_string()]));
        assert!(!should_replay_index(&[AI_RAG_CHAT_CAPABILITY.to_string()]));
        assert!(!should_replay_index(&[
            AI_INSIGHT_STUDENT_CAPABILITY.to_string()
        ]));
        assert!(!should_replay_index(&[
            AI_PODCAST_SUBMIT_CAPABILITY.to_string()
        ]));
        assert!(!should_replay_index(&[]));
    }
}
