//! The `board` table: whiteboard rooms and their rosters. Every write here is
//! field-scoped `UPDATE … SET` — a stroke landing concurrently is moving the
//! two counters this table's struct does not carry.

use surrealdb::types::SurrealValue;

use crate::constant::{MAX_BOARDS_PER_CREATOR, USER_BOARD_COUNT_FIELD};
use crate::database::{Database, transaction_with_retry};
use crate::db::cap;
use crate::db::page::PagedList;
use crate::domain::board::{Board, BoardId, BoardTitle, checked_participants};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Open a board, taking a slot on the creator's `board_count` in the same
/// transaction as the row — the counter is the authority on how many
/// boards exist, so it can never count a row that did not commit.
pub async fn create(
    db: &Database,
    creator: &UserId,
    title: BoardTitle,
    participants: Vec<UserId>,
) -> Result<Board, AppError> {
    let board = Board {
        id: BoardId::generate(),
        creator: creator.clone(),
        title,
        participants: checked_participants(participants)?,
        locked: false,
        locked_by: None,
        locked_at: None,
        epoch: 0,
        closed_at: None,
        created_at: Timestamp::now(),
    };
    match cap::claim_and_create(
        &creator.record(),
        USER_BOARD_COUNT_FIELD,
        MAX_BOARDS_PER_CREATOR,
        &board.id.record(),
        &board,
        db,
    )
    .await?
    {
        cap::Claimed::Made(saved) => Ok(saved),
        // Full, or the creator's row is gone — the conditional write
        // matches nothing either way.
        cap::Claimed::Full => Err(AppError::Conflict(
            "you have reached the limit on boards — delete one first",
        )),
        // The id is a freshly minted ULID on a table with no UNIQUE index,
        // so no rival can have aimed at it.
        cap::Claimed::Duplicate => Err(AppError::Internal("board id collided".into())),
    }
}

pub async fn read(db: &Database, id: &BoardId) -> Result<Option<Board>, AppError> {
    Ok(db.select(id.record()).await?)
}

/// Every board `user` may open: the ones they created and the ones they
/// were invited to. Newest first — the ids are monotonic, so `id` is the
/// creation order.
///
/// `open` narrows by `closed_at`, and it means exactly that flag: a board
/// that is locked, or full at its lifetime cap but never drawn on again, is
/// never stamped and so reads as **open** — because it is. Only a creator's
/// `/close` and the lifetime cap's own refusal ever stamp one.
pub async fn list_for_user(
    db: &Database,
    user: &UserId,
    open: Option<bool>,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Board>, i64), AppError> {
    let open_clause = match open {
        Some(true) => " AND closed_at = NONE",
        Some(false) => " AND closed_at != NONE",
        None => "",
    };
    PagedList::new(
        format!("board WHERE (creator = $usr OR participants CONTAINS $usr){open_clause}"),
        "ORDER BY id DESC",
    )
    .bind("usr", user.record())
    .run(limit, offset, db)
    .await
}

/// Re-invite. Field-scoped, like every write here: a stroke landing
/// concurrently is moving the counters this struct does not carry.
pub async fn set_participants(
    db: &Database,
    board: &Board,
    participants: Vec<UserId>,
) -> Result<Board, AppError> {
    let participants = checked_participants(participants)?;
    let mut result = db
        .query("UPDATE $id SET participants = $who RETURN AFTER")
        .bind(("id", board.id.record()))
        .bind((
            "who",
            participants
                .iter()
                .map(|user| user.record())
                .collect::<Vec<_>>(),
        ))
        .await?
        .check()?;
    one(result.take::<Vec<Board>>(0)?)
}

pub async fn set_title(db: &Database, board: &Board, title: BoardTitle) -> Result<Board, AppError> {
    let mut result = db
        .query("UPDATE $id SET title = $title RETURN AFTER")
        .bind(("id", board.id.record()))
        .bind(("title", title.0))
        .await?
        .check()?;
    one(result.take::<Vec<Board>>(0)?)
}

/// Freeze or thaw drawing. `locked_by`/`locked_at` are cleared on unlock so
/// the row never claims a lock that is not held.
pub async fn set_locked(
    db: &Database,
    board: &Board,
    locked: bool,
    by: &UserId,
) -> Result<Board, AppError> {
    let now = Timestamp::now();
    let mut result = db
        .query("UPDATE $id SET locked = $locked, locked_by = $by, locked_at = $at RETURN AFTER")
        .bind(("id", board.id.record()))
        .bind(("locked", locked))
        .bind(("by", locked.then(|| by.record())))
        .bind(("at", locked.then(|| now.as_millis())))
        .await?
        .check()?;
    one(result.take::<Vec<Board>>(0)?)
}

// The demotion sweep — stripping a user off every roster they are listed
// on, deleting nothing, and handing the affected rooms back so the caller
// can prompt them — lives in [`crate::service::user::set_role`], where
// it commits with the role write that invalidates the membership.

/// Retire the board: permanently read-only, history still readable.
/// Idempotent by the `WHERE` — a second call matches nothing and the first
/// stamp stands, which is what the stroke path's open-guard reads.
pub async fn close(db: &Database, board: &Board) -> Result<Board, AppError> {
    let mut result = db
        .query("UPDATE $id SET closed_at = $now WHERE closed_at = NONE RETURN AFTER")
        .bind(("id", board.id.record()))
        .bind(("now", Timestamp::now().as_millis()))
        .await?
        .check()?;
    match result.take::<Vec<Board>>(0)?.into_iter().next() {
        Some(board) => Ok(board),
        // Already closed: the row is unchanged, so read it back rather
        // than report a 404 for a board that plainly exists.
        None => read(db, &board.id).await?.ok_or(AppError::NotFound),
    }
}

/// Delete the board and give the creator's slot back in the *same*
/// transaction — a release issued afterwards can be lost, and the counter
/// would then ratchet the creator's limit shut forever.
///
/// The stroke cascade rides in that same transaction: it is the one and
/// only legitimate delete of `board_stroke` rows (a clear deletes nothing),
/// and issued separately it could leave a board's whole history orphaned
/// under a record that no longer exists.
pub async fn delete(db: &Database, board: Board) -> Result<Board, AppError> {
    // A stroke claim writes the very board row this deletes, so the store
    // aborts one of the two and a lost round is ordinary here — re-sent
    // rather than reported as a 500. Re-sending is sound: every statement
    // is a `DELETE` or a field-scoped `UPDATE`, none of which can ever
    // answer "already exists" (see [`transaction_with_retry`]).
    let (mut result, mut errors) = transaction_with_retry(
        db,
        "BEGIN TRANSACTION;
         DELETE board_stroke WHERE board = $id;
         LET $gone = (DELETE $id RETURN BEFORE);
         UPDATE $usr SET board_count = math::max([(board_count ?? 0) - array::len($gone), 0]);
         RETURN $gone;
         COMMIT TRANSACTION;",
        &[
            ("id".into(), board.id.record().into_value()),
            ("usr".into(), board.creator.record().into_value()),
        ],
        &[],
    )
    .await?;
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    // Slots count BEGIN, the cascade, the LET and the UPDATE: the RETURN
    // is slot 4.
    one(result.take::<Vec<Board>>(4)?)
}

fn one(rows: Vec<Board>) -> Result<Board, AppError> {
    rows.into_iter().next().ok_or(AppError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use surrealdb::types::RecordId;

    async fn a_db() -> Database {
        let db = crate::database::init_mem().await.unwrap();
        db.query(
            "CREATE user:c SET username = 'c', password_hash = 'x';
             CREATE user:p SET username = 'p', password_hash = 'x';
             CREATE user:s SET username = 's', password_hash = 'x';",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
        db
    }

    fn user(key: &str) -> UserId {
        UserId::from_key(key)
    }

    /// The stored counter, re-read out of the database. Never asserted off a
    /// return value: the in-memory engine forges concurrent-write wins
    /// (src/db/cap.rs:44-49).
    async fn stored_count(db: &Database) -> i64 {
        let mut result = db
            .query("SELECT VALUE board_count ?? 0 FROM user:c")
            .await
            .unwrap()
            .check()
            .unwrap();
        result.take::<Vec<i64>>(0).unwrap()[0]
    }

    async fn a_board(db: &Database) -> Board {
        create(
            db,
            &user("c"),
            BoardTitle::try_new("Geometri").unwrap(),
            vec![user("p")],
        )
        .await
        .unwrap()
    }

    /// The cap refuses at `MAX_BOARDS_PER_CREATOR`, and the refusal writes
    /// nothing — asserted by re-reading the counter, not off the return value.
    #[tokio::test]
    async fn the_per_creator_cap_refuses_a_full_creator() {
        let db = a_db().await;
        db.query("UPDATE user:c SET board_count = $full")
            .bind(("full", MAX_BOARDS_PER_CREATOR))
            .await
            .unwrap()
            .check()
            .unwrap();
        let refused = create(
            &db,
            &user("c"),
            BoardTitle::try_new("Geometri").unwrap(),
            vec![],
        )
        .await;
        assert!(matches!(refused, Err(AppError::Conflict(_))));
        assert_eq!(stored_count(&db).await, MAX_BOARDS_PER_CREATOR);
        let mut result = db
            .query("SELECT VALUE id FROM board")
            .await
            .unwrap()
            .check()
            .unwrap();
        assert!(result.take::<Vec<RecordId>>(0).unwrap().is_empty());
    }

    /// A seat is taken on create and handed back on delete, both read back out
    /// of the store.
    #[tokio::test]
    async fn a_seat_is_claimed_and_released() {
        let db = a_db().await;
        let board = a_board(&db).await;
        assert_eq!(stored_count(&db).await, 1);
        delete(&db, board).await.unwrap();
        assert_eq!(stored_count(&db).await, 0);
    }

    /// Closing twice must not re-stamp: the first `closed_at` is the record.
    #[tokio::test]
    async fn close_is_idempotent() {
        let db = a_db().await;
        let board = a_board(&db).await;
        let closed = close(&db, &board).await.unwrap();
        let first = closed.get_closed_at().unwrap();
        // A whole millisecond apart, so a re-stamp could not go unnoticed.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        assert_eq!(
            close(&db, &board).await.unwrap().get_closed_at(),
            Some(first)
        );
        assert_eq!(
            read(&db, board.get_id())
                .await
                .unwrap()
                .unwrap()
                .get_closed_at(),
            Some(first)
        );
    }

    #[tokio::test]
    async fn field_scoped_writes_leave_the_stroke_counters_alone() {
        let db = a_db().await;
        let board = a_board(&db).await;
        db.query("UPDATE $id SET epoch_stroke_count = 7, total_stroke_count = 9")
            .bind(("id", board.get_id().record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        set_title(&db, &board, BoardTitle::try_new("Cebir").unwrap())
            .await
            .unwrap();
        set_participants(&db, &board, vec![user("s")])
            .await
            .unwrap();
        set_locked(&db, &board, true, &user("c")).await.unwrap();
        close(&db, &board).await.unwrap();
        let mut result = db
            .query("SELECT VALUE [epoch_stroke_count, total_stroke_count] FROM $id")
            .bind(("id", board.get_id().record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        assert_eq!(result.take::<Vec<Vec<i64>>>(0).unwrap()[0], vec![7, 9]);
    }

    /// [`delete`] cascades the stroke log and decrements the creator's
    /// `board_count` off the very record a concurrent stroke claim increments,
    /// so the two contend by design. Losing that round writes nothing, which is
    /// what makes re-sending it the recovery; without
    /// [`crate::database::transaction_with_retry`] a lost round comes out as a
    /// 500.
    ///
    /// A refusal or an `Err(NotFound)` is *correct* here — the board really is
    /// gone — and must not fail this test. The only defect is `AppError::Db`.
    ///
    /// Multi-threaded and on a real server for the reason spelled out on
    /// [`super::super::course`]'s twin: the current-thread runtime never
    /// interleaves the two, and the embedded engine does not conflict-check
    /// concurrent writes to one record at all.
    ///
    /// Mutation status, stated honestly: cutting
    /// [`crate::database::transaction_with_retry`] to a single attempt leaves
    /// this GREEN — 5 runs of 5, every one at 20/20 contended. The mutation is
    /// real, not a dud: the same cut reddens the `course.rs` twin in 2 runs of
    /// 3. So the delete here never actually loses a round in this window, and
    /// nothing in the suite exercises its retry. Read this as a smoke test that
    /// a contended delete does not 500 — the retry stays because the batch is
    /// admissible for it and a lost round is possible in principle, not because
    /// a test has ever caught it losing one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs a real SurrealDB server: podman start hezarfen-surrealdb && cargo test -- --ignored"]
    async fn a_delete_racing_a_stroke_never_answers_500() {
        let (db, _serialized) = crate::database::init_test_server("board_delete_race").await;
        db.query("CREATE user:c SET username = 'c', password_hash = 'x';")
            .await
            .unwrap()
            .check()
            .unwrap();
        let (mut delete_500, mut stroke_500, mut drawn) = (0, 0, 0);
        let (mut last_delete, mut last_stroke) = (String::new(), String::new());
        for round in 0..20 {
            let board = a_board(&db).await;
            // The delete is held back by a sweeping beat: released together it
            // is one statement while an append spends a read before it claims,
            // so it would win every round and the guard would never be
            // contended at all. Six racers over a 0-3ms sweep put the delete
            // somewhere inside the counter writes instead.
            let drop_it = {
                let (board, db) = (board.clone(), db.clone());
                let beat = std::time::Duration::from_millis(round % 4);
                tokio::spawn(async move {
                    tokio::time::sleep(beat).await;
                    delete(&db, board).await
                })
            };
            let marks: Vec<_> = (0..6)
                .map(|mark| {
                    let (id, db) = (board.get_id().clone(), db.clone());
                    let author = user("c");
                    tokio::spawn(async move {
                        crate::db::board_stroke::append(
                            &db,
                            &id,
                            &author,
                            &format!("{{\"m\":{mark}}}"),
                            0,
                        )
                        .await
                    })
                })
                .collect();
            let drop_it = drop_it.await.unwrap();
            if matches!(drop_it, Err(AppError::Db(_))) {
                delete_500 += 1;
                last_delete = format!("{drop_it:?}");
            }
            // Contention is counted off the claim's own outcome, not off stored
            // rows: the delete cascades `board_stroke`, so a stroke that landed
            // and then lost its board leaves nothing behind to count. An `Ok`
            // means the claim committed against the board row the delete was
            // tearing down, which is exactly the overlap being measured.
            let mut claimed = 0;
            for mark in marks {
                let mark = mark.await.unwrap();
                if matches!(mark, Err(AppError::Db(_))) {
                    stroke_500 += 1;
                    last_stroke = format!("{mark:?}");
                }
                if mark.is_ok() {
                    claimed += 1;
                }
            }
            if claimed > 0 {
                drawn += 1;
            }
        }
        eprintln!(
            "Board::delete raced: {delete_500}/20 delete 500s, {stroke_500} stroke 500s, \
             {drawn}/20 rounds with a stroke claimed"
        );
        assert!(
            drawn > 0,
            "no round ever landed a stroke, so the delete's cascade was never contended"
        );
        assert_eq!(
            delete_500, 0,
            "a raced delete must retry, not 500: {delete_500}/20 rounds, last {last_delete}"
        );
        assert_eq!(
            stroke_500, 0,
            "a raced stroke must retry, not 500: {stroke_500}/20 rounds, last {last_stroke}"
        );
    }
}
