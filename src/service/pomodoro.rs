//! Pomodoro surfacing: the start/finish pair and the log the timer drives.
//! The queries live in [`crate::db::pomodoro`]; the counting verdict, the
//! lifetime counters, and the study streak are decided inside `finish`'s
//! single transaction there — there is no workflow of this domain's own.

use crate::database::Database;
use crate::db::pomodoro;
use crate::domain::pomodoro::PomodoroSession;
use crate::domain::user::UserId;
use crate::error::AppError;

pub async fn start(db: &Database, user: &UserId) -> Result<PomodoroSession, AppError> {
    pomodoro::start(db, user).await
}

/// Close the running session (`409` when none runs); the returned row
/// carries the `counted` verdict.
pub async fn finish(db: &Database, user: &UserId) -> Result<PomodoroSession, AppError> {
    pomodoro::finish(db, user).await
}

/// Every session of `user`, newest first — the running one included.
pub async fn list_for_user(db: &Database, user: &UserId) -> Result<Vec<PomodoroSession>, AppError> {
    pomodoro::list_for_user(db, user).await
}
