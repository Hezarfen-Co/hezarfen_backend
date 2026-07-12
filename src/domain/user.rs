use std::sync::OnceLock;

use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::database::{Database, USER_TABLE};
use crate::domain::profile::{BirthDate, Email, PersonName, Phone};
use crate::domain::role::Role;
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

/// A validated plaintext password. Never stored — only hashed or verified.
pub struct Password(String);

impl Password {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_password(value)?;
        Ok(Self(value.to_string()))
    }

    pub fn hash(&self) -> Result<PasswordHash, AppError> {
        let mut salt_bytes = [0u8; 16];
        getrandom::fill(&mut salt_bytes).map_err(|e| AppError::Internal(format!("rng: {e}")))?;
        let salt = SaltString::encode_b64(&salt_bytes)
            .map_err(|e| AppError::Internal(format!("salt: {e}")))?;
        let hash = Argon2::default()
            .hash_password(self.0.as_bytes(), &salt)?
            .to_string();
        Ok(PasswordHash(hash))
    }
}

/// An argon2 PHC hash, safe to persist.
#[derive(Debug, Clone, SurrealValue)]
pub struct PasswordHash(String);

/// A fixed password whose hash is the login decoy (see [`PasswordHash::verify_decoy`]).
/// Not a secret — it never matches a real account.
const DECOY_PASSWORD: &str = "decoy-password-not-a-secret";

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

    pub fn verify(&self, password: &Password) -> bool {
        match argon2::password_hash::PasswordHash::new(&self.0) {
            Ok(parsed) => Argon2::default()
                .verify_password(password.0.as_bytes(), &parsed)
                .is_ok(),
            Err(_) => false,
        }
    }

    /// Run a throwaway verification against the decoy hash. Login calls this when
    /// the username is unknown, so a missing user costs the same argon2 time as a
    /// real (wrong-password) check — otherwise the faster reply would let an
    /// attacker enumerate valid usernames by timing.
    pub fn verify_decoy(password: &Password) {
        let _ = decoy_hash().verify(password);
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

    /// Register a new account. New users always start as [`Role::Student`];
    /// elevation is a separate, admin-only action (see [`User::set_role`]).
    pub async fn create(
        username: Username,
        password_hash: PasswordHash,
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
            role: Role::Student,
            name: None,
            surname: None,
            email: None,
            phone: None,
            birth_date: None,
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
                let created = Self::create(username, password.hash()?, db).await?;
                let admin = created.set_role(Role::Admin, db).await?;
                tracing::info!(username = admin.username.as_str(), "seeded admin account");
                Ok(())
            }
        }
    }

    pub async fn read(id: &UserId, db: &Database) -> Result<Option<User>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    pub async fn list_all(db: &Database) -> Result<Vec<User>, AppError> {
        let mut result = db
            .query("SELECT * FROM user ORDER BY id DESC")
            .await?
            .check()?;
        Ok(result.take::<Vec<User>>(0)?)
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

    /// Case-insensitive fragment search over username, name, and surname —
    /// backs the user pickers. `role` narrows to one role (e.g. only students
    /// for an enroll picker); `None` searches everyone. Capped at 10 rows:
    /// pickers show a short list, and the cap keeps an over-broad fragment
    /// from hauling the whole table.
    pub async fn search(
        query: &str,
        role: Option<Role>,
        db: &Database,
    ) -> Result<Vec<User>, AppError> {
        let needle = query.trim().to_lowercase();
        let role_clause = if role.is_some() {
            "AND role = $role"
        } else {
            ""
        };
        let mut query = db
            .query(format!(
                "SELECT * FROM user WHERE \
                   (string::lowercase(username) CONTAINS $q \
                    OR string::lowercase(name ?? '') CONTAINS $q \
                    OR string::lowercase(surname ?? '') CONTAINS $q) \
                   {role_clause} \
                 ORDER BY username LIMIT 10"
            ))
            .bind(("q", needle));
        if let Some(role) = role {
            query = query.bind(("role", role));
        }
        let mut result = query.await?.check()?;
        Ok(result.take::<Vec<User>>(0)?)
    }

    /// Overwrite this user's role. The caller is responsible for authorizing it.
    pub async fn set_role(mut self, role: Role, db: &Database) -> Result<User, AppError> {
        self.role = role;
        let updated: Option<User> = db.update(self.id.record()).content(self).await?;
        updated.ok_or(AppError::NotFound)
    }

    /// Overwrite the personal-info fields wholesale. Each argument is the final
    /// value (`None` clears); merging "keep what wasn't sent" against the
    /// current row is the HTTP layer's job. The caller authorizes.
    pub async fn set_profile(
        mut self,
        name: Option<PersonName>,
        surname: Option<PersonName>,
        email: Option<Email>,
        phone: Option<Phone>,
        birth_date: Option<BirthDate>,
        db: &Database,
    ) -> Result<User, AppError> {
        self.name = name;
        self.surname = surname;
        self.email = email;
        self.phone = phone;
        self.birth_date = birth_date;
        let updated: Option<User> = db.update(self.id.record()).content(self).await?;
        updated.ok_or(AppError::NotFound)
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

    #[tokio::test]
    async fn username_newtype_validates() {
        assert_eq!(Username::try_new("ali").unwrap().as_str(), "ali");
        assert!(Username::try_new("").is_err());
        assert!(Username::try_new("ab").is_err());
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
