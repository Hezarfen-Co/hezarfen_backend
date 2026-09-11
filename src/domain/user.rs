use std::sync::OnceLock;

use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{
    AI_PRINCIPAL_KEY, APPOINTMENT_SLOT_TABLE, APPOINTMENT_TABLE, BOARD_TABLE, CLASS_GROUP_TABLE,
    CLASS_MEMBER_COUNT_FIELD, CLASS_MEMBER_TABLE, COURSE_TABLE, DECOY_PASSWORD,
    ENROLLMENT_COUNT_FIELD, ENROLLMENT_TABLE, PARENT_LINK_TABLE, REGISTRATION_COUNT_FIELD,
    REGISTRATION_FROZEN_GUARD, REGISTRATION_TABLE, USER_TABLE,
};
use crate::database::{Database, transaction_with_retry, write_with_retry};
use crate::db::field_update::FieldUpdate;
use crate::db::page::PagedList;
use crate::domain::appointment::AppointmentStatus;
use crate::domain::board::Board;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::note_file::FileContentType;
use crate::domain::preferences::{Language, PaletteColor, Theme};
use crate::domain::profile::{Bio, BirthDate, DisplayName, Email, PersonName, Phone};
use crate::domain::role::Role;
use crate::domain::text_fold::{search_fold, search_fold_sql};
use crate::domain::timestamp::Timestamp;
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_password, validate_username};

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

/// Typed user record id (`user:<ulid>`).
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct UserId(RecordId);

impl UserId {
    /// Minted from the process-wide monotonic generator, not `Ulid::new()`:
    /// users list `id DESC` (newest first, [`User::list_all`]) and page by
    /// offset over that order, and a random low half scrambles rows minted in
    /// the same millisecond. Not a secret: the session token is separate
    /// (32-byte CSPRNG, `web::auth`), so id order carries no authority.
    pub fn generate() -> Self {
        Self(RecordId::new(USER_TABLE, next_ulid().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(USER_TABLE, key))
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

/// A validated username. Construction guarantees the restrictions hold and
/// canonicalizes the value: the stored string is trimmed, since the unique
/// index and the login lookup key on it — `"ali "` must be `"ali"`, not a
/// second, visually identical account.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct Username(String);

impl Username {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_username(value)?;
        Ok(Self(value.trim().to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Bounds how many argon2 hashes run at once. `spawn_blocking` is unbounded —
/// tokio grows the blocking pool to 512 threads — and argon2's default `m_cost`
/// is 19 MiB, so an un-capped login flood claims gigabytes (measured: ~17 MB
/// per in-flight hash) and gets the whole process OOM-killed. A permit turns
/// that into queueing: latency instead of memory. `nproc * 2` keeps every core
/// busy while a hash waits on memory bandwidth, with a floor of 2 permits so a
/// single-core box still makes progress.
fn argon2_permits() -> &'static tokio::sync::Semaphore {
    static PERMITS: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    PERMITS.get_or_init(|| {
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        tokio::sync::Semaphore::new(cores * 2)
    })
}

/// A validated plaintext password. Never stored — only hashed or verified.
pub struct Password(String);

impl Password {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_password(value)?;
        Ok(Self(value.to_string()))
    }

    /// Private on purpose: argon2 must never run on an async worker. Callers
    /// outside this module go through [`Password::hash_async`].
    fn hash(&self) -> Result<PasswordHash, AppError> {
        let mut salt_bytes = [0u8; 16];
        getrandom::fill(&mut salt_bytes).map_err(|e| AppError::Internal(format!("rng: {e}")))?;
        let salt = SaltString::encode_b64(&salt_bytes)
            .map_err(|e| AppError::Internal(format!("salt: {e}")))?;
        let hash = Argon2::default()
            .hash_password(self.0.as_bytes(), &salt)?
            .to_string();
        Ok(PasswordHash(hash))
    }

    /// [`Password::hash`] moved onto the blocking pool. argon2 is deliberately
    /// CPU-expensive, so hashing on an async worker starves the runtime: under
    /// load a registration burst pushes requests past `REQUEST_TIMEOUT_SECS`
    /// and they come back as 503. Every request path must use this, not `hash`.
    pub async fn hash_async(&self) -> Result<PasswordHash, AppError> {
        let plain = self.0.clone();
        let _permit = argon2_permits()
            .acquire()
            .await
            .map_err(|e| AppError::Internal(format!("hash permit: {e}")))?;
        tokio::task::spawn_blocking(move || Password(plain).hash())
            .await
            .map_err(|e| AppError::Internal(format!("hash task: {e}")))?
    }
}

/// An argon2 PHC hash, safe to persist.
#[derive(Debug, Clone, SurrealValue)]
pub struct PasswordHash(String);

/// A process-wide decoy hash, computed once on first use. Verifying against it
/// performs the full argon2 work while matching no real account.
fn decoy_hash() -> &'static PasswordHash {
    static DECOY: OnceLock<PasswordHash> = OnceLock::new();
    DECOY.get_or_init(|| {
        Password::try_new(DECOY_PASSWORD)
            .expect("decoy password satisfies the password rules")
            .hash()
            .expect("decoy password hashes")
    })
}

impl PasswordHash {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Private on purpose — see [`Password::hash`]. Use [`PasswordHash::verify_async`].
    fn verify(&self, password: &Password) -> bool {
        match argon2::password_hash::PasswordHash::new(&self.0) {
            Ok(parsed) => Argon2::default()
                .verify_password(password.0.as_bytes(), &parsed)
                .is_ok(),
            Err(_) => false,
        }
    }

    /// [`PasswordHash::verify`] on the blocking pool — see [`Password::hash_async`].
    pub async fn verify_async(&self, password: &Password) -> bool {
        let hash = self.0.clone();
        let plain = password.0.clone();
        // A closed semaphore can't happen (it is `'static` and never closed), but
        // if it ever did this must fail like a pool panic does — 401, never a
        // status an attacker could tell apart from the unknown-user branch.
        let Ok(_permit) = argon2_permits().acquire().await else {
            return false;
        };
        tokio::task::spawn_blocking(move || PasswordHash(hash).verify(&Password(plain)))
            .await
            .unwrap_or(false)
    }

    /// Run a throwaway verification against the decoy hash, on the blocking pool
    /// (see [`Password::hash_async`]; the one-time decoy hash happens there too).
    /// Login calls this when the username is unknown, so a missing user costs the
    /// same argon2 time as a real (wrong-password) check — otherwise the faster
    /// reply would let an attacker enumerate valid usernames by timing.
    pub async fn verify_decoy_async(password: &Password) {
        let plain = password.0.clone();
        // Same permit as the real check, so both login branches queue alike.
        let Ok(_permit) = argon2_permits().acquire().await else {
            return;
        };
        let _ = tokio::task::spawn_blocking(move || decoy_hash().verify(&Password(plain))).await;
    }

    /// Compute the decoy hash at startup, off the async runtime. Without this the
    /// process's first unknown-username login pays hash + verify while a
    /// wrong-password login pays only verify — a one-shot timing tell. The
    /// `OnceLock` still initialises inside a blocking context, just earlier, and
    /// the task is detached so boot never waits on it.
    pub fn prewarm_decoy() {
        // No permit: one hash at boot, and taking one here could only delay the
        // seed's own hash.
        tokio::task::spawn_blocking(|| {
            decoy_hash();
        });
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct User {
    id: UserId,
    username: Username,
    password_hash: PasswordHash,
    role: Role,
    // Personal info, identical for every role. All optional: accounts are
    // created from bare credentials and filled in later, and rows from before
    // these fields existed simply read back as `None`.
    name: Option<PersonName>,
    surname: Option<PersonName>,
    email: Option<Email>,
    phone: Option<Phone>,
    birth_date: Option<BirthDate>,
    // Frontend UI preferences. Optional like the personal info: `None` means
    // "never chose", which the frontend renders as the device preference.
    theme: Option<Theme>,
    language: Option<Language>,
    palette_color: Option<PaletteColor>,
    // The public profile: what other people see instead of the credentials.
    // Optional like everything above — a profile is written after the account
    // exists, so rows from before these fields existed read back as `None`,
    // which is exactly a fresh account that never filled one in. The avatar is
    // a blob on disk (`FILES_PATH`) like every other upload; only its name,
    // type and size live on the row.
    display_name: Option<DisplayName>,
    bio: Option<Bio>,
    avatar_file: Option<String>,
    avatar_content_type: Option<FileContentType>,
    avatar_size: Option<i64>,
}

impl User {
    /// The in-memory principal an AI service acts as over the QUIC bridge.
    ///
    /// Never written to the database, and never read back from one: the id key
    /// is the literal `"ai_service"`, which no minted row can collide with
    /// ([`UserId::generate`] only ever produces ULIDs). Every other field is
    /// inert — the empty password hash parses as no argon2 hash, so it verifies
    /// against nothing, and [`Role::Ai`] clears no `at_least` bar and fails
    /// every exact `== Role::Student` / `== Role::Parent` gate.
    pub(crate) fn ai_principal() -> User {
        User {
            id: UserId::from_key(AI_PRINCIPAL_KEY),
            username: Username(AI_PRINCIPAL_KEY.to_string()),
            password_hash: PasswordHash(String::new()),
            role: Role::Ai,
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
        }
    }

    pub fn get_id(&self) -> &UserId {
        &self.id
    }

    pub fn get_username(&self) -> &Username {
        &self.username
    }

    pub fn get_password_hash(&self) -> &PasswordHash {
        &self.password_hash
    }

    pub fn get_role(&self) -> Role {
        self.role
    }

    pub fn get_name(&self) -> Option<&PersonName> {
        self.name.as_ref()
    }

    pub fn get_surname(&self) -> Option<&PersonName> {
        self.surname.as_ref()
    }

    pub fn get_email(&self) -> Option<&Email> {
        self.email.as_ref()
    }

    pub fn get_phone(&self) -> Option<&Phone> {
        self.phone.as_ref()
    }

    pub fn get_birth_date(&self) -> Option<&BirthDate> {
        self.birth_date.as_ref()
    }

    pub fn get_theme(&self) -> Option<Theme> {
        self.theme
    }

    pub fn get_language(&self) -> Option<Language> {
        self.language
    }

    pub fn get_palette_color(&self) -> Option<&PaletteColor> {
        self.palette_color.as_ref()
    }

    pub fn get_display_name(&self) -> Option<&DisplayName> {
        self.display_name.as_ref()
    }

    pub fn get_bio(&self) -> Option<&Bio> {
        self.bio.as_ref()
    }

    pub fn get_avatar_file(&self) -> Option<&str> {
        self.avatar_file.as_deref()
    }

    pub fn get_avatar_content_type(&self) -> Option<&FileContentType> {
        self.avatar_content_type.as_ref()
    }

    pub fn get_avatar_size(&self) -> Option<i64> {
        self.avatar_size
    }

    /// Register a new account. New users always start as [`Role::Student`];
    /// elevation is a separate, admin-only action (see [`User::set_role`]).
    pub async fn create(
        username: Username,
        password_hash: PasswordHash,
        db: &Database,
    ) -> Result<User, AppError> {
        Self::create_with_role(username, password_hash, Role::Student, db).await
    }

    /// The one row-minting path. `role` is written *with* the row rather than
    /// patched on afterwards, which is what makes the admin seed atomic: a
    /// create-then-promote pair can be interrupted between its halves (a SIGKILL,
    /// or a cancelled future), and the row left behind is an ordinary student
    /// account that [`User::ensure_admin`] must then refuse to touch — a
    /// deployment with no admin and no way for a later boot to repair it.
    pub async fn create_with_role(
        username: Username,
        password_hash: PasswordHash,
        role: Role,
        db: &Database,
    ) -> Result<User, AppError> {
        if Self::find_by_username(username.as_str(), db)
            .await?
            .is_some()
        {
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
                if Self::find_by_username(user.username.as_str(), db)
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

    /// Idempotent startup seed: guarantee an admin account with this username.
    /// Missing → created directly with [`Role::Admin`]. Already an admin →
    /// nothing to do (the stored password stays whatever it is — the seed never
    /// rewrites credentials). Taken by a non-admin → refuse to touch it and log
    /// a warning: silently promoting an account someone else registered would
    /// be a privilege escalation, so that conflict is resolved out-of-band.
    pub async fn ensure_admin(
        username: Username,
        password: Password,
        db: &Database,
    ) -> Result<(), AppError> {
        match Self::find_by_username(username.as_str(), db).await? {
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
                Self::create_with_role(username, password.hash_async().await?, Role::Admin, db)
                    .await?;
                // No `username` field: with OTLP on every event is exported as a
                // log record, so an account name here leaves the process.
                tracing::info!("seeded the admin account named by ADMIN_USERNAME");
                Ok(())
            }
        }
    }

    pub async fn read(id: &UserId, db: &Database) -> Result<Option<User>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    pub async fn list_all(
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<User>, i64), AppError> {
        PagedList::new("user", "ORDER BY id DESC")
            .run(limit, offset, db)
            .await
    }

    /// Fetch the users behind `ids` in one query. Ids with no row are simply
    /// absent from the result — the caller decides how to degrade.
    pub async fn list_by_ids(ids: &[UserId], db: &Database) -> Result<Vec<User>, AppError> {
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
    pub async fn list_by_role(role: Role, db: &Database) -> Result<Vec<User>, AppError> {
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
    async fn would_orphan_admins(target: &UserId, db: &Database) -> Result<bool, AppError> {
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
        query: &str,
        role: Option<Role>,
        allowed_roles: Option<&[Role]>,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
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

    /// Overwrite this user's role **and** shed every grant the new role may not
    /// hold, in one transaction. The caller is responsible for authorizing it.
    ///
    /// Refuses (`Conflict`) to lower the school's last admin, however the
    /// request is spelled and however many demotions are in flight at once —
    /// see [`ADMIN_FLOOR_LOCK`] for why that guard cannot live in the
    /// transaction below.
    ///
    /// Writes *only* the `role` field of the user row (never the whole row):
    /// the row mixes admin-owned (role) and self-service (profile, preferences)
    /// fields, and each writer starts from a snapshot read at request start. A
    /// whole-row write would carry the snapshot's copy of the *other* group back
    /// over a concurrent edit — an in-flight profile save silently reverting an
    /// admin's demotion, or this write erasing a profile edit that raced it.
    /// [`User::set_profile`] and [`User::set_preferences`] are scoped for the
    /// same reason.
    ///
    /// One transaction and not eight statements, which is what this used to be:
    /// a sweep that errored answered 500 with the role already lowered and some
    /// of the old grants still standing — exactly the state the sweeps exist to
    /// prevent, and a crash between them left it with nobody to answer to.
    /// Nothing here is a security hole either way (every gate re-reads the live
    /// role, `web::mod`), but the residue pollutes rosters, counts and signup
    /// lists, and one of those, the event seat, nothing could ever free again.
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
    pub async fn set_role(self, role: Role, db: &Database) -> Result<(User, Vec<Board>), AppError> {
        // The admin floor, held from the count to the commit ([`ADMIN_FLOOR_LOCK`]
        // for why a transaction cannot do this). Asked on the *live* rows rather
        // than `self.role`, so a snapshot that predates a promotion cannot skip
        // the check, and cheap enough to ask on every demotion: one round trip
        // on an admin-only route.
        let _floor = if role == Role::Admin {
            None
        } else {
            let guard = ADMIN_FLOOR_LOCK.lock().await;
            if Self::would_orphan_admins(&self.id, db).await? {
                return Err(AppError::Conflict(
                    "the school must keep at least one admin — promote another account first",
                ));
            }
            Some(guard)
        };
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
                ("usr".into(), self.id.record().into_value()),
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
    /// blob, which needs its own writer ([`User::set_avatar`]). Every
    /// column is nullable, so each argument is an outer/inner `Option`: `None`
    /// = omitted (not written at all), `Some(None)` = cleared, `Some(Some(v))`
    /// = set. Merging the request against the current row is the HTTP layer's
    /// job; the caller authorizes.
    ///
    /// Request-scoped, not merely field-scoped: nothing guards the row across
    /// the handler's read and this write, so re-sending the snapshot's value
    /// for an omitted field would revert a concurrent PATCH of that field —
    /// two profile edits (a name and a phone) used to lose each other. See
    /// [`User::set_role`] for why no writer here touches the whole row.
    // One argument per nullable column is the point: folding them into a struct
    // would just re-spell the HTTP DTO here and cost the compiler's check that
    // every column was considered at the call site.
    #[allow(clippy::too_many_arguments)]
    pub async fn set_profile(
        self,
        name: Option<Option<PersonName>>,
        surname: Option<Option<PersonName>>,
        email: Option<Option<Email>>,
        phone: Option<Option<Phone>>,
        birth_date: Option<Option<BirthDate>>,
        display_name: Option<Option<DisplayName>>,
        bio: Option<Option<Bio>>,
        db: &Database,
    ) -> Result<User, AppError> {
        FieldUpdate::new(self.id.record())
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
    /// Field-scoped for the same reason as [`User::set_profile`]: an avatar
    /// upload must not carry a stale snapshot's role back over an admin's
    /// change. `None` means the row is gone.
    ///
    /// Sent through [`write_with_retry`] like every other single-statement row
    /// write here, unguarded or not: the user row is contended (preferences,
    /// profile, role all write it), and a lost round wrote nothing, so
    /// re-sending it is the recovery rather than a 500 in the caller's face.
    pub async fn set_avatar(
        id: &UserId,
        file: &str,
        content_type: &FileContentType,
        size: i64,
        db: &Database,
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
    /// the blob. Same `RETURN BEFORE` contract as [`User::set_avatar`].
    pub async fn clear_avatar(id: &UserId, db: &Database) -> Result<Option<User>, AppError> {
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
        id: &UserId,
        password_hash: PasswordHash,
        db: &Database,
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
    /// contract as [`User::set_profile`]: `None` = omitted (not written),
    /// `Some(None)` = cleared back to "never chose", `Some(Some(v))` = set.
    pub async fn set_preferences(
        self,
        theme: Option<Option<Theme>>,
        language: Option<Option<Language>>,
        palette_color: Option<Option<PaletteColor>>,
        db: &Database,
    ) -> Result<User, AppError> {
        FieldUpdate::new(self.id.record())
            .set("theme", theme)
            .set("language", language)
            .set("palette_color", palette_color)
            .run::<User>(db)
            .await
    }

    pub async fn find_by_username(username: &str, db: &Database) -> Result<Option<User>, AppError> {
        let mut result = db
            .query("SELECT * FROM user WHERE username = $username LIMIT 1")
            .bind(("username", username.to_string()))
            .await?
            .check()?;
        Ok(result.take::<Vec<User>>(0)?.into_iter().next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_mem;

    /// The board result slot is an *index* into a batch whose length depends on
    /// which arms the role selected, so it is the one number in the cascade a
    /// silent mistake would not show up in the store: pointed at the wrong
    /// statement it yields an empty list, and every whiteboard room whose roster
    /// just changed is simply never told. Assert the rows come back.
    #[tokio::test]
    async fn the_cascade_returns_the_boards_it_stripped() {
        use crate::domain::board::{Board, BoardTitle};

        let db = init_mem().await.unwrap();
        let creator = User::create(
            Username::try_new("ogretmen").unwrap(),
            Password::try_new("secret1").unwrap().hash().unwrap(),
            &db,
        )
        .await
        .unwrap();
        let guest = User::create(
            Username::try_new("ogrenci").unwrap(),
            Password::try_new("secret1").unwrap().hash().unwrap(),
            &db,
        )
        .await
        .unwrap();
        let board = Board::create(
            creator.get_id(),
            BoardTitle::try_new("Geometri").unwrap(),
            vec![guest.get_id().clone()],
            &db,
        )
        .await
        .unwrap();

        // A promotion touches no roster and must report none.
        let (guest, boards) = guest.set_role(Role::Teacher, &db).await.unwrap();
        assert!(
            boards.is_empty(),
            "only a demotion to parent strips rosters"
        );

        let (_, boards) = guest.set_role(Role::Parent, &db).await.unwrap();
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

        let (creator, boards) = creator.set_role(Role::Parent, &db).await.unwrap();
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
        creator.set_role(Role::Parent, &db).await.unwrap();
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
        use crate::domain::appointment::AppointmentReason;
        use crate::domain::appointment_slot::AppointmentSlot;
        use crate::service::appointment;

        let db = init_mem().await.unwrap();
        let staff = |name: &'static str, db: Database| async move {
            a_user(name, &db)
                .await
                .set_role(Role::Teacher, &db)
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

        teacher.clone().set_role(Role::Student, &db).await.unwrap();

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
        assert!(result.take::<Vec<RecordId>>(0).unwrap().is_empty());

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

    #[tokio::test]
    async fn a_stale_profile_write_cannot_revert_a_role_change() {
        let db = init_mem().await.unwrap();
        let user = User::create(
            Username::try_new("aysenur").unwrap(),
            Password::try_new("secret1").unwrap().hash().unwrap(),
            &db,
        )
        .await
        .unwrap();

        // A PATCH /users/me handler reads its snapshot (role = student)...
        let stale = User::read(user.get_id(), &db).await.unwrap().unwrap();

        // ...then an admin's role change commits before the profile save runs.
        User::read(user.get_id(), &db)
            .await
            .unwrap()
            .unwrap()
            .set_role(Role::Teacher, &db)
            .await
            .unwrap();

        // The in-flight save lands from the stale snapshot. It must write only
        // the profile fields — not carry the snapshot's old role back over the
        // admin's change.
        let name = PersonName::try_new("name", "Ayşenur").unwrap();
        stale
            .set_profile(
                Some(Some(name.clone())),
                None,
                None,
                None,
                None,
                None,
                None,
                &db,
            )
            .await
            .unwrap();

        let after = User::read(user.get_id(), &db).await.unwrap().unwrap();
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

    fn png() -> FileContentType {
        FileContentType::try_new("image/png").unwrap()
    }

    async fn a_user(username: &str, db: &Database) -> User {
        User::create(
            Username::try_new(username).unwrap(),
            Password::try_new("secret1").unwrap().hash().unwrap(),
            db,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn an_avatar_upload_cannot_revert_a_role_change() {
        let db = init_mem().await.unwrap();
        let user = a_user("berk", &db).await;

        // The upload handler holds its snapshot (role = student)...
        let stale = User::read(user.get_id(), &db).await.unwrap().unwrap();

        // ...an admin promotes and the person edits their bio, both while the
        // blob is still being written...
        User::read(user.get_id(), &db)
            .await
            .unwrap()
            .unwrap()
            .set_role(Role::Teacher, &db)
            .await
            .unwrap();
        let display_name = DisplayName::try_new("Berk").unwrap();
        User::read(user.get_id(), &db)
            .await
            .unwrap()
            .unwrap()
            .set_profile(
                None,
                None,
                None,
                None,
                None,
                Some(Some(display_name.clone())),
                None,
                &db,
            )
            .await
            .unwrap();

        // ...and only then does the avatar land, from the stale row's id.
        User::set_avatar(stale.get_id(), "blob-1", &png(), 42, &db)
            .await
            .unwrap()
            .expect("the row exists");

        let after = User::read(user.get_id(), &db).await.unwrap().unwrap();
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

        let before = User::set_avatar(user.get_id(), "blob-1", &png(), 10, &db)
            .await
            .unwrap()
            .expect("the row exists");
        assert_eq!(before.get_avatar_file(), None, "no blob to collect yet");

        let before = User::set_avatar(user.get_id(), "blob-2", &png(), 20, &db)
            .await
            .unwrap()
            .expect("the row exists");
        assert_eq!(
            before.get_avatar_file(),
            Some("blob-1"),
            "the replaced blob is what the caller must delete"
        );

        let before = User::clear_avatar(user.get_id(), &db)
            .await
            .unwrap()
            .expect("the row exists");
        assert_eq!(before.get_avatar_file(), Some("blob-2"));

        let after = User::read(user.get_id(), &db).await.unwrap().unwrap();
        assert_eq!(after.get_avatar_file(), None);
        assert_eq!(after.get_avatar_content_type(), None);
        assert_eq!(after.get_avatar_size(), None);

        assert!(
            User::clear_avatar(&UserId::from_key("yok"), &db)
                .await
                .unwrap()
                .is_none(),
            "a missing row is None, not an error"
        );
    }

    #[tokio::test]
    async fn username_newtype_validates() {
        assert_eq!(Username::try_new("ali").unwrap().as_str(), "ali");
        assert!(Username::try_new("").is_err());
        assert!(Username::try_new("ab").is_err());
        assert!(Username::try_new("Ali").is_err());
        assert!(Username::try_new("-ali").is_err());
        assert!(Username::try_new("ali-").is_err());
    }

    #[tokio::test]
    async fn username_is_stored_trimmed() {
        // The unique index and the login lookup key on the *stored* string, so
        // padding must not survive construction — otherwise "ali " and "ali"
        // become two distinct, visually identical accounts.
        assert_eq!(Username::try_new(" ali ").unwrap().as_str(), "ali");
    }

    #[tokio::test]
    async fn short_password_rejected() {
        assert!(Password::try_new("123").is_err());
        assert!(Password::try_new("secret1").is_ok());
    }

    #[tokio::test]
    async fn password_hash_roundtrips() {
        let hash = Password::try_new("secret1").unwrap().hash().unwrap();
        assert!(hash.verify(&Password::try_new("secret1").unwrap()));
        assert!(!hash.verify(&Password::try_new("wrongpw").unwrap()));
    }

    #[tokio::test]
    async fn decoy_hash_is_valid_and_nonmatching() {
        // The decoy must be a real, parseable argon2 hash. If it ever became
        // malformed, verify() would early-return without doing the hashing work
        // and the login timing-equalization would silently disappear.
        assert!(
            decoy_hash().verify(&Password::try_new(DECOY_PASSWORD).unwrap()),
            "decoy hash must verify its own password"
        );
        assert!(!decoy_hash().verify(&Password::try_new("some-other-pw").unwrap()));
    }

    #[tokio::test]
    async fn user_id_key_roundtrips() {
        let id = UserId::generate();
        let key = id.key().to_string();
        assert!(!key.is_empty());
        assert_eq!(UserId::from_key(&key).key(), key);
    }
}
