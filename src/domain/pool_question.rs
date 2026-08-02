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
//! this module owns the rows.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{
    MAX_POOL_QUESTION_BODY_LEN, MAX_POOL_QUESTION_TITLE_LEN, POOL_QUESTION_TABLE, STATUS_APPROVED,
    STATUS_PENDING,
};
use crate::database::Database;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::note_file::FileContentType;
use crate::domain::solution::Solution;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_required;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct PoolQuestionId(RecordId);

impl PoolQuestionId {
    /// Minted from the process-wide monotonic generator, not `Ulid::new()`:
    /// the pool sorts `asked_at DESC, id DESC` and the id *is* the tie-break ([`PoolQuestion::list_all`]),
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
    id: PoolQuestionId,
    asker: UserId,
    title: PoolQuestionTitle,
    body: PoolQuestionBody,
    status: String,
    asked_at: Timestamp,
    approved_by: Option<UserId>,
    /// The photo's on-disk blob name — a fresh ULID every upload; `None` when
    /// the question carries no image.
    image_file: Option<String>,
    image_content_type: Option<FileContentType>,
    image_size: Option<i64>,
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

    pub async fn insert(self, db: &Database) -> Result<PoolQuestion, AppError> {
        // whole-row-save-ok: create of a fresh ULID row built in place by `new` — there is no prior row to clobber
        let created: Option<PoolQuestion> = db.create(self.id.record()).content(self).await?;
        created.ok_or_else(|| AppError::Internal("failed to create pool question".into()))
    }

    pub async fn read(
        id: &PoolQuestionId,
        db: &Database,
    ) -> Result<Option<PoolQuestion>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// Every question, newest first — the teacher+ view (approval queue and
    /// pool in one list).
    pub async fn list_all(db: &Database) -> Result<Vec<PoolQuestion>, AppError> {
        let mut result = db
            .query("SELECT * FROM pool_question ORDER BY asked_at DESC, id DESC")
            .await?
            .check()?;
        Ok(result.take::<Vec<PoolQuestion>>(0)?)
    }

    /// The pool as a non-staff user sees it, newest first: every approved
    /// question, plus the caller's own pending ones.
    pub async fn list_visible_to(
        user: &UserId,
        db: &Database,
    ) -> Result<Vec<PoolQuestion>, AppError> {
        let mut result = db
            .query(
                "SELECT * FROM pool_question WHERE status = $approved OR asker = $usr \
                 ORDER BY asked_at DESC, id DESC",
            )
            .bind(("approved", STATUS_APPROVED))
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<PoolQuestion>>(0)?)
    }

    /// Publish a pending question into the pool, stamping who approved it.
    /// The `WHERE status = $pending` guard makes the transition atomic: of two
    /// racing approvals exactly one wins, and an approve can never land on a
    /// question deleted mid-flight. `None` = the question wasn't pending
    /// (already approved, or gone) — the caller sorts out which.
    pub async fn approve(
        id: &PoolQuestionId,
        approver: &UserId,
        db: &Database,
    ) -> Result<Option<PoolQuestion>, AppError> {
        let mut result = db
            .query(
                "UPDATE $q SET status = $approved, approved_by = $by \
                 WHERE status = $pending RETURN AFTER",
            )
            .bind(("q", id.record()))
            .bind(("approved", STATUS_APPROVED))
            .bind(("by", approver.record()))
            .bind(("pending", STATUS_PENDING))
            .await?
            .check()?;
        Ok(result.take::<Vec<PoolQuestion>>(0)?.into_iter().next())
    }

    /// Point the question at a freshly written image blob. Guarded on
    /// `pending` for the same reason approval is: an image landing after
    /// approval would put unmoderated bytes in the pool, so an upload that
    /// loses the race gets `None` (and the caller takes the orphan blob back
    /// off disk). Returns the *before* row — its `image_file` is the replaced
    /// blob the caller must remove.
    pub async fn set_image(
        id: &PoolQuestionId,
        file: &str,
        content_type: &FileContentType,
        size: i64,
        db: &Database,
    ) -> Result<Option<PoolQuestion>, AppError> {
        let mut result = db
            .query(
                "UPDATE $q SET image_file = $file, image_content_type = $ct, image_size = $size \
                 WHERE status = $pending RETURN BEFORE",
            )
            .bind(("q", id.record()))
            .bind(("file", file.to_string()))
            .bind(("ct", content_type.clone()))
            .bind(("size", size))
            .bind(("pending", STATUS_PENDING))
            .await?
            .check()?;
        Ok(result.take::<Vec<PoolQuestion>>(0)?.into_iter().next())
    }

    /// Detach the question's image (pending only, like `set_image`). Returns
    /// the *before* row — its `image_file` is the blob the caller must remove.
    pub async fn clear_image(
        id: &PoolQuestionId,
        db: &Database,
    ) -> Result<Option<PoolQuestion>, AppError> {
        let mut result = db
            .query(
                "UPDATE $q SET image_file = NONE, image_content_type = NONE, image_size = NONE \
                 WHERE status = $pending RETURN BEFORE",
            )
            .bind(("q", id.record()))
            .bind(("pending", STATUS_PENDING))
            .await?
            .check()?;
        Ok(result.take::<Vec<PoolQuestion>>(0)?.into_iter().next())
    }

    /// Delete the question and its solutions in one transaction, returning
    /// the removed rows — the question's *and* the swept solutions' — so the
    /// caller can take every image blob off disk (solutions carry photos too,
    /// and a row-only sweep would strand theirs forever).
    pub async fn delete(
        id: &PoolQuestionId,
        db: &Database,
    ) -> Result<Option<(PoolQuestion, Vec<Solution>)>, AppError> {
        let mut result = db
            .query(
                "BEGIN TRANSACTION;
                 DELETE solution WHERE question = $q RETURN BEFORE;
                 DELETE $q RETURN BEFORE;
                 COMMIT TRANSACTION;",
            )
            .bind(("q", id.record()))
            .await?
            .check()?;
        // Slots count BEGIN: the solution sweep is slot 1, the question slot 2.
        let solutions = result.take::<Vec<Solution>>(1)?;
        Ok(result
            .take::<Vec<PoolQuestion>>(2)?
            .into_iter()
            .next()
            .map(|question| (question, solutions)))
    }
}

#[cfg(test)]
mod tests {
    use ulid::Ulid;

    use super::*;
    use crate::database;
    use crate::domain::solution::{Solution, SolutionBody};

    fn question(asker: &UserId) -> PoolQuestion {
        PoolQuestion::new(
            asker,
            PoolQuestionTitle::try_new("Bu integral nasıl çözülür?").unwrap(),
            PoolQuestionBody::try_new("∫x·eˣ dx adım adım?").unwrap(),
        )
    }

    #[tokio::test]
    async fn approval_is_a_one_way_race_safe_transition() {
        let db = database::init_mem().await.unwrap();
        let asker = UserId::from_key(&Ulid::new().to_string());
        let teacher = UserId::from_key(&Ulid::new().to_string());

        let q = question(&asker).insert(&db).await.unwrap();
        assert_eq!(q.get_status(), STATUS_PENDING);
        assert!(q.get_approved_by().is_none());

        let approved = PoolQuestion::approve(q.get_id(), &teacher, &db)
            .await
            .unwrap()
            .unwrap();
        assert!(approved.is_approved());
        assert_eq!(approved.get_approved_by(), Some(&teacher));

        // A second approval finds nothing pending.
        assert!(
            PoolQuestion::approve(q.get_id(), &teacher, &db)
                .await
                .unwrap()
                .is_none()
        );
        // And a missing question approves to None, not an error.
        assert!(
            PoolQuestion::approve(&PoolQuestionId::generate(), &teacher, &db)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn visibility_hides_others_pending_questions() {
        let db = database::init_mem().await.unwrap();
        let asker = UserId::from_key(&Ulid::new().to_string());
        let other = UserId::from_key(&Ulid::new().to_string());
        let teacher = UserId::from_key(&Ulid::new().to_string());

        let pending = question(&asker).insert(&db).await.unwrap();
        let published = question(&other).insert(&db).await.unwrap();
        PoolQuestion::approve(published.get_id(), &teacher, &db)
            .await
            .unwrap()
            .unwrap();

        // The asker sees their own pending question plus the pool; a stranger
        // sees only the pool; the teacher view (list_all) sees everything.
        assert_eq!(
            PoolQuestion::list_visible_to(&asker, &db)
                .await
                .unwrap()
                .len(),
            2
        );
        let stranger_view = PoolQuestion::list_visible_to(&other, &db).await.unwrap();
        assert_eq!(stranger_view.len(), 1);
        assert_eq!(stranger_view[0].get_id(), published.get_id());
        assert_eq!(PoolQuestion::list_all(&db).await.unwrap().len(), 2);
        assert!(
            PoolQuestion::read(pending.get_id(), &db)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn image_attaches_only_while_pending_and_reports_the_replaced_blob() {
        let db = database::init_mem().await.unwrap();
        let asker = UserId::from_key(&Ulid::new().to_string());
        let teacher = UserId::from_key(&Ulid::new().to_string());
        let png = FileContentType::try_new("image/png").unwrap();

        let q = question(&asker).insert(&db).await.unwrap();
        let before = PoolQuestion::set_image(q.get_id(), "blob_a", &png, 3, &db)
            .await
            .unwrap()
            .unwrap();
        assert!(before.get_image_file().is_none());

        // A replace reports the old blob for cleanup.
        let before = PoolQuestion::set_image(q.get_id(), "blob_b", &png, 5, &db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(before.get_image_file(), Some("blob_a"));
        let stored = PoolQuestion::read(q.get_id(), &db).await.unwrap().unwrap();
        assert_eq!(stored.get_image_file(), Some("blob_b"));
        assert_eq!(stored.get_image_size(), Some(5));

        // Clearing reports the detached blob and empties the fields.
        let before = PoolQuestion::clear_image(q.get_id(), &db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(before.get_image_file(), Some("blob_b"));
        assert!(
            PoolQuestion::read(q.get_id(), &db)
                .await
                .unwrap()
                .unwrap()
                .get_image_file()
                .is_none()
        );

        // Once approved, the image is frozen with the rest of the content.
        PoolQuestion::approve(q.get_id(), &teacher, &db)
            .await
            .unwrap()
            .unwrap();
        assert!(
            PoolQuestion::set_image(q.get_id(), "blob_c", &png, 7, &db)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            PoolQuestion::clear_image(q.get_id(), &db)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn delete_cascades_solutions_and_returns_the_row() {
        let db = database::init_mem().await.unwrap();
        let asker = UserId::from_key(&Ulid::new().to_string());
        let helper = UserId::from_key(&Ulid::new().to_string());

        let q = question(&asker).insert(&db).await.unwrap();
        let offered = Solution::new(
            q.get_id(),
            &helper,
            SolutionBody::try_new("Kısmi integrasyon uygula.").unwrap(),
        )
        .insert(&db)
        .await
        .unwrap();

        // The swept solutions ride back with the question so the caller can
        // take their image blobs off disk.
        let (removed, swept) = PoolQuestion::delete(q.get_id(), &db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(removed.get_id(), q.get_id());
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].get_id(), offered.get_id());
        assert!(PoolQuestion::read(q.get_id(), &db).await.unwrap().is_none());
        assert!(
            Solution::list_for(q.get_id(), None, 0, &db)
                .await
                .unwrap()
                .0
                .is_empty()
        );

        // Deleting the already-deleted is None, not an error.
        assert!(
            PoolQuestion::delete(q.get_id(), &db)
                .await
                .unwrap()
                .is_none()
        );
    }
}
