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
//! - **Pure rules.** The catalog is [`BADGES`], a hardcoded table, so
//!   [`earned`] is a total function of [`BadgeStats`] with no I/O and no
//!   configuration behind it.

use crate::constant::{
    EXAM_SAT_TOTAL_FIELD, HIGH_MARK_TOTAL_FIELD, HOMEWORK_ON_TIME_TOTAL_FIELD,
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

/// The badge catalog: every badge the system can award, as
/// `(id, the counter it reads, the value that earns it)`. Hardcoded on
/// purpose — moving a threshold is a deploy, which is what keeps
/// [`crate::domain::badge::earned`] a pure function of the stats and makes the
/// rules reviewable in a diff instead of editable in a settings row.
///
/// The ids are the API: the frontend maps them to a label and an icon, and an
/// award row stores one forever. So an id is never reused for a different
/// meaning; retiring one is done by deleting the line, and the awards that
/// carry it simply stop being served (no data migration —
/// [`crate::db::badge::list_for`] filters to the live
/// catalog).
pub const BADGES: [(&str, BadgeStat, i64); 34] = [
    ("homework_submitted_1", BadgeStat::HomeworkSubmitted, 1),
    ("homework_submitted_10", BadgeStat::HomeworkSubmitted, 10),
    ("homework_submitted_50", BadgeStat::HomeworkSubmitted, 50),
    ("homework_on_time_10", BadgeStat::HomeworkOnTime, 10),
    ("homework_on_time_25", BadgeStat::HomeworkOnTime, 25),
    ("exam_sat_1", BadgeStat::ExamSat, 1),
    ("exam_sat_10", BadgeStat::ExamSat, 10),
    ("exam_sat_25", BadgeStat::ExamSat, 25),
    ("pomodoro_finished_10", BadgeStat::PomodoroFinished, 10),
    ("pomodoro_finished_50", BadgeStat::PomodoroFinished, 50),
    ("pomodoro_finished_200", BadgeStat::PomodoroFinished, 200),
    // Ten and fifty hours of focus, in the milliseconds the counter stores.
    (
        "pomodoro_focus_ms_36000000",
        BadgeStat::PomodoroFocusMs,
        36_000_000,
    ),
    (
        "pomodoro_focus_ms_180000000",
        BadgeStat::PomodoroFocusMs,
        180_000_000,
    ),
    ("marks_given_10", BadgeStat::MarksGiven, 10),
    ("marks_given_50", BadgeStat::MarksGiven, 50),
    ("marks_given_250", BadgeStat::MarksGiven, 250),
    ("lessons_held_10", BadgeStat::LessonsHeld, 10),
    ("lessons_held_50", BadgeStat::LessonsHeld, 50),
    ("lessons_held_200", BadgeStat::LessonsHeld, 200),
    ("pool_approved_5", BadgeStat::PoolApproved, 5),
    ("pool_approved_25", BadgeStat::PoolApproved, 25),
    ("pool_approved_100", BadgeStat::PoolApproved, 100),
    ("pool_published_1", BadgeStat::PoolPublished, 1),
    ("pool_published_10", BadgeStat::PoolPublished, 10),
    ("pool_published_50", BadgeStat::PoolPublished, 50),
    ("lessons_attended_10", BadgeStat::LessonsAttended, 10),
    ("lessons_attended_50", BadgeStat::LessonsAttended, 50),
    ("lessons_attended_200", BadgeStat::LessonsAttended, 200),
    // Exam marks at or above `HIGH_MARK_MIN`, counted per graded sitting.
    ("high_mark_1", BadgeStat::HighMark, 1),
    ("high_mark_10", BadgeStat::HighMark, 10),
    ("high_mark_25", BadgeStat::HighMark, 25),
    // Consecutive study days, read off the longest run ever held — so these
    // are earned once and never lost when the run breaks.
    ("study_streak_3", BadgeStat::StudyStreak, 3),
    ("study_streak_7", BadgeStat::StudyStreak, 7),
    ("study_streak_30", BadgeStat::StudyStreak, 30),
];

/// The lifetime counters one user has accumulated, as the badge rules see
/// them. Every field is a count and never an `Option`: the columns are
/// `BIGINT NOT NULL DEFAULT 0`, so no rows is a true zero.
/// Not a row shape of its own — it is a projection the user-row queries
/// select into (`SELECT homework_submitted, … FROM app_user WHERE id = $1`).
#[derive(Debug, Clone, Default, sqlx::FromRow)]
pub struct BadgeStats {
    pub(crate) homework_submitted: i64,
    pub(crate) homework_on_time: i64,
    pub(crate) exam_sat: i64,
    pub(crate) pomodoro_finished: i64,
    pub(crate) pomodoro_focus_ms: i64,
    pub(crate) marks_given: i64,
    pub(crate) lessons_held: i64,
    pub(crate) pool_approved: i64,
    pub(crate) pool_published: i64,
    pub(crate) lessons_attended: i64,
    pub(crate) high_mark: i64,
    /// The longest study run, off `study_streak_longest`. Named for the wire,
    /// like every field here, and never the run in progress.
    pub(crate) study_streak: i64,
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
    pub(crate) badge: String,
    pub(crate) earned_at: Timestamp,
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
