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
               approved_by AS "approved_by: UserId", image_file,
               image_content_type AS "image_content_type: FileContentType",
               image_size"#,
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
               approved_by AS "approved_by: UserId", image_file,
               image_content_type AS "image_content_type: FileContentType",
               image_size
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
               approved_by AS "approved_by: UserId", image_file,
               image_content_type AS "image_content_type: FileContentType",
               image_size
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
               approved_by AS "approved_by: UserId", image_file,
               image_content_type AS "image_content_type: FileContentType",
               image_size
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
                   approved_by AS "approved_by: UserId", image_file,
                   image_content_type AS "image_content_type: FileContentType",
                   image_size"#,
            id.uuid(),
            STATUS_APPROVED,
            approver.uuid(),
            STATUS_PENDING,
        )
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(question) = &approved {
            if question.asker != approver {
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
        }
        Ok(approved)
    })
    .await
}

/// Point the question at a freshly written image blob. Guarded on
/// `pending` for the same reason approval is: an image landing after
/// approval would put unmoderated bytes in the pool, so an upload that
/// loses the race gets `None` (and the caller takes the orphan blob back
/// off disk). Returns the *before* row — its `image_file` is the replaced
/// blob the caller must remove.
///
/// The row lock makes the before-read and the guarded write one switch; a
/// racing approve either predates this transaction's snapshot or waits on
/// the lock, so a fresh image can never straddle an approval.
pub async fn set_image(
    db: &Database,
    id: &PoolQuestionId,
    file: &str,
    content_type: &FileContentType,
    size: i64,
) -> Result<Option<PoolQuestion>, AppError> {
    let id = *id;
    let file = file.to_owned();
    let content_type = content_type.clone();
    tx_with_retry(db, false, async move |tx| {
        let before = sqlx::query_as!(
            PoolQuestion,
            r#"SELECT id AS "id: PoolQuestionId", asker AS "asker: UserId",
                   title AS "title: PoolQuestionTitle", body AS "body: PoolQuestionBody",
                   status, asked_at AS "asked_at: Timestamp",
                   approved_by AS "approved_by: UserId", image_file,
                   image_content_type AS "image_content_type: FileContentType",
                   image_size
                   FROM pool_question WHERE id = $1 FOR UPDATE"#,
            id.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(before) = before else {
            return Ok(None);
        };
        let written = sqlx::query!(
            r#"UPDATE pool_question
               SET image_file = $2, image_content_type = $3, image_size = $4
               WHERE id = $1 AND status = $5"#,
            id.uuid(),
            file,
            content_type.as_str(),
            size,
            STATUS_PENDING,
        )
        .execute(&mut *tx)
        .await?;
        if written.rows_affected() == 0 {
            return Ok(None);
        }
        Ok(Some(before))
    })
    .await
}

/// Detach the question's image (pending only, like `set_image`). Returns
/// the *before* row — its `image_file` is the blob the caller must remove.
pub async fn clear_image(
    db: &Database,
    id: &PoolQuestionId,
) -> Result<Option<PoolQuestion>, AppError> {
    let id = *id;
    tx_with_retry(db, false, async move |tx| {
        let before = sqlx::query_as!(
            PoolQuestion,
            r#"SELECT id AS "id: PoolQuestionId", asker AS "asker: UserId",
                   title AS "title: PoolQuestionTitle", body AS "body: PoolQuestionBody",
                   status, asked_at AS "asked_at: Timestamp",
                   approved_by AS "approved_by: UserId", image_file,
                   image_content_type AS "image_content_type: FileContentType",
                   image_size
                   FROM pool_question WHERE id = $1 FOR UPDATE"#,
            id.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(before) = before else {
            return Ok(None);
        };
        let written = sqlx::query!(
            r#"UPDATE pool_question
               SET image_file = NULL, image_content_type = NULL, image_size = NULL
               WHERE id = $1 AND status = $2"#,
            id.uuid(),
            STATUS_PENDING,
        )
        .execute(&mut *tx)
        .await?;
        if written.rows_affected() == 0 {
            return Ok(None);
        }
        Ok(Some(before))
    })
    .await
}

/// Delete the question and its solutions in one transaction, returning
/// the removed rows — the question's *and* the swept solutions' — so the
/// caller can take every image blob off disk (solutions carry photos too,
/// and a row-only sweep would strand theirs forever).
///
/// Children before the parent, so the foreign keys never refuse the parent
/// delete. `cascade = true` because a solution offer committing inside this
/// window makes the parent delete answer 23503 — a mid-cascade race the
/// retry loop re-runs, sweeping the latecomer with it. That foreign key is
/// also what keeps a solution from outliving its question on the insert
/// side, so the old existence-move transaction is simply gone.
pub async fn delete(
    db: &Database,
    id: &PoolQuestionId,
) -> Result<Option<(PoolQuestion, Vec<Solution>)>, AppError> {
    let id = *id;
    tx_with_retry(db, true, async move |tx| {
        let solutions = sqlx::query_as!(
            Solution,
            r#"DELETE FROM solution WHERE question = $1
               RETURNING id AS "id: SolutionId", question AS "question: PoolQuestionId",
               author AS "author: UserId", body AS "body: SolutionBody",
               offered_at AS "offered_at: Timestamp", image_file,
               image_content_type AS "image_content_type: FileContentType",
               image_size"#,
            id.uuid()
        )
        .fetch_all(&mut *tx)
        .await?;
        let question = sqlx::query_as!(
            PoolQuestion,
            r#"DELETE FROM pool_question WHERE id = $1
               RETURNING id AS "id: PoolQuestionId", asker AS "asker: UserId",
               title AS "title: PoolQuestionTitle", body AS "body: PoolQuestionBody",
               status, asked_at AS "asked_at: Timestamp",
               approved_by AS "approved_by: UserId", image_file,
               image_content_type AS "image_content_type: FileContentType",
               image_size"#,
            id.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        Ok(question.map(|question| (question, solutions)))
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
        sqlx::query("INSERT INTO app_user (id, username, password_hash) VALUES ($1, $2, 'x')")
            .bind(user.uuid())
            .bind(format!("u{}", &user.key()[..8]))
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
        let before = set_image(&db, q.get_id(), "blob_a", &png, 3)
            .await
            .unwrap()
            .unwrap();
        assert!(before.get_image_file().is_none());

        // A replace reports the old blob for cleanup.
        let before = set_image(&db, q.get_id(), "blob_b", &png, 5)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(before.get_image_file(), Some("blob_a"));
        let stored = read(&db, q.get_id()).await.unwrap().unwrap();
        assert_eq!(stored.get_image_file(), Some("blob_b"));
        assert_eq!(stored.get_image_size(), Some(5));

        // Clearing reports the detached blob and empties the fields.
        let before = clear_image(&db, q.get_id()).await.unwrap().unwrap();
        assert_eq!(before.get_image_file(), Some("blob_b"));
        assert!(
            read(&db, q.get_id())
                .await
                .unwrap()
                .unwrap()
                .get_image_file()
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

        // The swept solutions ride back with the question so the caller can
        // take their image blobs off disk.
        let (removed, swept) = delete(&db, q.get_id()).await.unwrap().unwrap();
        assert_eq!(removed.get_id(), q.get_id());
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].get_id(), offered.get_id());
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
    ///
    /// The window is opened by the database, not by a lucky interleaving: a
    /// `DEFINE EVENT` on `pool_question` fires *inside* the delete's own
    /// transaction the instant the row goes, so the `SLEEP` lands exactly
    /// between the delete and its cascade every time.
    ///
    /// Real server, and `#[ignore]`d for it: the subject *is* the store's
    /// conflict detection, which `init_mem`'s embedded engine does not have —
    /// it commits both writes and answers `Ok` to each, so this passes there on
    /// broken code.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs a real SurrealDB server: podman start hezarfen-surrealdb && cargo test -- --ignored"]
    async fn a_solution_offered_inside_a_delete_never_outlives_its_question() {
        let (db, _serialized) = database::init_test_server("solution_race").await;
        // Hold the delete open for a full second after the row is gone, while
        // its cascade still has to run.
        db.query(
            "DEFINE EVENT hold_the_window ON TABLE pool_question WHEN $event = 'DELETE' \
             THEN { SLEEP 1s; };",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

        let (mut solutions, mut swept, mut delete_500) = (0, 0, 0);
        for round in 0..4 {
            let asker = a_user(&db).await;
            let helper = a_user(&db).await;
            let q = insert(&db, question(&asker)).await.unwrap();
            let id = q.get_id().clone();

            let drop_it = {
                let (db, id) = (db.clone(), id.clone());
                tokio::spawn(async move { delete(&db, &id).await })
            };
            // The offer starts inside the held window — the question row is
            // gone but uncommitted, which is exactly what a read believes.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let child = {
                let (db, id) = (db.clone(), id.clone());
                tokio::spawn(async move {
                    crate::db::solution::insert(
                        &db,
                        Solution::new(
                            &id,
                            &helper,
                            SolutionBody::try_new("Kismi integrasyon uygula.").unwrap(),
                        ),
                    )
                    .await
                })
            };
            let (drop_it, child) = (drop_it.await.unwrap(), child.await.unwrap());
            // A 404 for the offer is a correct answer; the only defect is
            // stored state. The delete must not 500 either — it contends with
            // the offer by design and a lost round is re-sent, not reported.
            assert!(
                !matches!(child, Err(AppError::Db(_))),
                "round {round}: a raced offer must be answered, not 500: {child:?}"
            );
            if matches!(drop_it, Err(AppError::Db(_))) {
                delete_500 += 1;
            }

            // Stored state is the whole verdict; a return value is not evidence.
            if read(&db, &id).await.unwrap().is_none() {
                swept += 1;
                solutions += crate::db::solution::list_for(&db, &id, None, 0)
                    .await
                    .unwrap()
                    .0
                    .len();
            }
        }
        eprintln!("pool_question::delete raced by an offer: {swept}/4 rounds deleted the question");
        assert!(
            swept > 0,
            "no round ever deleted the question, so the window was never reached"
        );
        assert_eq!(solutions, 0, "a solution outlived its question");
        assert_eq!(delete_500, 0, "a raced delete must retry, not 500");
    }
}
