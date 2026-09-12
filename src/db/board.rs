//! The `board` table: whiteboard rooms and their rosters. Every write here is
//! field-scoped `UPDATE … SET` — a stroke landing concurrently is moving the
//! two counters this table's struct does not carry.

use crate::constant::{MAX_BOARD_PARTICIPANTS, MAX_BOARDS_PER_CREATOR};
use crate::database::{Database, tx_with_retry, unique_violation};
use crate::db::page::PagedList;
use crate::domain::board::{Board, BoardId, BoardTitle, checked_participants};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

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
    // The seat claim and the row are one statement (the `cap::claim_and_create`
    // CTE, spelled out at its call site): a refused insert takes its own
    // seat bump back, so the counter can never count a row that did not commit.
    let saved = sqlx::query_as!(
        Board,
        r#"WITH seat AS (
               UPDATE app_user SET board_count = board_count + 1
               WHERE id = $1 AND board_count < $2
               RETURNING 1)
           INSERT INTO board (id, creator, title, participants, locked, locked_by, locked_at,
                              epoch, closed_at, created_at)
           SELECT $3, $1, $4, $5, false, NULL, NULL, 0, NULL, $6
           WHERE EXISTS (SELECT 1 FROM seat)
           RETURNING id, creator, title, participants, locked, locked_by, locked_at,
                     epoch, closed_at, created_at"#,
        board.creator,
        MAX_BOARDS_PER_CREATOR,
        board.id,
        board.title,
        &board.participants,
        board.created_at
    )
    .fetch_optional(db)
    .await;
    match saved {
        Ok(Some(saved)) => Ok(saved),
        // Full, or the creator's row is gone — the conditional write
        // matches nothing either way.
        Ok(None) => Err(AppError::Conflict(
            "you have reached the limit on boards — delete one first",
        )),
        // The id is a freshly minted UUID on a table whose only UNIQUE index
        // is its own primary key, so no rival can have aimed at it.
        Err(err) if unique_violation(&err).is_some() => {
            Err(AppError::Internal("board id collided".into()))
        }
        Err(err) => Err(err.into()),
    }
}

pub async fn read(db: &Database, id: &BoardId) -> Result<Option<Board>, AppError> {
    let board = sqlx::query_as!(
        Board,
        r#"SELECT id, creator, title, participants, locked, locked_by, locked_at,
                  epoch, closed_at, created_at
           FROM board WHERE id = $1"#,
        id
    )
    .fetch_optional(db)
    .await?;
    Ok(board)
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
        Some(true) => " AND closed_at IS NULL",
        Some(false) => " AND closed_at IS NOT NULL",
        None => "",
    };
    PagedList::new(
        format!("board WHERE (creator = $1 OR $2 = ANY(participants)){open_clause}"),
        "ORDER BY id DESC",
    )
    .bind(user.uuid())
    .bind(user.uuid())
    .run::<Board>(limit, offset, db)
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
    let updated = sqlx::query_as!(
        Board,
        r#"UPDATE board SET participants = $2 WHERE id = $1
           RETURNING id, creator, title, participants, locked, locked_by, locked_at,
                     epoch, closed_at, created_at"#,
        board.id,
        &participants
    )
    .fetch_optional(db)
    .await?;
    one(updated)
}

pub async fn set_title(db: &Database, board: &Board, title: BoardTitle) -> Result<Board, AppError> {
    let updated = sqlx::query_as!(
        Board,
        r#"UPDATE board SET title = $2 WHERE id = $1
           RETURNING id, creator, title, participants, locked, locked_by, locked_at,
                     epoch, closed_at, created_at"#,
        board.id,
        title
    )
    .fetch_optional(db)
    .await?;
    one(updated)
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
    let updated = sqlx::query_as!(
        Board,
        r#"UPDATE board SET locked = $2, locked_by = $3, locked_at = $4 WHERE id = $1
           RETURNING id, creator, title, participants, locked, locked_by, locked_at,
                     epoch, closed_at, created_at"#,
        board.id,
        locked,
        locked.then(|| by.clone()),
        locked.then(|| now)
    )
    .fetch_optional(db)
    .await?;
    one(updated)
}

/// Union a bulk invite's resolved ids into the roster — the atomic-array
/// replacement for the old process-wide roster lock. The merge and the
/// participant cap are one guarded statement: `SET` and `WHERE` both read
/// the row version this `UPDATE` is acting on, so of two concurrent invites
/// each sees the other's committed roster and the cap refuses the second —
/// neither can drop the other's group, which is what the lock existed to
/// prevent.
///
/// `None` means the guard refused (or the board is gone): the roster was
/// left exactly as it was, and the caller re-reads to pick the message.
pub(crate) async fn invite_group(
    db: &Database,
    board: &BoardId,
    invited: Vec<UserId>,
) -> Result<Option<Board>, AppError> {
    let updated = sqlx::query_as!(
        Board,
        r#"UPDATE board
           SET participants = (SELECT coalesce(array_agg(DISTINCT x), '{}')
                               FROM unnest(participants || $2::uuid[]) AS x)
           WHERE id = $1
             AND cardinality((SELECT coalesce(array_agg(DISTINCT x), '{}')
                              FROM unnest(participants || $2::uuid[]) AS x)) <= $3
           RETURNING id, creator, title, participants, locked, locked_by, locked_at,
                     epoch, closed_at, created_at"#,
        board.clone(),
        &invited,
        MAX_BOARD_PARTICIPANTS as i64
    )
    .fetch_optional(db)
    .await?;
    Ok(updated)
}

// The demotion sweep — stripping a user off every roster they are listed
// on, deleting nothing, and handing the affected rooms back so the caller
// can prompt them — lives in [`crate::service::user::set_role`], where
// it commits with the role write that invalidates the membership.

/// Retire the board: permanently read-only, history still readable.
/// Idempotent by the `WHERE` — a second call matches nothing and the first
/// stamp stands, which is what the stroke path's open-guard reads.
pub async fn close(db: &Database, board: &Board) -> Result<Board, AppError> {
    let closed = sqlx::query_as!(
        Board,
        r#"UPDATE board SET closed_at = $2 WHERE id = $1 AND closed_at IS NULL
           RETURNING id, creator, title, participants, locked, locked_by, locked_at,
                     epoch, closed_at, created_at"#,
        board.id,
        Timestamp::now()
    )
    .fetch_optional(db)
    .await?;
    match closed {
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
    tx_with_retry(db, true, async |conn| {
        sqlx::query!("DELETE FROM board_stroke WHERE board = $1", board.id)
            .execute(&mut *conn)
            .await?;
        let gone = sqlx::query_as!(
            Board,
            r#"DELETE FROM board WHERE id = $1
               RETURNING id, creator, title, participants, locked, locked_by, locked_at,
                         epoch, closed_at, created_at"#,
            board.id
        )
        .fetch_optional(&mut *conn)
        .await?;
        let board = gone.ok_or(AppError::NotFound)?;
        // Exactly one row was deleted above, so the release is one seat —
        // the counter floor keeps a stray double-release from ratcheting
        // the limit shut.
        sqlx::query!(
            "UPDATE app_user SET board_count = GREATEST(board_count - 1, 0) WHERE id = $1",
            board.creator
        )
        .execute(&mut *conn)
        .await?;
        Ok(board)
    })
    .await
}

fn one(board: Option<Board>) -> Result<Board, AppError> {
    board.ok_or(AppError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::board::BoardTitle;

    /// A roster write lands, and a write against a board that vanished in
    /// the meantime is a 404 — not a silent success.
    #[tokio::test]
    async fn field_scoped_writes_return_the_row_and_a_gone_board_refuses() {
        let db = crate::database::init_mem().await.unwrap();
        let creator = UserId::generate();
        let board = create(
            &db,
            &creator,
            BoardTitle::try_new("Geometri").unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();

        let retitled = set_title(&db, &board, BoardTitle::try_new("Cebir").unwrap())
            .await
            .unwrap();
        assert_eq!(retitled.get_title().as_str(), "Cebir");

        let locked = set_locked(&db, &board, true, &creator).await.unwrap();
        assert!(locked.is_locked());

        let roster = vec![UserId::generate()];
        let re_rostered = set_participants(&db, &board, roster).await.unwrap();
        assert_eq!(re_rostered.get_participants().len(), 1);

        delete(&db, re_rostered).await.unwrap();
        assert!(read(&db, board.get_id()).await.unwrap().is_none());
    }

    /// The seat and the row commit together: a create over the cap writes no
    /// board row, and deleting a board hands the seat back.
    #[tokio::test]
    async fn the_cap_refuses_and_delete_releases_the_seat() {
        let db = crate::database::init_mem().await.unwrap();
        let creator = UserId::generate();
        for index in 0..MAX_BOARDS_PER_CREATOR {
            create(
                &db,
                &creator,
                BoardTitle::try_new(&format!("b{index}")).unwrap(),
                Vec::new(),
            )
            .await
            .unwrap();
        }
        let over = create(
            &db,
            &creator,
            BoardTitle::try_new("over").unwrap(),
            Vec::new(),
        )
        .await;
        assert!(matches!(over, Err(AppError::Conflict(_))));

        let last = list_for_user(&db, &creator, None, None, 0)
            .await
            .unwrap()
            .0
            .into_iter()
            .next()
            .unwrap();
        delete(&db, last).await.unwrap();
        // The freed seat admits one more board.
        let again = create(
            &db,
            &creator,
            BoardTitle::try_new("again").unwrap(),
            Vec::new(),
        )
        .await;
        assert!(again.is_ok());
    }
}
