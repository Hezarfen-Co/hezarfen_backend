use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::constant::{
    EXAM_TABLE, MAX_EXAM_DESCRIPTION_LEN, MAX_EXAM_TITLE_LEN, UNLIMITED_EXAM_ATTEMPTS,
};
use crate::database::Database;
use crate::domain::course::CourseId;
use crate::domain::settings::ExamKindDef;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// The one spelling of the re-draft refusal, shared by the handler's
/// pre-flight gate and the in-transaction guard that re-makes it at write
/// time — a client cannot tell which of the two refused.
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
    allow_review: bool,
    // Work-in-progress marker: a draft is visible only to its course's
    // managers, cannot be sat, and cannot be graded. Rows predating the
    // column are backfilled published (`false`).
    draft: bool,
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
        allow_review: bool,
        draft: bool,
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
            allow_review,
            draft,
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
    /// Save the merged exam, but only while the row still reads as the
    /// snapshot the caller merged over — `None` means a concurrent PATCH
    /// landed in between and nothing was written: re-read, re-merge, retry.
    ///
    /// Every column this write replaces is in the guard, which is what makes
    /// the whole-row `CONTENT` save safe without a lock held across the
    /// handler's read: the compare-and-set refuses precisely when that save
    /// would have reverted somebody. `course`/`creator` are not editable and
    /// ride along unchanged. Same shape as
    /// [`crate::domain::settings::Settings::save_if_unchanged`].
    pub async fn update_if_unchanged(
        mut self,
        title: ExamTitle,
        description: ExamDescription,
        kind: ExamKind,
        schedule: ExamSchedule,
        max_attempts: ExamAttemptLimit,
        allow_rejoin: bool,
        allow_review: bool,
        draft: bool,
        db: &Database,
    ) -> Result<Option<Exam>, AppError> {
        // Re-drafting hides an exam: it must be refused while any sitting or
        // mark exists, and that check has to be *in this transaction*. Holding
        // it under a lock outside would only order the two writers inside one
        // process — and it did not even do that, since grading takes the reader
        // lease this write does.
        let redraft = draft && !self.draft;
        let was = (
            self.title.clone(),
            self.description.clone(),
            self.kind.clone(),
            self.mode.clone(),
            self.starts_at,
            self.ends_at,
            self.duration_ms,
            self.max_attempts,
            self.allow_rejoin,
            self.allow_review,
            self.draft,
        );
        self.title = title;
        self.description = description;
        self.kind = kind;
        self.mode = schedule.mode;
        self.starts_at = schedule.starts_at;
        self.ends_at = schedule.ends_at;
        self.duration_ms = schedule.duration_ms;
        self.max_attempts = max_attempts;
        self.allow_rejoin = allow_rejoin;
        self.allow_review = allow_review;
        self.draft = draft;
        // whole-row-save-ok: the WHERE below pins every column this replaces to
        // the caller's snapshot, so no concurrent write can be reverted
        let mut result = db
            .query(
                "BEGIN TRANSACTION;
                 IF $redraft AND (
                     array::len((SELECT VALUE id FROM exam_attempt WHERE exam = $id LIMIT 1)) > 0
                     OR array::len((SELECT VALUE id FROM exam_result WHERE exam = $id LIMIT 1)) > 0
                 ) { THROW 'exam_redraft' };
                 UPDATE $id CONTENT $new
                 WHERE title = $was_title AND description = $was_description
                   AND kind = $was_kind AND mode = $was_mode
                   AND starts_at = $was_starts AND ends_at = $was_ends
                   AND duration_ms = $was_duration
                   AND max_attempts = $was_max_attempts
                   AND allow_rejoin = $was_allow_rejoin
                   AND allow_review = $was_allow_review
                   AND draft = $was_draft
                 RETURN AFTER;
                 COMMIT TRANSACTION;",
            )
            .bind(("redraft", redraft))
            .bind(("id", self.id.record()))
            .bind(("was_title", was.0))
            .bind(("was_description", was.1))
            .bind(("was_kind", was.2))
            .bind(("was_mode", was.3))
            .bind(("was_starts", was.4))
            .bind(("was_ends", was.5))
            .bind(("was_duration", was.6))
            .bind(("was_max_attempts", was.7))
            .bind(("was_allow_rejoin", was.8))
            .bind(("was_allow_review", was.9))
            .bind(("was_draft", was.10))
            .bind(("new", self))
            .await?;
        // An aborted transaction errors every slot; only the THROW's names the
        // marker (the `frozen_check` treatment).
        let mut errors = result.take_errors();
        if errors
            .values()
            .any(|error| error.to_string().contains("exam_redraft"))
        {
            return Err(redraft_error());
        }
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        // BEGIN and the IF take a slot each.
        Ok(result.take::<Vec<Exam>>(2)?.into_iter().next())
    }

    /// Delete the exam and cascade-remove its result, attempt, question,
    /// answer, question-image, and answer-image rows — all in one transaction,
    /// so a failure can't leave an emptied-out exam shell behind. The image
    /// *blobs* (question and answer) are the web layer's to remove — it collects
    /// their names before calling this.
    ///
    /// Bank templates saved out of this exam survive it — they are a separate,
    /// reusable library — so only their `source_exam` provenance link is cleared,
    /// in the same transaction, never left pointing at a dead exam.
    pub async fn delete(self, db: &Database) -> Result<Exam, AppError> {
        let mut result = db
            .query(
                "BEGIN TRANSACTION;
                 DELETE exam_result WHERE exam = $ex;
                 DELETE exam_attempt WHERE exam = $ex;
                 DELETE exam_answer WHERE exam = $ex;
                 DELETE answer_image WHERE exam = $ex;
                 DELETE question_image WHERE exam = $ex;
                 DELETE exam_question WHERE exam = $ex;
                 UPDATE bank_question SET source_exam = NONE WHERE source_exam = $ex;
                 LET $before = (DELETE $ex RETURN BEFORE);
                 RETURN $before;
                 COMMIT TRANSACTION;",
            )
            .bind(("ex", self.id.record()))
            .await?
            .check()?;
        // The deleted row comes back through the transaction's trailing
        // `RETURN`, never a hand-counted slot: the old `take(8)` turned a
        // successful delete into a 404 the moment a cascade statement was
        // inserted above it (it already had to be bumped once). `RETURN` is
        // always the last statement before `COMMIT`, so its slot is derived
        // from the statement count and every insertion above it shifts it
        // along. `num_statements` counts BEGIN and COMMIT too, hence -2.
        let slot = result.num_statements().saturating_sub(2);
        let deleted: Option<Exam> = result.take::<Vec<Exam>>(slot)?.into_iter().next();
        deleted.ok_or(AppError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unscheduled published exam, the minimum this file's write tests need.
    async fn published(db: &Database) -> Exam {
        let allowed: Vec<ExamKindDef> = crate::domain::settings::Settings::defaults()
            .get_exam_kinds()
            .to_vec();
        Exam::create(
            &UserId::generate(),
            &CourseId::generate(),
            ExamTitle::try_new("midterm").unwrap(),
            ExamDescription::try_new("").unwrap(),
            ExamKind::try_new("midterm", &allowed).unwrap(),
            ExamSchedule::try_new(None, None, None, None).unwrap(),
            ExamAttemptLimit::try_new(1).unwrap(),
            true,
            false,
            false,
            db,
        )
        .await
        .unwrap()
    }

    fn edit(exam: &Exam) -> (ExamTitle, ExamDescription, ExamKind, ExamSchedule) {
        (
            exam.get_title().clone(),
            exam.get_description().clone(),
            exam.get_kind().clone(),
            ExamSchedule::try_new(None, None, None, None).unwrap(),
        )
    }

    /// The bite test for the compare-and-set that replaced the writer lease on
    /// `PATCH /exams/{id}`: a merge built on a snapshot the row has moved past
    /// must be refused (the handler then re-reads and re-merges), never
    /// written over somebody else's edit. Asserts the *stored* row.
    #[tokio::test]
    async fn a_merge_built_on_a_stale_snapshot_is_refused() {
        let db = crate::database::init_mem().await.unwrap();
        let stale = published(&db).await;
        let (_, description, kind, schedule) = edit(&stale);
        let landed = stale
            .clone()
            .update_if_unchanged(
                ExamTitle::try_new("theirs").unwrap(),
                description,
                kind,
                schedule,
                ExamAttemptLimit::try_new(1).unwrap(),
                true,
                false,
                false,
                &db,
            )
            .await
            .unwrap();
        assert!(landed.is_some(), "the first write is on a fresh snapshot");

        let (_, description, kind, schedule) = edit(&stale);
        let refused = stale
            .update_if_unchanged(
                ExamTitle::try_new("mine").unwrap(),
                description,
                kind,
                schedule,
                ExamAttemptLimit::try_new(1).unwrap(),
                true,
                false,
                false,
                &db,
            )
            .await
            .unwrap();
        assert!(refused.is_none(), "a stale merge must not be written");
        let stored = Exam::read(landed.unwrap().get_id(), &db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.get_title().as_str(), "theirs");
    }

    /// The bite test for the re-draft guard now living *inside* the update's
    /// transaction: a mark that lands after the handler's pre-flight gate (the
    /// grade path is a lock reader, exactly like this one) must still stop the
    /// exam from being hidden.
    #[tokio::test]
    async fn re_drafting_is_refused_by_the_write_itself_once_a_mark_exists() {
        use crate::domain::exam_result::{ExamResult, Mark};
        let db = crate::database::init_mem().await.unwrap();
        let exam = published(&db).await;
        let student = UserId::generate();
        ExamResult::grade(
            exam.get_id(),
            &student,
            1,
            Mark::try_new(80).unwrap(),
            &UserId::generate(),
            &db,
        )
        .await
        .unwrap();

        let (title, description, kind, schedule) = edit(&exam);
        let refused = exam
            .clone()
            .update_if_unchanged(
                title,
                description,
                kind,
                schedule,
                ExamAttemptLimit::try_new(1).unwrap(),
                true,
                false,
                true,
                &db,
            )
            .await
            .expect_err("a graded exam cannot be re-drafted");
        assert!(refused.to_string().contains("back into a draft"));
        let stored = Exam::read(exam.get_id(), &db).await.unwrap().unwrap();
        assert!(!stored.is_draft(), "nothing may have been written");
    }

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
        // Over-long `dur` in a 1ms window, but the root cause is that sync
        // takes no duration at all — that message wins over the window one.
        assert!(matches!(
            ExamSchedule::try_new(mode("sync"), at(1), at(2), dur),
            Err(ValidationError::Invalid {
                reason: "only async and open exams take a duration",
                ..
            })
        ));

        // Async: window plus a per-student duration, which must fit inside it
        // (equal to the window is fine; the window `at(1), at(2)` above is
        // too narrow for `dur`, so give it one exactly `dur` wide instead).
        assert!(ExamSchedule::try_new(mode("async"), at(1), at(1 + 90 * 60 * 1000), dur).is_ok());
        assert!(ExamSchedule::try_new(mode("async"), at(1), at(2), dur).is_err());
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
