//! Badge workflows: the reads and the award sync the web layer calls behind
//! counter-moving writes. The rules live in [`crate::domain::badge`] (pure)
//! and the queries in [`crate::db::badge`]; there is no lock and no
//! orchestration here — a badge is a decoration that never fails the write
//! it decorates, so every caller logs and swallows [`sync`]'s error.

use crate::database::Database;
use crate::db::badge;
use crate::domain::badge::BadgeAward;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Everything `user` has earned, oldest first.
pub async fn list_for(db: &Database, user: &UserId) -> Result<Vec<BadgeAward>, AppError> {
    badge::list_for(db, user).await
}

/// Write an award row for every badge `user` has now earned and does not
/// already hold.
pub async fn sync(db: &Database, user: &UserId) -> Result<(), AppError> {
    badge::sync(db, user).await
}
