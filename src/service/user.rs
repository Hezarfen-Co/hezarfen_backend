//! User workflows: the boot-time admin seed, and the role change — the
//! admin floor ([`ADMIN_FLOOR_LOCK`]) held across the count that checks it
//! and the transaction that commits it, sweeps included. Reads, listings,
//! the row mint, and the field-scoped writers live in [`crate::db::user`].

use crate::database::Database;
use crate::db::user;
use crate::domain::board::Board;
use crate::domain::note_file::FileContentType;
use crate::domain::preferences::{Language, PaletteColor, Theme};
use crate::domain::profile::{Bio, BirthDate, DisplayName, Email, PersonName, Phone};
use crate::domain::role::Role;
use crate::domain::user::{Password, PasswordHash, User, UserId, Username};
use crate::error::AppError;

/// One role demotion at a time, school-wide.
///
/// The invariant it protects is "the school always keeps an admin", and that
/// guard is a count-then-write: read whether another admin exists, then lower
/// this row. SurrealDB conflict-checks neither side of that pair — a
/// `BEGIN…COMMIT` does not serialize a cross-record count against a concurrent
/// update (write-skew, see [`crate::db::cap`]) and a statement that only
/// *reads* the rival's row never collides with it. So two admins demoting each
/// other both counted the other and both committed, leaving **zero** admins and
/// a school nobody can administer: `ensure_admin` refuses to promote an
/// existing non-admin row on every later boot, so the only way back was hand-run
/// SurrealQL against the volume.
///
/// Serializing the pair is what closes it, the same argument
/// [`crate::service::settings::SETTINGS_LOCK`] makes one level up: the deployment
/// runs one process by contract (stop-the-world upgrades), so process-wide is
/// deployment-wide. A per-row counter (the [`crate::db::cap`] shape) does
/// not fit — the count being capped is over *every* user row, with no parent
/// record to hold it and no place to seed one without a backfill.
///
/// **Lock order:** taken alone. Nothing is locked while it is held.
static ADMIN_FLOOR_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Idempotent startup seed: guarantee an admin account with this username.
/// Missing → created directly with [`Role::Admin`]. Already an admin →
/// nothing to do (the stored password stays whatever it is — the seed never
/// rewrites credentials). Taken by a non-admin → refuse to touch it and log
/// a warning: silently promoting an account someone else registered would
/// be a privilege escalation, so that conflict is resolved out-of-band.
pub async fn ensure_admin(
    db: &Database,
    username: Username,
    password: Password,
) -> Result<(), AppError> {
    match user::find_by_username(db, username.as_str()).await? {
        Some(user) if user.role == Role::Admin => Ok(()),
        Some(_) => {
            tracing::warn!(
                "ADMIN_USERNAME names an existing non-admin account; refusing to promote it. \
                 Grant the role through an existing admin or the manual SurrealQL path."
            );
            Ok(())
        }
        None => {
            // One statement, admin from birth. Never create-then-promote:
            // an interruption between those two writes leaves a student row
            // holding ADMIN_USERNAME, which the `Some(_)` arm above then
            // refuses to promote — forever, on every subsequent boot. There
            // is no marker that could tell such a row apart from a stranger
            // who registered the name first, so the hole cannot be healed
            // later; it has to be impossible to open.
            user::create_with_role(db, username, password.hash_async().await?, Role::Admin)
                .await?;
            // No `username` field: with OTLP on every event is exported as a
            // log record, so an account name here leaves the process.
            tracing::info!("seeded the admin account named by ADMIN_USERNAME");
            Ok(())
        }
    }
}

/// Overwrite this user's role **and** shed every grant the new role may not
/// hold, in one transaction. The caller is responsible for authorizing it.
///
/// Refuses (`Conflict`) to lower the school's last admin, however the
/// request is spelled and however many demotions are in flight at once —
/// see [`ADMIN_FLOOR_LOCK`] for why that guard cannot live in the
/// transaction below.
///
/// The sweep itself — what each role change owes, arm by arm, and why the
/// conditions differ — is [`user::set_role_cascade`]'s statement list,
/// documented there.
pub async fn set_role(
    db: &Database,
    target: &UserId,
    role: Role,
) -> Result<(User, Vec<Board>), AppError> {
    // A missing account is a 404, answered before the floor is asked.
    user::read(db, target).await?.ok_or(AppError::NotFound)?;
    // The admin floor, held from the count to the commit ([`ADMIN_FLOOR_LOCK`]
    // for why a transaction cannot do this). Asked on the *live* rows rather
    // than a caller's snapshot, so a snapshot that predates a promotion cannot
    // skip the check, and cheap enough to ask on every demotion: one round
    // trip on an admin-only route.
    let _floor = if role == Role::Admin {
        None
    } else {
        let guard = ADMIN_FLOOR_LOCK.lock().await;
        if user::would_orphan_admins(db, target).await? {
            return Err(AppError::Conflict(
                "the school must keep at least one admin — promote another account first",
            ));
        }
        Some(guard)
    };
    user::set_role_cascade(db, target, role).await
}

/// Register a new account — the web layer's row mint. New users always start
/// as [`Role::Student`]; elevation is a separate, admin-only action
/// (see [`set_role`]).
pub async fn create(
    db: &Database,
    username: Username,
    password_hash: PasswordHash,
) -> Result<User, AppError> {
    user::create(db, username, password_hash).await
}

/// Mint a row holding `role` outright — the school-creation path, whose
/// first account must be an admin from birth.
pub async fn create_with_role(
    db: &Database,
    username: Username,
    password_hash: PasswordHash,
    role: Role,
) -> Result<User, AppError> {
    user::create_with_role(db, username, password_hash, role).await
}

/// One user row, for callers that only inspect it — the web layer's reads go
/// through here.
pub async fn read(db: &Database, id: &UserId) -> Result<Option<User>, AppError> {
    user::read(db, id).await
}

/// Every account, newest first — the web layer's paging read.
pub async fn list_all(
    db: &Database,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<User>, i64), AppError> {
    user::list_all(db, limit, offset).await
}

/// The users behind `ids` in one query; missing ids simply absent.
pub async fn list_by_ids(db: &Database, ids: &[UserId]) -> Result<Vec<User>, AppError> {
    user::list_by_ids(db, ids).await
}

/// Username/name/surname fragment search — the pickers' backing read.
pub async fn search(
    db: &Database,
    query: &str,
    role: Option<Role>,
    allowed_roles: Option<&[Role]>,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<User>, i64), AppError> {
    user::search(db, query, role, allowed_roles, limit, offset).await
}

/// The login lookup. Usernames are stored trimmed; callers trim the same way
/// so a padded attempt matches the canonical name.
pub async fn find_by_username(db: &Database, username: &str) -> Result<Option<User>, AppError> {
    user::find_by_username(db, username).await
}

/// Persist the personal-info/profile fields a PATCH carried.
pub async fn set_profile(
    db: &Database,
    id: &UserId,
    name: Option<Option<PersonName>>,
    surname: Option<Option<PersonName>>,
    email: Option<Option<Email>>,
    phone: Option<Option<Phone>>,
    birth_date: Option<Option<BirthDate>>,
    display_name: Option<Option<DisplayName>>,
    bio: Option<Option<Bio>>,
) -> Result<User, AppError> {
    user::set_profile(db, id, name, surname, email, phone, birth_date, display_name, bio).await
}

/// Persist an avatar upload, handing back the replaced blob's name.
pub async fn set_avatar(
    db: &Database,
    id: &UserId,
    file: &str,
    content_type: &FileContentType,
    size: i64,
) -> Result<Option<User>, AppError> {
    user::set_avatar(db, id, file, content_type, size).await
}

/// Clear the avatar, handing back the blob's name for deletion.
pub async fn clear_avatar(db: &Database, id: &UserId) -> Result<Option<User>, AppError> {
    user::clear_avatar(db, id).await
}

/// Rewrite the credential — the admin-password reset's store half.
pub async fn set_password_hash(
    db: &Database,
    id: &UserId,
    password_hash: PasswordHash,
) -> Result<Option<User>, AppError> {
    user::set_password_hash(db, id, password_hash).await
}

/// Persist the UI-preference fields a PATCH carried.
pub async fn set_preferences(
    db: &Database,
    id: &UserId,
    theme: Option<Option<Theme>>,
    language: Option<Option<Language>>,
    palette_color: Option<Option<PaletteColor>>,
) -> Result<User, AppError> {
    user::set_preferences(db, id, theme, language, palette_color).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::{Database, init_mem};
    use crate::domain::user::Password;

    async fn a_user(username: &str, db: &Database) -> User {
        user::create(
            db,
            Username::try_new(username).unwrap(),
            Password::try_new("secret1")
                .unwrap()
                .hash_async()
                .await
                .unwrap(),
        )
        .await
        .unwrap()
    }

    /// The board result slot is an *index* into a batch whose length depends on
    /// which arms the role selected, so it is the one number in the cascade a
    /// silent mistake would not show up in the store: pointed at the wrong
    /// statement it yields an empty list, and every whiteboard room whose roster
    /// just changed is simply never told. Assert the rows come back.
    #[tokio::test]
    async fn the_cascade_returns_the_boards_it_stripped() {
        use crate::domain::board::{Board, BoardTitle};

        let db = init_mem().await.unwrap();
        let creator = a_user("ogretmen", &db).await;
        let guest = a_user("ogrenci", &db).await;
        let board = Board::create(
            creator.get_id(),
            BoardTitle::try_new("Geometri").unwrap(),
            vec![guest.get_id().clone()],
            &db,
        )
        .await
        .unwrap();

        // A promotion touches no roster and must report none.
        let (guest, boards) = set_role(&db, guest.get_id(), Role::Teacher)
            .await
            .unwrap();
        assert!(
            boards.is_empty(),
            "only a demotion to parent strips rosters"
        );

        let (_, boards) = set_role(&db, guest.get_id(), Role::Parent)
            .await
            .unwrap();
        assert_eq!(boards.len(), 1, "the stripped room must be reported back");
        assert_eq!(boards[0].get_id(), board.get_id());
        assert!(
            boards[0].get_participants().is_empty(),
            "and carry the roster the room is about to be told about"
        );
    }

    /// The half the roster strip cannot reach: the board's **creator**. Demoted,
    /// they are 404'd off their own room, every command on it is creator-only
    /// for everyone else, and nothing in the crate lists a board a caller is not
    /// on — so the room could be cleared, locked, closed and deleted by nobody
    /// while its participants kept drawing on it. The demotion closes it, and
    /// closing is *all* it does: the roster, the row and every mark stay
    /// readable.
    #[tokio::test]
    async fn a_demoted_creator_s_board_is_closed_and_still_readable() {
        use crate::domain::board::{Board, BoardTitle};
        use crate::domain::board_stroke::{BOARD_CLOSED, BoardStroke};

        let db = init_mem().await.unwrap();
        let creator = a_user("ogretmen", &db).await;
        let guest = a_user("ogrenci", &db).await;
        let board = Board::create(
            creator.get_id(),
            BoardTitle::try_new("Geometri").unwrap(),
            vec![guest.get_id().clone()],
            &db,
        )
        .await
        .unwrap();
        let mark = |epoch| {
            let (id, author, db) = (board.get_id().clone(), guest.get_id().clone(), db.clone());
            async move { BoardStroke::append(&id, &author, "{\"p\":[1]}", epoch, &db).await }
        };
        mark(0).await.unwrap();

        let (creator, boards) = set_role(&db, creator.get_id(), Role::Parent)
            .await
            .unwrap();
        assert_eq!(boards.len(), 1, "the room must be reported back");
        let stamp = boards[0]
            .get_closed_at()
            .expect("and carry the closing stamp the room is told about");

        // Stored, not merely reported — and nothing else moved.
        let stored = Board::read(board.get_id(), &db).await.unwrap().unwrap();
        assert_eq!(stored.get_closed_at(), Some(stamp));
        assert_eq!(
            stored.get_participants(),
            [guest.get_id().clone()],
            "closing must not empty the roster it did not touch"
        );
        assert_eq!(
            BoardStroke::history(board.get_id(), None, false, None, 0, &db)
                .await
                .unwrap()
                .1,
            1,
            "the marks the participants drew stay on disk"
        );

        // Read-only for the participants who are still on it.
        assert!(
            matches!(mark(0).await, Err(AppError::Conflict(BOARD_CLOSED))),
            "a closed board must refuse every write with the terminal answer"
        );

        // The stamp is the record: a re-run of the sweep must not move it.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        set_role(&db, creator.get_id(), Role::Parent)
            .await
            .unwrap();
        assert_eq!(
            Board::read(board.get_id(), &db)
                .await
                .unwrap()
                .unwrap()
                .get_closed_at(),
            Some(stamp),
            "the close is a compare-and-set, not a re-stamp"
        );
    }

    /// A demotion below `teacher` must take the published calendar with it and
    /// settle the bookings on it. Nothing else can: a slot is listed only on its
    /// own teacher's calendar (and hidden from every other reader once its owner
    /// is demoted), deleted only by a teacher+, and a booking on one can be
    /// decided only by a teacher+ and cancelled only by its requester — who is
    /// refused the moment the window opens. So a slot left behind here is a row
    /// no route can reach and a seat nothing can ever free.
    #[tokio::test]
    async fn a_demotion_withdraws_the_calendar_and_settles_its_bookings() {
        use crate::domain::appointment_slot::AppointmentSlot;
        use crate::domain::appointment::{AppointmentReason, AppointmentStatus};
        use crate::service::appointment;
        use crate::domain::timestamp::Timestamp;

        let db = init_mem().await.unwrap();
        let staff = |name: &'static str, db: Database| async move {
            let teacher = a_user(name, &db).await;
            set_role(&db, teacher.get_id(), Role::Teacher)
                .await
                .unwrap()
                .0
        };
        let teacher = staff("ogretmen", db.clone()).await;
        let other = staff("digerogretmen", db.clone()).await;
        let student = a_user("ogrenci", &db).await;
        let soon = |offset| Timestamp::from_millis(Timestamp::now().as_millis() + offset);
        let reason = || AppointmentReason::try_new("görüşme").unwrap();
        let slot = |owner: &User, offset: i64, db: Database| {
            let owner = owner.get_id().clone();
            async move {
                AppointmentSlot::create(&owner, soon(offset), soon(offset + 60_000), None, &db)
                    .await
                    .unwrap()
            }
        };

        let free = slot(&teacher, 60_000, db.clone()).await;
        let asked = slot(&teacher, 180_000, db.clone()).await;
        let agreed = slot(&teacher, 300_000, db.clone()).await;
        let kept = slot(&other, 60_000, db.clone()).await;

        let pending = appointment::book(&db, asked.get_id(), student.get_id(), reason())
            .await
            .unwrap();
        let approved = appointment::book(&db, agreed.get_id(), student.get_id(), reason())
            .await
            .unwrap();
        appointment::approve(&db, approved.get_id(), teacher.get_id())
            .await
            .unwrap();
        let elsewhere = appointment::book(&db, kept.get_id(), student.get_id(), reason())
            .await
            .unwrap();

        set_role(&db, teacher.get_id(), Role::Student)
            .await
            .unwrap();

        // The calendar is gone, the booked weeks included.
        assert!(
            AppointmentSlot::list_for_teacher(teacher.get_id(), &db)
                .await
                .unwrap()
                .is_empty()
        );
        for slot in [&free, &asked, &agreed] {
            assert!(
                AppointmentSlot::read(slot.get_id(), &db)
                    .await
                    .unwrap()
                    .is_none(),
                "a slot only a teacher+ could reach outlived the role"
            );
        }
        // Both bookings — the pending request and the confirmed meeting — ended
        // in a state their requester can still read, saying who dropped it.
        for booking in [&pending, &approved] {
            let after = appointment::read(&db, booking.get_id())
                .await
                .unwrap()
                .expect("the requester keeps the row");
            assert_eq!(after.get_status(), AppointmentStatus::Cancelled);
            assert_eq!(after.get_cancelled_by(), Some(teacher.get_id()));
            assert!(after.get_cancel_reason().is_some(), "and why");
        }
        // The seat each held died with its slot row, so nothing is pinned: no
        // live booking is left pointing at a slot that no longer exists.
        let mut result = db
            .query(
                "SELECT VALUE id FROM appointment \
                 WHERE status IN ['pending', 'approved'] AND slot.starts_at = NONE",
            )
            .await
            .unwrap()
            .check()
            .unwrap();
        assert!(result.take::<Vec<surrealdb::types::RecordId>>(0).unwrap().is_empty());

        // Another teacher's calendar is nobody else's business.
        assert_eq!(
            AppointmentSlot::list_for_teacher(other.get_id(), &db)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            appointment::read(&db, elsewhere.get_id())
                .await
                .unwrap()
                .unwrap()
                .get_status(),
            AppointmentStatus::Pending
        );
    }
}
