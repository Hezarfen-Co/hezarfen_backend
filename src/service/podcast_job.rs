//! Podcast job workflows: the submit write that mints a job, the report write
//! the service's `podcast.report` calls land through, the cancel/fail writes
//! behind the HTTP verdicts, and the audio reference an ingest stamps.
//!
//! The queries live in [`crate::db::podcast_job`]. Everything that decides
//! *whether* a write may happen lives here: the report path is the trust
//! boundary between the service and this table, so it validates the sender's
//! echo (user, source), the transition (`jobs.py`'s table, via the domain
//! enum), the retention window, and the "done means audio" rule — in that
//! order — before a single statement runs.

use crate::database::Database;
use crate::db::podcast_job;
use crate::domain::podcast_job::{PodcastJob, PodcastJobId, PodcastJobState};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;
use crate::validate::validate_required;

/// The most characters a report may put in `stage` / `error_code`. Generous
/// for the service's own short codes and stage names; the bound exists so a
/// service cannot write a paragraph into a column a UI renders inline.
const MAX_CODE_LEN: usize = 64;

/// Why one `podcast.report` (or an audio ingest) was refused. `code()` is the
/// flat, stable, machine-readable string the bridge answers in the
/// capability's `code` field — the same vocabulary the protocol's other
/// refusals use. Never nested, never localized.
#[derive(Debug)]
pub enum PodcastRefusal {
    /// No job with that id exists in the school the frame named — including a
    /// real id belonging to another school, which reads the same on purpose.
    UnknownJob,
    /// The report does not describe a state this service could have produced:
    /// an unknown state name, an out-of-range progress, an over-long code, or
    /// a transition the state machine forbids.
    InvalidPayload(String),
    /// The sender's echo of the job disagrees with the row — a report for a
    /// job under a different user or source.
    NotPermitted(String),
    /// A `done` report arrived before its audio was ingested. The row must
    /// never claim a finished episode whose bytes are not there.
    AudioMissing,
    /// The job aged out of the retention window; its blob is no longer served
    /// and its state is frozen history.
    Expired,
    /// The store itself refused — nothing the service can fix by retrying
    /// differently.
    Unavailable(String),
}

impl PodcastRefusal {
    pub fn code(&self) -> &'static str {
        match self {
            PodcastRefusal::UnknownJob => "unknown_job",
            PodcastRefusal::InvalidPayload(_) => "invalid_payload",
            PodcastRefusal::NotPermitted(_) => "not_permitted",
            PodcastRefusal::AudioMissing => "audio_missing",
            PodcastRefusal::Expired => "expired",
            PodcastRefusal::Unavailable(_) => "unavailable",
        }
    }

    pub fn message(&self) -> String {
        match self {
            PodcastRefusal::UnknownJob => "no such podcast job".to_string(),
            PodcastRefusal::InvalidPayload(why) => why.clone(),
            PodcastRefusal::NotPermitted(why) => why.clone(),
            PodcastRefusal::AudioMissing => {
                "a done report requires the episode to be uploaded first".to_string()
            }
            PodcastRefusal::Expired => "this podcast job has expired".to_string(),
            PodcastRefusal::Unavailable(why) => why.clone(),
        }
    }
}

impl From<AppError> for PodcastRefusal {
    fn from(err: AppError) -> Self {
        PodcastRefusal::Unavailable(err.to_string())
    }
}

/// One state report from the service, as [`crate::ai::podcast`] decoded it.
/// The borrowed strings are the wire's own; nothing here is trusted until
/// [`report`] has checked it against the row.
#[derive(Debug)]
pub struct ReportInput<'a> {
    pub job_id: &'a str,
    pub source_id: &'a str,
    pub format: Option<&'a str>,
    pub user_id: &'a str,
    pub state: &'a str,
    pub stage: &'a str,
    pub progress: f64,
    pub error_code: Option<&'a str>,
}

/// Mint and store one freshly submitted job. The id is the backend's (the
/// service is handed it in the submit payload), and the row starts `queued`
/// with no ETA — the service's own estimate lands a moment later, with
/// [`set_eta`], once it has accepted the job.
pub async fn create(
    db: &Database,
    user: &UserId,
    source_id: &str,
    format: Option<&str>,
) -> Result<PodcastJob, AppError> {
    let now = Timestamp::now();
    let job = PodcastJob {
        id: PodcastJobId::generate(),
        user_id: *user,
        source_id: source_id.to_string(),
        format: format.map(str::to_string),
        state: PodcastJobState::Queued,
        stage: String::new(),
        progress: 0.0,
        error_code: None,
        audio_key: None,
        audio_name: None,
        audio_type: None,
        audio_bytes: None,
        duration_secs: None,
        eta_secs: None,
        created_at: now,
        updated_at: now,
    };
    podcast_job::insert(db, &job).await
}

/// Store the service's own ETA on the job it just accepted — the number the
/// read-side staleness window is scaled by, echoed back to the submitter.
pub async fn set_eta(
    db: &Database,
    id: &PodcastJobId,
    eta_secs: i64,
) -> Result<Option<PodcastJob>, AppError> {
    podcast_job::set_eta(db, id, eta_secs.max(0)).await
}

/// Read a job only if `user` owns it — a foreign id reads as absent, so the
/// HTTP doors answer 404 rather than leaking that it exists.
pub async fn read_for(
    db: &Database,
    id: &PodcastJobId,
    user: &UserId,
) -> Result<Option<PodcastJob>, AppError> {
    podcast_job::read_for(db, id, user).await
}

/// A user's own jobs, newest first, each paired with the title of the course
/// note it narrates — `None` once that note is gone. The pairing is this
/// list's whole extra read; everything else is the row.
pub async fn list_for_user(
    db: &Database,
    user: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<(PodcastJob, Option<String>)>, i64), AppError> {
    podcast_job::list_for_user(db, user, limit, offset).await
}

/// Apply one report from the service. The order of the checks is the point:
/// shape first (so nothing malformed reaches a query), then existence, then
/// the sender's own echo, then the retention window, then the transition, and
/// finally "done means audio" — the rule the schema's CHECK also backstops.
///
/// The write itself is conditional on the state just read (`from`), so a
/// cancel racing this report cannot be overwritten by it; when the condition
/// loses, the refusal names the state that actually holds.
pub async fn report(
    db: &Database,
    input: &ReportInput<'_>,
) -> Result<PodcastJob, PodcastRefusal> {
    let id = PodcastJobId::from_key(input.job_id);
    let Some(target) = PodcastJobState::parse(input.state) else {
        return Err(PodcastRefusal::InvalidPayload(format!(
            "`{}` is not a known job state",
            input.state
        )));
    };
    if !input.progress.is_finite() || !(0.0..=1.0).contains(&input.progress) {
        return Err(PodcastRefusal::InvalidPayload(
            "progress must be a fraction between 0 and 1".to_string(),
        ));
    }
    if input.stage.chars().count() > MAX_CODE_LEN {
        return Err(PodcastRefusal::InvalidPayload(
            "stage is longer than 64 characters".to_string(),
        ));
    }
    if let Some(code) = input.error_code
        && code.chars().count() > MAX_CODE_LEN
    {
        return Err(PodcastRefusal::InvalidPayload(
            "error_code is longer than 64 characters".to_string(),
        ));
    }

    let Some(row) = podcast_job::read(db, &id).await? else {
        return Err(PodcastRefusal::UnknownJob);
    };
    if row.get_user_id().key() != input.user_id {
        return Err(PodcastRefusal::NotPermitted(
            "the job belongs to another user".to_string(),
        ));
    }
    if row.get_source_id() != input.source_id {
        return Err(PodcastRefusal::NotPermitted(
            "the job's source does not match this report".to_string(),
        ));
    }
    if let (Some(stored), Some(reported)) = (row.get_format(), input.format)
        && stored != reported
    {
        return Err(PodcastRefusal::NotPermitted(
            "the job's format was already resolved and this report disagrees".to_string(),
        ));
    }
    if row.is_expired() {
        return Err(PodcastRefusal::Expired);
    }
    if !row.get_state().can_transition_to(target) {
        return Err(PodcastRefusal::InvalidPayload(format!(
            "`{}` cannot move to `{}`",
            row.get_state().as_str(),
            target.as_str()
        )));
    }
    if target == PodcastJobState::Done && row.get_audio_key().is_none() {
        return Err(PodcastRefusal::AudioMissing);
    }

    match podcast_job::report(
        db,
        &id,
        row.get_state(),
        target,
        input.stage,
        input.progress,
        input.error_code,
        input.format,
    )
    .await?
    {
        Some(updated) => Ok(updated),
        // The row moved between the read and the write (a cancel, most
        // likely). Nothing was stored; name the state that actually holds.
        None => match podcast_job::read(db, &id).await? {
            Some(current) => Err(PodcastRefusal::InvalidPayload(format!(
                "the job moved to `{}` while this report was in flight",
                current.get_state().as_str()
            ))),
            None => Err(PodcastRefusal::UnknownJob),
        },
    }
}

/// Take the backend's own cancel write: only a live row moves, and `None`
/// (already terminal, or gone) is the `cancelled: false` verdict.
pub async fn mark_cancelled(
    db: &Database,
    id: &PodcastJobId,
) -> Result<Option<PodcastJob>, AppError> {
    podcast_job::set_cancelled(db, id).await
}

/// Mark a live job failed with a backend-side reason code — a dispatch that
/// never reached a worker, or a service that lost a job its row still calls
/// live.
pub async fn mark_failed(
    db: &Database,
    id: &PodcastJobId,
    error_code: &str,
) -> Result<Option<PodcastJob>, AppError> {
    podcast_job::set_failed(db, id, error_code).await
}

/// Validate one upload's metadata before a byte is written: the name and
/// content type a browser will see, and the size bound. `Err` carries the flat
/// refusal code the bridge answers.
pub fn validate_audio(
    name: &str,
    content_type: &str,
    size: u64,
) -> Result<(), PodcastRefusal> {
    if let Err(why) = validate_required("name", name, 200) {
        return Err(PodcastRefusal::InvalidPayload(why.to_string()));
    }
    if !content_type.starts_with("audio/") {
        return Err(PodcastRefusal::InvalidPayload(format!(
            "`{content_type}` is not an audio content type"
        )));
    }
    if size == 0 {
        return Err(PodcastRefusal::InvalidPayload(
            "an empty episode is not an episode".to_string(),
        ));
    }
    if size > crate::constant::PODCAST_AUDIO_MAX_BYTES as u64 {
        return Err(PodcastRefusal::InvalidPayload(format!(
            "the upload is larger than {} bytes",
            crate::constant::PODCAST_AUDIO_MAX_BYTES
        )));
    }
    Ok(())
}

/// The blob key an uploaded episode is stored under: `podcast/<job-id>.<ext>`.
/// The extension comes off the service's own file name when it is a short,
/// boring one, and is otherwise derived from the content type — never copied
/// verbatim, so nothing the service sends can shape a path.
pub fn audio_key(job_id: &PodcastJobId, name: &str, content_type: &str) -> String {
    let extension = name
        .rsplit_once('.')
        .map(|(_, ext)| ext)
        .filter(|ext| {
            (1..=5).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric())
        })
        .map(|ext| ext.to_ascii_lowercase())
        .unwrap_or_else(|| match content_type {
            "audio/mpeg" => "mp3".to_string(),
            "audio/mp4" => "m4a".to_string(),
            "audio/aac" => "aac".to_string(),
            "audio/wav" | "audio/x-wav" => "wav".to_string(),
            "audio/ogg" => "ogg".to_string(),
            "audio/opus" => "opus".to_string(),
            _ => "bin".to_string(),
        });
    format!("podcast/{}.{extension}", job_id.key())
}

/// Stamp the ingested episode onto its row, after the bytes are on disk.
pub async fn set_audio(
    db: &Database,
    id: &PodcastJobId,
    key: &str,
    name: &str,
    content_type: &str,
    bytes: i64,
    duration_secs: Option<f64>,
) -> Result<Option<PodcastJob>, AppError> {
    podcast_job::set_audio(db, id, key, name, content_type, bytes, duration_secs).await
}
