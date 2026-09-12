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

use crate::constant::{BOARD_TABLE, MAX_BOARD_PARTICIPANTS, MAX_BOARD_TITLE_LEN};
use crate::domain::monotonic_id::next_ulid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::validate_required;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct BoardId(RecordId);

impl BoardId {
    /// Monotonic, not `Ulid::generate()`: boards list newest-first by id, and a
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
pub struct BoardTitle(pub(crate) String);

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
pub(crate) fn checked_participants(
    participants: Vec<UserId>,
) -> Result<Vec<UserId>, ValidationError> {
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
    pub(crate) id: BoardId,
    pub(crate) creator: UserId,
    pub(crate) title: BoardTitle,
    pub(crate) participants: Vec<UserId>,
    pub(crate) locked: bool,
    pub(crate) locked_by: Option<UserId>,
    pub(crate) locked_at: Option<Timestamp>,
    pub(crate) epoch: i64,
    pub(crate) closed_at: Option<Timestamp>,
    pub(crate) created_at: Timestamp,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::MAX_BOARD_TITLE_LEN;

    fn user(key: &str) -> UserId {
        UserId::from_key(key)
    }

    /// A board row assembled in memory — the predicates under test are pure,
    /// so no store is involved.
    fn a_board() -> Board {
        Board {
            id: BoardId::from_key("b"),
            creator: user("c"),
            title: BoardTitle::try_new("Geometri").unwrap(),
            participants: vec![user("p")],
            locked: false,
            locked_by: None,
            locked_at: None,
            epoch: 0,
            closed_at: None,
            created_at: Timestamp::now(),
        }
    }

    #[test]
    fn the_two_predicates_are_the_permission_model() {
        let board = a_board();
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

    #[test]
    fn participants_are_deduplicated_and_bounded() {
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
}
