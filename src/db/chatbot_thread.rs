//! The `chatbot_thread` table: one user's AI-chat threads, listed
//! newest-activity-first, deleted together with every turn in them. The
//! entity and its validated newtypes live in
//! [`crate::domain::chatbot_thread`]; this module is where their rows are
//! read and written.

use surrealdb::types::{SurrealValue, Value};

use crate::constant::{
    CHATBOT_THREAD_COUNT_FIELD, DEFAULT_MAX_CHATBOT_THREADS, SETTINGS_KEY, SETTINGS_TABLE,
};
use crate::database::{Database, transaction_with_retry};
use crate::db::cap;
use crate::db::page::PagedList;
use crate::domain::chatbot_thread::{ChatbotThread, ChatbotThreadId, ChatbotThreadTitle};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Move the thread's `updated_at` **and** run `statement` — a write to a turn
/// hanging off that thread — in one transaction, handing back the row it
/// returned. [`AppError::NotFound`] means the thread is gone and `statement`
/// wrote nothing.
///
/// The bump is what ties a turn's fate to its thread, and reading the thread
/// first does not: [`delete`] sweeps `chatbot_message` and drops
/// the thread in one transaction, so a create landing after that sweep but
/// before its commit reads a thread that is still there (uncommitted) while the
/// sweep ran on a snapshot predating the new row — both commit, and the turn
/// outlives the thread it was deleted with, readable and deletable by nothing.
/// Reads do not conflict; only a write that *moves* a value on a key the delete
/// also writes does. That is the shape
/// [`bump_menu_and_write`](crate::domain::menu::bump_menu_and_write) and
/// [`ExamAnswer::save`](crate::domain::exam_answer::ExamAnswer::save) already
/// use, and the sweep's ordering inside the delete stops mattering once it is
/// in place.
///
/// `updated_at` is the value moved, and it doubles as the stamp the thread list
/// sorts on — this *is* the activity touch the send path used to issue
/// afterwards as a separate, best-effort query whose failure was only logged.
/// It is written strictly upwards rather than to `$now`: an `UPDATE` that
/// leaves the document unchanged is elided and never reaches the store's write
/// set, a turn's two rows are routinely written inside one millisecond, and a
/// write that does not happen collides with nothing. The stamp can therefore
/// sit a few milliseconds ahead of the clock, which an ordering key does not
/// care about.
///
/// Admissible for [`transaction_with_retry`] as long as `statement` is: the
/// `UPDATE`, the `IF`/`THROW` and the `RETURN` can never answer "already
/// exists", and neither can a `CREATE` on a freshly minted ULID on a table with
/// no `UNIQUE` index — no rival can have aimed at it, and a lost round wrote
/// nothing, so re-sending it is the recovery.
pub(crate) async fn touch_and_write<T: SurrealValue>(
    db: &Database,
    thread: &ChatbotThreadId,
    statement: &str,
    mut bindings: Vec<(String, Value)>,
) -> Result<Option<T>, AppError> {
    bindings.push(("conv".into(), thread.record().into_value()));
    bindings.push(("now".into(), Timestamp::now().as_millis().into_value()));
    let (mut result, mut errors) = transaction_with_retry(
        db,
        &format!(
            "BEGIN TRANSACTION;
             LET $touched = (UPDATE $conv SET updated_at = \
                 math::max([$now, updated_at + 1]) RETURN VALUE id);
             IF array::len($touched) = 0 {{ THROW 'no_thread' }};
             LET $row = ({statement});
             RETURN $row;
             COMMIT TRANSACTION;"
        ),
        &bindings,
        &["no_thread"],
    )
    .await?;
    // An aborted transaction errors *every* slot, most with a generic "not
    // executed" — only the THROW's own slot names the reason.
    if errors
        .values()
        .any(|error| error.to_string().contains("no_thread"))
    {
        return Err(AppError::NotFound);
    }
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    // Slots count BEGIN, the LET, the IF and the second LET: the RETURN is 4.
    Ok(result.take::<Vec<T>>(4)?.into_iter().next())
}

/// Start a thread unless `user` is already at the school's
/// `max_chatbot_threads`. The slot is taken on the user row in the same
/// transaction as the thread ([`cap::claim_live_and_create`]) — an atomic
/// single-record write, so two requests racing the same user's last slot
/// cannot both win and the counter can never count a row that did not
/// commit.
///
/// The cap is the one *live* on the settings singleton, sub-queried inside
/// that same conditional write rather than bound as a number: this cap does
/// not live on the parent row (the seat is on the user, the limit is the
/// school's), and a snapshot of it admits every request already in flight
/// when a `PATCH /settings` lowers it. The subquery costs one extra record
/// read per claim on a path that already reads the row it writes.
/// `DEFAULT_MAX_CHATBOT_THREADS` is the fallback the settings row's own
/// absent column means — no row, or a school that never set the knob.
pub async fn create_capped(
    db: &Database,
    user: &UserId,
    title: Option<ChatbotThreadTitle>,
) -> Result<ChatbotThread, AppError> {
    let now = Timestamp::now();
    let thread = ChatbotThread {
        id: ChatbotThreadId::generate(),
        user_id: user.clone(),
        title,
        created_at: now,
        updated_at: now,
    };
    match cap::claim_live_and_create(
        &user.record(),
        CHATBOT_THREAD_COUNT_FIELD,
        &format!(
            "(SELECT VALUE max_chatbot_threads FROM ONLY {SETTINGS_TABLE}:{SETTINGS_KEY}) ?? $num"
        ),
        DEFAULT_MAX_CHATBOT_THREADS,
        // No holder claim: the parent row *is* the user's, so this write
        // already lands on the key a role change writes.
        None,
        (&thread.id.record(), &thread),
        db,
    )
    .await?
    {
        cap::Claimed::Made(saved) => Ok(saved),
        // Full, or the user's row is gone — the conditional write matches
        // nothing either way.
        cap::Claimed::Full => Err(AppError::Conflict(
            "you have reached the school's limit on saved threads — delete one first",
        )),
        // The id is a freshly minted ULID on a table with no UNIQUE index,
        // so no rival can have aimed at it.
        cap::Claimed::Duplicate => Err(AppError::Internal("thread id collided".into())),
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
        "chatbot_thread WHERE user_id = $usr",
        "ORDER BY updated_at DESC, id DESC",
    )
    .bind("usr", user.record())
    .run(limit, offset, db)
    .await
}

/// How many threads `user` keeps — the `max_chatbot_threads` cap check.
pub async fn count_for_user(db: &Database, user: &UserId) -> Result<usize, AppError> {
    let mut result = db
        .query("SELECT VALUE count() FROM chatbot_thread WHERE user_id = $usr GROUP ALL")
        .bind(("usr", user.record()))
        .await?
        .check()?;
    Ok(result
        .take::<Vec<i64>>(0)?
        .first()
        .copied()
        .unwrap_or_default()
        .max(0) as usize)
}

/// Read a thread only if `user` owns it — a foreign id reads as absent, so
/// the web layer answers 404 rather than leaking that it exists.
pub async fn read_for(
    db: &Database,
    id: &ChatbotThreadId,
    user: &UserId,
) -> Result<Option<ChatbotThread>, AppError> {
    let thread: Option<ChatbotThread> = db.select(id.record()).await?;
    Ok(thread.filter(|thread| &thread.user_id == user))
}

// Stamping new activity is not a call of its own: it is the bump inside
// [`touch_and_write`], which every turn already rides, so a stamp can no
// longer be lost (it used to be a best-effort query after the two creates,
// whose failure was a `warn!`) and no turn can be written without it.

/// Rename the thread (`None` clears the name back to untitled), stamping
/// the edit as activity. Field-scoped for the same reason as
/// [`touch_and_write`]: a turn may be landing concurrently.
pub async fn rename(
    db: &Database,
    thread: &ChatbotThread,
    title: Option<ChatbotThreadTitle>,
) -> Result<ChatbotThread, AppError> {
    let mut result = db
        .query("UPDATE $id SET title = $title, updated_at = $now RETURN AFTER")
        .bind(("id", thread.id.record()))
        .bind(("title", title.map(|title| title.0)))
        .bind(("now", Timestamp::now().as_millis()))
        .await?
        .check()?;
    result
        .take::<Vec<ChatbotThread>>(0)?
        .into_iter()
        .next()
        .ok_or(AppError::NotFound)
}

/// Delete the thread and every turn in it — one transaction, so a crash
/// can't orphan messages under a vanished thread. The owner's slot comes
/// back in that same transaction, or the cap would ratchet shut.
///
/// A turn, a rename and this delete all write the thread row (see
/// [`touch_and_write`]), so the store aborting one of them is ordinary
/// here — that collision is exactly what keeps a turn from outliving the
/// thread — and a lost round is re-sent rather than reported as the `500`
/// it used to be. Re-sending is sound: every statement is a `DELETE` or a
/// field-scoped `UPDATE`, none of which can ever answer "already exists"
/// (see [`transaction_with_retry`]). On the re-sent round the thread is
/// already gone, so the `RETURN` is empty and the caller gets the `404`
/// that is the truth.
pub async fn delete(db: &Database, thread: ChatbotThread) -> Result<ChatbotThread, AppError> {
    let (mut result, mut errors) = transaction_with_retry(
        db,
        "BEGIN TRANSACTION;
         DELETE chatbot_message WHERE thread_id = $conv;
         LET $gone = (DELETE $conv RETURN BEFORE);
         UPDATE $usr SET chatbot_thread_count = math::max([(chatbot_thread_count ?? 0) - array::len($gone), 0]);
         RETURN $gone;
         COMMIT TRANSACTION;",
        &[
            ("conv".into(), thread.id.record().into_value()),
            ("usr".into(), thread.user_id.record().into_value()),
        ],
        &[],
    )
    .await?;
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    // Slots count BEGIN, the cascade, the LET and the UPDATE: the RETURN
    // is slot 4.
    let deleted: Option<ChatbotThread> = result.take::<Vec<ChatbotThread>>(4)?.into_iter().next();
    deleted.ok_or(AppError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use surrealdb::types::RecordId;

    use crate::db::chatbot_message;
    use crate::domain::chatbot_message::ChatContent;
    use crate::domain::settings::{Settings, SettingsParams};

    /// The owner's counter and the threads it counts, both re-read out of the
    /// store — never off a return value, which the in-memory engine forges
    /// wins on (see [`cap`]).
    async fn stored(db: &Database) -> (i64, usize) {
        let mut result = db
            .query("SELECT VALUE (chatbot_thread_count ?? 0) FROM user:u")
            .query("SELECT VALUE id FROM chatbot_thread")
            .await
            .unwrap()
            .check()
            .unwrap();
        let counter = result.take::<Vec<i64>>(0).unwrap();
        let rows = result.take::<Vec<RecordId>>(1).unwrap();
        (counter[0], rows.len())
    }

    async fn a_user_capped_at(threads: i64) -> Database {
        let db = crate::database::init_mem().await.unwrap();
        db.query("CREATE user:u SET username = 'u', password_hash = 'x';")
            .await
            .unwrap()
            .check()
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
        db
    }

    /// The seat and the row commit together, so the counter the cap reads can
    /// never disagree with the threads it counts — and a create refused at the
    /// cap advances neither.
    #[tokio::test]
    async fn a_capped_create_moves_the_counter_with_the_row() {
        let db = a_user_capped_at(1).await;
        let user = UserId::from_key("u");

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
        let db = a_user_capped_at(2).await;
        let user = UserId::from_key("u");
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

        let db = a_user_capped_at(1).await;
        let user = UserId::from_key("u");
        let thread = create_capped(&db, &user, None).await.expect("thread");

        let parked = Timestamp::now().as_millis() + LEAD;
        db.query("UPDATE $id SET updated_at = $parked")
            .bind(("id", thread.get_id().record()))
            .bind(("parked", parked))
            .await
            .unwrap()
            .check()
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
    /// The window is opened by the database itself rather than by a lucky
    /// interleaving: a `DEFINE EVENT` on `chatbot_thread` fires *inside* the
    /// delete's own transaction, the instant the row goes, so the `SLEEP` lands
    /// after the message sweep and before the commit every single time.
    ///
    /// Real server, and `#[ignore]`d for it: the subject is the store's
    /// conflict detection, which `init_mem`'s embedded engine does not have —
    /// it commits both writes and answers `Ok` to each, which would fail this
    /// test on correct code (see [`crate::database::init_test_server`]).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs a real SurrealDB server: podman start hezarfen-surrealdb && cargo test -- --ignored"]
    async fn a_turn_written_inside_a_delete_never_outlives_the_thread() {
        let (db, _serialized) = crate::database::init_test_server("chat_delete_race").await;
        db.query("CREATE user:u SET username = 'u', password_hash = 'x';")
            .await
            .unwrap()
            .check()
            .unwrap();
        // Hold the delete open for a full second after the thread row is gone,
        // while its transaction still has to commit.
        db.query(
            "DEFINE EVENT hold_the_window ON TABLE chatbot_thread WHEN $event = 'DELETE' \
             THEN { SLEEP 1s; };",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

        let user = UserId::from_key("u");
        let (mut orphans, mut swept) = (0, 0);
        for round in 0..4 {
            let thread = create_capped(&db, &user, None).await.expect("thread");
            let id = thread.get_id().clone();

            let drop_it = {
                let db = db.clone();
                tokio::spawn(async move { delete(&db, thread).await })
            };
            // The turn starts inside the held window — the thread row is gone
            // but uncommitted, which is exactly what a read believes.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let turn = {
                let (id, db, user) = (id.clone(), db.clone(), user.clone());
                tokio::spawn(async move {
                    chatbot_message::append_user(
                        &db,
                        &id,
                        &user,
                        ChatContent::try_new("selam").unwrap(),
                    )
                    .await
                    .map(|_| ())
                })
            };
            let (drop_it, turn) = (drop_it.await.unwrap(), turn.await.unwrap());
            // A 404 for the turn, or a NotFound for the delete, is a correct
            // answer — the only defect is stored state.
            assert!(
                !matches!(turn, Err(AppError::Db(_))),
                "round {round}: a raced turn must be answered, not 500: {turn:?}"
            );

            // Stored state is the whole verdict; a return value is not evidence.
            if read_for(&db, &id, &user).await.unwrap().is_none() {
                swept += 1;
                orphans += chatbot_message::list_for_thread(&db, &id, None, 0)
                    .await
                    .unwrap()
                    .1;
            } else if drop_it.is_ok() {
                panic!("round {round}: the delete reported success but the thread is still there");
            }
        }
        eprintln!("chatbot_thread::delete raced by a turn: {swept}/4 rounds deleted the thread");
        assert!(
            swept > 0,
            "no round ever deleted the thread, so the window was never reached"
        );
        assert_eq!(orphans, 0, "a turn outlived its thread");
    }
}
