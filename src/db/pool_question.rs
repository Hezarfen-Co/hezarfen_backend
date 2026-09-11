//! The `pool_question` table: ask/insert, the two listings, the guarded
//! approve that rides the publish counters along, the pending-only image
//! writes, and the delete that cascades solutions — plus
//! [`bump_question_and_write`], the existence-move every child write goes
//! through. The pure entity and newtypes live in
//! [`crate::domain::pool_question`]; the web layer reaches these through
//! [`crate::service::pool_question`].

use surrealdb::types::SurrealValue;

use crate::constant::{
    POOL_APPROVED_TOTAL_FIELD, POOL_PUBLISHED_TOTAL_FIELD, STATUS_APPROVED, STATUS_PENDING,
};
use crate::database::{Database, transaction_with_retry};
use crate::domain::note_file::FileContentType;
use crate::domain::pool_question::{PoolQuestion, PoolQuestionId};
use crate::domain::solution::Solution;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Move the question's `asked_at` **and** run `statement` — a write to
/// something hanging off a question — in one transaction, handing back the row
/// it returned. `NotFound` means the question is gone and nothing was written.
///
/// The move is what makes the question's **existence** part of the child's own
/// write, the way [`crate::domain::menu::bump_menu_and_write`] does for a menu:
/// the `UPDATE` matches nothing once the row is deleted, and a delete racing
/// this one touches the very key this transaction writes, so the two cannot
/// both commit. Reading the question first does *not* survive that race — the
/// read sees a row [`delete`] has removed but not committed, while its
/// `DELETE solution WHERE question = $q` ran on a snapshot predating this
/// insert, so both commit and the child outlives its question with neither
/// caller told anything.
///
/// `asked_at` rather than a revision column because this row carries no counter
/// to bump and must not grow one for this alone. It is put back *by captured
/// value* in the same transaction, so nothing outside ever observes the move
/// and the row is byte-identical afterwards (the pool lists order on it).
/// Writing the same value would buy nothing: an `UPDATE` that leaves the
/// document unchanged is elided and never reaches the store's write set, so it
/// collides with nothing.
///
/// Admissible for [`transaction_with_retry`] as long as `statement` is: the
/// `UPDATE`s, `SELECT`, `IF`/`THROW` and `RETURN` can never answer "already
/// exists".
pub(crate) async fn bump_question_and_write<T: SurrealValue>(
    question: &PoolQuestionId,
    statement: &str,
    mut bindings: Vec<(String, surrealdb::types::Value)>,
    db: &Database,
) -> Result<Option<T>, AppError> {
    bindings.push(("q".into(), question.record().into_value()));
    let (mut result, mut errors) = transaction_with_retry(
        db,
        &format!(
            "BEGIN TRANSACTION;
             LET $was_asked = (SELECT VALUE asked_at FROM ONLY $q);
             LET $bumped = (UPDATE $q SET asked_at = asked_at + 1 RETURN VALUE id);
             IF array::len($bumped) = 0 {{ THROW 'no_question' }};
             UPDATE $q SET asked_at = $was_asked;
             LET $row = ({statement});
             RETURN $row;
             COMMIT TRANSACTION;"
        ),
        &bindings,
        &["no_question"],
    )
    .await?;
    // An aborted transaction errors *every* slot, most with a generic "not
    // executed" — only the THROW's own slot names the reason.
    if errors
        .values()
        .any(|error| error.to_string().contains("no_question"))
    {
        return Err(AppError::NotFound);
    }
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    // The trailing `RETURN` is the last statement before `COMMIT`, so its slot
    // follows the statement count rather than a hand-kept number;
    // `num_statements` counts BEGIN and COMMIT.
    let slot = result.num_statements().saturating_sub(2);
    Ok(result.take::<Vec<T>>(slot)?.into_iter().next())
}

pub async fn insert(db: &Database, question: PoolQuestion) -> Result<PoolQuestion, AppError> {
    // whole-row-save-ok: create of a fresh ULID row built in place by `new` — there is no prior row to clobber
    let created: Option<PoolQuestion> = db.create(question.id.record()).content(question).await?;
    created.ok_or_else(|| AppError::Internal("failed to create pool question".into()))
}

pub async fn read(db: &Database, id: &PoolQuestionId) -> Result<Option<PoolQuestion>, AppError> {
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
pub async fn list_visible_to(db: &Database, user: &UserId) -> Result<Vec<PoolQuestion>, AppError> {
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
/// [`insert`] is self-service and the author can
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
    db: &Database,
    id: &PoolQuestionId,
    approver: &UserId,
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
    db: &Database,
    id: &PoolQuestionId,
    file: &str,
    content_type: &FileContentType,
    size: i64,
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
    db: &Database,
    id: &PoolQuestionId,
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
///
/// Rides the retry because a solution offer now writes this very row
/// ([`bump_question_and_write`]): the two contend by design, and a lost
/// round through a bare `check()` was a 500 for a delete that only had to
/// be re-sent. Admissible — a `DELETE` can never answer "already exists".
pub async fn delete(
    db: &Database,
    id: &PoolQuestionId,
) -> Result<Option<(PoolQuestion, Vec<Solution>)>, AppError> {
    let (mut result, mut errors) = transaction_with_retry(
        db,
        "BEGIN TRANSACTION;
             DELETE solution WHERE question = $q RETURN BEFORE;
             DELETE $q RETURN BEFORE;
             COMMIT TRANSACTION;",
        &[("q".into(), id.record().into_value())],
        &[],
    )
    .await?;
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    // Slots count BEGIN: the solution sweep is slot 1, the question slot 2.
    let solutions = result.take::<Vec<Solution>>(1)?;
    Ok(result
        .take::<Vec<PoolQuestion>>(2)?
        .into_iter()
        .next()
        .map(|question| (question, solutions)))
}

#[cfg(test)]
mod tests {
    use ulid::Ulid;

    use super::*;
    use crate::database;
    use crate::domain::solution::SolutionBody;

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
        use crate::domain::pool_question::{PoolQuestionBody, PoolQuestionTitle};
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
        let db = database::init_mem().await.unwrap();
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
        let db = database::init_mem().await.unwrap();
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
        let db = database::init_mem().await.unwrap();
        let asker = UserId::from_key(&Ulid::new().to_string());
        let other = UserId::from_key(&Ulid::new().to_string());
        let teacher = UserId::from_key(&Ulid::new().to_string());

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
        let db = database::init_mem().await.unwrap();
        let asker = UserId::from_key(&Ulid::new().to_string());
        let teacher = UserId::from_key(&Ulid::new().to_string());
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
        let db = database::init_mem().await.unwrap();
        let asker = UserId::from_key(&Ulid::new().to_string());
        let helper = UserId::from_key(&Ulid::new().to_string());

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
            let asker = UserId::from_key(&Ulid::new().to_string());
            let helper = UserId::from_key(&Ulid::new().to_string());
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
