//! The `solution` table: offers (their question's existence enforced by the
//! real foreign key), the question-scoped read and page, the grouped counts,
//! and the unconditional body/image writes of an unmoderated row. The pure
//! entity and newtypes live in [`crate::domain::solution`]; the web layer
//! reaches these through [`crate::service::solution`].

use std::collections::HashMap;

use crate::database::{Database, tx_with_retry};
use crate::db::page::PagedList;
use crate::domain::note_file::FileContentType;
use crate::domain::pool_question::PoolQuestionId;
use crate::domain::solution::{Solution, SolutionBody, SolutionId};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Offer the solution. `NotFound` = the question is gone, and nothing was
/// written: the insert's foreign key (`solution_question_fkey`) refuses an
/// orphan the old existence-move transaction spent five statements dodging —
/// a bare create landing inside
/// [`crate::db::pool_question::delete`]'s window used to be swept by
/// nothing and left a solution no route could ever reach or remove (every
/// path to one goes through its question), photo blob included.
///
/// The author's own foreign key never fires in practice (the request
/// carries a live session user) and stays a database error if it ever does;
/// only the question's key maps to the 404.
pub async fn insert(db: &Database, solution: Solution) -> Result<Solution, AppError> {
    match sqlx::query_as!(
        Solution,
        r#"INSERT INTO solution (id, question, author, body, offered_at)
           VALUES ($1, $2, $3, $4, $5)
           RETURNING id AS "id: SolutionId", question AS "question: PoolQuestionId",
               author AS "author: UserId", body AS "body: SolutionBody",
               offered_at AS "offered_at: Timestamp""#,
        solution.id.uuid(),
        solution.question.uuid(),
        solution.author.uuid(),
        solution.body.as_str(),
        solution.offered_at.as_millis(),
    )
    .fetch_one(db)
    .await
    {
        Ok(row) => Ok(row),
        Err(err) if is_question_gone(&err) => Err(AppError::NotFound),
        Err(err) => Err(err.into()),
    }
}

/// Did this insert fail on the *question* foreign key — the parent-gone
/// refusal — rather than some unrelated database fault?
fn is_question_gone(err: &sqlx::Error) -> bool {
    err.as_database_error().and_then(|db| db.constraint()) == Some("solution_question_fkey")
}

/// Read a solution only if it belongs to `question` — keeps the nested
/// route honest (a solution id under someone else's question 404s).
pub async fn read_for(
    db: &Database,
    id: &SolutionId,
    question: &PoolQuestionId,
) -> Result<Option<Solution>, AppError> {
    let row = sqlx::query_as!(
        Solution,
        r#"SELECT id AS "id: SolutionId", question AS "question: PoolQuestionId",
               author AS "author: UserId", body AS "body: SolutionBody",
               offered_at AS "offered_at: Timestamp"
           FROM solution WHERE id = $1 AND question = $2"#,
        id.uuid(),
        question.uuid()
    )
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// The question's solutions, oldest first — a discussion reads downward.
/// Ordered by `offered_at`; the `id` tiebreak only makes same-millisecond
/// offers *stable* across re-queries (the ids are write-ordered UUIDv7, so
/// same-ms rows still sort by mint order).
pub async fn list_for(
    db: &Database,
    question: &PoolQuestionId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Solution>, i64), AppError> {
    // `.uuid()` feeds the builder its raw bind value — exact, typed.
    PagedList::new(
        "solution WHERE question = $1",
        "ORDER BY offered_at ASC, id ASC",
    )
    .bind(question.uuid())
    .run(limit, offset, db)
    .await
}

/// Per-question solution tallies for a page of questions, in one grouped
/// query — a per-row `count()` would cost a query per question. Keys are
/// the questions' wire keys; a question with no solutions has no entry, so
/// the caller reads misses as zero.
pub async fn counts_for(
    db: &Database,
    questions: &[PoolQuestionId],
) -> Result<HashMap<String, i64>, AppError> {
    if questions.is_empty() {
        return Ok(HashMap::new());
    }
    let ids: Vec<uuid::Uuid> = questions.iter().map(|id| id.uuid()).collect();
    let rows = sqlx::query!(
        r#"SELECT question AS "question: PoolQuestionId", count(*) AS n
           FROM solution
           WHERE question = ANY($1)
           GROUP BY question"#,
        &ids,
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.question.key(), row.n.unwrap_or(0)))
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
    let row = sqlx::query_as!(
        Solution,
        r#"UPDATE solution SET body = $2 WHERE id = $1
           RETURNING id AS "id: SolutionId", question AS "question: PoolQuestionId",
               author AS "author: UserId", body AS "body: SolutionBody",
               offered_at AS "offered_at: Timestamp""#,
        id.uuid(),
        body.as_str()
    )
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// Point the solution at a freshly written image blob. Unconditional for
/// the same reason as [`set_body`]. `Ok(None)` means the row was deleted
/// mid-flight (the fresh blob is the caller's orphan to take back off
/// disk); `Ok(Some(replaced))` names the blob this upload displaced — the
/// caller's to remove, `None` inside when nothing was there before.
///
/// The photo is its own row in `solution_image` (the child-table shape of
/// `question_image`), so the write is that row's upsert; the parent's
/// `FOR UPDATE` row lock still makes the before-read and the write one
/// switch, so two racing uploads can never both be told they replaced the
/// same blob — the loser re-reads the winner's blob name, the discipline
/// `crate::db::question_image::upsert` documents.
pub async fn set_image(
    db: &Database,
    id: &SolutionId,
    file: &str,
    content_type: &FileContentType,
    size: i64,
) -> Result<Option<Option<String>>, AppError> {
    // Owned captures (`Send` rule of `tx_with_retry` closures).
    let id = *id;
    let file = file.to_owned();
    let content_type = content_type.clone();
    tx_with_retry(db, false, async move |tx| {
        let before = sqlx::query!(
            r#"SELECT id FROM solution WHERE id = $1 FOR UPDATE"#,
            id.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        if before.is_none() {
            return Ok(None);
        }
        let replaced = sqlx::query_scalar!(
            r#"SELECT file FROM solution_image WHERE solution = $1"#,
            id.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        sqlx::query!(
            r#"INSERT INTO solution_image (solution, file, content_type, size)
               VALUES ($1, $2, $3, $4)
               ON CONFLICT (solution) DO UPDATE
                   SET file = EXCLUDED.file, content_type = EXCLUDED.content_type,
                       size = EXCLUDED.size"#,
            id.uuid(),
            file,
            content_type.as_str(),
            size,
        )
        .execute(&mut *tx)
        .await?;
        Ok(Some(replaced))
    })
    .await
}

/// Detach the solution's image. `Ok(None)` = the row was deleted mid-flight;
/// `Ok(Some(detached))` names the blob the caller removes — `None` inside
/// when the solution carried no image (the route answers that 404).
pub async fn clear_image(
    db: &Database,
    id: &SolutionId,
) -> Result<Option<Option<String>>, AppError> {
    // Owned capture (`Send` rule of `tx_with_retry` closures).
    let id = *id;
    tx_with_retry(db, false, async move |tx| {
        let before = sqlx::query!(
            r#"SELECT id FROM solution WHERE id = $1 FOR UPDATE"#,
            id.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        if before.is_none() {
            return Ok(None);
        }
        let detached = sqlx::query_scalar!(
            r#"DELETE FROM solution_image WHERE solution = $1 RETURNING file"#,
            id.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        Ok(Some(detached))
    })
    .await
}

/// Delete a solution, returning the row plus the blob name of the photo it
/// carried (the caller's disk-GC list — one entry at most). The photo row
/// goes first: its foreign key is `NO ACTION`, so it cannot outlive being
/// pointed at, and its file name is read on the way out, under the same
/// transaction — a photo uploaded by a racing `set_image` loses the row
/// lock to this delete and comes back as the caller's orphan.
pub async fn delete(
    db: &Database,
    solution: Solution,
) -> Result<Option<(Solution, Option<String>)>, AppError> {
    // Owned capture (`Send` rule of `tx_with_retry` closures).
    let id = *solution.get_id();
    tx_with_retry(db, false, async move |tx| {
        let image = sqlx::query_scalar!(
            r#"DELETE FROM solution_image WHERE solution = $1 RETURNING file"#,
            id.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        let deleted = sqlx::query_as!(
            Solution,
            r#"DELETE FROM solution WHERE id = $1
               RETURNING id AS "id: SolutionId", question AS "question: PoolQuestionId",
                   author AS "author: UserId", body AS "body: SolutionBody",
                   offered_at AS "offered_at: Timestamp""#,
            id.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        Ok(deleted.map(|deleted| (deleted, image)))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database;
    use crate::domain::pool_question::{PoolQuestion, PoolQuestionBody, PoolQuestionTitle};
    use crate::domain::timestamp::Timestamp;
    use crate::domain::user::UserId;

    /// A real `app_user` row: askers and authors are foreign keys now.
    async fn a_person(db: &Database, label: &str) -> UserId {
        let user = UserId::generate();
        sqlx::query(
            "INSERT INTO app_user (id, username, created_at) \
             VALUES ($1, $2, 0)",
        )
        .bind(user.uuid())
        .bind(format!("{label}-{}", &user.key()[30..]))
        .execute(db)
        .await
        .unwrap();
        user
    }

    /// A real question row: offering a solution moves its question's
    /// `asked_at` (that is what keeps a solution from outliving its question),
    /// so a minted id nothing wrote is a 404.
    async fn question_row(db: &Database) -> PoolQuestionId {
        let asker = a_person(db, "asker").await;
        *crate::db::pool_question::insert(
            db,
            PoolQuestion::new(
                &asker,
                PoolQuestionTitle::try_new("soru").unwrap(),
                PoolQuestionBody::try_new("neden").unwrap(),
            ),
        )
        .await
        .unwrap()
        .get_id()
    }

    #[tokio::test]
    async fn rows_scope_to_their_question_and_list_oldest_first() {
        let (db, _leases) = database::init_test_db().await;
        let question_a = question_row(&db).await;
        let question_b = question_row(&db).await;
        let author = a_person(&db, "author").await;

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

        let first_id = *first.get_id();
        let (deleted, image) = delete(&db, first).await.unwrap().unwrap();
        assert_eq!(deleted.get_id(), &first_id);
        assert!(image.is_none());
        assert_eq!(
            list_for(&db, &question_a, None, 0).await.unwrap().0.len(),
            1
        );
    }

    #[tokio::test]
    async fn image_set_replace_clear_report_the_replaced_blob() {
        let (db, _leases) = database::init_test_db().await;
        let question = question_row(&db).await;
        let author = a_person(&db, "author").await;
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

        let replaced = set_image(&db, solution.get_id(), "blob_a", &png, 3)
            .await
            .unwrap()
            .unwrap();
        assert!(replaced.is_none());

        // A replace reports the old blob for cleanup.
        let replaced = set_image(&db, solution.get_id(), "blob_b", &png, 5)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(replaced.as_deref(), Some("blob_a"));
        let stored = crate::db::solution_image::read(&db, solution.get_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.get_file(), "blob_b");
        assert_eq!(stored.get_size(), 5);

        // Clearing reports the detached blob and drops the row.
        let detached = clear_image(&db, solution.get_id()).await.unwrap().unwrap();
        assert_eq!(detached.as_deref(), Some("blob_b"));
        assert!(
            crate::db::solution_image::read(&db, solution.get_id())
                .await
                .unwrap()
                .is_none()
        );

        // A row deleted mid-flight surfaces as None, not an error.
        let (deleted, image) = delete(&db, solution.clone()).await.unwrap().unwrap();
        assert_eq!(deleted.get_id(), solution.get_id());
        assert!(image.is_none());
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
        let (db, _leases) = database::init_test_db().await;
        let question = question_row(&db).await;
        let author = a_person(&db, "author").await;

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
                &SolutionId::from_key(&uuid::Uuid::now_v7().to_string()),
                &SolutionBody::try_new("boş").unwrap(),
            )
            .await
            .unwrap()
            .is_none()
        );
    }

    #[tokio::test]
    async fn counts_for_groups_per_question() {
        let (db, _leases) = database::init_test_db().await;
        let two = question_row(&db).await;
        let one = question_row(&db).await;
        let none = question_row(&db).await;
        let author = a_person(&db, "author").await;

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

        let counts = counts_for(&db, &[two, one, none]).await.unwrap();
        assert_eq!(counts.get(two.key().as_str()), Some(&2));
        assert_eq!(counts.get(one.key().as_str()), Some(&1));
        // No solutions = no entry; the caller reads the miss as zero.
        assert_eq!(counts.get(none.key().as_str()), None);
        assert!(counts_for(&db, &[]).await.unwrap().is_empty());
    }
}
