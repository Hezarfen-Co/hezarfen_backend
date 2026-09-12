//! The profile's motivational stats: the one aggregate read behind a profile
//! response. The counters themselves are [`crate::domain::profile`]'s; the
//! auto-earned badge rules next door are [`crate::domain::badge`] (pure) and
//! [`crate::db::badge`] (reads and writes).

use crate::database::Database;
use crate::domain::badge::BadgeStats;
use crate::domain::profile::ProfileStats;
use crate::domain::user::UserId;
use crate::error::AppError;

/// The aggregate row of [`load`].
#[derive(Debug)]
struct PomodoroTotals {
    sessions: i64,
    focus_ms: i64,
}

/// One round trip, and only one: `courses` and `classes` arrive from the
/// caller, which already holds them as the `total` of the paged class and
/// course readers it ran for the profile's own blocks — re-counting them
/// here would be two extra reads for numbers already in hand.
///
/// The pomodoro aggregate rides the per-user index, so it is a lookup and
/// not a scan. `finished_at IS NOT NULL` is the filter: an open stint
/// contributes to neither the count nor the sum — unfinished focus has no
/// honest duration to add. The Postgres aggregate always answers exactly
/// one row (`count` 0, `sum` NULL on no rows), so the empty school reads as
/// zeros rather than as a missing row; `COALESCE` floors the sum the same
/// way the old `GROUP ALL`-absent case did.
///
/// The lifetime counters ride along as a second statement in the *same*
/// round rather than a [`super::badge::load`] call, which would be a second
/// round trip for one record lookup. It repeats that projection — the
/// `*_total` columns, because they are the badge rules' inputs — so the two
/// must move together if the shape changes.
pub async fn load(
    db: &Database,
    user: &UserId,
    courses: i64,
    classes: i64,
) -> Result<ProfileStats, AppError> {
    let totals = sqlx::query!(
        r#"SELECT count(*) AS "sessions!: i64",
                  COALESCE(sum(finished_at - started_at), 0)::BIGINT AS "focus_ms!: i64"
           FROM pomodoro_session
           WHERE app_user = $1 AND finished_at IS NOT NULL"#,
        user.uuid()
    )
    .fetch_one(db)
    .await?;
    let stats = sqlx::query_as!(
        BadgeStats,
        r#"SELECT homework_submitted_total AS homework_submitted,
                  homework_on_time_total    AS homework_on_time,
                  exam_sat_total            AS exam_sat,
                  pomodoro_finished_total   AS pomodoro_finished,
                  pomodoro_focus_ms_total   AS pomodoro_focus_ms,
                  marks_given_total         AS marks_given,
                  lessons_held_total        AS lessons_held,
                  pool_approved_total       AS pool_approved,
                  pool_published_total      AS pool_published,
                  lessons_attended_total    AS lessons_attended,
                  high_mark_total           AS high_mark,
                  study_streak_longest      AS study_streak
           FROM app_user WHERE id = $1"#,
        user.uuid()
    )
    .fetch_optional(db)
    .await?;
    Ok(ProfileStats {
        totals: stats.unwrap_or_default(),
        pomodoro_sessions: totals.sessions,
        // Floored at zero: a stint stamped by a wall clock that stepped
        // backwards mid-session sums as negative time, and "you focused for
        // minus five seconds" is never the truthful answer. New stints
        // cannot go inverted (see [`PomodoroSession::finish`]); rows written
        // before that guard still can, and this is what covers them.
        pomodoro_focus_ms: totals.focus_ms.max(0),
        courses,
        classes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_test_db;
    use crate::domain::user::{Password, Username};

    async fn a_user(username: &str, db: &Database) -> crate::domain::user::User {
        crate::db::user::create(
            db,
            Username::try_new(username).unwrap(),
            Password::try_new("secret1")
                .unwrap()
                .hash_async()
                .await
                .unwrap(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn an_account_with_no_history_reads_as_zeros() {
        let (db, _leases) = init_test_db().await;
        let user = a_user("profil-bos", &db).await;

        let stats = load(&db, user.get_id(), 3, 1).await.unwrap();
        assert_eq!(stats.pomodoro_sessions, 0);
        assert_eq!(stats.pomodoro_focus_ms, 0);
        assert_eq!(stats.totals.get_homework_submitted(), 0);
        // The caller-supplied counts ride through untouched.
        assert_eq!(stats.courses, 3);
        assert_eq!(stats.classes, 1);
    }

    #[tokio::test]
    async fn only_finished_stints_count_and_an_open_one_is_invisible() {
        let (db, _leases) = init_test_db().await;
        let user = a_user("profil-dolu", &db).await;

        sqlx::query(
            "INSERT INTO pomodoro_session (id, app_user, started_at, finished_at) \
             VALUES ($1, $2, 1000, 3000), ($3, $2, 5000, 6000)",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(user.uuid())
        .bind(uuid::Uuid::now_v7())
        .execute(&db)
        .await
        .unwrap();
        // An open stint: contributes to neither the count nor the sum.
        sqlx::query(
            "INSERT INTO pomodoro_session (id, app_user, started_at) VALUES ($1, $2, 9000)",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(user.uuid())
        .execute(&db)
        .await
        .unwrap();

        let stats = load(&db, user.get_id(), 0, 0).await.unwrap();
        assert_eq!(stats.pomodoro_sessions, 2);
        assert_eq!(stats.pomodoro_focus_ms, 2000 + 1000);
    }

    #[tokio::test]
    async fn a_negative_stint_sum_is_floored_at_zero() {
        let (db, _leases) = init_test_db().await;
        let user = a_user("profil-terstarihi", &db).await;

        // A stint closed by a wall clock that stepped backwards: the sum is
        // negative and "minus five seconds of focus" is never the answer.
        sqlx::query(
            "INSERT INTO pomodoro_session (id, app_user, started_at, finished_at) \
             VALUES ($1, $2, 5000, 1000)",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(user.uuid())
        .execute(&db)
        .await
        .unwrap();

        let stats = load(&db, user.get_id(), 0, 0).await.unwrap();
        assert_eq!(stats.pomodoro_focus_ms, 0);
    }
}
