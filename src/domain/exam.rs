use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{
    EXAM_TABLE, MAX_EXAM_DESCRIPTION_LEN, MAX_EXAM_TITLE_LEN, UNLIMITED_EXAM_ATTEMPTS,
};
use crate::domain::course::CourseId;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::settings::ExamKindDef;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// The one spelling of the re-draft refusal, shared by the update workflow's
/// pre-flight gate ([`crate::service::exam::update`]) and the in-transaction
/// guard that re-makes it at write time — a client cannot tell which of the
/// two refused.
pub(crate) fn redraft_error() -> AppError {
    AppError::Conflict(
        "cannot turn a published exam back into a draft after attempts or results exist",
    )
}
use crate::validate::{
    validate_attempt_limit, validate_exam_duration, validate_exam_mode, validate_optional,
    validate_required,
};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ExamId(RecordId);

impl ExamId {
    /// Minted from the process-wide monotonic generator, not `Ulid::new()`:
    /// exams list `id DESC` (newest first, [`crate::db::exam::list_all`]),
    /// and a random low half scrambles rows minted in the same millisecond.
    pub fn generate() -> Self {
        Self(RecordId::new(EXAM_TABLE, next_ulid().to_string()))
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
///   exam; attempts are rejected),
/// - `sync` → `starts_at` + `ends_at`, no duration (everyone's deadline is
///   `ends_at`),
/// - `async` → `starts_at` + `ends_at` + `duration_ms`, with `duration_ms` no
///   greater than the window (a student who starts at `t` gets until
///   `min(t + duration_ms, ends_at)`),
/// - `open` → no window; `duration_ms` optional (a student who starts at `t`
///   gets until `t + duration_ms`, or forever when absent),
/// - `ends_at` strictly after `starts_at` whenever the window exists.
#[derive(Debug, Clone, Default)]
pub struct ExamSchedule {
    pub(crate) mode: Option<ExamMode>,
    pub(crate) starts_at: Option<Timestamp>,
    pub(crate) ends_at: Option<Timestamp>,
    pub(crate) duration_ms: Option<ExamDuration>,
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
                // Below the match on purpose: a `sync` exam carrying a
                // duration is answered by the match's root cause (it takes no
                // duration at all), not by how the duration compares to the
                // window. Only `async` reaches here with one to compare.
                if let (Some(duration), Some(starts), Some(ends)) =
                    (duration_ms, starts_at, ends_at)
                {
                    let window_ms = ends.as_millis().saturating_sub(starts.as_millis());
                    if duration.as_millis() > window_ms {
                        return Err(invalid(
                            "duration_ms",
                            "must fit within the starts_at..ends_at window",
                        ));
                    }
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
    pub(crate) id: ExamId,
    pub(crate) creator: UserId,
    pub(crate) course: CourseId,
    pub(crate) title: ExamTitle,
    pub(crate) description: ExamDescription,
    pub(crate) kind: ExamKind,
    // The schedule, flattened into columns (SCHEMAFULL keeps them typed).
    // Always written through an `ExamSchedule`, so the invariants above hold
    // for every stored row; pre-schedule rows read back as all-`None`.
    pub(crate) mode: Option<ExamMode>,
    pub(crate) starts_at: Option<Timestamp>,
    pub(crate) ends_at: Option<Timestamp>,
    pub(crate) duration_ms: Option<ExamDuration>,
    // Attempt policy. Rows predating these columns are backfilled by the boot
    // migration (limit 1, rejoin open), so reads never see them missing.
    pub(crate) max_attempts: ExamAttemptLimit,
    pub(crate) allow_rejoin: bool,
    pub(crate) allow_review: bool,
    // Work-in-progress marker: a draft is visible only to its course's
    // managers, cannot be sat, and cannot be graded. Rows predating the
    // column are backfilled published (`false`).
    pub(crate) draft: bool,
    /// How many marks the exam carries — a cap-style counter
    /// ([`crate::db::cap::claim`]
    /// from the grade, decremented by every delete of a mark), absent meaning
    /// zero. Unlike every other counter it is carried *in the struct*, because
    /// the save below is a whole-row `CONTENT` write: a column this type did
    /// not know about would be wiped by the next exam PATCH. Being in the row
    /// is also what makes it useful — the save pins it, so a grade landing
    /// mid-PATCH refuses the save instead of slipping past its gates.
    pub(crate) result_count: Option<i64>,
}

impl Exam {
    pub fn get_id(&self) -> &ExamId {
        &self.id
    }

    /// The marks this exam carries, as the counter reads (absent = none yet).
    pub fn get_result_count(&self) -> i64 {
        self.result_count.unwrap_or(0)
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

    /// Whether students may review their graded attempt once results are out.
    /// The teacher can flip this live.
    pub fn get_allow_review(&self) -> bool {
        self.allow_review
    }

    /// Whether the exam is still being prepared — hidden from students, not
    /// sittable, not gradable, until published.
    pub fn is_draft(&self) -> bool {
        self.draft
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
}
