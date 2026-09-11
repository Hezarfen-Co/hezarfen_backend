//! Personal-info newtypes shared by every account role — student, teacher,
//! manager, and admin records all carry the same optional contact fields.

use surrealdb::types::SurrealValue;

use crate::constant::{MAX_BIO_LEN, MAX_DISPLAY_NAME_LEN, MAX_NAME_LEN};
use crate::database::Database;
use crate::domain::badge::{BadgeStat, BadgeStats};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_email, validate_optional, validate_phone, validate_required};

/// A person's given or family name. One type serves both fields — the `field`
/// tag only steers the error message ("name …" vs "surname …"). Stored trimmed;
/// unicode is welcome (names are not ASCII).
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct PersonName(String);

impl PersonName {
    pub fn try_new(field: &'static str, value: &str) -> Result<Self, ValidationError> {
        validate_required(field, value, MAX_NAME_LEN)?;
        Ok(Self(value.trim().to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A plausibly-shaped email address (see [`validate_email`]). Stored trimmed.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct Email(String);

impl Email {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_email(value)?;
        Ok(Self(value.trim().to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A phone number: optional leading `+`, 7–15 digits, cosmetic separators
/// allowed (see [`validate_phone`]). Stored trimmed, separators kept as given.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct Phone(String);

impl Phone {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_phone(value)?;
        Ok(Self(value.trim().to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A birth date, canonicalized to `YYYY-MM-DD`. Construction parses the input
/// as a real calendar date (no 2026-02-30) and refuses future dates.
///
/// "Future" is judged against UTC **plus one day of grace**: a birth date is a
/// calendar date on the writer's wall, and a client ahead of UTC (up to
/// UTC+14) legitimately submits a date the server's UTC calendar hasn't
/// reached yet. Without the grace, "born today" entered from Istanbul or
/// Auckland shortly after local midnight is wrongly rejected.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct BirthDate(String);

impl BirthDate {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        let date = chrono::NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d").map_err(|_| {
            ValidationError::Invalid {
                field: "birth_date",
                reason: "must be a calendar date in YYYY-MM-DD form",
            }
        })?;
        let latest_allowed = Timestamp::today_utc()
            .succ_opt()
            .unwrap_or(chrono::NaiveDate::MAX);
        if date > latest_allowed {
            return Err(ValidationError::Invalid {
                field: "birth_date",
                reason: "must not be in the future",
            });
        }
        Ok(Self(date.format("%Y-%m-%d").to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The name a profile shows instead of the legal one — a nickname, a shortened
/// form, whatever the person answers to. Stored trimmed; unicode is welcome,
/// same as [`PersonName`] and unlike the ASCII-only username.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct DisplayName(String);

impl DisplayName {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("display_name", value, MAX_DISPLAY_NAME_LEN)?;
        Ok(Self(value.trim().to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A short self-description on the public profile. Free text within
/// [`MAX_BIO_LEN`], blank allowed — an empty bio is a written-then-erased one,
/// which is a legitimate state and not an error. Stored trimmed.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct Bio(String);

impl Bio {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_optional("bio", value, MAX_BIO_LEN)?;
        Ok(Self(value.trim().to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The motivational counters on a public profile. Every field is a derived
/// count, never a settable one: zero rows is a true zero, so these are `i64`
/// and never `Option` — a `null` in this API means "never chose", which a
/// derived number can't be. An account that predates the feature therefore
/// reads exactly like a fresh one.
///
/// One named struct on purpose: the auto-earned badge rules take a whole
/// `ProfileStats` as their sole input, so a counter added here reaches them
/// without touching a signature.
#[derive(Debug, Clone, SurrealValue)]
pub struct ProfileStats {
    pomodoro_sessions: i64,
    pomodoro_focus_ms: i64,
    courses: i64,
    classes: i64,
    totals: BadgeStats,
}

impl ProfileStats {
    pub fn get_pomodoro_sessions(&self) -> i64 {
        self.pomodoro_sessions
    }

    pub fn get_pomodoro_focus_ms(&self) -> i64 {
        self.pomodoro_focus_ms
    }

    pub fn get_courses(&self) -> i64 {
        self.courses
    }

    pub fn get_classes(&self) -> i64 {
        self.classes
    }

    /// The stored lifetime counters — the same numbers [`crate::domain::badge`]
    /// decides an award from, so a profile read can check the shelf without a
    /// second load of them.
    pub fn get_totals(&self) -> &BadgeStats {
        &self.totals
    }

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
    /// query rather than a [`BadgeStats::load`] call, which would be a second
    /// round trip for one record lookup. It repeats that projection — `?? 0`
    /// per column, because the columns are `option<int>` and a row predating
    /// them carries none — so the two must move together if the shape changes.
    pub async fn load(
        user: &UserId,
        courses: i64,
        classes: i64,
        db: &Database,
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
            // cannot go inverted (see [`crate::db::pomodoro::finish`]); rows written
            // before that guard still can, and this is what covers them.
            pomodoro_focus_ms: row.map_or(0, |totals| totals.focus_ms.max(0)),
            courses,
            classes,
        })
    }
}

/// The aggregate row of [`ProfileStats::load`].
#[derive(Debug, SurrealValue)]
struct PomodoroTotals {
    sessions: i64,
    focus_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn person_name_trims_and_validates() {
        assert_eq!(
            PersonName::try_new("name", "  Ada  ").unwrap().as_str(),
            "Ada"
        );
        // Unicode names must pass — this is not the ASCII-only username rule.
        assert_eq!(
            PersonName::try_new("surname", "Gümüş").unwrap().as_str(),
            "Gümüş"
        );
        assert!(PersonName::try_new("name", "   ").is_err());
        assert!(PersonName::try_new("name", &"x".repeat(101)).is_err());
    }

    #[tokio::test]
    async fn name_errors_carry_the_field_tag() {
        // One newtype serves two fields; the message must still say which one.
        let err = PersonName::try_new("surname", "").unwrap_err();
        assert!(err.to_string().contains("surname"));
    }

    #[tokio::test]
    async fn email_and_phone_store_trimmed() {
        assert_eq!(
            Email::try_new(" ada@example.com ").unwrap().as_str(),
            "ada@example.com"
        );
        assert_eq!(
            Phone::try_new(" +90 555 123 45 67 ").unwrap().as_str(),
            "+90 555 123 45 67"
        );
    }

    #[tokio::test]
    async fn birth_date_canonicalizes() {
        assert_eq!(
            BirthDate::try_new("1990-1-2").unwrap().as_str(),
            "1990-01-02"
        );
        assert!(BirthDate::try_new("1990-02-30").is_err()); // not a real date
        assert!(BirthDate::try_new("02/01/1990").is_err()); // wrong format
        assert!(BirthDate::try_new("9999-01-01").is_err()); // future
        assert!(BirthDate::try_new("").is_err());
    }

    #[tokio::test]
    async fn birth_date_tolerates_clients_ahead_of_utc() {
        let fmt = |d: chrono::NaiveDate| d.format("%Y-%m-%d").to_string();
        let today = Timestamp::today_utc();

        // "Today" for a client in UTC+14 can be the server's UTC tomorrow —
        // that must pass, or "born today" fails near midnight east of UTC.
        assert!(BirthDate::try_new(&fmt(today)).is_ok());
        assert!(BirthDate::try_new(&fmt(today.succ_opt().unwrap())).is_ok());

        // Two days out is beyond any real timezone: still rejected.
        let two_days_out = today.succ_opt().unwrap().succ_opt().unwrap();
        assert!(BirthDate::try_new(&fmt(two_days_out)).is_err());
    }

    #[tokio::test]
    async fn display_name_trims_and_validates() {
        assert_eq!(DisplayName::try_new("  Ada  ").unwrap().as_str(), "Ada");
        // A display name is a name, not a username: unicode passes.
        assert_eq!(
            DisplayName::try_new("Gümüş 🐢").unwrap().as_str(),
            "Gümüş 🐢"
        );
        assert!(DisplayName::try_new("   ").is_err());
        assert!(DisplayName::try_new(&"x".repeat(MAX_DISPLAY_NAME_LEN + 1)).is_err());
    }

    #[tokio::test]
    async fn bio_trims_and_validates() {
        assert_eq!(Bio::try_new("  hello  ").unwrap().as_str(), "hello");
        // Blank is a legitimate bio (erased), unlike a blank display name.
        assert_eq!(Bio::try_new("   ").unwrap().as_str(), "");
        assert!(Bio::try_new(&"é".repeat(MAX_BIO_LEN)).is_ok());
        assert!(Bio::try_new(&"é".repeat(MAX_BIO_LEN + 1)).is_err());
    }

    #[tokio::test]
    async fn stats_are_zero_without_pomodoro_rows() {
        let db = crate::database::init_mem().await.unwrap();
        let user = UserId::from_key(&ulid::Ulid::new().to_string());

        let stats = ProfileStats::load(&user, 3, 1, &db).await.unwrap();
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

        let totals = ProfileStats::load(&user, 0, 0, &db).await.unwrap();
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

        let stats = ProfileStats::load(&user, 0, 0, &db).await.unwrap();
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

        let stats = ProfileStats::load(&user, 0, 0, &db).await.unwrap();
        // The stints still happened, so the count is honest at 2.
        assert_eq!(stats.get_pomodoro_sessions(), 2);
        assert_eq!(stats.get_pomodoro_focus_ms(), 0);
    }
}
