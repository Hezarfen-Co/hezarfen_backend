//! What an AI service produced for a course note — an index, a summary, an
//! embedding manifest: the backend does not read into it. The row is the
//! backend's own copy of that output; the AI service never writes here (it
//! answers a request over the bridge, and this side stores the answer), which
//! is what keeps the api-read bridge GET-only.
//!
//! Derived data, so it is disposable: the note is the source of truth and a
//! row here can be dropped and regenerated at any time. It therefore cascades
//! from both sides of what it was built from —
//! [`delete_for_note`](crate::db::rag_output::delete_for_note) when the note
//! goes, [`delete_with_source`](crate::db::rag_output::delete_with_source)
//! when one of the attachments it was built from goes — so a stale output
//! never outlives its input, even in a deployment with no AI service
//! connected. Persistence lives in [`crate::db::rag_output`].

use serde_json::Value;
use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::RAG_OUTPUT_TABLE;
use crate::domain::course::CourseId;
use crate::domain::course_note::CourseNoteId;
use crate::domain::course_note_file::CourseNoteFileId;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::timestamp::Timestamp;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct RagOutputId(RecordId);

impl RagOutputId {
    /// Minted from the process-wide monotonic generator, not `Ulid::new()`:
    /// a note's outputs list `id DESC` (newest first,
    /// [`list_for`](crate::db::rag_output::list_for)).
    pub fn generate() -> Self {
        Self(RecordId::new(RAG_OUTPUT_TABLE, next_ulid().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(RAG_OUTPUT_TABLE, key))
    }

    pub fn record(&self) -> RecordId {
        self.0.clone()
    }

    pub fn key(&self) -> &str {
        match &self.0.key {
            RecordIdKey::String(key) => key,
            _ => "",
        }
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct RagOutput {
    pub(crate) id: RagOutputId,
    pub(crate) course_note: CourseNoteId,
    /// The note's course, denormalised so a course-wide read needs no join.
    pub(crate) course: CourseId,
    /// The attachments the output was built from, as they stood at generation
    /// time. Deleting any one of them drops this row
    /// ([`delete_with_source`](crate::db::rag_output::delete_with_source))
    /// rather than leaving an output citing a file that no longer exists.
    pub(crate) sources: Vec<CourseNoteFileId>,
    /// The service's answer, stored verbatim. Opaque to the backend — it is a
    /// service-owned shape, so this side neither validates nor interprets it,
    /// beyond it having to be a JSON **object** (the stored column is one).
    pub(crate) payload: Value,
    pub(crate) generated_at: Timestamp,
}

impl RagOutput {
    pub fn get_id(&self) -> &RagOutputId {
        &self.id
    }

    pub fn get_course_note(&self) -> &CourseNoteId {
        &self.course_note
    }

    pub fn get_course(&self) -> &CourseId {
        &self.course
    }

    pub fn get_sources(&self) -> &[CourseNoteFileId] {
        &self.sources
    }

    pub fn get_payload(&self) -> &Value {
        &self.payload
    }

    pub fn get_generated_at(&self) -> Timestamp {
        self.generated_at
    }
}
