//! ZEKA's own tables, and the operations that write and read them.
//!
//! ZEKA (`hezarfen_zeka`) computes student and class insights, but it does
//! **not** touch a school database: every statement against the `zeka_*`
//! tables runs here, on the backend, under the school the caller named. That
//! is a standing rule of this deployment, not a preference — an AI service
//! process holding a database credential of its own would be a second, silent
//! door into every school's rows, one no request-scoped check can close. So
//! the service's storage surface is nine named operations ([`crate::ai::insight`]
//! carries the wire contract and the dispatch), and this module is what they
//! execute.
//!
//! The row types here are therefore **both** the SQL's input and the bridge's
//! payload: a nightly batch arrives as JSON, is parsed into these structs
//! (`invalid_payload` on anything that does not fit), and is written verbatim —
//! the backend is a trust boundary for the *shape*, never an author of the
//! content. Fields are spelled exactly as the tables' columns are, because a
//! consumer that has to guess a mapping between two spellings will eventually
//! guess wrong.
//!
//! Three invariants are worth stating up front, since they are what the SQL
//! below is arranged around:
//!
//! * **A batch is one transaction.** Every bulk write is all-or-nothing: a
//!   refusal means nothing landed, so a service that retries a batch cannot
//!   half-apply it. Row-at-a-time statements inside one transaction keep that
//!   honesty without a multi-row statement whose bind count would be a second
//!   bound to police; the batch ceiling [`MAX_INSIGHT_BATCH_ROWS`] keeps the
//!   statement count bounded.
//! * **Child rows are replaced, never merged.** `attention`, the segment
//!   `dimension` rows, and a run's `pending`/`failed_module` rows are deleted
//!   for their owners and re-inserted from the payload. Merging would leave a
//!   fact the service stopped producing on the screen forever.
//! * **The school is the database.** Every function here takes the school's
//!   own pool and nothing else: there is no `school` column to filter on
//!   because there is no row from another school in reach. Foreign keys carry
//!   the rest — an id from another school's database is not a row this
//!   database will accept, and that refusal is mapped to a validation error
//!   rather than a `500` so the caller learns which id was wrong.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sqlx::AssertSqlSafe;
use sqlx::types::Json as SqlJson;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::constant::{
    MAX_INSIGHT_BATCH_ROWS, MAX_INSIGHT_PENDING_STUDENTS, MAX_INSIGHT_PURGE_STUDENTS,
};
use crate::database::{Database, foreign_key_violation, tx_with_retry};
use crate::domain::monotonic_id::next_uuid;
use crate::error::{AppError, ValidationError};

// ---- the wire rows ---------------------------------------------------------

/// One student's computed summary, and the attention items that travel with
/// it. The two tables are replaced together — see the module docs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SummaryRow {
    /// `app_user.id` of the student this summary is about.
    pub student: String,
    /// The four evidence blocks, stored verbatim as JSONB. `null` is a legal
    /// value: it means the section had no source rows, which is not the same
    /// fact as a section that was not computed.
    #[serde(default)]
    pub marks: Option<JsonValue>,
    #[serde(default)]
    pub attendance: Option<JsonValue>,
    #[serde(default)]
    pub submission: Option<JsonValue>,
    #[serde(default)]
    pub study: Option<JsonValue>,
    /// `none` | `exploratory` | `stable` — the service's own confidence in
    /// the summary; the table's CHECK is the authority on the vocabulary.
    pub confidence: String,
    /// When the service computed this row, unix milliseconds.
    pub computed_at: i64,
    /// Unix milliseconds after which the sweep deletes the row. The service
    /// owns its retention policy; the backend writes what it is told.
    pub retain_until: i64,
    /// The student's attention items, in the service's own order. Omitted
    /// means "this student has none", which clears the previous ones.
    #[serde(default)]
    pub attention: Vec<AttentionRow>,
}

/// One attention item: a fact about a student that a teacher should see.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AttentionRow {
    /// The rule's trigger name, e.g. `not_egilimi_dusuyor`.
    pub trigger: String,
    /// The course the trigger fired in, when it fired in one.
    #[serde(default)]
    pub course: Option<String>,
    /// The one-line fact a reader sees.
    pub fact: String,
    /// The window the fact was measured over, unix milliseconds.
    pub window_from: i64,
    pub window_to: i64,
    /// The evidence object: the numbers behind the fact. Required — an item
    /// whose evidence is missing is dropped by the service before it is sent,
    /// and one that arrives without it is a payload error, not a silent write.
    pub evidence: JsonValue,
}

/// One recommendation card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct RecommendationRow {
    /// The user the card is addressed to (`audience_role` says as what).
    pub audience: String,
    /// The product surface it belongs to, e.g. `student_home`.
    pub product: String,
    /// The rule that produced it, and the rule set's version.
    pub rule_id: String,
    pub rule_version: i64,
    /// The rule's own scope marker (a segment label, a course) — part of the
    /// natural key, so two scopes of one rule are two cards.
    #[serde(default)]
    pub scope: Option<String>,
    /// The student the card is *about*, when it is about one.
    #[serde(default)]
    pub about: Option<String>,
    /// `student` | `teacher` | `parent` | `manager`.
    pub audience_role: String,
    #[serde(default)]
    pub course: Option<String>,
    /// The evidence object. A row whose evidence carries nothing but
    /// `limitation` is **not written** and is counted `rejected` in the
    /// receipt — the service's own rule, kept where it can be audited.
    pub evidence: JsonValue,
    /// The stated limitation of the measurement, merged into the stored
    /// evidence object under `limitation`.
    #[serde(default)]
    pub limitation: Option<String>,
    /// `none` | `exploratory` | `stable`.
    pub confidence: String,
    pub computed_at: i64,
    pub expires_at: i64,
    pub retain_until: i64,
}

/// The four rubric labels of one question, in the service's nested shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SegmentLabels {
    pub bilissel_talep: String,
    pub dikkat_tuzagi: String,
    pub okuma_yuku: String,
    pub adim_sayisi: String,
}

/// The model's confidence in each of the four labels, `0..=1`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SegmentConfidences {
    pub bilissel_talep: f64,
    pub dikkat_tuzagi: f64,
    pub okuma_yuku: f64,
    pub adim_sayisi: f64,
}

/// One question's segment labels, and the dimension split that travels with
/// them (replaced together).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SegmentRow {
    /// The exam question this row labels.
    pub question: String,
    pub exam: String,
    pub course: String,
    pub subject: String,
    pub labels: SegmentLabels,
    pub confidences: SegmentConfidences,
    /// The row's overall confidence, `none` | `exploratory` | `stable`.
    pub confidence: String,
    /// The distractors the question's wrong answers point at, when known.
    #[serde(default)]
    pub trap_choice: Option<String>,
    /// Why the labels are what they are — the reader's audit trail.
    pub rationale: String,
    /// The model (and prompt) that produced the labels.
    pub model: String,
    pub prompt_version: String,
    /// The prompt variant, when the service is A/B-ing one.
    #[serde(default)]
    pub variant: Option<String>,
    pub computed_at: i64,
    pub retain_until: i64,
    /// Dimensions the downstream rules may use.
    #[serde(default)]
    pub downstream_dimensions: Vec<String>,
    /// Dimensions still being measured.
    #[serde(default)]
    pub experimental_dimensions: Vec<String>,
}

/// One student's accuracy inside one rubric segment, against the same
/// student's overall rate. `contrast` is the only field rules may fire on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ProfileRow {
    pub student: String,
    /// `bilissel_talep` | `dikkat_tuzagi` | `okuma_yuku` (production only).
    pub dimension: String,
    pub label: String,
    pub n_answers: i64,
    pub n_correct: i64,
    pub accuracy: f64,
    pub overall_n_answers: i64,
    pub overall_accuracy: f64,
    pub contrast: f64,
    /// `none` | `exploratory` | `stable`.
    pub confidence: String,
    pub computed_at: i64,
    pub retain_until: i64,
}

/// One compute run's ledger row, with the run's own children.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct RunRow {
    /// `YYYY-MM-DD` (TR day). The primary key: a re-run of the same night
    /// overwrites rather than duplicating.
    pub run_day: String,
    pub started_at: i64,
    #[serde(default)]
    pub finished_at: Option<i64>,
    /// `running` | `ok` | `partial` | `failed` | `skipped`.
    pub status: String,
    #[serde(default)]
    pub duration_ms: Option<i64>,
    pub students_total: i64,
    pub students_ok: i64,
    pub students_failed: i64,
    pub students_skipped: i64,
    pub rows_written: i64,
    pub budget_exceeded: bool,
    pub budget_ms: i64,
    pub retain_until: i64,
    /// Students the budget ran out on — the next run's starting point.
    #[serde(default)]
    pub pending_students: Vec<String>,
    /// Modules that failed in this run.
    #[serde(default)]
    pub failed_modules: Vec<String>,
}

// ---- the request/receipt envelopes ----------------------------------------

/// `insight.summary.upsert` / `POST /insights/summaries`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SummaryWriteRequest {
    pub rows: Vec<SummaryRow>,
}

/// `insight.recommendation.upsert` / `POST /insights/recommendations`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct RecommendationWriteRequest {
    pub rows: Vec<RecommendationRow>,
}

/// `insight.segment.upsert` / `POST /insights/segments`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SegmentWriteRequest {
    pub rows: Vec<SegmentRow>,
}

/// `insight.profile.upsert` / `POST /insights/profiles`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ProfileWriteRequest {
    pub rows: Vec<ProfileRow>,
}

/// `insight.run.upsert` / `POST /insights/runs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct RunWriteRequest {
    pub run: RunRow,
}

/// `insight.departed.purge` / `POST /insights/purge`. The **active** roster:
/// every student not named loses their derived rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct PurgeRequest {
    pub students: Vec<String>,
}

/// What a bulk write did: rows written, and (for recommendations only) rows
/// the service's own evidence rule kept out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct WriteReceipt {
    pub written: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rejected: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// The freshest run's pending students, in the service's order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct PendingList {
    pub students: Vec<String>,
}

/// The per-table verdict of a sweep or a purge, keyed by table name — the
/// shape the service's CLI prints and exits non-zero on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct TableVerdicts {
    pub tables: BTreeMap<String, bool>,
}

/// The deployment's active schools, by slug.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SchoolDirectory {
    pub schools: Vec<String>,
}

/// One school's own identity, as a rendered report prints it. The bridge
/// frame already names the school by slug, but a document that reaches a
/// manager's screen carries the display name, and the control row is the only
/// authority on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ReportSchool {
    /// `school.id` — the surrogate key, not the slug: a slug may be renamed.
    pub id: String,
    pub slug: String,
    pub name: String,
}

// ---- shared validation -----------------------------------------------------

/// Refuse a batch past [`MAX_INSIGHT_BATCH_ROWS`] rather than taking it
/// piecemeal: a batch is the unit the service retries, so accepting part of
/// one would turn its retry into a duplicate.
fn check_batch(field: &'static str, rows: &[impl Sized]) -> Result<(), AppError> {
    if rows.len() > MAX_INSIGHT_BATCH_ROWS {
        return Err(AppError::PayloadTooLarge(format!(
            "{field}: {} rows is over the ceiling of {MAX_INSIGHT_BATCH_ROWS} rows per call",
            rows.len()
        )));
    }
    Ok(())
}

/// A foreign-key violation is a payload error here, not a server fault: the
/// statement named a row this school's database does not have (most often an
/// id belonging to another school), and the caller can fix it.
fn as_validation(err: AppError) -> AppError {
    match &err {
        AppError::Db(source) if foreign_key_violation(source) => {
            AppError::Validation(ValidationError::Invalid {
                field: "rows",
                reason: "a referenced row does not exist in this school",
            })
        }
        _ => err,
    }
}

/// One hyphenated uuid from the wire. Ids cross the bridge as strings — the
/// `uuid` crate's serde support is not enabled here, and a string that is not
/// a uuid should read as a field error, not as a decode failure with no field.
fn parse_uuid(field: &'static str, value: &str) -> Result<Uuid, AppError> {
    Uuid::parse_str(value).map_err(|_| {
        AppError::Validation(ValidationError::Invalid {
            field,
            reason: "must be a hyphenated uuid",
        })
    })
}

/// `YYYY-MM-DD`, the ledger's key. Checked here so a malformed day is the
/// caller's `invalid_payload` and not a CHECK violation dressed as a `500`.
/// The report doors share it: a run day shapes a blob key there, and a
/// caller-supplied path segment that shapes a path is validated before it is
/// joined.
pub fn run_day_ok(day: &str) -> bool {
    let bytes = day.as_bytes();
    bytes.len() == 10
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| if matches!(i, 4 | 7) { *b == b'-' } else { b.is_ascii_digit() })
}

// ---- writes ----------------------------------------------------------------

/// Store a batch of student summaries, replacing each student's attention
/// items. One transaction: a batch lands whole or not at all.
pub async fn write_summaries(db: &Database, rows: Vec<SummaryRow>) -> Result<WriteReceipt, AppError> {
    check_batch("rows", &rows)?;
    let written = rows.len() as u64;
    tx_with_retry(db, false, async move |tx| write_summaries_in(tx, &rows).await)
        .await
        .map_err(as_validation)?;
    Ok(WriteReceipt {
        written,
        rejected: 0,
    })
}

/// The transaction body of [`write_summaries`], a named fn rather than an async
/// closure: an `AsyncFnMut` closure whose future does the work trips rustc's
/// "implementation of `Send` is not general enough" (see `tx_with_retry`).
async fn write_summaries_in(tx: &mut sqlx::PgConnection, rows: &[SummaryRow]) -> Result<(), AppError> {

    for row in rows {
        let student = parse_uuid("student", &row.student)?;
        sqlx::query!(
            "INSERT INTO zeka_student_summary
               (student, marks, attendance, submission, study, confidence,
                computed_at, retain_until)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (student) DO UPDATE SET
               marks = EXCLUDED.marks, attendance = EXCLUDED.attendance,
               submission = EXCLUDED.submission, study = EXCLUDED.study,
               confidence = EXCLUDED.confidence, computed_at = EXCLUDED.computed_at,
               retain_until = EXCLUDED.retain_until",
            student,
            row.marks.clone(),
            row.attendance.clone(),
            row.submission.clone(),
            row.study.clone(),
            row.confidence,
            row.computed_at,
            row.retain_until,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "DELETE FROM zeka_attention_item WHERE student = $1",
            student,
        )
        .execute(&mut *tx)
        .await?;
        for (ord, item) in row.attention.iter().enumerate() {
            let course = item
                .course
                .as_deref()
                .map(|id| parse_uuid("course", id))
                .transpose()?;
            sqlx::query!(
                "INSERT INTO zeka_attention_item
                   (student, trigger, course, fact, window_from, window_to, evidence, ord)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                student,
                item.trigger,
                course,
                item.fact,
                item.window_from,
                item.window_to,
                item.evidence.clone(),
                ord as i16,
            )
            .execute(&mut *tx)
            .await?;
        }
    }
    Ok(())
}

/// The service's own evidence rule, applied where the write happens: a card
/// whose evidence carries nothing but its limitation is not a finding. Such a
/// row is counted `rejected` in the receipt and left unwritten — silently
/// dropping it would be the one thing this rule exists to prevent.
fn carries_evidence(evidence: &JsonValue) -> bool {
    evidence.as_object().is_some_and(|object| {
        object
            .iter()
            .any(|(key, value)| key != "limitation" && !value.is_null())
    })
}

/// Store a batch of recommendation cards. The backend mints each row's id:
/// an id belongs to whoever writes the row, and that is now this process.
pub async fn write_recommendations(
    db: &Database,
    rows: Vec<RecommendationRow>,
) -> Result<WriteReceipt, AppError> {
    check_batch("rows", &rows)?;
    let (written, rejected) =
        tx_with_retry(db, false, async move |tx| write_recommendations_in(tx, &rows).await)
            .await
            .map_err(as_validation)?;
    Ok(WriteReceipt { written, rejected })
}

/// The service's own evidence rule, applied where the write happens: a card
/// whose evidence carries nothing but its limitation is not a finding. Such a
/// row is counted `rejected` and left unwritten — silently dropping it would
/// be the one thing this rule exists to prevent. Returns `(written, rejected)`.
async fn write_recommendations_in(
    tx: &mut sqlx::PgConnection,
    rows: &[RecommendationRow],
) -> Result<(u64, u64), AppError> {
    let mut written = 0u64;
    let mut rejected = 0u64;
    for row in rows {
        if !carries_evidence(&row.evidence) {
            rejected += 1;
            continue;
        }
        let evidence = merge_limitation(&row.evidence, row.limitation.as_deref())?;
        let audience = parse_uuid("audience", &row.audience)?;
        let about = row
            .about
            .as_deref()
            .map(|id| parse_uuid("about", id))
            .transpose()?;
        let course = row
            .course
            .as_deref()
            .map(|id| parse_uuid("course", id))
            .transpose()?;
        sqlx::query!(
            "INSERT INTO zeka_recommendation
               (id, audience, product, rule_id, rule_version, scope, about, audience_role,
                course, evidence, confidence, created_at, expires_at, retain_until)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
             ON CONFLICT (audience, product, rule_id, about, scope) DO UPDATE SET
               rule_version = EXCLUDED.rule_version,
               audience_role = EXCLUDED.audience_role,
               course = EXCLUDED.course,
               evidence = EXCLUDED.evidence,
               confidence = EXCLUDED.confidence,
               created_at = EXCLUDED.created_at,
               expires_at = EXCLUDED.expires_at,
               retain_until = EXCLUDED.retain_until",
            next_uuid(),
            audience,
            row.product,
            row.rule_id,
            row.rule_version,
            row.scope,
            about,
            row.audience_role,
            course,
            evidence,
            row.confidence,
            row.computed_at,
            row.expires_at,
            row.retain_until,
        )
        .execute(&mut *tx)
        .await?;
        written += 1;
    }
    Ok((written, rejected))
}

/// The stored evidence object: the payload's own object with `limitation`
/// written into it, exactly as the service kept it. A payload whose `evidence`
/// is not an object cannot be merged and is refused.
fn merge_limitation(
    evidence: &JsonValue,
    limitation: Option<&str>,
) -> Result<JsonValue, AppError> {
    let mut object = evidence
        .as_object()
        .cloned()
        .ok_or(AppError::Validation(ValidationError::Invalid {
            field: "evidence",
            reason: "must be a JSON object",
        }))?;
    if let Some(limitation) = limitation {
        object.insert(
            "limitation".to_string(),
            JsonValue::String(limitation.to_string()),
        );
    }
    Ok(JsonValue::Object(object))
}

/// Store a batch of question segments, replacing each question's dimension
/// split.
pub async fn write_segments(db: &Database, rows: Vec<SegmentRow>) -> Result<WriteReceipt, AppError> {
    check_batch("rows", &rows)?;
    let written = rows.len() as u64;
    tx_with_retry(db, false, async move |tx| write_segments_in(tx, &rows).await)
        .await
        .map_err(as_validation)?;
    Ok(WriteReceipt {
        written,
        rejected: 0,
    })
}

/// The transaction body of [`write_segments`], a named fn rather than an async
/// closure: an `AsyncFnMut` closure whose future does the work trips rustc's
/// "implementation of `Send` is not general enough" (see `tx_with_retry`).
async fn write_segments_in(tx: &mut sqlx::PgConnection, rows: &[SegmentRow]) -> Result<(), AppError> {

    for row in rows {
        let question = parse_uuid("question", &row.question)?;
        let exam = parse_uuid("exam", &row.exam)?;
        let course = parse_uuid("course", &row.course)?;
        let subject = parse_uuid("subject", &row.subject)?;
        sqlx::query!(
            "INSERT INTO zeka_question_segment
               (question, exam, course, subject, bilissel_talep, dikkat_tuzagi, okuma_yuku,
                adim_sayisi, confidence_bilissel_talep, confidence_dikkat_tuzagi,
                confidence_okuma_yuku, confidence_adim_sayisi, confidence, trap_choice,
                rationale, model, prompt_version, variant, computed_at, retain_until)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                     $11, $12, $13, $14, $15, $16, $17, $18, $19, $20)
             ON CONFLICT (question) DO UPDATE SET
               exam = EXCLUDED.exam, course = EXCLUDED.course, subject = EXCLUDED.subject,
               bilissel_talep = EXCLUDED.bilissel_talep,
               dikkat_tuzagi = EXCLUDED.dikkat_tuzagi,
               okuma_yuku = EXCLUDED.okuma_yuku,
               adim_sayisi = EXCLUDED.adim_sayisi,
               confidence_bilissel_talep = EXCLUDED.confidence_bilissel_talep,
               confidence_dikkat_tuzagi = EXCLUDED.confidence_dikkat_tuzagi,
               confidence_okuma_yuku = EXCLUDED.confidence_okuma_yuku,
               confidence_adim_sayisi = EXCLUDED.confidence_adim_sayisi,
               confidence = EXCLUDED.confidence, trap_choice = EXCLUDED.trap_choice,
               rationale = EXCLUDED.rationale, model = EXCLUDED.model,
               prompt_version = EXCLUDED.prompt_version, variant = EXCLUDED.variant,
               computed_at = EXCLUDED.computed_at, retain_until = EXCLUDED.retain_until",
            question,
            exam,
            course,
            subject,
            row.labels.bilissel_talep,
            row.labels.dikkat_tuzagi,
            row.labels.okuma_yuku,
            row.labels.adim_sayisi,
            row.confidences.bilissel_talep,
            row.confidences.dikkat_tuzagi,
            row.confidences.okuma_yuku,
            row.confidences.adim_sayisi,
            row.confidence,
            row.trap_choice,
            row.rationale,
            row.model,
            row.prompt_version,
            row.variant,
            row.computed_at,
            row.retain_until,
        )
        .execute(&mut *tx)
        .await?;
        // A dimension that moved between downstream and experimental (or
        // a rubric version that dropped one) must not leave its old row
        // behind.
        sqlx::query!(
            "DELETE FROM zeka_question_segment_dimension WHERE question = $1",
            question,
        )
        .execute(&mut *tx)
        .await?;
        let mut ord: i16 = 0;
        for (role, names) in [
            ("downstream", &row.downstream_dimensions),
            ("experimental", &row.experimental_dimensions),
        ] {
            for name in names {
                sqlx::query!(
                    "INSERT INTO zeka_question_segment_dimension
                       (question, dimension, role, ord)
                     VALUES ($1, $2, $3, $4)",
                    question,
                    name,
                    role,
                    ord,
                )
                .execute(&mut *tx)
                .await?;
                ord += 1;
            }
        }
    }
    Ok(())
}

/// Store a batch of student segment profiles.
pub async fn write_profiles(db: &Database, rows: Vec<ProfileRow>) -> Result<WriteReceipt, AppError> {
    check_batch("rows", &rows)?;
    let written = rows.len() as u64;
    tx_with_retry(db, false, async move |tx| write_profiles_in(tx, &rows).await)
        .await
        .map_err(as_validation)?;
    Ok(WriteReceipt {
        written,
        rejected: 0,
    })
}

/// The transaction body of [`write_profiles`], a named fn rather than an async
/// closure: an `AsyncFnMut` closure whose future does the work trips rustc's
/// "implementation of `Send` is not general enough" (see `tx_with_retry`).
async fn write_profiles_in(tx: &mut sqlx::PgConnection, rows: &[ProfileRow]) -> Result<(), AppError> {

    for row in rows {
        let student = parse_uuid("student", &row.student)?;
        sqlx::query!(
            "INSERT INTO zeka_student_segment_profile
               (student, dimension, label, n_answers, n_correct, accuracy,
                overall_n_answers, overall_accuracy, contrast, confidence,
                computed_at, retain_until)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
             ON CONFLICT (student, dimension, label) DO UPDATE SET
               n_answers = EXCLUDED.n_answers, n_correct = EXCLUDED.n_correct,
               accuracy = EXCLUDED.accuracy,
               overall_n_answers = EXCLUDED.overall_n_answers,
               overall_accuracy = EXCLUDED.overall_accuracy,
               contrast = EXCLUDED.contrast, confidence = EXCLUDED.confidence,
               computed_at = EXCLUDED.computed_at, retain_until = EXCLUDED.retain_until",
            student,
            row.dimension,
            row.label,
            row.n_answers,
            row.n_correct,
            row.accuracy,
            row.overall_n_answers,
            row.overall_accuracy,
            row.contrast,
            row.confidence,
            row.computed_at,
            row.retain_until,
        )
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// Store one run's ledger row, replacing the run's pending/failed children.
pub async fn write_run(db: &Database, run: RunRow) -> Result<WriteReceipt, AppError> {
    if !run_day_ok(&run.run_day) {
        return Err(AppError::Validation(ValidationError::Invalid {
            field: "run_day",
            reason: "must be YYYY-MM-DD",
        }));
    }
    let pending: Vec<Uuid> = run
        .pending_students
        .iter()
        .map(|id| parse_uuid("pending_students", id))
        .collect::<Result<_, _>>()?;
    tx_with_retry(db, false, async move |tx| write_run_in(tx, &run, &pending).await)
        .await
        .map_err(as_validation)?;
    Ok(WriteReceipt {
        written: 1,
        rejected: 0,
    })
}

/// The transaction body of [`write_run`], a named fn rather than an async
/// closure: an `AsyncFnMut` closure whose future does the work trips rustc's
/// "implementation of `Send` is not general enough" (see `tx_with_retry`).
async fn write_run_in(tx: &mut sqlx::PgConnection, run: &RunRow, pending: &[Uuid]) -> Result<(), AppError> {

    sqlx::query!(
        "INSERT INTO zeka_run
           (run_day, started_at, finished_at, status, duration_ms, students_total,
            students_ok, students_failed, students_skipped, rows_written,
            budget_exceeded, budget_ms, retain_until)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
         ON CONFLICT (run_day) DO UPDATE SET
           started_at = EXCLUDED.started_at, finished_at = EXCLUDED.finished_at,
           status = EXCLUDED.status, duration_ms = EXCLUDED.duration_ms,
           students_total = EXCLUDED.students_total, students_ok = EXCLUDED.students_ok,
           students_failed = EXCLUDED.students_failed,
           students_skipped = EXCLUDED.students_skipped,
           rows_written = EXCLUDED.rows_written,
           budget_exceeded = EXCLUDED.budget_exceeded, budget_ms = EXCLUDED.budget_ms,
           retain_until = EXCLUDED.retain_until",
        run.run_day,
        run.started_at,
        run.finished_at,
        run.status,
        run.duration_ms,
        run.students_total,
        run.students_ok,
        run.students_failed,
        run.students_skipped,
        run.rows_written,
        run.budget_exceeded,
        run.budget_ms,
        run.retain_until,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!("DELETE FROM zeka_run_pending WHERE run = $1", run.run_day)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        "DELETE FROM zeka_run_failed_module WHERE run = $1",
        run.run_day
    )
    .execute(&mut *tx)
    .await?;
    for (ord, student) in pending.iter().enumerate() {
        sqlx::query!(
            "INSERT INTO zeka_run_pending (run, student, ord) VALUES ($1, $2, $3)",
            run.run_day,
            student,
            ord as i32,
        )
        .execute(&mut *tx)
        .await?;
    }
    for (ord, module) in run.failed_modules.iter().enumerate() {
        sqlx::query!(
            "INSERT INTO zeka_run_failed_module (run, module, ord) VALUES ($1, $2, $3)",
            run.run_day,
            module,
            ord as i16,
        )
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

// ---- reads -----------------------------------------------------------------

/// The freshest run's pending students, in the order the run recorded them.
///
/// Refused rather than clipped past [`MAX_INSIGHT_PENDING_STUDENTS`]: the
/// service resumes its next run from this list, so a shortened one would drop
/// students silently. The read asks for one row past the ceiling so the
/// refusal never has to materialize the whole list.
pub async fn last_pending(db: &Database) -> Result<PendingList, AppError> {
    let ceiling = MAX_INSIGHT_PENDING_STUDENTS as i64;
    let rows = sqlx::query_scalar!(
        "SELECT p.student FROM zeka_run_pending p
           JOIN zeka_run r ON r.run_day = p.run
          WHERE r.run_day = (SELECT run_day FROM zeka_run
                              ORDER BY started_at DESC, run_day DESC LIMIT 1)
          ORDER BY p.ord
          LIMIT $1",
        ceiling + 1,
    )
    .fetch_all(db)
    .await?;
    if rows.len() > MAX_INSIGHT_PENDING_STUDENTS {
        return Err(AppError::PayloadTooLarge(format!(
            "students: the freshest run left more than {MAX_INSIGHT_PENDING_STUDENTS} students pending"
        )));
    }
    Ok(PendingList {
        students: rows.iter().map(|id| id.to_string()).collect(),
    })
}

/// The deployment's active schools, by slug — the one read that is not
/// scoped to a school, because it is how a shared AI fleet learns which
/// schools exist.
pub async fn active_schools(control: &Database) -> Result<SchoolDirectory, AppError> {
    let schools = sqlx::query_scalar!(
        "SELECT slug FROM school WHERE status = 'active' ORDER BY slug"
    )
    .fetch_all(control)
    .await?;
    Ok(SchoolDirectory { schools })
}

/// One school's identity row, by slug. `None` only for a slug the control
/// database does not hold — a resolved tenant always has one, so a caller
/// reads that as an internal fault, not as a refusal.
pub async fn school_identity(
    control: &Database,
    slug: &str,
) -> Result<Option<ReportSchool>, AppError> {
    let row = sqlx::query!("SELECT id, name FROM school WHERE slug = $1", slug)
        .fetch_optional(control)
        .await?;
    Ok(row.map(|row| ReportSchool {
        id: row.id.to_string(),
        slug: slug.to_string(),
        name: row.name,
    }))
}

// ---- the school-wide reads the report payload carries -----------------------
//
// Every read above is per-student or per-audience, because that is every
// reader this nest had. A *document* about the school is the first caller that
// needs the whole row set at once, so these four project each table straight
// into the wire rows the bridge payload carries ([`SummaryRow`],
// [`RecommendationRow`], [`ProfileRow`], [`RunRow`]) — one mapping, in one
// place, shared by SQL and the wire. No filters: the document's own reader
// (the service's report package) decides what it shows, and a backend that
// pre-filtered would be a second, silent author of the report.

/// Every stored summary with its attention items, in student order. The
/// attention list is one query for the whole set, grouped by student — not
/// one query per summary.
pub async fn all_summaries(db: &Database) -> Result<Vec<SummaryRow>, AppError> {
    let rows = sqlx::query!(
        r#"SELECT student,
                  marks AS "marks?: SqlJson<JsonValue>",
                  attendance AS "attendance?: SqlJson<JsonValue>",
                  submission AS "submission?: SqlJson<JsonValue>",
                  study AS "study?: SqlJson<JsonValue>",
                  confidence, computed_at, retain_until
           FROM zeka_student_summary ORDER BY student"#
    )
    .fetch_all(db)
    .await?;
    let mut attention: BTreeMap<Uuid, Vec<AttentionRow>> = BTreeMap::new();
    for row in sqlx::query!(
        r#"SELECT student, trigger, course, fact, window_from, window_to,
                  evidence AS "evidence: SqlJson<JsonValue>"
           FROM zeka_attention_item ORDER BY student, ord"#
    )
    .fetch_all(db)
    .await?
    {
        attention.entry(row.student).or_default().push(AttentionRow {
            trigger: row.trigger,
            course: row.course.map(|course| course.to_string()),
            fact: row.fact,
            window_from: row.window_from,
            window_to: row.window_to,
            evidence: row.evidence.0,
        });
    }
    Ok(rows
        .into_iter()
        .map(|row| SummaryRow {
            attention: attention.remove(&row.student).unwrap_or_default(),
            student: row.student.to_string(),
            marks: row.marks.map(|json| json.0),
            attendance: row.attendance.map(|json| json.0),
            submission: row.submission.map(|json| json.0),
            study: row.study.map(|json| json.0),
            confidence: row.confidence,
            computed_at: row.computed_at,
            retain_until: row.retain_until,
        })
        .collect())
}

/// Every stored card, whatever its audience, in a stable order. The write
/// merged each row's `limitation` into its stored evidence object, so the
/// read leaves the [write-side](RecommendationRow::limitation) field empty —
/// a second copy here could only disagree with the one inside `evidence`.
///
/// The wire row carries no `dismissed_at`, so the stored dismissal trail
/// (`dismissed_at`/`dismiss_by`/`dismiss_reason` on the table) does not
/// travel: a consumer that gates on dismissal cannot fire on this read. That
/// is a known gap of the report payload's contract — the reader that filters
/// dismissals is the per-audience read ([`crate::web::insights`]), and
/// widening the row here is a contract change this lane does not make.
pub async fn all_recommendations(db: &Database) -> Result<Vec<RecommendationRow>, AppError> {
    let rows = sqlx::query!(
        r#"SELECT audience, product, rule_id, rule_version, scope, about, audience_role,
                  course, evidence AS "evidence: SqlJson<JsonValue>",
                  confidence, created_at, expires_at, retain_until
           FROM zeka_recommendation ORDER BY audience, created_at, rule_id"#
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| RecommendationRow {
            audience: row.audience.to_string(),
            product: row.product,
            rule_id: row.rule_id,
            rule_version: row.rule_version,
            scope: row.scope,
            about: row.about.map(|about| about.to_string()),
            audience_role: row.audience_role,
            course: row.course.map(|course| course.to_string()),
            evidence: row.evidence.0,
            limitation: None,
            confidence: row.confidence,
            // The column is `created_at`; the wire row spells it `computed_at`,
            // and the write maps the same pair the same way.
            computed_at: row.created_at,
            expires_at: row.expires_at,
            retain_until: row.retain_until,
        })
        .collect())
}

/// Every stored segment profile, in student order. Noise rows (`confidence =
/// 'none'`) are included: this read feeds a document, and the display floor is
/// the reader's rule, applied where the reader applies it.
pub async fn all_profiles(db: &Database) -> Result<Vec<ProfileRow>, AppError> {
    let rows = sqlx::query!(
        r#"SELECT student, dimension, label, n_answers, n_correct, accuracy,
                  overall_n_answers, overall_accuracy, contrast, confidence,
                  computed_at, retain_until
           FROM zeka_student_segment_profile ORDER BY student, dimension, label"#
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| ProfileRow {
            student: row.student.to_string(),
            dimension: row.dimension,
            label: row.label,
            n_answers: row.n_answers,
            n_correct: row.n_correct,
            accuracy: row.accuracy,
            overall_n_answers: row.overall_n_answers,
            overall_accuracy: row.overall_accuracy,
            contrast: row.contrast,
            confidence: row.confidence,
            computed_at: row.computed_at,
            retain_until: row.retain_until,
        })
        .collect())
}

/// How many ledger rows a report carries: the named day plus recent context.
/// Deliberately small — the ledger is context for the document, which is
/// about one day, not a ledger dump.
const REPORT_RUNS: i64 = 5;

/// The run ledger a report carries, with each run's children. The named day
/// leads when it exists (a report about it must be able to show it even if it
/// has scrolled out of the newest rows), then the newest days follow.
pub async fn runs_for_report(db: &Database, run_day: &str) -> Result<Vec<RunRow>, AppError> {
    let rows = sqlx::query!(
        r#"SELECT run_day, started_at, finished_at, status, duration_ms,
                  students_total, students_ok, students_failed, students_skipped,
                  rows_written, budget_exceeded, budget_ms, retain_until
           FROM zeka_run
           ORDER BY (run_day = $1) DESC, started_at DESC, run_day DESC
           LIMIT $2"#,
        run_day,
        REPORT_RUNS,
    )
    .fetch_all(db)
    .await?;
    let days: Vec<String> = rows.iter().map(|row| row.run_day.clone()).collect();
    let mut pending: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in sqlx::query!(
        r#"SELECT run, student FROM zeka_run_pending
           WHERE run = ANY($1) ORDER BY run, ord"#,
        &days
    )
    .fetch_all(db)
    .await?
    {
        pending
            .entry(row.run)
            .or_default()
            .push(row.student.to_string());
    }
    let mut failed: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in sqlx::query!(
        r#"SELECT run, module FROM zeka_run_failed_module
           WHERE run = ANY($1) ORDER BY run, ord"#,
        &days
    )
    .fetch_all(db)
    .await?
    {
        failed.entry(row.run).or_default().push(row.module);
    }
    Ok(rows
        .into_iter()
        .map(|row| RunRow {
            pending_students: pending.remove(&row.run_day).unwrap_or_default(),
            failed_modules: failed.remove(&row.run_day).unwrap_or_default(),
            run_day: row.run_day,
            started_at: row.started_at,
            finished_at: row.finished_at,
            status: row.status,
            duration_ms: row.duration_ms,
            students_total: row.students_total,
            students_ok: row.students_ok,
            students_failed: row.students_failed,
            students_skipped: row.students_skipped,
            rows_written: row.rows_written,
            budget_exceeded: row.budget_exceeded,
            budget_ms: row.budget_ms,
            retain_until: row.retain_until,
        })
        .collect())
}

// ---- retention -------------------------------------------------------------

/// The nine tables a sweep walks, children before their parents: the foreign
/// keys are `ON DELETE NO ACTION`, so a parent cannot go while a child row
/// still names it.
const SWEEP_CHILDREN: &[(&str, &str, &str)] = &[
    ("zeka_attention_item", "zeka_student_summary", "student"),
    ("zeka_question_segment_dimension", "zeka_question_segment", "question"),
    ("zeka_run_pending", "zeka_run", "run"),
    ("zeka_run_failed_module", "zeka_run", "run"),
];

/// The tables that carry their own `retain_until`.
const SWEEP_PARENTS: &[&str] = &[
    "zeka_student_summary",
    "zeka_recommendation",
    "zeka_run",
    "zeka_question_segment",
    "zeka_student_segment_profile",
];

/// Delete every row whose retention window has closed. `now_ms` is the
/// backend's own clock — one clock decides expiry, never the caller's.
///
/// One table at a time, each in its own statement, and a table whose delete
/// fails is reported `false` rather than aborting the rest: a sweep is
/// housekeeping, and losing the whole night's cleanup because one table is
/// locked would leave every other table's expired rows in place. The caller
/// reads the verdict per table, exactly as the service's own sweep did.
pub async fn sweep(db: &Database, now_ms: i64) -> Result<TableVerdicts, AppError> {
    let mut tables = BTreeMap::new();
    for (child, parent, key) in SWEEP_CHILDREN {
        let parent_key = if *parent == "zeka_run" { "run_day" } else { *key };
        let sql = format!(
            "DELETE FROM {child} WHERE {key} IN \
             (SELECT {parent_key} FROM {parent} WHERE retain_until < $1)"
        );
        tables.insert(
            (*child).to_string(),
            run_sweep_statement(db, &sql, now_ms).await,
        );
    }
    for table in SWEEP_PARENTS {
        let sql = format!("DELETE FROM {table} WHERE retain_until < $1");
        tables.insert((*table).to_string(), run_sweep_statement(db, &sql, now_ms).await);
    }
    Ok(TableVerdicts { tables })
}

/// The table names come from [`SWEEP_CHILDREN`]/[`SWEEP_PARENTS`], never from
/// a caller — the only dynamic SQL in this module, and the reason
/// [`AssertSqlSafe`] is licensed here.
async fn run_sweep_statement(db: &Database, sql: &str, now_ms: i64) -> bool {
    sqlx::query(AssertSqlSafe(sql.to_string()))
        .bind(now_ms)
        .execute(db)
        .await
        .is_ok()
}

/// Delete the derived rows of everyone who is no longer on the roster. One
/// transaction: a purge is a decision about a cohort, not a per-row repair.
///
/// The empty list is **refused**, never obeyed: an empty roster is almost
/// always a fetch that failed, and reading it as "nobody is enrolled any
/// more" would delete every student's derived data in one call.
pub async fn purge_departed(
    db: &Database,
    students: Vec<String>,
) -> Result<TableVerdicts, AppError> {
    if students.is_empty() {
        return Err(AppError::Validation(ValidationError::Empty("students")));
    }
    if students.len() > MAX_INSIGHT_PURGE_STUDENTS {
        return Err(AppError::PayloadTooLarge(format!(
            "students: {} ids is over the ceiling of {MAX_INSIGHT_PURGE_STUDENTS} per call",
            students.len()
        )));
    }
    let active: Vec<Uuid> = students
        .iter()
        .map(|id| parse_uuid("students", id))
        .collect::<Result<_, _>>()?;
    tx_with_retry(db, false, async move |tx| purge_departed_in(tx, &active).await)
        .await
        .map_err(as_validation)?;

    // One transaction, so the verdicts move together: every table succeeded
    // or the call was refused above.
    let tables = [
        "zeka_attention_item",
        "zeka_student_summary",
        "zeka_student_segment_profile",
        "zeka_recommendation",
        "zeka_run_pending",
    ]
    .into_iter()
    .map(|table| (table.to_string(), true))
    .collect();
    Ok(TableVerdicts { tables })
}

/// The transaction body of [`purge_departed`], a named fn rather than an async
/// closure: an `AsyncFnMut` closure whose future does the work trips rustc's
/// "implementation of `Send` is not general enough" (see `tx_with_retry`).
async fn purge_departed_in(tx: &mut sqlx::PgConnection, active: &[Uuid]) -> Result<(), AppError> {

    // Children first (they reference the summary), then the parents.
    sqlx::query!(
        "DELETE FROM zeka_attention_item WHERE student <> ALL($1)",
        active,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM zeka_student_summary WHERE student <> ALL($1)",
        active,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM zeka_student_segment_profile WHERE student <> ALL($1)",
        active,
    )
    .execute(&mut *tx)
    .await?;
    // Cards about a departed student go; school-wide cards (`about` is
    // NULL) are addressed to someone still enrolled and stay.
    sqlx::query!(
        "DELETE FROM zeka_recommendation
          WHERE about IS NOT NULL AND about <> ALL($1)",
        active,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM zeka_run_pending WHERE student <> ALL($1)",
        active,
    )
    .execute(&mut *tx)
    .await?;
    Ok(())
}
