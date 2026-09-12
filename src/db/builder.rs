//! The builder tables against the **control** database: the account lookup
//! and row insert, and the login sessions keyed by cookie token. The
//! credential newtypes are [`crate::domain::user`]'s; the seed workflow that
//! decides whether an account exists is [`crate::service::builder::ensure`].

use crate::constant::SESSION_DURATION_DAYS;
use crate::database::Database;
use crate::domain::builder::{Builder, BuilderId, BuilderSession};
use crate::domain::session::SessionToken;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::{PasswordHash, Username};
use crate::error::AppError;

pub async fn find_by_username(db: &Database, username: &str) -> Result<Option<Builder>, AppError> {
    let builder = sqlx::query_as!(
        Builder,
        r#"SELECT id AS "id: BuilderId",
                  username AS "username: Username",
                  password_hash AS "password_hash: PasswordHash"
           FROM builder WHERE username = $1"#,
        username.trim()
    )
    .fetch_optional(db)
    .await?;
    Ok(builder)
}

pub async fn read(db: &Database, id: &BuilderId) -> Result<Option<Builder>, AppError> {
    let builder = sqlx::query_as!(
        Builder,
        r#"SELECT id AS "id: BuilderId",
                  username AS "username: Username",
                  password_hash AS "password_hash: PasswordHash"
           FROM builder WHERE id = $1"#,
        id.uuid()
    )
    .fetch_optional(db)
    .await?;
    Ok(builder)
}

/// The raw account insert behind [`crate::service::builder::ensure`]'s seed.
/// The unique index on `username` is the whole availability check: a racing
/// second seed is a `23505` on `builder_username`, and the seed's
/// find-then-insert order makes that unreachable in practice — but the
/// constraint stands guard regardless.
pub async fn create(db: &Database, builder: Builder) -> Result<(), AppError> {
    sqlx::query!(
        "INSERT INTO builder (id, username, password_hash) VALUES ($1, $2, $3)",
        builder.id.uuid(),
        builder.username.as_str(),
        builder.password_hash.as_str(),
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Mint and store one builder login session.
pub async fn create_session(
    db: &Database,
    builder: &BuilderId,
) -> Result<BuilderSession, AppError> {
    let token = SessionToken::generate()?;
    let created_at = Timestamp::now();
    let expires_at = Timestamp::in_days(SESSION_DURATION_DAYS);
    let session = sqlx::query_as!(
        BuilderSession,
        r#"INSERT INTO builder_session (id, builder, token, created_at, expires_at)
           VALUES ($1, $2, $3, $4, $5)
           RETURNING id,
                     builder AS "builder: BuilderId",
                     token AS "token: SessionToken",
                     created_at AS "created_at: Timestamp",
                     expires_at AS "expires_at: Timestamp""#,
        crate::domain::monotonic_id::next_uuid(),
        builder.uuid(),
        token.as_str(),
        created_at.as_millis(),
        expires_at.as_millis(),
    )
    .fetch_one(db)
    .await?;
    Ok(session)
}

pub async fn find_by_token(db: &Database, token: &str) -> Result<Option<BuilderSession>, AppError> {
    let session = sqlx::query_as!(
        BuilderSession,
        r#"SELECT id,
                  builder AS "builder: BuilderId",
                  token AS "token: SessionToken",
                  created_at AS "created_at: Timestamp",
                  expires_at AS "expires_at: Timestamp"
           FROM builder_session WHERE token = $1"#,
        token
    )
    .fetch_optional(db)
    .await?;
    Ok(session)
}

pub async fn delete_by_token(db: &Database, token: &str) -> Result<(), AppError> {
    sqlx::query!("DELETE FROM builder_session WHERE token = $1", token)
        .execute(db)
        .await?;
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
        let tenants = crate::database::init_test_tenants().await;
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
