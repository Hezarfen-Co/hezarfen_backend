//! Profile workflows: the aggregate stats read behind a profile response.
//! There is no decision and no lock here — the query itself is the whole
//! story, so this wraps [`crate::db::profile::load`] for the web layer.

use crate::database::Database;
use crate::db::profile;
use crate::domain::profile::ProfileStats;
use crate::domain::user::UserId;
use crate::error::AppError;

/// One round trip for a profile's motivational counters; `courses` and
/// `classes` arrive from the caller's own paged reads.
pub async fn load(
    db: &Database,
    user: &UserId,
    courses: i64,
    classes: i64,
) -> Result<ProfileStats, AppError> {
    profile::load(db, user, courses, classes).await
}
