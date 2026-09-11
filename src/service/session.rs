//! Session workflows: mint on login, resolve per request, revoke on logout
//! and on password resets, sweep expired rows. The queries live in
//! [`crate::db::session`].

use crate::database::Database;
use crate::db::session;
use crate::domain::session::Session;
use crate::domain::user::UserId;
use crate::error::AppError;

pub async fn create(db: &Database, user: &UserId) -> Result<Session, AppError> {
    session::create(db, user).await
}

pub async fn find_by_token(db: &Database, token: &str) -> Result<Option<Session>, AppError> {
    session::find_by_token(db, token).await
}

pub async fn delete_by_token(db: &Database, token: &str) -> Result<(), AppError> {
    session::delete_by_token(db, token).await
}

/// Revoke every session a user holds — the other half of a password reset.
pub async fn delete_by_user(db: &Database, user: &UserId) -> Result<(), AppError> {
    session::delete_by_user(db, user).await
}

/// Drop expired rows; best-effort on the login path.
pub async fn purge_expired(db: &Database) -> Result<u64, AppError> {
    session::purge_expired(db).await
}
