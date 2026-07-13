//! Embedded SurrealDB (surrealkv, file-backed) connection + schema.

use surrealdb::Surreal;
use surrealdb::engine::local::{Db, Mem, SurrealKv};

use crate::config::Config;
use crate::error::AppError;

pub type Database = Surreal<Db>;

pub const USER_TABLE: &str = "user";
pub const SESSION_TABLE: &str = "session";
pub const NOTE_TABLE: &str = "note";
pub const EVENT_TABLE: &str = "event";
pub const ATTENDANCE_TABLE: &str = "attendance";
pub const EXAM_TABLE: &str = "exam";
pub const EXAM_RESULT_TABLE: &str = "exam_result";
pub const EXAM_ATTEMPT_TABLE: &str = "exam_attempt";
pub const EXAM_QUESTION_TABLE: &str = "exam_question";
pub const EXAM_ANSWER_TABLE: &str = "exam_answer";
pub const COURSE_TABLE: &str = "course";
pub const ENROLLMENT_TABLE: &str = "enrollment";
pub const COURSE_SESSION_TABLE: &str = "course_session";
pub const SESSION_ATTENDANCE_TABLE: &str = "session_attendance";
pub const WORK_ENTRY_TABLE: &str = "work_entry";
pub const SETTINGS_TABLE: &str = "settings";
pub const TERM_TABLE: &str = "term";

/// SCHEMAFULL schema: every column is typed, references use `record<..>`.
/// Idempotent — safe to run on every boot.
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
    DEFINE INDEX IF NOT EXISTS user_username ON user FIELDS username UNIQUE;
    UPDATE user SET role = 'student' WHERE role = NONE;

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

    DEFINE TABLE IF NOT EXISTS event SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS creator ON event TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS title ON event TYPE string;
    DEFINE FIELD IF NOT EXISTS description ON event TYPE string;
    DEFINE FIELD IF NOT EXISTS starts_at ON event TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS ends_at ON event TYPE option<int>;

    DEFINE TABLE IF NOT EXISTS attendance SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS event ON attendance TYPE record<event>;
    DEFINE FIELD IF NOT EXISTS user ON attendance TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS status ON attendance TYPE string;
    DEFINE FIELD IF NOT EXISTS marked_by ON attendance TYPE record<user>;
    DEFINE INDEX IF NOT EXISTS attendance_event_user ON attendance FIELDS event, user UNIQUE;
    DEFINE INDEX IF NOT EXISTS attendance_user ON attendance FIELDS user;

    DEFINE TABLE IF NOT EXISTS term SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS name ON term TYPE string;
    DEFINE FIELD IF NOT EXISTS starts_at ON term TYPE int;
    DEFINE FIELD IF NOT EXISTS ends_at ON term TYPE int;

    DEFINE TABLE IF NOT EXISTS course SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS creator ON course TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS title ON course TYPE string;
    DEFINE FIELD IF NOT EXISTS description ON course TYPE string;
    DEFINE FIELD IF NOT EXISTS term ON course TYPE option<record<term>>;

    DEFINE TABLE IF NOT EXISTS enrollment SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS course ON enrollment TYPE record<course>;
    DEFINE FIELD IF NOT EXISTS user ON enrollment TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS enrolled_by ON enrollment TYPE record<user>;
    DEFINE INDEX IF NOT EXISTS enrollment_course_user ON enrollment FIELDS course, user UNIQUE;
    DEFINE INDEX IF NOT EXISTS enrollment_user ON enrollment FIELDS user;

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

    DEFINE TABLE IF NOT EXISTS exam SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS creator ON exam TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS course ON exam TYPE record<course>;
    DEFINE FIELD IF NOT EXISTS title ON exam TYPE string;
    DEFINE FIELD IF NOT EXISTS description ON exam TYPE string;
    DEFINE FIELD IF NOT EXISTS kind ON exam TYPE string;
    DEFINE FIELD IF NOT EXISTS weight ON exam TYPE int;
    DEFINE FIELD IF NOT EXISTS mode ON exam TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS starts_at ON exam TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS ends_at ON exam TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS duration_ms ON exam TYPE option<int>;
    DEFINE INDEX IF NOT EXISTS exam_course ON exam FIELDS course;

    DEFINE TABLE IF NOT EXISTS exam_attempt SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS exam ON exam_attempt TYPE record<exam>;
    DEFINE FIELD IF NOT EXISTS user ON exam_attempt TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS started_at ON exam_attempt TYPE int;
    DEFINE FIELD IF NOT EXISTS finished_at ON exam_attempt TYPE option<int>;
    DEFINE INDEX IF NOT EXISTS exam_attempt_exam_user ON exam_attempt FIELDS exam, user UNIQUE;
    DEFINE INDEX IF NOT EXISTS exam_attempt_exam ON exam_attempt FIELDS exam;

    DEFINE TABLE IF NOT EXISTS exam_question SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS exam ON exam_question TYPE record<exam>;
    DEFINE FIELD IF NOT EXISTS text ON exam_question TYPE string;
    DEFINE FIELD IF NOT EXISTS kind ON exam_question TYPE string;
    DEFINE FIELD IF NOT EXISTS points ON exam_question TYPE int;
    DEFINE FIELD IF NOT EXISTS choices ON exam_question TYPE option<array<string>>;
    DEFINE FIELD IF NOT EXISTS correct ON exam_question TYPE option<int>;
    DEFINE INDEX IF NOT EXISTS exam_question_exam ON exam_question FIELDS exam;

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

    DEFINE TABLE IF NOT EXISTS settings SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS exam_kinds ON settings TYPE array<string>;
    DEFINE FIELD IF NOT EXISTS attendance_statuses ON settings TYPE array<string>;
    DEFINE FIELD IF NOT EXISTS grade_bands ON settings TYPE array<object>;
    DEFINE FIELD IF NOT EXISTS grade_bands.*.min ON settings TYPE int;
    DEFINE FIELD IF NOT EXISTS grade_bands.*.label ON settings TYPE string;
";

/// Open the file-backed database and apply the schema.
pub async fn init(cfg: &Config) -> Result<Database, AppError> {
    let db = Surreal::new::<SurrealKv>(cfg.db_path.clone()).await?;
    db.use_ns(cfg.db_ns.clone())
        .use_db(cfg.db_name.clone())
        .await?;
    migrate(&db).await?;
    Ok(db)
}

/// A fresh in-memory database with the schema applied. For tests.
pub async fn init_mem() -> Result<Database, AppError> {
    let db = Surreal::new::<Mem>(()).await?;
    db.use_ns("hezarfen").use_db("hezarfen").await?;
    migrate(&db).await?;
    Ok(db)
}

async fn migrate(db: &Database) -> Result<(), AppError> {
    db.query(MIGRATION).await?.check()?;
    Ok(())
}
