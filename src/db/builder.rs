//! The builder tables against the **control** database: the account lookup
//! and row insert, and the login sessions keyed by cookie token. The
//! credential newtypes are [`crate::domain::user`]'s; the seed workflow that
//! decides whether an account exists is [`crate::service::builder::ensure`].

use surrealdb::types::RecordId;
use ulid::Ulid;

use crate::constant::SESSION_DURATION_DAYS;
use crate::database::Database;
use crate::domain::builder::{BUILDER_SESSION_TABLE, Builder, BuilderId, BuilderSession};
use crate::domain::session::SessionToken;
use crate::domain::timestamp::Timestamp;
use crate::error::AppError;

pub async fn find_by_username(db: &Database, username: &str) -> Result<Option<Builder>, AppError> {
    let mut result = db
        .query("SELECT * FROM builder WHERE username = $u LIMIT 1")
        .bind(("u", username.trim().to_string()))
        .await?
        .check()?;
    Ok(result.take::<Vec<Builder>>(0)?.into_iter().next())
}

pub async fn read(db: &Database, id: &BuilderId) -> Result<Option<Builder>, AppError> {
    Ok(db.select(id.record()).await?)
}

/// The raw account insert behind [`crate::service::builder::ensure`]'s seed.
pub async fn create(db: &Database, builder: Builder) -> Result<(), AppError> {
    let created: Option<Builder> = db
        .create(builder.get_id().record())
        .content(builder.clone())
        .await?;
    created.ok_or_else(|| AppError::Internal("failed to create the builder account".into()))?;
    Ok(())
}

/// Mint and store one builder login session.
pub async fn create_session(
    db: &Database,
    builder: &BuilderId,
) -> Result<BuilderSession, AppError> {
    let key = RecordId::new(BUILDER_SESSION_TABLE, Ulid::new().to_string());
    let session = BuilderSession {
        id: key.clone(),
        builder: builder.clone(),
        token: SessionToken::generate()?,
        created_at: Timestamp::now(),
        expires_at: Timestamp::in_days(SESSION_DURATION_DAYS),
    };
    let created: Option<BuilderSession> = db.create(key).content(session).await?;
    created.ok_or_else(|| AppError::Internal("failed to create the builder session".into()))
}

pub async fn find_by_token(db: &Database, token: &str) -> Result<Option<BuilderSession>, AppError> {
    let mut result = db
        .query("SELECT * FROM builder_session WHERE token = $tok LIMIT 1")
        .bind(("tok", token.to_string()))
        .await?
        .check()?;
    Ok(result.take::<Vec<BuilderSession>>(0)?.into_iter().next())
}

pub async fn delete_by_token(db: &Database, token: &str) -> Result<(), AppError> {
    db.query("DELETE builder_session WHERE token = $tok")
        .bind(("tok", token.to_string()))
        .await?
        .check()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{create_session, delete_by_token, find_by_token, find_by_username};
    use crate::domain::user::{Password, Username};
    use crate::service::builder;
    use crate::tenant::Tenants;

    #[tokio::test]
    async fn a_builder_session_round_trips_and_deletes() {
        let tenants = Tenants::new_mem().await.unwrap();
        let control = tenants.control();
        builder::ensure(
            control,
            Username::try_new("builder1").unwrap(),
            Password::try_new("secret1").unwrap(),
        )
        .await
        .unwrap();
        let builder = find_by_username(control, "builder1")
            .await
            .unwrap()
            .unwrap();

        let session = create_session(control, builder.get_id()).await.unwrap();
        let token = session.token().as_str().to_string();
        let found = find_by_token(control, &token)
            .await
            .unwrap()
            .expect("found by token");
        assert_eq!(found.builder(), builder.get_id());
        assert!(!found.is_expired());

        delete_by_token(control, &token).await.unwrap();
        assert!(find_by_token(control, &token).await.unwrap().is_none());
    }
}
