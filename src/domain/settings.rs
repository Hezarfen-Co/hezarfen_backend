//! School-adjustable policy: which exam kinds exist (and how much each
//! weighs in course averages), which attendance statuses the roll call
//! accepts, how numeric marks display as grades, and how large an uploaded
//! note file may be.
//!
//! One singleton record (`settings:school` — one school per deployment). An
//! absent record means "the defaults from `constant.rs`", so a fresh or
//! pre-existing database needs no seeding and behaves exactly as before. The
//! same holds per field: `max_file_bytes` is `option<int>` and reads as the
//! default while unset, so rows saved before the field existed keep working
//! without a backfill (backfills crash boots — `DEFAULT` never rescues
//! existing rows, and `UPDATE` re-validates whole records).

use surrealdb::types::{RecordId, SurrealValue};
use tokio::sync::Mutex;

use crate::constant::{
    DEFAULT_ATTENDANCE_STATUSES, DEFAULT_CHATBOT_HISTORY_TURNS, DEFAULT_DIETARY_TAGS,
    DEFAULT_EXAM_KINDS, DEFAULT_MAX_CHATBOT_MESSAGE_LEN, DEFAULT_MAX_CHATBOT_THREADS,
    DEFAULT_MAX_FILE_BYTES, DEFAULT_MEAL_SLOTS, MAX_CHATBOT_HISTORY_TURNS, MAX_EXAM_KIND_WEIGHT,
    MAX_GRADE_BANDS, MAX_GRADE_LABEL_LEN, MAX_MARK, MAX_MAX_CHATBOT_MESSAGE_LEN,
    MAX_MAX_CHATBOT_THREADS, MAX_MAX_FILE_BYTES, MAX_MEAL_CANCEL_CUTOFF_MINUTES,
    MAX_MEAL_SERVING_MINUTE, MAX_SETTINGS_ITEM_LEN, MAX_SETTINGS_LIST_LEN,
    MIN_CHATBOT_HISTORY_TURNS, MIN_EXAM_KIND_WEIGHT, MIN_MARK, MIN_MAX_CHATBOT_MESSAGE_LEN,
    MIN_MAX_CHATBOT_THREADS, MIN_MAX_FILE_BYTES, SETTINGS_KEY, SETTINGS_TABLE,
};
use crate::database::Database;
use crate::domain::text_fold;
use crate::error::{AppError, ValidationError};

/// One settings edit at a time, over the whole process.
///
/// A list edit is a **pair** of writes, and the pair is the guard: every name
/// the edit drops is retired on its reference counter first (which refuses
/// every later claim), and only then is the list itself committed with a
/// compare-and-set against the snapshot the removals were judged from. Neither
/// write can be made to cover the other. Both retirement and un-retirement are
/// *idempotent*, so a rival that decided the same removal against the same
/// snapshot is told "already retired" and records no rollback — and when it is
/// that rival's save that wins the compare-and-set, the attempt which really
/// flipped the bit rolls it back, leaving the name off the stored list with a
/// counter reading "in service": gradable again, and unremovable for good once
/// a mark lands. No per-name bit can close that, because `Unchanged` has
/// erased which attempt owns the flip.
///
/// So the pair is serialized instead. Every writer of the singleton goes
/// through `PATCH /settings`, and the deployment runs one process by contract
/// (stop-the-world upgrades), so process-wide is deployment-wide here — the
/// same argument [`crate::domain::cap`]'s own lock makes for `retire_name`'s
/// two statements, one level up. The compare-and-set stays: it is what keeps a
/// crashed or rolled-back attempt from writing a list nobody merged.
///
/// **Lock order:** `SETTINGS_LOCK` → `cap`'s `CLAIM_LOCK`, never the reverse —
/// the retirements are taken while this is held.
pub(crate) static SETTINGS_LOCK: Mutex<()> = Mutex::const_new(());

/// One exam kind the school runs (`"midterm"`, `"oral"`, …) with its weight:
/// how many times an exam of that kind counts into its course's average.
/// Weight lives here, not on the exam — reweighting a kind reweights every
/// exam of that kind at once.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct ExamKindDef {
    name: String,
    weight: i64,
}

impl ExamKindDef {
    pub fn try_new(name: &str, weight: i64) -> Result<Self, ValidationError> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > MAX_SETTINGS_ITEM_LEN {
            return Err(ValidationError::Invalid {
                field: "exam_kinds",
                reason: "kind names must be 1 to 50 characters",
            });
        }
        if !(MIN_EXAM_KIND_WEIGHT..=MAX_EXAM_KIND_WEIGHT).contains(&weight) {
            return Err(ValidationError::Invalid {
                field: "exam_kinds",
                reason: "kind weights must be between 1 and 100",
            });
        }
        Ok(Self {
            name: name.to_string(),
            weight,
        })
    }

    pub fn get_name(&self) -> &str {
        &self.name
    }

    pub fn get_weight(&self) -> i64 {
        self.weight
    }
}

/// One meal slot the school serves (`"lunch"`, `"snack"`, …). A published menu
/// snapshots the slot's *name* as text, so retiring a slot never rewrites a
/// past menu — same contract exam kinds have with exams. An object, not a bare
/// string, which is what let the serving time land here without a migration.
///
/// `serving_minute` is minutes past midnight **UTC** on the menu's date, and
/// it is what the booking/cancel cutoff counts back from. The backend stores
/// no school timezone (deliberately), so staff enter UTC: a UTC+3 school sets
/// `540` (09:00) for a meal served at noon locally. `None` — every slot
/// written before the field existed — falls back to midnight UTC, the old
/// behaviour.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct MealSlotDef {
    name: String,
    serving_minute: Option<i64>,
}

impl MealSlotDef {
    pub fn try_new(name: &str, serving_minute: Option<i64>) -> Result<Self, ValidationError> {
        Self::build(name, serving_minute, false)
    }

    /// [`Self::try_new`] for a name the stored list *already* carries.
    ///
    /// The URL-safety rule below is younger than the lists it validates, and
    /// `PATCH /settings` re-validates the whole submitted list — so a school
    /// that stored a slot named `a/b` before the rule existed could never edit
    /// `meal_slots` again: re-sending the name is a 400, and dropping it is a
    /// 409 the moment a menu was published under it. Grandfathering a name the
    /// row already holds unwedges the rest of the list while leaving the rule
    /// in full force for every *new* name — the stored one is exactly as
    /// unusable as it already was, and dropping it stays the only way out.
    pub fn try_kept(name: &str, serving_minute: Option<i64>) -> Result<Self, ValidationError> {
        Self::build(name, serving_minute, true)
    }

    fn build(name: &str, serving_minute: Option<i64>, kept: bool) -> Result<Self, ValidationError> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > MAX_SETTINGS_ITEM_LEN {
            return Err(ValidationError::Invalid {
                field: "meal_slots",
                reason: "slot names must be 1 to 50 characters",
            });
        }
        // The name goes verbatim into a menu's record id, and that id is a URL
        // path segment: a slot named `a/b` would define a slot no menu can ever
        // be published under (`MenuSlot::try_new` refuses the same characters,
        // at the one place a name becomes an id). Refusing it here too is what
        // keeps the settings from offering a slot the canteen cannot use.
        if !kept
            && name
                .chars()
                .any(|c| matches!(c, '/' | '\\' | '?' | '#' | '%'))
        {
            return Err(ValidationError::Invalid {
                field: "meal_slots",
                reason: "meal slot names used for menus must not contain / \\ ? # or %",
            });
        }
        if let Some(minute) = serving_minute {
            in_range(
                "meal_slots",
                minute,
                0..=MAX_MEAL_SERVING_MINUTE,
                "serving minutes must be between 0 (00:00 UTC) and 1439 (23:59 UTC)",
            )?;
        }
        Ok(Self {
            name: name.to_string(),
            serving_minute,
        })
    }

    pub fn get_name(&self) -> &str {
        &self.name
    }

    /// Minutes past midnight UTC at which this slot is served; `None` = unset,
    /// so the cutoff counts back from midnight UTC instead.
    pub fn get_serving_minute(&self) -> Option<i64> {
        self.serving_minute
    }
}

/// One grade-display band: marks at or above `min` (and below the next band's
/// `min`) render as `label`. Display only — storage and averaging stay numeric.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct GradeBand {
    min: i64,
    label: String,
}

impl GradeBand {
    pub fn try_new(min: i64, label: &str) -> Result<Self, ValidationError> {
        if !(MIN_MARK..=MAX_MARK).contains(&min) {
            return Err(ValidationError::Invalid {
                field: "grade_bands",
                reason: "band mins must be between 0 and 100",
            });
        }
        let label = label.trim();
        if label.is_empty() || label.chars().count() > MAX_GRADE_LABEL_LEN {
            return Err(ValidationError::Invalid {
                field: "grade_bands",
                reason: "band labels must be 1 to 20 characters",
            });
        }
        Ok(Self {
            min,
            label: label.to_string(),
        })
    }

    pub fn get_min(&self) -> i64 {
        self.min
    }

    pub fn get_label(&self) -> &str {
        &self.label
    }
}

/// The school's policy knobs, validated as a whole (parse, don't validate —
/// like every other entity, an existing `Settings` is always internally
/// consistent).
#[derive(Debug, Clone, SurrealValue)]
pub struct Settings {
    id: RecordId,
    exam_kinds: Vec<ExamKindDef>,
    attendance_statuses: Vec<String>,
    grade_bands: Vec<GradeBand>,
    /// Per-file byte cap for note uploads. `None` = the row predates the
    /// field (or the defaults) — reads as `DEFAULT_MAX_FILE_BYTES`.
    max_file_bytes: Option<i64>,
    /// Chatbot knobs, `None`-while-unset exactly like `max_file_bytes`.
    chatbot_history_turns: Option<i64>,
    max_chatbot_threads: Option<i64>,
    max_chatbot_message_len: Option<i64>,
    /// Food-program knobs. `None`-while-unset exactly like `max_file_bytes` —
    /// the columns are `option<…>`, never `DEFAULT []`: `save_if_unchanged`
    /// writes the whole row, so a field this struct did not carry would coerce
    /// to `NONE` and abort the transaction.
    meal_slots: Option<Vec<MealSlotDef>>,
    dietary_tags: Option<Vec<String>>,
    /// Minutes before a meal at which booking *and* cancelling close. One knob
    /// for both deadlines; `None` = no cutoff at all, which is also what an
    /// unset column reads as.
    meal_cancel_cutoff_minutes: Option<i64>,
}

/// Everything [`Settings::try_new`] validates, in one struct — the knobs
/// outgrew a readable argument list. Take a resolved snapshot with
/// [`Settings::params`] and overwrite only the fields being changed; that is
/// exactly what `PATCH /settings` merges.
pub struct SettingsParams {
    pub exam_kinds: Vec<ExamKindDef>,
    pub attendance_statuses: Vec<String>,
    pub grade_bands: Vec<GradeBand>,
    pub max_file_bytes: i64,
    pub chatbot_history_turns: i64,
    pub max_chatbot_threads: i64,
    pub max_chatbot_message_len: i64,
    pub meal_slots: Vec<MealSlotDef>,
    pub dietary_tags: Vec<String>,
    /// `None` = no booking/cancel cutoff.
    pub meal_cancel_cutoff_minutes: Option<i64>,
}

impl Settings {
    fn record_id() -> RecordId {
        RecordId::new(SETTINGS_TABLE, SETTINGS_KEY)
    }

    /// The out-of-the-box policy — mirrors the constants that were previously
    /// hard-coded, so a school that never touches `/settings` sees no change.
    pub fn defaults() -> Self {
        Self {
            id: Self::record_id(),
            exam_kinds: DEFAULT_EXAM_KINDS
                .map(|(name, weight)| ExamKindDef {
                    name: name.to_string(),
                    weight,
                })
                .to_vec(),
            attendance_statuses: DEFAULT_ATTENDANCE_STATUSES.map(String::from).to_vec(),
            grade_bands: Vec::new(),
            max_file_bytes: None,
            chatbot_history_turns: None,
            max_chatbot_threads: None,
            max_chatbot_message_len: None,
            meal_slots: None,
            dietary_tags: None,
            meal_cancel_cutoff_minutes: None,
        }
    }

    /// This policy as editable parameters, every optional knob resolved to the
    /// value it currently reads as.
    pub fn params(&self) -> SettingsParams {
        SettingsParams {
            exam_kinds: self.exam_kinds.clone(),
            attendance_statuses: self.attendance_statuses.clone(),
            grade_bands: self.grade_bands.clone(),
            max_file_bytes: self.get_max_file_bytes(),
            chatbot_history_turns: self.get_chatbot_history_turns(),
            max_chatbot_threads: self.get_max_chatbot_threads(),
            max_chatbot_message_len: self.get_max_chatbot_message_len(),
            meal_slots: self.get_meal_slots(),
            dietary_tags: self.get_dietary_tags(),
            meal_cancel_cutoff_minutes: self.meal_cancel_cutoff_minutes,
        }
    }

    /// Validate a full policy. List entries are trimmed; kinds and statuses
    /// must be non-empty, unique (folded: case- and Turkish-insensitive), and bounded; the four
    /// core attendance statuses can never be removed (the attendance rate's
    /// semantics are defined over them). Bands may be empty (numeric-only
    /// display), otherwise their mins are unique and one band must start at 0
    /// so every mark maps to a label. The upload cap must sit inside the
    /// server's hard bounds — the ceiling protects memory and disk, whatever
    /// the school would prefer, and the same holds for the chatbot knobs.
    pub fn try_new(params: SettingsParams) -> Result<Self, ValidationError> {
        let SettingsParams {
            exam_kinds,
            attendance_statuses,
            grade_bands,
            max_file_bytes,
            chatbot_history_turns,
            max_chatbot_threads,
            max_chatbot_message_len,
            meal_slots,
            dietary_tags,
            meal_cancel_cutoff_minutes,
        } = params;
        in_range(
            "max_file_bytes",
            max_file_bytes,
            MIN_MAX_FILE_BYTES..=MAX_MAX_FILE_BYTES,
            "must be between 1024 (1 KiB) and 26214400 (25 MiB)",
        )?;
        in_range(
            "chatbot_history_turns",
            chatbot_history_turns,
            MIN_CHATBOT_HISTORY_TURNS..=MAX_CHATBOT_HISTORY_TURNS,
            "must be between 1 and 50",
        )?;
        in_range(
            "max_chatbot_threads",
            max_chatbot_threads,
            MIN_MAX_CHATBOT_THREADS..=MAX_MAX_CHATBOT_THREADS,
            "must be between 1 and 500",
        )?;
        in_range(
            "max_chatbot_message_len",
            max_chatbot_message_len,
            MIN_MAX_CHATBOT_MESSAGE_LEN..=MAX_MAX_CHATBOT_MESSAGE_LEN,
            "must be between 100 and 8000 characters",
        )?;
        // Per-entry rules (name shape, weight range) hold structurally on any
        // `ExamKindDef`; here only the list-level rules need checking.
        let names: Vec<String> = exam_kinds
            .iter()
            .map(|kind| kind.get_name().to_string())
            .collect();
        validate_list("exam_kinds", names)?;
        let attendance_statuses = validate_list("attendance_statuses", attendance_statuses)?;
        if DEFAULT_ATTENDANCE_STATUSES
            .iter()
            .any(|core| !attendance_statuses.iter().any(|s| s == core))
        {
            return Err(ValidationError::Invalid {
                field: "attendance_statuses",
                reason: "must keep the core statuses: present, absent, late, excused",
            });
        }

        if grade_bands.len() > MAX_GRADE_BANDS {
            return Err(ValidationError::Invalid {
                field: "grade_bands",
                reason: "must contain at most 20 bands",
            });
        }
        let mut mins: Vec<i64> = grade_bands.iter().map(GradeBand::get_min).collect();
        mins.sort_unstable();
        mins.dedup();
        if mins.len() != grade_bands.len() {
            return Err(ValidationError::Invalid {
                field: "grade_bands",
                reason: "band mins must be unique",
            });
        }
        if !grade_bands.is_empty() && !mins.contains(&MIN_MARK) {
            return Err(ValidationError::Invalid {
                field: "grade_bands",
                reason: "one band must start at 0 so every mark gets a label",
            });
        }
        // Canonical order: highest band first, matching lookup direction.
        let mut grade_bands = grade_bands;
        grade_bands.sort_by_key(|band| std::cmp::Reverse(band.get_min()));

        // The food lists follow the same list rules as the kinds and statuses
        // above, with one difference: empty is legal on both. A school that
        // runs no canteen has no slots and no dietary tags, and refusing that
        // would force it to keep a list it never uses.
        let slot_names: Vec<String> = meal_slots
            .iter()
            .map(|slot| slot.get_name().to_string())
            .collect();
        if !slot_names.is_empty() {
            validate_list("meal_slots", slot_names)?;
        }
        let dietary_tags = if dietary_tags.is_empty() {
            dietary_tags
        } else {
            validate_list("dietary_tags", dietary_tags)?
        };
        // One knob for both deadlines: 0 = closes exactly at serving time.
        if let Some(minutes) = meal_cancel_cutoff_minutes {
            in_range(
                "meal_cancel_cutoff_minutes",
                minutes,
                0..=MAX_MEAL_CANCEL_CUTOFF_MINUTES,
                "must be between 0 and 10080 (one week) minutes",
            )?;
        }

        Ok(Self {
            id: Self::record_id(),
            exam_kinds,
            attendance_statuses,
            grade_bands,
            max_file_bytes: Some(max_file_bytes),
            chatbot_history_turns: Some(chatbot_history_turns),
            max_chatbot_threads: Some(max_chatbot_threads),
            max_chatbot_message_len: Some(max_chatbot_message_len),
            meal_slots: Some(meal_slots),
            dietary_tags: Some(dietary_tags),
            meal_cancel_cutoff_minutes,
        })
    }

    pub fn get_exam_kinds(&self) -> &[ExamKindDef] {
        &self.exam_kinds
    }

    /// The weight of the kind named `kind` (exact match, like kind
    /// validation), or `None` when the school no longer lists it. Callers
    /// averaging marks fall back to weight 1 — an exam keeps its retired kind
    /// (settings edits never rewrite history), so it must still count.
    pub fn exam_kind_weight(&self, kind: &str) -> Option<i64> {
        self.exam_kinds
            .iter()
            .find(|def| def.get_name() == kind)
            .map(ExamKindDef::get_weight)
    }

    pub fn get_attendance_statuses(&self) -> &[String] {
        &self.attendance_statuses
    }

    pub fn get_grade_bands(&self) -> &[GradeBand] {
        &self.grade_bands
    }

    /// The per-file upload cap in bytes; the built-in default while the
    /// school never set one (including rows saved before the field existed).
    pub fn get_max_file_bytes(&self) -> i64 {
        self.max_file_bytes.unwrap_or(DEFAULT_MAX_FILE_BYTES)
    }

    /// How many prior thread turns ride along as context on an AI
    /// request; the built-in default while the school never set one.
    pub fn get_chatbot_history_turns(&self) -> i64 {
        self.chatbot_history_turns
            .unwrap_or(DEFAULT_CHATBOT_HISTORY_TURNS)
    }

    /// How many threads one user may keep; the built-in default while
    /// the school never set one.
    pub fn get_max_chatbot_threads(&self) -> i64 {
        self.max_chatbot_threads
            .unwrap_or(DEFAULT_MAX_CHATBOT_THREADS)
    }

    /// Character cap on one chat message; the built-in default while the
    /// school never set one.
    pub fn get_max_chatbot_message_len(&self) -> i64 {
        self.max_chatbot_message_len
            .unwrap_or(DEFAULT_MAX_CHATBOT_MESSAGE_LEN)
    }

    /// The meal slots the school serves; the built-in list while it never set
    /// one (including rows saved before the field existed). An explicitly
    /// stored empty list stays empty — that is "no meal program", not "unset".
    pub fn get_meal_slots(&self) -> Vec<MealSlotDef> {
        self.meal_slots.clone().unwrap_or_else(|| {
            DEFAULT_MEAL_SLOTS
                .map(|name| MealSlotDef {
                    name: name.to_string(),
                    // No built-in serving time, and the backend will not guess
                    // one: a slot with no hour has no instant for the cutoff to
                    // count back from, so the school's `meal_cancel_cutoff_
                    // minutes` binds none of these slots until it sets real
                    // hours (see `check_cutoff`).
                    serving_minute: None,
                })
                .to_vec()
        })
    }

    /// The dietary tags a dish and a student's profile may carry; the built-in
    /// list while the school never set one.
    pub fn get_dietary_tags(&self) -> Vec<String> {
        self.dietary_tags
            .clone()
            .unwrap_or_else(|| DEFAULT_DIETARY_TAGS.map(String::from).to_vec())
    }

    /// Minutes before a meal at which booking and cancelling close; `None` =
    /// no cutoff, the default while the school never set one.
    pub fn get_meal_cancel_cutoff_minutes(&self) -> Option<i64> {
        self.meal_cancel_cutoff_minutes
    }

    /// The label of the band `mark` falls into: the band with the greatest
    /// `min` at or below it. `None` when no bands are configured. Takes an
    /// `f64` so course averages label the same way plain marks do.
    pub fn grade_label(&self, mark: f64) -> Option<&str> {
        self.grade_bands
            .iter()
            .filter(|band| band.get_min() as f64 <= mark)
            .max_by_key(|band| band.get_min())
            .map(GradeBand::get_label)
    }

    /// The stored policy, or the defaults when no row exists yet.
    pub async fn load(db: &Database) -> Result<Settings, AppError> {
        let found: Option<Settings> = db.select(Self::record_id()).await?;
        Ok(found.unwrap_or_else(Self::defaults))
    }

    /// Persist the policy (single UPSERT on the fixed singleton id),
    /// unconditionally — last write wins. Prefer [`Self::save_if_unchanged`]
    /// wherever the new policy was merged from a loaded snapshot.
    pub async fn save(self, db: &Database) -> Result<Settings, AppError> {
        // whole-row-save-ok: test-only seeding; every production write merges from a loaded snapshot and goes through save_if_unchanged
        let saved: Option<Settings> = db.upsert(Self::record_id()).content(self).await?;
        saved.ok_or_else(|| AppError::Internal("failed to save settings".into()))
    }

    /// Persist the policy only while the stored row still matches `expected`
    /// — the snapshot the caller merged omitted fields from. `None` means a
    /// concurrent edit landed in between and nothing was written: reload,
    /// re-merge, retry. Without this compare-and-set, two managers patching
    /// *different* fields silently revert each other (both merge from the
    /// same snapshot; the later whole-row write restores its stale copy of
    /// the other's field).
    ///
    /// One transaction: the seed insert materializes the defaults-as-loaded
    /// state when no row exists yet (`load` reported the defaults, so the
    /// defaults are what the caller merged over), then the guarded update
    /// applies `self` only if the row (still) equals `expected`.
    ///
    /// **`meal_slots` is compared through a projection, and it has to be.**
    /// SurrealDB *drops* an object key whose value is `NONE` on write, while
    /// the `SurrealValue` derive always emits `serving_minute: NONE` for a
    /// slot without one — so `{name: 'lunch'} = {name: 'lunch', serving_minute:
    /// NONE}` is **false** and a plain equality guard would never match again
    /// for any school with a serving-time-less slot (which is every school
    /// until it sets one): every `PATCH /settings` would 409 forever. Rebuilding
    /// both sides as full objects normalizes the shapes. The NONE-ness of the
    /// column itself is compared separately, so "never set" stays distinguishable
    /// from "explicitly empty". Top-level optional columns need none of this:
    /// a missing field reads as `NONE`, and `NONE = NONE` holds.
    pub async fn save_if_unchanged(
        self,
        expected: &Settings,
        db: &Database,
    ) -> Result<Option<Settings>, AppError> {
        let mut result = db
            .query(
                "BEGIN TRANSACTION;
                 INSERT IGNORE INTO settings $expected;
                 UPDATE $id CONTENT $new
                     WHERE exam_kinds = $ek
                       AND attendance_statuses = $st
                       AND grade_bands = $gb
                       AND max_file_bytes = $mf
                       AND chatbot_history_turns = $ct
                       AND max_chatbot_threads = $cc
                       AND max_chatbot_message_len = $cl
                       AND (meal_slots = NONE) = $ms_unset
                       AND (meal_slots ?? []).map(|$s| {
                               name: $s.name,
                               serving_minute: $s.serving_minute
                           }) = ($ms ?? [])
                       AND dietary_tags = $dt
                       AND meal_cancel_cutoff_minutes = $mc;
                 COMMIT TRANSACTION;",
            )
            .bind(("expected", expected.clone()))
            .bind(("id", Self::record_id()))
            .bind(("new", self))
            .bind(("ek", expected.exam_kinds.clone()))
            .bind(("st", expected.attendance_statuses.clone()))
            .bind(("gb", expected.grade_bands.clone()))
            .bind(("mf", expected.max_file_bytes))
            .bind(("ct", expected.chatbot_history_turns))
            .bind(("cc", expected.max_chatbot_threads))
            .bind(("cl", expected.max_chatbot_message_len))
            .bind(("ms_unset", expected.meal_slots.is_none()))
            .bind(("ms", expected.meal_slots.clone()))
            .bind(("dt", expected.dietary_tags.clone()))
            .bind(("mc", expected.meal_cancel_cutoff_minutes))
            .await?
            .check()?;
        // Statement slots count BEGIN too: the guarded UPDATE is slot 2. An
        // empty slot means the row no longer matched `expected`.
        Ok(result.take::<Vec<Settings>>(2)?.into_iter().next())
    }
}

/// Shared bounds check for the numeric knobs (upload cap, chatbot limits):
/// each is held to a server-defined inclusive range.
fn in_range(
    field: &'static str,
    value: i64,
    range: std::ops::RangeInclusive<i64>,
    reason: &'static str,
) -> Result<(), ValidationError> {
    if range.contains(&value) {
        Ok(())
    } else {
        Err(ValidationError::Invalid { field, reason })
    }
}

/// Shared rules for the editable string lists: 1–20 trimmed, non-empty,
/// ≤50-char entries with no case-insensitive duplicates.
fn validate_list(field: &'static str, values: Vec<String>) -> Result<Vec<String>, ValidationError> {
    if values.is_empty() {
        return Err(ValidationError::Empty(field));
    }
    if values.len() > MAX_SETTINGS_LIST_LEN {
        return Err(ValidationError::Invalid {
            field,
            reason: "must contain at most 20 entries",
        });
    }
    let mut trimmed = Vec::with_capacity(values.len());
    for value in &values {
        let value = value.trim();
        if value.is_empty() || value.chars().count() > MAX_SETTINGS_ITEM_LEN {
            return Err(ValidationError::Invalid {
                field,
                reason: "entries must be 1 to 50 characters",
            });
        }
        trimmed.push(value.to_string());
    }
    // `to_lowercase` is locale-invariant — `İZİN` lowercases to `i̇zin` (with a
    // leftover combining dot) and would slip past `izin` as a distinct entry.
    // The *identity* fold, not search's: search deliberately collapses `ü`→`u`,
    // which would make a school unable to have both `tur` and `tür`.
    let mut folded: Vec<String> = trimmed.iter().map(|v| text_fold::case_fold_tr(v)).collect();
    folded.sort_unstable();
    folded.dedup();
    if folded.len() != trimmed.len() {
        return Err(ValidationError::Invalid {
            field,
            reason: "two entries are the same word apart from upper/lower case \
                     or Turkish letters — keep only one of them",
        });
    }
    Ok(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::MAX_CHATBOT_MESSAGE_LEN;

    fn kinds(list: &[&str]) -> Vec<ExamKindDef> {
        list.iter()
            .map(|s| ExamKindDef::try_new(s, 1).unwrap())
            .collect()
    }

    fn names(settings: &Settings) -> Vec<&str> {
        settings
            .get_exam_kinds()
            .iter()
            .map(ExamKindDef::get_name)
            .collect()
    }

    /// The defaults as tweakable params — every site below overrides only the
    /// fields it is testing (`..params()`).
    fn params() -> SettingsParams {
        Settings::defaults().params()
    }

    fn bands(list: &[(i64, &str)]) -> Vec<GradeBand> {
        list.iter()
            .map(|(min, label)| GradeBand::try_new(*min, label).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn defaults_are_a_valid_policy() {
        let defaults = Settings::defaults();
        let rebuilt = Settings::try_new(defaults.params()).unwrap();
        assert_eq!(rebuilt.get_exam_kinds(), defaults.get_exam_kinds());
        assert!(defaults.get_grade_bands().is_empty());
    }

    #[tokio::test]
    async fn kind_defs_hold_name_and_weight_rules() {
        // Names are trimmed; blank or over-long names die.
        assert_eq!(
            ExamKindDef::try_new("  lab  ", 1).unwrap().get_name(),
            "lab"
        );
        assert!(ExamKindDef::try_new("  ", 1).is_err());
        assert!(ExamKindDef::try_new(&"x".repeat(51), 1).is_err());
        // Weights are held to 1–100 — 0 would erase the kind's exams from the
        // average, and negatives would corrupt it.
        for weight in [1, 50, 100] {
            assert_eq!(
                ExamKindDef::try_new("oral", weight).unwrap().get_weight(),
                weight
            );
        }
        assert!(ExamKindDef::try_new("oral", 0).is_err());
        assert!(ExamKindDef::try_new("oral", -1).is_err());
        assert!(ExamKindDef::try_new("oral", 101).is_err());
    }

    #[tokio::test]
    async fn lists_are_bounded_and_deduped() {
        let with_kinds = |exam_kinds| {
            Settings::try_new(SettingsParams {
                exam_kinds,
                ..params()
            })
        };
        let s = with_kinds(kinds(&["lab", "quiz"])).unwrap();
        assert_eq!(names(&s), ["lab", "quiz"]);
        // Empty list and case-insensitive duplicate names are rejected.
        assert!(with_kinds(vec![]).is_err());
        assert!(with_kinds(kinds(&["Lab", "lab"])).is_err());
        let too_many: Vec<ExamKindDef> = (0..21)
            .map(|i| ExamKindDef::try_new(&format!("kind{i}"), 1).unwrap())
            .collect();
        assert!(with_kinds(too_many).is_err());
    }

    /// Rust's `to_lowercase` is locale-invariant: `İZİN` becomes `i̇zin` (with a
    /// combining dot above), which never equals `izin`, so both spellings of
    /// the same word used to land in one list. Teachers type these by hand in
    /// Turkish, which is exactly when it happens.
    #[tokio::test]
    async fn turkish_casing_is_a_duplicate() {
        let with_kinds = |exam_kinds| {
            Settings::try_new(SettingsParams {
                exam_kinds,
                ..params()
            })
        };
        assert!(with_kinds(kinds(&["İZİN", "izin"])).is_err());
        assert!(with_kinds(kinds(&["SINAV", "sınav"])).is_err());
        // Plain English duplicates still die, genuinely distinct entries live.
        assert!(with_kinds(kinds(&["Lab", "LAB"])).is_err());
        assert!(with_kinds(kinds(&["izin", "sınav", "lab"])).is_ok());
        // Same rule on the other guarded list.
        assert!(
            Settings::try_new(SettingsParams {
                attendance_statuses: statuses_with(&["İZİN", "izin"]),
                ..params()
            })
            .is_err()
        );
    }

    /// Dedup folds *case*, not letters. The search fold collapses `ü`→`u`, so
    /// using it here made genuinely distinct Turkish words un-listable: a
    /// school could not have both a `tur` and a `tür`.
    #[tokio::test]
    async fn distinct_turkish_words_are_not_duplicates() {
        let with_kinds = |exam_kinds| {
            Settings::try_new(SettingsParams {
                exam_kinds,
                ..params()
            })
        };
        assert!(with_kinds(kinds(&["tur", "tür"])).is_ok());
        assert!(with_kinds(kinds(&["kir", "kır"])).is_ok());
        // A true case variant of one of them is still a duplicate.
        assert!(with_kinds(kinds(&["tür", "TÜR"])).is_err());
        assert!(with_kinds(kinds(&["kır", "KIR"])).is_err());
        // Same on the other guarded list.
        assert!(
            Settings::try_new(SettingsParams {
                attendance_statuses: statuses_with(&["tur", "tür"]),
                ..params()
            })
            .is_ok()
        );
    }

    #[tokio::test]
    async fn kind_weights_resolve_by_exact_name() {
        let s = Settings::try_new(SettingsParams {
            exam_kinds: vec![
                ExamKindDef::try_new("midterm", 2).unwrap(),
                ExamKindDef::try_new("final", 3).unwrap(),
            ],
            ..params()
        })
        .unwrap();
        assert_eq!(s.exam_kind_weight("final"), Some(3));
        assert_eq!(s.exam_kind_weight("midterm"), Some(2));
        // Exact match, case included — same contract as kind validation.
        assert_eq!(s.exam_kind_weight("Final"), None);
        assert_eq!(s.exam_kind_weight("oral"), None);
    }

    #[tokio::test]
    async fn core_attendance_statuses_are_mandatory() {
        let with_statuses = |attendance_statuses| {
            Settings::try_new(SettingsParams {
                attendance_statuses,
                ..params()
            })
        };
        // Extras on top of the core are fine.
        assert!(with_statuses(statuses_with(&["online"])).is_ok());
        // Dropping any core status is not.
        let missing: Vec<String> = statuses_with(&[])
            .into_iter()
            .filter(|s| s != "late")
            .collect();
        assert!(with_statuses(missing).is_err());
    }

    fn statuses_with(extra: &[&str]) -> Vec<String> {
        DEFAULT_ATTENDANCE_STATUSES
            .iter()
            .map(|s| s.to_string())
            .chain(extra.iter().map(|s| s.to_string()))
            .collect()
    }

    #[tokio::test]
    async fn band_rules() {
        // Per-band validation: min range and label shape.
        assert!(GradeBand::try_new(-1, "FF").is_err());
        assert!(GradeBand::try_new(101, "AA").is_err());
        assert!(GradeBand::try_new(50, "  ").is_err());
        assert!(GradeBand::try_new(50, &"x".repeat(21)).is_err());
        assert_eq!(GradeBand::try_new(50, "  CC ").unwrap().get_label(), "CC");

        let ok = |grade_bands| {
            Settings::try_new(SettingsParams {
                grade_bands,
                ..params()
            })
        };
        // Empty = numeric-only display.
        assert!(ok(vec![]).is_ok());
        // Duplicate mins die; a set without a 0-band dies (marks below the
        // lowest band would have no label).
        assert!(ok(bands(&[(0, "F"), (0, "E")])).is_err());
        assert!(ok(bands(&[(50, "CC"), (85, "AA")])).is_err());
        assert!(ok(bands(&[(0, "FF"), (50, "CC"), (85, "AA")])).is_ok());
    }

    #[tokio::test]
    async fn max_file_bytes_is_bounded() {
        let with_cap = |max_file_bytes| {
            Settings::try_new(SettingsParams {
                max_file_bytes,
                ..params()
            })
        };
        for cap in [
            MIN_MAX_FILE_BYTES,
            DEFAULT_MAX_FILE_BYTES,
            MAX_MAX_FILE_BYTES,
        ] {
            assert_eq!(with_cap(cap).unwrap().get_max_file_bytes(), cap);
        }
        assert!(with_cap(MIN_MAX_FILE_BYTES - 1).is_err());
        assert!(with_cap(MAX_MAX_FILE_BYTES + 1).is_err());
        assert!(with_cap(0).is_err());
        assert!(with_cap(-1).is_err());
    }

    #[tokio::test]
    async fn chat_knobs_are_bounded() {
        // Each knob is held to its own inclusive range; the edges are legal.
        let turns = |chatbot_history_turns| {
            Settings::try_new(SettingsParams {
                chatbot_history_turns,
                ..params()
            })
        };
        assert_eq!(
            turns(MAX_CHATBOT_HISTORY_TURNS)
                .unwrap()
                .get_chatbot_history_turns(),
            MAX_CHATBOT_HISTORY_TURNS
        );
        assert!(turns(MIN_CHATBOT_HISTORY_TURNS).is_ok());
        assert!(turns(0).is_err());
        assert!(turns(MAX_CHATBOT_HISTORY_TURNS + 1).is_err());

        let convos = |max_chatbot_threads| {
            Settings::try_new(SettingsParams {
                max_chatbot_threads,
                ..params()
            })
        };
        assert_eq!(
            convos(7).unwrap().get_max_chatbot_threads(),
            7,
            "an in-range cap survives validation"
        );
        assert!(convos(MIN_MAX_CHATBOT_THREADS).is_ok());
        assert!(convos(MAX_MAX_CHATBOT_THREADS).is_ok());
        assert!(convos(0).is_err());
        assert!(convos(MAX_MAX_CHATBOT_THREADS + 1).is_err());

        let len = |max_chatbot_message_len| {
            Settings::try_new(SettingsParams {
                max_chatbot_message_len,
                ..params()
            })
        };
        assert!(len(MIN_MAX_CHATBOT_MESSAGE_LEN).is_ok());
        assert!(len(MAX_MAX_CHATBOT_MESSAGE_LEN).is_ok());
        assert!(len(MIN_MAX_CHATBOT_MESSAGE_LEN - 1).is_err());
        // The ceiling is the newtype's hard cap: no school can raise it.
        assert!(len(MAX_CHATBOT_MESSAGE_LEN as i64 + 1).is_err());
    }

    #[tokio::test]
    async fn a_row_predating_the_optional_knobs_reads_the_defaults() {
        let db = crate::database::init_mem().await.unwrap();
        // The defaults carry no explicit knobs, so this writes a row without
        // those fields — exactly what a volume from before them looks like.
        Settings::defaults().save(&db).await.unwrap();
        let loaded = Settings::load(&db).await.unwrap();
        assert_eq!(loaded.get_max_file_bytes(), DEFAULT_MAX_FILE_BYTES);
        assert_eq!(
            loaded.get_chatbot_history_turns(),
            DEFAULT_CHATBOT_HISTORY_TURNS
        );
        assert_eq!(
            loaded.get_max_chatbot_threads(),
            DEFAULT_MAX_CHATBOT_THREADS
        );
        assert_eq!(
            loaded.get_max_chatbot_message_len(),
            DEFAULT_MAX_CHATBOT_MESSAGE_LEN
        );
        // And a snapshot of that old row still passes the compare-and-set.
        let saved = Settings::try_new(SettingsParams {
            max_file_bytes: 4096,
            ..loaded.params()
        })
        .unwrap()
        .save_if_unchanged(&loaded, &db)
        .await
        .unwrap()
        .expect("a merge over an old-shape row applies");
        assert_eq!(saved.get_max_file_bytes(), 4096);
    }

    #[tokio::test]
    async fn load_save_roundtrip_on_the_singleton() {
        let db = crate::database::init_mem().await.unwrap();
        // No row yet → the defaults, not an error.
        let loaded = Settings::load(&db).await.unwrap();
        assert_eq!(
            loaded.get_exam_kinds(),
            Settings::defaults().get_exam_kinds()
        );
        // Save a custom policy and read it back — bands (nested objects under
        // a FLEXIBLE field) must survive the trip.
        Settings::try_new(SettingsParams {
            exam_kinds: kinds(&["lab"]),
            grade_bands: bands(&[(0, "F"), (50, "P")]),
            max_file_bytes: 2048,
            chatbot_history_turns: 3,
            ..params()
        })
        .unwrap()
        .save(&db)
        .await
        .unwrap();
        let loaded = Settings::load(&db).await.unwrap();
        assert_eq!(names(&loaded), ["lab"]);
        assert_eq!(loaded.get_grade_bands().len(), 2);
        assert_eq!(loaded.grade_label(60.0), Some("P"));
        assert_eq!(loaded.get_max_file_bytes(), 2048);
        assert_eq!(loaded.get_chatbot_history_turns(), 3);
        // A second save lands on the same singleton row, not a new one.
        Settings::try_new(SettingsParams {
            exam_kinds: kinds(&["quiz"]),
            ..params()
        })
        .unwrap()
        .save(&db)
        .await
        .unwrap();
        let mut result = db.query("SELECT * FROM settings").await.unwrap();
        let rows: Vec<Settings> = result.take(0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(names(&rows[0]), ["quiz"]);
    }

    #[tokio::test]
    async fn a_stale_snapshot_cannot_revert_a_newer_policy() {
        let db = crate::database::init_mem().await.unwrap();

        // Editor A snapshots the policy (the defaults — no row yet)...
        let stale = Settings::load(&db).await.unwrap();
        // ...then editor B lands a new exam-kind list first.
        Settings::try_new(SettingsParams {
            exam_kinds: kinds(&["lab"]),
            ..params()
        })
        .unwrap()
        .save(&db)
        .await
        .unwrap();

        // A's merge over the stale snapshot (kinds kept "as loaded", bands
        // changed) — exactly what a concurrent PATCH /settings computes —
        // must be refused, not applied.
        let refused = Settings::try_new(SettingsParams {
            grade_bands: bands(&[(0, "F"), (50, "P")]),
            ..stale.params()
        })
        .unwrap()
        .save_if_unchanged(&stale, &db)
        .await
        .unwrap();
        assert!(refused.is_none(), "a stale snapshot's save must not apply");

        // B's edit must survive A's stale write attempt.
        let after = Settings::load(&db).await.unwrap();
        assert_eq!(
            names(&after),
            ["lab"],
            "a concurrent editor's exam kinds must not be silently reverted"
        );
        assert!(after.get_grade_bands().is_empty());

        // A's retry — reload, re-merge, save again — lands both edits.
        let fresh = Settings::load(&db).await.unwrap();
        let saved = Settings::try_new(SettingsParams {
            grade_bands: bands(&[(0, "F"), (50, "P")]),
            ..fresh.params()
        })
        .unwrap()
        .save_if_unchanged(&fresh, &db)
        .await
        .unwrap()
        .expect("a merge over the current row applies");
        assert_eq!(names(&saved), ["lab"]);
        assert_eq!(saved.get_grade_bands().len(), 2);
    }

    #[tokio::test]
    async fn save_if_unchanged_seeds_the_first_row() {
        let db = crate::database::init_mem().await.unwrap();
        // No row yet: `load` reports the defaults, and a save conditioned on
        // that snapshot must apply (seeding the singleton on the way).
        let current = Settings::load(&db).await.unwrap();
        let saved = Settings::try_new(SettingsParams {
            exam_kinds: kinds(&["lab"]),
            ..current.params()
        })
        .unwrap()
        .save_if_unchanged(&current, &db)
        .await
        .unwrap()
        .expect("the first save applies");
        assert_eq!(names(&saved), ["lab"]);
        let mut result = db.query("SELECT * FROM settings").await.unwrap();
        let rows: Vec<Settings> = result.take(0).unwrap();
        assert_eq!(rows.len(), 1, "still one singleton row");
    }

    #[tokio::test]
    async fn grade_label_picks_the_greatest_min_at_or_below() {
        let s = Settings::try_new(SettingsParams {
            grade_bands: bands(&[(0, "FF"), (50, "CC"), (85, "AA")]),
            ..params()
        })
        .unwrap();
        assert_eq!(s.grade_label(0.0), Some("FF"));
        assert_eq!(s.grade_label(49.9), Some("FF"));
        assert_eq!(s.grade_label(50.0), Some("CC"));
        assert_eq!(s.grade_label(84.9), Some("CC"));
        assert_eq!(s.grade_label(85.0), Some("AA"));
        assert_eq!(s.grade_label(100.0), Some("AA"));
        // No bands configured → no label, never a panic.
        assert_eq!(Settings::defaults().grade_label(90.0), None);
    }
}
