//! The boot migration, as SurrealQL text.
//!
//! Split out of [`crate::constant`] purely for size: these three batches are
//! ~700 lines of schema, and left in the constants file they buried the
//! validation bounds that file exists to hold. Nothing else changed — they are
//! still `pub const`, still the only migration text, and
//! [`crate::database::migrate`] still reads [`MIGRATION_BATCHES`] and nothing
//! else.
//!
//! Numbers stay in `constant.rs`: `tests/limits_completeness.rs` sweeps `src/`
//! for stray bounds, and a bound hidden here would reach neither `GET /limits`
//! nor the OpenAPI spec. Only SurrealQL strings live in this file.

/// Repairs that must run *before* the DDL batch, because the DDL is what makes
/// them impossible.
///
/// `MIGRATION` retires `exam_question.source_bank` with `REMOVE FIELD`, and
/// once the column is gone SCHEMAFULL rejects every write to a row that still
/// stores it ("Found field 'source_bank', but no such field exists") — an
/// UNSET included. So the value has to go while its definition is still
/// standing. Runs as its own query for the usual reason (see `MIGRATION`).
///
/// `chatbot_message.claimed_by` / `claimed_at` (retired 2026-07-30 with the
/// chat claim queue) are the same story, one table over.
///
/// The table-exists guard is load-bearing: on a fresh database `MIGRATION` has
/// not run yet, and an UPDATE against an undefined table is an error, not an
/// empty result. On a database that has the table but never had the column the
/// `WHERE` simply matches nothing.
pub const PRE_REPAIR: &str = "
    IF 'exam_question' IN object::keys((INFO FOR DB).tables) {
        UPDATE exam_question UNSET source_bank WHERE source_bank != NONE
    };
    IF 'chatbot_message' IN object::keys((INFO FOR DB).tables) {
        UPDATE chatbot_message UNSET claimed_by, claimed_at
            WHERE claimed_by != NONE OR claimed_at != NONE
    };
";

/// SCHEMAFULL schema: every column is typed, references use `record<..>`.
/// Idempotent — safe to run on every boot: `IF NOT EXISTS` guards the
/// definitions and `REMOVE ... IF EXISTS` retires schema (like the
/// single-attempt unique index) exactly once. DDL only — data backfills live
/// in `BACKFILL`, which runs as a *separate* query: statements inside one
/// batch see the schema as it stood when the batch started, so an UPDATE next
/// to a fresh `DEFINE FIELD audience.*` writes against the old field set and
/// SCHEMAFULL silently strips the very keys being backfilled.
pub const MIGRATION: &str = "
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
    DEFINE FIELD IF NOT EXISTS palette_color ON user TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS chatbot_thread_count ON user TYPE option<int>;
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
    DEFINE FIELD IF NOT EXISTS file_count ON note TYPE option<int>;
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
    -- Where each side's copy sat before it was filed into archive/trash, so a
    -- restore lands where it came from. NONE while the copy is in its home
    -- folder — and on rows filed before this field existed (2026-07-23), which
    -- restore to the home folder just as they always did.
    DEFINE FIELD IF NOT EXISTS sender_origin ON message TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS recipient_origin ON message TYPE option<string>;
    DEFINE INDEX IF NOT EXISTS message_sender ON message FIELDS sender;
    DEFINE INDEX IF NOT EXISTS message_recipient ON message FIELDS recipient;

    DEFINE TABLE IF NOT EXISTS note_file SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS note ON note_file TYPE record<note>;
    DEFINE FIELD IF NOT EXISTS name ON note_file TYPE string;
    DEFINE FIELD IF NOT EXISTS content_type ON note_file TYPE string;
    DEFINE FIELD IF NOT EXISTS size ON note_file TYPE int;
    DEFINE INDEX IF NOT EXISTS note_file_note ON note_file FIELDS note;

    -- Chatbot relay (2026-07-23): a `thread` groups the turns, one
    -- `chatbot_message` is one turn. `user_id` rides on the message too so an
    -- ownership check needs no join. An assistant turn is born `pending` and
    -- is completed (or failed) by the task holding the AI-bridge stream —
    -- hence the BACKFILL sweep, since that task dies with the process.
    DEFINE TABLE IF NOT EXISTS chatbot_thread SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS user_id ON chatbot_thread TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS title ON chatbot_thread TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS created_at ON chatbot_thread TYPE int READONLY;
    DEFINE FIELD IF NOT EXISTS updated_at ON chatbot_thread TYPE int;
    DEFINE INDEX IF NOT EXISTS chatbot_thread_user_updated ON chatbot_thread FIELDS user_id, updated_at;

    DEFINE TABLE IF NOT EXISTS chatbot_message SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS thread_id ON chatbot_message TYPE record<chatbot_thread> READONLY;
    DEFINE FIELD IF NOT EXISTS user_id ON chatbot_message TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS role ON chatbot_message TYPE string READONLY;
    DEFINE FIELD IF NOT EXISTS content ON chatbot_message TYPE string;
    DEFINE FIELD IF NOT EXISTS status ON chatbot_message TYPE string;
    DEFINE FIELD IF NOT EXISTS truncated ON chatbot_message TYPE bool DEFAULT false;
    DEFINE FIELD IF NOT EXISTS error_code ON chatbot_message TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS created_at ON chatbot_message TYPE int READONLY;
    DEFINE FIELD IF NOT EXISTS completed_at ON chatbot_message TYPE option<int>;
    -- The chat claim queue is gone (2026-07-30): one process answers its own
    -- turns, so nothing claims them. PRE_REPAIR clears the values first —
    -- SCHEMAFULL rejects every write to a row still storing a column that no
    -- longer exists.
    REMOVE FIELD IF EXISTS claimed_by ON TABLE chatbot_message;
    REMOVE FIELD IF EXISTS claimed_at ON TABLE chatbot_message;
    DEFINE INDEX IF NOT EXISTS chatbot_message_thread_created ON chatbot_message FIELDS thread_id, created_at;
    DEFINE INDEX IF NOT EXISTS chatbot_message_user ON chatbot_message FIELDS user_id;
    -- The boot sweep asks for pending rows by age.
    DEFINE INDEX IF NOT EXISTS chatbot_message_status_created ON chatbot_message FIELDS status, created_at;

    -- The AI worker presence table is gone (2026-07-30): one process holds
    -- every worker socket, so its in-process registry is the whole truth and
    -- nothing needed publishing.
    REMOVE TABLE IF EXISTS ai_worker;

    -- The elected-boot lease is gone with it (2026-07-30): one process migrates,
    -- so nothing takes the lock. SCHEMALESS and read by nobody, so unlike the
    -- claim columns it needs no PRE_REPAIR — the table just goes.
    REMOVE TABLE IF EXISTS migration_lock;

    -- How much of a rate-limit budget the whole fleet has spent in one wall
    -- window (2026-07-27). One row per tier+client+window, id-keyed so every
    -- replica lands on the same record; `hits` is the cross-replica total each
    -- replica folds its local admits into (see `crate::rate_limit`). Rows are
    -- swept by the same task once their window is past.
    DEFINE TABLE IF NOT EXISTS rate_limit SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS hits ON rate_limit TYPE int DEFAULT 0;
    DEFINE FIELD IF NOT EXISTS window_start ON rate_limit TYPE int;

    DEFINE TABLE IF NOT EXISTS event SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS creator ON event TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS title ON event TYPE string;
    DEFINE FIELD IF NOT EXISTS description ON event TYPE string;
    DEFINE FIELD IF NOT EXISTS audience ON event TYPE object;
    DEFINE FIELD IF NOT EXISTS audience.kind ON event TYPE string;
    DEFINE FIELD IF NOT EXISTS audience.role ON event TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS audience.course ON event TYPE option<record<course>>;
    DEFINE FIELD IF NOT EXISTS audience.capacity ON event TYPE option<int>;
    -- Seats taken on the signup list, the cross-replica capacity guard (see
    -- `crate::domain::cap`). Absent reads as zero, so no Rust struct needs it.
    DEFINE FIELD IF NOT EXISTS registration_count ON event TYPE option<int>;
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
    -- Courses still linking this term, the cross-replica delete guard (see
    -- `crate::domain::cap`). Absent reads as zero, so no Rust struct needs it.
    DEFINE FIELD IF NOT EXISTS course_count ON term TYPE option<int>;

    DEFINE TABLE IF NOT EXISTS course SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS creator ON course TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS teachers ON course TYPE array<record<user>> DEFAULT [];
    DEFINE FIELD IF NOT EXISTS teachers[*] ON course TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS title ON course TYPE string;
    DEFINE FIELD IF NOT EXISTS description ON course TYPE string;
    DEFINE FIELD IF NOT EXISTS kind ON course TYPE string DEFAULT 'course';
    DEFINE FIELD IF NOT EXISTS term ON course TYPE option<record<term>>;
    DEFINE FIELD IF NOT EXISTS capacity ON course TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS enrollment_count ON course TYPE option<int>;

    DEFINE TABLE IF NOT EXISTS subject SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS course ON subject TYPE record<course>;
    DEFINE FIELD IF NOT EXISTS name ON subject TYPE string;
    DEFINE FIELD IF NOT EXISTS description ON subject TYPE string;
    -- How many exam questions and how many homework still point here. The
    -- delete is conditioned on both reading zero, which is what makes the
    -- guard hold against a question created on another replica.
    DEFINE FIELD IF NOT EXISTS exam_question_count ON subject TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS homework_count ON subject TYPE option<int>;
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
    DEFINE FIELD IF NOT EXISTS allow_review ON exam TYPE bool DEFAULT false;
    DEFINE FIELD IF NOT EXISTS draft ON exam TYPE bool DEFAULT false;
    -- How many marks the exam carries (2026-07-27). A kind change is refused
    -- against it, and the exam's own compare-and-set pins it, so a grade
    -- landing mid-PATCH refuses that save instead of slipping past its gates.
    DEFINE FIELD IF NOT EXISTS result_count ON exam TYPE option<int>;
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
    -- OVERWRITE, not IF NOT EXISTS: these columns changed type when choices
    -- gained stable ids (`array<string>`/`int` -> `array<object>`/`string`),
    -- and IF NOT EXISTS would leave an existing database on the old types.
    DEFINE FIELD OVERWRITE choices ON exam_question TYPE option<array<object>>;
    -- SCHEMAFULL rejects any nested key it wasn't told about, so the choice
    -- object's own fields are declared too (same shape as `settings.exam_kinds`).
    DEFINE FIELD OVERWRITE choices.*.id ON exam_question TYPE string;
    DEFINE FIELD OVERWRITE choices.*.text ON exam_question TYPE string;
    DEFINE FIELD OVERWRITE correct ON exam_question TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS subject ON exam_question TYPE record<subject>;
    -- Provenance, one column per direction: `from_bank` is the template this
    -- question was inserted from, `banked_as` the template most recently minted
    -- by saving it into the bank. They replace the single `source_bank`, which
    -- both directions wrote — so a question inserted from the bank read as
    -- already-saved. OVERWRITE (not IF NOT EXISTS): the old column has to
    -- actually go on an existing database, or SCHEMAFULL keeps accepting it
    -- while the struct no longer writes it.
    DEFINE FIELD OVERWRITE from_bank ON exam_question TYPE option<record<bank_question>>;
    DEFINE FIELD OVERWRITE banked_as ON exam_question TYPE option<record<bank_question>>;
    REMOVE FIELD IF EXISTS source_bank ON TABLE exam_question;
    DEFINE INDEX IF NOT EXISTS exam_question_exam ON exam_question FIELDS exam;
    DEFINE INDEX IF NOT EXISTS exam_question_subject ON exam_question FIELDS subject;

    DEFINE TABLE IF NOT EXISTS question_image SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS exam ON question_image TYPE record<exam>;
    DEFINE FIELD IF NOT EXISTS question ON question_image TYPE record<exam_question>;
    -- OVERWRITE: `slot` is now the choice's id, not its position.
    DEFINE FIELD OVERWRITE slot ON question_image TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS file ON question_image TYPE string;
    DEFINE FIELD IF NOT EXISTS content_type ON question_image TYPE string;
    DEFINE FIELD IF NOT EXISTS size ON question_image TYPE int;
    DEFINE INDEX IF NOT EXISTS question_image_exam ON question_image FIELDS exam;
    DEFINE INDEX IF NOT EXISTS question_image_question ON question_image FIELDS question;

    DEFINE TABLE IF NOT EXISTS bank_question SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS owner ON bank_question TYPE record<user>;
    -- OVERWRITE, not IF NOT EXISTS: the field started life as a required
    -- `record<subject>` and has to actually change type on an existing database,
    -- otherwise the subject-delete cascade (`SET subject = NONE`) is rejected.
    DEFINE FIELD OVERWRITE subject ON bank_question TYPE option<record<subject>>;
    DEFINE FIELD IF NOT EXISTS text ON bank_question TYPE string;
    DEFINE FIELD IF NOT EXISTS kind ON bank_question TYPE string;
    DEFINE FIELD IF NOT EXISTS points ON bank_question TYPE int;
    -- OVERWRITE, not IF NOT EXISTS: these columns changed type when choices
    -- gained stable ids (`array<string>`/`int` -> `array<object>`/`string`),
    -- and IF NOT EXISTS would leave an existing database on the old types.
    DEFINE FIELD OVERWRITE choices ON bank_question TYPE option<array<object>>;
    -- SCHEMAFULL rejects any nested key it wasn't told about, so the choice
    -- object's own fields are declared too (same shape as `settings.exam_kinds`).
    DEFINE FIELD OVERWRITE choices.*.id ON bank_question TYPE string;
    DEFINE FIELD OVERWRITE choices.*.text ON bank_question TYPE string;
    DEFINE FIELD OVERWRITE correct ON bank_question TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS source_exam ON bank_question TYPE option<record<exam>>;
    -- 'private' | 'school'. DEFAULT so a row that predates the field (or any
    -- partial write) can never come out published: the answer key stays the
    -- owner's until they publish it.
    DEFINE FIELD IF NOT EXISTS visibility ON bank_question TYPE string DEFAULT 'private';
    DEFINE FIELD IF NOT EXISTS created_at ON bank_question TYPE int READONLY;
    DEFINE INDEX IF NOT EXISTS bank_question_owner ON bank_question FIELDS owner;
    DEFINE INDEX IF NOT EXISTS bank_question_subject ON bank_question FIELDS subject;

    DEFINE TABLE IF NOT EXISTS bank_question_image SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS bank_question ON bank_question_image TYPE record<bank_question>;
    -- OVERWRITE: `slot` is now the choice's id, not its position.
    DEFINE FIELD OVERWRITE slot ON bank_question_image TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS file ON bank_question_image TYPE string;
    DEFINE FIELD IF NOT EXISTS content_type ON bank_question_image TYPE string;
    DEFINE FIELD IF NOT EXISTS size ON bank_question_image TYPE int;
    DEFINE INDEX IF NOT EXISTS bank_question_image_question ON bank_question_image FIELDS bank_question;

    DEFINE TABLE IF NOT EXISTS exam_answer SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS exam ON exam_answer TYPE record<exam>;
    DEFINE FIELD IF NOT EXISTS question ON exam_answer TYPE record<exam_question>;
    DEFINE FIELD IF NOT EXISTS user ON exam_answer TYPE record<user>;
    -- OVERWRITE: `selected` now names the picked option by id.
    DEFINE FIELD OVERWRITE selected ON exam_answer TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS text ON exam_answer TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS updated_at ON exam_answer TYPE int;
    -- `seq` numbers the sitting an answer belongs to (1, 2, …), mirroring
    -- exam_attempt. It rides the record key too, so a retake's answer is a new
    -- row, not an overwrite of the prior sitting.
    DEFINE FIELD IF NOT EXISTS seq ON exam_answer TYPE int DEFAULT 1;
    -- Retire the pre-history unique index: (question, user) is no longer unique
    -- once a student re-sits; (question, user, seq) is.
    REMOVE INDEX IF EXISTS exam_answer_question_user ON exam_answer;
    DEFINE INDEX IF NOT EXISTS exam_answer_question_user_seq ON exam_answer FIELDS question, user, seq UNIQUE;
    DEFINE INDEX IF NOT EXISTS exam_answer_exam_user ON exam_answer FIELDS exam, user;
    DEFINE INDEX IF NOT EXISTS exam_answer_exam ON exam_answer FIELDS exam;

    DEFINE TABLE IF NOT EXISTS answer_image SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS exam ON answer_image TYPE record<exam>;
    DEFINE FIELD IF NOT EXISTS question ON answer_image TYPE record<exam_question>;
    DEFINE FIELD IF NOT EXISTS user ON answer_image TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS file ON answer_image TYPE string;
    DEFINE FIELD IF NOT EXISTS content_type ON answer_image TYPE string;
    DEFINE FIELD IF NOT EXISTS size ON answer_image TYPE int;
    -- `seq` numbers the sitting this drawing belongs to; it rides the record
    -- key so a retake's image never overwrites the prior sitting's.
    DEFINE FIELD IF NOT EXISTS seq ON answer_image TYPE int DEFAULT 1;
    DEFINE INDEX IF NOT EXISTS answer_image_exam ON answer_image FIELDS exam;
    DEFINE INDEX IF NOT EXISTS answer_image_user ON answer_image FIELDS user;

    DEFINE TABLE IF NOT EXISTS exam_result SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS exam ON exam_result TYPE record<exam>;
    DEFINE FIELD IF NOT EXISTS user ON exam_result TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS mark ON exam_result TYPE int;
    DEFINE FIELD IF NOT EXISTS graded_by ON exam_result TYPE record<user>;
    -- `seq` numbers the sitting a mark belongs to; a retake earns its own mark
    -- row, and the latest seq is the student's standing.
    DEFINE FIELD IF NOT EXISTS seq ON exam_result TYPE int DEFAULT 1;
    REMOVE INDEX IF EXISTS exam_result_exam_user ON exam_result;
    DEFINE INDEX IF NOT EXISTS exam_result_exam_user_seq ON exam_result FIELDS exam, user, seq UNIQUE;

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
    DEFINE FIELD IF NOT EXISTS chatbot_history_turns ON settings TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS max_chatbot_threads ON settings TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS max_chatbot_message_len ON settings TYPE option<int>;
    -- Food program (2026-07-26). `meal_slots` mirrors the `exam_kinds` shape —
    -- a named list a menu snapshots from — and `dietary_tags` mirrors
    -- `attendance_statuses`. `meal_cancel_cutoff_minutes` is ONE knob for both
    -- the booking and the cancel deadline; absent means no cutoff at all. All
    -- three are `option<>`, exactly like `max_file_bytes`: NOT `DEFAULT []`.
    -- `DEFAULT` fires on create only, and `PATCH /settings` writes the whole
    -- row with `UPDATE ... CONTENT`, so a required-with-default field a writer
    -- omits coerces to NONE and fails every settings write until the domain
    -- struct carries it. `option<>` also means the existing singleton reads
    -- fine, which is why the food program needs no BACKFILL anywhere.
    DEFINE FIELD IF NOT EXISTS meal_slots ON settings TYPE option<array<object>>;
    DEFINE FIELD IF NOT EXISTS meal_slots.*.name ON settings TYPE string;
    -- Minutes past midnight UTC at which the slot is served (2026-07-26).
    -- `option<int>` for the same reason the list itself is: slots written
    -- before the field existed simply have no key, and read back as NONE ->
    -- the booking cutoff falls back to midnight UTC, its old meaning. No
    -- BACKFILL, and no DEFAULT — see the note above.
    DEFINE FIELD IF NOT EXISTS meal_slots.*.serving_minute ON settings TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS dietary_tags ON settings TYPE option<array<string>>;
    DEFINE FIELD IF NOT EXISTS meal_cancel_cutoff_minutes ON settings TYPE option<int>;

    -- Homework (greenfield, 2026-07-22): a teacher assigns per course, students
    -- submit files + optional text, a teacher grades a status + optional mark.
    -- `course`/`created_by`/`created_at` are READONLY (a homework never moves
    -- course, and stamps never rewrite); `subject` is NOT — it is re-taggable
    -- via PATCH. `assigned` NONE/empty means the whole course. No BACKFILL: the
    -- tables are new, so the whole migration stays additive and idempotent.
    DEFINE TABLE IF NOT EXISTS homework SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS course ON homework TYPE record<course> READONLY;
    DEFINE FIELD IF NOT EXISTS subject ON homework TYPE record<subject>;
    DEFINE FIELD IF NOT EXISTS title ON homework TYPE string;
    DEFINE FIELD IF NOT EXISTS description ON homework TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS due_at ON homework TYPE int;
    DEFINE FIELD IF NOT EXISTS assigned ON homework TYPE option<array<record<user>>>;
    DEFINE FIELD IF NOT EXISTS assigned[*] ON homework TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS created_by ON homework TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS created_at ON homework TYPE int READONLY;
    DEFINE INDEX IF NOT EXISTS homework_course ON homework FIELDS course;

    DEFINE TABLE IF NOT EXISTS homework_submission SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS homework ON homework_submission TYPE record<homework> READONLY;
    DEFINE FIELD IF NOT EXISTS user ON homework_submission TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS text ON homework_submission TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS submitted_at ON homework_submission TYPE int READONLY;
    DEFINE FIELD IF NOT EXISTS updated_at ON homework_submission TYPE int;
    DEFINE FIELD IF NOT EXISTS file_count ON homework_submission TYPE option<int>;
    -- The grade that froze this submission, absent while it is still open. The
    -- freeze used to be a cross-table read (does a homework_result exist?)
    -- followed by a write here, which no lock can hold across replicas; the
    -- stamp turns every submission edit into a conditional single-record write
    -- (`WHERE graded_by_result = NONE`), the one guard that survives. Set by
    -- grading, cleared by un-grading, never by the student.
    DEFINE FIELD IF NOT EXISTS graded_by_result ON homework_submission TYPE option<record<homework_result>>;
    DEFINE INDEX IF NOT EXISTS homework_submission_homework ON homework_submission FIELDS homework;

    DEFINE TABLE IF NOT EXISTS homework_file SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS submission ON homework_file TYPE record<homework_submission> READONLY;
    DEFINE FIELD IF NOT EXISTS name ON homework_file TYPE string;
    DEFINE FIELD IF NOT EXISTS content_type ON homework_file TYPE string;
    DEFINE FIELD IF NOT EXISTS size ON homework_file TYPE int;
    DEFINE FIELD IF NOT EXISTS file ON homework_file TYPE string READONLY;
    DEFINE FIELD IF NOT EXISTS created_at ON homework_file TYPE int READONLY;
    DEFINE INDEX IF NOT EXISTS homework_file_submission ON homework_file FIELDS submission;

    DEFINE TABLE IF NOT EXISTS homework_result SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS homework ON homework_result TYPE record<homework> READONLY;
    DEFINE FIELD IF NOT EXISTS user ON homework_result TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS status ON homework_result TYPE string;
    DEFINE FIELD IF NOT EXISTS mark ON homework_result TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS graded_by ON homework_result TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS created_at ON homework_result TYPE int;
    DEFINE INDEX IF NOT EXISTS homework_result_homework ON homework_result FIELDS homework;

    -- Appointments (greenfield, 2026-07-23): a teacher publishes availability
    -- slots, a student or parent books one. `series` groups the occurrences a
    -- weekly repeat expanded into, so one cancel deletes one row and a series
    -- delete finds the rest. A booking's live/dead state is the `status`
    -- string; `occupied` is the cap-1 counter that says whether the slot is
    -- taken (2026-07-27 — it used to be counted from the booking rows under a
    -- process-local lock, which two replicas could each pass), decremented in
    -- the same transaction as the reject/cancel that frees it.
    -- The `proposed_*` fields carry a teacher's counter-proposal on the same
    -- row until the requester accepts.
    DEFINE TABLE IF NOT EXISTS appointment_slot SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS teacher ON appointment_slot TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS starts_at ON appointment_slot TYPE int;
    DEFINE FIELD IF NOT EXISTS ends_at ON appointment_slot TYPE int;
    DEFINE FIELD IF NOT EXISTS occupied ON appointment_slot TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS note ON appointment_slot TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS series ON appointment_slot TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS created_at ON appointment_slot TYPE int;
    DEFINE INDEX IF NOT EXISTS appointment_slot_teacher_starts ON appointment_slot FIELDS teacher, starts_at;
    DEFINE INDEX IF NOT EXISTS appointment_slot_series ON appointment_slot FIELDS series;

    DEFINE TABLE IF NOT EXISTS appointment SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS slot ON appointment TYPE record<appointment_slot>;
    DEFINE FIELD IF NOT EXISTS requester ON appointment TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS status ON appointment TYPE string DEFAULT 'pending';
    DEFINE FIELD IF NOT EXISTS reason ON appointment TYPE string;
    DEFINE FIELD IF NOT EXISTS proposed_starts_at ON appointment TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS proposed_ends_at ON appointment TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS proposed_by ON appointment TYPE option<record<user>>;
    DEFINE FIELD IF NOT EXISTS decided_by ON appointment TYPE option<record<user>>;
    DEFINE FIELD IF NOT EXISTS cancelled_by ON appointment TYPE option<record<user>>;
    DEFINE FIELD IF NOT EXISTS cancel_reason ON appointment TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS reject_reason ON appointment TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS created_at ON appointment TYPE int;
    DEFINE INDEX IF NOT EXISTS appointment_slot_ref ON appointment FIELDS slot;
    DEFINE INDEX IF NOT EXISTS appointment_requester ON appointment FIELDS requester;
    DEFINE INDEX IF NOT EXISTS appointment_status ON appointment FIELDS status;

    -- Food program (greenfield, 2026-07-26): a published menu per day+slot,
    -- dishes on it, a dietary profile per student, bookings against a menu's
    -- capacity, who actually ate, and the money that moved. `date` is a
    -- calendar day as `YYYY-MM-DD` text, not a timestamp: the unique index is
    -- an equality test and a midnight-in-millis day is only unique for one
    -- timezone. `slot` is snapshotted text, not a link, so retiring a slot in
    -- settings never rewrites a past menu. Stamps stay `int` millis like the
    -- rest of the schema. No BACKFILL: all six tables are new.
    DEFINE TABLE IF NOT EXISTS menu SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS date ON menu TYPE string READONLY;
    DEFINE FIELD IF NOT EXISTS slot ON menu TYPE string READONLY;
    DEFINE FIELD IF NOT EXISTS capacity ON menu TYPE option<int>;
    -- `seats_booked` is the seat cap's counter (see `domain::cap`) and
    -- `version` the menu's revision: a booking claims its seat only while the
    -- menu still stands at the revision it read the price at, so a dish
    -- re-priced mid-booking cannot be billed as the old price. Both absent
    -- means zero, which is what `(x ?? 0)` reads on a row written before them.
    DEFINE FIELD IF NOT EXISTS seats_booked ON menu TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS version ON menu TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS created_by ON menu TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS created_at ON menu TYPE int READONLY;
    DEFINE INDEX IF NOT EXISTS menu_date_slot ON menu FIELDS date, slot UNIQUE;

    DEFINE TABLE IF NOT EXISTS menu_dish SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS menu ON menu_dish TYPE record<menu> READONLY;
    DEFINE FIELD IF NOT EXISTS name ON menu_dish TYPE string;
    DEFINE FIELD IF NOT EXISTS description ON menu_dish TYPE option<string>;
    -- Money is minor units (kuruş) as an integer, everywhere. Never decimal.
    DEFINE FIELD IF NOT EXISTS price_minor ON menu_dish TYPE int;
    DEFINE FIELD IF NOT EXISTS tags ON menu_dish TYPE array<string> DEFAULT [];
    DEFINE FIELD IF NOT EXISTS tags[*] ON menu_dish TYPE string;
    DEFINE FIELD IF NOT EXISTS created_at ON menu_dish TYPE int READONLY;
    DEFINE INDEX IF NOT EXISTS menu_dish_menu ON menu_dish FIELDS menu;

    DEFINE TABLE IF NOT EXISTS dietary_profile SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS student ON dietary_profile TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS tags ON dietary_profile TYPE array<string> DEFAULT [];
    DEFINE FIELD IF NOT EXISTS tags[*] ON dietary_profile TYPE string;
    DEFINE FIELD IF NOT EXISTS note ON dietary_profile TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS updated_by ON dietary_profile TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS updated_at ON dietary_profile TYPE int;
    DEFINE INDEX IF NOT EXISTS dietary_profile_student ON dietary_profile FIELDS student UNIQUE;

    -- A cancel flips `status` and stamps `cancelled_at`; the row stays so the
    -- freed seat is still auditable against the ledger line it charged.
    DEFINE TABLE IF NOT EXISTS meal_booking SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS menu ON meal_booking TYPE record<menu> READONLY;
    DEFINE FIELD IF NOT EXISTS student ON meal_booking TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS booked_by ON meal_booking TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS status ON meal_booking TYPE string DEFAULT 'booked';
    -- How many times this seat has been taken, and what it cost when the
    -- *current* attempt took it (NONE = the menu was free then). Together they
    -- key the attempt's ledger lines, which is what makes billing idempotent
    -- by identity rather than by scanning for an outstanding charge.
    DEFINE FIELD IF NOT EXISTS attempt ON meal_booking TYPE int DEFAULT 1;
    DEFINE FIELD IF NOT EXISTS price_minor ON meal_booking TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS cancelled_at ON meal_booking TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS created_at ON meal_booking TYPE int READONLY;
    DEFINE INDEX IF NOT EXISTS meal_booking_menu ON meal_booking FIELDS menu;
    DEFINE INDEX IF NOT EXISTS meal_booking_student ON meal_booking FIELDS student;

    DEFINE TABLE IF NOT EXISTS meal_attendance SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS menu ON meal_attendance TYPE record<menu> READONLY;
    DEFINE FIELD IF NOT EXISTS student ON meal_attendance TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS status ON meal_attendance TYPE string;
    DEFINE FIELD IF NOT EXISTS marked_by ON meal_attendance TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS marked_at ON meal_attendance TYPE int;
    DEFINE INDEX IF NOT EXISTS meal_attendance_menu_student ON meal_attendance FIELDS menu, student UNIQUE;

    -- APPEND-ONLY by design: a mistake is corrected with an opposing
    -- `reversal` line, never by editing or deleting one. Hence every field is
    -- READONLY. `source` is untyped on purpose — a charge points at the
    -- booking that caused it, a reversal at the line it undoes.
    DEFINE TABLE IF NOT EXISTS meal_ledger SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS student ON meal_ledger TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS kind ON meal_ledger TYPE string READONLY;
    DEFINE FIELD IF NOT EXISTS amount_minor ON meal_ledger TYPE int READONLY;
    DEFINE FIELD IF NOT EXISTS source ON meal_ledger TYPE option<record> READONLY;
    DEFINE FIELD IF NOT EXISTS method ON meal_ledger TYPE option<string> READONLY;
    DEFINE FIELD IF NOT EXISTS note ON meal_ledger TYPE option<string> READONLY;
    DEFINE FIELD IF NOT EXISTS recorded_by ON meal_ledger TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS created_at ON meal_ledger TYPE int READONLY;
    DEFINE INDEX IF NOT EXISTS meal_ledger_student ON meal_ledger FIELDS student;

    -- A school fee plan: a name and its installments, each an amount and the
    -- day it falls due. The installments are embedded (SCHEMAFULL, so the
    -- object's own fields are declared too, same shape as `settings.exam_kinds`)
    -- because a charge line copies the amount it was assigned at — child rows
    -- would buy nothing. Neither field inside the object is optional: a NONE
    -- value would be dropped from the stored object entirely.
    DEFINE TABLE IF NOT EXISTS fee_plan SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS name ON fee_plan TYPE string;
    DEFINE FIELD IF NOT EXISTS installments ON fee_plan TYPE array<object>;
    DEFINE FIELD IF NOT EXISTS installments.*.amount_minor ON fee_plan TYPE int;
    DEFINE FIELD IF NOT EXISTS installments.*.due_at ON fee_plan TYPE int;
    DEFINE FIELD IF NOT EXISTS created_by ON fee_plan TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS created_at ON fee_plan TYPE int READONLY;
    -- How many students are on the plan. The edit and delete guards read it,
    -- an assignment's own transaction increments it, and nothing decrements it
    -- (assignments are never removed). `option<int>` and no DEFAULT, like every
    -- other counter: no Rust struct carries it, so the plan's create must not
    -- be expected to state it.
    DEFINE FIELD IF NOT EXISTS assignment_count ON fee_plan TYPE option<int>;

    -- One plan on one student, keyed `<plan>_<student>`: assigning twice is the
    -- same record id, so a replay writes nothing and an assignment cut short
    -- half-way self-heals when it is repeated. Nothing here is ever edited.
    DEFINE TABLE IF NOT EXISTS fee_plan_assignment SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS plan ON fee_plan_assignment TYPE record<fee_plan> READONLY;
    DEFINE FIELD IF NOT EXISTS student ON fee_plan_assignment TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS assigned_by ON fee_plan_assignment TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS created_at ON fee_plan_assignment TYPE int READONLY;
    DEFINE INDEX IF NOT EXISTS fee_plan_assignment_plan ON fee_plan_assignment FIELDS plan;
    DEFINE INDEX IF NOT EXISTS fee_plan_assignment_student ON fee_plan_assignment FIELDS student;

    -- School fees, APPEND-ONLY by design exactly like `meal_ledger`: a mistake
    -- is corrected with an opposing line, never by editing or deleting one,
    -- hence every field is READONLY. `source` is untyped on purpose — a charge
    -- points at the `fee_plan_assignment` that raised it, a credit at the charge
    -- it pays, a refund at the credit it returns, a reversal at the line it
    -- undoes. `due_at` is set on charges alone.
    DEFINE TABLE IF NOT EXISTS payment_ledger SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS student ON payment_ledger TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS kind ON payment_ledger TYPE string READONLY;
    DEFINE FIELD IF NOT EXISTS amount_minor ON payment_ledger TYPE int READONLY;
    DEFINE FIELD IF NOT EXISTS source ON payment_ledger TYPE option<record> READONLY;
    DEFINE FIELD IF NOT EXISTS due_at ON payment_ledger TYPE option<int> READONLY;
    DEFINE FIELD IF NOT EXISTS method ON payment_ledger TYPE option<string> READONLY;
    DEFINE FIELD IF NOT EXISTS note ON payment_ledger TYPE option<string> READONLY;
    DEFINE FIELD IF NOT EXISTS recorded_by ON payment_ledger TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS created_at ON payment_ledger TYPE int READONLY;
    DEFINE INDEX IF NOT EXISTS payment_ledger_student ON payment_ledger FIELDS student;
    DEFINE INDEX IF NOT EXISTS payment_ledger_source ON payment_ledger FIELDS source;

    -- Reference counters, one row per *name* the settings offer, keyed by the
    -- name itself (2026-07-27). They are how 'a kind nothing is graded under
    -- may be removed' survives a second replica: the guard used to be a
    -- count-then-write across two tables under a process-wide lock, and the
    -- database serializes neither half of that. Both columns are `option<…>`
    -- so an absent row reads as zero references, in service — the counter is
    -- created by the first claim, never seeded per name.
    DEFINE TABLE IF NOT EXISTS kind_ref SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS count ON kind_ref TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS retired ON kind_ref TYPE option<bool>;

    DEFINE TABLE IF NOT EXISTS slot_ref SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS count ON slot_ref TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS retired ON slot_ref TYPE option<bool>;
";

/// Data backfills for rows written by older binaries. Runs *after* (and apart
/// from) the DDL batch so every statement sees the freshly defined schema —
/// see the `MIGRATION` doc for why sharing the batch corrupts the writes.
/// Idempotent: each backfill's `WHERE` only matches unconverted rows.
pub const BACKFILL: &str = "
    UPDATE user SET role = 'student' WHERE role = NONE;

    UPDATE course SET kind = 'course' WHERE kind = NONE;

    -- Courses predate assignable teachers (2026-07-21): every existing course
    -- was run by its creator alone, so it starts with nobody else assigned.
    UPDATE course SET teachers = [] WHERE teachers = NONE;

    -- Exams predate the draft flag (2026-07-19): everything already out there
    -- was live for its course, so it stays published.
    UPDATE exam SET draft = false WHERE draft = NONE;

    -- Exams predate the review toggle (2026-07-24): default it closed (opt-in).
    UPDATE exam SET allow_review = false WHERE allow_review = NONE;

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

    -- Turns written before the clipped-answer flag existed (2026-07-23): the
    -- clip was silent then, so nothing can be recovered — they read as whole,
    -- which is what they were presented as all along.
    UPDATE chatbot_message SET truncated = false WHERE truncated = NONE;

    -- An assistant turn is answered by a task in this process, so a restart can
    -- leave its row `pending` with nobody left to complete it. Only rows past
    -- the stale horizon ($stale_ms, from `CHATBOT_PENDING_STALE_SECS`) are
    -- certainly abandoned though: a young one may still be being answered on
    -- the other side of the AI bridge, and a deploy must not shoot down a turn
    -- dispatched seconds ago. Nothing is lost by waiting — a reader already
    -- presents an over-age `pending` row as failed, and the next boot sweeps
    -- for real whatever crossed the horizon in the meantime.
    --
    -- The claim half of this guard went with the claim queue (2026-07-30): it
    -- only ever spared a row a *live peer* was answering, and there is no peer.
    -- The rows selected are otherwise the same ones — an unclaimed row past the
    -- horizon was always swept, and a claim of this process's own could not
    -- outlive the process whose boot is running this.
    UPDATE chatbot_message SET status = 'failed', error_code = 'interrupted',
        completed_at = time::unix(time::now()) * 1000
        WHERE status = 'pending' AND created_at < time::unix(time::now()) * 1000 - $stale_ms;

    -- Promotion out of student now deletes the user's enrollments (2026-07-18);
    -- this sweeps rows promoted before that fix. A deleted user reads as
    -- `user.role = NONE`, which is also != 'student' — those rows go too.
    DELETE enrollment WHERE user.role != 'student';

    -- Choices gained stable ids (2026-07-24): `choices` held bare strings and
    -- `correct`/`selected`/`slot` held the option's *position*. The DDL above
    -- retypes those columns, which leaves every old-shaped row readable but
    -- unwritable ('Expected `none | array<object>` but found `[..]`'), so this
    -- conversion is repair, not cosmetics.
    --
    -- Ids are minted in array order, so the id at position i *is* the id for
    -- old index i — that identity is what keeps a stored answer, and an option
    -- picture, pointing at the option the student actually saw. Hence the
    -- dependent rows are remapped inside the loop that mints `$new`, against
    -- that same array: once the ids are stored the positions are still
    -- recoverable, but nothing guarantees a later pass would look them up the
    -- same way. `WHERE choices[0] != NONE AND type::is_string(choices[0])`
    -- matches only unconverted rows, so a second boot re-mints nothing; an
    -- empty or absent `choices` needs no conversion at all. A question-level
    -- picture (`slot = NONE`) is not an index and is left alone.
    --
    -- Runs *before* every other backfill that writes these rows (the `seq`
    -- stamps below): a write coerces the whole record, so touching a row for
    -- any reason fails while its choices are still positional. For the same
    -- reason the answer's remap stamps `seq` itself — the two legacy gaps sit
    -- on the same row, and a write that fixes only one of them is rejected for
    -- the other. `seq ?? 1` leaves an already-numbered sitting alone (DEFAULT
    -- fills a CREATE, not an UPDATE of a row that predates the column).
    FOR $q IN ((SELECT id, choices, correct FROM exam_question
        WHERE choices[0] != NONE AND type::is_string(choices[0])) ?? []) {
        LET $new = $q.choices.map(|$c| { id: rand::ulid(), text: $c });
        UPDATE $q.id SET choices = $new,
            correct = IF $q.correct = NONE { NONE } ELSE { $new[$q.correct].id };
        FOR $img IN ((SELECT id, slot FROM question_image
            WHERE question = $q.id AND type::is_int(slot)) ?? []) {
            UPDATE $img.id SET slot = $new[$img.slot].id;
        };
        FOR $ans IN ((SELECT id, selected FROM exam_answer
            WHERE question = $q.id AND type::is_int(selected)) ?? []) {
            UPDATE $ans.id SET selected = $new[$ans.selected].id, seq = seq ?? 1;
        };
    };

    FOR $q IN ((SELECT id, choices, correct FROM bank_question
        WHERE choices[0] != NONE AND type::is_string(choices[0])) ?? []) {
        LET $new = $q.choices.map(|$c| { id: rand::ulid(), text: $c });
        UPDATE $q.id SET choices = $new,
            correct = IF $q.correct = NONE { NONE } ELSE { $new[$q.correct].id };
        FOR $img IN ((SELECT id, slot FROM bank_question_image
            WHERE bank_question = $q.id AND type::is_int(slot)) ?? []) {
            UPDATE $img.id SET slot = $new[$img.slot].id;
        };
    };

    -- Per-attempt history (2026-07-24): answers, drawings, and marks written
    -- before retakes stopped wiping belong to the student's first sitting.
    -- They already use the bare (seq==1) record key, so only the denormalized
    -- `seq` field needs stamping.
    UPDATE exam_answer SET seq = 1 WHERE seq = NONE;
    UPDATE answer_image SET seq = 1 WHERE seq = NONE;
    UPDATE exam_result SET seq = 1 WHERE seq = NONE;

    -- The homework freeze became cross-replica (2026-07-27): a grade that
    -- predates the stamp column has to put it on the submission it already
    -- froze, or that submission would read as open and be editable again.
    -- Result and submission share the deterministic `{homework}_{user}` key, so
    -- the owning row is addressed directly rather than searched for; an UPDATE
    -- of a record that isn't there (graded absent work) is an empty no-op. The
    -- `= NONE` guard keeps it one-time, for the same reason the counters below
    -- are one-time — a peer replica is serving while this runs.
    FOR $r IN ((SELECT VALUE id FROM homework_result) ?? []) {
        UPDATE type::record('homework_submission', record::id($r))
            SET graded_by_result = $r WHERE graded_by_result = NONE;
    };

    -- Count caps became cross-replica (2026-07-27): the authority moved from a
    -- process-wide mutex to a counter column on the parent row, so every parent
    -- that predates the column is seeded from the children it actually has.
    -- Runs LAST, after the sweeps above have deleted whatever they delete.
    --
    -- Deliberately one-time (`= NONE` guards every write): a peer replica is
    -- serving while this boots, and recomputing a counter it is concurrently
    -- incrementing would hand back a seat that is already taken. An absent
    -- counter reads as zero, so the group pass runs before the zero pass.
    FOR $row IN ((SELECT course, count() AS n FROM enrollment GROUP BY course) ?? []) {
        UPDATE $row.course SET enrollment_count = $row.n WHERE enrollment_count = NONE;
    };
    UPDATE course SET enrollment_count = 0 WHERE enrollment_count = NONE;

    -- The one refcount among them: courses per term, not children per parent.
    -- Same one-time `= NONE` guard, and `WHERE term != NONE` keeps the
    -- unlinked courses out of the group (they would form a NONE bucket).
    FOR $row IN ((SELECT term, count() AS n FROM course WHERE term != NONE GROUP BY term) ?? []) {
        UPDATE $row.term SET course_count = $row.n WHERE course_count = NONE;
    };
    UPDATE term SET course_count = 0 WHERE course_count = NONE;

    FOR $row IN ((SELECT event, count() AS n FROM registration GROUP BY event) ?? []) {
        UPDATE $row.event SET registration_count = $row.n WHERE registration_count = NONE;
    };
    UPDATE event SET registration_count = 0 WHERE registration_count = NONE;

    FOR $row IN ((SELECT note, count() AS n FROM note_file GROUP BY note) ?? []) {
        UPDATE $row.note SET file_count = $row.n WHERE file_count = NONE;
    };
    UPDATE note SET file_count = 0 WHERE file_count = NONE;

    FOR $row IN ((SELECT submission, count() AS n FROM homework_file GROUP BY submission) ?? []) {
        UPDATE $row.submission SET file_count = $row.n WHERE file_count = NONE;
    };
    UPDATE homework_submission SET file_count = 0 WHERE file_count = NONE;

    FOR $row IN ((SELECT user_id, count() AS n FROM chatbot_thread GROUP BY user_id) ?? []) {
        UPDATE $row.user_id SET chatbot_thread_count = $row.n WHERE chatbot_thread_count = NONE;
    };
    UPDATE user SET chatbot_thread_count = 0 WHERE chatbot_thread_count = NONE;

    -- The appointment slot's counter is the odd one out: it counts only the
    -- bookings that are still *live*, because a rejected or cancelled one gave
    -- the slot back long before this column existed.
    FOR $row IN ((SELECT slot, count() AS n FROM appointment
        WHERE status IN ['pending', 'approved'] GROUP BY slot) ?? []) {
        UPDATE $row.slot SET occupied = $row.n WHERE occupied = NONE;
    };
    UPDATE appointment_slot SET occupied = 0 WHERE occupied = NONE;

    -- A menu's seats, counted from the bookings still *held* — a cancelled one
    -- gave its seat back before this column existed, exactly like the slot
    -- above. `version` gets no pass at all: absent already reads as revision
    -- zero everywhere it is compared, so writing it would be a no-op that
    -- touches every menu row.
    FOR $row IN ((SELECT menu, count() AS n FROM meal_booking
        WHERE status = 'booked' GROUP BY menu) ?? []) {
        UPDATE $row.menu SET seats_booked = $row.n WHERE seats_booked = NONE;
    };
    UPDATE menu SET seats_booked = 0 WHERE seats_booked = NONE;

    -- A subject's two reference counters, seeded from what actually points at
    -- it. This must run *after* the legacy sweep above destroys the questions
    -- that predate subjects, or a subject would be seeded with rows that are
    -- about to vanish and could then never be deleted. No zero pass, for the
    -- same reason `version` gets none: absent already reads as zero in the
    -- delete's `(count ?? 0) = 0` guard, so writing it would touch every
    -- subject row to say what it already says.
    FOR $row IN ((SELECT subject, count() AS n FROM exam_question GROUP BY subject) ?? []) {
        UPDATE $row.subject SET exam_question_count = $row.n WHERE exam_question_count = NONE;
    };

    FOR $row IN ((SELECT subject, count() AS n FROM homework GROUP BY subject) ?? []) {
        UPDATE $row.subject SET homework_count = $row.n WHERE homework_count = NONE;
    };

    -- The settings guards became cross-replica the same way (2026-07-27): an
    -- exam kind counts the marks written under it, a meal slot the menus
    -- published for it, and a name is removable exactly while its counter reads
    -- zero. Rows written before the counters existed are counted once here.
    --
    -- Same `= NONE` guard and the same reason as the caps above: a peer replica
    -- may already be serving, and recomputing a counter it is incrementing
    -- would hand back a reference that is still held. No zero pass either — an
    -- absent counter already reads as zero, and a name nobody ever used needs
    -- no row at all.
    --
    -- Marks predate the counter but so may their *kind*: an exam whose kind was
    -- removed from the list long ago is counted too, which only ever refuses a
    -- removal that already happened. `slot` is text on the menu, so its group
    -- needs no join; `kind` lives on the exam, one hop from the mark.
    FOR $row IN ((SELECT exam.kind AS kind, count() AS n FROM exam_result GROUP BY kind) ?? []) {
        UPSERT type::record('kind_ref', $row.kind) SET count = $row.n WHERE count = NONE;
    };

    FOR $row IN ((SELECT slot, count() AS n FROM menu GROUP BY slot) ?? []) {
        UPSERT type::record('slot_ref', $row.slot) SET count = $row.n WHERE count = NONE;
    };

    -- Same one-time seeding for the per-exam mark counter: an exam graded by an
    -- older binary must read as graded, or its kind could be changed under the
    -- marks it already carries.
    FOR $row IN ((SELECT exam, count() AS n FROM exam_result GROUP BY exam) ?? []) {
        UPDATE $row.exam SET result_count = $row.n WHERE result_count = NONE;
    };

    -- The fee-plan guard became a refcount (2026-07-30): editing or deleting a
    -- plan is now refused by `(assignment_count ?? 0) = 0` on the plan row
    -- itself, instead of by a scan a concurrent assign could land behind.
    --
    -- This seeding is what keeps a *stale* volume safe: a plan written by an
    -- older binary carries no counter, an absent counter reads as zero, and a
    -- zero would make an already-assigned plan editable and deletable again —
    -- the exact thing the 409 exists to prevent. Boot seeds it from the
    -- assignment rows that actually exist, and `migrate` runs to completion
    -- before the router is bound, so no request is ever served against the
    -- unseeded shape. `= NONE` keeps it one-time, so a second boot recomputes
    -- nothing.
    --
    -- No zero pass, for the same reason the subject counters get none: a plan
    -- nobody is on has no assignment rows, and absent already reads as zero in
    -- the guard — writing it would touch every plan row to say what it says.
    FOR $row IN ((SELECT plan, count() AS n FROM fee_plan_assignment GROUP BY plan) ?? []) {
        UPDATE $row.plan SET assignment_count = $row.n WHERE assignment_count = NONE;
    };
";

/// The migration batches, in the order a boot applies them — and the *only*
/// list of them. [`crate::database::migrate`] runs exactly these. They stay
/// three separate queries: statements in one batch see the schema as it stood
/// when the batch started (see `MIGRATION`).
pub const MIGRATION_BATCHES: [&str; 3] = [PRE_REPAIR, MIGRATION, BACKFILL];
