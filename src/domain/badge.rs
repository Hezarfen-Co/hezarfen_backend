//! Auto-earned badges: the lifetime counters a badge is decided from, the
//! catalog rule that decides it, and the award rows it writes.
//!
//! Three properties hold here and nowhere else:
//!
//! - **Auto-earned only.** No route awards a badge; [`sync`] is called after
//!   the writes that move a counter, and that is the only way a row appears.
//! - **Permanent.** Nothing in this file deletes or revokes an award. A
//!   counter that later drops below the threshold — a submission withdrawn,
//!   say — leaves the badge standing, because it records that the student did
//!   the thing, not that they still have it.
//! - **Pure rules.** The catalog is [`crate::constant::BADGES`], a hardcoded
//!   table, so [`earned`] is a total function of [`BadgeStats`] with no I/O
//!   and no configuration behind it.

use surrealdb::types::SurrealValue;

use crate::constant::{
    BADGE_AWARD_TABLE, BADGES, EXAM_SAT_TOTAL_FIELD, HIGH_MARK_TOTAL_FIELD,
    HOMEWORK_ON_TIME_TOTAL_FIELD, HOMEWORK_SUBMITTED_TOTAL_FIELD, LESSONS_ATTENDED_TOTAL_FIELD,
    LESSONS_HELD_TOTAL_FIELD, MARKS_GIVEN_TOTAL_FIELD, POMODORO_FINISHED_TOTAL_FIELD,
    POMODORO_FOCUS_MS_TOTAL_FIELD, POOL_APPROVED_TOTAL_FIELD, POOL_PUBLISHED_TOTAL_FIELD,
    STUDY_STREAK_LONGEST_FIELD,
};
use crate::database::Database;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// The counter a badge family reads. One variant per column, and the enum is
/// what ties a catalog line to a real number: a family added here cannot be
/// referenced from `BADGES` until [`BadgeStats`] carries its counter, because
/// both matches below are exhaustive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeStat {
    HomeworkSubmitted,
    HomeworkOnTime,
    ExamSat,
    PomodoroFinished,
    PomodoroFocusMs,
    MarksGiven,
    LessonsHeld,
    PoolApproved,
    PoolPublished,
    LessonsAttended,
    HighMark,
    /// The *longest* run of study days, not the run in progress: the two
    /// bookkeeping columns behind it are not counters and are deliberately not
    /// variants here, because nothing may award a badge off a number that comes
    /// back down.
    StudyStreak,
}

impl BadgeStat {
    /// Where it is *stored*: the column on the user row this counter lives in.
    /// Internal — every caller is a query in this crate, and renaming a column
    /// means changing this and the migration together.
    pub fn field(&self) -> &'static str {
        match self {
            BadgeStat::HomeworkSubmitted => HOMEWORK_SUBMITTED_TOTAL_FIELD,
            BadgeStat::HomeworkOnTime => HOMEWORK_ON_TIME_TOTAL_FIELD,
            BadgeStat::ExamSat => EXAM_SAT_TOTAL_FIELD,
            BadgeStat::PomodoroFinished => POMODORO_FINISHED_TOTAL_FIELD,
            BadgeStat::PomodoroFocusMs => POMODORO_FOCUS_MS_TOTAL_FIELD,
            BadgeStat::MarksGiven => MARKS_GIVEN_TOTAL_FIELD,
            BadgeStat::LessonsHeld => LESSONS_HELD_TOTAL_FIELD,
            BadgeStat::PoolApproved => POOL_APPROVED_TOTAL_FIELD,
            BadgeStat::PoolPublished => POOL_PUBLISHED_TOTAL_FIELD,
            BadgeStat::LessonsAttended => LESSONS_ATTENDED_TOTAL_FIELD,
            BadgeStat::HighMark => HIGH_MARK_TOTAL_FIELD,
            BadgeStat::StudyStreak => STUDY_STREAK_LONGEST_FIELD,
        }
    }

    /// What the *API* calls it: the name `GET /limits` publishes, the same
    /// spelling the badge ids are built from. Deliberately not [`field`] —
    /// these are two names for two audiences, and keeping them apart is what
    /// lets a column be renamed without breaking a published contract. Do not
    /// collapse them.
    ///
    /// [`field`]: BadgeStat::field
    pub fn as_str(&self) -> &'static str {
        match self {
            BadgeStat::HomeworkSubmitted => "homework_submitted",
            BadgeStat::HomeworkOnTime => "homework_on_time",
            BadgeStat::ExamSat => "exam_sat",
            BadgeStat::PomodoroFinished => "pomodoro_finished",
            BadgeStat::PomodoroFocusMs => "pomodoro_focus_ms",
            BadgeStat::MarksGiven => "marks_given",
            BadgeStat::LessonsHeld => "lessons_held",
            BadgeStat::PoolApproved => "pool_approved",
            BadgeStat::PoolPublished => "pool_published",
            BadgeStat::LessonsAttended => "lessons_attended",
            BadgeStat::HighMark => "high_mark",
            BadgeStat::StudyStreak => "study_streak",
        }
    }
}

/// The lifetime counters one user has accumulated, as the badge rules see
/// them. Every field is a count and never an `Option`: no rows is a true zero,
/// and an account that predates the columns reads like a fresh one.
#[derive(Debug, Clone, Default, SurrealValue)]
pub struct BadgeStats {
    homework_submitted: i64,
    homework_on_time: i64,
    exam_sat: i64,
    pomodoro_finished: i64,
    pomodoro_focus_ms: i64,
    marks_given: i64,
    lessons_held: i64,
    pool_approved: i64,
    pool_published: i64,
    lessons_attended: i64,
    high_mark: i64,
    /// The longest study run, off `study_streak_longest`. Named for the wire,
    /// like every field here, and never the run in progress.
    study_streak: i64,
}

impl BadgeStats {
    pub fn get_homework_submitted(&self) -> i64 {
        self.homework_submitted
    }

    pub fn get_homework_on_time(&self) -> i64 {
        self.homework_on_time
    }

    pub fn get_exam_sat(&self) -> i64 {
        self.exam_sat
    }

    pub fn get_pomodoro_finished(&self) -> i64 {
        self.pomodoro_finished
    }

    pub fn get_pomodoro_focus_ms(&self) -> i64 {
        self.pomodoro_focus_ms
    }

    pub fn get_marks_given(&self) -> i64 {
        self.marks_given
    }

    pub fn get_lessons_held(&self) -> i64 {
        self.lessons_held
    }

    pub fn get_pool_approved(&self) -> i64 {
        self.pool_approved
    }

    pub fn get_pool_published(&self) -> i64 {
        self.pool_published
    }

    pub fn get_lessons_attended(&self) -> i64 {
        self.lessons_attended
    }

    pub fn get_high_mark(&self) -> i64 {
        self.high_mark
    }

    pub fn get_study_streak(&self) -> i64 {
        self.study_streak
    }

    /// Every counter off the user row, in one statement — a record lookup, not
    /// a scan.
    ///
    /// Each projection is `(column ?? 0)`, parenthesized because `??` binds
    /// loosely, and it is the whole stale-data story: the columns are
    /// `option<int>`, a row written before they existed carries none of them,
    /// and absent must read as zero rather than as a null or an error. A user
    /// row that is gone entirely reads as all zeros too — a badge sync is not
    /// the place to discover a deleted account.
    pub async fn load(user: &UserId, db: &Database) -> Result<BadgeStats, AppError> {
        let mut result = db
            .query(format!(
                "SELECT ({homework_submitted} ?? 0) AS homework_submitted,
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
                 FROM $usr",
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
        Ok(result
            .take::<Vec<BadgeStats>>(0)?
            .into_iter()
            .next()
            .unwrap_or_default())
    }
}

/// The ids of every badge `stats` has earned — the whole rule, and pure: no
/// database, no clock, no configuration. A badge is earned at `>=` its
/// threshold and stays earned, so this is only ever asked "what should be
/// held", never "what should be taken away".
pub fn earned(stats: &BadgeStats) -> Vec<&'static str> {
    BADGES
        .iter()
        .filter(|(_, stat, threshold)| {
            // Exhaustive on purpose: a new `BadgeStat` variant fails to compile
            // here until the counter behind it exists on `BadgeStats`.
            let value = match stat {
                BadgeStat::HomeworkSubmitted => stats.homework_submitted,
                BadgeStat::HomeworkOnTime => stats.homework_on_time,
                BadgeStat::ExamSat => stats.exam_sat,
                BadgeStat::PomodoroFinished => stats.pomodoro_finished,
                BadgeStat::PomodoroFocusMs => stats.pomodoro_focus_ms,
                BadgeStat::MarksGiven => stats.marks_given,
                BadgeStat::LessonsHeld => stats.lessons_held,
                BadgeStat::PoolApproved => stats.pool_approved,
                BadgeStat::PoolPublished => stats.pool_published,
                BadgeStat::LessonsAttended => stats.lessons_attended,
                BadgeStat::HighMark => stats.high_mark,
                BadgeStat::StudyStreak => stats.study_streak,
            };
            value >= *threshold
        })
        .map(|(id, ..)| *id)
        .collect()
}

/// The record one (user, badge) pair always maps to, so a second [`sync`]
/// resolves onto the row the first one wrote instead of minting a duplicate.
/// A user key is a ULID (alphanumeric), so `_` cannot make two pairs collide.
fn award_key(user: &UserId, badge: &str) -> String {
    format!("{}_{badge}", user.key())
}

/// One badge one user has earned, and when.
#[derive(Debug, Clone, SurrealValue)]
pub struct BadgeAward {
    badge: String,
    earned_at: Timestamp,
}

impl BadgeAward {
    pub fn get_badge(&self) -> &str {
        &self.badge
    }

    pub fn get_earned_at(&self) -> Timestamp {
        self.earned_at
    }

    /// Everything `user` has earned, oldest first.
    ///
    /// Filtered to the ids the catalog still holds: a badge retired from
    /// `BADGES` stops being served without a data migration, and its rows stay
    /// where they are in case the line comes back.
    ///
    /// The badge id breaks the tie, because one sync stamps every badge it
    /// awards with the same millisecond and an arbitrary order there would
    /// shuffle a student's shelf between reads. It is the record id's order
    /// too — the key is `{user}_{badge}` and this reads one user — but spelled
    /// as the projected column on purpose: SurrealDB 3.2.3 refuses to sort by
    /// an idiom the selection does not carry ("Missing order idiom `id` in
    /// statement selection"), and `id` is not a field of an award.
    pub async fn list_for(user: &UserId, db: &Database) -> Result<Vec<BadgeAward>, AppError> {
        let ids: Vec<&str> = BADGES.iter().map(|(id, ..)| *id).collect();
        let mut result = db
            .query(format!(
                "SELECT badge, earned_at FROM {BADGE_AWARD_TABLE}
                 WHERE user = $usr AND badge IN $ids
                 ORDER BY earned_at ASC, badge ASC LIMIT {}",
                BADGES.len()
            ))
            .bind(("usr", user.record()))
            .bind(("ids", ids))
            .await?
            .check()?;
        Ok(result.take::<Vec<BadgeAward>>(0)?)
    }
}

/// Write an award row for every badge `user` has now earned and does not
/// already hold.
///
/// The `WHERE earned_at = NONE` on an `UPSERT` of the pair's own key is the
/// whole of the idempotency: the first call creates the row, and every later
/// call — with a later `$now` — matches a row that already has a stamp and
/// writes nothing at all. So the earned-at a student sees is when they first
/// crossed the line, not when the counter last moved.
///
/// **Callers must log and ignore the error.** A badge is a decoration on top
/// of somebody's homework submission, exam sitting or pomodoro stint, and none
/// of those may fail because the decoration could not be written. The `Result`
/// is returned rather than swallowed here so the caller's log line names the
/// real cause; the next counter bump re-runs this and heals whatever was
/// missed, which is also why a lost write race needs no retry loop.
pub async fn sync(user: &UserId, db: &Database) -> Result<(), AppError> {
    let stats = BadgeStats::load(user, db).await?;
    let earned = earned(&stats);
    if earned.is_empty() {
        return Ok(());
    }
    let mut sql = String::new();
    for index in 0..earned.len() {
        sql.push_str(&format!(
            "UPSERT type::record($table, $key{index}) SET user = $usr, badge = $badge{index},
                 earned_at = $now WHERE earned_at = NONE RETURN AFTER;\n"
        ));
    }
    let mut query = db
        .query(sql)
        .bind(("table", BADGE_AWARD_TABLE))
        .bind(("usr", user.record()))
        .bind(("now", Timestamp::now()));
    for (index, badge) in earned.iter().enumerate() {
        query = query
            .bind((format!("key{index}"), award_key(user, badge)))
            .bind((format!("badge{index}"), badge.to_string()));
    }
    query.await?.check()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::{STUDY_STREAK_CURRENT_FIELD, STUDY_STREAK_LAST_DAY_FIELD};

    /// The schema these rows need, which `src/migration_sql.rs` owns. Spelled
    /// with the same `IF NOT EXISTS` guards the migration uses, so it is a
    /// no-op once the real definitions land next to it.
    async fn schema(db: &Database) {
        db.query(format!(
            "DEFINE FIELD IF NOT EXISTS {HOMEWORK_SUBMITTED_TOTAL_FIELD} ON user TYPE option<int>;
             DEFINE FIELD IF NOT EXISTS {HOMEWORK_ON_TIME_TOTAL_FIELD} ON user TYPE option<int>;
             DEFINE FIELD IF NOT EXISTS {EXAM_SAT_TOTAL_FIELD} ON user TYPE option<int>;
             DEFINE FIELD IF NOT EXISTS {POMODORO_FINISHED_TOTAL_FIELD} ON user TYPE option<int>;
             DEFINE FIELD IF NOT EXISTS {POMODORO_FOCUS_MS_TOTAL_FIELD} ON user TYPE option<int>;
             DEFINE FIELD IF NOT EXISTS {MARKS_GIVEN_TOTAL_FIELD} ON user TYPE option<int>;
             DEFINE FIELD IF NOT EXISTS {LESSONS_HELD_TOTAL_FIELD} ON user TYPE option<int>;
             DEFINE FIELD IF NOT EXISTS {POOL_APPROVED_TOTAL_FIELD} ON user TYPE option<int>;
             DEFINE FIELD IF NOT EXISTS {POOL_PUBLISHED_TOTAL_FIELD} ON user TYPE option<int>;
             DEFINE FIELD IF NOT EXISTS {LESSONS_ATTENDED_TOTAL_FIELD} ON user TYPE option<int>;
             DEFINE FIELD IF NOT EXISTS {HIGH_MARK_TOTAL_FIELD} ON user TYPE option<int>;
             DEFINE FIELD IF NOT EXISTS {STUDY_STREAK_LONGEST_FIELD} ON user TYPE option<int>;
             DEFINE FIELD IF NOT EXISTS {STUDY_STREAK_CURRENT_FIELD} ON user TYPE option<int>;
             DEFINE FIELD IF NOT EXISTS {STUDY_STREAK_LAST_DAY_FIELD} ON user TYPE option<int>;
             DEFINE TABLE IF NOT EXISTS {BADGE_AWARD_TABLE} SCHEMAFULL;
             DEFINE FIELD IF NOT EXISTS user ON {BADGE_AWARD_TABLE} TYPE record<user>;
             DEFINE FIELD IF NOT EXISTS badge ON {BADGE_AWARD_TABLE} TYPE string;
             DEFINE FIELD IF NOT EXISTS earned_at ON {BADGE_AWARD_TABLE} TYPE option<int>;
             DEFINE INDEX IF NOT EXISTS badge_award_user ON {BADGE_AWARD_TABLE} FIELDS user;"
        ))
        .await
        .unwrap()
        .check()
        .unwrap();
    }

    /// A user row carrying `counters`, by column name.
    async fn a_user(counters: &[(&str, i64)], db: &Database) -> UserId {
        let user = UserId::from_key(&ulid::Ulid::new().to_string());
        let sets: Vec<String> = counters
            .iter()
            .map(|(field, value)| format!("{field} = {value}"))
            .collect();
        let extra = if sets.is_empty() {
            String::new()
        } else {
            format!(", {}", sets.join(", "))
        };
        db.query(format!(
            "CREATE $usr SET username = $name, password_hash = 'x'{extra}"
        ))
        .bind(("usr", user.record()))
        .bind(("name", user.key().to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
        user
    }

    /// The stamps on the stored rows, oldest first.
    async fn stamps(user: &UserId, db: &Database) -> Vec<(String, i64)> {
        BadgeAward::list_for(user, db)
            .await
            .unwrap()
            .into_iter()
            .map(|award| (award.badge, award.earned_at.as_millis()))
            .collect()
    }

    /// The wire name and the column name are two names, not one. If a later
    /// edit points `as_str` back at `field`, this fails — which is the point:
    /// the split is what lets a column be renamed without breaking `/limits`.
    #[test]
    fn the_wire_name_is_never_the_column_name() {
        for (stat, wire) in [
            (BadgeStat::HomeworkSubmitted, "homework_submitted"),
            (BadgeStat::HomeworkOnTime, "homework_on_time"),
            (BadgeStat::ExamSat, "exam_sat"),
            (BadgeStat::PomodoroFinished, "pomodoro_finished"),
            (BadgeStat::PomodoroFocusMs, "pomodoro_focus_ms"),
            (BadgeStat::MarksGiven, "marks_given"),
            (BadgeStat::LessonsHeld, "lessons_held"),
            (BadgeStat::PoolApproved, "pool_approved"),
            (BadgeStat::PoolPublished, "pool_published"),
            (BadgeStat::LessonsAttended, "lessons_attended"),
            (BadgeStat::HighMark, "high_mark"),
            (BadgeStat::StudyStreak, "study_streak"),
        ] {
            assert_eq!(stat.as_str(), wire, "the published name is frozen");
            assert_ne!(stat.as_str(), stat.field(), "{stat:?} collapsed the two");
        }
    }

    #[tokio::test]
    async fn a_row_without_the_columns_loads_as_zeros() {
        let db = crate::database::init_mem().await.unwrap();
        schema(&db).await;
        // The stale-data case: an account written before the counters existed
        // carries none of them. And the harsher one — no row at all.
        for user in [a_user(&[], &db).await, UserId::from_key("ghost")] {
            let stats = BadgeStats::load(&user, &db).await.unwrap();
            assert_eq!(stats.get_homework_submitted(), 0);
            assert_eq!(stats.get_homework_on_time(), 0);
            assert_eq!(stats.get_exam_sat(), 0);
            assert_eq!(stats.get_pomodoro_finished(), 0);
            assert_eq!(stats.get_pomodoro_focus_ms(), 0);
            assert_eq!(stats.get_marks_given(), 0);
            assert_eq!(stats.get_lessons_held(), 0);
            assert_eq!(stats.get_pool_approved(), 0);
            assert_eq!(stats.get_pool_published(), 0);
            assert_eq!(stats.get_lessons_attended(), 0);
            assert_eq!(stats.get_high_mark(), 0);
            assert_eq!(stats.get_study_streak(), 0);
            assert!(earned(&stats).is_empty(), "zero earns nothing");
        }
    }

    #[tokio::test]
    async fn the_rule_takes_every_threshold_at_or_below_the_count() {
        let stats = BadgeStats {
            homework_submitted: 10,
            homework_on_time: 9,
            exam_sat: 0,
            pomodoro_finished: 200,
            pomodoro_focus_ms: 36_000_000,
            high_mark: 1,
            // The longest run, so 7 keeps the 3 even once the run breaks.
            study_streak: 7,
            ..BadgeStats::default()
        };
        assert_eq!(
            earned(&stats),
            vec![
                // 10 clears the 1 and the 10, not the 50.
                "homework_submitted_1",
                "homework_submitted_10",
                // 9 clears nothing; 0 exams likewise, so `exam_sat_1` is out.
                "pomodoro_finished_10",
                "pomodoro_finished_50",
                "pomodoro_finished_200",
                // Exactly at the threshold earns it.
                "pomodoro_focus_ms_36000000",
                "high_mark_1",
                "study_streak_3",
                "study_streak_7",
            ]
        );
    }

    #[tokio::test]
    async fn a_second_sync_does_not_move_the_stamp() {
        let db = crate::database::init_mem().await.unwrap();
        schema(&db).await;
        let user = a_user(&[(HOMEWORK_SUBMITTED_TOTAL_FIELD, 1)], &db).await;

        sync(&user, &db).await.unwrap();
        let first = stamps(&user, &db).await;
        assert_eq!(first.len(), 1, "{first:?}");
        assert_eq!(first[0].0, "homework_submitted_1");

        // A later sync, provably at a later millisecond: the row must keep the
        // instant the badge was *first* earned, and must not be doubled.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        sync(&user, &db).await.unwrap();
        assert_eq!(stamps(&user, &db).await, first, "the stamp is frozen");
    }

    #[tokio::test]
    async fn an_award_survives_a_counter_that_falls_back_below_it() {
        let db = crate::database::init_mem().await.unwrap();
        schema(&db).await;
        let user = a_user(&[(HOMEWORK_SUBMITTED_TOTAL_FIELD, 10)], &db).await;
        sync(&user, &db).await.unwrap();
        assert_eq!(
            stamps(&user, &db).await.len(),
            2,
            "1 and 10 are both earned"
        );

        // The submission is withdrawn and the counter comes back down. A badge
        // records that it was done, so nothing here may take it away.
        db.query(format!(
            "UPDATE $usr SET {HOMEWORK_SUBMITTED_TOTAL_FIELD} = 0"
        ))
        .bind(("usr", user.record()))
        .await
        .unwrap()
        .check()
        .unwrap();
        assert!(
            earned(&BadgeStats::load(&user, &db).await.unwrap()).is_empty(),
            "the rule no longer holds…"
        );
        sync(&user, &db).await.unwrap();
        let kept: Vec<String> = stamps(&user, &db)
            .await
            .into_iter()
            .map(|(badge, _)| badge)
            .collect();
        assert_eq!(
            kept,
            vec!["homework_submitted_1", "homework_submitted_10"],
            "…and the awards stand anyway"
        );
    }

    #[tokio::test]
    async fn a_badge_the_catalog_dropped_is_not_served() {
        let db = crate::database::init_mem().await.unwrap();
        schema(&db).await;
        let user = a_user(&[(EXAM_SAT_TOTAL_FIELD, 1)], &db).await;
        sync(&user, &db).await.unwrap();
        // The row a retired catalog line left behind: still stored, never
        // served, and no migration deleted it.
        db.query("CREATE $id SET user = $usr, badge = 'exam_sat_3', earned_at = 1")
            .bind((
                "id",
                surrealdb::types::RecordId::new(BADGE_AWARD_TABLE, award_key(&user, "exam_sat_3")),
            ))
            .bind(("usr", user.record()))
            .await
            .unwrap()
            .check()
            .unwrap();

        let served: Vec<String> = stamps(&user, &db)
            .await
            .into_iter()
            .map(|(badge, _)| badge)
            .collect();
        assert_eq!(served, vec!["exam_sat_1"]);
        let mut rows = db
            .query(format!("SELECT VALUE badge FROM {BADGE_AWARD_TABLE}"))
            .await
            .unwrap();
        assert_eq!(rows.take::<Vec<String>>(0).unwrap().len(), 2, "both stored");
    }

    /// One sync stamps every badge it writes with the same millisecond, so the
    /// tie-break is what makes the order stable rather than incidental.
    #[tokio::test]
    async fn same_millisecond_awards_come_back_in_badge_order() {
        let db = crate::database::init_mem().await.unwrap();
        schema(&db).await;
        let user = a_user(
            &[
                (HOMEWORK_SUBMITTED_TOTAL_FIELD, 50),
                (EXAM_SAT_TOTAL_FIELD, 1),
            ],
            &db,
        )
        .await;
        sync(&user, &db).await.unwrap();

        let served = stamps(&user, &db).await;
        let ids: Vec<&str> = served.iter().map(|(badge, _)| badge.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "exam_sat_1",
                "homework_submitted_1",
                "homework_submitted_10",
                "homework_submitted_50"
            ]
        );
        assert!(
            served.windows(2).all(|pair| pair[0].1 == pair[1].1),
            "one sync stamps one instant: {served:?}"
        );
    }
}
