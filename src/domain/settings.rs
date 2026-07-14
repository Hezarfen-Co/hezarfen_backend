//! School-adjustable policy: which exam kinds exist, which attendance
//! statuses the roll call accepts, and how numeric marks display as grades.
//!
//! One singleton record (`settings:school` — one school per deployment). An
//! absent record means "the defaults from `constant.rs`", so a fresh or
//! pre-existing database needs no seeding and behaves exactly as before.

use surrealdb::types::{RecordId, SurrealValue};

use crate::constant::{
    DEFAULT_ATTENDANCE_STATUSES, DEFAULT_EXAM_KINDS, MAX_GRADE_BANDS, MAX_GRADE_LABEL_LEN,
    MAX_MARK, MAX_SETTINGS_ITEM_LEN, MAX_SETTINGS_LIST_LEN, MIN_MARK,
};
use crate::database::{Database, SETTINGS_TABLE};
use crate::error::{AppError, ValidationError};

/// The singleton's fixed key: one school per deployment, one settings row.
const SETTINGS_KEY: &str = "school";

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
    exam_kinds: Vec<String>,
    attendance_statuses: Vec<String>,
    grade_bands: Vec<GradeBand>,
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
            exam_kinds: DEFAULT_EXAM_KINDS.map(String::from).to_vec(),
            attendance_statuses: DEFAULT_ATTENDANCE_STATUSES.map(String::from).to_vec(),
            grade_bands: Vec::new(),
        }
    }

    /// Validate a full policy. List entries are trimmed; kinds and statuses
    /// must be non-empty, unique (case-insensitive), and bounded; the four
    /// core attendance statuses can never be removed (the attendance rate's
    /// semantics are defined over them). Bands may be empty (numeric-only
    /// display), otherwise their mins are unique and one band must start at 0
    /// so every mark maps to a label.
    pub fn try_new(
        exam_kinds: Vec<String>,
        attendance_statuses: Vec<String>,
        grade_bands: Vec<GradeBand>,
    ) -> Result<Self, ValidationError> {
        let exam_kinds = validate_list("exam_kinds", exam_kinds)?;
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

        Ok(Self {
            id: Self::record_id(),
            exam_kinds,
            attendance_statuses,
            grade_bands,
        })
    }

    pub fn get_exam_kinds(&self) -> &[String] {
        &self.exam_kinds
    }

    pub fn get_attendance_statuses(&self) -> &[String] {
        &self.attendance_statuses
    }

    pub fn get_grade_bands(&self) -> &[GradeBand] {
        &self.grade_bands
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
                       AND grade_bands = $gb;
                 COMMIT TRANSACTION;",
            )
            .bind(("expected", expected.clone()))
            .bind(("id", Self::record_id()))
            .bind(("new", self))
            .bind(("ek", expected.exam_kinds.clone()))
            .bind(("st", expected.attendance_statuses.clone()))
            .bind(("gb", expected.grade_bands.clone()))
            .await?
            .check()?;
        // Statement slots count BEGIN too: the guarded UPDATE is slot 2. An
        // empty slot means the row no longer matched `expected`.
        Ok(result.take::<Vec<Settings>>(2)?.into_iter().next())
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
    let mut folded: Vec<String> = trimmed.iter().map(|v| v.to_lowercase()).collect();
    folded.sort_unstable();
    folded.dedup();
    if folded.len() != trimmed.len() {
        return Err(ValidationError::Invalid {
            field,
            reason: "entries must be unique (case-insensitive)",
        });
    }
    Ok(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn bands(list: &[(i64, &str)]) -> Vec<GradeBand> {
        list.iter()
            .map(|(min, label)| GradeBand::try_new(*min, label).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn defaults_are_a_valid_policy() {
        let defaults = Settings::defaults();
        let rebuilt = Settings::try_new(
            defaults.get_exam_kinds().to_vec(),
            defaults.get_attendance_statuses().to_vec(),
            defaults.get_grade_bands().to_vec(),
        )
        .unwrap();
        assert_eq!(rebuilt.get_exam_kinds(), defaults.get_exam_kinds());
        assert!(defaults.get_grade_bands().is_empty());
    }

    #[tokio::test]
    async fn lists_are_trimmed_bounded_and_deduped() {
        let statuses = Settings::defaults().get_attendance_statuses().to_vec();
        // Trimming happens before storage.
        let s = Settings::try_new(kinds(&["  lab  ", "quiz"]), statuses.clone(), vec![]).unwrap();
        assert_eq!(s.get_exam_kinds(), ["lab", "quiz"]);
        // Empty list, blank entry, over-long entry, and case-insensitive
        // duplicates are all rejected.
        assert!(Settings::try_new(vec![], statuses.clone(), vec![]).is_err());
        assert!(Settings::try_new(kinds(&["  "]), statuses.clone(), vec![]).is_err());
        assert!(Settings::try_new(kinds(&[&"x".repeat(51)]), statuses.clone(), vec![]).is_err());
        assert!(Settings::try_new(kinds(&["Lab", "lab"]), statuses.clone(), vec![]).is_err());
        let too_many: Vec<String> = (0..21).map(|i| format!("kind{i}")).collect();
        assert!(Settings::try_new(too_many, statuses, vec![]).is_err());
    }

    #[tokio::test]
    async fn core_attendance_statuses_are_mandatory() {
        let kinds = Settings::defaults().get_exam_kinds().to_vec();
        // Extras on top of the core are fine.
        let extended = statuses_with(&["online"]);
        assert!(Settings::try_new(kinds.clone(), extended, vec![]).is_ok());
        // Dropping any core status is not.
        let missing: Vec<String> = statuses_with(&[])
            .into_iter()
            .filter(|s| s != "late")
            .collect();
        assert!(Settings::try_new(kinds, missing, vec![]).is_err());
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

        let defaults = Settings::defaults();
        let ok = |b: Vec<GradeBand>| {
            Settings::try_new(
                defaults.get_exam_kinds().to_vec(),
                defaults.get_attendance_statuses().to_vec(),
                b,
            )
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
        let statuses = Settings::defaults().get_attendance_statuses().to_vec();
        Settings::try_new(
            kinds(&["lab"]),
            statuses.clone(),
            bands(&[(0, "F"), (50, "P")]),
        )
        .unwrap()
        .save(&db)
        .await
        .unwrap();
        let loaded = Settings::load(&db).await.unwrap();
        assert_eq!(loaded.get_exam_kinds(), ["lab"]);
        assert_eq!(loaded.get_grade_bands().len(), 2);
        assert_eq!(loaded.grade_label(60.0), Some("P"));
        // A second save lands on the same singleton row, not a new one.
        Settings::try_new(kinds(&["quiz"]), statuses, vec![])
            .unwrap()
            .save(&db)
            .await
            .unwrap();
        let mut result = db.query("SELECT * FROM settings").await.unwrap();
        let rows: Vec<Settings> = result.take(0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get_exam_kinds(), ["quiz"]);
    }

    #[tokio::test]
    async fn a_stale_snapshot_cannot_revert_a_newer_policy() {
        let db = crate::database::init_mem().await.unwrap();
        let statuses = Settings::defaults().get_attendance_statuses().to_vec();

        // Editor A snapshots the policy (the defaults — no row yet)...
        let stale = Settings::load(&db).await.unwrap();
        // ...then editor B lands a new exam-kind list first.
        Settings::try_new(kinds(&["lab"]), statuses.clone(), vec![])
            .unwrap()
            .save(&db)
            .await
            .unwrap();

        // A's merge over the stale snapshot (kinds kept "as loaded", bands
        // changed) — exactly what a concurrent PATCH /settings computes —
        // must be refused, not applied.
        let refused = Settings::try_new(
            stale.get_exam_kinds().to_vec(),
            stale.get_attendance_statuses().to_vec(),
            bands(&[(0, "F"), (50, "P")]),
        )
        .unwrap()
        .save_if_unchanged(&stale, &db)
        .await
        .unwrap();
        assert!(refused.is_none(), "a stale snapshot's save must not apply");

        // B's edit must survive A's stale write attempt.
        let after = Settings::load(&db).await.unwrap();
        assert_eq!(
            after.get_exam_kinds(),
            ["lab"],
            "a concurrent editor's exam kinds must not be silently reverted"
        );
        assert!(after.get_grade_bands().is_empty());

        // A's retry — reload, re-merge, save again — lands both edits.
        let fresh = Settings::load(&db).await.unwrap();
        let saved = Settings::try_new(
            fresh.get_exam_kinds().to_vec(),
            fresh.get_attendance_statuses().to_vec(),
            bands(&[(0, "F"), (50, "P")]),
        )
        .unwrap()
        .save_if_unchanged(&fresh, &db)
        .await
        .unwrap()
        .expect("a merge over the current row applies");
        assert_eq!(saved.get_exam_kinds(), ["lab"]);
        assert_eq!(saved.get_grade_bands().len(), 2);
    }

    #[tokio::test]
    async fn save_if_unchanged_seeds_the_first_row() {
        let db = crate::database::init_mem().await.unwrap();
        // No row yet: `load` reports the defaults, and a save conditioned on
        // that snapshot must apply (seeding the singleton on the way).
        let current = Settings::load(&db).await.unwrap();
        let saved = Settings::try_new(
            kinds(&["lab"]),
            current.get_attendance_statuses().to_vec(),
            vec![],
        )
        .unwrap()
        .save_if_unchanged(&current, &db)
        .await
        .unwrap()
        .expect("the first save applies");
        assert_eq!(saved.get_exam_kinds(), ["lab"]);
        let mut result = db.query("SELECT * FROM settings").await.unwrap();
        let rows: Vec<Settings> = result.take(0).unwrap();
        assert_eq!(rows.len(), 1, "still one singleton row");
    }

    #[tokio::test]
    async fn grade_label_picks_the_greatest_min_at_or_below() {
        let defaults = Settings::defaults();
        let s = Settings::try_new(
            defaults.get_exam_kinds().to_vec(),
            defaults.get_attendance_statuses().to_vec(),
            bands(&[(0, "FF"), (50, "CC"), (85, "AA")]),
        )
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
