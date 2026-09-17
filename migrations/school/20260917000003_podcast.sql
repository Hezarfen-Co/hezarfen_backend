-- The podcast job table: the backend-owned record of every episode the podcast
-- service was asked to produce.
--
-- WHERE THIS FILE BELONGS: `migrations/school/` in the backend repo, applied by
-- `Migrator::new(migrations/school)` to the template at boot and to every school
-- database at create. APPEND-ONLY relative to the existing school migrations: it
-- creates one new table and touches none of them.
--
-- WHY THE BACKEND HOLDS IT: until now the job record lived only in the service's
-- own volume (`jobs.py`'s per-job JSON files), which made a service restart or a
-- wiped volume lose every in-flight job the caller was polling — and the produced
-- mp3 with it. The backend now mints the job id, writes the row before it
-- dispatches, answers `podcast.status`/`podcast.result` from it, and stores the
-- uploaded audio in the school's own blob directory. The service reports each
-- transition back over the bridge (`podcast.report`) and uploads the finished
-- mp3. A service that vanishes therefore costs liveness — the read side projects
-- a job nobody is updating as failed after a bounded window — never the record.
--
-- NO `school` COLUMN: each school already lives in its own database, so the
-- tenant IS the database (the same rule every `zeka_*`/`rag_*` table follows).
-- The row's tenancy is the pair (this database, `user_id`), and the web layer
-- checks both: a foreign user's id reads as absent, exactly like `rag_thread`.
--
-- Translation rules inherited from the school schema's other parts:
--   * entity ids are app-minted uuid v7 (`domain::monotonic_id::next_uuid`),
--     never a DB default;
--   * timestamps are BIGINT unix-ms UTC, never `timestamptz`;
--   * FKs point outward, one direction only, all `ON DELETE NO ACTION`.
--
-- The state vocabulary and the transition table are the service's own
-- (`jobs.py`): queued -> running -> done|failed|cancelled, plus
-- queued -> cancelled|failed. The CHECK is the backstop under the Rust enum,
-- which is the source.

CREATE TABLE podcast_job (
    id            uuid PRIMARY KEY,
    -- Who submitted it. The ownership guard: every read door answers 404 for a
    -- row whose user_id is not the caller.
    user_id       uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    -- The source the service narrates, verbatim as submitted — the backend id
    -- of a course note today. No FK: `source_id` is the service's own handle on
    -- shared media, and the backend cannot say what it names.
    source_id     TEXT NOT NULL,
    -- The narration format. NULL until a report resolves it: the service applies
    -- its own default (and refuses formats that need an LLM key it lacks), so
    -- this column records the service's verdict, never a guess of ours.
    format        TEXT NULL,
    -- 'queued' | 'running' | 'done' | 'failed' | 'cancelled' (jobs.py STATES).
    state         TEXT NOT NULL
        CHECK (state IN ('queued', 'running', 'done', 'failed', 'cancelled')),
    -- The pipeline stage inside `running` (the service's SIMULATED_STAGES or its
    -- real ones); '' outside it.
    stage         TEXT NOT NULL DEFAULT '',
    -- Fraction complete, 0.0..=1.0. The service rounds to 4 decimals; the
    -- CHECK is the bound, not the rounding.
    progress      DOUBLE PRECISION NOT NULL DEFAULT 0
        CHECK (progress BETWEEN 0 AND 1),
    -- Set on `failed`: the service's own short error code, or `interrupted` when
    -- the backend projected a dead job or the dispatch never reached a worker.
    error_code    TEXT NULL,
    -- The uploaded episode, when one landed (see `web::podcast`'s ingest
    -- handshake): the blob key under the school's files root, plus the metadata
    -- the audio door replays as headers. All NULL until an upload.
    audio_key     TEXT NULL,
    audio_name    TEXT NULL,
    audio_type    TEXT NULL,
    audio_bytes   BIGINT NULL,
    duration_secs DOUBLE PRECISION NULL,
    -- The service's own ETA from the submit receipt. Drives the read-side
    -- staleness window (a long job is not projected dead mid-flight) and is
    -- echoed in the submit receipt.
    eta_secs      BIGINT NULL,
    created_at    BIGINT NOT NULL,
    updated_at    BIGINT NOT NULL,
    -- A deferred upload's key is set in the same statement that reports it, so
    -- "done" and "has audio" can never disagree: the report that says done
    -- requires `audio_key` to be non-NULL.
    CONSTRAINT podcast_job_done_has_audio
        CHECK (state <> 'done' OR audio_key IS NOT NULL)
);

-- The list view's sort key (a user's own jobs, newest first) and the sweep's.
CREATE INDEX podcast_job_user_created ON podcast_job (user_id, created_at);
CREATE INDEX podcast_job_created ON podcast_job (created_at);
