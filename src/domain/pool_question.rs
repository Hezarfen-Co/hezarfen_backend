//! A student-asked question on its way into the school-wide question pool.
//! Born `pending` (visible to its asker and to teacher+, who run the approval
//! queue); a teacher+ approval flips it to `approved`, which publishes it to
//! the whole school for everyone to read and offer solutions on. Approved
//! content is frozen — edits after approval would bypass moderation — so the
//! only post-approval change is deletion (asker or teacher+), which takes the
//! question's solutions with it. An optional photo of the problem is its own
//! row in `pool_question_image` — the child-table shape of
//! [`crate::domain::question_image`] with a single (implicit) slot — and the
//! bytes live on disk under [`crate::config::Config::files_path`] in a file
//! named by that row's `file` — a fresh server-generated id per upload. The
//! web layer owns the blob I/O; the queries live in
//! [`crate::db::pool_question`], the workflows in
//! [`crate::service::pool_question`].

use uuid::Uuid;

use crate::constant::{
    MAX_POOL_QUESTION_BODY_LEN, MAX_POOL_QUESTION_TITLE_LEN, STATUS_APPROVED, STATUS_PENDING,
};
use crate::domain::monotonic_id::next_uuid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::validate_required;

/// Typed pool-question row id. A UUIDv7 minted by the process-wide monotonic
/// generator, so `id` order is mint order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct PoolQuestionId(Uuid);

impl PoolQuestionId {
    /// Minted from the process-wide monotonic generator, not a random v4:
    /// the pool sorts `asked_at DESC, id DESC` and the id *is* the tie-break
    /// ([`crate::db::pool_question::list`]),
    /// and a random low half scrambles rows minted in the same millisecond.
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

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
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

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
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

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PoolQuestion {
    pub(crate) id: PoolQuestionId,
    pub(crate) asker: UserId,
    pub(crate) title: PoolQuestionTitle,
    pub(crate) body: PoolQuestionBody,
    pub(crate) status: String,
    pub(crate) asked_at: Timestamp,
    pub(crate) approved_by: Option<UserId>,
}

impl PoolQuestion {
    pub fn new(asker: &UserId, title: PoolQuestionTitle, body: PoolQuestionBody) -> Self {
        Self {
            id: PoolQuestionId::generate(),
            asker: *asker,
            title,
            body,
            status: STATUS_PENDING.to_string(),
            asked_at: Timestamp::now(),
            approved_by: None,
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
}
