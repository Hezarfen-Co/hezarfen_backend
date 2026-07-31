//! A collaborative whiteboard room. Membership is an ad-hoc invite list the
//! creator names — no course, session or appointment is involved — and the
//! whole permission model is the two predicates [`Board::is_participant`]
//! (draw) and [`Board::is_creator`] (clear / lock / close / delete). Every
//! route and every socket frame gates on exactly those two.
//!
//! The marks themselves live in `board_stroke`, appended and never deleted: a
//! clear bumps `epoch` so the canvas empties while the history stays
//! replayable. The two stroke counters (`epoch_stroke_count`,
//! `total_stroke_count`) are `option<int>` columns this struct deliberately
//! does NOT carry — they are written only by the stroke path's conditional
//! claim, so every mutation here is field-scoped `UPDATE … SET`. A whole-row
//! `CONTENT` save would silently wipe both (src/constant.rs:717-722).

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{
    BOARD_TABLE, MAX_BOARD_PARTICIPANTS, MAX_BOARD_TITLE_LEN, MAX_BOARDS_PER_CREATOR,
    USER_BOARD_COUNT_FIELD,
};
use crate::database::Database;
use crate::domain::cap;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::page::PagedList;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_required;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct BoardId(RecordId);

impl BoardId {
    /// Monotonic, not `Ulid::new()`: boards list newest-first by id, and a
    /// random low half scrambles rows minted in the same millisecond.
    pub fn generate() -> Self {
        Self(RecordId::new(BOARD_TABLE, next_ulid().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(BOARD_TABLE, key))
    }

    pub fn record(&self) -> RecordId {
        self.0.clone()
    }

    pub fn key(&self) -> &str {
        match &self.0.key {
            RecordIdKey::String(key) => key,
            _ => "",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct BoardTitle(String);

impl BoardTitle {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("title", value, MAX_BOARD_TITLE_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The invite list, deduplicated and bounded. The creator is a participant by
/// construction ([`Board::is_participant`]), so their presence in the list is
/// neither required nor rejected.
fn checked_participants(participants: Vec<UserId>) -> Result<Vec<UserId>, ValidationError> {
    let mut unique: Vec<UserId> = Vec::with_capacity(participants.len());
    for user in participants {
        if !unique.contains(&user) {
            unique.push(user);
        }
    }
    if unique.len() > MAX_BOARD_PARTICIPANTS {
        return Err(ValidationError::TooLong {
            field: "participants",
            max: MAX_BOARD_PARTICIPANTS,
            got: unique.len(),
        });
    }
    Ok(unique)
}

#[derive(Debug, Clone, SurrealValue)]
pub struct Board {
    id: BoardId,
    creator: UserId,
    title: BoardTitle,
    participants: Vec<UserId>,
    locked: bool,
    locked_by: Option<UserId>,
    locked_at: Option<Timestamp>,
    epoch: i64,
    closed_at: Option<Timestamp>,
    created_at: Timestamp,
}

impl Board {
    pub fn get_id(&self) -> &BoardId {
        &self.id
    }

    pub fn get_creator(&self) -> &UserId {
        &self.creator
    }

    pub fn get_title(&self) -> &BoardTitle {
        &self.title
    }

    pub fn get_participants(&self) -> &[UserId] {
        &self.participants
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    pub fn get_locked_by(&self) -> Option<&UserId> {
        self.locked_by.as_ref()
    }

    pub fn get_locked_at(&self) -> Option<Timestamp> {
        self.locked_at
    }

    pub fn get_epoch(&self) -> i64 {
        self.epoch
    }

    pub fn get_closed_at(&self) -> Option<Timestamp> {
        self.closed_at
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }

    /// May draw. The creator is always in, without being named on the list.
    pub fn is_participant(&self, user: &UserId) -> bool {
        &self.creator == user || self.participants.contains(user)
    }

    /// May clear, lock, close and delete. Nothing else is creator-only.
    pub fn is_creator(&self, user: &UserId) -> bool {
        &self.creator == user
    }

    /// Open a board, taking a slot on the creator's `board_count` in the same
    /// transaction as the row — the counter is the authority on how many
    /// boards exist, so it can never count a row that did not commit.
    pub async fn create(
        creator: &UserId,
        title: BoardTitle,
        participants: Vec<UserId>,
        db: &Database,
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

    pub async fn read(id: &BoardId, db: &Database) -> Result<Option<Board>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// Every board `user` may open: the ones they created and the ones they
    /// were invited to. Newest first — the ids are monotonic, so `id` is the
    /// creation order.
    pub async fn list_for_user(
        user: &UserId,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<Board>, i64), AppError> {
        PagedList::new(
            "board WHERE creator = $usr OR participants CONTAINS $usr",
            "ORDER BY id DESC",
        )
        .bind("usr", user.record())
        .run(limit, offset, db)
        .await
    }

    /// Re-invite. Field-scoped, like every write here: a stroke landing
    /// concurrently is moving the counters this struct does not carry.
    pub async fn set_participants(
        &self,
        participants: Vec<UserId>,
        db: &Database,
    ) -> Result<Board, AppError> {
        let participants = checked_participants(participants)?;
        let mut result = db
            .query("UPDATE $id SET participants = $who RETURN AFTER")
            .bind(("id", self.id.record()))
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

    pub async fn set_title(&self, title: BoardTitle, db: &Database) -> Result<Board, AppError> {
        let mut result = db
            .query("UPDATE $id SET title = $title RETURN AFTER")
            .bind(("id", self.id.record()))
            .bind(("title", title.0))
            .await?
            .check()?;
        one(result.take::<Vec<Board>>(0)?)
    }

    /// Freeze or thaw drawing. `locked_by`/`locked_at` are cleared on unlock so
    /// the row never claims a lock that is not held.
    pub async fn set_locked(
        &self,
        locked: bool,
        by: &UserId,
        db: &Database,
    ) -> Result<Board, AppError> {
        let now = Timestamp::now();
        let mut result = db
            .query("UPDATE $id SET locked = $locked, locked_by = $by, locked_at = $at RETURN AFTER")
            .bind(("id", self.id.record()))
            .bind(("locked", locked))
            .bind(("by", locked.then(|| by.record())))
            .bind(("at", locked.then(|| now.as_millis())))
            .await?
            .check()?;
        one(result.take::<Vec<Board>>(0)?)
    }

    /// Retire the board: permanently read-only, history still readable.
    /// Idempotent by the `WHERE` — a second call matches nothing and the first
    /// stamp stands, which is what the stroke path's open-guard reads.
    pub async fn close(&self, db: &Database) -> Result<Board, AppError> {
        let mut result = db
            .query("UPDATE $id SET closed_at = $now WHERE closed_at = NONE RETURN AFTER")
            .bind(("id", self.id.record()))
            .bind(("now", Timestamp::now().as_millis()))
            .await?
            .check()?;
        match result.take::<Vec<Board>>(0)?.into_iter().next() {
            Some(board) => Ok(board),
            // Already closed: the row is unchanged, so read it back rather
            // than report a 404 for a board that plainly exists.
            None => Self::read(&self.id, db).await?.ok_or(AppError::NotFound),
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
    pub async fn delete(self, db: &Database) -> Result<Board, AppError> {
        let mut result = db
            .query(
                "BEGIN TRANSACTION;
                 DELETE board_stroke WHERE board = $id;
                 LET $gone = (DELETE $id RETURN BEFORE);
                 UPDATE $usr SET board_count = math::max([(board_count ?? 0) - array::len($gone), 0]);
                 RETURN $gone;
                 COMMIT TRANSACTION;",
            )
            .bind(("id", self.id.record()))
            .bind(("usr", self.creator.record()))
            .await?
            .check()?;
        // Slots count BEGIN, the cascade, the LET and the UPDATE: the RETURN
        // is slot 4.
        one(result.take::<Vec<Board>>(4)?)
    }
}

fn one(rows: Vec<Board>) -> Result<Board, AppError> {
    rows.into_iter().next().ok_or(AppError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    /// (src/domain/cap.rs:44-49).
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
        Board::create(
            &user("c"),
            BoardTitle::try_new("Geometri").unwrap(),
            vec![user("p")],
            db,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn the_two_predicates_are_the_permission_model() {
        let db = a_db().await;
        let board = a_board(&db).await;
        // Creator: draws AND commands.
        assert!(board.is_participant(&user("c")));
        assert!(board.is_creator(&user("c")));
        // Invited: draws only.
        assert!(board.is_participant(&user("p")));
        assert!(!board.is_creator(&user("p")));
        // Stranger: neither.
        assert!(!board.is_participant(&user("s")));
        assert!(!board.is_creator(&user("s")));
    }

    #[test]
    fn title_is_required_and_bounded() {
        assert!(BoardTitle::try_new("   ").is_err());
        assert!(BoardTitle::try_new(&"x".repeat(MAX_BOARD_TITLE_LEN)).is_ok());
        assert!(BoardTitle::try_new(&"x".repeat(MAX_BOARD_TITLE_LEN + 1)).is_err());
    }

    #[tokio::test]
    async fn participants_are_deduplicated_and_bounded() {
        assert_eq!(
            checked_participants(vec![user("p"), user("p")])
                .unwrap()
                .len(),
            1
        );
        let many = (0..MAX_BOARD_PARTICIPANTS + 1)
            .map(|n| UserId::from_key(&format!("u{n}")))
            .collect();
        assert!(checked_participants(many).is_err());
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
        let refused = Board::create(
            &user("c"),
            BoardTitle::try_new("Geometri").unwrap(),
            vec![],
            &db,
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
        board.delete(&db).await.unwrap();
        assert_eq!(stored_count(&db).await, 0);
    }

    /// Closing twice must not re-stamp: the first `closed_at` is the record.
    #[tokio::test]
    async fn close_is_idempotent() {
        let db = a_db().await;
        let board = a_board(&db).await;
        let closed = board.close(&db).await.unwrap();
        let first = closed.get_closed_at().unwrap();
        // A whole millisecond apart, so a re-stamp could not go unnoticed.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        assert_eq!(board.close(&db).await.unwrap().get_closed_at(), Some(first));
        assert_eq!(
            Board::read(board.get_id(), &db)
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
        board
            .set_title(BoardTitle::try_new("Cebir").unwrap(), &db)
            .await
            .unwrap();
        board.set_participants(vec![user("s")], &db).await.unwrap();
        board.set_locked(true, &user("c"), &db).await.unwrap();
        board.close(&db).await.unwrap();
        let mut result = db
            .query("SELECT VALUE [epoch_stroke_count, total_stroke_count] FROM $id")
            .bind(("id", board.get_id().record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        assert_eq!(result.take::<Vec<Vec<i64>>>(0).unwrap()[0], vec![7, 9]);
    }
}
