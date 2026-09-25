//! Pomodoro surfacing: the start/finish pair and the log the timer drives.
//! The queries live in [`crate::db::pomodoro`]; the counting verdict, the
//! lifetime counters, and the study streak are decided inside `finish`'s
//! single transaction there — there is no workflow of this domain's own.

use crate::database::Database;
use crate::db::pomodoro;
pub(crate) use crate::db::pomodoro::PomodoroLogPage;
use crate::domain::pomodoro::PomodoroSession;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Start a session for `user`; `label` is the stint's optional
/// student-supplied name, already validated and trimmed by the web layer.
pub async fn start(
    db: &Database,
    user: &UserId,
    label: Option<String>,
) -> Result<PomodoroSession, AppError> {
    pomodoro::start(db, user, label).await
}

/// Close the running session (`409` when none runs); the returned row
/// carries the `counted` verdict.
pub async fn finish(db: &Database, user: &UserId) -> Result<PomodoroSession, AppError> {
    pomodoro::finish(db, user).await
}

/// A page of `user`'s log, newest first — the running one included — with the
/// half-open `[from, to)` window (unix ms) applied before the count and the
/// focus sum.
pub(crate) async fn page_for_user(
    db: &Database,
    user: &UserId,
    from: Option<i64>,
    to: Option<i64>,
    limit: Option<i64>,
    offset: i64,
) -> Result<PomodoroLogPage, AppError> {
    pomodoro::page_for_user(db, user, from, to, limit, offset).await
}
