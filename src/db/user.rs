//! The `user` table: the row mint (with the duplicate-username mapping), row
//! reads and listings, the admin-floor count, the role cascade transaction,
//! and the field-scoped writers. The workflows that sequence these under
//! [`crate::service::user::ADMIN_FLOOR_LOCK`] live in
//! [`crate::service::user`].

use surrealdb::types::{RecordId, SurrealValue};

use crate::constant::{
    APPOINTMENT_SLOT_TABLE, APPOINTMENT_TABLE, BOARD_TABLE, CLASS_GROUP_TABLE,
    CLASS_MEMBER_COUNT_FIELD, CLASS_MEMBER_TABLE, COURSE_TABLE, ENROLLMENT_COUNT_FIELD,
    ENROLLMENT_TABLE, PARENT_LINK_TABLE, REGISTRATION_COUNT_FIELD, REGISTRATION_FROZEN_GUARD,
    REGISTRATION_TABLE,
};
use crate::database::{Database, transaction_with_retry, write_with_retry};
use crate::db::field_update::FieldUpdate;
use crate::db::page::PagedList;
use crate::domain::appointment::AppointmentStatus;
use crate::domain::board::Board;
use crate::domain::note_file::FileContentType;
use crate::domain::preferences::{Language, PaletteColor, Theme};
use crate::domain::profile::{Bio, BirthDate, DisplayName, Email, PersonName, Phone};
use crate::domain::role::Role;
use crate::domain::text_fold::{search_fold, search_fold_sql};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{PasswordHash, User, UserId, Username};
use crate::error::AppError;

/// Register a new account. New users always start as [`Role::Student`];
/// elevation is a separate, admin-only action
/// (see [`crate::service::user::set_role`]).
pub async fn create(
    db: &Database,
    username: Username,
    password_hash: PasswordHash,
) -> Result<User, AppError> {
    create_with_role(db, username, password_hash, Role::Student).await
}

/// The one row-minting path. `role` is written *with* the row rather than
/// patched on afterwards, which is what makes the admin seed atomic: a
/// create-then-promote pair can be interrupted between its halves (a SIGKILL,
/// or a cancelled future), and the row left behind is an ordinary student
/// account that [`crate::service::user::ensure_admin`] must then refuse to
/// touch — a deployment with no admin and no way for a later boot to repair it.
pub async fn create_with_role(
    db: &Database,
    username: Username,
    password_hash: PasswordHash,
    role: Role,
) -> Result<User, AppError> {
    if find_by_username(db, username.as_str()).await?.is_some() {
        return Err(AppError::Conflict("username already taken"));
    }
    let user = User {
        id: UserId::generate(),
        username,
        password_hash,
        role,
        name: None,
        surname: None,
        email: None,
        phone: None,
        birth_date: None,
        theme: None,
        language: None,
        palette_color: None,
        display_name: None,
        bio: None,
        avatar_file: None,
        avatar_content_type: None,
        avatar_size: None,
    };
    let created: Result<Option<User>, surrealdb::Error> =
        db.create(user.id.record()).content(user.clone()).await;
    match created {
        Ok(Some(created)) => Ok(created),
        Ok(None) => Err(AppError::Internal("failed to create user".into())),
        // The availability pre-check above is not atomic with the insert:
        // two concurrent registrations can both pass it, and the loser then
        // trips the unique username index. That loss is the same condition
        // as the sequential duplicate, so report the same 409 — not a 500.
        Err(err) => {
            if find_by_username(db, user.username.as_str())
                .await?
                .is_some()
            {
                Err(AppError::Conflict("username already taken"))
            } else {
                Err(err.into())
            }
        }
    }
}

pub async fn read(db: &Database, id: &UserId) -> Result<Option<User>, AppError> {
    Ok(db.select(id.record()).await?)
}

pub async fn list_all(
    db: &Database,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<User>, i64), AppError> {
    PagedList::new("user", "ORDER BY id DESC")
        .run(limit, offset, db)
        .await
}

/// Fetch the users behind `ids` in one query. Ids with no row are simply
/// absent from the result — the caller decides how to degrade.
pub async fn list_by_ids(db: &Database, ids: &[UserId]) -> Result<Vec<User>, AppError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let records: Vec<RecordId> = ids.iter().map(UserId::record).collect();
    let mut result = db
        .query("SELECT * FROM user WHERE id IN $ids")
        .bind(("ids", records))
        .await?
        .check()?;
    Ok(result.take::<Vec<User>>(0)?)
}

/// Every user holding exactly `role` — e.g. the roster of a role-targeted
/// event. Exact match, not `at_least`: "all teachers" means teachers, not
/// managers and admins too.
pub async fn list_by_role(db: &Database, role: Role) -> Result<Vec<User>, AppError> {
    let mut result = db
        .query("SELECT * FROM user WHERE role = $role ORDER BY id DESC")
        .bind(("role", role))
        .await?
        .check()?;
    Ok(result.take::<Vec<User>>(0)?)
}

/// Would lowering `target` out of `admin` leave the school with none? Both
/// halves are asked of the **live** rows in one round trip: is that row an
/// admin right now, and does any other admin exist. It asks for one id per
/// half rather than a `count()` — a count over an indexed field compared
/// against a plan-time value answers `{count: N}` on the real server, and
/// neither half needs a number.
///
/// Caller must hold
/// [`crate::service::user::ADMIN_FLOOR_LOCK`] for the answer to still be
/// true by the time it is acted on.
pub async fn would_orphan_admins(db: &Database, target: &UserId) -> Result<bool, AppError> {
    let mut result = db
        .query(
            "SELECT VALUE id FROM user WHERE id = $usr AND role = $role;\n\
             SELECT VALUE id FROM user WHERE role = $role AND id != $usr LIMIT 1",
        )
        .bind(("role", Role::Admin))
        .bind(("usr", target.record()))
        .await?
        .check()?;
    let is_admin = !result.take::<Vec<RecordId>>(0)?.is_empty();
    let others = !result.take::<Vec<RecordId>>(1)?.is_empty();
    Ok(is_admin && !others)
}

/// Case- and diacritic-insensitive fragment search over username, name,
/// and surname. Needle and columns both go through
/// [`crate::domain::text_fold`], so `ilker` finds `İLKER` and back —
/// backs the user pickers. `role` narrows to one role (e.g. only students
/// for an enroll picker); `None` searches everyone. `allowed_roles`
/// narrows the *visible* set (the roles a non-staff caller may message —
/// see [`Role::messageable_roles`]); it is part of the query, not a
/// post-filter, so `total` counts only what the caller may see. A blank
/// `query`
/// matches everyone, so blank + `role` is a role-scoped listing. Returns
/// every match, ordered by username; the HTTP layer pages the result like
/// any other list (no built-in cap — an over-broad fragment is windowed
/// by `?limit`).
pub async fn search(
    db: &Database,
    query: &str,
    role: Option<Role>,
    allowed_roles: Option<&[Role]>,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<User>, i64), AppError> {
    let needle = search_fold(query.trim());
    let text_clause = format!(
        "({} CONTAINS $q OR {} CONTAINS $q OR {} CONTAINS $q)",
        search_fold_sql("username"),
        search_fold_sql("name ?? ''"),
        search_fold_sql("surname ?? ''"),
    );
    let mut clauses = Vec::new();
    if !needle.is_empty() {
        clauses.push(text_clause.as_str());
    }
    if role.is_some() {
        clauses.push("role = $role");
    }
    if allowed_roles.is_some() {
        clauses.push("role IN $allowed");
    }
    let where_clause = if clauses.is_empty() {
        "true".to_string()
    } else {
        clauses.join(" AND ")
    };
    let mut list = PagedList::new(format!("user WHERE {where_clause}"), "ORDER BY username")
        .bind("q", needle);
    if let Some(role) = role {
        list = list.bind("role", role);
    }
    if let Some(allowed) = allowed_roles {
        list = list.bind("allowed", allowed.to_vec());
    }
    list.run(limit, offset, db).await
}

/// The role write and every sweep it owes, in one transaction — the
/// statement list behind [`crate::service::user::set_role`]. See there for
/// the floor that guards it and the arm-by-arm reasoning; what follows is
/// the store side.
///
/// Writes *only* the `role` field of the user row (never the whole row):
/// the row mixes admin-owned (role) and self-service (profile, preferences)
/// fields, and each writer starts from a snapshot read at request start. A
/// whole-row write would carry the snapshot's copy of the *other* group back
/// over a concurrent edit — an in-flight profile save silently reverting an
/// admin's demotion, or this write erasing a profile edit that raced it.
/// [`crate::db::user::set_profile`] and
/// [`crate::db::user::set_preferences`] are scoped for the same reason.
///
/// What is swept, and why the conditions differ:
///
/// * **Non-student** sheds class memberships, *every* enrollment row
///   (hand-placed ones included — only students enroll, and a kept row
///   would grant nothing and count a seat) and the parent links observing
///   them. Memberships and enrollments were already one transaction and
///   still are: released apart, an enrollment row could be left tagged with
///   a class whose counters had been given back, so the class passed its
///   zero-zero delete guard and no sweep could ever reach the row again.
/// * **Parent** additionally frees event seats, comes off every whiteboard
///   roster and has the boards they *created* closed — permanently
///   read-only, rows kept, because a room whose creator is demoted is
///   commandable by nobody at all (see the statement's own note). The seat
///   is the one thing on a signup list nothing
///   else could remove: staff free their own by hand (`unregister` allows
///   the self case) but a parent can reach no such door, so a promotion
///   strands nothing and only this demotion does. A list that has already
///   **frozen** ([`REGISTRATION_FROZEN_GUARD`]) is left exactly as it
///   stands — past the freeze it is historical record, re-registering
///   answers 409, and rewriting it here would be irrecoverable. A signup
///   whose event record is *gone* has no seat to hand back and no list that
///   can freeze, so it is deleted rather than skipped: skipped, it is
///   stranded forever (`unregister` 404s on the missing event).
/// * **Below teacher** gives up course staffing and homeroom-teacher
///   columns — only teacher+ may hold either — and the published
///   appointment calendar, whose live bookings are cancelled in the same
///   breath. Nothing else could ever reach those: a slot is listed only on
///   its own teacher's calendar and deleted only by a teacher+, so after the
///   demotion neither its owner nor a manager has a route that yields its
///   id, and a booking on it can be neither decided (teacher+ only) nor
///   cancelled once its window opens — leaving the slot's `occupied` seat
///   pinned at one and the slot itself undeletable forever. The requester
///   keeps the row, `cancelled`, with the teacher on `cancelled_by` and the
///   reason on `cancel_reason`; the slot it pointed at is gone, so the
///   booking renders without a window
///   ([`crate::domain::appointment::Appointment`]'s reader already treats a
///   vanished slot that way). There is no notification anywhere in
///   this crate, so a settled row the person can read is the strongest
///   defined end available — and the alternative, deleting it, would make a
///   confirmed meeting vanish with no trace at all.
///
/// The returned boards are the rooms whose roster this changed, plus those
/// the user created — whose roster is untouched but which are now closed,
/// and they carry that stamp because the close is written first. Publishing
/// to them is the caller's job: an in-process
/// fan-out cannot sit inside a database transaction, and a room told before
/// the commit would re-read the pre-commit state.
///
/// Admissible for [`transaction_with_retry`] by construction: `SELECT`,
/// `UPDATE` and `DELETE` only, so no statement can answer "already exists"
/// and every lost round is a plain re-send.
pub async fn set_role_cascade(
    db: &Database,
    target: &UserId,
    role: Role,
) -> Result<(User, Vec<Board>), AppError> {
    // Built as a statement list rather than one string so the result slot
    // of the board write is *counted*, not hand-tallied against arms that
    // may or may not be in the batch. Slot 0 is `BEGIN`, as everywhere.
    let mut batch = vec![
        "BEGIN TRANSACTION".to_string(),
        "UPDATE $usr SET role = $role RETURN AFTER".to_string(),
    ];
    if role != Role::Student {
        batch.push(format!(
            "LET $links = (DELETE {CLASS_MEMBER_TABLE} WHERE user = $usr RETURN BEFORE)"
        ));
        batch.push(format!(
            "FOR $link IN ($links ?? []) {{ UPDATE $link.class SET \
             {CLASS_MEMBER_COUNT_FIELD} = \
             math::max([({CLASS_MEMBER_COUNT_FIELD} ?? 0) - 1, 0]); }}"
        ));
        batch.push(format!(
            "LET $rows = (DELETE {ENROLLMENT_TABLE} WHERE user = $usr RETURN BEFORE)"
        ));
        batch.push(format!(
            "FOR $row IN ($rows ?? []) {{ UPDATE $row.course SET \
             {ENROLLMENT_COUNT_FIELD} = \
             math::max([({ENROLLMENT_COUNT_FIELD} ?? 0) - 1, 0]); }}"
        ));
        batch.push(format!("DELETE {PARENT_LINK_TABLE} WHERE student = $usr"));
    }
    let mut board_slot = None;
    if role == Role::Parent {
        batch.push(format!(
            "LET $signups = (SELECT * FROM {REGISTRATION_TABLE} WHERE user = $usr)"
        ));
        // Row and seat move together per signup, the way `unregister` does:
        // the seats are independent facts on unrelated events. A missing
        // event matches nothing in the guard *and* nothing in the counter
        // write, which is precisely the orphan rule.
        batch.push(format!(
            "FOR $signup IN ($signups ?? []) {{ \
             IF array::len((SELECT VALUE id FROM $signup.event \
             WHERE {REGISTRATION_FROZEN_GUARD})) = 0 {{ \
             LET $freed = (DELETE $signup.id RETURN BEFORE); \
             UPDATE $signup.event SET {REGISTRATION_COUNT_FIELD} = \
             math::max([({REGISTRATION_COUNT_FIELD} ?? 0) - array::len($freed), 0]); \
             }}; }}"
        ));
        // A room whose *creator* is demoted can never be ended by anyone:
        // the whiteboard is closed to parents outright, so the creator is
        // 404'd off their own board, `clear`/`lock`/`close`/`delete` are
        // creator-only for everyone else, and `Board::list_for_user` is the
        // crate's only enumeration — no manager or admin can so much as find
        // the id. Its participants meanwhile keep drawing (the room re-derives
        // membership per frame and they still pass), into a board only the
        // 50 000-stroke lifetime cap could ever retire. So the demotion
        // retires it, with the same compare-and-set [`Board::close`] uses: an
        // already-closed board keeps its first stamp. Closed and not deleted
        // because the marks are the participants' work too — they keep reading
        // the board and its whole history, and the creator's `board_count`
        // seat stays taken, which is correct while the row it counts exists.
        // Stamped *before* the roster strip so the strip's `RETURN AFTER`
        // carries the closed row the caller fans out.
        batch.push(format!(
            "UPDATE {BOARD_TABLE} SET closed_at = $now WHERE creator = $usr AND closed_at = NONE"
        ));
        board_slot = Some(batch.len());
        batch.push(format!(
            "UPDATE {BOARD_TABLE} SET participants -= $usr \
             WHERE $usr IN participants OR creator = $usr RETURN AFTER"
        ));
    }
    if role != Role::Parent {
        batch.push(format!("DELETE {PARENT_LINK_TABLE} WHERE parent = $usr"));
    }
    if !role.at_least(Role::Teacher) {
        batch.push(format!(
            "UPDATE {COURSE_TABLE} SET teachers -= $usr WHERE $usr IN teachers"
        ));
        batch.push(format!(
            "UPDATE {CLASS_GROUP_TABLE} SET teacher = NONE WHERE teacher = $usr"
        ));
        // The published calendar goes too, and the bookings on it are
        // settled first: a slot only its own teacher can list and only a
        // teacher+ can delete is reachable by nobody once that teacher is
        // demoted, and a live booking on one is worse — nobody can approve,
        // reject or (past its start) cancel it, so it pins the slot's
        // `occupied` seat forever. Cancelled rather than deleted so the
        // person who asked is left with a settled booking they can still
        // read, carrying who dropped it and why; the slot row (and with it
        // the seat) goes, which is what makes this convergent.
        batch.push(format!(
            "LET $slots = (SELECT VALUE id FROM {APPOINTMENT_SLOT_TABLE} WHERE teacher = $usr)"
        ));
        batch.push(format!(
            "UPDATE {APPOINTMENT_TABLE} SET status = '{cancelled}', cancelled_by = $usr, \
             cancel_reason = 'the teacher no longer holds a teaching role' \
             WHERE slot IN $slots AND status IN ['{pending}', '{approved}']",
            cancelled = AppointmentStatus::Cancelled.as_str(),
            pending = AppointmentStatus::Pending.as_str(),
            approved = AppointmentStatus::Approved.as_str(),
        ));
        // Deleting the slots is also what makes a *concurrent* booking safe:
        // `Appointment::book` claims the slot row this deletes, so the two
        // collide in the store and the loser re-sends. A slot *published*
        // concurrently shares no key with any of this, which is why
        // `AppointmentSlot::insert_claimed` claims the user row instead.
        batch.push(format!(
            "DELETE {APPOINTMENT_SLOT_TABLE} WHERE id IN $slots"
        ));
    }
    let sql = format!("{};\nCOMMIT TRANSACTION;", batch.join(";\n"));
    let (mut result, mut errors) = transaction_with_retry(
        db,
        &sql,
        &[
            ("usr".into(), target.record().into_value()),
            ("role".into(), role.into_value()),
            ("now".into(), Timestamp::now().as_millis().into_value()),
        ],
        &[],
    )
    .await?;
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    let updated = result
        .take::<Vec<User>>(1)?
        .into_iter()
        .next()
        .ok_or(AppError::NotFound)?;
    let boards = match board_slot {
        Some(slot) => result.take::<Vec<Board>>(slot)?,
        None => Vec::new(),
    };
    Ok((updated, boards))
}

/// Write the personal-info and public-profile fields the request actually
/// carried — everything a person edits about themselves except the avatar
/// blob, which needs its own writer ([`set_avatar`]). Every
/// column is nullable, so each argument is an outer/inner `Option`: `None`
/// = omitted (not written at all), `Some(None)` = cleared, `Some(Some(v))`
/// = set. Merging the request against the current row is the HTTP layer's
/// job; the caller authorizes.
///
/// Request-scoped, not merely field-scoped: nothing guards the row across
/// the handler's read and this write, so re-sending the snapshot's value
/// for an omitted field would revert a concurrent PATCH of that field —
/// two profile edits (a name and a phone) used to lose each other. See
/// [`set_role_cascade`] for why no writer here touches the whole row.
// One argument per nullable column is the point: folding them into a struct
// would just re-spell the HTTP DTO here and cost the compiler's check that
// every column was considered at the call site.
#[allow(clippy::too_many_arguments)]
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
    FieldUpdate::new(id.record())
        .set("name", name)
        .set("surname", surname)
        .set("email", email)
        .set("phone", phone)
        .set("birth_date", birth_date)
        .set("display_name", display_name)
        .set("bio", bio)
        .run::<User>(db)
        .await
}

/// Point the row at a freshly uploaded avatar blob, returning the row *as
/// it was* — the caller deletes `before.get_avatar_file()` off disk. Losing
/// `RETURN BEFORE` here strands the replaced blob forever: no route ever
/// deletes a user, so nothing else would collect it.
///
/// Field-scoped for the same reason as [`set_profile`]: an avatar
/// upload must not carry a stale snapshot's role back over an admin's
/// change. `None` means the row is gone.
///
/// Sent through [`write_with_retry`] like every other single-statement row
/// write here, unguarded or not: the user row is contended (preferences,
/// profile, role all write it), and a lost round wrote nothing, so
/// re-sending it is the recovery rather than a 500 in the caller's face.
pub async fn set_avatar(
    db: &Database,
    id: &UserId,
    file: &str,
    content_type: &FileContentType,
    size: i64,
) -> Result<Option<User>, AppError> {
    let rows: Vec<User> = write_with_retry(
        db,
        "UPDATE $u SET avatar_file = $file, avatar_content_type = $ct, avatar_size = $size \
         RETURN BEFORE",
        &[
            ("u".into(), id.record().into_value()),
            ("file".into(), file.to_string().into_value()),
            ("ct".into(), content_type.clone().into_value()),
            ("size".into(), size.into_value()),
        ],
    )
    .await?;
    Ok(rows.into_iter().next())
}

/// Drop the avatar, returning the row as it was so the caller can delete
/// the blob. Same `RETURN BEFORE` contract as [`set_avatar`].
pub async fn clear_avatar(db: &Database, id: &UserId) -> Result<Option<User>, AppError> {
    let rows: Vec<User> = write_with_retry(
        db,
        "UPDATE $u SET avatar_file = NONE, avatar_content_type = NONE, \
         avatar_size = NONE RETURN BEFORE",
        &[("u".into(), id.record().into_value())],
    )
    .await?;
    Ok(rows.into_iter().next())
}

/// Replace a user's password hash — the builder's admin-password reset
/// (`POST /schools/{slug}/admin-password`). Only the credential is
/// rewritten; revoking the sessions minted under the old one is the
/// caller's second half
/// ([`crate::db::session::delete_by_user`]), because a reset
/// that leaves a stolen cookie working resets nothing.
pub async fn set_password_hash(
    db: &Database,
    id: &UserId,
    password_hash: PasswordHash,
) -> Result<Option<User>, AppError> {
    let rows: Vec<User> = write_with_retry(
        db,
        "UPDATE $u SET password_hash = $hash RETURN AFTER",
        &[
            ("u".into(), id.record().into_value()),
            ("hash".into(), password_hash.into_value()),
        ],
    )
    .await?;
    Ok(rows.into_iter().next())
}

/// Write the UI-preference fields the request actually carried. Same
/// contract as [`set_profile`]: `None` = omitted (not written),
/// `Some(None)` = cleared back to "never chose", `Some(Some(v))` = set.
pub async fn set_preferences(
    db: &Database,
    id: &UserId,
    theme: Option<Option<Theme>>,
    language: Option<Option<Language>>,
    palette_color: Option<Option<PaletteColor>>,
) -> Result<User, AppError> {
    FieldUpdate::new(id.record())
        .set("theme", theme)
        .set("language", language)
        .set("palette_color", palette_color)
        .run::<User>(db)
        .await
}

pub async fn find_by_username(db: &Database, username: &str) -> Result<Option<User>, AppError> {
    let mut result = db
        .query("SELECT * FROM user WHERE username = $username LIMIT 1")
        .bind(("username", username.to_string()))
        .await?
        .check()?;
    Ok(result.take::<Vec<User>>(0)?.into_iter().next())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_mem;
    use crate::domain::user::Password;

    fn png() -> FileContentType {
        FileContentType::try_new("image/png").unwrap()
    }

    async fn a_user(username: &str, db: &Database) -> User {
        create(
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

    #[tokio::test]
    async fn a_stale_profile_write_cannot_revert_a_role_change() {
        let db = init_mem().await.unwrap();
        let user = a_user("aysenur", &db).await;

        // A PATCH /users/me handler reads its snapshot (role = student)...
        let stale = read(&db, user.get_id()).await.unwrap().unwrap();

        // ...then an admin's role change commits before the profile save runs.
        crate::service::user::set_role(&db, user.get_id(), Role::Teacher)
            .await
            .unwrap();

        // The in-flight save lands from the stale snapshot. It must write only
        // the profile fields — not carry the snapshot's old role back over the
        // admin's change.
        let name = PersonName::try_new("name", "Ayşenur").unwrap();
        set_profile(
            &db,
            stale.get_id(),
            Some(Some(name.clone())),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let after = read(&db, user.get_id()).await.unwrap().unwrap();
        assert_eq!(
            after.get_role(),
            Role::Teacher,
            "the role change must survive the racing profile write"
        );
        assert_eq!(
            after.get_name(),
            Some(&name),
            "the profile edit itself lands"
        );
    }

    #[tokio::test]
    async fn an_avatar_upload_cannot_revert_a_role_change() {
        let db = init_mem().await.unwrap();
        let user = a_user("berk", &db).await;

        // The upload handler holds its snapshot (role = student)...
        let stale = read(&db, user.get_id()).await.unwrap().unwrap();

        // ...an admin promotes and the person edits their bio, both while the
        // blob is still being written...
        crate::service::user::set_role(&db, user.get_id(), Role::Teacher)
            .await
            .unwrap();
        let display_name = DisplayName::try_new("Berk").unwrap();
        set_profile(
            &db,
            user.get_id(),
            None,
            None,
            None,
            None,
            None,
            Some(Some(display_name.clone())),
            None,
        )
        .await
        .unwrap();

        // ...and only then does the avatar land, from the stale row's id.
        set_avatar(&db, stale.get_id(), "blob-1", &png(), 42)
            .await
            .unwrap()
            .expect("the row exists");

        let after = read(&db, user.get_id()).await.unwrap().unwrap();
        assert_eq!(
            after.get_role(),
            Role::Teacher,
            "the role change must survive the racing avatar write"
        );
        assert_eq!(
            after.get_display_name(),
            Some(&display_name),
            "and so must every other column the avatar write never named"
        );
        assert_eq!(after.get_avatar_file(), Some("blob-1"));
        assert_eq!(after.get_avatar_content_type(), Some(&png()));
        assert_eq!(after.get_avatar_size(), Some(42));
    }

    /// The blob cleanup is built entirely on `RETURN BEFORE`: without the old
    /// `avatar_file` coming back, every replace strands a file on disk that no
    /// route ever collects.
    #[tokio::test]
    async fn the_avatar_writers_return_the_replaced_blob() {
        let db = init_mem().await.unwrap();
        let user = a_user("ceyda", &db).await;

        let before = set_avatar(&db, user.get_id(), "blob-1", &png(), 10)
            .await
            .unwrap()
            .expect("the row exists");
        assert_eq!(before.get_avatar_file(), None, "no blob to collect yet");

        let before = set_avatar(&db, user.get_id(), "blob-2", &png(), 20)
            .await
            .unwrap()
            .expect("the row exists");
        assert_eq!(
            before.get_avatar_file(),
            Some("blob-1"),
            "the replaced blob is what the caller must delete"
        );

        let before = clear_avatar(&db, user.get_id())
            .await
            .unwrap()
            .expect("the row exists");
        assert_eq!(before.get_avatar_file(), Some("blob-2"));

        let after = read(&db, user.get_id()).await.unwrap().unwrap();
        assert_eq!(after.get_avatar_file(), None);
        assert_eq!(after.get_avatar_content_type(), None);
        assert_eq!(after.get_avatar_size(), None);

        assert!(
            clear_avatar(&db, &UserId::from_key("yok"))
                .await
                .unwrap()
                .is_none(),
            "a missing row is None, not an error"
        );
    }
}
