use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::{MAX_EXAM_DESCRIPTION_LEN, MAX_EXAM_TITLE_LEN, UNLIMITED_EXAM_ATTEMPTS};
use crate::database::{Database, EXAM_TABLE};
use crate::domain::course::CourseId;
use crate::domain::settings::ExamKindDef;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::{
    validate_attempt_limit, validate_exam_duration, validate_exam_mode, validate_optional,
    validate_required,
};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ExamId(RecordId);

impl ExamId {
    pub fn generate() -> Self {
        Self(RecordId::new(EXAM_TABLE, Ulid::new().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(EXAM_TABLE, key))
    }

    pub fn record(&self) -> RecordId {
        self.0.clone()
    }

    pub fn key(&self) -> &str {
        match &self.0.key {
            RecordIdKey::String(key) => key,
            _ => "",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ExamTitle(String);

impl ExamTitle {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("title", value, MAX_EXAM_TITLE_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ExamDescription(String);

impl ExamDescription {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_optional("description", value, MAX_EXAM_DESCRIPTION_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A validated exam kind — one of the school's configured kinds
/// ([`crate::domain::settings::Settings::get_exam_kinds`]). The kind carries
/// the exam's weight in the course average (set per kind in settings, not per
/// exam). Stored exams keep their kind even if the school later edits the
/// list; only new writes are held to the current one.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ExamKind(String);

impl ExamKind {
    pub fn try_new(value: &str, allowed: &[ExamKindDef]) -> Result<Self, ValidationError> {
        if !allowed.iter().any(|kind| kind.get_name() == value) {
            return Err(ValidationError::Invalid {
                field: "kind",
                reason: "is not one of this school's exam kinds (see GET /settings)",
            });
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A validated exam mode: `sync` (everyone sits inside one fixed window),
/// `async` (each student starts inside the window and gets `duration_ms`), or
/// `open` (no window — sit anytime, with an optional per-attempt duration).
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ExamMode(String);

impl ExamMode {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_exam_mode(value)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A validated per-attempt time budget, milliseconds. Required for an `async`
/// exam, optional for an `open` one (absent = unlimited time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, SurrealValue)]
pub struct ExamDuration(i64);

impl ExamDuration {
    pub fn try_new(value: i64) -> Result<Self, ValidationError> {
        validate_exam_duration(value)?;
        Ok(Self(value))
    }

    pub fn as_millis(&self) -> i64 {
        self.0
    }
}

/// How many attempts a student gets at an exam. `0` means unlimited — the
/// same spelling on the wire and in storage; `1` (the default) is the classic
/// single sitting. Editable live: raising it mid-exam grants retakes, and
/// lowering it only blocks *future* starts (existing attempts stand).
#[derive(Debug, Clone, Copy, PartialEq, Eq, SurrealValue)]
pub struct ExamAttemptLimit(i64);

impl ExamAttemptLimit {
    pub fn try_new(value: i64) -> Result<Self, ValidationError> {
        validate_attempt_limit(value)?;
        Ok(Self(value))
    }

    /// The single-sitting default for exams created without a limit.
    pub fn single() -> Self {
        Self(1)
    }

    pub fn as_i64(&self) -> i64 {
        self.0
    }

    pub fn is_unlimited(&self) -> bool {
        self.0 == UNLIMITED_EXAM_ATTEMPTS
    }

    /// Whether a student who already used `used` attempts may start another.
    pub fn allows_another(&self, used: usize) -> bool {
        self.is_unlimited() || (used as i64) < self.0
    }
}

/// The scheduling fields of an exam, validated as a unit — they only make
/// sense together. `try_new` is the sole constructor, so an `ExamSchedule` in
/// hand always satisfies:
///
/// - no mode → no `starts_at`/`ends_at`/`duration_ms` (an offline-graded
///   draft; attempts are rejected),
/// - `sync` → `starts_at` + `ends_at`, no duration (everyone's deadline is
///   `ends_at`),
/// - `async` → `starts_at` + `ends_at` + `duration_ms` (a student who starts
///   at `t` gets until `min(t + duration_ms, ends_at)`),
/// - `open` → no window; `duration_ms` optional (a student who starts at `t`
///   gets until `t + duration_ms`, or forever when absent),
/// - `ends_at` strictly after `starts_at` whenever the window exists.
#[derive(Debug, Clone, Default)]
pub struct ExamSchedule {
    mode: Option<ExamMode>,
    starts_at: Option<Timestamp>,
    ends_at: Option<Timestamp>,
    duration_ms: Option<ExamDuration>,
}

impl ExamSchedule {
    pub fn try_new(
        mode: Option<ExamMode>,
        starts_at: Option<Timestamp>,
        ends_at: Option<Timestamp>,
        duration_ms: Option<ExamDuration>,
    ) -> Result<Self, ValidationError> {
        let invalid = |field, reason| ValidationError::Invalid { field, reason };
        match &mode {
            None => {
                if starts_at.is_some() || ends_at.is_some() || duration_ms.is_some() {
                    return Err(invalid(
                        "mode",
                        "starts_at, ends_at, and duration_ms require a mode (sync, async, or open)",
                    ));
                }
            }
            Some(m) if m.as_str() == "open" => {
                if starts_at.is_some() || ends_at.is_some() {
                    return Err(invalid(
                        "starts_at",
                        "an open exam has no window — drop starts_at/ends_at or pick sync/async",
                    ));
                }
                // `duration_ms` stays optional: limited time per attempt when
                // set, unlimited when absent.
            }
            Some(m) => {
                if starts_at.is_none() {
                    return Err(invalid("starts_at", "required for a scheduled exam"));
                }
                if ends_at.is_none() {
                    return Err(invalid("ends_at", "required for a scheduled exam"));
                }
                if ends_at <= starts_at {
                    return Err(invalid("ends_at", "must be after starts_at"));
                }
                match (m.as_str(), duration_ms.is_some()) {
                    ("async", false) => {
                        return Err(invalid("duration_ms", "required for an async exam"));
                    }
                    ("sync", true) => {
                        return Err(invalid(
                            "duration_ms",
                            "only async and open exams take a duration",
                        ));
                    }
                    _ => {}
                }
            }
        }
        Ok(Self {
            mode,
            starts_at,
            ends_at,
            duration_ms,
        })
    }

    pub fn get_mode(&self) -> Option<&ExamMode> {
        self.mode.as_ref()
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct Exam {
    id: ExamId,
    creator: UserId,
    course: CourseId,
    title: ExamTitle,
    description: ExamDescription,
    kind: ExamKind,
    // The schedule, flattened into columns (SCHEMAFULL keeps them typed).
    // Always written through an `ExamSchedule`, so the invariants above hold
    // for every stored row; pre-schedule rows read back as all-`None`.
    mode: Option<ExamMode>,
    starts_at: Option<Timestamp>,
    ends_at: Option<Timestamp>,
    duration_ms: Option<ExamDuration>,
    // Attempt policy. Rows predating these columns are backfilled by the boot
    // migration (limit 1, rejoin open), so reads never see them missing.
    max_attempts: ExamAttemptLimit,
    allow_rejoin: bool,
}

impl Exam {
    pub fn get_id(&self) -> &ExamId {
        &self.id
    }

    pub fn get_creator(&self) -> &UserId {
        &self.creator
    }

    pub fn get_course(&self) -> &CourseId {
        &self.course
    }

    pub fn get_title(&self) -> &ExamTitle {
        &self.title
    }

    pub fn get_description(&self) -> &ExamDescription {
        &self.description
    }

    pub fn get_kind(&self) -> &ExamKind {
        &self.kind
    }

    pub fn get_mode(&self) -> Option<&ExamMode> {
        self.mode.as_ref()
    }

    pub fn get_starts_at(&self) -> Option<Timestamp> {
        self.starts_at
    }

    pub fn get_ends_at(&self) -> Option<Timestamp> {
        self.ends_at
    }

    pub fn get_duration_ms(&self) -> Option<ExamDuration> {
        self.duration_ms
    }

    pub fn get_max_attempts(&self) -> ExamAttemptLimit {
        self.max_attempts
    }

    /// Whether a student who left the exam room may come back into it (and
    /// keep saving answers). The teacher can flip this live.
    pub fn get_allow_rejoin(&self) -> bool {
        self.allow_rejoin
    }

    /// The stored schedule as the validated bundle (for merge-on-update).
    /// Bypasses `try_new`: the fields were written through an `ExamSchedule`,
    /// so the invariants already hold.
    pub fn schedule(&self) -> ExamSchedule {
        ExamSchedule {
            mode: self.mode.clone(),
            starts_at: self.starts_at,
            ends_at: self.ends_at,
            duration_ms: self.duration_ms,
        }
    }

    pub fn is_creator(&self, user: &UserId) -> bool {
        &self.creator == user
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors the sibling entities' create(field, field, ..) shape"
    )]
    pub async fn create(
        creator: &UserId,
        course: &CourseId,
        title: ExamTitle,
        description: ExamDescription,
        kind: ExamKind,
        schedule: ExamSchedule,
        max_attempts: ExamAttemptLimit,
        allow_rejoin: bool,
        db: &Database,
    ) -> Result<Exam, AppError> {
        let exam = Exam {
            id: ExamId::generate(),
            creator: creator.clone(),
            course: course.clone(),
            title,
            description,
            kind,
            mode: schedule.mode,
            starts_at: schedule.starts_at,
            ends_at: schedule.ends_at,
            duration_ms: schedule.duration_ms,
            max_attempts,
            allow_rejoin,
        };
        let created: Option<Exam> = db.create(exam.id.record()).content(exam).await?;
        created.ok_or_else(|| AppError::Internal("failed to create exam".into()))
    }

    pub async fn read(id: &ExamId, db: &Database) -> Result<Option<Exam>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    pub async fn list_all(db: &Database) -> Result<Vec<Exam>, AppError> {
        let mut result = db
            .query("SELECT * FROM exam ORDER BY id DESC")
            .await?
            .check()?;
        Ok(result.take::<Vec<Exam>>(0)?)
    }

    pub async fn list_for_course(course: &CourseId, db: &Database) -> Result<Vec<Exam>, AppError> {
        let mut result = db
            .query("SELECT * FROM exam WHERE course = $course ORDER BY id DESC")
            .bind(("course", course.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Exam>>(0)?)
    }

    /// Every exam of every course in `courses` (one query) — the catalog as one
    /// user sees it.
    pub async fn list_for_courses(
        courses: &[CourseId],
        db: &Database,
    ) -> Result<Vec<Exam>, AppError> {
        if courses.is_empty() {
            return Ok(Vec::new());
        }
        let records: Vec<RecordId> = courses.iter().map(CourseId::record).collect();
        let mut result = db
            .query("SELECT * FROM exam WHERE course IN $courses ORDER BY id DESC")
            .bind(("courses", records))
            .await?
            .check()?;
        Ok(result.take::<Vec<Exam>>(0)?)
    }

    // `course` is deliberately not updatable — moving an exam between courses
    // would strand results of students not enrolled in the target course.
    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors the sibling entities' update(field, field, ..) shape"
    )]
    pub async fn update(
        mut self,
        title: ExamTitle,
        description: ExamDescription,
        kind: ExamKind,
        schedule: ExamSchedule,
        max_attempts: ExamAttemptLimit,
        allow_rejoin: bool,
        db: &Database,
    ) -> Result<Exam, AppError> {
        self.title = title;
        self.description = description;
        self.kind = kind;
        self.mode = schedule.mode;
        self.starts_at = schedule.starts_at;
        self.ends_at = schedule.ends_at;
        self.duration_ms = schedule.duration_ms;
        self.max_attempts = max_attempts;
        self.allow_rejoin = allow_rejoin;
        let updated: Option<Exam> = db.update(self.id.record()).content(self).await?;
        updated.ok_or(AppError::NotFound)
    }

    /// Delete the exam and cascade-remove its result, attempt, question, and
    /// answer rows — all in one transaction, so a failure can't leave an
    /// emptied-out exam shell behind.
    pub async fn delete(self, db: &Database) -> Result<Exam, AppError> {
        let mut result = db
            .query(
                "BEGIN TRANSACTION;
                 DELETE exam_result WHERE exam = $ex;
                 DELETE exam_attempt WHERE exam = $ex;
                 DELETE exam_answer WHERE exam = $ex;
                 DELETE exam_question WHERE exam = $ex;
                 DELETE $ex RETURN BEFORE;
                 COMMIT TRANSACTION;",
            )
            .bind(("ex", self.id.record()))
            .await?
            .check()?;
        // Statement slots count BEGIN and the child deletes: the exam's own
        // DELETE is slot 5.
        let deleted: Option<Exam> = result.take::<Vec<Exam>>(5)?.into_iter().next();
        deleted.ok_or(AppError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn title_is_required() {
        assert!(ExamTitle::try_new("midterm").is_ok());
        assert!(ExamTitle::try_new("").is_err());
        assert!(ExamTitle::try_new("   ").is_err());
    }

    #[tokio::test]
    async fn description_is_optional() {
        assert!(ExamDescription::try_new("").is_ok());
    }

    #[tokio::test]
    async fn kind_must_be_in_the_allowed_list() {
        let allowed: Vec<ExamKindDef> = crate::domain::settings::Settings::defaults()
            .get_exam_kinds()
            .to_vec();
        for kind in ["homework", "quiz", "midterm", "final", "project", "oral"] {
            assert_eq!(ExamKind::try_new(kind, &allowed).unwrap().as_str(), kind);
        }
        assert!(ExamKind::try_new("essay", &allowed).is_err());
        assert!(ExamKind::try_new("", &allowed).is_err());
        // A school-defined list swaps the acceptance set wholesale.
        let custom = vec![ExamKindDef::try_new("lab", 2).unwrap()];
        assert!(ExamKind::try_new("lab", &custom).is_ok());
        assert!(ExamKind::try_new("midterm", &custom).is_err());
        // Matching is exact, case included — the settings list is the wire
        // truth, not a case-folded suggestion.
        assert!(ExamKind::try_new("Lab", &custom).is_err());
    }

    #[tokio::test]
    async fn mode_must_be_known() {
        for mode in ["sync", "async", "open"] {
            assert_eq!(ExamMode::try_new(mode).unwrap().as_str(), mode);
        }
        assert!(ExamMode::try_new("live").is_err());
    }

    #[tokio::test]
    async fn attempt_limit_counts_or_never_runs_out() {
        let single = ExamAttemptLimit::single();
        assert_eq!(single.as_i64(), 1);
        assert!(!single.is_unlimited());
        assert!(single.allows_another(0));
        assert!(!single.allows_another(1));

        let three = ExamAttemptLimit::try_new(3).unwrap();
        assert!(three.allows_another(2));
        assert!(!three.allows_another(3));
        assert!(!three.allows_another(4));

        let unlimited = ExamAttemptLimit::try_new(0).unwrap();
        assert!(unlimited.is_unlimited());
        assert!(unlimited.allows_another(0));
        assert!(unlimited.allows_another(10_000));

        assert!(ExamAttemptLimit::try_new(-1).is_err());
        assert!(ExamAttemptLimit::try_new(101).is_err());
    }

    #[tokio::test]
    async fn schedule_invariants_hold() {
        let mode = |m| Some(ExamMode::try_new(m).unwrap());
        let at = |ms| Some(Timestamp::from_millis(ms));
        let dur = Some(ExamDuration::try_new(90 * 60 * 1000).unwrap());

        // Unscheduled: nothing set is fine, any time/duration without a mode is not.
        assert!(ExamSchedule::try_new(None, None, None, None).is_ok());
        assert!(ExamSchedule::try_new(None, at(1), None, None).is_err());
        assert!(ExamSchedule::try_new(None, None, at(2), None).is_err());
        assert!(ExamSchedule::try_new(None, None, None, dur).is_err());

        // Sync: fixed window, no duration.
        assert!(ExamSchedule::try_new(mode("sync"), at(1), at(2), None).is_ok());
        assert!(ExamSchedule::try_new(mode("sync"), None, at(2), None).is_err());
        assert!(ExamSchedule::try_new(mode("sync"), at(1), None, None).is_err());
        assert!(ExamSchedule::try_new(mode("sync"), at(1), at(2), dur).is_err());

        // Async: window plus a per-student duration.
        assert!(ExamSchedule::try_new(mode("async"), at(1), at(2), dur).is_ok());
        assert!(ExamSchedule::try_new(mode("async"), at(1), at(2), None).is_err());

        // Open: no window, duration optional (limited or unlimited time).
        assert!(ExamSchedule::try_new(mode("open"), None, None, None).is_ok());
        assert!(ExamSchedule::try_new(mode("open"), None, None, dur).is_ok());
        assert!(ExamSchedule::try_new(mode("open"), at(1), None, None).is_err());
        assert!(ExamSchedule::try_new(mode("open"), None, at(2), None).is_err());
        assert!(ExamSchedule::try_new(mode("open"), at(1), at(2), dur).is_err());

        // The window must be a real interval, whatever the mode.
        assert!(ExamSchedule::try_new(mode("sync"), at(2), at(2), None).is_err());
        assert!(ExamSchedule::try_new(mode("async"), at(3), at(2), dur).is_err());
    }
}
