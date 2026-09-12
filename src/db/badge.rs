//! The badge counters' reads and the award rows' writes: the lifetime-counter
//! lookup off the user row, the shelf read, and the idempotent sync that
//! mints an award per newly earned badge. The rule deciding *which* badges
//! those are is [`crate::domain::badge::earned`] — pure, no I/O — and the
//! service wrappers the web layer calls live in [`crate::service::badge`].

use sqlx::AssertSqlSafe;

use crate::constant::{BADGE_AWARD_TABLE, BADGES};
use crate::database::Database;
use crate::domain::badge::{BadgeAward, BadgeStats};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Every counter off the user row, in one statement — a record lookup, not
/// a scan.
///
/// The columns are `BIGINT NOT NULL DEFAULT 0`, so each reads as a true zero
/// no matter when the row was written — the old `(column ?? 0)` projection
/// existed because the old store stored absent counters as absent keys. A
/// user row that is gone entirely reads as all zeros too — a badge sync is
/// not the place to discover a deleted account.
pub async fn load(db: &Database, user: &UserId) -> Result<BadgeStats, AppError> {
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
    Ok(stats.unwrap_or_default())
}

/// Everything `user` has earned, oldest first.
///
/// Filtered to the ids the catalog still holds: a badge retired from
/// `BADGES` stops being served without a data migration, and its rows stay
/// where they are in case the line comes back.
///
/// The badge id breaks the tie, because one sync stamps every badge it
/// awards with the same millisecond and an arbitrary order there would
/// shuffle a student's shelf between reads. It is the row's own key order
/// too — the primary key is `(app_user, badge)` and this reads one user —
/// but spelled as the projected column on purpose, because it is the
/// *served* order, not an internal one.
pub async fn list_for(db: &Database, user: &UserId) -> Result<Vec<BadgeAward>, AppError> {
    let ids: Vec<String> = BADGES.iter().map(|(id, ..)| (*id).to_string()).collect();
    let awards = sqlx::query_as!(
        BadgeAward,
        r#"SELECT badge,
                  earned_at AS "earned_at!: Timestamp"
           FROM badge_award
           WHERE app_user = $1 AND badge = ANY($2)
           ORDER BY earned_at ASC, badge ASC
           LIMIT $3"#,
        user.uuid(),
        &ids,
        BADGES.len() as i64,
    )
    .fetch_all(db)
    .await?;
    Ok(awards)
}

/// Write an award row for every badge `user` has now earned and does not
/// already hold.
///
/// The `WHERE earned_at IS NULL` arm of the conflict update is the whole of
/// the idempotency: the first call inserts the row, and every later call —
/// with a later `$now` — conflicts on the pair's primary key and matches a
/// row that already has a stamp, writing nothing at all. So the earned-at a
/// student sees is when they first crossed the line, not when the counter
/// last moved.
///
/// Runtime-built SQL by the named exemption for this sync (the statement
/// count is the earned list's length, decided by the pure rule). Each
/// statement is fully bound — table name is a crate constant, badge ids are
/// catalog constants, nothing client-shaped enters.
///
/// **Callers must log and ignore the error.** A badge is a decoration on top
/// of somebody's homework submission, exam sitting or pomodoro stint, and none
/// of those may fail because the decoration could not be written. The `Result`
/// is returned rather than swallowed here so the caller's log line names the
/// real cause; the next counter bump re-runs this and heals whatever was
/// missed.
pub async fn sync(db: &Database, user: &UserId) -> Result<(), AppError> {
    let stats = load(db, user).await?;
    let earned = crate::domain::badge::earned(&stats);
    if earned.is_empty() {
        return Ok(());
    }
    let now = Timestamp::now().as_millis();
    for badge in earned {
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {BADGE_AWARD_TABLE} (app_user, badge, earned_at) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (app_user, badge) DO UPDATE \
                 SET earned_at = badge_award.earned_at \
               WHERE badge_award.earned_at IS NULL"
        )))
.bind(user.uuid())
        .bind(badge)
        .bind(now)
        .execute(db)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::BADGE_AWARD_TABLE;
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

    async fn count_awards(db: &Database, user: &UserId) -> i64 {
        let (count,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {BADGE_AWARD_TABLE} WHERE app_user = $1"
        )))
        .bind(user.uuid())
        .fetch_one(db)
        .await
        .unwrap();
        count
    }

    #[tokio::test]
    async fn load_defaults_every_counter_to_zero() {
        let (db, _leases) = init_test_db().await;
        let user = a_user("badge-sifir", &db).await;

        let stats = load(&db, user.get_id()).await.unwrap();
        assert_eq!(stats.get_homework_submitted(), 0);
        assert_eq!(stats.get_pomodoro_focus_ms(), 0);
        assert_eq!(stats.get_study_streak(), 0);

        // A row that is gone entirely is all zeros, not an error.
        let ghost = UserId::from_key("00000000-0000-0000-0000-000000000000");
        let stats = load(&db, &ghost).await.unwrap();
        assert_eq!(stats.get_homework_submitted(), 0);
    }

    #[tokio::test]
    async fn sync_is_idempotent_and_keeps_the_first_stamp() {
        let (db, _leases) = init_test_db().await;
        let user = a_user("badge-ali", &db).await;

        // Seed two submitted homeworks: enough for the first badge tier.
        sqlx::query("UPDATE app_user SET homework_submitted_total = 2 WHERE id = $1")
            .bind(user.get_id().uuid())
            .execute(&db)
            .await
            .unwrap();

        sync(&db, user.get_id()).await.unwrap();
        let first = count_awards(&db, user.get_id()).await;
        assert!(first > 0, "at least one badge crossed the line");
        let shelf = list_for(&db, user.get_id()).await.unwrap();
        let earned_at = shelf[0].get_earned_at();
        // A second sync with the counter moved *further* writes nothing: the
        // stamps are from the first crossing, and no row is duplicated.
        sqlx::query("UPDATE app_user SET homework_submitted_total = 5 WHERE id = $1")
            .bind(user.get_id().uuid())
            .execute(&db)
            .await
            .unwrap();
        sync(&db, user.get_id()).await.unwrap();
        assert_eq!(count_awards(&db, user.get_id()).await, first);
        let shelf_again = list_for(&db, user.get_id()).await.unwrap();
        assert_eq!(shelf_again[0].get_earned_at(), earned_at);
        assert_eq!(shelf_again.len(), shelf.len());
        assert_eq!(shelf_again[0].get_badge(), shelf[0].get_badge());

        // The shelf is filtered to badges the catalog still holds, oldest
        // first with the badge id breaking same-millisecond ties.
        for pair in shelf.windows(2) {
            assert!(
                pair[0].get_earned_at().as_millis() <= pair[1].get_earned_at().as_millis(),
                "the shelf is ordered oldest first"
            );
        }
    }

    #[tokio::test]
    async fn sync_writes_nothing_for_a_earned_nothing() {
        let (db, _leases) = init_test_db().await;
        let user = a_user("badge-bos", &db).await;
        sync(&db, user.get_id()).await.unwrap();
        assert_eq!(
            count_awards(&db, user.get_id()).await,
            0,
            "zero counters earn zero badges"
        );
    }
}
