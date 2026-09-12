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
use sqlx::types::Json;
use uuid::Uuid;

use crate::domain::course::CourseId;
use crate::domain::course_note::CourseNoteId;
use crate::domain::course_note_file::CourseNoteFileId;
use crate::domain::monotonic_id::next_uuid;
use crate::domain::timestamp::Timestamp;

/// Typed rag-output row id. A UUIDv7 minted by the process-wide monotonic
/// generator, so `id` order is mint order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct RagOutputId(Uuid);

impl RagOutputId {
    /// Minted from the process-wide monotonic generator, not a random v4:
    /// a note's outputs list `id DESC` (newest first,
    /// [`list_for`](crate::db::rag_output::list_for)).
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    /// The inner uuid, for runtime-checked binds (Param/QueryBuilder) that
    /// cannot take the newtype. Static `query!` binds take `self` directly.
    pub fn uuid(&self) -> Uuid {
        self.0
    }

    /// Parse a wire key. A key that parses as no UUID — a malformed path
    /// segment — reads as the nil id, which matches no row: exactly the 404 a
    /// dangling record key produced under the old store, without turning a
    /// typo into a panic.
    pub fn from_key(key: &str) -> Self {
        Self(Uuid::parse_str(key).unwrap_or(Uuid::nil()))
    }

    /// The hyphenated wire form.
    pub fn key(&self) -> String {
        self.0.to_string()
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
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
    /// The service's answer, stored verbatim in a JSONB column. Opaque to the
    /// backend — it is a service-owned shape, so this side neither validates
    /// nor interprets it, beyond it having to be a JSON **object** (the
    /// stored column is one).
    pub(crate) payload: Json<Value>,
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
        &self.payload.0
    }

    pub fn get_generated_at(&self) -> Timestamp {
        self.generated_at
    }
}
