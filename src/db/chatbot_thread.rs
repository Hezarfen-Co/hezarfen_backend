//! The `chatbot_thread` table: one user's AI-chat threads, listed
//! newest-activity-first, deleted together with every turn in them. The
//! entity and its validated newtypes live in
//! [`crate::domain::chatbot_thread`]; this module is where their rows are
//! read and written.

use crate::constant::DEFAULT_MAX_CHATBOT_THREADS;
use crate::database::{Database, tx_with_retry};
use crate::db::page::PagedList;
use crate::domain::chatbot_thread::{ChatbotThread, ChatbotThreadId, ChatbotThreadTitle};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;
use sqlx::query_as;

/// Start a thread unless `user` is already at the school's
/// `max_chatbot_threads`. The claim recipe of one: the slot bump on the
/// user row, the live cap read, and the thread's insert are a single
/// statement — two requests racing the same user's last slot cannot both
/// win, and the counter can never count a row that did not commit.
///
/// The cap is the one *live* on the settings singleton, sub-queried inside
/// that same conditional write rather than bound as a number: this cap does
/// not live on the parent row (the seat is on the user, the limit is the
/// school's), and a snapshot of it admits every request already in flight
/// when a `PATCH /settings` lowers it. `DEFAULT_MAX_CHATBOT_THREADS` is the
/// fallback the settings row's own NULL means — no row, or a school that
/// never set the knob.
///
/// The seat is taken on the user's own row, which is also the key a role
/// change writes, so this write already contends with a demotion — no
/// separate holder claim is needed.
pub async fn create_capped(
    db: &Database,
    user: &UserId,
    title: Option<ChatbotThreadTitle>,
) -> Result<ChatbotThread, AppError> {
    let now = Timestamp::now();
    match query_as!(
        ChatbotThread,
        "WITH seat AS (
             UPDATE app_user SET chatbot_thread_count = chatbot_thread_count + 1
             WHERE id = $1
               AND chatbot_thread_count < COALESCE(
                     (SELECT max_chatbot_threads FROM settings WHERE id = 'school'),
                     $2)
             RETURNING 1)
         INSERT INTO chatbot_thread (id, user_id, title, created_at, updated_at)
         SELECT $3, $1, $4, $5, $5
         WHERE EXISTS (SELECT 1 FROM seat)
         RETURNING id AS \"id: ChatbotThreadId\", user_id AS \"user_id: UserId\", title AS \"title: ChatbotThreadTitle\", \
                   created_at AS \"created_at: Timestamp\", updated_at AS \"updated_at: Timestamp\"",
        user.uuid(),
        DEFAULT_MAX_CHATBOT_THREADS,
        ChatbotThreadId::generate().uuid(),
        title.map(|t| t.as_str().to_string()),
        now.as_millis(),
    )
    .fetch_optional(db)
    .await?
    {
        Some(saved) => Ok(saved),
        // Full, or the user's row is gone — the conditional write matches
        // nothing either way, as before.
        None => Err(AppError::Conflict(
            "you have reached the school's limit on saved threads — delete one first",
        )),
    }
}

/// A user's threads, most recently active first — the sort the
/// `chatbot_thread_user_updated` index exists for.
pub async fn list_for_user(
    db: &Database,
    user: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<ChatbotThread>, i64), AppError> {
    PagedList::new(
        "chatbot_thread WHERE user_id = $1",
        "ORDER BY updated_at DESC, id DESC",
    )
    .bind(user.uuid())
    .run(limit, offset, db)
    .await
}

/// How many threads `user` keeps — the `max_chatbot_threads` cap check.
pub async fn count_for_user(db: &Database, user: &UserId) -> Result<usize, AppError> {
    let row = sqlx::query!(
        "SELECT count(*) AS threads FROM chatbot_thread WHERE user_id = $1",
        user.uuid()
    )
    .fetch_one(db)
    .await?;
    Ok(row.threads.unwrap_or(0).max(0) as usize)
}

/// Read a thread only if `user` owns it — a foreign id reads as absent, so
/// the web layer answers 404 rather than leaking that it exists.
pub async fn read_for(
    db: &Database,
    id: &ChatbotThreadId,
    user: &UserId,
) -> Result<Option<ChatbotThread>, AppError> {
    let thread = query_as!(
        ChatbotThread,
        "SELECT id AS \"id: ChatbotThreadId\", user_id AS \"user_id: UserId\", title AS \"title: ChatbotThreadTitle\", created_at AS \"created_at: Timestamp\", updated_at AS \"updated_at: Timestamp\" FROM chatbot_thread WHERE id = $1",
        id.uuid()
    )
    .fetch_optional(db)
    .await?;
    Ok(thread.filter(|thread| &thread.user_id == user))
}

// Stamping new activity is not a call of its own: it is the monotonic bump
// inside every write that touches the thread (a turn's insert, the rename,
// the delete below), so a stamp can no longer be lost (it used to be a
// best-effort query after the two creates, whose failure was a `warn!`) and
// no turn can be written without it.

/// Rename the thread (`None` clears the name back to untitled), stamping
/// the edit as activity. Field-scoped: a turn may be landing concurrently.
/// The stamp is written strictly upwards — `GREATEST($now, updated_at + 1)`
/// — because a thread's writes routinely land inside one millisecond and
/// `updated_at` is the list's ordering key; the stamp may sit a few
/// milliseconds ahead of the clock, which an ordering key does not care
/// about. Zero rows: the thread is gone, the same 404 the delete answers.
pub async fn rename(
    db: &Database,
    thread: &ChatbotThread,
    title: Option<ChatbotThreadTitle>,
) -> Result<ChatbotThread, AppError> {
    let updated = query_as!(
        ChatbotThread,
        "UPDATE chatbot_thread SET title = $2, updated_at = GREATEST($3, updated_at + 1) \
         WHERE id = $1 \
         RETURNING id AS \"id: ChatbotThreadId\", user_id AS \"user_id: UserId\", title AS \"title: ChatbotThreadTitle\", \
                   created_at AS \"created_at: Timestamp\", updated_at AS \"updated_at: Timestamp\"",
        thread.get_id().uuid(),
        title.map(|t| t.as_str().to_string()),
        Timestamp::now().as_millis(),
    )
    .fetch_optional(db)
    .await?;
    updated.ok_or(AppError::NotFound)
}

/// Delete the thread and every turn in it — one transaction, so a crash
/// can't orphan messages under a vanished thread. The owner's slot comes
/// back in that same transaction, or the cap would ratchet shut.
///
/// A turn's insert (see [`crate::db::chatbot_message`]) writes *through*
/// the thread's row — its guarded statement moves `updated_at`, the key
/// this delete's final write removes — so under Postgres's row locking the
/// two serialize: either the turn saw the thread and committed first (the
/// delete then refuses nothing, the sweep takes the turn), or the thread
/// was gone and the turn's gate matched nothing. No turn outlives its
/// thread. On a re-sent round the thread is already gone, so the caller
/// gets the `404` that is the truth.
pub async fn delete(db: &Database, thread: ChatbotThread) -> Result<ChatbotThread, AppError> {
    tx_with_retry(db, false, async move |tx| {
        // Children first: the foreign key would refuse the parent while a
        // turn still names it.
        sqlx::query!(
            "DELETE FROM chatbot_message WHERE thread_id = $1",
            thread.get_id().uuid()
        )
        .execute(&mut *tx)
        .await?;
        let gone = query_as!(
            ChatbotThread,
            "DELETE FROM chatbot_thread WHERE id = $1 \
             RETURNING id AS \"id: ChatbotThreadId\", user_id AS \"user_id: UserId\", title AS \"title: ChatbotThreadTitle\", \
                       created_at AS \"created_at: Timestamp\", updated_at AS \"updated_at: Timestamp\"",
            thread.get_id().uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(gone) = &gone {
            sqlx::query!(
                "UPDATE app_user SET chatbot_thread_count = GREATEST(chatbot_thread_count - 1, 0) \
                 WHERE id = $1",
                gone.get_user_id().uuid()
            )
            .execute(&mut *tx)
            .await?;
        }
        gone.ok_or(AppError::NotFound)
    })
    .await
}
#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row as _;

    use crate::db::chatbot_message;
    use crate::domain::chatbot_message::ChatContent;
    use crate::domain::settings::{Settings, SettingsParams};

    /// The fixed fixture person, by a valid id every helper can name.
    const U: &str = "019732e3-7b00-7000-8000-00000000aaaa";

    /// The owner's counter and the threads it counts, both re-read out of the
    /// store — never off a return value.
    async fn stored(db: &Database) -> (i64, usize) {
        let counter = sqlx::query("SELECT chatbot_thread_count FROM app_user WHERE id = $1")
            .bind(UserId::from_key(U).uuid())
            .fetch_one(db)
            .await
            .unwrap()
            .try_get::<i64, _>(0)
            .unwrap();
        let rows = sqlx::query("SELECT count(*) FROM chatbot_thread")
            .fetch_one(db)
            .await
            .unwrap()
            .try_get::<i64, _>(0)
            .unwrap();
        (counter, rows as usize)
    }

    async fn a_user_capped_at(threads: i64) -> (Database, crate::database::TestDatabases) {
        let (db, leases) = crate::database::init_test_db().await;
        sqlx::query("INSERT INTO app_user (id, username, created_at) VALUES ($1, 'u', 0)")
            .bind(UserId::from_key(U).uuid())
            .execute(&db)
            .await
            .unwrap();
        crate::db::settings::save(
            &db,
            Settings::try_new(SettingsParams {
                max_chatbot_threads: threads,
                ..Settings::defaults().params()
            })
            .unwrap(),
        )
        .await
        .unwrap();
        (db, leases)
    }

    /// The seat and the row commit together, so the counter the cap reads can
    /// never disagree with the threads it counts — and a create refused at the
    /// cap advances neither.
    #[tokio::test]
    async fn a_capped_create_moves_the_counter_with_the_row() {
        let (db, _leases) = a_user_capped_at(1).await;
        let user = UserId::from_key(U);

        create_capped(&db, &user, None).await.expect("first thread");
        assert_eq!(stored(&db).await, (1, 1));

        let refused = create_capped(&db, &user, None).await;
        assert!(matches!(refused, Err(AppError::Conflict(_))), "at the cap");
        assert_eq!(
            stored(&db).await,
            (1, 1),
            "a refused create advances neither"
        );
    }

    /// A turn is written *through* its thread's row, so a thread that is gone
    /// takes the write with it — and one that is there is stamped by it.
    /// The race this shape exists to close is the `#[ignore]`d test below;
    /// this one pins the logic, which the in-memory engine can answer.
    #[tokio::test]
    async fn a_turn_writes_its_thread_and_dies_with_it() {
        let (db, _leases) = a_user_capped_at(2).await;
        let user = UserId::from_key(U);
        let thread = create_capped(&db, &user, None).await.expect("thread");
        let opened = thread.get_updated_at().as_millis();

        let say = || ChatContent::try_new("selam").unwrap();
        chatbot_message::append_user(&db, thread.get_id(), &user, say())
            .await
            .expect("append");
        let stamped = read_for(&db, thread.get_id(), &user)
            .await
            .expect("re-read")
            .expect("still there")
            .get_updated_at()
            .as_millis();
        // Strictly upwards, never merely re-stamped: an `UPDATE` that leaves
        // the row unchanged is elided, and both rows of a turn are routinely
        // written inside one millisecond of the thread's own creation.
        assert!(stamped > opened, "{stamped} !> {opened}");

        let id = thread.get_id().clone();
        delete(&db, thread).await.expect("delete");
        let orphan = chatbot_message::append_user(&db, &id, &user, say()).await;
        assert!(
            matches!(orphan, Err(AppError::NotFound)),
            "a turn on a deleted thread must be refused: {orphan:?}"
        );
        assert_eq!(
            chatbot_message::list_for_thread(&db, &id, None, 0)
                .await
                .expect("list")
                .1,
            0,
            "the refused turn wrote nothing"
        );
    }

    /// The stamp moves **strictly upwards**, never to the clock — the property
    /// the whole shape rests on, since an `UPDATE` that leaves the row
    /// unchanged is elided, never reaches the store's write set, and so
    /// collides with the racing delete not at all.
    ///
    /// The test above cannot see it: it compares one append against
    /// `create_capped`, which costs a millisecond or so on its own, and a plain
    /// `$now` clears that. Nor can a burst of appends — each is a transaction
    /// of its own and takes about as long, so the clock has moved on by the
    /// time the next one reads it. Timing cannot reach the window on demand.
    ///
    /// So the *state* the window produces is set up directly instead: a stamp
    /// **ahead of the clock**, which is precisely what `math::max` leaves
    /// behind (see [`touch_and_write`]) and therefore an ordinary row, not a
    /// contrived one. From there the two rules are told apart with no race at
    /// all — upwards keeps climbing off the stored value, the clock drags the
    /// stamp back down to itself. The lead is a second, far more than two
    /// writes can burn, and the guard below fails loudly rather than vacuously
    /// on a machine that manages to burn it.
    #[tokio::test]
    async fn a_stamp_ahead_of_the_clock_still_climbs() {
        const LEAD: i64 = 1_000;

        let (db, _leases) = a_user_capped_at(1).await;
        let user = UserId::from_key(U);
        let thread = create_capped(&db, &user, None).await.expect("thread");

        let parked = Timestamp::now().as_millis() + LEAD;
        sqlx::query("UPDATE chatbot_thread SET updated_at = $1 WHERE id = $2")
            .bind(parked)
            .bind(thread.get_id().uuid())
            .execute(&db)
            .await
            .unwrap();

        for _ in 0..2 {
            chatbot_message::append_user(
                &db,
                thread.get_id(),
                &user,
                ChatContent::try_new("selam").unwrap(),
            )
            .await
            .expect("append");
        }
        let stamped = read_for(&db, thread.get_id(), &user)
            .await
            .expect("re-read")
            .expect("still there")
            .get_updated_at()
            .as_millis();

        assert!(
            Timestamp::now().as_millis() < parked,
            "the clock caught the {LEAD}ms lead up during two appends, so this \
             probe proves nothing"
        );
        assert_eq!(
            stamped,
            parked + 2,
            "two appends off a stamp of {parked} must leave {}: a stamp that \
             follows the clock instead is one an unchanged-row UPDATE elides",
            parked + 2
        );
    }

    /// No turn may survive the delete that swept its thread. An orphan is not
    /// litter: nothing sweeps `chatbot_message` afterwards, and both
    /// `GET /chatbot/threads/{id}/messages/{mid}` and its `/stream` answered
    /// `200` with the text of a thread the user had deleted.
    ///
    /// The window is between [`delete`]'s sweep and its commit:
    /// a create landing in there reads a thread that is still present
    /// (uncommitted) while the sweep ran on a snapshot predating the new row,
    /// so both commit. It is closed by making the create **write** the thread
    /// row ([`touch_and_write`]) instead of reading it — the two transactions
    /// then touch one key and the store refuses to commit both.
    ///
    /// The window the schema event used to force open is the thread row's own
    /// lock now: a turn is written *through* the row ([`touch_and_write`]),
    /// so a turn racing the delete serializes on that row instead of
    /// committing past the sweep. Both orders are forced by awaiting one side
    /// to completion before the other starts — a barrier is a coin toss under
    /// load, and a run where the delete loses every overlapped round has still
    /// proved the invariant. A few rounds still start together; those may all
    /// lose the delete, and that is not a failure.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_turn_written_inside_a_delete_never_outlives_the_thread() {
        // Forced rounds free their seat. Concurrent rounds may keep theirs, so
        // the cap covers every thread this test can leave standing.
        let (db, _leases) = a_user_capped_at(6).await;
        let user = UserId::from_key(U);

        async fn append(
            db: &Database,
            id: &ChatbotThreadId,
            user: &UserId,
        ) -> Result<(), AppError> {
            chatbot_message::append_user(db, id, user, ChatContent::try_new("selam").unwrap())
                .await
                .map(|_| ())
        }

        // Delete-first: the thread is gone before the turn starts.
        {
            let thread = create_capped(&db, &user, None).await.expect("thread");
            let id = thread.get_id().clone();
            let dropped = delete(&db, thread).await;
            assert!(
                !matches!(dropped, Err(AppError::Db(_))),
                "delete-first: a delete must be answered, not 500: {dropped:?}"
            );
            assert!(
                read_for(&db, &id, &user).await.unwrap().is_none(),
                "delete-first: the thread is still there after delete: {dropped:?}"
            );
            let turn = append(&db, &id, &user).await;
            assert!(
                !matches!(turn, Err(AppError::Db(_))),
                "delete-first: a turn must be answered, not 500: {turn:?}"
            );
            assert_eq!(
                chatbot_message::list_for_thread(&db, &id, None, 0)
                    .await
                    .unwrap()
                    .1,
                0,
                "a turn outlived its thread"
            );
        }

        // Write-first: the turn lands, then the delete sweeps it.
        {
            let thread = create_capped(&db, &user, None).await.expect("thread");
            let id = thread.get_id().clone();
            let turn = append(&db, &id, &user).await;
            assert!(
                turn.is_ok(),
                "write-first: the turn must land before the delete: {turn:?}"
            );
            let dropped = delete(&db, thread).await;
            assert!(
                !matches!(dropped, Err(AppError::Db(_))),
                "write-first: a delete must be answered, not 500: {dropped:?}"
            );
            assert!(
                read_for(&db, &id, &user).await.unwrap().is_none(),
                "write-first: the thread is still there"
            );
            assert_eq!(
                chatbot_message::list_for_thread(&db, &id, None, 0)
                    .await
                    .unwrap()
                    .1,
                0,
                "a turn outlived its thread"
            );
        }

        // Overlapped rounds. Delete may lose every one of them.
        let mut swept = 0;
        for round in 0..4 {
            let thread = create_capped(&db, &user, None).await.expect("thread");
            let id = thread.get_id().clone();

            let gate = std::sync::Arc::new(tokio::sync::Barrier::new(2));
            let drop_it = {
                let (db, thread, gate) = (db.clone(), thread, gate.clone());
                tokio::spawn(async move {
                    gate.wait().await;
                    delete(&db, thread).await
                })
            };
            let turn = {
                let (id, db, user, gate) = (id.clone(), db.clone(), user, gate);
                tokio::spawn(async move {
                    gate.wait().await;
                    append(&db, &id, &user).await
                })
            };
            let (drop_it, turn) = (drop_it.await.unwrap(), turn.await.unwrap());
            assert!(
                !matches!(turn, Err(AppError::Db(_))),
                "round {round}: a raced turn must be answered, not 500: {turn:?}"
            );
            // A lost round here is a 23503, not a missed sweep. The turn's
            // insert and this delete's `DELETE chatbot_message` do not share a
            // row lock, `chatbot_message.thread_id` is `ON DELETE NO ACTION`,
            // and [`delete`] does not retry that (`cascade = false`). The
            // thread is still there; nothing outlived a gone parent. Any other
            // database error is a 500. The forced rounds above are the ones
            // that require the delete itself to answer without one.
            let parent_gone = read_for(&db, &id, &user).await.unwrap().is_none();
            if let Err(AppError::Db(err)) = &drop_it {
                let lost_the_gap = crate::database::foreign_key_violation(err) && !parent_gone;
                assert!(
                    lost_the_gap,
                    "round {round}: a raced delete must be answered, not 500: {drop_it:?}"
                );
            }
            if parent_gone {
                swept += 1;
                assert_eq!(
                    chatbot_message::list_for_thread(&db, &id, None, 0)
                        .await
                        .unwrap()
                        .1,
                    0,
                    "round {round}: a turn outlived its thread"
                );
            } else if drop_it.is_ok() {
                panic!("round {round}: the delete reported success but the thread is still there");
            }
        }
        eprintln!(
            "chatbot_thread::delete raced by a turn: {swept}/4 concurrent rounds deleted the thread"
        );
    }
}
