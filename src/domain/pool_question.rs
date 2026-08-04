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
    MAX_POOL_QUESTION_BODY_LEN, MAX_POOL_QUESTION_TITLE_LEN, POOL_APPROVED_TOTAL_FIELD,
    POOL_PUBLISHED_TOTAL_FIELD, POOL_QUESTION_TABLE, STATUS_APPROVED, STATUS_PENDING,
};
use crate::database::{Database, transaction_with_retry};
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
    ///
    /// Two lifetime badge counters ride that same guard, inside this
    /// transaction: the approver's `pool_approved_total` and the *asker's*
    /// `pool_published_total`. Both hang off `array::len($done) > 0` — the
    /// guard's own verdict — so the pair moves once per real transition and
    /// never on a second approve of an already-approved question.
    /// (`count ?? 0 > 0` misparses here; `array::len` is the spelling that
    /// holds.) The second conjunct is the self-approval rule below.
    ///
    /// Two rules keep the pair unfarmable, one per side of the transition.
    ///
    /// The asker is credited at *approval*, not at asking: a bare
    /// [`insert`](PoolQuestion::insert) is self-service and the author can
    /// delete their own question and ask again forever, so a counter moved
    /// there is farmable. Approval is teacher-gated and one-way, which is why
    /// the family is named `pool_published` and not `pool_asked`. That closes
    /// the student side.
    ///
    /// And a *self*-approval — a teacher+ approving a question they asked
    /// themselves, which the route deliberately still allows — moves neither
    /// counter, hence the `$done[0].asker != $by` half of the condition. It is
    /// the same farm from the other end (ask, approve, delete, repeat, with no
    /// second person involved), and the two together are why a counter can only
    /// move when one person's work was judged by another's. Nothing else about
    /// a self-approval changes: same 200, same freeze, same stamp — this skips
    /// the credit, not the approval.
    ///
    /// Re-sent while the store answers "conflict, retry": the guard reads a
    /// column a rival approve writes, and both counters sit on user rows every
    /// other counter site writes too. Sound to re-send — every statement is an
    /// `UPDATE`, none of which can legitimately answer "already exists" — and a
    /// lost round aborts having written nothing, increments included.
    pub async fn approve(
        id: &PoolQuestionId,
        approver: &UserId,
        db: &Database,
    ) -> Result<Option<PoolQuestion>, AppError> {
        let (mut result, mut errors) = transaction_with_retry(
            db,
            &format!(
                "BEGIN TRANSACTION;
                 LET $done = (UPDATE $q SET status = $approved, approved_by = $by
                     WHERE status = $pending RETURN AFTER);
                 IF array::len($done) > 0 AND $done[0].asker != $by {{
                     UPDATE $by SET
                         {POOL_APPROVED_TOTAL_FIELD} = ({POOL_APPROVED_TOTAL_FIELD} ?? 0) + 1;
                     LET $asker = $done[0].asker;
                     UPDATE $asker SET
                         {POOL_PUBLISHED_TOTAL_FIELD} = ({POOL_PUBLISHED_TOTAL_FIELD} ?? 0) + 1;
                 }};
                 RETURN $done;
                 COMMIT TRANSACTION;"
            ),
            &[
                ("q".into(), id.record().into_value()),
                ("approved".into(), STATUS_APPROVED.to_string().into_value()),
                ("by".into(), approver.record().into_value()),
                ("pending".into(), STATUS_PENDING.to_string().into_value()),
            ],
            &[],
        )
        .await?;
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        // The trailing `RETURN` is always the last statement before `COMMIT`,
        // so its slot follows the statement count rather than a hand-kept
        // number (`num_statements` counts `BEGIN` and `COMMIT` too, hence -2).
        let slot = result.num_statements().saturating_sub(2);
        Ok(result.take::<Vec<PoolQuestion>>(slot)?.into_iter().next())
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

    /// The two counter columns plus a row to carry them: the `user` table is
    /// SCHEMAFULL in production, and the counters are `option<int>` there.
    async fn a_user(db: &Database) -> UserId {
        let user = UserId::from_key(&Ulid::new().to_string());
        db.query(format!(
            "DEFINE FIELD IF NOT EXISTS {POOL_APPROVED_TOTAL_FIELD} ON user TYPE option<int>;
             DEFINE FIELD IF NOT EXISTS {POOL_PUBLISHED_TOTAL_FIELD} ON user TYPE option<int>;
             CREATE $usr SET username = $name, password_hash = 'x';"
        ))
        .bind(("usr", user.record()))
        .bind(("name", user.key().to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
        user
    }

    /// `(pool_approved_total, pool_published_total)` as stored — absent reads
    /// zero, the way `BadgeStats::load` reads it.
    async fn counters(user: &UserId, db: &Database) -> (i64, i64) {
        let mut result = db
            .query(format!(
                "SELECT VALUE [({POOL_APPROVED_TOTAL_FIELD} ?? 0),
                               ({POOL_PUBLISHED_TOTAL_FIELD} ?? 0)] FROM $usr"
            ))
            .bind(("usr", user.record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let rows = result.take::<Vec<Vec<i64>>>(0).unwrap();
        let row = rows.into_iter().next().unwrap_or_default();
        (
            row.first().copied().unwrap_or(0),
            row.get(1).copied().unwrap_or(0),
        )
    }

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

    /// The publish counters ride the guard, so they move exactly once per real
    /// transition: the approver's on the approve side, the asker's on the
    /// published side, and neither on a second approve of the same question.
    #[tokio::test]
    async fn approval_moves_both_counters_once_and_only_once() {
        let db = database::init_mem().await.unwrap();
        let asker = a_user(&db).await;
        let teacher = a_user(&db).await;

        let q = question(&asker).insert(&db).await.unwrap();
        assert_eq!(counters(&asker, &db).await, (0, 0), "absent reads zero");

        PoolQuestion::approve(q.get_id(), &teacher, &db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(counters(&teacher, &db).await, (1, 0), "the approver");
        assert_eq!(counters(&asker, &db).await, (0, 1), "the asker");

        // The guard finds nothing pending, so neither counter may move — this
        // is what stops an approve loop from farming either one.
        assert!(
            PoolQuestion::approve(q.get_id(), &teacher, &db)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(counters(&teacher, &db).await, (1, 0), "still one approve");
        assert_eq!(counters(&asker, &db).await, (0, 1), "still one publish");

        // And an approve that lands on nothing at all writes nothing.
        assert!(
            PoolQuestion::approve(&PoolQuestionId::generate(), &teacher, &db)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(counters(&teacher, &db).await, (1, 0));
    }

    /// A teacher may ask a question and the route does not bar them from
    /// approving it — that still works, unchanged. What it does not do is pay
    /// for itself: ask, approve, delete, repeat is the staff-side farm, and
    /// neither counter moves when one person is both ends of the transition.
    #[tokio::test]
    async fn self_approval_still_approves_but_moves_neither_counter() {
        let db = database::init_mem().await.unwrap();
        let teacher = a_user(&db).await;

        let q = question(&teacher).insert(&db).await.unwrap();
        let approved = PoolQuestion::approve(q.get_id(), &teacher, &db)
            .await
            .unwrap()
            .unwrap();
        // The approval itself is untouched: published, stamped, frozen.
        assert!(approved.is_approved());
        assert_eq!(approved.get_approved_by(), Some(&teacher));
        assert_eq!(counters(&teacher, &db).await, (0, 0), "no self-credit");

        // And the skip is about the *pair*, not about the teacher: approving
        // somebody else's question right after still pays.
        let asker = a_user(&db).await;
        let other = question(&asker).insert(&db).await.unwrap();
        PoolQuestion::approve(other.get_id(), &teacher, &db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(counters(&teacher, &db).await, (1, 0));
        assert_eq!(counters(&asker, &db).await, (0, 1));
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
