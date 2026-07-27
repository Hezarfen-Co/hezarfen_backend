//! A solution offered on an approved pool question (see
//! [`crate::domain::pool_question`]). Anyone in the school — student or staff
//! — may offer one; rows list oldest first, reading as a discussion thread.
//! Solutions die with their question (the question delete cascades here).
//! Like a question, a solution may carry one photo (`image_*` metadata on the
//! row, bytes on disk under a server-generated ULID) — but unlike a question
//! it has no moderation state, so its author may edit the body and swap the
//! photo at any time; there is nothing to freeze.

use std::collections::HashMap;

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::{MAX_SOLUTION_BODY_LEN, SOLUTION_TABLE};
use crate::database::Database;
use crate::domain::note_file::FileContentType;
use crate::domain::page::PagedList;
use crate::domain::pool_question::PoolQuestionId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_required;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct SolutionId(RecordId);

impl SolutionId {
    pub fn generate() -> Self {
        Self(RecordId::new(SOLUTION_TABLE, Ulid::new().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(SOLUTION_TABLE, key))
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
pub struct SolutionBody(String);

impl SolutionBody {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("body", value, MAX_SOLUTION_BODY_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct Solution {
    id: SolutionId,
    question: PoolQuestionId,
    author: UserId,
    body: SolutionBody,
    offered_at: Timestamp,
    /// The photo's on-disk blob name — a fresh ULID every upload; `None` when
    /// the solution carries no image.
    image_file: Option<String>,
    image_content_type: Option<FileContentType>,
    image_size: Option<i64>,
}

/// One `GROUP BY question` row of [`Solution::counts_for`].
#[derive(SurrealValue)]
struct SolutionCount {
    question: PoolQuestionId,
    n: i64,
}

impl Solution {
    pub fn new(question: &PoolQuestionId, author: &UserId, body: SolutionBody) -> Self {
        Self {
            id: SolutionId::generate(),
            question: question.clone(),
            author: author.clone(),
            body,
            offered_at: Timestamp::now(),
            image_file: None,
            image_content_type: None,
            image_size: None,
        }
    }

    pub fn get_id(&self) -> &SolutionId {
        &self.id
    }

    pub fn get_question(&self) -> &PoolQuestionId {
        &self.question
    }

    pub fn get_author(&self) -> &UserId {
        &self.author
    }

    pub fn get_body(&self) -> &SolutionBody {
        &self.body
    }

    pub fn get_offered_at(&self) -> Timestamp {
        self.offered_at
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

    pub async fn insert(self, db: &Database) -> Result<Solution, AppError> {
        // whole-row-save-ok: create of a fresh ULID row built in place by `new` — there is no prior row to clobber
        let created: Option<Solution> = db.create(self.id.record()).content(self).await?;
        created.ok_or_else(|| AppError::Internal("failed to create solution".into()))
    }

    /// Read a solution only if it belongs to `question` — keeps the nested
    /// route honest (a solution id under someone else's question 404s).
    pub async fn read_for(
        id: &SolutionId,
        question: &PoolQuestionId,
        db: &Database,
    ) -> Result<Option<Solution>, AppError> {
        let solution: Option<Solution> = db.select(id.record()).await?;
        Ok(solution.filter(|solution| &solution.question == question))
    }

    /// The question's solutions, oldest first — a discussion reads downward.
    /// Ordered by `offered_at`; the `id` tiebreak only makes same-millisecond
    /// offers *stable* across re-queries, not insertion-ordered (ULID low bits
    /// are random within a millisecond, so same-ms order is arbitrary).
    pub async fn list_for(
        question: &PoolQuestionId,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<Solution>, i64), AppError> {
        PagedList::new(
            "solution WHERE question = $q",
            "ORDER BY offered_at ASC, id ASC",
        )
        .bind("q", question.record())
        .run(limit, offset, db)
        .await
    }

    /// Per-question solution tallies for a page of questions, in one grouped
    /// query — a per-row `count()` would cost a query per question. Keys are
    /// question record keys; a question with no solutions has no entry, so
    /// the caller reads misses as zero.
    pub async fn counts_for(
        questions: &[PoolQuestionId],
        db: &Database,
    ) -> Result<HashMap<String, i64>, AppError> {
        if questions.is_empty() {
            return Ok(HashMap::new());
        }
        let ids: Vec<RecordId> = questions.iter().map(|question| question.record()).collect();
        let mut result = db
            .query("SELECT question, count() AS n FROM solution WHERE question IN $qs GROUP BY question")
            .bind(("qs", ids))
            .await?
            .check()?;
        Ok(result
            .take::<Vec<SolutionCount>>(0)?
            .into_iter()
            .map(|row| (row.question.key().to_string(), row.n))
            .collect())
    }

    /// Replace the body. Unconditional (solutions have no moderation state to
    /// guard, unlike a question's `pending` gate) — `None` only means the row
    /// was deleted mid-flight. Authorship is the web layer's check.
    pub async fn set_body(
        id: &SolutionId,
        body: &SolutionBody,
        db: &Database,
    ) -> Result<Option<Solution>, AppError> {
        let mut result = db
            .query("UPDATE $s SET body = $b RETURN AFTER")
            .bind(("s", id.record()))
            .bind(("b", body.clone()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Solution>>(0)?.into_iter().next())
    }

    /// Point the solution at a freshly written image blob. Unconditional for
    /// the same reason as [`Self::set_body`]. Returns the *before* row — its
    /// `image_file` is the replaced blob the caller must remove; `None` means
    /// the row was deleted mid-flight (the fresh blob is the caller's orphan
    /// to take back off disk).
    pub async fn set_image(
        id: &SolutionId,
        file: &str,
        content_type: &FileContentType,
        size: i64,
        db: &Database,
    ) -> Result<Option<Solution>, AppError> {
        let mut result = db
            .query("UPDATE $s SET image_file = $file, image_content_type = $ct, image_size = $size RETURN BEFORE")
            .bind(("s", id.record()))
            .bind(("file", file.to_string()))
            .bind(("ct", content_type.clone()))
            .bind(("size", size))
            .await?
            .check()?;
        Ok(result.take::<Vec<Solution>>(0)?.into_iter().next())
    }

    /// Detach the solution's image. Returns the *before* row — its
    /// `image_file` is the blob the caller must remove.
    pub async fn clear_image(id: &SolutionId, db: &Database) -> Result<Option<Solution>, AppError> {
        let mut result = db
            .query(
                "UPDATE $s SET image_file = NONE, image_content_type = NONE, image_size = NONE \
                 RETURN BEFORE",
            )
            .bind(("s", id.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Solution>>(0)?.into_iter().next())
    }

    pub async fn delete(self, db: &Database) -> Result<Solution, AppError> {
        let deleted: Option<Solution> = db.delete(self.id.record()).await?;
        deleted.ok_or(AppError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rows_scope_to_their_question_and_list_oldest_first() {
        let db = crate::database::init_mem().await.unwrap();
        let question_a = PoolQuestionId::generate();
        let question_b = PoolQuestionId::generate();
        let author = UserId::from_key(&Ulid::new().to_string());

        // Distinct offer times so the assertion pins the real contract —
        // older `offered_at` sorts first — not the same-millisecond `id`
        // tiebreak, whose random ULID low bits would make the order a coin
        // flip (two `now()` reads can land in one millisecond).
        let mut first = Solution::new(
            &question_a,
            &author,
            SolutionBody::try_new("Önce türevi al.").unwrap(),
        );
        first.offered_at = Timestamp::from_millis(1);
        let first = first.insert(&db).await.unwrap();
        let mut second = Solution::new(
            &question_a,
            &author,
            SolutionBody::try_new("Ya da tablo yöntemi.").unwrap(),
        );
        second.offered_at = Timestamp::from_millis(2);
        let second = second.insert(&db).await.unwrap();

        let (listed, _) = Solution::list_for(&question_a, None, 0, &db).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].get_id(), first.get_id());
        assert_eq!(listed[1].get_id(), second.get_id());
        assert!(
            Solution::list_for(&question_b, None, 0, &db)
                .await
                .unwrap()
                .0
                .is_empty()
        );

        // Readable under its own question, invisible under another.
        assert!(
            Solution::read_for(first.get_id(), &question_a, &db)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            Solution::read_for(first.get_id(), &question_b, &db)
                .await
                .unwrap()
                .is_none()
        );

        first.delete(&db).await.unwrap();
        assert_eq!(
            Solution::list_for(&question_a, None, 0, &db)
                .await
                .unwrap()
                .0
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn image_set_replace_clear_report_the_replaced_blob() {
        let db = crate::database::init_mem().await.unwrap();
        let question = PoolQuestionId::generate();
        let author = UserId::from_key(&Ulid::new().to_string());
        let png = FileContentType::try_new("image/png").unwrap();

        let solution = Solution::new(
            &question,
            &author,
            SolutionBody::try_new("Grafiği çiz.").unwrap(),
        )
        .insert(&db)
        .await
        .unwrap();

        let before = Solution::set_image(solution.get_id(), "blob_a", &png, 3, &db)
            .await
            .unwrap()
            .unwrap();
        assert!(before.get_image_file().is_none());

        // A replace reports the old blob for cleanup.
        let before = Solution::set_image(solution.get_id(), "blob_b", &png, 5, &db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(before.get_image_file(), Some("blob_a"));
        let stored = Solution::read_for(solution.get_id(), &question, &db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.get_image_file(), Some("blob_b"));
        assert_eq!(stored.get_image_size(), Some(5));

        // Clearing reports the detached blob and empties the fields.
        let before = Solution::clear_image(solution.get_id(), &db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(before.get_image_file(), Some("blob_b"));
        assert!(
            Solution::read_for(solution.get_id(), &question, &db)
                .await
                .unwrap()
                .unwrap()
                .get_image_file()
                .is_none()
        );

        // A row deleted mid-flight surfaces as None, not an error.
        solution.clone().delete(&db).await.unwrap();
        assert!(
            Solution::set_image(solution.get_id(), "blob_c", &png, 7, &db)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            Solution::clear_image(solution.get_id(), &db)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn set_body_edits_in_place() {
        let db = crate::database::init_mem().await.unwrap();
        let question = PoolQuestionId::generate();
        let author = UserId::from_key(&Ulid::new().to_string());

        let solution = Solution::new(
            &question,
            &author,
            SolutionBody::try_new("İlk deneme.").unwrap(),
        )
        .insert(&db)
        .await
        .unwrap();

        let edited = Solution::set_body(
            solution.get_id(),
            &SolutionBody::try_new("Düzeltilmiş çözüm.").unwrap(),
            &db,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(edited.get_body().as_str(), "Düzeltilmiş çözüm.");
        assert_eq!(edited.get_id(), solution.get_id());

        // Editing a missing solution is None, not an error.
        assert!(
            Solution::set_body(
                &SolutionId::generate(),
                &SolutionBody::try_new("boş").unwrap(),
                &db,
            )
            .await
            .unwrap()
            .is_none()
        );
    }

    #[tokio::test]
    async fn counts_for_groups_per_question() {
        let db = crate::database::init_mem().await.unwrap();
        let two = PoolQuestionId::generate();
        let one = PoolQuestionId::generate();
        let none = PoolQuestionId::generate();
        let author = UserId::from_key(&Ulid::new().to_string());

        for (question, bodies) in [(&two, vec!["a", "b"]), (&one, vec!["c"])] {
            for body in bodies {
                Solution::new(question, &author, SolutionBody::try_new(body).unwrap())
                    .insert(&db)
                    .await
                    .unwrap();
            }
        }

        let counts = Solution::counts_for(&[two.clone(), one.clone(), none.clone()], &db)
            .await
            .unwrap();
        assert_eq!(counts.get(two.key()), Some(&2));
        assert_eq!(counts.get(one.key()), Some(&1));
        // No solutions = no entry; the caller reads the miss as zero.
        assert_eq!(counts.get(none.key()), None);
        assert!(Solution::counts_for(&[], &db).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn body_rules() {
        assert!(SolutionBody::try_new("").is_err());
        assert!(SolutionBody::try_new("   ").is_err());
        assert!(SolutionBody::try_new(&"x".repeat(10_001)).is_err());
        assert_eq!(
            SolutionBody::try_new("Kısmi integrasyon.")
                .unwrap()
                .as_str(),
            "Kısmi integrasyon."
        );
    }
}
