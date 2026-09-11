//! A student-asked question on its way into the school-wide question pool.
//! Born `pending` (visible to its asker and to teacher+, who run the approval
//! queue); a teacher+ approval flips it to `approved`, which publishes it to
//! the whole school for everyone to read and offer solutions on. Approved
//! content is frozen — edits after approval would bypass moderation — so the
//! only post-approval change is deletion (asker or teacher+), which takes the
//! question's solutions with it. An optional photo of the problem rides on
//! the row as metadata (`image_*`); the bytes live on disk under
//! [`crate::config::Config::files_path`] in a file named by `image_file` — a
//! fresh server-generated ULID per upload. The web layer owns the blob I/O;
//! the queries live in [`crate::db::pool_question`], the workflows in
//! [`crate::service::pool_question`].

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{
    MAX_POOL_QUESTION_BODY_LEN, MAX_POOL_QUESTION_TITLE_LEN, POOL_QUESTION_TABLE, STATUS_APPROVED,
    STATUS_PENDING,
};
use crate::domain::monotonic_id::next_ulid;
use crate::domain::note_file::FileContentType;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::validate_required;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct PoolQuestionId(RecordId);

impl PoolQuestionId {
    /// Minted from the process-wide monotonic generator, not `Ulid::new()`:
    /// the pool sorts `asked_at DESC, id DESC` and the id *is* the tie-break
    /// ([`crate::db::pool_question::list_all`]),
    /// and a random low half scrambles rows minted in the same millisecond.
    pub fn generate() -> Self {
        Self(RecordId::new(POOL_QUESTION_TABLE, next_ulid().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(POOL_QUESTION_TABLE, key))
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

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct PoolQuestionTitle(String);

impl PoolQuestionTitle {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("title", value, MAX_POOL_QUESTION_TITLE_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct PoolQuestionBody(String);

impl PoolQuestionBody {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("body", value, MAX_POOL_QUESTION_BODY_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct PoolQuestion {
    pub(crate) id: PoolQuestionId,
    pub(crate) asker: UserId,
    pub(crate) title: PoolQuestionTitle,
    pub(crate) body: PoolQuestionBody,
    pub(crate) status: String,
    pub(crate) asked_at: Timestamp,
    pub(crate) approved_by: Option<UserId>,
    /// The photo's on-disk blob name — a fresh ULID every upload; `None` when
    /// the question carries no image.
    pub(crate) image_file: Option<String>,
    pub(crate) image_content_type: Option<FileContentType>,
    pub(crate) image_size: Option<i64>,
}

impl PoolQuestion {
    pub fn new(asker: &UserId, title: PoolQuestionTitle, body: PoolQuestionBody) -> Self {
        Self {
            id: PoolQuestionId::generate(),
            asker: asker.clone(),
            title,
            body,
            status: STATUS_PENDING.to_string(),
            asked_at: Timestamp::now(),
            approved_by: None,
            image_file: None,
            image_content_type: None,
            image_size: None,
        }
    }

    pub fn get_id(&self) -> &PoolQuestionId {
        &self.id
    }

    pub fn get_asker(&self) -> &UserId {
        &self.asker
    }

    pub fn get_title(&self) -> &PoolQuestionTitle {
        &self.title
    }

    pub fn get_body(&self) -> &PoolQuestionBody {
        &self.body
    }

    pub fn get_status(&self) -> &str {
        &self.status
    }

    pub fn is_approved(&self) -> bool {
        self.status == STATUS_APPROVED
    }

    pub fn get_asked_at(&self) -> Timestamp {
        self.asked_at
    }

    pub fn get_approved_by(&self) -> Option<&UserId> {
        self.approved_by.as_ref()
    }

    pub fn get_image_file(&self) -> Option<&str> {
        self.image_file.as_deref()
    }

    pub fn get_image_content_type(&self) -> Option<&FileContentType> {
        self.image_content_type.as_ref()
    }

    pub fn get_image_size(&self) -> Option<i64> {
        self.image_size
    }
}
