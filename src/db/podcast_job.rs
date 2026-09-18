//! The `podcast_job` table: one row per submitted episode, written by the
//! submit door before it dispatches and updated by the service's own
//! `podcast.report` calls. The entity, its state enum and the read-side
//! projection live in [`crate::domain::podcast_job`]; this module is where the
//! rows are read and written.
//!
//! Every write here is a single conditional statement, never a read-then-write:
//! the report path passes the state it validated against (`from`) and matches
//! zero rows when a concurrent cancel moved the row first — the caller re-reads
//! and answers what actually happened. The transition *table* therefore lives
//! in exactly one place (the domain enum); SQL only guards the race.
//!
//! `done` can never be stored without an audio key: the column CHECK
//! (`podcast_job_done_has_audio`) backstops the service layer's refusal, so a
//! result door can trust every `done` row to name a blob.

use std::collections::HashMap;

use crate::database::Database;
use crate::db::page::PagedList;
use crate::domain::course_note::CourseNoteId;
use crate::domain::podcast_job::{PodcastJob, PodcastJobId, PodcastJobState};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;
use sqlx::{query, query_as};

/// Write one freshly minted job. The id and timestamps are the caller's — the
/// service is handed the id this statement stores, so the row and the
/// service's record can never start out disagreeing.
pub async fn insert(db: &Database, job: &PodcastJob) -> Result<PodcastJob, AppError> {
    Ok(query_as!(
        PodcastJob,
        "INSERT INTO podcast_job (id, user_id, source_id, format, state, stage, progress, \
             error_code, audio_key, audio_name, audio_type, audio_bytes, duration_secs, \
             eta_secs, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $15) \
         RETURNING id AS \"id: PodcastJobId\", user_id AS \"user_id: UserId\", \
             source_id, format, state AS \"state: PodcastJobState\", stage, progress, error_code, \
             audio_key, audio_name, audio_type, audio_bytes, duration_secs, eta_secs, \
             created_at AS \"created_at: Timestamp\", updated_at AS \"updated_at: Timestamp\"",
        job.get_id().uuid(),
        job.get_user_id().uuid(),
        job.get_source_id(),
        job.get_format(),
        job.get_state().as_str(),
        job.get_stage(),
        job.get_progress(),
        job.get_error_code(),
        job.get_audio_key(),
        job.get_audio_name(),
        job.get_audio_type(),
        job.get_audio_bytes(),
        job.get_duration_secs(),
        job.get_eta_secs(),
        job.get_created_at().as_millis(),
    )
    .fetch_one(db)
    .await?)
}

/// Read one job by id, whoever owns it — the report path only, which has
/// already checked that the reporting user is the row's own.
pub async fn read(db: &Database, id: &PodcastJobId) -> Result<Option<PodcastJob>, AppError> {
    Ok(query_as!(
        PodcastJob,
        "SELECT id AS \"id: PodcastJobId\", user_id AS \"user_id: UserId\", \
             source_id, format, state AS \"state: PodcastJobState\", stage, progress, error_code, \
             audio_key, audio_name, audio_type, audio_bytes, duration_secs, eta_secs, \
             created_at AS \"created_at: Timestamp\", updated_at AS \"updated_at: Timestamp\" \
         FROM podcast_job WHERE id = $1",
        id.uuid(),
    )
    .fetch_optional(db)
    .await?)
}

/// Read a job only if `user` owns it — a foreign id reads as absent, so every
/// HTTP door answers 404 rather than leaking that it exists. The filter is in
/// the statement, not applied after the read, so the row never leaves the
/// database on a foreign caller's behalf.
pub async fn read_for(
    db: &Database,
    id: &PodcastJobId,
    user: &UserId,
) -> Result<Option<PodcastJob>, AppError> {
    Ok(query_as!(
        PodcastJob,
        "SELECT id AS \"id: PodcastJobId\", user_id AS \"user_id: UserId\", \
             source_id, format, state AS \"state: PodcastJobState\", stage, progress, error_code, \
             audio_key, audio_name, audio_type, audio_bytes, duration_secs, eta_secs, \
             created_at AS \"created_at: Timestamp\", updated_at AS \"updated_at: Timestamp\" \
         FROM podcast_job WHERE id = $1 AND user_id = $2",
        id.uuid(),
        user.uuid(),
    )
    .fetch_optional(db)
    .await?)
}

/// A user's jobs, newest first — the sort `podcast_job_user_created` exists
/// for — each paired with the title of the course note it narrates.
///
/// The title is a second read over the page alone (the [`PagedList`] window
/// already chose the rows), not a join inside the paging statement: the
/// count that pages the list must count jobs, and a join there could only
/// multiply or drop one. A note that is gone answers `None` — the job stays
/// in the caller's history either way.
pub async fn list_for_user(
    db: &Database,
    user: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<(PodcastJob, Option<String>)>, i64), AppError> {
    let (rows, total) = PagedList::new(
        "podcast_job WHERE user_id = $1",
        "ORDER BY created_at DESC, id DESC",
    )
    .bind(user.uuid())
    .run::<PodcastJob>(limit, offset, db)
    .await?;
    Ok((with_source_titles(db, rows).await?, total))
}

/// Pair the jobs of one page with the titles of the notes they narrate: one
/// statement however many distinct notes the page names (the ids ride a
/// single `ANY`), none at all for an empty page.
///
/// `source_id` is stored as the string the submit door was handed, so the
/// lookup parses it the same way that door did
/// ([`CourseNoteId::from_key`]); a job whose source no row matches keeps its
/// `None`.
async fn with_source_titles(
    db: &Database,
    jobs: Vec<PodcastJob>,
) -> Result<Vec<(PodcastJob, Option<String>)>, AppError> {
    if jobs.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<uuid::Uuid> = jobs
        .iter()
        .map(|job| CourseNoteId::from_key(job.get_source_id()).uuid())
        .collect();
    let titles: HashMap<uuid::Uuid, String> =
        query!("SELECT id, title FROM course_note WHERE id = ANY($1)", &ids,)
            .fetch_all(db)
            .await?
            .into_iter()
            .map(|row| (row.id, row.title))
            .collect();
    Ok(jobs
        .into_iter()
        .map(|job| {
            let title = titles
                .get(&CourseNoteId::from_key(job.get_source_id()).uuid())
                .cloned();
            (job, title)
        })
        .collect())
}

/// Apply one validated report: `from` is the state the service layer read and
/// checked the transition against, and the statement moves the row only if it
/// still holds that state. `None` therefore means either the row is gone or a
/// concurrent write (a cancel, another report) moved it first — the caller
/// re-reads and answers the truth.
///
/// `format` fills the column once and never rewrites it: the service's default
/// resolution arrives on the first report, and a later report disagreeing with
/// it is refused by the service layer, not silently stored.
#[allow(clippy::too_many_arguments)]
pub async fn report(
    db: &Database,
    id: &PodcastJobId,
    from: PodcastJobState,
    to: PodcastJobState,
    stage: &str,
    progress: f64,
    error_code: Option<&str>,
    format: Option<&str>,
) -> Result<Option<PodcastJob>, AppError> {
    let now = Timestamp::now().as_millis();
    Ok(query_as!(
        PodcastJob,
        "UPDATE podcast_job SET state = $3, stage = $4, progress = $5, error_code = $6, \
             format = COALESCE(format, $7::text), updated_at = $8 \
         WHERE id = $1 AND state = $2 \
         RETURNING id AS \"id: PodcastJobId\", user_id AS \"user_id: UserId\", \
             source_id, format, state AS \"state: PodcastJobState\", stage, progress, error_code, \
             audio_key, audio_name, audio_type, audio_bytes, duration_secs, eta_secs, \
             created_at AS \"created_at: Timestamp\", updated_at AS \"updated_at: Timestamp\"",
        id.uuid(),
        from.as_str(),
        to.as_str(),
        stage,
        progress,
        error_code,
        format,
        now,
    )
    .fetch_optional(db)
    .await?)
}

/// Mark a live job cancelled — the backend's own write behind the cancel
/// verdict, taken the moment the service confirms it. `None` means the row was
/// not live (already terminal, or gone), which is exactly the `cancelled:
/// false` verdict.
pub async fn set_cancelled(
    db: &Database,
    id: &PodcastJobId,
) -> Result<Option<PodcastJob>, AppError> {
    let now = Timestamp::now().as_millis();
    Ok(query_as!(
        PodcastJob,
        "UPDATE podcast_job SET state = 'cancelled', stage = '', updated_at = $2 \
         WHERE id = $1 AND state IN ('queued', 'running') \
         RETURNING id AS \"id: PodcastJobId\", user_id AS \"user_id: UserId\", \
             source_id, format, state AS \"state: PodcastJobState\", stage, progress, error_code, \
             audio_key, audio_name, audio_type, audio_bytes, duration_secs, eta_secs, \
             created_at AS \"created_at: Timestamp\", updated_at AS \"updated_at: Timestamp\"",
        id.uuid(),
        now,
    )
    .fetch_optional(db)
    .await?)
}

/// Mark a live job failed with the backend's own reason code — a dispatch that
/// never reached a worker, or a service that answered `not_found` for a job
/// whose row still says it is running. Same `None` contract as
/// [`set_cancelled`].
pub async fn set_failed(
    db: &Database,
    id: &PodcastJobId,
    error_code: &str,
) -> Result<Option<PodcastJob>, AppError> {
    let now = Timestamp::now().as_millis();
    Ok(query_as!(
        PodcastJob,
        "UPDATE podcast_job SET state = 'failed', error_code = $2, stage = '', updated_at = $3 \
         WHERE id = $1 AND state IN ('queued', 'running') \
         RETURNING id AS \"id: PodcastJobId\", user_id AS \"user_id: UserId\", \
             source_id, format, state AS \"state: PodcastJobState\", stage, progress, error_code, \
             audio_key, audio_name, audio_type, audio_bytes, duration_secs, eta_secs, \
             created_at AS \"created_at: Timestamp\", updated_at AS \"updated_at: Timestamp\"",
        id.uuid(),
        error_code,
        now,
    )
    .fetch_optional(db)
    .await?)
}

/// Record the uploaded episode on its job: the blob key plus the metadata the
/// audio door replays as headers. One statement, so "the bytes are stored" and
/// "the row names them" land together; a job that is gone answers `None`.
#[allow(clippy::too_many_arguments)]
pub async fn set_audio(
    db: &Database,
    id: &PodcastJobId,
    key: &str,
    name: &str,
    content_type: &str,
    bytes: i64,
    duration_secs: Option<f64>,
) -> Result<Option<PodcastJob>, AppError> {
    let now = Timestamp::now().as_millis();
    Ok(query_as!(
        PodcastJob,
        "UPDATE podcast_job SET audio_key = $2, audio_name = $3, audio_type = $4, \
             audio_bytes = $5, duration_secs = $6, updated_at = $7 \
         WHERE id = $1 \
         RETURNING id AS \"id: PodcastJobId\", user_id AS \"user_id: UserId\", \
             source_id, format, state AS \"state: PodcastJobState\", stage, progress, error_code, \
             audio_key, audio_name, audio_type, audio_bytes, duration_secs, eta_secs, \
             created_at AS \"created_at: Timestamp\", updated_at AS \"updated_at: Timestamp\"",
        id.uuid(),
        key,
        name,
        content_type,
        bytes,
        duration_secs,
        now,
    )
    .fetch_optional(db)
    .await?)
}

/// Store the service's own ETA estimate on a job it has just accepted — the
/// number the read-side staleness window is scaled by and the submit receipt
/// echoes. A job that is gone answers `None`.
pub async fn set_eta(
    db: &Database,
    id: &PodcastJobId,
    eta_secs: i64,
) -> Result<Option<PodcastJob>, AppError> {
    Ok(query_as!(
        PodcastJob,
        "UPDATE podcast_job SET eta_secs = $2 WHERE id = $1 \
         RETURNING id AS \"id: PodcastJobId\", user_id AS \"user_id: UserId\", \
             source_id, format, state AS \"state: PodcastJobState\", stage, progress, error_code, \
             audio_key, audio_name, audio_type, audio_bytes, duration_secs, eta_secs, \
             created_at AS \"created_at: Timestamp\", updated_at AS \"updated_at: Timestamp\"",
        id.uuid(),
        eta_secs,
    )
    .fetch_optional(db)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::monotonic_id::next_uuid;

    /// A leased school database with one app_user row, and that user's id.
    async fn fixture() -> (Database, crate::database::TestDatabases, UserId) {
        let (db, leases) = crate::database::init_test_db().await;
        let user = UserId::from_key(&next_uuid().to_string());
        insert_user(&db, &user).await;
        (db, leases, user)
    }

    async fn insert_user(db: &Database, user: &UserId) {
        // A monotonic counter, not a slice of the id: two UUIDv7s minted in
        // the same millisecond share their first hex digits.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        sqlx::query("INSERT INTO app_user (id, username, created_at) VALUES ($1, $2, 0)")
            .bind(user.uuid())
            .bind(format!("test-user-{n}"))
            .execute(db)
            .await
            .expect("insert user");
    }

    /// A minimal queued row for the statement tests below.
    fn job(user: &UserId) -> PodcastJob {
        let now = Timestamp::now();
        PodcastJob {
            id: PodcastJobId::generate(),
            user_id: *user,
            source_id: "src".to_string(),
            format: None,
            state: PodcastJobState::Queued,
            stage: String::new(),
            progress: 0.0,
            error_code: None,
            audio_key: None,
            audio_name: None,
            audio_type: None,
            audio_bytes: None,
            duration_secs: None,
            eta_secs: Some(30),
            created_at: now,
            updated_at: now,
        }
    }

    /// The report write is conditional on the state the caller validated
    /// against, so a cancel and a late report cannot both win: whoever lands
    /// second matches nothing and the caller re-reads.
    #[tokio::test]
    async fn a_report_is_guarded_by_the_state_it_validated_against() {
        let (db, _leases, user) = fixture().await;
        let row = insert(&db, &job(&user)).await.expect("insert");
        let id = row.get_id().clone();

        // The running report lands, and carries the resolved format with it.
        let running = report(
            &db,
            &id,
            PodcastJobState::Queued,
            PodcastJobState::Running,
            "script",
            0.25,
            None,
            Some("duz_okuma"),
        )
        .await
        .expect("report")
        .expect("row was queued");
        assert_eq!(running.get_state(), PodcastJobState::Running);
        assert_eq!(running.get_format(), Some("duz_okuma"));

        // A second report claiming the row is still queued matches nothing.
        let stale = report(
            &db,
            &id,
            PodcastJobState::Queued,
            PodcastJobState::Running,
            "script",
            0.5,
            None,
            None,
        )
        .await
        .expect("report");
        assert!(stale.is_none());

        // A cancel moves it; the done report that raced it matches nothing.
        assert!(set_cancelled(&db, &id).await.expect("cancel").is_some());
        let done = report(
            &db,
            &id,
            PodcastJobState::Running,
            PodcastJobState::Done,
            "done",
            1.0,
            None,
            None,
        )
        .await
        .expect("report");
        assert!(done.is_none());
        assert_eq!(
            read(&db, &id).await.expect("read").unwrap().get_state(),
            PodcastJobState::Cancelled
        );
        // A terminal row refuses the backend's own writes too.
        assert!(set_failed(&db, &id, "interrupted").await.unwrap().is_none());
    }

    /// A foreign user's id reads as absent — the query itself filters, so the
    /// row never leaves the database on the stranger's behalf.
    #[tokio::test]
    async fn a_foreign_user_reads_no_row() {
        let (db, _leases, owner) = fixture().await;
        let row = insert(&db, &job(&owner)).await.expect("insert");
        let id = row.get_id().clone();
        let stranger = UserId::from_key(&next_uuid().to_string());
        insert_user(&db, &stranger).await;
        assert!(
            read_for(&db, &id, &stranger)
                .await
                .expect("read")
                .is_none()
        );
        assert!(
            read_for(&db, &id, row.get_user_id())
                .await
                .expect("read")
                .is_some()
        );
    }

    /// `done` and "has audio" can never disagree: the schema refuses a done
    /// report before an upload landed, and accepts it after.
    #[tokio::test]
    async fn done_cannot_be_stored_without_an_audio_key() {
        let (db, _leases, user) = fixture().await;
        let row = insert(&db, &job(&user)).await.expect("insert");
        let id = row.get_id().clone();
        let refused = report(
            &db,
            &id,
            PodcastJobState::Queued,
            PodcastJobState::Done,
            "done",
            1.0,
            None,
            None,
        )
        .await;
        assert!(refused.is_err(), "the CHECK backstops the service layer");
        set_audio(
            &db,
            &id,
            "podcast/x.mp3",
            "episode.mp3",
            "audio/mpeg",
            12,
            Some(3.5),
        )
        .await
        .expect("upload")
        .expect("row");
        let done = report(
            &db,
            &id,
            PodcastJobState::Queued,
            PodcastJobState::Done,
            "done",
            1.0,
            None,
            None,
        )
        .await
        .expect("report")
        .expect("row");
        assert_eq!(done.get_state(), PodcastJobState::Done);
        assert_eq!(done.get_audio_key(), Some("podcast/x.mp3"));
        assert_eq!(done.get_duration_secs(), Some(3.5));
        assert_eq!(done.get_audio_bytes(), Some(12));
    }
}
