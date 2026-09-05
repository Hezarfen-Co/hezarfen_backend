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

use serde::{Deserialize, Serialize};

use crate::constant::AI_RAG_INDEX_TIMEOUT_SECS;
use crate::database::Database;
use crate::domain::course_note::CourseNote;
use crate::domain::course_note_file::CourseNoteFile;
use crate::domain::rag_output::RagOutput;
use crate::state::AppState;
use crate::tenant::Slug;

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
/// `school` is the caller's own school: it rides the `hab/2` frame, and
/// `state.db` — the same school's database, since the caller reached this
/// through the shadow `State` — is where the answer is stored.
pub async fn index_course_note(state: &AppState, school: &Slug, note: &CourseNote) {
    let Some(bridge) = state.ai.as_ref() else {
        return;
    };
    if !bridge.has_capability(AI_RAG_INDEX_CAPABILITY) {
        return;
    }
    // Read the attachments here rather than taking a caller's list: the
    // dispatch runs after the handler answered, so this is the freshest view
    // of what the note holds, and the ids it sends are the ids it stores.
    let files = match CourseNoteFile::list_for(note.get_id(), None, 0, &state.db).await {
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

    let sources = files.iter().map(|file| file.get_id().clone()).collect();
    if let Err(err) = replace(&state.db, note, sources, answer).await {
        tracing::warn!("could not store the rag.index output: {err}");
    }
}

/// Swap the note's outputs for the fresh one. Create-then-drop-older, not an
/// update: the previous output belongs to a previous version of the note, and
/// this order is what makes two concurrent index tasks converge on one row
/// ([`RagOutput::replace_for_note`]).
async fn replace(
    db: &Database,
    note: &CourseNote,
    sources: Vec<crate::domain::course_note_file::CourseNoteFileId>,
    payload: serde_json::Value,
) -> Result<(), crate::error::AppError> {
    RagOutput::replace_for_note(note.get_id(), note.get_course(), sources, payload, db).await?;
    Ok(())
}

/// [`index_course_note`] off the request path: the handler has already
/// answered by the time the service is asked.
pub fn spawn_index(state: &AppState, school: &Slug, note: CourseNote) {
    // Cheap when AI is off — the spawned task returns on the first `let else`.
    let state = state.clone();
    let school = school.clone();
    tokio::spawn(async move { index_course_note(&state, &school, &note).await });
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
