//! Pure user shapes: the typed id, the credential newtypes (username,
//! password, argon2 hash — with their blocking-pool discipline), and the row
//! itself. Persistence lives in [`crate::db::user`]; the admin seed and the
//! role change in [`crate::service::user`].

use std::sync::OnceLock;

use argon2::password_hash::phc::PasswordHash as PhcHash;
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{AI_PRINCIPAL_KEY, DECOY_PASSWORD, USER_TABLE};
use crate::domain::monotonic_id::next_ulid;
use crate::domain::note_file::FileContentType;
use crate::domain::preferences::{Language, PaletteColor, Theme};
use crate::domain::profile::{Bio, BirthDate, DisplayName, Email, PersonName, Phone};
use crate::domain::role::Role;
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_password, validate_username};

/// Typed user record id (`user:<ulid>`).
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct UserId(RecordId);

impl UserId {
    /// Minted from the process-wide monotonic generator, not `Ulid::generate()`:
    /// users list `id DESC` (newest first, [`crate::db::user::list_all`]) and page by
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
        // password-hash 0.6 generates the salt internally (its own CSPRNG);
        // the explicit SaltString step is gone.
        let hash = Argon2::default().hash_password(self.0.as_bytes())?.to_string();
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
        match PhcHash::new(&self.0) {
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
/// Fields are crate-visible: [`crate::db::user`] mints rows and the role
/// cascade reads the id, exactly as the other split domains do.
pub struct User {
    pub(crate) id: UserId,
    pub(crate) username: Username,
    pub(crate) password_hash: PasswordHash,
    pub(crate) role: Role,
    // Personal info, identical for every role. All optional: accounts are
    // created from bare credentials and filled in later, and rows from before
    // these fields existed simply read back as `None`.
    pub(crate) name: Option<PersonName>,
    pub(crate) surname: Option<PersonName>,
    pub(crate) email: Option<Email>,
    pub(crate) phone: Option<Phone>,
    pub(crate) birth_date: Option<BirthDate>,
    // Frontend UI preferences. Optional like the personal info: `None` means
    // "never chose", which the frontend renders as the device preference.
    pub(crate) theme: Option<Theme>,
    pub(crate) language: Option<Language>,
    pub(crate) palette_color: Option<PaletteColor>,
    // The public profile: what other people see instead of the credentials.
    // Optional like everything above — a profile is written after the account
    // exists, so rows from before these fields existed read back as `None`,
    // which is exactly a fresh account that never filled one in. The avatar is
    // a blob on disk (`FILES_PATH`) like every other upload; only its name,
    // type and size live on the row.
    pub(crate) display_name: Option<DisplayName>,
    pub(crate) bio: Option<Bio>,
    pub(crate) avatar_file: Option<String>,
    pub(crate) avatar_content_type: Option<FileContentType>,
    pub(crate) avatar_size: Option<i64>,
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

}

#[cfg(test)]
mod tests {
    use super::*;

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
