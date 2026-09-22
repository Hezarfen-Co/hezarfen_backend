//! The `pool_question` table: ask/insert, the two listings, the guarded
//! approve that rides the publish counters along, the pending-only image
//! writes, and the delete that cascades solutions. The pure entity and
//! newtypes live in [`crate::domain::pool_question`]; the web layer reaches
//! these through [`crate::service::pool_question`].

use crate::constant::{STATUS_APPROVED, STATUS_PENDING};
use crate::database::{Database, tx_with_retry};
use crate::domain::note_file::FileContentType;
use crate::domain::pool_question::{
    PoolQuestion, PoolQuestionBody, PoolQuestionId, PoolQuestionTitle,
};
use crate::domain::solution::{Solution, SolutionBody, SolutionId};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

pub async fn insert(db: &Database, question: PoolQuestion) -> Result<PoolQuestion, AppError> {
    let row = sqlx::query_as!(
        PoolQuestion,
        r#"INSERT INTO pool_question (id, asker, title, body, status, asked_at)
           VALUES ($1, $2, $3, $4, $5, $6)
           RETURNING id AS "id: PoolQuestionId", asker AS "asker: UserId",
               title AS "title: PoolQuestionTitle", body AS "body: PoolQuestionBody",
               status, asked_at AS "asked_at: Timestamp",
               approved_by AS "approved_by: UserId""#,
        question.id.uuid(),
        question.asker.uuid(),
        question.title.as_str(),
        question.body.as_str(),
        question.status,
        question.asked_at.as_millis(),
    )
    .fetch_one(db)
    .await?;
    Ok(row)
}

pub async fn read(db: &Database, id: &PoolQuestionId) -> Result<Option<PoolQuestion>, AppError> {
    let row = sqlx::query_as!(
        PoolQuestion,
        r#"SELECT id AS "id: PoolQuestionId", asker AS "asker: UserId",
               title AS "title: PoolQuestionTitle", body AS "body: PoolQuestionBody",
               status, asked_at AS "asked_at: Timestamp",
               approved_by AS "approved_by: UserId"
           FROM pool_question WHERE id = $1"#,
        id.uuid()
    )
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// Every question, newest first — the teacher+ view (approval queue and
/// pool in one list).
pub async fn list_all(db: &Database) -> Result<Vec<PoolQuestion>, AppError> {
    let rows = sqlx::query_as!(
        PoolQuestion,
        r#"SELECT id AS "id: PoolQuestionId", asker AS "asker: UserId",
               title AS "title: PoolQuestionTitle", body AS "body: PoolQuestionBody",
               status, asked_at AS "asked_at: Timestamp",
               approved_by AS "approved_by: UserId"
           FROM pool_question ORDER BY asked_at DESC, id DESC"#,
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// The pool as a non-staff user sees it, newest first: every approved
/// question, plus the caller's own pending ones.
pub async fn list_visible_to(db: &Database, user: &UserId) -> Result<Vec<PoolQuestion>, AppError> {
    let rows = sqlx::query_as!(
        PoolQuestion,
        r#"SELECT id AS "id: PoolQuestionId", asker AS "asker: UserId",
               title AS "title: PoolQuestionTitle", body AS "body: PoolQuestionBody",
               status, asked_at AS "asked_at: Timestamp",
               approved_by AS "approved_by: UserId"
           FROM pool_question
           WHERE status = $1 OR asker = $2
           ORDER BY asked_at DESC, id DESC"#,
        STATUS_APPROVED,
        user.uuid()
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// Publish a pending question into the pool, stamping who approved it.
/// The `WHERE status = 'pending'` guard makes the transition atomic: of two
/// racing approvals exactly one wins, and an approve can never land on a
/// question deleted mid-flight. `None` = the question wasn't pending
/// (already approved, or gone) — the caller sorts out which.
///
/// Two lifetime badge counters ride that same guard inside this
/// transaction: the approver's `pool_approved_total` and the *asker's*
/// `pool_published_total`. Both move only when the guard's own verdict
/// landed, so the pair moves once per real transition and never on a second
/// approve of an already-approved question.
///
/// Two rules keep the pair unfarmable, one per side of the transition.
///
/// The asker is credited at *approval*, not at asking: a bare
/// [`insert`] is self-service and the author can
/// delete their own question and ask again forever, so a counter moved
/// there is farmable. Approval is teacher-gated and one-way, which is why
/// the family is named `pool_published` and not `pool_asked`. That closes
/// the student side.
///
/// And a *self*-approval — a teacher+ approving a question they asked
/// themselves, which the route deliberately still allows — moves neither
/// counter. It is the same farm from the other end (ask, approve, delete,
/// repeat, with no second person involved), and the two together are why a
/// counter can only move when one person's work was judged by another's.
/// Nothing else about a self-approval changes: same 200, same freeze, same
/// stamp — this skips the credit, not the approval.
///
/// Rides the retry loop because the counter bumps lock user rows every
/// other counter site writes too; a deadlock between two approvals takes a
/// round and lands the next attempt.
pub async fn approve(
    db: &Database,
    id: &PoolQuestionId,
    approver: &UserId,
) -> Result<Option<PoolQuestion>, AppError> {
    let id = *id;
    let approver = *approver;
    tx_with_retry(db, false, async move |tx| {
        let approved = sqlx::query_as!(
            PoolQuestion,
            r#"UPDATE pool_question SET status = $2, approved_by = $3
               WHERE id = $1 AND status = $4
               RETURNING id AS "id: PoolQuestionId", asker AS "asker: UserId",
                   title AS "title: PoolQuestionTitle", body AS "body: PoolQuestionBody",
                   status, asked_at AS "asked_at: Timestamp",
                   approved_by AS "approved_by: UserId""#,
            id.uuid(),
            STATUS_APPROVED,
            approver.uuid(),
            STATUS_PENDING,
        )
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(question) = &approved
            && question.asker != approver
        {
            // The counter columns are NOT NULL DEFAULT 0 — the old
            // absent-reads-as-zero coalesce is gone with the schema.
            sqlx::query!(
                "UPDATE app_user SET pool_approved_total = pool_approved_total + 1
                     WHERE id = $1",
                approver.uuid()
            )
            .execute(&mut *tx)
            .await?;
            sqlx::query!(
                "UPDATE app_user SET pool_published_total = pool_published_total + 1
                     WHERE id = $1",
                question.asker.uuid()
            )
            .execute(&mut *tx)
            .await?;
        }
        Ok(approved)
    })
    .await
}

/// Point the question at a freshly written image blob. Guarded on `pending`
/// for the same reason approval is: an image landing after approval would
/// put unmoderated bytes in the pool, so an upload that loses the race gets
/// `Ok(None)` (and the caller takes the orphan blob back off disk).
/// `Ok(Some(replaced))` names the blob this upload displaced — the caller's
/// to remove, `None` inside when nothing was there before.
///
/// The photo is its own row in `pool_question_image` (the child-table shape
/// of `question_image`), so the write is that row's upsert; the parent's
/// `FOR UPDATE` row lock still makes the pending check and the write one
/// switch — a racing approve either predates this transaction's snapshot or
/// waits on the lock, so a fresh image can never straddle an approval. The
/// replaced name is read *inside the same transaction*, not by the caller
/// before it: two uploads to one question both write this row, so they
/// contend on it and the loser re-reads the winner's blob name — the
/// discipline `crate::db::question_image::upsert` documents.
pub async fn set_image(
    db: &Database,
    id: &PoolQuestionId,
    file: &str,
    content_type: &FileContentType,
    size: i64,
) -> Result<Option<Option<String>>, AppError> {
    let id = *id;
    let file = file.to_owned();
    let content_type = content_type.clone();
    tx_with_retry(db, false, async move |tx| {
        let before = sqlx::query!(
            r#"SELECT status FROM pool_question WHERE id = $1 FOR UPDATE"#,
            id.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(before) = before else {
            return Ok(None);
        };
        if before.status != STATUS_PENDING {
            return Ok(None);
        }
        let replaced = sqlx::query_scalar!(
            r#"SELECT file FROM pool_question_image WHERE question = $1"#,
            id.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        sqlx::query!(
            r#"INSERT INTO pool_question_image (question, file, content_type, size)
               VALUES ($1, $2, $3, $4)
               ON CONFLICT (question) DO UPDATE
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

/// Detach the question's image (pending only, like [`set_image`]).
/// `Ok(None)` = the question is gone or no longer pending;
/// `Ok(Some(detached))` names the blob the caller removes — `None` inside
/// when the question carried no image (the route answers that 404).
pub async fn clear_image(
    db: &Database,
    id: &PoolQuestionId,
) -> Result<Option<Option<String>>, AppError> {
    let id = *id;
    tx_with_retry(db, false, async move |tx| {
        let before = sqlx::query!(
            r#"SELECT status FROM pool_question WHERE id = $1 FOR UPDATE"#,
            id.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(before) = before else {
            return Ok(None);
        };
        if before.status != STATUS_PENDING {
            return Ok(None);
        }
        let detached = sqlx::query_scalar!(
            r#"DELETE FROM pool_question_image WHERE question = $1 RETURNING file"#,
            id.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        Ok(Some(detached))
    })
    .await
}

/// What [`delete`] took with the question: the rows ride back for their
/// ids, and `image_files` is the caller's disk-GC list — the question's
/// photo and every swept solution's.
#[derive(Debug)]
pub struct Deleted {
    pub question: PoolQuestion,
    pub solutions: Vec<Solution>,
    pub image_files: Vec<String>,
}

/// Delete the question and its solutions in one transaction, returning the
/// removed rows plus every image blob key the two kinds of rows named, so
/// the caller can take every blob off disk (solutions carry photos too, and
/// a row-only sweep would strand theirs forever).
///
/// Children before the parent, so the foreign keys never refuse the parent
/// delete — the photo rows first (they name their parents), then the
/// solutions, then the question. `cascade = true` because a solution offer
/// committing inside this window makes the parent delete answer 23503 — a
/// mid-cascade race the retry loop re-runs, sweeping the latecomer with it.
/// That foreign key is also what keeps a solution from outliving its
/// question on the insert side, so the old existence-move transaction is
/// simply gone.
pub async fn delete(db: &Database, id: &PoolQuestionId) -> Result<Option<Deleted>, AppError> {
    let id = *id;
    tx_with_retry(db, true, async move |tx| {
        let solution_files = sqlx::query_scalar!(
            r#"DELETE FROM solution_image si
               USING solution s
               WHERE si.solution = s.id AND s.question = $1
               RETURNING si.file"#,
            id.uuid()
        )
        .fetch_all(&mut *tx)
        .await?;
        let solutions = sqlx::query_as!(
            Solution,
            r#"DELETE FROM solution WHERE question = $1
               RETURNING id AS "id: SolutionId", question AS "question: PoolQuestionId",
                   author AS "author: UserId", body AS "body: SolutionBody",
                   offered_at AS "offered_at: Timestamp""#,
            id.uuid()
        )
        .fetch_all(&mut *tx)
        .await?;
        let question_file = sqlx::query_scalar!(
            r#"DELETE FROM pool_question_image WHERE question = $1 RETURNING file"#,
            id.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        let question = sqlx::query_as!(
            PoolQuestion,
            r#"DELETE FROM pool_question WHERE id = $1
               RETURNING id AS "id: PoolQuestionId", asker AS "asker: UserId",
                   title AS "title: PoolQuestionTitle", body AS "body: PoolQuestionBody",
                   status, asked_at AS "asked_at: Timestamp",
                   approved_by AS "approved_by: UserId""#,
            id.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        Ok(question.map(|question| {
            let mut image_files = solution_files;
            image_files.extend(question_file);
            Deleted {
                question,
                solutions,
                image_files,
            }
        }))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database;
    use crate::domain::solution::SolutionBody;
    use sqlx::Row as _;

    /// A real `app_user` row: the asker and the approver are foreign keys on
    /// the question, and the counters are plain `NOT NULL DEFAULT 0` columns.
    async fn a_user(db: &Database) -> UserId {
        let user = UserId::generate();
        sqlx::query("INSERT INTO app_user (id, username, created_at) VALUES ($1, $2, 0)")
            .bind(user.uuid())
            .bind(format!("u{}", &user.key()[30..]))
            .execute(db)
            .await
            .unwrap();
        user
    }

    /// `(pool_approved_total, pool_published_total)` as stored — the columns
    /// read zero on a fresh row, the way `BadgeStats::load` reads them.
    async fn counters(user: &UserId, db: &Database) -> (i64, i64) {
        let row = sqlx::query(
            "SELECT pool_approved_total, pool_published_total \
             FROM app_user WHERE id = $1",
        )
        .bind(user.uuid())
        .fetch_one(db)
        .await
        .unwrap();
        (
            row.try_get::<i64, _>(0).unwrap(),
            row.try_get::<i64, _>(1).unwrap(),
        )
    }

    fn question(asker: &UserId) -> PoolQuestion {
        use crate::domain::pool_question::{PoolQuestionBody, PoolQuestionTitle};
        PoolQuestion::new(
            asker,
            PoolQuestionTitle::try_new("Bu integral nasıl çözülür?").unwrap(),
            PoolQuestionBody::try_new("∫x·eˣ dx adım adım?").unwrap(),
        )
    }

    #[tokio::test]
    async fn approval_is_a_one_way_race_safe_transition() {
        let (db, _leases) = database::init_test_db().await;
        let asker = a_user(&db).await;
        let teacher = a_user(&db).await;

        let q = insert(&db, question(&asker)).await.unwrap();
        assert_eq!(q.get_status(), STATUS_PENDING);
        assert!(q.get_approved_by().is_none());

        let approved = approve(&db, q.get_id(), &teacher).await.unwrap().unwrap();
        assert!(approved.is_approved());
        assert_eq!(approved.get_approved_by(), Some(&teacher));

        // A second approval finds nothing pending.
        assert!(approve(&db, q.get_id(), &teacher).await.unwrap().is_none());
        // And a missing question approves to None, not an error.
        assert!(
            approve(&db, &PoolQuestionId::generate(), &teacher)
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
        let (db, _leases) = database::init_test_db().await;
        let asker = a_user(&db).await;
        let teacher = a_user(&db).await;

        let q = insert(&db, question(&asker)).await.unwrap();
        assert_eq!(counters(&asker, &db).await, (0, 0), "absent reads zero");

        approve(&db, q.get_id(), &teacher).await.unwrap().unwrap();
        assert_eq!(counters(&teacher, &db).await, (1, 0), "the approver");
        assert_eq!(counters(&asker, &db).await, (0, 1), "the asker");

        // The guard finds nothing pending, so neither counter may move — this
        // is what stops an approve loop from farming either one.
        assert!(approve(&db, q.get_id(), &teacher).await.unwrap().is_none());
        assert_eq!(counters(&teacher, &db).await, (1, 0), "still one approve");
        assert_eq!(counters(&asker, &db).await, (0, 1), "still one publish");

        // And an approve that lands on nothing at all writes nothing.
        assert!(
            approve(&db, &PoolQuestionId::generate(), &teacher)
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
        let (db, _leases) = database::init_test_db().await;
        let teacher = a_user(&db).await;

        let q = insert(&db, question(&teacher)).await.unwrap();
        let approved = approve(&db, q.get_id(), &teacher).await.unwrap().unwrap();
        // The approval itself is untouched: published, stamped, frozen.
        assert!(approved.is_approved());
        assert_eq!(approved.get_approved_by(), Some(&teacher));
        assert_eq!(counters(&teacher, &db).await, (0, 0), "no self-credit");

        // And the skip is about the *pair*, not about the teacher: approving
        // somebody else's question right after still pays.
        let asker = a_user(&db).await;
        let other = insert(&db, question(&asker)).await.unwrap();
        approve(&db, other.get_id(), &teacher)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(counters(&teacher, &db).await, (1, 0));
        assert_eq!(counters(&asker, &db).await, (0, 1));
    }

    #[tokio::test]
    async fn visibility_hides_others_pending_questions() {
        let (db, _leases) = database::init_test_db().await;
        let asker = a_user(&db).await;
        let other = a_user(&db).await;
        let teacher = a_user(&db).await;

        let pending = insert(&db, question(&asker)).await.unwrap();
        let published = insert(&db, question(&other)).await.unwrap();
        approve(&db, published.get_id(), &teacher)
            .await
            .unwrap()
            .unwrap();

        // The asker sees their own pending question plus the pool; a stranger
        // sees only the pool; the teacher view (list_all) sees everything.
        assert_eq!(list_visible_to(&db, &asker).await.unwrap().len(), 2);
        let stranger_view = list_visible_to(&db, &other).await.unwrap();
        assert_eq!(stranger_view.len(), 1);
        assert_eq!(stranger_view[0].get_id(), published.get_id());
        assert_eq!(list_all(&db).await.unwrap().len(), 2);
        assert!(read(&db, pending.get_id()).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn image_attaches_only_while_pending_and_reports_the_replaced_blob() {
        let (db, _leases) = database::init_test_db().await;
        let asker = a_user(&db).await;
        let teacher = a_user(&db).await;
        let png = FileContentType::try_new("image/png").unwrap();

        let q = insert(&db, question(&asker)).await.unwrap();
        let replaced = set_image(&db, q.get_id(), "blob_a", &png, 3)
            .await
            .unwrap()
            .unwrap();
        assert!(replaced.is_none());

        // A replace reports the old blob for cleanup.
        let replaced = set_image(&db, q.get_id(), "blob_b", &png, 5)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(replaced.as_deref(), Some("blob_a"));
        let stored = crate::db::pool_question_image::read(&db, q.get_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.get_file(), "blob_b");
        assert_eq!(stored.get_size(), 5);

        // Clearing reports the detached blob and drops the row.
        let detached = clear_image(&db, q.get_id()).await.unwrap().unwrap();
        assert_eq!(detached.as_deref(), Some("blob_b"));
        assert!(
            crate::db::pool_question_image::read(&db, q.get_id())
                .await
                .unwrap()
                .is_none()
        );

        // Once approved, the image is frozen with the rest of the content.
        approve(&db, q.get_id(), &teacher).await.unwrap().unwrap();
        assert!(
            set_image(&db, q.get_id(), "blob_c", &png, 7)
                .await
                .unwrap()
                .is_none()
        );
        assert!(clear_image(&db, q.get_id()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_cascades_solutions_and_returns_the_row() {
        let (db, _leases) = database::init_test_db().await;
        let asker = a_user(&db).await;
        let helper = a_user(&db).await;
        let png = FileContentType::try_new("image/png").unwrap();

        let q = insert(&db, question(&asker)).await.unwrap();
        let offered = crate::db::solution::insert(
            &db,
            Solution::new(
                q.get_id(),
                &helper,
                SolutionBody::try_new("Kısmi integrasyon uygula.").unwrap(),
            ),
        )
        .await
        .unwrap();
        // Photos on both rows: their blob names must ride out with the
        // delete, or the files strand on disk forever.
        set_image(&db, q.get_id(), "q_blob", &png, 1)
            .await
            .unwrap()
            .unwrap();
        crate::db::solution::set_image(&db, offered.get_id(), "s_blob", &png, 2)
            .await
            .unwrap()
            .unwrap();

        // The swept solutions ride back with the question so the caller can
        // take every image blob off disk.
        let removed = delete(&db, q.get_id()).await.unwrap().unwrap();
        assert_eq!(removed.question.get_id(), q.get_id());
        assert_eq!(removed.solutions.len(), 1);
        assert_eq!(removed.solutions[0].get_id(), offered.get_id());
        assert_eq!(removed.image_files.len(), 2);
        assert!(removed.image_files.contains(&"q_blob".to_string()));
        assert!(removed.image_files.contains(&"s_blob".to_string()));
        assert!(read(&db, q.get_id()).await.unwrap().is_none());
        assert!(
            crate::db::solution::list_for(&db, q.get_id(), None, 0)
                .await
                .unwrap()
                .0
                .is_empty()
        );

        // Deleting the already-deleted is None, not an error.
        assert!(delete(&db, q.get_id()).await.unwrap().is_none());
    }

    /// The [`crate::db::exam_answer`] defect, one domain
    /// over: a solution offered inside its question's delete window must not
    /// outlive it. Guarding the offer by *reading* the question does not do it
    /// — the read sees a row [`delete`] has removed but not committed, while
    /// its `DELETE solution WHERE question = $q` ran on a snapshot predating
    /// the offer, so both commit and the solution is left pointing at a
    /// question that is gone. Nothing can reach it after that: every route to
    /// a solution goes through its question, so neither a reader nor a second
    /// delete can ever clear it, and its photo stays on disk for good.
    /// [`bump_question_and_write`] moves the question's own `asked_at`
    /// instead, so the two transactions touch one key and the store refuses
    /// one of them.
    /// The offer writes the question row too (its `solution_count`), so the
    /// two transactions touch one row and Postgres refuses one of them.
    /// Both orders are forced by awaiting one side to completion before the
    /// other starts. A barrier is a coin toss under load, and a run where the
    /// delete loses every overlapped round has still proved the invariant.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_solution_offered_inside_a_delete_never_outlives_its_question() {
        let (db, _leases) = database::init_test_db().await;

        async fn offer(
            db: &Database,
            id: &PoolQuestionId,
            helper: &UserId,
        ) -> Result<Solution, AppError> {
            crate::db::solution::insert(
                db,
                Solution::new(
                    id,
                    helper,
                    SolutionBody::try_new("Kismi integrasyon uygula.").unwrap(),
                ),
            )
            .await
        }

        async fn solutions(db: &Database, id: &PoolQuestionId) -> usize {
            crate::db::solution::list_for(db, id, None, 0)
                .await
                .unwrap()
                .0
                .len()
        }

        // Delete-first: the question is gone before the offer starts.
        {
            let asker = a_user(&db).await;
            let helper = a_user(&db).await;
            let q = insert(&db, question(&asker)).await.unwrap();
            let id = *q.get_id();
            let dropped = delete(&db, &id).await;
            assert!(
                !matches!(dropped, Err(AppError::Db(_))),
                "delete-first: a delete must be answered, not 500: {dropped:?}"
            );
            assert!(
                read(&db, &id).await.unwrap().is_none(),
                "delete-first: the question is still there after delete: {dropped:?}"
            );
            let child = offer(&db, &id, &helper).await;
            assert!(
                !matches!(child, Err(AppError::Db(_))),
                "delete-first: an offer must be answered, not 500: {child:?}"
            );
            assert_eq!(solutions(&db, &id).await, 0, "a solution outlived its question");
        }

        // Write-first: the offer lands, then the delete sweeps it.
        {
            let asker = a_user(&db).await;
            let helper = a_user(&db).await;
            let q = insert(&db, question(&asker)).await.unwrap();
            let id = *q.get_id();
            let child = offer(&db, &id, &helper).await;
            assert!(
                child.is_ok(),
                "write-first: the offer must land before the delete: {child:?}"
            );
            let dropped = delete(&db, &id).await;
            assert!(
                !matches!(dropped, Err(AppError::Db(_))),
                "write-first: a delete must be answered, not 500: {dropped:?}"
            );
            assert!(
                read(&db, &id).await.unwrap().is_none(),
                "write-first: the question is still there"
            );
            assert_eq!(solutions(&db, &id).await, 0, "a solution outlived its question");
        }

        // Overlapped rounds. Delete may lose every one of them.
        let mut swept = 0;
        for round in 0..4 {
            let asker = a_user(&db).await;
            let helper = a_user(&db).await;
            let q = insert(&db, question(&asker)).await.unwrap();
            let id = *q.get_id();

            let gate = std::sync::Arc::new(tokio::sync::Barrier::new(2));
            let drop_it = {
                let (db, id, gate) = (db.clone(), id, gate.clone());
                tokio::spawn(async move {
                    gate.wait().await;
                    delete(&db, &id).await
                })
            };
            let child = {
                let (db, id, helper, gate) = (db.clone(), id, helper, gate);
                tokio::spawn(async move {
                    gate.wait().await;
                    offer(&db, &id, &helper).await
                })
            };
            let (drop_it, child) = (drop_it.await.unwrap(), child.await.unwrap());
            assert!(
                !matches!(child, Err(AppError::Db(_))),
                "round {round}: a raced offer must be answered, not 500: {child:?}"
            );
            assert!(
                !matches!(drop_it, Err(AppError::Db(_))),
                "round {round}: a raced delete must be answered, not 500: {drop_it:?}"
            );

            if read(&db, &id).await.unwrap().is_none() {
                swept += 1;
                assert_eq!(
                    solutions(&db, &id).await,
                    0,
                    "round {round}: a solution outlived its question"
                );
            } else if drop_it.is_ok() {
                panic!(
                    "round {round}: the delete reported success but the question is still there"
                );
            }
        }
        eprintln!(
            "pool_question::delete raced by an offer: {swept}/4 concurrent rounds deleted the question"
        );
    }
}
