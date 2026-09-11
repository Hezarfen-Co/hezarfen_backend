//! The profile's motivational stats: the one aggregate read behind a profile
//! response. The counters themselves are [`crate::domain::profile`]'s; the
//! auto-earned badge rules next door are [`crate::domain::badge`] (pure) and
//! [`crate::db::badge`] (reads and writes).

use crate::database::Database;
use crate::domain::badge::{BadgeStat, BadgeStats};
use crate::domain::profile::ProfileStats;
use crate::domain::user::UserId;
use crate::error::AppError;
use surrealdb::types::SurrealValue;

/// One round trip, and only one: `courses` and `classes` arrive from the
/// caller, which already holds them as the `total` of the paged class and
/// course readers it ran for the profile's own blocks — re-counting them
/// here would be two extra reads for numbers already in hand.
///
/// The pomodoro aggregate rides the per-user index, so it is a lookup and
/// not a scan. `finished_at != NONE` is the filter because NONE is falsy: a
/// bare truthiness test would also drop a stint that finished at epoch. An
/// open stint contributes to neither the count nor the sum — unfinished
/// focus has no honest duration to add.
///
/// The lifetime counters ride along as a second statement in the *same*
/// query rather than a [`super::badge::load`] call, which would be a second
/// round trip for one record lookup. It repeats that projection — `?? 0`
/// per column, because the columns are `option<int>` and a row predating
/// them carries none — so the two must move together if the shape changes.
pub async fn load(
    db: &Database,
    user: &UserId,
    courses: i64,
    classes: i64,
) -> Result<ProfileStats, AppError> {
    let mut result = db
        .query(format!(
            "SELECT count() AS sessions, math::sum(finished_at - started_at) AS focus_ms
             FROM pomodoro_session WHERE user = $usr AND finished_at != NONE GROUP ALL;
             SELECT ({homework_submitted} ?? 0) AS homework_submitted,
                    ({homework_on_time} ?? 0) AS homework_on_time,
                    ({exam_sat} ?? 0) AS exam_sat,
                    ({pomodoro_finished} ?? 0) AS pomodoro_finished,
                    ({pomodoro_focus_ms} ?? 0) AS pomodoro_focus_ms,
                    ({marks_given} ?? 0) AS marks_given,
                    ({lessons_held} ?? 0) AS lessons_held,
                    ({pool_approved} ?? 0) AS pool_approved,
                    ({pool_published} ?? 0) AS pool_published,
                    ({lessons_attended} ?? 0) AS lessons_attended,
                    ({high_mark} ?? 0) AS high_mark,
                    ({study_streak} ?? 0) AS study_streak
             FROM $usr;",
            homework_submitted = BadgeStat::HomeworkSubmitted.field(),
            homework_on_time = BadgeStat::HomeworkOnTime.field(),
            exam_sat = BadgeStat::ExamSat.field(),
            pomodoro_finished = BadgeStat::PomodoroFinished.field(),
            pomodoro_focus_ms = BadgeStat::PomodoroFocusMs.field(),
            marks_given = BadgeStat::MarksGiven.field(),
            lessons_held = BadgeStat::LessonsHeld.field(),
            pool_approved = BadgeStat::PoolApproved.field(),
            pool_published = BadgeStat::PoolPublished.field(),
            lessons_attended = BadgeStat::LessonsAttended.field(),
            high_mark = BadgeStat::HighMark.field(),
            study_streak = BadgeStat::StudyStreak.field(),
        ))
        .bind(("usr", user.record()))
        .await?
        .check()?;
    // `GROUP ALL` yields no row at all when nothing matched — that is the
    // zero case, not a missing one.
    let row = result.take::<Vec<PomodoroTotals>>(0)?.into_iter().next();
    // A vanished user row reads as all zeros, same as a fresh account.
    let totals = result
        .take::<Vec<BadgeStats>>(1)?
        .into_iter()
        .next()
        .unwrap_or_default();
    Ok(ProfileStats {
        totals,
        pomodoro_sessions: row.as_ref().map_or(0, |totals| totals.sessions),
        // Floored at zero: a stint stamped by a wall clock that stepped
        // backwards mid-session sums as negative time, and "you focused for
        // minus five seconds" is never the truthful answer. New stints
        // cannot go inverted (see [`PomodoroSession::finish`]); rows written
        // before that guard still can, and this is what covers them.
        pomodoro_focus_ms: row.map_or(0, |totals| totals.focus_ms.max(0)),
        courses,
        classes,
    })
}

/// The aggregate row of [`load`].
#[derive(Debug, SurrealValue)]
struct PomodoroTotals {
    sessions: i64,
    focus_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stats_are_zero_without_pomodoro_rows() {
        let db = crate::database::init_mem().await.unwrap();
        let user = UserId::from_key(&ulid::Ulid::new().to_string());

        let stats = load(&db, &user, 3, 1).await.unwrap();
        // Derived counters: no rows is a true zero, never a null.
        assert_eq!(stats.get_pomodoro_sessions(), 0);
        assert_eq!(stats.get_pomodoro_focus_ms(), 0);
        // The passed-in counts survive untouched.
        assert_eq!(stats.get_courses(), 3);
        assert_eq!(stats.get_classes(), 1);
        // No user row at all — the lifetime counters still read as zeros.
        assert_eq!(stats.get_totals().get_homework_submitted(), 0);
        assert_eq!(stats.get_totals().get_pomodoro_focus_ms(), 0);
    }

    /// The second statement of the same query: the stored counters, straight off
    /// the user row, with the columns an older row is missing read as zero.
    #[tokio::test]
    async fn lifetime_totals_ride_the_same_query() {
        let db = crate::database::init_mem().await.unwrap();
        let user = UserId::from_key(&ulid::Ulid::new().to_string());
        db.query(format!(
            "CREATE $usr SET username = $name, password_hash = 'x',
                 {} = 4, {} = 3",
            crate::constant::HOMEWORK_SUBMITTED_TOTAL_FIELD,
            crate::constant::EXAM_SAT_TOTAL_FIELD,
        ))
        .bind(("usr", user.record()))
        .bind(("name", user.key().to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();

        let totals = load(&db, &user, 0, 0).await.unwrap();
        let totals = totals.get_totals();
        assert_eq!(totals.get_homework_submitted(), 4);
        assert_eq!(totals.get_exam_sat(), 3);
        // Never written on this row: absent must read as 0, not as an error.
        assert_eq!(totals.get_homework_on_time(), 0);
        assert_eq!(totals.get_pomodoro_finished(), 0);
        assert_eq!(totals.get_pomodoro_focus_ms(), 0);
    }

    #[tokio::test]
    async fn stats_count_finished_stints_only() {
        let db = crate::database::init_mem().await.unwrap();
        let user = UserId::from_key(&ulid::Ulid::new().to_string());
        let other = UserId::from_key(&ulid::Ulid::new().to_string());

        // Two finished stints (1000 ms + 2500 ms), one still running, and one
        // finished stint belonging to somebody else.
        db.query(
            "CREATE pomodoro_session CONTENT { user: $usr, started_at: 1000, finished_at: 2000 };
             CREATE pomodoro_session CONTENT { user: $usr, started_at: 5000, finished_at: 7500 };
             CREATE pomodoro_session CONTENT { user: $usr, started_at: 9000 };
             CREATE pomodoro_session CONTENT { user: $oth, started_at: 0, finished_at: 60000 };",
        )
        .bind(("usr", user.record()))
        .bind(("oth", other.record()))
        .await
        .unwrap()
        .check()
        .unwrap();

        let stats = load(&db, &user, 0, 0).await.unwrap();
        assert_eq!(stats.get_pomodoro_sessions(), 2);
        assert_eq!(stats.get_pomodoro_focus_ms(), 3500);
    }

    #[tokio::test]
    async fn focus_time_never_reads_negative() {
        let db = crate::database::init_mem().await.unwrap();
        let user = UserId::from_key(&ulid::Ulid::new().to_string());

        // One honest 1000 ms stint and one inverted row — what a backwards
        // clock step left behind before `finish` grew its floor. The raw sum is
        // -5000; the profile must still answer a duration, not an accusation.
        db.query(
            "CREATE pomodoro_session CONTENT { user: $usr, started_at: 1000, finished_at: 2000 };
             CREATE pomodoro_session CONTENT { user: $usr, started_at: 9000, finished_at: 3000 };",
        )
        .bind(("usr", user.record()))
        .await
        .unwrap()
        .check()
        .unwrap();

        let stats = load(&db, &user, 0, 0).await.unwrap();
        // The stints still happened, so the count is honest at 2.
        assert_eq!(stats.get_pomodoro_sessions(), 2);
        assert_eq!(stats.get_pomodoro_focus_ms(), 0);
    }
}
