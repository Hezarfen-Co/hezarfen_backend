//! The `board` table: whiteboard rooms and their rosters. Every write here is
//! field-scoped `UPDATE … SET` — a stroke landing concurrently is moving the
//! two counters this table's struct does not carry.

use crate::constant::{MAX_BOARD_PARTICIPANTS, MAX_BOARDS_PER_CREATOR};
use crate::database::{Database, tx_with_retry, unique_violation};
use crate::db::page::PagedList;
use crate::domain::board::{Board, BoardId, BoardTitle, checked_participants};
use crate::domain::timestamp::Timestamp;
use crate::domain::text_fold::{search_fold, search_fold_sql};
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
        creator: *creator,
        title,
        participants: checked_participants(participants)?,
        locked: false,
        locked_by: None,
        locked_at: None,
        epoch: 0,
        closed_at: None,
        created_at: Timestamp::now(),
    };
    // The seat claim, the row and the roster are one statement (the
    // `cap::claim_and_create` CTE, spelled out at its call site): a refused
    // insert takes its own seat bump back, so the counter can never count a
    // row that did not commit, and the roster can never trail the row. The
    // `participants` array is aggregated from the roster CTE's RETURNING —
    // the statement cannot see its own writes to `board_participant`.
    let saved = sqlx::query_as!(
        Board,
        r#"WITH seat AS (
               UPDATE app_user SET board_count = board_count + 1
               WHERE id = $1 AND board_count < $2
               RETURNING 1),
           new_board AS (
               INSERT INTO board (id, creator, title, locked, locked_by, locked_at,
                                  epoch, closed_at, created_at)
               SELECT $3, $1, $4, false, NULL, NULL, 0, NULL, $6
               WHERE EXISTS (SELECT 1 FROM seat)
               RETURNING id, creator, title, locked, locked_by, locked_at,
                         epoch, closed_at, created_at),
           roster AS (
               INSERT INTO board_participant (board, participant)
               SELECT $3, t.x FROM unnest($5::uuid[]) AS t(x)
               WHERE EXISTS (SELECT 1 FROM new_board)
               RETURNING participant)
           SELECT nb.id AS "id: BoardId", nb.creator AS "creator: UserId",
                  nb.title AS "title: BoardTitle",
                  COALESCE((SELECT array_agg(r.participant ORDER BY r.participant)
                            FROM roster r), '{}')
                      AS "participants!: Vec<UserId>",
                  nb.locked, nb.locked_by AS "locked_by: UserId",
                  nb.locked_at AS "locked_at: Timestamp", nb.epoch,
                  nb.closed_at AS "closed_at: Timestamp",
                  nb.created_at AS "created_at: Timestamp"
           FROM new_board nb"#,
        board.creator.uuid(),
        MAX_BOARDS_PER_CREATOR,
        board.id.uuid(),
        board.title.as_str(),
        &board
            .participants
            .iter()
            .map(UserId::uuid)
            .collect::<Vec<uuid::Uuid>>(),
        board.created_at.as_millis()
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

/// The one board read: row columns plus the roster, re-assembled from
/// `board_participant` (the array column is gone). The junction keeps no
/// insertion order, so every reader sorts by participant and the wire list
/// is deterministic.
async fn read_row<'e, E>(db: E, id: &BoardId) -> Result<Option<Board>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let board = sqlx::query_as!(
        Board,
        r#"SELECT b.id AS "id: BoardId", b.creator AS "creator: UserId",
               b.title AS "title: BoardTitle",
               ARRAY(SELECT p.participant FROM board_participant p
                     WHERE p.board = b.id ORDER BY p.participant)
                   AS "participants!: Vec<UserId>",
               b.locked, b.locked_by AS "locked_by: UserId",
               b.locked_at AS "locked_at: Timestamp", b.epoch,
               b.closed_at AS "closed_at: Timestamp",
               b.created_at AS "created_at: Timestamp"
           FROM board b WHERE b.id = $1"#,
        id.uuid()
    )
    .fetch_optional(db)
    .await?;
    Ok(board)
}

pub async fn read(db: &Database, id: &BoardId) -> Result<Option<Board>, AppError> {
    read_row(db, id).await
}

/// Every board `user` may open: the ones they created and the ones they
/// were invited to. Newest first — the ids are monotonic, so `id` is the
/// creation order.
///
/// `open` narrows by `closed_at`, and it means exactly that flag: a board
/// that is locked, or full at its lifetime cap but never drawn on again, is
/// never stamped and so reads as **open** — because it is. Only a creator's
/// `/close` and the lifetime cap's own refusal ever stamp one.
///
/// `q` is an optional free-text needle over the title, folded case- and
/// diacritic-insensitively on both sides ([`search_fold`] /
/// [`search_fold_sql`]); a blank needle searches nothing.
pub async fn list_for_user(
    db: &Database,
    user: &UserId,
    open: Option<bool>,
    q: Option<&str>,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Board>, i64), AppError> {
    // A blank needle searches nothing, exactly like an absent one.
    let needle = q.map(|q| search_fold(q.trim())).filter(|q| !q.is_empty());
    let open_clause = match open {
        Some(true) => " AND b.closed_at IS NULL",
        Some(false) => " AND b.closed_at IS NOT NULL",
        None => "",
    };
    // The needle rides the same fold on both sides (`position`, not `LIKE`,
    // keeps `%` and `_` literal); `$1` and `$2` are always spent on the
    // membership predicate, so the needle is `$3`.
    let q_clause = match needle {
        Some(_) => format!(" AND position($3 in {}) > 0", search_fold_sql("b.title")),
        None => String::new(),
    };
    // `PagedList` wraps `from_where` in `SELECT * FROM (…)`, so the window
    // is a whole derived table: the row columns plus the roster
    // re-assembled from `board_participant`, and the membership half of the
    // predicate as an `EXISTS` over it (the participant-side index covers
    // it, as the GIN index once did).
    let mut builder = PagedList::new(
        format!(
            "(SELECT b.id, b.creator, b.title,
                    ARRAY(SELECT p.participant FROM board_participant p
                          WHERE p.board = b.id ORDER BY p.participant) AS participants,
                    b.locked, b.locked_by, b.locked_at, b.epoch, b.closed_at, b.created_at
             FROM board b
             WHERE (b.creator = $1
                    OR EXISTS (SELECT 1 FROM board_participant bp
                               WHERE bp.board = b.id AND bp.participant = $2)){open_clause}{q_clause})"
        ),
        "ORDER BY id DESC",
    )
    .bind(user.uuid())
    .bind(user.uuid());
    if let Some(needle) = needle {
        builder = builder.bind(needle);
    }
    builder.run::<Board>(limit, offset, db).await
}

/// Re-invite: the creator-driven roster replace. The write is the
/// junction's — rows go in one transaction with the read-back, because the
/// old single `UPDATE` both wrote the array and returned the row; a read
/// issued apart from the write could answer a roster someone else had
/// replaced in between.
pub async fn set_participants(
    db: &Database,
    board: &Board,
    participants: Vec<UserId>,
) -> Result<Board, AppError> {
    let participants = checked_participants(participants)?;
    let ids: Vec<uuid::Uuid> = participants.iter().map(UserId::uuid).collect();
    let id = board.id.clone();
    tx_with_retry(db, true, async move |tx| {
        sqlx::query!("DELETE FROM board_participant WHERE board = $1", id.uuid())
            .execute(&mut *tx)
            .await?;
        sqlx::query!(
            "INSERT INTO board_participant (board, participant)
             SELECT $1, t.x FROM unnest($2::uuid[]) AS t(x)
             ON CONFLICT DO NOTHING",
            id.uuid(),
            &ids,
        )
        .execute(&mut *tx)
        .await?;
        // The read-back is under the write path's own transaction: None here
        // means the board vanished between the caller's read and this tx,
        // which is the same 404 the old `UPDATE … RETURNING` produced.
        read_row(&mut *tx, &id).await?.ok_or(AppError::NotFound)
    })
    .await
}

pub async fn set_title(db: &Database, board: &Board, title: BoardTitle) -> Result<Board, AppError> {
    let updated = sqlx::query_as!(
        Board,
        r#"UPDATE board SET title = $2 WHERE id = $1
           RETURNING id AS "id: BoardId", creator AS "creator: UserId",
               title AS "title: BoardTitle",
               ARRAY(SELECT p.participant FROM board_participant p
                     WHERE p.board = board.id ORDER BY p.participant)
                   AS "participants!: Vec<UserId>", locked,
               locked_by AS "locked_by: UserId",
               locked_at AS "locked_at: Timestamp", epoch,
               closed_at AS "closed_at: Timestamp",
               created_at AS "created_at: Timestamp""#,
        board.id.uuid(),
        title.as_str()
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
           RETURNING id AS "id: BoardId", creator AS "creator: UserId",
               title AS "title: BoardTitle",
               ARRAY(SELECT p.participant FROM board_participant p
                     WHERE p.board = board.id ORDER BY p.participant)
                   AS "participants!: Vec<UserId>", locked,
               locked_by AS "locked_by: UserId",
               locked_at AS "locked_at: Timestamp", epoch,
               closed_at AS "closed_at: Timestamp",
               created_at AS "created_at: Timestamp""#,
        board.id.uuid(),
        locked,
        locked.then(|| by.uuid()),
        locked.then(|| now.as_millis())
    )
    .fetch_optional(db)
    .await?;
    one(updated)
}

/// Union a bulk invite's resolved ids into the roster — the atomic-union
/// replacement for the old process-wide roster lock. The merge is `INSERT
/// … ON CONFLICT DO NOTHING`, and the board row is locked (`FOR NO KEY
/// UPDATE`) before the cap counts: that lock is what the old guarded array
/// `UPDATE` got from its own row write, and it keeps the same property —
/// of two concurrent invites the second waits, counts against the first's
/// committed roster, and the cap refuses it — neither can drop the other's
/// group.
///
/// `None` means the guard refused (or the board is gone): the roster was
/// left exactly as it was, and the caller re-reads to pick the message.
pub(crate) async fn invite_group(
    db: &Database,
    board: &BoardId,
    invited: Vec<UserId>,
) -> Result<Option<Board>, AppError> {
    let invited: Vec<uuid::Uuid> = invited.iter().map(UserId::uuid).collect();
    let board = board.clone();
    tx_with_retry(db, true, async move |tx| {
        // The serialization point; a vanished board refuses here, exactly
        // like the old conditional `UPDATE` matching nothing.
        let live = sqlx::query!(
            "SELECT id FROM board WHERE id = $1 FOR NO KEY UPDATE",
            board.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        if live.is_none() {
            return Ok(None);
        }
        let would_be: i64 = sqlx::query_scalar!(
            r#"SELECT ((SELECT count(*) FROM board_participant WHERE board = $1)
                    + (SELECT count(*) FROM unnest($2::uuid[]) AS t(x)
                       WHERE NOT EXISTS (SELECT 1 FROM board_participant bp
                                         WHERE bp.board = $1 AND bp.participant = t.x)))
                AS "filled!""#,
            board.uuid(),
            &invited,
        )
        .fetch_one(&mut *tx)
        .await?;
        if would_be > MAX_BOARD_PARTICIPANTS as i64 {
            return Ok(None);
        }
        sqlx::query!(
            "INSERT INTO board_participant (board, participant)
             SELECT $1, t.x FROM unnest($2::uuid[]) AS t(x)
             ON CONFLICT DO NOTHING",
            board.uuid(),
            &invited,
        )
        .execute(&mut *tx)
        .await?;
        read_row(&mut *tx, &board).await
    })
    .await
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
           RETURNING id AS "id: BoardId", creator AS "creator: UserId",
               title AS "title: BoardTitle",
               ARRAY(SELECT p.participant FROM board_participant p
                     WHERE p.board = board.id ORDER BY p.participant)
                   AS "participants!: Vec<UserId>", locked,
               locked_by AS "locked_by: UserId",
               locked_at AS "locked_at: Timestamp", epoch,
               closed_at AS "closed_at: Timestamp",
               created_at AS "created_at: Timestamp""#,
        board.id.uuid(),
        Timestamp::now().as_millis()
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
/// The roster and the stroke cascade ride in that same transaction: the FK
/// is NO ACTION, so the junction rows go before the board row they name,
/// and the stroke delete is the one and only legitimate delete of
/// `board_stroke` rows (a clear deletes nothing) — issued separately it
/// could leave a board's whole history orphaned under a record that no
/// longer exists.
pub async fn delete(db: &Database, board: Board) -> Result<Board, AppError> {
    tx_with_retry(db, true, async move |conn| {
        sqlx::query!(
            "DELETE FROM board_participant WHERE board = $1",
            board.id.uuid()
        )
        .execute(&mut *conn)
        .await?;
        sqlx::query!("DELETE FROM board_stroke WHERE board = $1", board.id.uuid())
            .execute(&mut *conn)
            .await?;
        let gone = sqlx::query_as!(
            Board,
            r#"DELETE FROM board WHERE id = $1
               RETURNING id AS "id: BoardId", creator AS "creator: UserId",
               title AS "title: BoardTitle",
               ARRAY(SELECT p.participant FROM board_participant p
                     WHERE p.board = board.id ORDER BY p.participant)
                   AS "participants!: Vec<UserId>", locked,
               locked_by AS "locked_by: UserId",
               locked_at AS "locked_at: Timestamp", epoch,
               closed_at AS "closed_at: Timestamp",
               created_at AS "created_at: Timestamp""#,
            board.id.uuid()
        )
        .fetch_optional(&mut *conn)
        .await?;
        let board = gone.ok_or(AppError::NotFound)?;
        // Exactly one row was deleted above, so the release is one seat —
        // the counter floor keeps a stray double-release from ratcheting
        // the limit shut.
        sqlx::query!(
            "UPDATE app_user SET board_count = GREATEST(board_count - 1, 0) WHERE id = $1",
            board.creator.uuid()
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
        let (db, _leases) = crate::database::init_test_db().await;
        let creator = crate::db::class_member::tests::fixture_user(&db, "board-creator").await;
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
        let member = crate::db::class_member::tests::fixture_user(&db, "board-member").await;
        let roster = vec![member];
        let re_rostered = set_participants(&db, &board, roster).await.unwrap();
        assert_eq!(re_rostered.get_participants().len(), 1);

        delete(&db, re_rostered).await.unwrap();
        assert!(read(&db, board.get_id()).await.unwrap().is_none());
    }

    /// The seat and the row commit together: a create over the cap writes no
    /// board row, and deleting a board hands the seat back.
    #[tokio::test]
    async fn the_cap_refuses_and_delete_releases_the_seat() {
        let (db, _leases) = crate::database::init_test_db().await;
        let creator = crate::db::class_member::tests::fixture_user(&db, "board-creator").await;
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

        let last = list_for_user(&db, &creator, None, None, None, 0)
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

    /// `q` narrows by title through the search fold (`geometri` finds
    /// `Geometri`), composes with `open`, and pages after the filter
    /// (`total` counts matches, not rows) — while the membership predicate
    /// still scopes every row: a non-participant searching a real title
    /// sees nothing.
    #[tokio::test]
    async fn q_searches_titles_and_composes_with_open() {
        let (db, _leases) = crate::database::init_test_db().await;
        let creator = crate::db::class_member::tests::fixture_user(&db, "q-board-kurucu").await;
        let outsider = crate::db::class_member::tests::fixture_user(&db, "q-board-disari").await;

        let open_hit = create(
            &db,
            &creator,
            BoardTitle::try_new("Geometri Kampı").unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
        create(
            &db,
            &creator,
            BoardTitle::try_new("Cebir").unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
        let closed_hit = create(
            &db,
            &creator,
            BoardTitle::try_new("Geometri Kapanış").unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
        close(&db, &closed_hit).await.unwrap();

        // Absent and blank needles both see every board.
        let (_, total) = list_for_user(&db, &creator, None, None, None, 0)
            .await
            .unwrap();
        assert_eq!(total, 3);
        let (_, total) = list_for_user(&db, &creator, None, Some("   "), None, 0)
            .await
            .unwrap();
        assert_eq!(total, 3);

        // Title hits through the fold, in both directions.
        for needle in ["geometri", "GEOMETRİ"] {
            let (hits, total) =
                list_for_user(&db, &creator, None, Some(needle), None, 0)
                    .await
                    .unwrap();
            assert_eq!(total, 2);
            let keys: Vec<String> = hits
                .iter()
                .map(|board| board.get_id().key().to_string())
                .collect();
            assert!(keys.contains(&open_hit.get_id().key().to_string()));
            assert!(keys.contains(&closed_hit.get_id().key().to_string()));
        }

        // Composes with `open`.
        let (hits, total) =
            list_for_user(&db, &creator, Some(true), Some("geometri"), None, 0)
                .await
                .unwrap();
        assert_eq!(total, 1);
        assert_eq!(hits[0].get_id().key().to_string(), open_hit.get_id().key().to_string());
        let (_, total) = list_for_user(&db, &creator, Some(false), Some("geometri"), None, 0)
            .await
            .unwrap();
        assert_eq!(total, 1);

        // The window runs after the filter: `total` counts matches.
        let (hits, total) = list_for_user(&db, &creator, None, Some("geometri"), Some(1), 0)
            .await
            .unwrap();
        assert_eq!(total, 2);
        assert_eq!(hits.len(), 1);

        // A needle matching nothing: an empty page, not an error.
        let (hits, total) = list_for_user(&db, &creator, None, Some("yok boyle bir tahta"), None, 0)
            .await
            .unwrap();
        assert_eq!(total, 0);
        assert!(hits.is_empty());

        // The membership predicate still scopes the search: a non-participant
        // searching a title that exists finds nothing.
        let (hits, total) =
            list_for_user(&db, &outsider, None, Some("geometri"), None, 0)
                .await
                .unwrap();
        assert_eq!(total, 0);
        assert!(hits.is_empty());
    }
}
