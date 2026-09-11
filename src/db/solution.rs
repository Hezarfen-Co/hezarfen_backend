//! The `solution` table: offers (through the pool question's
//! existence-move), the question-scoped reads and page, the grouped counts,
//! and the unconditional body/image writes of an unmoderated row. The pure
//! entity and newtypes live in [`crate::domain::solution`]; the web layer
//! reaches these through [`crate::service::solution`].

use std::collections::HashMap;

use surrealdb::types::{RecordId, SurrealValue};

use crate::database::Database;
use crate::db::page::PagedList;
use crate::db::pool_question::bump_question_and_write;
use crate::domain::note_file::FileContentType;
use crate::domain::pool_question::PoolQuestionId;
use crate::domain::solution::{Solution, SolutionBody, SolutionId};
use crate::error::AppError;

/// One `GROUP BY question` row of [`counts_for`].
#[derive(SurrealValue)]
struct SolutionCount {
    question: PoolQuestionId,
    n: i64,
}

/// Offer the solution. `NotFound` = the question is gone, and nothing was
/// written: the create rides [`bump_question_and_write`], so the question's
/// existence is a *write* to its row rather than a read the handler made a
/// moment earlier — a bare create landing inside
/// [`crate::db::pool_question::delete`]'s
/// window was swept by nothing and left a solution no route could ever
/// reach or remove (every path to one goes through its question), photo
/// blob included.
///
/// Re-sendable despite the `CREATE`: `$id` is a ULID minted once per call
/// on a table with no `UNIQUE` index, so a re-send cannot answer "already
/// exists" — the one thing the retry could not survive.
pub async fn insert(db: &Database, solution: Solution) -> Result<Solution, AppError> {
    // whole-row-save-ok: create of a fresh ULID row built in place by `new` — there is no prior row to clobber
    let (question, id) = (solution.question.clone(), solution.id.record());
    bump_question_and_write(
        &question,
        "CREATE $id CONTENT $solution",
        vec![
            ("id".into(), id.into_value()),
            ("solution".into(), solution.into_value()),
        ],
        db,
    )
    .await?
    .ok_or_else(|| AppError::Internal("failed to create solution".into()))
}

/// Read a solution only if it belongs to `question` — keeps the nested
/// route honest (a solution id under someone else's question 404s).
pub async fn read_for(
    db: &Database,
    id: &SolutionId,
    question: &PoolQuestionId,
) -> Result<Option<Solution>, AppError> {
    let solution: Option<Solution> = db.select(id.record()).await?;
    Ok(solution.filter(|solution| &solution.question == question))
}

/// The question's solutions, oldest first — a discussion reads downward.
/// Ordered by `offered_at`; the `id` tiebreak only makes same-millisecond
/// offers *stable* across re-queries, not insertion-ordered (ULID low bits
/// are random within a millisecond, so same-ms order is arbitrary).
pub async fn list_for(
    db: &Database,
    question: &PoolQuestionId,
    limit: Option<i64>,
    offset: i64,
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
    db: &Database,
    questions: &[PoolQuestionId],
) -> Result<HashMap<String, i64>, AppError> {
    if questions.is_empty() {
        return Ok(HashMap::new());
    }
    let ids: Vec<RecordId> = questions.iter().map(|question| question.record()).collect();
    let mut result = db
        .query(
            "SELECT question, count() AS n FROM solution WHERE question IN $qs GROUP BY question",
        )
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
    db: &Database,
    id: &SolutionId,
    body: &SolutionBody,
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
/// the same reason as [`set_body`]. Returns the *before* row — its
/// `image_file` is the replaced blob the caller must remove; `None` means
/// the row was deleted mid-flight (the fresh blob is the caller's orphan
/// to take back off disk).
pub async fn set_image(
    db: &Database,
    id: &SolutionId,
    file: &str,
    content_type: &FileContentType,
    size: i64,
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
pub async fn clear_image(db: &Database, id: &SolutionId) -> Result<Option<Solution>, AppError> {
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

pub async fn delete(db: &Database, solution: Solution) -> Result<Solution, AppError> {
    let deleted: Option<Solution> = db.delete(solution.id.record()).await?;
    deleted.ok_or(AppError::NotFound)
}

#[cfg(test)]
mod tests {
    use ulid::Ulid;

    use super::*;
    use crate::database;
    use crate::domain::pool_question::{PoolQuestion, PoolQuestionBody, PoolQuestionTitle};
    use crate::domain::timestamp::Timestamp;
    use crate::domain::user::UserId;

    /// A real question row: offering a solution moves its question's
    /// `asked_at` (that is what keeps a solution from outliving its question),
    /// so a minted id nothing wrote is a 404.
    async fn question_row(db: &Database) -> PoolQuestionId {
        crate::db::pool_question::insert(
            db,
            PoolQuestion::new(
                &UserId::from_key(&Ulid::new().to_string()),
                PoolQuestionTitle::try_new("soru").unwrap(),
                PoolQuestionBody::try_new("neden").unwrap(),
            ),
        )
        .await
        .unwrap()
        .get_id()
        .clone()
    }

    #[tokio::test]
    async fn rows_scope_to_their_question_and_list_oldest_first() {
        let db = database::init_mem().await.unwrap();
        let question_a = question_row(&db).await;
        let question_b = question_row(&db).await;
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
        let first = insert(&db, first).await.unwrap();
        let mut second = Solution::new(
            &question_a,
            &author,
            SolutionBody::try_new("Ya da tablo yöntemi.").unwrap(),
        );
        second.offered_at = Timestamp::from_millis(2);
        let second = insert(&db, second).await.unwrap();

        let (listed, _) = list_for(&db, &question_a, None, 0).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].get_id(), first.get_id());
        assert_eq!(listed[1].get_id(), second.get_id());
        assert!(
            list_for(&db, &question_b, None, 0)
                .await
                .unwrap()
                .0
                .is_empty()
        );

        // Readable under its own question, invisible under another.
        assert!(
            read_for(&db, first.get_id(), &question_a)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            read_for(&db, first.get_id(), &question_b)
                .await
                .unwrap()
                .is_none()
        );

        delete(&db, first).await.unwrap();
        assert_eq!(
            list_for(&db, &question_a, None, 0).await.unwrap().0.len(),
            1
        );
    }

    #[tokio::test]
    async fn image_set_replace_clear_report_the_replaced_blob() {
        let db = database::init_mem().await.unwrap();
        let question = question_row(&db).await;
        let author = UserId::from_key(&Ulid::new().to_string());
        let png = FileContentType::try_new("image/png").unwrap();

        let solution = insert(
            &db,
            Solution::new(
                &question,
                &author,
                SolutionBody::try_new("Grafiği çiz.").unwrap(),
            ),
        )
        .await
        .unwrap();

        let before = set_image(&db, solution.get_id(), "blob_a", &png, 3)
            .await
            .unwrap()
            .unwrap();
        assert!(before.get_image_file().is_none());

        // A replace reports the old blob for cleanup.
        let before = set_image(&db, solution.get_id(), "blob_b", &png, 5)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(before.get_image_file(), Some("blob_a"));
        let stored = read_for(&db, solution.get_id(), &question)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.get_image_file(), Some("blob_b"));
        assert_eq!(stored.get_image_size(), Some(5));

        // Clearing reports the detached blob and empties the fields.
        let before = clear_image(&db, solution.get_id()).await.unwrap().unwrap();
        assert_eq!(before.get_image_file(), Some("blob_b"));
        assert!(
            read_for(&db, solution.get_id(), &question)
                .await
                .unwrap()
                .unwrap()
                .get_image_file()
                .is_none()
        );

        // A row deleted mid-flight surfaces as None, not an error.
        let deleted = delete(&db, solution.clone()).await.unwrap();
        assert_eq!(deleted.get_id(), solution.get_id());
        assert!(
            set_image(&db, solution.get_id(), "blob_c", &png, 7)
                .await
                .unwrap()
                .is_none()
        );
        assert!(clear_image(&db, solution.get_id()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn set_body_edits_in_place() {
        let db = database::init_mem().await.unwrap();
        let question = question_row(&db).await;
        let author = UserId::from_key(&Ulid::new().to_string());

        let solution = insert(
            &db,
            Solution::new(
                &question,
                &author,
                SolutionBody::try_new("İlk deneme.").unwrap(),
            ),
        )
        .await
        .unwrap();

        let edited = set_body(
            &db,
            solution.get_id(),
            &SolutionBody::try_new("Düzeltilmiş çözüm.").unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(edited.get_body().as_str(), "Düzeltilmiş çözüm.");
        assert_eq!(edited.get_id(), solution.get_id());

        // Editing a missing solution is None, not an error.
        assert!(
            set_body(
                &db,
                &SolutionId::from_key(&Ulid::new().to_string()),
                &SolutionBody::try_new("boş").unwrap(),
            )
            .await
            .unwrap()
            .is_none()
        );
    }

    #[tokio::test]
    async fn counts_for_groups_per_question() {
        let db = database::init_mem().await.unwrap();
        let two = question_row(&db).await;
        let one = question_row(&db).await;
        let none = question_row(&db).await;
        let author = UserId::from_key(&Ulid::new().to_string());

        for (question, bodies) in [(&two, vec!["a", "b"]), (&one, vec!["c"])] {
            for body in bodies {
                insert(
                    &db,
                    Solution::new(question, &author, SolutionBody::try_new(body).unwrap()),
                )
                .await
                .unwrap();
            }
        }

        let counts = counts_for(&db, &[two.clone(), one.clone(), none.clone()])
            .await
            .unwrap();
        assert_eq!(counts.get(two.key()), Some(&2));
        assert_eq!(counts.get(one.key()), Some(&1));
        // No solutions = no entry; the caller reads the miss as zero.
        assert_eq!(counts.get(none.key()), None);
        assert!(counts_for(&db, &[]).await.unwrap().is_empty());
    }
}
