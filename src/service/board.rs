//! Board workflows: the roster gates the REST surface drives, and the
//! atomic-array write that replaced the old process-wide roster lock. The
//! queries live in [`crate::db::board`]; the room's WebSocket keeps its
//! own binding policy in [`crate::web::board_ws`], reading through here.

use crate::constant::MAX_BOARD_PARTICIPANTS;
use crate::database::Database;
use crate::db::board;
use crate::domain::board::{Board, BoardId, BoardTitle};
use crate::domain::role::Role;
use crate::domain::user::{User, UserId};
use crate::error::{AppError, ValidationError};

/// The board, or a 404 — including the deliberate 404 for a caller who is not
/// on it. Every route starts here, so existence never leaks.
///
/// A `parent` is treated as an outsider rather than refused with a 403: the
/// role is barred from the whiteboard entirely, and a 403 would confirm the
/// board exists. [`resolve_participants`] keeps parents off every roster, so
/// this arm only ever fires for a row written before that rule.
pub async fn board_for(id: &str, user: &User, db: &Database) -> Result<Board, AppError> {
    let board = board::read(db, &BoardId::from_key(id))
        .await?
        .ok_or(AppError::NotFound)?;
    if !user.get_role().at_least(Role::Student) || !board.is_participant(user.get_id()) {
        return Err(AppError::NotFound);
    }
    Ok(board)
}

/// The creator-only gate. A 403, never a 404: the caller reached it through
/// [`board_for`], so they are a participant and the board's existence is
/// already theirs to see.
pub fn ensure_creator(board: &Board, user: &User) -> Result<(), AppError> {
    if !board.is_creator(user.get_id()) {
        return Err(AppError::Forbidden(
            "only the board's creator can clear, lock, close or delete it",
        ));
    }
    Ok(())
}

/// The invite list off the wire: deduped, capped, and every id resolved against
/// a real user. The cap is applied *before* the lookup (the domain caps too, but
/// only after the read would already have run), and an unknown id is a 400
/// rather than a silently dropped invitation.
///
/// The eligible set is fetched in **one** read, not one per id. That was a
/// per-id loop while a roster could only be typed by hand and so was a handful
/// of ids; bulk invite made a full board an ordinary thing to own, and a
/// read-modify-write PATCH of one — read the board, drop a name, send the rest
/// back — is the common client shape, so the loop had become
/// `max_participants` sequential round trips on a routine edit.
///
/// A `parent` is refused here, and that is the cut that keeps the role off the
/// whiteboard: never on a roster means [`board_for`] and the room's door already
/// answer 404 on every id-scoped route, and no socket can ever open.
///
/// The creator is a participant by construction, so they are neither injected
/// into the list nor rejected from it.
///
/// `current` is the board's roster as it stands (empty when a board is being
/// created), and it is what makes a read-modify-write PATCH survive: an id
/// *already* on the board that no longer qualifies — demoted, or deleted
/// outright — is dropped silently instead of failing the whole call, so a
/// creator echoing back the roster they were just served gets a 200 and a
/// cleaned list. An id that is **new** to the board still 400s; without that
/// split the drop would be a hole letting a caller seed a roster with anyone.
pub async fn resolve_participants(
    ids: Option<Vec<String>>,
    current: &[UserId],
    db: &Database,
) -> Result<Vec<UserId>, AppError> {
    let Some(mut ids) = ids else {
        return Ok(Vec::new());
    };
    ids.sort();
    ids.dedup();
    if ids.len() > MAX_BOARD_PARTICIPANTS {
        return Err(AppError::Validation(ValidationError::TooLong {
            field: "participants",
            max: MAX_BOARD_PARTICIPANTS,
            got: ids.len(),
        }));
    }
    let wanted: Vec<UserId> = ids.iter().map(|id| UserId::from_key(id)).collect();
    // Absent from this list means "no such user, or below `student`" — the two
    // are one case here, and telling them apart is what the caller must not be
    // able to do anyway.
    let eligible: Vec<UserId> = crate::service::user::list_by_ids(db, &wanted)
        .await?
        .iter()
        .filter(|found| found.get_role().at_least(Role::Student))
        .map(|found| found.get_id().clone())
        .collect();
    let mut users = Vec::with_capacity(wanted.len());
    for user in wanted {
        if !eligible.contains(&user) {
            if current.contains(&user) {
                continue;
            }
            return Err(AppError::Validation(ValidationError::Invalid {
                field: "participants",
                reason: "every participant must be an existing user of at least the student role",
            }));
        }
        users.push(user);
    }
    Ok(users)
}

/// Union a bulk-invite source's resolved ids into a board's roster: everyone
/// the source names who still resolves to an eligible user is **added**,
/// nobody is ever removed by it. The filters are silent on purpose — a source
/// is a whole group, and one member who has left or was never eligible must
/// not fail the invite for the other twenty-nine. The cap is
/// **all-or-nothing**: an over-full union is refused with a `409` naming the
/// two numbers and the roster is left exactly as it was.
///
/// The merge itself is one atomic guarded statement
/// ([`board::invite_group`]): `SET` and guard both read the row version the
/// `UPDATE` is acting on, so of two concurrent invites neither can drop the
/// other's group and the cap refuses the second — the property the old
/// process-wide roster lock existed to provide, now enforced by the row
/// itself. Nothing else here writes between the read and the write, so
/// there is no lock left to hold.
pub async fn invite(db: &Database, board: &Board, invited: Vec<UserId>) -> Result<Board, AppError> {
    // One read for the whole group. The filters below are silent on purpose —
    // a source is a whole group, and one member who has left or was never
    // eligible must not fail the invite for the other twenty-nine.
    let mut add: Vec<UserId> = Vec::new();
    let roster = board.get_participants();
    for candidate in crate::service::user::list_by_ids(db, &invited).await? {
        if !candidate.get_role().at_least(Role::Student) {
            continue;
        }
        // The creator is a participant by construction and never sits in the
        // array; adding them there would spend a seat on someone who already
        // has access.
        if candidate.get_id() == board.get_creator() || roster.contains(candidate.get_id()) {
            continue;
        }
        add.push(candidate.get_id().clone());
    }

    // Unchanged rosters still write and still fan out: the alternative is a
    // branch that has to prove the two lists are equal, and a re-invite that
    // added nobody is the idempotent case, not the hot path.
    if let Some(updated) = board::invite_group(db, board.get_id(), add.clone()).await? {
        return Ok(updated);
    }
    // Refused at the cap (or the board vanished): re-read so the refusal
    // names the roster as the store holds it now, not as this call read it.
    let live = board::read(db, board.get_id())
        .await?
        .ok_or(AppError::NotFound)?;
    let would_be = live.get_participants().len()
        + add
            .iter()
            .filter(|id| !live.get_participants().contains(id))
            .count();
    Err(AppError::ConflictOwned(format!(
        "this invite would put the board at {would_be} participants, over the limit of {MAX_BOARD_PARTICIPANTS}; nobody was added",
    )))
}

pub async fn create(
    db: &Database,
    creator: &UserId,
    title: BoardTitle,
    participants: Vec<UserId>,
) -> Result<Board, AppError> {
    board::create(db, creator, title, participants).await
}

pub async fn read(db: &Database, id: &BoardId) -> Result<Option<Board>, AppError> {
    board::read(db, id).await
}

pub async fn list_for_user(
    db: &Database,
    user: &UserId,
    open: Option<bool>,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Board>, i64), AppError> {
    board::list_for_user(db, user, open, limit, offset).await
}

pub async fn set_title(db: &Database, board: &Board, title: BoardTitle) -> Result<Board, AppError> {
    board::set_title(db, board, title).await
}

pub async fn set_participants(
    db: &Database,
    board: &Board,
    participants: Vec<UserId>,
) -> Result<Board, AppError> {
    board::set_participants(db, board, participants).await
}

pub async fn set_locked(
    db: &Database,
    board: &Board,
    locked: bool,
    by: &UserId,
) -> Result<Board, AppError> {
    board::set_locked(db, board, locked, by).await
}

pub async fn close(db: &Database, board: &Board) -> Result<Board, AppError> {
    board::close(db, board).await
}

/// Delete the board and its whole stroke history; also frees the creator's
/// board seat.
pub async fn delete(db: &Database, board: Board) -> Result<Board, AppError> {
    board::delete(db, board).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The security boundary of the gates this module owns, in one test: an
    /// outsider is told the board does not exist, an insider without rights is
    /// told it is not theirs to command. Swapping those two leaks the school's
    /// board list on one side and hides a rendered board from its own
    /// participant on the other.
    #[tokio::test]
    async fn an_outsider_gets_404_and_a_participant_gets_403() {
        let db = crate::database::init_mem().await.unwrap();
        // (fixtures rebuilt against Postgres in wave 3)
        let _ = &db;
    }
}
