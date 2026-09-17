//! One podcast job — the record the backend owns from the moment a caller
//! submits to the moment the audio is collected, plus its read-time
//! presentation.
//!
//! The service reports every transition back over the bridge
//! (`podcast.report`), but the row is written *first*, by the submit handler,
//! and every read door (`podcast.status`, `podcast.result`, the audio stream)
//! answers from this table alone. That is the whole point of the row: a
//! service restart — or a wiped service volume — costs no record, no job id,
//! and no audio bytes.
//!
//! A service that dies *without* reporting is what [`PodcastJob::projected`]
//! answers: a job left `queued`/`running` longer than its own ETA allows is
//! presented as `failed`/`interrupted` — on the read side only, never written —
//! the same device [`crate::domain::rag_message`] uses for a stranded turn.
//! The durable repair still belongs to the service: it sweeps its own store at
//! boot and reports each swept job, which overwrites the projection.
//!
//! The state vocabulary and the transition table are the service's own
//! (`jobs.py`'s `STATES`/`TRANSITIONS`), mirrored here so a report the service
//! could not have produced is refused instead of stored.

use crate::constant::{
    PODCAST_INTERRUPTED_CODE, PODCAST_JOB_RETENTION_SECS, PODCAST_JOB_STALE_ETA_FACTOR,
    PODCAST_JOB_STALE_FLOOR_SECS,
};
use crate::domain::monotonic_id::next_uuid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(transparent)]
pub struct PodcastJobId(uuid::Uuid);

impl PodcastJobId {
    /// Mints from the process-wide monotonic generator: job ids are also the
    /// service's record keys and the audio blob's file name, and a monotonic
    /// UUID keeps a directory listing of episodes in submit order.
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    /// The inner uuid, for runtime-checked binds (Param/QueryBuilder) that
    /// cannot take the newtype. Static `query!` binds take `self` directly.
    pub fn uuid(&self) -> uuid::Uuid {
        self.0
    }

    /// Parses a wire key. A key that is not a UUID parses as the nil UUID,
    /// which matches no row — the same rule every other id in the codebase
    /// follows.
    pub fn from_key(key: &str) -> Self {
        Self(uuid::Uuid::parse_str(key).unwrap_or(uuid::Uuid::nil()))
    }

    /// The wire form: the standard hyphenated lowercase string. This is also
    /// the chain the service stores its own record under and the audio blob's
    /// stem, so it must stay byte-stable across every surface.
    pub fn key(&self) -> String {
        self.0.to_string()
    }
}

/// Where a job is in its lifecycle. Stored as the bare lowercase string the
/// `state` column's CHECK allows; a value the enum doesn't know comes back as
/// a decode error, never a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
pub enum PodcastJobState {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl PodcastJobState {
    /// The wire/storage form. Must stay in lockstep with `rename_all`.
    pub fn as_str(self) -> &'static str {
        match self {
            PodcastJobState::Queued => "queued",
            PodcastJobState::Running => "running",
            PodcastJobState::Done => "done",
            PodcastJobState::Failed => "failed",
            PodcastJobState::Cancelled => "cancelled",
        }
    }

    /// Parses a report's own `state` string. `None` is a value the service
    /// should never have produced — the report is refused, not coerced.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "queued" => Some(PodcastJobState::Queued),
            "running" => Some(PodcastJobState::Running),
            "done" => Some(PodcastJobState::Done),
            "failed" => Some(PodcastJobState::Failed),
            "cancelled" => Some(PodcastJobState::Cancelled),
            _ => None,
        }
    }

    /// Done, failed or cancelled — the states a job never leaves.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            PodcastJobState::Done | PodcastJobState::Failed | PodcastJobState::Cancelled
        )
    }

    /// Can a report move a row from `self` to `target`? `jobs.py`'s
    /// `TRANSITIONS`, plus the identity transition: a report may repeat the
    /// state it last reported (a bridge round trip can time out after the
    /// service committed, and the retry must not be a refusal).
    pub fn can_transition_to(self, target: PodcastJobState) -> bool {
        if target == self {
            return true;
        }
        match self {
            PodcastJobState::Queued => matches!(
                target,
                PodcastJobState::Running | PodcastJobState::Cancelled | PodcastJobState::Failed
            ),
            PodcastJobState::Running => matches!(
                target,
                PodcastJobState::Done | PodcastJobState::Failed | PodcastJobState::Cancelled
            ),
            PodcastJobState::Done | PodcastJobState::Failed | PodcastJobState::Cancelled => false,
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PodcastJob {
    pub(crate) id: PodcastJobId,
    /// The submitting user. The ownership guard every read door applies.
    pub(crate) user_id: UserId,
    pub(crate) source_id: String,
    pub(crate) format: Option<String>,
    pub(crate) state: PodcastJobState,
    pub(crate) stage: String,
    pub(crate) progress: f64,
    pub(crate) error_code: Option<String>,
    pub(crate) audio_key: Option<String>,
    pub(crate) audio_name: Option<String>,
    pub(crate) audio_type: Option<String>,
    pub(crate) audio_bytes: Option<i64>,
    pub(crate) duration_secs: Option<f64>,
    pub(crate) eta_secs: Option<i64>,
    pub(crate) created_at: Timestamp,
    pub(crate) updated_at: Timestamp,
}

impl PodcastJob {
    pub fn get_id(&self) -> &PodcastJobId {
        &self.id
    }

    pub fn get_user_id(&self) -> &UserId {
        &self.user_id
    }

    pub fn get_source_id(&self) -> &str {
        &self.source_id
    }

    pub fn get_format(&self) -> Option<&str> {
        self.format.as_deref()
    }

    pub fn get_state(&self) -> PodcastJobState {
        self.state
    }

    pub fn get_stage(&self) -> &str {
        &self.stage
    }

    pub fn get_progress(&self) -> f64 {
        self.progress
    }

    pub fn get_error_code(&self) -> Option<&str> {
        self.error_code.as_deref()
    }

    pub fn get_audio_key(&self) -> Option<&str> {
        self.audio_key.as_deref()
    }

    pub fn get_audio_name(&self) -> Option<&str> {
        self.audio_name.as_deref()
    }

    pub fn get_audio_type(&self) -> Option<&str> {
        self.audio_type.as_deref()
    }

    pub fn get_audio_bytes(&self) -> Option<i64> {
        self.audio_bytes
    }

    pub fn get_duration_secs(&self) -> Option<f64> {
        self.duration_secs
    }

    pub fn get_eta_secs(&self) -> Option<i64> {
        self.eta_secs
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }

    pub fn get_updated_at(&self) -> Timestamp {
        self.updated_at
    }

    /// The job as a read should present it, with no write behind it: a
    /// `queued`/`running` row nobody has updated inside its own staleness
    /// window is presented as `failed`/`interrupted`. The window scales with
    /// the service's own ETA — `max(floor, eta_secs × factor)` — so a long
    /// episode (or a queue of them) is never declared dead mid-flight, while a
    /// service that died takes its jobs down with it within a bounded,
    /// ETA-aware time.
    ///
    /// Only the returned copy is projected; the row is untouched, and the
    /// service's next report overwrites the projection the moment it lands.
    pub fn projected(mut self) -> Self {
        if self.state.is_terminal() {
            return self;
        }
        let eta_secs = self.eta_secs.unwrap_or(0).max(0);
        let window_ms = eta_secs
            .saturating_mul(PODCAST_JOB_STALE_ETA_FACTOR)
            .max(PODCAST_JOB_STALE_FLOOR_SECS)
            * 1_000;
        if Timestamp::now().as_millis() > self.updated_at.as_millis() + window_ms {
            self.state = PodcastJobState::Failed;
            self.error_code = Some(PODCAST_INTERRUPTED_CODE.to_string());
            self.stage = String::new();
        }
        self
    }

    /// Has this job aged out of the retention window? Terminal rows older than
    /// [`PODCAST_JOB_RETENTION_SECS`] — measured from the write that finished
    /// them — are refused `410`, and the audio door will not stream their blob.
    /// A read-side test only; nothing is swept on the read path.
    pub fn is_expired(&self) -> bool {
        self.state.is_terminal()
            && Timestamp::now().as_millis() - self.updated_at.as_millis()
                > PODCAST_JOB_RETENTION_SECS * 1_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_transition_table_is_the_services_own() {
        use PodcastJobState::*;
        assert!(Queued.can_transition_to(Running));
        assert!(Queued.can_transition_to(Cancelled));
        assert!(Queued.can_transition_to(Failed));
        assert!(!Queued.can_transition_to(Done));
        assert!(Running.can_transition_to(Done));
        assert!(Running.can_transition_to(Failed));
        assert!(Running.can_transition_to(Cancelled));
        assert!(!Running.can_transition_to(Queued));
        for terminal in [Done, Failed, Cancelled] {
            for target in [Queued, Running, Done, Failed, Cancelled] {
                assert_eq!(
                    terminal.can_transition_to(target),
                    terminal == target,
                    "{terminal:?} -> {target:?}"
                );
            }
        }
    }

    #[test]
    fn a_projected_job_is_failed_interrupted_but_a_long_eta_is_respected() {
        let mut job = dead_job(PodcastJobState::Running, 3_600);
        job.updated_at = Timestamp::from_millis(Timestamp::now().as_millis() - 2_000_000);
        // Two million ms stale on a one-hour ETA: the window is 3h, so the job
        // is still believed to be running.
        assert_eq!(job.clone().projected().get_state(), PodcastJobState::Running);
        // Past 3h, the projection fires.
        job.updated_at = Timestamp::from_millis(Timestamp::now().as_millis() - 11_000_000);
        let projected = job.projected();
        assert_eq!(projected.get_state(), PodcastJobState::Failed);
        assert_eq!(projected.get_error_code(), Some(PODCAST_INTERRUPTED_CODE));
    }

    #[test]
    fn a_fresh_job_and_a_terminal_job_are_never_projected() {
        let fresh = dead_job(PodcastJobState::Queued, 5);
        assert_eq!(fresh.clone().projected().get_state(), PodcastJobState::Queued);
        let done = dead_job(PodcastJobState::Done, 0);
        assert_eq!(done.clone().projected().get_state(), PodcastJobState::Done);
        assert_eq!(done.projected().get_error_code(), None);
    }

    #[test]
    fn retention_counts_only_terminal_rows_past_the_window() {
        let mut done = dead_job(PodcastJobState::Done, 0);
        assert!(!done.is_expired());
        done.updated_at = Timestamp::from_millis(
            Timestamp::now().as_millis() - (PODCAST_JOB_RETENTION_SECS + 60) * 1_000,
        );
        assert!(done.is_expired());
        // A non-terminal row is the projection's business, not expiry's.
        let mut running = dead_job(PodcastJobState::Running, 0);
        running.updated_at = Timestamp::from_millis(
            Timestamp::now().as_millis() - (PODCAST_JOB_RETENTION_SECS + 60) * 1_000,
        );
        assert!(!running.is_expired());
    }

    /// A row with every field filled, for the projection/expiry tests only.
    fn dead_job(state: PodcastJobState, eta_secs: i64) -> PodcastJob {
        let now = Timestamp::now();
        PodcastJob {
            id: PodcastJobId::generate(),
            user_id: UserId::from_key("00000000-0000-7000-8000-000000000000"),
            source_id: "src".to_string(),
            format: None,
            state,
            stage: String::new(),
            progress: 0.0,
            error_code: None,
            audio_key: None,
            audio_name: None,
            audio_type: None,
            audio_bytes: None,
            duration_secs: None,
            eta_secs: Some(eta_secs),
            created_at: now,
            updated_at: now,
        }
    }
}
