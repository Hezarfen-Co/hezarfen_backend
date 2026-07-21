//! SurrealDB connection (WebSocket to a server; in-memory for tests) + schema.

use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::opt::auth::Root;

use crate::config::Config;
use crate::error::AppError;

/// The shared database handle.
///
/// `Arc` is load-bearing, not decoration. `Surreal`'s own `Clone` mints a
/// fresh server-side session per clone (`Uuid::new_v4()` + a fire-and-forget
/// `SessionId::Clone` to the router, SDK `lib.rs:340`), and axum clones the
/// application state on every request — so a bare `Surreal<Any>` here means a
/// new session, and a matching `SessionId::Drop`, for each request served.
/// Under concurrency those lifecycle events race the queries riding on them:
/// sessions disappear ("Session not found"), and a session whose signin has
/// not been replayed answers "Anonymous access not allowed" / "Specify a
/// namespace" — the same strings [`crate::error::is_session_replay_error`]
/// treats as a transient reconnect, so the failures masquerade as one.
///
/// Cloning the `Arc` shares the single session established at boot instead.
pub type Database = std::sync::Arc<Surreal<Any>>;

pub const USER_TABLE: &str = "user";
pub const SESSION_TABLE: &str = "session";
pub const NOTE_TABLE: &str = "note";
pub const MESSAGE_TABLE: &str = "message";
pub const NOTE_FILE_TABLE: &str = "note_file";
pub const EVENT_TABLE: &str = "event";
pub const ATTENDANCE_TABLE: &str = "attendance";
pub const REGISTRATION_TABLE: &str = "registration";
pub const EXAM_TABLE: &str = "exam";
pub const EXAM_RESULT_TABLE: &str = "exam_result";
pub const EXAM_ATTEMPT_TABLE: &str = "exam_attempt";
pub const EXAM_QUESTION_TABLE: &str = "exam_question";
pub const QUESTION_IMAGE_TABLE: &str = "question_image";
pub const EXAM_ANSWER_TABLE: &str = "exam_answer";
pub const COURSE_TABLE: &str = "course";
pub const ENROLLMENT_TABLE: &str = "enrollment";
pub const PARENT_LINK_TABLE: &str = "parent_link";
pub const COURSE_SESSION_TABLE: &str = "course_session";
pub const SESSION_ATTENDANCE_TABLE: &str = "session_attendance";
pub const WORK_ENTRY_TABLE: &str = "work_entry";
pub const POMODORO_SESSION_TABLE: &str = "pomodoro_session";
pub const SETTINGS_TABLE: &str = "settings";
pub const TERM_TABLE: &str = "term";
pub const SUBJECT_TABLE: &str = "subject";
pub const POOL_QUESTION_TABLE: &str = "pool_question";
pub const SOLUTION_TABLE: &str = "solution";

/// SCHEMAFULL schema: every column is typed, references use `record<..>`.
/// Idempotent — safe to run on every boot: `IF NOT EXISTS` guards the
/// definitions and `REMOVE ... IF EXISTS` retires schema (like the
/// single-attempt unique index) exactly once. DDL only — data backfills live
/// in `BACKFILL`, which runs as a *separate* query: statements inside one
/// batch see the schema as it stood when the batch started, so an UPDATE next
/// to a fresh `DEFINE FIELD audience.*` writes against the old field set and
/// SCHEMAFULL silently strips the very keys being backfilled.
const MIGRATION: &str = "
    DEFINE TABLE IF NOT EXISTS user SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS username ON user TYPE string;
    DEFINE FIELD IF NOT EXISTS password_hash ON user TYPE string;
    DEFINE FIELD IF NOT EXISTS role ON user TYPE string DEFAULT 'student';
    DEFINE FIELD IF NOT EXISTS name ON user TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS surname ON user TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS email ON user TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS phone ON user TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS birth_date ON user TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS theme ON user TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS language ON user TYPE option<string>;
    DEFINE INDEX IF NOT EXISTS user_username ON user FIELDS username UNIQUE;

    DEFINE TABLE IF NOT EXISTS session SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS user ON session TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS token ON session TYPE string;
    DEFINE FIELD IF NOT EXISTS expires_at ON session TYPE int;
    DEFINE INDEX IF NOT EXISTS session_token ON session FIELDS token UNIQUE;
    DEFINE INDEX IF NOT EXISTS session_expires ON session FIELDS expires_at;

    DEFINE TABLE IF NOT EXISTS note SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS user ON note TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS title ON note TYPE string;
    DEFINE FIELD IF NOT EXISTS content ON note TYPE string;
    DEFINE INDEX IF NOT EXISTS note_user ON note FIELDS user;

    DEFINE TABLE IF NOT EXISTS message SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS sender ON message TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS recipient ON message TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS subject ON message TYPE string;
    DEFINE FIELD IF NOT EXISTS body ON message TYPE string;
    DEFINE FIELD IF NOT EXISTS label ON message TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS sent_at ON message TYPE int;
    DEFINE FIELD IF NOT EXISTS read ON message TYPE bool DEFAULT false;
    DEFINE FIELD IF NOT EXISTS sender_folder ON message TYPE string DEFAULT 'sent';
    DEFINE FIELD IF NOT EXISTS recipient_folder ON message TYPE string DEFAULT 'inbox';
    DEFINE INDEX IF NOT EXISTS message_sender ON message FIELDS sender;
    DEFINE INDEX IF NOT EXISTS message_recipient ON message FIELDS recipient;

    DEFINE TABLE IF NOT EXISTS note_file SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS note ON note_file TYPE record<note>;
    DEFINE FIELD IF NOT EXISTS name ON note_file TYPE string;
    DEFINE FIELD IF NOT EXISTS content_type ON note_file TYPE string;
    DEFINE FIELD IF NOT EXISTS size ON note_file TYPE int;
    DEFINE INDEX IF NOT EXISTS note_file_note ON note_file FIELDS note;

    DEFINE TABLE IF NOT EXISTS event SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS creator ON event TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS title ON event TYPE string;
    DEFINE FIELD IF NOT EXISTS description ON event TYPE string;
    DEFINE FIELD IF NOT EXISTS audience ON event TYPE object;
    DEFINE FIELD IF NOT EXISTS audience.kind ON event TYPE string;
    DEFINE FIELD IF NOT EXISTS audience.role ON event TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS audience.course ON event TYPE option<record<course>>;
    DEFINE FIELD IF NOT EXISTS audience.capacity ON event TYPE option<int>;
    REMOVE FIELD IF EXISTS audience.users ON TABLE event;
    DEFINE FIELD IF NOT EXISTS starts_at ON event TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS ends_at ON event TYPE option<int>;

    DEFINE TABLE IF NOT EXISTS attendance SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS event ON attendance TYPE record<event>;
    DEFINE FIELD IF NOT EXISTS user ON attendance TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS status ON attendance TYPE string;
    DEFINE FIELD IF NOT EXISTS marked_by ON attendance TYPE record<user>;
    DEFINE INDEX IF NOT EXISTS attendance_event_user ON attendance FIELDS event, user UNIQUE;
    DEFINE INDEX IF NOT EXISTS attendance_user ON attendance FIELDS user;

    DEFINE TABLE IF NOT EXISTS registration SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS event ON registration TYPE record<event>;
    DEFINE FIELD IF NOT EXISTS user ON registration TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS registered_by ON registration TYPE record<user>;
    DEFINE INDEX IF NOT EXISTS registration_event_user ON registration FIELDS event, user UNIQUE;
    DEFINE INDEX IF NOT EXISTS registration_event ON registration FIELDS event;

    DEFINE TABLE IF NOT EXISTS term SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS name ON term TYPE string;
    DEFINE FIELD IF NOT EXISTS starts_at ON term TYPE int;
    DEFINE FIELD IF NOT EXISTS ends_at ON term TYPE int;

    DEFINE TABLE IF NOT EXISTS course SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS creator ON course TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS teachers ON course TYPE array<record<user>> DEFAULT [];
    DEFINE FIELD IF NOT EXISTS teachers[*] ON course TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS title ON course TYPE string;
    DEFINE FIELD IF NOT EXISTS description ON course TYPE string;
    DEFINE FIELD IF NOT EXISTS kind ON course TYPE string DEFAULT 'course';
    DEFINE FIELD IF NOT EXISTS term ON course TYPE option<record<term>>;
    DEFINE FIELD IF NOT EXISTS capacity ON course TYPE option<int>;

    DEFINE TABLE IF NOT EXISTS subject SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS course ON subject TYPE record<course>;
    DEFINE FIELD IF NOT EXISTS name ON subject TYPE string;
    DEFINE FIELD IF NOT EXISTS description ON subject TYPE string;
    DEFINE INDEX IF NOT EXISTS subject_course ON subject FIELDS course;

    DEFINE TABLE IF NOT EXISTS enrollment SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS course ON enrollment TYPE record<course>;
    DEFINE FIELD IF NOT EXISTS user ON enrollment TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS enrolled_by ON enrollment TYPE record<user>;
    DEFINE INDEX IF NOT EXISTS enrollment_course_user ON enrollment FIELDS course, user UNIQUE;
    DEFINE INDEX IF NOT EXISTS enrollment_user ON enrollment FIELDS user;

    DEFINE TABLE IF NOT EXISTS parent_link SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS parent ON parent_link TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS student ON parent_link TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS linked_by ON parent_link TYPE record<user>;
    DEFINE INDEX IF NOT EXISTS parent_link_parent_student ON parent_link FIELDS parent, student UNIQUE;
    DEFINE INDEX IF NOT EXISTS parent_link_parent ON parent_link FIELDS parent;
    DEFINE INDEX IF NOT EXISTS parent_link_student ON parent_link FIELDS student;

    DEFINE TABLE IF NOT EXISTS course_session SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS course ON course_session TYPE record<course>;
    DEFINE FIELD IF NOT EXISTS teacher ON course_session TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS topic ON course_session TYPE string;
    DEFINE FIELD IF NOT EXISTS starts_at ON course_session TYPE int;
    DEFINE FIELD IF NOT EXISTS ends_at ON course_session TYPE option<int>;
    DEFINE INDEX IF NOT EXISTS course_session_course ON course_session FIELDS course;

    DEFINE TABLE IF NOT EXISTS session_attendance SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS session ON session_attendance TYPE record<course_session>;
    DEFINE FIELD IF NOT EXISTS course ON session_attendance TYPE record<course>;
    DEFINE FIELD IF NOT EXISTS user ON session_attendance TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS status ON session_attendance TYPE string;
    DEFINE FIELD IF NOT EXISTS marked_by ON session_attendance TYPE record<user>;
    DEFINE INDEX IF NOT EXISTS session_attendance_session_user ON session_attendance FIELDS session, user UNIQUE;
    DEFINE INDEX IF NOT EXISTS session_attendance_session ON session_attendance FIELDS session;
    DEFINE INDEX IF NOT EXISTS session_attendance_user ON session_attendance FIELDS user;
    DEFINE INDEX IF NOT EXISTS session_attendance_course ON session_attendance FIELDS course;

    DEFINE TABLE IF NOT EXISTS work_entry SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS user ON work_entry TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS check_in ON work_entry TYPE int;
    DEFINE FIELD IF NOT EXISTS check_out ON work_entry TYPE option<int>;
    DEFINE INDEX IF NOT EXISTS work_entry_user ON work_entry FIELDS user;

    DEFINE TABLE IF NOT EXISTS pomodoro_session SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS user ON pomodoro_session TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS started_at ON pomodoro_session TYPE int;
    DEFINE FIELD IF NOT EXISTS finished_at ON pomodoro_session TYPE option<int>;
    DEFINE INDEX IF NOT EXISTS pomodoro_session_user ON pomodoro_session FIELDS user;

    DEFINE TABLE IF NOT EXISTS exam SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS creator ON exam TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS course ON exam TYPE record<course>;
    DEFINE FIELD IF NOT EXISTS title ON exam TYPE string;
    DEFINE FIELD IF NOT EXISTS description ON exam TYPE string;
    DEFINE FIELD IF NOT EXISTS kind ON exam TYPE string;
    DEFINE FIELD IF NOT EXISTS mode ON exam TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS starts_at ON exam TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS ends_at ON exam TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS duration_ms ON exam TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS max_attempts ON exam TYPE int DEFAULT 1;
    DEFINE FIELD IF NOT EXISTS allow_rejoin ON exam TYPE bool DEFAULT true;
    DEFINE FIELD IF NOT EXISTS draft ON exam TYPE bool DEFAULT false;
    DEFINE INDEX IF NOT EXISTS exam_course ON exam FIELDS course;

    DEFINE TABLE IF NOT EXISTS exam_attempt SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS exam ON exam_attempt TYPE record<exam>;
    DEFINE FIELD IF NOT EXISTS user ON exam_attempt TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS seq ON exam_attempt TYPE int DEFAULT 1;
    DEFINE FIELD IF NOT EXISTS started_at ON exam_attempt TYPE int;
    DEFINE FIELD IF NOT EXISTS finished_at ON exam_attempt TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS left_at ON exam_attempt TYPE option<int>;
    DEFINE INDEX IF NOT EXISTS exam_attempt_exam_user_seq ON exam_attempt FIELDS exam, user, seq UNIQUE;
    DEFINE INDEX IF NOT EXISTS exam_attempt_exam ON exam_attempt FIELDS exam;

    DEFINE TABLE IF NOT EXISTS exam_question SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS exam ON exam_question TYPE record<exam>;
    DEFINE FIELD IF NOT EXISTS text ON exam_question TYPE string;
    DEFINE FIELD IF NOT EXISTS kind ON exam_question TYPE string;
    DEFINE FIELD IF NOT EXISTS points ON exam_question TYPE int;
    DEFINE FIELD IF NOT EXISTS choices ON exam_question TYPE option<array<string>>;
    DEFINE FIELD IF NOT EXISTS correct ON exam_question TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS subject ON exam_question TYPE record<subject>;
    DEFINE INDEX IF NOT EXISTS exam_question_exam ON exam_question FIELDS exam;
    DEFINE INDEX IF NOT EXISTS exam_question_subject ON exam_question FIELDS subject;

    DEFINE TABLE IF NOT EXISTS question_image SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS exam ON question_image TYPE record<exam>;
    DEFINE FIELD IF NOT EXISTS question ON question_image TYPE record<exam_question>;
    DEFINE FIELD IF NOT EXISTS slot ON question_image TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS file ON question_image TYPE string;
    DEFINE FIELD IF NOT EXISTS content_type ON question_image TYPE string;
    DEFINE FIELD IF NOT EXISTS size ON question_image TYPE int;
    DEFINE INDEX IF NOT EXISTS question_image_exam ON question_image FIELDS exam;
    DEFINE INDEX IF NOT EXISTS question_image_question ON question_image FIELDS question;

    DEFINE TABLE IF NOT EXISTS exam_answer SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS exam ON exam_answer TYPE record<exam>;
    DEFINE FIELD IF NOT EXISTS question ON exam_answer TYPE record<exam_question>;
    DEFINE FIELD IF NOT EXISTS user ON exam_answer TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS selected ON exam_answer TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS text ON exam_answer TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS updated_at ON exam_answer TYPE int;
    DEFINE INDEX IF NOT EXISTS exam_answer_question_user ON exam_answer FIELDS question, user UNIQUE;
    DEFINE INDEX IF NOT EXISTS exam_answer_exam_user ON exam_answer FIELDS exam, user;
    DEFINE INDEX IF NOT EXISTS exam_answer_exam ON exam_answer FIELDS exam;

    DEFINE TABLE IF NOT EXISTS exam_result SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS exam ON exam_result TYPE record<exam>;
    DEFINE FIELD IF NOT EXISTS user ON exam_result TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS mark ON exam_result TYPE int;
    DEFINE FIELD IF NOT EXISTS graded_by ON exam_result TYPE record<user>;
    DEFINE INDEX IF NOT EXISTS exam_result_exam_user ON exam_result FIELDS exam, user UNIQUE;

    DEFINE TABLE IF NOT EXISTS pool_question SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS asker ON pool_question TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS title ON pool_question TYPE string;
    DEFINE FIELD IF NOT EXISTS body ON pool_question TYPE string;
    DEFINE FIELD IF NOT EXISTS status ON pool_question TYPE string DEFAULT 'pending';
    DEFINE FIELD IF NOT EXISTS asked_at ON pool_question TYPE int;
    DEFINE FIELD IF NOT EXISTS approved_by ON pool_question TYPE option<record<user>>;
    DEFINE FIELD IF NOT EXISTS image_file ON pool_question TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS image_content_type ON pool_question TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS image_size ON pool_question TYPE option<int>;
    DEFINE INDEX IF NOT EXISTS pool_question_status ON pool_question FIELDS status;
    DEFINE INDEX IF NOT EXISTS pool_question_asker ON pool_question FIELDS asker;

    DEFINE TABLE IF NOT EXISTS solution SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS question ON solution TYPE record<pool_question>;
    DEFINE FIELD IF NOT EXISTS author ON solution TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS body ON solution TYPE string;
    DEFINE FIELD IF NOT EXISTS offered_at ON solution TYPE int;
    DEFINE FIELD IF NOT EXISTS image_file ON solution TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS image_content_type ON solution TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS image_size ON solution TYPE option<int>;
    DEFINE INDEX IF NOT EXISTS solution_question ON solution FIELDS question;

    DEFINE TABLE IF NOT EXISTS settings SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS exam_kinds ON settings TYPE array<object>;
    DEFINE FIELD IF NOT EXISTS exam_kinds.*.name ON settings TYPE string;
    DEFINE FIELD IF NOT EXISTS exam_kinds.*.weight ON settings TYPE int;
    DEFINE FIELD IF NOT EXISTS attendance_statuses ON settings TYPE array<string>;
    DEFINE FIELD IF NOT EXISTS grade_bands ON settings TYPE array<object>;
    DEFINE FIELD IF NOT EXISTS grade_bands.*.min ON settings TYPE int;
    DEFINE FIELD IF NOT EXISTS grade_bands.*.label ON settings TYPE string;
    DEFINE FIELD IF NOT EXISTS max_file_bytes ON settings TYPE option<int>;
";

/// Data backfills for rows written by older binaries. Runs *after* (and apart
/// from) the DDL batch so every statement sees the freshly defined schema —
/// see the `MIGRATION` doc for why sharing the batch corrupts the writes.
/// Idempotent: each backfill's `WHERE` only matches unconverted rows.
const BACKFILL: &str = "
    UPDATE user SET role = 'student' WHERE role = NONE;

    UPDATE course SET kind = 'course' WHERE kind = NONE;

    -- Courses predate assignable teachers (2026-07-21): every existing course
    -- was run by its creator alone, so it starts with nobody else assigned.
    UPDATE course SET teachers = [] WHERE teachers = NONE;

    -- Exams predate the draft flag (2026-07-19): everything already out there
    -- was live for its course, so it stays published.
    UPDATE exam SET draft = false WHERE draft = NONE;

    UPDATE event SET audience = { kind: 'school' } WHERE audience = NONE OR audience = {};

    -- Questions written before subjects existed (2026-07-17) are destroyed, not
    -- backfilled: a subject is mandatory and there is nothing truthful to
    -- assign. Their answers go first so no answer row outlives its question.
    DELETE exam_answer WHERE question IN (SELECT VALUE id FROM exam_question WHERE subject = NONE);
    DELETE exam_question WHERE subject = NONE;

    -- The hand-picked `users` audience retired into `registration` (2026-07-16):
    -- each listed user becomes a signup row credited to the event's creator, and
    -- the event becomes an uncapped registration list. Runs once — converted
    -- events no longer match the `kind = 'users'` filter. (`?? []`: an empty
    -- statement-subquery evaluates to NONE, which FOR refuses to iterate.)
    FOR $ev IN ((SELECT id, creator, audience FROM event WHERE audience.kind = 'users') ?? []) {
        FOR $usr IN ($ev.audience.users ?? []) {
            UPSERT type::record('registration', string::concat(record::id($ev.id), '_', record::id($usr))) CONTENT {
                event: $ev.id,
                user: $usr,
                registered_by: $ev.creator,
            };
        };
        UPDATE $ev.id SET audience = { kind: 'registration' };
    };

    -- Promotion out of student now deletes the user's enrollments (2026-07-18);
    -- this sweeps rows promoted before that fix. A deleted user reads as
    -- `user.role = NONE`, which is also != 'student' — those rows go too.
    DELETE enrollment WHERE user.role != 'student';
";

/// Connect to the SurrealDB server, sign in as root, and apply the schema.
pub async fn init(cfg: &Config) -> Result<Database, AppError> {
    let db = connect_with_retry(cfg).await;
    db.signin(Root {
        username: cfg.db_user.clone(),
        password: cfg.db_pass.clone(),
    })
    .await?;
    db.use_ns(cfg.db_ns.clone())
        .use_db(cfg.db_name.clone())
        .await?;
    migrate(&db).await?;
    Ok(std::sync::Arc::new(db))
}

/// Dial the database, retrying until it answers.
///
/// Never gives up, because giving up is worse than waiting: the database is
/// normally a sibling container booting in parallel, and a process that exits
/// on the first refusal gets restarted by the runtime, fails again in ~100ms,
/// and burns its whole restart budget inside a second — leaving the backend
/// permanently down over a startup skew that would have cleared on its own.
///
/// Only the dial is retried. Bad credentials or a broken migration still fail
/// hard, since no amount of waiting fixes those.
async fn connect_with_retry(cfg: &Config) -> Surreal<Any> {
    let mut backoff = 1;
    loop {
        match surrealdb::engine::any::connect(cfg.db_url.clone()).await {
            Ok(db) => return db,
            Err(err) => {
                tracing::warn!(
                    "database at {} unreachable ({err}) — retrying in {backoff}s",
                    cfg.db_url
                );
                tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(crate::constant::DB_CONNECT_BACKOFF_MAX_SECS);
            }
        }
    }
}

/// A fresh in-memory database with the schema applied. For tests.
pub async fn init_mem() -> Result<Database, AppError> {
    let db = surrealdb::engine::any::connect("memory").await?;
    db.use_ns("hezarfen").use_db("hezarfen").await?;
    migrate(&db).await?;
    Ok(std::sync::Arc::new(db))
}

/// Apply the schema + backfills. Idempotent — `init` runs it on every boot,
/// and tests re-run it on a live handle to simulate a second boot.
pub async fn migrate(db: &Surreal<Any>) -> Result<(), AppError> {
    db.query(MIGRATION).await?.check()?;
    db.query(BACKFILL).await?.check()?;
    Ok(())
}
