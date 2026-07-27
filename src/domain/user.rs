use std::sync::OnceLock;

use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::{DECOY_PASSWORD, USER_TABLE};
use crate::database::Database;
use crate::domain::field_update::FieldUpdate;
use crate::domain::page::PagedList;
use crate::domain::preferences::{Language, Theme};
use crate::domain::profile::{BirthDate, Email, PersonName, Phone};
use crate::domain::role::Role;
use crate::domain::text_fold::{search_fold, search_fold_sql};
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_password, validate_username};

/// Typed user record id (`user:<ulid>`).
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct UserId(RecordId);

impl UserId {
    pub fn generate() -> Self {
        Self(RecordId::new(USER_TABLE, Ulid::new().to_string()))
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
}

impl User {
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
    async fn create_with_role(
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
                    username = username.as_str(),
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
                let admin =
                    Self::create_with_role(username, password.hash_async().await?, Role::Admin, db)
                        .await?;
                tracing::info!(username = admin.username.as_str(), "seeded admin account");
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

    /// Case- and diacritic-insensitive fragment search over username, name,
    /// and surname. Needle and columns both go through
    /// [`crate::domain::text_fold`], so `ilker` finds `İLKER` and back —
    /// backs the user pickers. `role` narrows to one role (e.g. only students
    /// for an enroll picker); `None` searches everyone. A blank `query`
    /// matches everyone, so blank + `role` is a role-scoped listing. Returns
    /// every match, ordered by username; the HTTP layer pages the result like
    /// any other list (no built-in cap — an over-broad fragment is windowed
    /// by `?limit`).
    pub async fn search(
        query: &str,
        role: Option<Role>,
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
        list.run(limit, offset, db).await
    }

    /// Overwrite this user's role. The caller is responsible for authorizing it.
    ///
    /// Writes *only* the `role` field (never the whole row): the row mixes
    /// admin-owned (role) and self-service (profile, preferences) fields, and
    /// each writer starts from a snapshot read at request start. A whole-row
    /// write would carry the snapshot's copy of the *other* group back over a
    /// concurrent edit — an in-flight profile save silently reverting an
    /// admin's demotion, or this write erasing a profile edit that raced it.
    /// [`User::set_profile`] and [`User::set_preferences`] are scoped for the
    /// same reason.
    pub async fn set_role(self, role: Role, db: &Database) -> Result<User, AppError> {
        let mut result = db
            .query("UPDATE $id SET role = $role RETURN AFTER")
            .bind(("id", self.id.record()))
            .bind(("role", role))
            .await?
            .check()?;
        result
            .take::<Vec<User>>(0)?
            .into_iter()
            .next()
            .ok_or(AppError::NotFound)
    }

    /// Write the personal-info fields the request actually carried. Every
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
    pub async fn set_profile(
        self,
        name: Option<Option<PersonName>>,
        surname: Option<Option<PersonName>>,
        email: Option<Option<Email>>,
        phone: Option<Option<Phone>>,
        birth_date: Option<Option<BirthDate>>,
        db: &Database,
    ) -> Result<User, AppError> {
        FieldUpdate::new(self.id.record())
            .set("name", name)
            .set("surname", surname)
            .set("email", email)
            .set("phone", phone)
            .set("birth_date", birth_date)
            .run::<User>(db)
            .await
    }

    /// Write the UI-preference fields the request actually carried. Same
    /// contract as [`User::set_profile`]: `None` = omitted (not written),
    /// `Some(None)` = cleared back to "never chose", `Some(Some(v))` = set.
    pub async fn set_preferences(
        self,
        theme: Option<Option<Theme>>,
        language: Option<Option<Language>>,
        db: &Database,
    ) -> Result<User, AppError> {
        FieldUpdate::new(self.id.record())
            .set("theme", theme)
            .set("language", language)
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
            .set_profile(Some(Some(name.clone())), None, None, None, None, &db)
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
