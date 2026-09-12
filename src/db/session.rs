//! The `user_session` table: opaque login tokens with an expiry, swept on login.

use crate::constant::SESSION_DURATION_DAYS;
use crate::database::Database;
use crate::domain::session::{Session, SessionId, SessionToken};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

pub async fn create(db: &Database, user: &UserId) -> Result<Session, AppError> {
    let id = SessionId::generate();
    let token = SessionToken::generate()?;
    let expires_at = Timestamp::in_days(SESSION_DURATION_DAYS);
    // One statement is the whole mint: the row that comes back is the row
    // that was stored.
    let session = sqlx::query_as!(
        Session,
        r#"INSERT INTO user_session (id, app_user, token, expires_at)
           VALUES ($1, $2, $3, $4)
           RETURNING id AS "id: SessionId",
                     app_user AS "user: UserId",
                     token AS "token: SessionToken",
                     expires_at AS "expires_at: Timestamp""#,
        id.uuid(),
        user.uuid(),
        token.as_str(),
        expires_at.as_millis(),
    )
    .fetch_one(db)
    .await?;
    Ok(session)
}

pub async fn find_by_token(db: &Database, token: &str) -> Result<Option<Session>, AppError> {
    let session = sqlx::query_as!(
        Session,
        r#"SELECT id AS "id: SessionId",
                  app_user AS "user: UserId",
                  token AS "token: SessionToken",
                  expires_at AS "expires_at: Timestamp"
           FROM user_session WHERE token = $1"#,
        token
    )
    .fetch_optional(db)
    .await?;
    Ok(session)
}

pub async fn delete_by_token(db: &Database, token: &str) -> Result<(), AppError> {
    sqlx::query!("DELETE FROM user_session WHERE token = $1", token)
        .execute(db)
        .await?;
    Ok(())
}

/// Revoke every session a user holds. The other half of a password reset:
/// the new credential means nothing while a cookie minted under the old one
/// still authenticates.
pub async fn delete_by_user(db: &Database, user: &UserId) -> Result<(), AppError> {
    sqlx::query!("DELETE FROM user_session WHERE app_user = $1", user.uuid())
        .execute(db)
        .await?;
    Ok(())
}

/// Delete every session whose expiry is in the past, returning how many were
/// removed. Expired sessions are already rejected at auth, but nothing else
/// deletes their rows — without this sweep the `user_session` table grows
/// forever.
pub async fn purge_expired(db: &Database) -> Result<u64, AppError> {
    let now = Timestamp::now().as_millis();
    let row = sqlx::query!(
        r#"WITH gone AS (
                            DELETE FROM user_session WHERE expires_at < $1 RETURNING 1
                        )
                        SELECT count(*) AS "deleted!: i64" FROM gone"#,
        now
    )
    .fetch_one(db)
    .await?;
    Ok(u64::try_from(row.deleted).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn purge_expired_removes_only_past_sessions() {
        let db = crate::database::init_mem().await.unwrap();
        db.query(
            "CREATE session SET user = type::record('user', 'u'), token = 'past', expires_at = 1;
             CREATE session SET user = type::record('user', 'u'), token = 'future', expires_at = 99999999999999;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

        assert_eq!(purge_expired(&db).await.unwrap(), 1);
        assert!(find_by_token(&db, "past").await.unwrap().is_none());
        assert!(find_by_token(&db, "future").await.unwrap().is_some());
    }
}
