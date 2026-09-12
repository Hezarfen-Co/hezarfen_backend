//! Auto-earned badges: the lifetime counters a badge is decided from, the
//! catalog rule that decides it, and the award rows it writes.
//!
//! Three properties hold here and nowhere else:
//!
//! - **Auto-earned only.** No route awards a badge; [`crate::db::badge::sync`]
//!   is called after
//!   the writes that move a counter, and that is the only way a row appears.
//! - **Permanent.** Nothing in this file deletes or revokes an award. A
//!   counter that later drops below the threshold — a submission withdrawn,
//!   say — leaves the badge standing, because it records that the student did
//!   the thing, not that they still have it.
//! - **Pure rules.** The catalog is [`crate::constant::BADGES`], a hardcoded
//!   table, so [`earned`] is a total function of [`BadgeStats`] with no I/O
//!   and no configuration behind it.

use crate::constant::{
    BADGES, EXAM_SAT_TOTAL_FIELD, HIGH_MARK_TOTAL_FIELD, HOMEWORK_ON_TIME_TOTAL_FIELD,
    HOMEWORK_SUBMITTED_TOTAL_FIELD, LESSONS_ATTENDED_TOTAL_FIELD, LESSONS_HELD_TOTAL_FIELD,
    MARKS_GIVEN_TOTAL_FIELD, POMODORO_FINISHED_TOTAL_FIELD, POMODORO_FOCUS_MS_TOTAL_FIELD,
    POOL_APPROVED_TOTAL_FIELD, POOL_PUBLISHED_TOTAL_FIELD, STUDY_STREAK_LONGEST_FIELD,
};
use crate::domain::timestamp::Timestamp;

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
/// them. Every field is a count and never an `Option`: the columns are
/// `BIGINT NOT NULL DEFAULT 0`, so no rows is a true zero.
/// Not a row shape of its own — it is a projection the user-row queries
/// select into (`SELECT homework_submitted, … FROM app_user WHERE id = $1`).
#[derive(Debug, Clone, Default, sqlx::FromRow)]
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

/// One badge one user has earned, and when — a projection of the `badge_award`
/// row (whose identity is the (user, badge) pair) down to the two served
/// columns.
#[derive(Debug, Clone, sqlx::FromRow)]
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
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn the_rule_takes_every_threshold_at_or_below_the_count() {
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
}
