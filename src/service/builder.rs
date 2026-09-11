//! Builder workflows: the boot-time seed and the lookups the web layer
//! resolves cookies and logins with. The control-database queries live in
//! [`crate::db::builder`]; the school-provisioning routes never come through
//! here — they speak to [`crate::tenant::Tenants`] directly.

use crate::database::Database;
use crate::db::builder;
use crate::domain::builder::{Builder, BuilderId, BuilderSession};
use crate::domain::user::{Password, Username};
use crate::error::AppError;

/// Idempotent startup seed: guarantee an account with this username in the
/// control database. Missing → created. Already present → nothing to do (the
/// stored password stays whatever it is — the seed never rewrites a
/// credential, so rotating `BUILDER_PASSWORD` in the environment does not
/// silently re-key a live account; delete the row to re-seed). There is no
/// role to promote here, so this cannot have
/// [`crate::service::user::ensure_admin`]'s create-then-promote hole.
pub async fn ensure(db: &Database, username: Username, password: Password) -> Result<(), AppError> {
    if builder::find_by_username(db, username.as_str())
        .await?
        .is_some()
    {
        return Ok(());
    }
    let builder = Builder {
        id: BuilderId::generate(),
        password_hash: password.hash_async().await?,
        username,
    };
    builder::create(db, builder).await?;
    // The account name stays out of the event: exported log records must
    // name no person (see `telemetry`).
    tracing::info!("seeded the builder account named by BUILDER_USERNAME");
    Ok(())
}

/// The login lookup. Usernames are stored trimmed; callers trim the same way
/// so a padded attempt matches the canonical name.
pub async fn find_by_username(db: &Database, username: &str) -> Result<Option<Builder>, AppError> {
    builder::find_by_username(db, username).await
}

pub async fn read(db: &Database, id: &BuilderId) -> Result<Option<Builder>, AppError> {
    builder::read(db, id).await
}

pub async fn create_session(
    db: &Database,
    builder_id: &BuilderId,
) -> Result<BuilderSession, AppError> {
    builder::create_session(db, builder_id).await
}

pub async fn find_by_token(db: &Database, token: &str) -> Result<Option<BuilderSession>, AppError> {
    builder::find_by_token(db, token).await
}

pub async fn delete_by_token(db: &Database, token: &str) -> Result<(), AppError> {
    builder::delete_by_token(db, token).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenant::Tenants;

    #[tokio::test]
    async fn the_seed_is_idempotent_and_never_rewrites_a_credential() {
        let tenants = Tenants::new_mem().await.unwrap();
        let control = tenants.control();
        let name = Username::try_new("builder1").unwrap();
        ensure(control, name.clone(), Password::try_new("secret1").unwrap())
            .await
            .unwrap();
        let first = find_by_username(control, "builder1")
            .await
            .unwrap()
            .expect("seeded");

        // A second boot with a *different* password must leave the row alone:
        // an environment edit is not a password reset.
        ensure(control, name, Password::try_new("secret2").unwrap())
            .await
            .unwrap();
        let again = find_by_username(control, "builder1")
            .await
            .unwrap()
            .expect("still there");
        assert_eq!(again.get_id(), first.get_id());
        assert_eq!(
            again.get_password_hash().as_str(),
            first.get_password_hash().as_str()
        );
    }
}
