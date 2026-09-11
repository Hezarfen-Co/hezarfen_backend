//! The `session` table: opaque login tokens with an expiry, swept on login.

use crate::constant::SESSION_DURATION_DAYS;
use crate::database::Database;
use crate::domain::session::{Session, SessionId, SessionToken};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

pub async fn create(db: &Database, user: &UserId) -> Result<Session, AppError> {
    let session = Session {
        id: SessionId::generate(),
        user: user.clone(),
        token: SessionToken::generate()?,
        expires_at: Timestamp::in_days(SESSION_DURATION_DAYS),
    };
    let created: Option<Session> = db.create(session.id.record()).content(session).await?;
    created.ok_or_else(|| AppError::Internal("failed to create session".into()))
}

pub async fn find_by_token(db: &Database, token: &str) -> Result<Option<Session>, AppError> {
    let mut result = db
        .query("SELECT * FROM session WHERE token = $tok LIMIT 1")
        .bind(("tok", token.to_string()))
        .await?
        .check()?;
    Ok(result.take::<Vec<Session>>(0)?.into_iter().next())
}

pub async fn delete_by_token(db: &Database, token: &str) -> Result<(), AppError> {
    db.query("DELETE session WHERE token = $tok")
        .bind(("tok", token.to_string()))
        .await?
        .check()?;
    Ok(())
}

/// Revoke every session a user holds. The other half of a password reset:
/// the new credential means nothing while a cookie minted under the old one
/// still authenticates.
pub async fn delete_by_user(db: &Database, user: &UserId) -> Result<(), AppError> {
    db.query("DELETE session WHERE user = $usr")
        .bind(("usr", user.record()))
        .await?
        .check()?;
    Ok(())
}

/// Delete every session whose expiry is in the past, returning how many were
/// removed. Expired sessions are already rejected at auth, but nothing else
/// deletes their rows — without this sweep the `session` table grows forever.
pub async fn purge_expired(db: &Database) -> Result<u64, AppError> {
    let now = Timestamp::now().as_millis();
    let mut result = db
        .query("DELETE session WHERE expires_at < $now RETURN BEFORE")
        .bind(("now", now))
        .await?
        .check()?;
    Ok(result.take::<Vec<Session>>(0)?.len() as u64)
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
