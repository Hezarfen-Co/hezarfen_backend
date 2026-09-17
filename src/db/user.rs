//! The `app_user` table: the row mint (with the duplicate-username mapping),
//! row reads and listings, the role cascade transaction (admin floor and all
//! sweeps included), and the field-scoped writers. The workflow that drives
//! the cascade lives in [`crate::service::user`].
//!
//! The compile-time macros map columns by name and check each decoded type
//! against the prepare schema, so the seventeen user columns are spelled
//! out — newtype overrides included — in every static statement below. The
//! repetitions are the check.

use crate::database::{Database, tx_with_retry};
use crate::db::field_update::FieldUpdate;
use crate::db::page::{PagedList, Param};
use crate::domain::board::{Board, BoardId, BoardTitle};
use crate::domain::note_file::FileContentType;
use crate::domain::person::PersonId;
use crate::domain::preferences::{Language, PaletteColor, Theme};
use crate::domain::profile::{Bio, BirthDate, DisplayName, Email, PersonName, Phone};
use crate::domain::role::Role;
use crate::domain::text_fold::{search_fold, search_fold_sql};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{User, UserId, Username};
use crate::error::AppError;

/// Register a new school account for the control-plane `person` whose
/// credential login verifies — `person` is the join key, `None` only where the
/// caller cannot know it yet (tests, the boot seed's row-first order — see
/// [`crate::service::user::ensure_admin`]). New users always start as
/// [`Role::Student`]; elevation is a separate, admin-only action
/// (see [`crate::service::user::set_role`]).
pub async fn create(
    db: &Database,
    username: Username,
    person: Option<PersonId>,
) -> Result<User, AppError> {
    create_with_role(db, username, person, Role::Student).await
}

/// The one row-minting path. `role` is written *with* the row rather than
/// patched on afterwards, which is what makes the admin seed atomic: a
/// create-then-promote pair can be interrupted between its halves (a SIGKILL,
/// or a cancelled future), and the row left behind is an ordinary student
/// account that [`crate::service::user::ensure_admin`] must then refuse to
/// touch — a deployment with no admin and no way for a later boot to repair it.
///
/// The unique index on `username` is the whole availability check. The old
/// pre-check-and-recheck dance existed because the old store could not name
/// the constraint a racing insert had violated; here the violated
/// constraint's name answers directly (`app_user_username`), and the loser
/// of a race gets the same 409 the sequential duplicate always got.
///
/// The row carries its `created_at` mint stamp and, when the caller knows it,
/// the `person` id the credential lives on.
pub async fn create_with_role(
    db: &Database,
    username: Username,
    person: Option<PersonId>,
    role: Role,
) -> Result<User, AppError> {
    let id = UserId::generate();
    match sqlx::query_as!(
        User,
        r#"INSERT INTO app_user (id, username, person, role, created_at)
           VALUES ($1, $2, $3, $4, $5)
           RETURNING id AS "id: UserId",
                     username AS "username: Username",
                     person AS "person: PersonId",
                     role AS "role: Role",
                     name AS "name: PersonName",
                     surname AS "surname: PersonName",
                     email AS "email: Email",
                     phone AS "phone: Phone",
                     birth_date AS "birth_date: BirthDate",
                     theme AS "theme: Theme",
                     language AS "language: Language",
                     palette_color AS "palette_color: PaletteColor",
                     display_name AS "display_name: DisplayName",
                     bio AS "bio: Bio",
                     avatar_file,
                     avatar_content_type AS "avatar_content_type: FileContentType",
                     branch,
                     avatar_size"#,
        id.uuid(),
        username.as_str(),
        person.as_ref().map(PersonId::uuid),
        role.as_str(),
        Timestamp::now().as_millis(),
    )
    .fetch_one(db)
    .await
    {
        Ok(user) => {
            debug_assert_eq!(
                user.get_person().map(PersonId::uuid),
                person.as_ref().map(PersonId::uuid)
            );
            Ok(user)
        }
        Err(err) if crate::database::unique_violation(&err) == Some("app_user_username") => {
            Err(AppError::Conflict("username already taken"))
        }
        Err(err) => Err(err.into()),
    }
}

pub async fn read(db: &Database, id: &UserId) -> Result<Option<User>, AppError> {
    let user = sqlx::query_as!(
        User,
        r#"SELECT id AS "id: UserId",
                  username AS "username: Username",
                  person AS "person: PersonId",
                  role AS "role: Role",
                  name AS "name: PersonName",
                  surname AS "surname: PersonName",
                  email AS "email: Email",
                  phone AS "phone: Phone",
                  birth_date AS "birth_date: BirthDate",
                  theme AS "theme: Theme",
                  language AS "language: Language",
                  palette_color AS "palette_color: PaletteColor",
                  display_name AS "display_name: DisplayName",
                  bio AS "bio: Bio",
                  avatar_file,
                  avatar_content_type AS "avatar_content_type: FileContentType",
                  branch,
                  avatar_size
           FROM app_user WHERE id = $1"#,
        id.uuid()
    )
    .fetch_optional(db)
    .await?;
    Ok(user)
}

pub async fn list_all(
    db: &Database,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<User>, i64), AppError> {
    PagedList::new("app_user", "ORDER BY id DESC")
        .run(limit, offset, db)
        .await
}

/// Fetch the users behind `ids` in one query. Ids with no row are simply
/// absent from the result — the caller decides how to degrade.
pub async fn list_by_ids(db: &Database, ids: &[UserId]) -> Result<Vec<User>, AppError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<uuid::Uuid> = ids.iter().map(UserId::uuid).collect();
    let users = sqlx::query_as!(
        User,
        r#"SELECT id AS "id: UserId",
                  username AS "username: Username",
                  person AS "person: PersonId",
                  role AS "role: Role",
                  name AS "name: PersonName",
                  surname AS "surname: PersonName",
                  email AS "email: Email",
                  phone AS "phone: Phone",
                  birth_date AS "birth_date: BirthDate",
                  theme AS "theme: Theme",
                  language AS "language: Language",
                  palette_color AS "palette_color: PaletteColor",
                  display_name AS "display_name: DisplayName",
                  bio AS "bio: Bio",
                  avatar_file,
                  avatar_content_type AS "avatar_content_type: FileContentType",
                  branch,
                  avatar_size
           FROM app_user WHERE id = ANY($1)"#,
        &ids
    )
    .fetch_all(db)
    .await?;
    Ok(users)
}

/// Every user holding exactly `role` — e.g. the roster of a role-targeted
/// event. Exact match, not `at_least`: "all teachers" means teachers, not
/// managers and admins too.
pub async fn list_by_role(db: &Database, role: Role) -> Result<Vec<User>, AppError> {
    let users = sqlx::query_as!(
        User,
        r#"SELECT id AS "id: UserId",
                  username AS "username: Username",
                  person AS "person: PersonId",
                  role AS "role: Role",
                  name AS "name: PersonName",
                  surname AS "surname: PersonName",
                  email AS "email: Email",
                  phone AS "phone: Phone",
                  birth_date AS "birth_date: BirthDate",
                  theme AS "theme: Theme",
                  language AS "language: Language",
                  palette_color AS "palette_color: PaletteColor",
                  display_name AS "display_name: DisplayName",
                  bio AS "bio: Bio",
                  avatar_file,
                  avatar_content_type AS "avatar_content_type: FileContentType",
                  branch,
                  avatar_size
           FROM app_user WHERE role = $1 ORDER BY id DESC"#,
        role.as_str()
    )
    .fetch_all(db)
    .await?;
    Ok(users)
}

/// Case- and diacritic-insensitive fragment search over username, name,
/// and surname. Needle and columns both go through
/// [`crate::domain::text_fold`], so `ilker` finds `İLKER` and back —
/// backs the user pickers. `role` narrows to one role (e.g. only students
/// for an enroll picker); `None` searches everyone. `allowed_roles`
/// narrows the *visible* set (the roles a non-staff caller may message —
/// see [`Role::messageable_roles`]); it is part of the query, not a
/// post-filter, so `total` counts only what the caller may see. A blank
/// `query` matches everyone visible: blank alone is the caller's whole
/// directory, blank + `role` a role-scoped listing. Returns
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
    // Placeholders are positional, and every clause is optional, so the
    // numbering is assigned as the clauses are added. Containment is spelled
    // `position(...) > 0` rather than `LIKE` so the needle's `%` and `_`
    // stay literal characters — the fold does not escape, and a query for
    // `100%` must not grow a wildcard.
    let mut binds: Vec<Param> = Vec::new();
    let mut clauses: Vec<String> = Vec::new();
    if !needle.is_empty() {
        let n = binds.len() + 1;
        clauses.push(format!(
            "(position(${n} in {}) > 0 \
             OR position(${n} in {}) > 0 \
             OR position(${n} in {}) > 0)",
            search_fold_sql("username"),
            search_fold_sql("coalesce(name, '')"),
            search_fold_sql("coalesce(surname, '')"),
        ));
        binds.push(Param::Text(needle));
    }
    if let Some(role) = role {
        let n = binds.len() + 1;
        clauses.push(format!("role = ${n}"));
        binds.push(Param::Text(role.as_str().to_string()));
    }
    if let Some(allowed) = allowed_roles {
        let n = binds.len() + 1;
        clauses.push(format!("role = ANY(${n})"));
        binds.push(Param::Texts(
            allowed
                .iter()
                .map(|role| role.as_str().to_string())
                .collect(),
        ));
    }
    let where_clause = if clauses.is_empty() {
        "TRUE".to_string()
    } else {
        clauses.join(" AND ")
    };
    let mut list = PagedList::new(
        format!("app_user WHERE {where_clause}"),
        "ORDER BY username",
    );
    for bind in binds {
        list = list.bind(bind);
    }
    list.run(limit, offset, db).await
}

/// The role write and every sweep it owes, in one transaction — the
/// statement list behind [`crate::service::user::set_role`]; see there for
/// the floor that guards it and the arm-by-arm reasoning. What follows is
/// the store side.
///
/// Writes *only* the `role` field of the user row (never the whole row):
/// the row mixes admin-owned (role) and self-service (profile, preferences)
/// fields, and each writer starts from a snapshot read at request start. A
/// whole-row write would carry the snapshot's copy of the *other* group back
/// over a concurrent edit — an in-flight profile save silently reverting an
/// admin's demotion, or this write erasing a profile edit that raced it.
/// [`set_profile`] and [`set_preferences`] are scoped for the same reason.
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
///   **frozen** (an audience-`registration` event whose start-or-end time
///   has passed — the same predicate
///   [`crate::domain::registration`]'s Rust check enforces) is left exactly
///   as it stands — past the freeze it is historical record,
///   re-registering answers 409, and rewriting it here would be
///   irrecoverable. A signup whose event record is *gone* has no seat to
///   hand back and no list that can freeze, so it is deleted rather than
///   skipped: skipped, it is stranded forever (`unregister` 404s on the
///   missing event).
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
///   reason on `cancel_reason`; the slot it pointed at is gone (the
///   `ON DELETE SET NULL` link carries the delete), so the booking renders
///   without a window
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
/// **The admin floor is the role write's own `WHERE`.** The old guard was a
/// count read under a process-wide lock held across the whole cascade,
/// because the old store could not serialize a cross-record count against a
/// concurrent update (write-skew). Here the count rides the very row write
/// it guards — a demotion that would leave no other admin writes zero rows
/// — so two racing demotions contend on row locks and exactly one lands.
/// Zero rows therefore means the floor refused; the missing-row reading of
/// zero rows is unreachable (no route deletes a user row, and the caller's
/// 404 pre-read answers that case first anyway).
pub async fn set_role_cascade(
    db: &Database,
    target: &UserId,
    role: Role,
) -> Result<(User, Vec<Board>), AppError> {
    // One wall-clock read for the whole cascade, bound by every stamping
    // statement — the old batch's single `$now`.
    let now = Timestamp::now().as_millis();
    // Owned capture: an `async move` closure holding a `&UserId` fails the
    // higher-ranked `Send` check `tx_with_retry`'s future must pass.
    let target = *target;
    tx_with_retry(db, true, async move |tx| {
        // The floor's serialization point: a demotion takes the admin set's
        // row locks *before* the write, so two same-instant demotions of
        // each other cannot both count the other as the surviving admin —
        // the loser re-counts against the winner's commit and refuses
        // here. (The predicate on the `UPDATE` below stays as the write's
        // own guard; under these locks it can no longer be raced past.)
        if role != Role::Admin {
            let current = sqlx::query!(
                r#"SELECT role AS "role: Role" FROM app_user WHERE id = $1"#,
                target.uuid()
            )
            .fetch_optional(&mut *tx)
            .await?
            .map(|row| row.role);
            if current == Some(Role::Admin) {
                let admins = sqlx::query_as::<_, (uuid::Uuid,)>(
                    "SELECT id FROM app_user WHERE role = 'admin' FOR UPDATE",
                )
                .fetch_all(&mut *tx)
                .await?;
                if admins.len() <= 1 {
                    return Err(AppError::Conflict(
                        "the school must keep at least one admin — promote another account first",
                    ));
                }
            }
        }

        // The role write, floor guard included. `$2 <> 'admin'` arms the
        // guard only for a demotion: a promotion or a same-role rewrite can
        // never orphan the admins.
        let updated = sqlx::query_as!(
            User,
            r#"UPDATE app_user SET role = $2
               WHERE id = $1
                 AND NOT (role = 'admin'
                          AND $2 <> 'admin'
                          AND (SELECT count(*) FROM app_user
                               WHERE role = 'admin' AND id <> $1) = 0)
               RETURNING id AS "id: UserId",
                         username AS "username: Username",
                         person AS "person: PersonId",
                         role AS "role: Role",
                         name AS "name: PersonName",
                         surname AS "surname: PersonName",
                         email AS "email: Email",
                         phone AS "phone: Phone",
                         birth_date AS "birth_date: BirthDate",
                         theme AS "theme: Theme",
                         language AS "language: Language",
                         palette_color AS "palette_color: PaletteColor",
                         display_name AS "display_name: DisplayName",
                         bio AS "bio: Bio",
                         avatar_file,
                         avatar_content_type AS "avatar_content_type: FileContentType",
                         branch,
                         avatar_size"#,
            target.uuid(),
            role.as_str(),
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(updated) = updated else {
            return Err(AppError::Conflict(
                "the school must keep at least one admin — promote another account first",
            ));
        };

        if role != Role::Student {
            // Class memberships go *soft*: the live stint is stamped, not
            // deleted — membership history survives a demotion, and the
            // partial unique index leaves the pair free to rejoin if the
            // account is promoted back — while each class they were live in
            // gets its seat back (the counter counts live rows). One live row
            // per (class, user), so one decrement each.
            sqlx::query!(
                r#"WITH gone AS (
                       UPDATE class_member SET left_at = $2
                        WHERE app_user = $1 AND left_at IS NULL
                        RETURNING class
                   )
                   UPDATE class_group
                   SET class_member_count = GREATEST(class_member_count - 1, 0)
                   WHERE id IN (SELECT gone.class FROM gone)"#,
                target.uuid(),
                now,
            )
            .execute(&mut *tx)
            .await?;
            // Every enrollment row — hand-placed included — and each
            // *instance's* roster count with it (the roster counter lives on
            // the class×course instance now).
            sqlx::query!(
                r#"WITH gone AS (
                       DELETE FROM enrollment WHERE app_user = $1 RETURNING class_course
                   )
                   UPDATE class_course
                   SET enrollment_count = GREATEST(enrollment_count - 1, 0)
                   WHERE id IN (SELECT gone.class_course FROM gone)"#,
                target.uuid(),
            )
            .execute(&mut *tx)
            .await?;
            // The individual club/supervised-study memberships go with them: a demoted
            // account may hold no seat in a school-scoped course either, and
            // each course gets its membership count back.
            sqlx::query!(
                r#"WITH gone AS (
                       DELETE FROM course_membership WHERE app_user = $1 RETURNING course
                   )
                   UPDATE course
                   SET course_membership_count = GREATEST(course_membership_count - 1, 0)
                   WHERE id IN (SELECT gone.course FROM gone)"#,
                target.uuid(),
            )
            .execute(&mut *tx)
            .await?;
            sqlx::query!("DELETE FROM parent_link WHERE student = $1", target.uuid())
                .execute(&mut *tx)
                .await?;
        }

        if role == Role::Parent {
            // Signups whose event is gone: no seat to hand back, no list
            // that could freeze — delete the stray row outright.
            sqlx::query!(
                r#"DELETE FROM registration r
                   WHERE r.app_user = $1
                     AND NOT EXISTS (SELECT 1 FROM event e WHERE e.id = r.event)"#,
                target.uuid(),
            )
            .execute(&mut *tx)
            .await?;
            // Row and seat move together per signup, the way `unregister`
            // does: the seats are independent facts on unrelated events. A
            // frozen list keeps its rows exactly as they stand; every other
            // list — including a registration event with no dates at all,
            // which never freezes — releases the seat.
            sqlx::query!(
                r#"WITH gone AS (
                       DELETE FROM registration r
                       USING event e
                       WHERE r.event = e.id
                         AND r.app_user = $1
                         AND NOT (e.audience_kind = 'registration'
                                  AND COALESCE(e.starts_at, e.ends_at) IS NOT NULL
                                  AND COALESCE(e.starts_at, e.ends_at) <= $2)
                       RETURNING r.event AS event_id
                   )
                   UPDATE event
                   SET registration_count = GREATEST(registration_count - 1, 0)
                   WHERE id IN (SELECT gone.event_id FROM gone)"#,
                target.uuid(),
                now,
            )
            .execute(&mut *tx)
            .await?;
            // A room whose *creator* is demoted can never be ended by anyone:
            // the whiteboard is closed to parents outright, so the creator is
            // 404'd off their own board, `clear`/`lock`/`close`/`delete` are
            // creator-only for everyone else, and
            // `crate::db::board::list_for_user` is the crate's only
            // enumeration — no manager or admin can so much as find the id.
            // Its participants meanwhile keep drawing (the room re-derives
            // membership per frame and they still pass), into a board only
            // the 50 000-stroke lifetime cap could ever retire. So the
            // demotion retires it, with the same compare-and-set
            // [`crate::db::board::close`] uses: an already-closed board
            // keeps its first stamp. Closed and not deleted because the
            // marks are the participants' work too — they keep reading the
            // board and its whole history, and the creator's `board_count`
            // seat stays taken, which is correct while the row it counts
            // exists. Stamped *before* the roster strip so the strip's
            // read-back below carries the closed row the caller fans out.
            // The strip itself is the junction's: the demoted user's roster
            // rows are deleted and their board ids collected from
            // RETURNING, then the affected rooms are read back in the same
            // transaction — the junction delete cannot return the whole
            // row the way the old `array_remove` `UPDATE` did.
            sqlx::query!(
                "UPDATE board SET closed_at = $2 WHERE creator = $1 AND closed_at IS NULL",
                target.uuid(),
                now,
            )
            .execute(&mut *tx)
            .await?;
            let stripped = sqlx::query!(
                "DELETE FROM board_participant WHERE participant = $1 RETURNING board",
                target.uuid(),
            )
            .fetch_all(&mut *tx)
            .await?;
            let stripped_boards: Vec<uuid::Uuid> =
                stripped.into_iter().map(|row| row.board).collect();
            let boards = sqlx::query_as!(
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
                   FROM board b
                   WHERE b.id = ANY($1::uuid[]) OR b.creator = $2
                   ORDER BY b.id"#,
                &stripped_boards,
                target.uuid(),
            )
            .fetch_all(&mut *tx)
            .await?;
            return Ok((updated, boards));
        }

        if role != Role::Parent {
            sqlx::query!("DELETE FROM parent_link WHERE parent = $1", target.uuid())
                .execute(&mut *tx)
                .await?;
        }

        if !role.at_least(Role::Teacher) {
            // Instance staffing goes — only teacher+ may hold a seat, and the
            // seat is a `class_course_teacher` row now (D6 moved assignment
            // onto the instance): one `DELETE` by its `teacher` index takes
            // every assignment.
            sqlx::query!(
                "DELETE FROM class_course_teacher WHERE teacher = $1",
                target.uuid(),
            )
            .execute(&mut *tx)
            .await?;
            sqlx::query!(
                "UPDATE class_group SET teacher = NULL WHERE teacher = $1",
                target.uuid(),
            )
            .execute(&mut *tx)
            .await?;
            // The published calendar goes too, and the bookings on it are
            // settled first: a slot only its own teacher can list and only a
            // teacher+ can delete is reachable by nobody once that teacher is
            // demoted, and a live booking on one is worse — nobody can
            // approve, reject or (past its start) cancel it, so it pins the
            // slot's `occupied` seat forever. Cancelled rather than deleted
            // so the person who asked is left with a settled booking they can
            // still read, carrying who dropped it and why; the slot row (and
            // with it the seat) goes, which is what makes this convergent.
            sqlx::query!(
                r#"UPDATE appointment
                   SET status = 'cancelled', cancelled_by = $1, cancel_reason = $2
                   WHERE slot IN (SELECT id FROM appointment_slot WHERE teacher = $1)
                     AND status IN ('pending', 'approved')"#,
                target.uuid(),
                "the teacher no longer holds a teaching role",
            )
            .execute(&mut *tx)
            .await?;
            // Deleting the slots is also what makes a *concurrent* booking
            // safe: `Appointment::book` claims the slot row this deletes, so
            // the two collide in the store. A slot *published* concurrently
            // shares no key with any of this, which is why the publish path
            // claims the user row instead. The settled booking rows survive
            // the delete with `slot = NULL` (`ON DELETE SET NULL`) — still
            // readable, windowless.
            sqlx::query!(
                "DELETE FROM appointment_slot WHERE teacher = $1",
                target.uuid(),
            )
            .execute(&mut *tx)
            .await?;
        }

        Ok((updated, Vec::new()))
    })
    .await
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
    branch: Option<Option<String>>,
) -> Result<User, AppError> {
    // `Some(None)` must bind an explicit NULL and `Some(Some(v))` a value:
    // `Param::OptText` carries both shapes of one nullable TEXT column.
    fn text(value: Option<Option<&str>>) -> Option<Param> {
        value.map(|inner| Param::OptText(inner.map(str::to_string)))
    }
    FieldUpdate::new("app_user", id.uuid())
        .set(
            "name",
            text(name.as_ref().map(|n| n.as_ref().map(|x| x.as_str()))),
        )
        .set(
            "surname",
            text(surname.as_ref().map(|n| n.as_ref().map(|x| x.as_str()))),
        )
        .set(
            "email",
            text(email.as_ref().map(|n| n.as_ref().map(|x| x.as_str()))),
        )
        .set(
            "phone",
            text(phone.as_ref().map(|n| n.as_ref().map(|x| x.as_str()))),
        )
        .set(
            "birth_date",
            text(birth_date.as_ref().map(|n| n.as_ref().map(|x| x.as_str()))),
        )
        .set(
            "display_name",
            text(
                display_name
                    .as_ref()
                    .map(|n| n.as_ref().map(|x| x.as_str())),
            ),
        )
        .set(
            "bio",
            text(bio.as_ref().map(|n| n.as_ref().map(|x| x.as_str()))),
        )
        // The subject area is plain text, not a newtype: the school's own list is
        // settings data, so its membership check belongs to the write path
        // that holds that list (the web layer), not to a row type.
        .set(
            "branch",
            text(branch.as_ref().map(|n| n.as_ref().map(String::as_str))),
        )
        .run(db)
        .await
}

/// Point the row at a freshly uploaded avatar blob, returning the row *as
/// it was* — the caller deletes `before.get_avatar_file()` off disk. Losing
/// the pre-image here strands the replaced blob forever: no route ever
/// deletes a user, so nothing else would collect it.
///
/// Field-scoped for the same reason as [`set_profile`]: an avatar
/// upload must not carry a stale snapshot's role back over an admin's
/// change. `None` means the row is gone.
///
/// The row is locked across the pre-image read and the write (`FOR
/// UPDATE`), so two racing uploads each collect the blob *they* actually
/// replaced — the read and the write are one unit, not a guess that stayed
/// true.
pub async fn set_avatar(
    db: &Database,
    id: &UserId,
    file: &str,
    content_type: &FileContentType,
    size: i64,
) -> Result<Option<User>, AppError> {
    // Owned captures, same `Send` rule as every `tx_with_retry` closure.
    let id = *id;
    let file = file.to_string();
    let content_type = content_type.clone();
    tx_with_retry(db, false, async move |tx| {
        let before = sqlx::query_as!(
            User,
            r#"SELECT id AS "id: UserId",
                      username AS "username: Username",
                      person AS "person: PersonId",
                      role AS "role: Role",
                      name AS "name: PersonName",
                      surname AS "surname: PersonName",
                      email AS "email: Email",
                      phone AS "phone: Phone",
                      birth_date AS "birth_date: BirthDate",
                      theme AS "theme: Theme",
                      language AS "language: Language",
                      palette_color AS "palette_color: PaletteColor",
                      display_name AS "display_name: DisplayName",
                      bio AS "bio: Bio",
                      avatar_file,
                      avatar_content_type AS "avatar_content_type: FileContentType",
                      branch,
                      avatar_size
               FROM app_user WHERE id = $1 FOR UPDATE"#,
            id.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(before) = before else {
            return Ok(None);
        };
        sqlx::query!(
            r#"UPDATE app_user
               SET avatar_file = $2, avatar_content_type = $3, avatar_size = $4
               WHERE id = $1"#,
            id.uuid(),
            file.as_str(),
            content_type.as_str(),
            size,
        )
        .execute(&mut *tx)
        .await?;
        Ok(Some(before))
    })
    .await
}

/// Drop the avatar, returning the row as it was so the caller can delete
/// the blob. Same pre-image contract as [`set_avatar`].
pub async fn clear_avatar(db: &Database, id: &UserId) -> Result<Option<User>, AppError> {
    let id = *id;
    tx_with_retry(db, false, async move |tx| {
        let before = sqlx::query_as!(
            User,
            r#"SELECT id AS "id: UserId",
                      username AS "username: Username",
                      person AS "person: PersonId",
                      role AS "role: Role",
                      name AS "name: PersonName",
                      surname AS "surname: PersonName",
                      email AS "email: Email",
                      phone AS "phone: Phone",
                      birth_date AS "birth_date: BirthDate",
                      theme AS "theme: Theme",
                      language AS "language: Language",
                      palette_color AS "palette_color: PaletteColor",
                      display_name AS "display_name: DisplayName",
                      bio AS "bio: Bio",
                      avatar_file,
                      avatar_content_type AS "avatar_content_type: FileContentType",
                      branch,
                      avatar_size
               FROM app_user WHERE id = $1 FOR UPDATE"#,
            id.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(before) = before else {
            return Ok(None);
        };
        sqlx::query!(
            r#"UPDATE app_user
               SET avatar_file = NULL, avatar_content_type = NULL, avatar_size = NULL
               WHERE id = $1"#,
            id.uuid(),
        )
        .execute(&mut *tx)
        .await?;
        Ok(Some(before))
    })
    .await
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
    let theme = theme.map(|t| Param::OptText(t.map(|v| v.as_str().to_string())));
    let language = language.map(|l| Param::OptText(l.map(|v| v.as_str().to_string())));
    let palette_color = palette_color.map(|p| Param::OptText(p.map(|v| v.as_str().to_string())));
    FieldUpdate::new("app_user", id.uuid())
        .set("theme", theme)
        .set("language", language)
        .set("palette_color", palette_color)
        .run(db)
        .await
}

pub async fn find_by_username(db: &Database, username: &str) -> Result<Option<User>, AppError> {
    let user = sqlx::query_as!(
        User,
        r#"SELECT id AS "id: UserId",
                  username AS "username: Username",
                  person AS "person: PersonId",
                  role AS "role: Role",
                  name AS "name: PersonName",
                  surname AS "surname: PersonName",
                  email AS "email: Email",
                  phone AS "phone: Phone",
                  birth_date AS "birth_date: BirthDate",
                  theme AS "theme: Theme",
                  language AS "language: Language",
                  palette_color AS "palette_color: PaletteColor",
                  display_name AS "display_name: DisplayName",
                  bio AS "bio: Bio",
                  avatar_file,
                  avatar_content_type AS "avatar_content_type: FileContentType",
                  branch,
                  avatar_size
           FROM app_user WHERE username = $1"#,
        username
    )
    .fetch_optional(db)
    .await?;
    Ok(user)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_test_db;

    fn png() -> FileContentType {
        FileContentType::try_new("image/png").unwrap()
    }

    async fn a_user(username: &str, db: &Database) -> User {
        create(db, Username::try_new(username).unwrap(), None)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_stale_profile_write_cannot_revert_a_role_change() {
        let (db, _leases) = init_test_db().await;
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
        let (db, _leases) = init_test_db().await;
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

    /// The blob cleanup is built on the pre-image: without the old
    /// `avatar_file` coming back, every replace strands a file on disk that
    /// no route ever collects.
    #[tokio::test]
    async fn the_avatar_writers_return_the_replaced_blob() {
        let (db, _leases) = init_test_db().await;
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
