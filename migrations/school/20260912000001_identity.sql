-- School-database schema, part 1: identity, sessions, notes, messages,
-- parent links, work, pomodoro, badges, boards, chatbot, settings, refcounts
-- (Postgres).
--
-- DISJOINTNESS INVARIANT: the tables in migrations/school/*.sql and the ones
-- in migrations/control/*.sql together form ONE union schema, applied to a
-- single prepare database by scripts/prepare_db.sh for sqlx's compile-time
-- query macros. The two sets must never collide on a table name.
--
-- Translated from src/migration_sql.rs MIGRATION (SurrealDB), per the plan's
-- schema translation rules. Binding renames (reserved words, never quoted):
--   table `user`    -> app_user
--   table `session` -> user_session
--   every column named `user` -> `app_user` (PostgreSQL rejects the bare
--   word as a column name; the plan's fallback prefixes the domain noun)
-- `session` as a COLUMN name (session_attendance.session) is legal in
-- PostgreSQL (non-reserved) and stays. Entity ids are app-minted UUID v7 (no
-- DB default); timestamps stay BIGINT unix-ms; counter columns
-- (absent-reads-as-zero in Surreal) are BIGINT NOT NULL DEFAULT 0 and stay
-- out of the Rust structs. All FKs ON DELETE NO ACTION.
--
-- Deliberately NOT translated from the Surreal source: `rate_limit` (control
-- only — see migrations/control), `migration_mark` + every `REMOVE ...`
-- retirement line, PRE_REPAIR, BACKFILL, migration_mark machinery (all dead,
-- preproduction).
--
-- CHECK constraints carry exactly the `#[surreal(untagged,
-- rename_all = "lowercase")]` enums (role, folder, chatbot role/status,
-- ledger kinds, audience tags, school status). Validated-String newtypes
-- (course kind, exam kind/mode, homework status, attendance statuses, pool
-- status, board_stroke kind, bank visibility) keep NO CHECK — the repo
-- convention is that those enums live in Rust newtypes, and attendance
-- statuses / exam kinds are school-configurable at runtime, which a DDL
-- CHECK could not survive.

-- The school-side identity row. The password hash is NOT here: the credential
-- lives on the control database's `person` row, and `app_user.person` is the
-- join key to it (uuid, no FK — the two halves are separate databases). The
-- `username` copy stays for display and for the school-scoped unique name.
CREATE TABLE app_user (
    id                        uuid PRIMARY KEY,
    username                  TEXT NOT NULL,
    person                    uuid NULL,
    role                      TEXT NOT NULL DEFAULT 'student'
        CHECK (role IN ('ai', 'parent', 'student', 'teacher', 'manager', 'admin')),
    name                      TEXT NULL,
    surname                   TEXT NULL,
    email                     TEXT NULL,
    phone                     TEXT NULL,
    birth_date                TEXT NULL,
    theme                     TEXT NULL CHECK (theme IN ('light', 'dark')),
    language                  TEXT NULL CHECK (language IN ('tr', 'en')),
    palette_color             TEXT NULL,
    display_name              TEXT NULL,
    bio                       TEXT NULL,
    avatar_file               TEXT NULL,
    avatar_content_type       TEXT NULL,
    avatar_size               BIGINT NULL,
    -- Lifetime/claim counters: absent-reads-as-zero in Surreal, DEFAULT 0
    -- here, and (as today) carried by no Rust struct.
    chatbot_thread_count      BIGINT NOT NULL DEFAULT 0,
    board_count               BIGINT NOT NULL DEFAULT 0,
    homework_submitted_total  BIGINT NOT NULL DEFAULT 0,
    homework_on_time_total    BIGINT NOT NULL DEFAULT 0,
    exam_sat_total            BIGINT NOT NULL DEFAULT 0,
    pomodoro_finished_total   BIGINT NOT NULL DEFAULT 0,
    pomodoro_focus_ms_total   BIGINT NOT NULL DEFAULT 0,
    marks_given_total         BIGINT NOT NULL DEFAULT 0,
    lessons_held_total        BIGINT NOT NULL DEFAULT 0,
    pool_approved_total       BIGINT NOT NULL DEFAULT 0,
    pool_published_total      BIGINT NOT NULL DEFAULT 0,
    lessons_attended_total    BIGINT NOT NULL DEFAULT 0,
    high_mark_total           BIGINT NOT NULL DEFAULT 0,
    study_streak_longest      BIGINT NOT NULL DEFAULT 0,
    study_streak_current      BIGINT NOT NULL DEFAULT 0,
    -- Day markers, not counts: absent means "no day yet", which NULL keeps
    -- (DEFAULT 0 would mean the epoch day).
    study_streak_last_day     BIGINT NULL,
    pomodoro_counted_today    BIGINT NOT NULL DEFAULT 0,
    pomodoro_counted_day      BIGINT NULL,
    created_at                BIGINT NOT NULL,
    CONSTRAINT app_user_username UNIQUE (username)
);

-- At most one school identity per person: the join key is 1:1. Partial
-- because a school row minted before its person was known (boot seed order)
-- legally reads NULL.
CREATE UNIQUE INDEX app_user_person ON app_user (person) WHERE person IS NOT NULL;

CREATE TABLE user_session (
    id         uuid PRIMARY KEY,
    app_user   uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    token      TEXT NOT NULL,
    expires_at BIGINT NOT NULL,
    CONSTRAINT session_token UNIQUE (token)
);

CREATE INDEX session_expires ON user_session (expires_at);

CREATE TABLE note (
    id         uuid PRIMARY KEY,
    app_user   uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    title      TEXT NOT NULL,
    content    TEXT NOT NULL,
    file_count BIGINT NOT NULL DEFAULT 0
);

CREATE INDEX note_user ON note (app_user);

CREATE TABLE note_file (
    id           uuid PRIMARY KEY,
    note         uuid NOT NULL REFERENCES note(id) ON DELETE NO ACTION,
    name         TEXT NOT NULL,
    content_type TEXT NOT NULL,
    size         BIGINT NOT NULL
);

CREATE INDEX note_file_note ON note_file (note);

CREATE TABLE message (
    id               uuid PRIMARY KEY,
    sender           uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    recipient        uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    subject          TEXT NOT NULL,
    body             TEXT NOT NULL,
    label            TEXT NULL,
    sent_at          BIGINT NOT NULL,
    read             BOOLEAN NOT NULL DEFAULT false,
    sender_folder    TEXT NOT NULL DEFAULT 'sent'
        CHECK (sender_folder IN ('inbox', 'sent', 'archive', 'trash', 'deleted')),
    recipient_folder TEXT NOT NULL DEFAULT 'inbox'
        CHECK (recipient_folder IN ('inbox', 'sent', 'archive', 'trash', 'deleted')),
    -- Where each side's copy sat before it was filed into archive/trash;
    -- NULL while the copy is in its home folder.
    sender_origin    TEXT NULL,
    recipient_origin TEXT NULL
);

CREATE INDEX message_sender ON message (sender);
CREATE INDEX message_recipient ON message (recipient);

CREATE TABLE parent_link (
    parent    uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    student   uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    linked_by uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    CONSTRAINT parent_link_parent_student PRIMARY KEY (parent, student)
);

CREATE INDEX parent_link_parent ON parent_link (parent);
CREATE INDEX parent_link_student ON parent_link (student);

CREATE TABLE work_entry (
    id        uuid PRIMARY KEY,
    app_user  uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    check_in  BIGINT NOT NULL,
    check_out BIGINT NULL
);

CREATE INDEX work_entry_user ON work_entry (app_user);
-- At most one open stint per user: the deterministic `open_{user}` key of
-- `WorkEntryId::open_for`, now DB-enforced (closed entries keep minted ids).
CREATE UNIQUE INDEX work_entry_open ON work_entry (app_user) WHERE check_out IS NULL;

CREATE TABLE pomodoro_session (
    id          uuid PRIMARY KEY,
    app_user    uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    started_at  BIGINT NOT NULL,
    finished_at BIGINT NULL,
    -- The verdict `finish` reached for this stint, stamped at close; NULL on
    -- a running stint, and on a stint closed before the rule existed.
    counted     BOOLEAN NULL,
    -- The student's own name for what the stint is for ('math', 'TYT
    -- denemesi') — free text given at start, not a subject link.
    label       TEXT NULL
);

CREATE INDEX pomodoro_session_user ON pomodoro_session (app_user);
-- One open stint per user: the deterministic open-stint row of
-- `PomodoroSession::open_for`, now DB-enforced.
CREATE UNIQUE INDEX pomodoro_session_open_stint
    ON pomodoro_session (app_user) WHERE finished_at IS NULL;

-- One row per badge a user has earned; the deterministic {user}_{badge}
-- pair. Awards are permanent — nothing deletes a row here.
CREATE TABLE badge_award (
    app_user  uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    badge     TEXT NOT NULL,
    earned_at BIGINT NULL,
    CONSTRAINT badge_award_user_badge PRIMARY KEY (app_user, badge)
);

CREATE INDEX badge_award_user ON badge_award (app_user);

CREATE TABLE board (
    id                 uuid PRIMARY KEY,
    creator            uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    title              TEXT NOT NULL,
    locked             BOOLEAN NOT NULL DEFAULT false,
    locked_by          uuid NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    locked_at          BIGINT NULL,
    epoch              BIGINT NOT NULL DEFAULT 0,
    epoch_stroke_count BIGINT NOT NULL DEFAULT 0,
    total_stroke_count BIGINT NOT NULL DEFAULT 0,
    closed_at          BIGINT NULL,
    created_at         BIGINT NOT NULL
);

CREATE INDEX board_creator ON board (creator);

-- The roster, one row per (board, invited user). The membership predicate
-- everywhere is `creator = $u OR EXISTS (… WHERE participant = $u)` over
-- this table; the participant-side index is what the board list, the
-- demotion strip in src/db/user.rs and the cap checks read through. Rows
-- carry no order: every reader sorts by participant, so the assembled
-- roster is deterministic.
CREATE TABLE board_participant (
    board       uuid NOT NULL REFERENCES board(id) ON DELETE NO ACTION,
    participant uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    CONSTRAINT board_participant_board_participant PRIMARY KEY (board, participant)
);

CREATE INDEX board_participant_participant ON board_participant (participant);

CREATE TABLE board_stroke (
    id         uuid PRIMARY KEY,
    board      uuid NOT NULL REFERENCES board(id) ON DELETE NO ACTION,
    author     uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    -- 'stroke' or 'clear'; payload NULL on a clear marker, count populated
    -- only on a clear marker — the final stroke count of the epoch that
    -- marker closed.
    kind       TEXT NOT NULL,
    payload    TEXT NULL,
    count      BIGINT NULL,
    epoch      BIGINT NOT NULL,
    created_at BIGINT NOT NULL
);

CREATE INDEX board_stroke_board_epoch ON board_stroke (board, epoch);
CREATE INDEX board_stroke_board_kind ON board_stroke (board, kind);

CREATE TABLE chatbot_thread (
    id         uuid PRIMARY KEY,
    user_id    uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    title      TEXT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);

CREATE INDEX chatbot_thread_user_updated ON chatbot_thread (user_id, updated_at);

CREATE TABLE chatbot_message (
    id           uuid PRIMARY KEY,
    thread_id    uuid NOT NULL REFERENCES chatbot_thread(id) ON DELETE NO ACTION,
    user_id      uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    role         TEXT NOT NULL CHECK (role IN ('user', 'assistant')),
    content      TEXT NOT NULL,
    status       TEXT NOT NULL CHECK (status IN ('pending', 'complete', 'failed')),
    truncated    BOOLEAN NOT NULL DEFAULT false,
    error_code   TEXT NULL,
    created_at   BIGINT NOT NULL,
    completed_at BIGINT NULL
);

CREATE INDEX chatbot_message_thread_created ON chatbot_message (thread_id, created_at);
CREATE INDEX chatbot_message_user ON chatbot_message (user_id);
CREATE INDEX chatbot_message_status_created ON chatbot_message (status, created_at);

CREATE TABLE settings (
    id                         TEXT PRIMARY KEY,
    exam_kinds                 JSONB NOT NULL,
    grade_bands                JSONB NOT NULL,
    max_file_bytes             BIGINT NULL,
    chatbot_history_turns      BIGINT NULL,
    max_chatbot_message_len    BIGINT NULL,
    max_chatbot_threads        BIGINT NULL,
    meal_slots                 JSONB NULL,
    meal_cancel_cutoff_minutes BIGINT NULL
);

-- The settings singleton's two vocabulary lists, one row per entry. They
-- replace TEXT[] columns; the compare-and-set guard in src/db/settings.rs
-- compares these row *sets*, so nothing here carries order — every reader
-- sorts, and an absent set (never configured) and an explicitly empty one
-- both read as zero rows.
CREATE TABLE settings_attendance_status (
    settings TEXT NOT NULL REFERENCES settings(id) ON DELETE NO ACTION,
    status   TEXT NOT NULL,
    CONSTRAINT settings_attendance_status_settings_status PRIMARY KEY (settings, status)
);

CREATE TABLE settings_dietary_tag (
    settings TEXT NOT NULL REFERENCES settings(id) ON DELETE NO ACTION,
    tag      TEXT NOT NULL,
    CONSTRAINT settings_dietary_tag_settings_tag PRIMARY KEY (settings, tag)
);

-- Reference counters, one row per *name* the settings offer. Created by the
-- first claim; an absent row reads as zero references, in service.
CREATE TABLE kind_ref (
    name    TEXT PRIMARY KEY,
    count   BIGINT NOT NULL DEFAULT 0,
    retired BOOLEAN NOT NULL DEFAULT false
);

CREATE TABLE slot_ref (
    name    TEXT PRIMARY KEY,
    count   BIGINT NOT NULL DEFAULT 0,
    retired BOOLEAN NOT NULL DEFAULT false
);
