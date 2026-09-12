-- School-database schema, part 3: exams, question bank, pool, homework
-- (Postgres).
--
-- DISJOINTNESS INVARIANT: the tables in migrations/school/*.sql and the ones
-- in migrations/control/*.sql together form ONE union schema, applied to a
-- single prepare database by scripts/prepare_db.sh for sqlx's compile-time
-- query macros. The two sets must never collide on a table name.
--
-- Same translation rules as parts 1-2. Sitting-scoped tables
-- (exam_attempt / exam_answer / answer_image / exam_result) key on their
-- natural composite (…, app_user, seq); a retake's row is a new row, never an
-- overwrite of the prior sitting. `choices` is JSONB (array of {id, text}
-- objects); exam.kind / exam.mode / homework_result.status /
-- pool_question.status / bank_question.visibility carry NO CHECK — validated
-- String newtypes in Rust, and exam kinds are school-configurable at runtime.

CREATE TABLE exam (
    id            uuid PRIMARY KEY,
    creator       uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    course        uuid NOT NULL REFERENCES course(id) ON DELETE NO ACTION,
    title         TEXT NOT NULL,
    description   TEXT NOT NULL,
    kind          TEXT NOT NULL,
    mode          TEXT NULL,
    starts_at     BIGINT NULL,
    ends_at       BIGINT NULL,
    duration_ms   BIGINT NULL,
    max_attempts  BIGINT NOT NULL DEFAULT 1,
    allow_rejoin  BOOLEAN NOT NULL DEFAULT true,
    allow_review  BOOLEAN NOT NULL DEFAULT false,
    draft         BOOLEAN NOT NULL DEFAULT false,
    result_count  BIGINT NOT NULL DEFAULT 0
);

CREATE INDEX exam_course ON exam (course);

-- Created before exam_question: source_exam points back at exam.
CREATE TABLE bank_question (
    id           uuid PRIMARY KEY,
    owner        uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    subject      uuid NULL REFERENCES subject(id) ON DELETE NO ACTION,
    text         TEXT NOT NULL,
    kind         TEXT NOT NULL,
    points       BIGINT NOT NULL,
    choices      JSONB NULL,
    correct      TEXT NULL,
    source_exam  uuid NULL REFERENCES exam(id) ON DELETE NO ACTION,
    -- 'private' | 'school'; DEFAULT so a partial write can never come out
    -- published.
    visibility   TEXT NOT NULL DEFAULT 'private',
    created_at   BIGINT NOT NULL
);

CREATE INDEX bank_question_owner ON bank_question (owner);
CREATE INDEX bank_question_subject ON bank_question (subject);

CREATE TABLE exam_question (
    id        uuid PRIMARY KEY,
    exam      uuid NOT NULL REFERENCES exam(id) ON DELETE NO ACTION,
    text      TEXT NOT NULL,
    kind      TEXT NOT NULL,
    points    BIGINT NOT NULL,
    choices   JSONB NULL,
    correct   TEXT NULL,
    subject   uuid NOT NULL REFERENCES subject(id) ON DELETE NO ACTION,
    -- Provenance, one column per direction: from_bank = the template this
    -- question was inserted from, banked_as = the template most recently
    -- minted by saving it into the bank.
    from_bank uuid NULL REFERENCES bank_question(id) ON DELETE NO ACTION,
    banked_as uuid NULL REFERENCES bank_question(id) ON DELETE NO ACTION
);

CREATE INDEX exam_question_exam ON exam_question (exam);
CREATE INDEX exam_question_subject ON exam_question (subject);

CREATE TABLE bank_question_image (
    bank_question uuid NOT NULL REFERENCES bank_question(id) ON DELETE NO ACTION,
    -- NULL = the question's own illustration; otherwise the choice's id. A
    -- (question, slot) pair holds at most one picture, and a question holds
    -- at most one illustration — NULLS NOT DISTINCT is what makes the NULL
    -- case count as taken.
    slot          TEXT NULL,
    file          TEXT NOT NULL,
    content_type  TEXT NOT NULL,
    size          BIGINT NOT NULL,
    CONSTRAINT bank_question_image_question_slot UNIQUE NULLS NOT DISTINCT (bank_question, slot)
);

CREATE TABLE question_image (
    exam         uuid NOT NULL REFERENCES exam(id) ON DELETE NO ACTION,
    question     uuid NOT NULL REFERENCES exam_question(id) ON DELETE NO ACTION,
    -- NULL = the question's own illustration; otherwise the choice's id. A
    -- (question, slot) pair holds at most one picture, and a question holds
    -- at most one illustration — NULLS NOT DISTINCT is what makes the NULL
    -- case count as taken.
    slot         TEXT NULL,
    file         TEXT NOT NULL,
    content_type TEXT NOT NULL,
    size         BIGINT NOT NULL,
    CONSTRAINT question_image_question_slot UNIQUE NULLS NOT DISTINCT (question, slot)
);

CREATE INDEX question_image_exam ON question_image (exam);
CREATE INDEX question_image_question ON question_image (question);

CREATE TABLE exam_attempt (
    exam        uuid NOT NULL REFERENCES exam(id) ON DELETE NO ACTION,
    app_user    uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    seq         BIGINT NOT NULL DEFAULT 1,
    started_at  BIGINT NOT NULL,
    finished_at BIGINT NULL,
    left_at     BIGINT NULL,
    CONSTRAINT exam_attempt_exam_user_seq PRIMARY KEY (exam, app_user, seq)
);

CREATE INDEX exam_attempt_exam ON exam_attempt (exam);

CREATE TABLE exam_answer (
    exam       uuid NOT NULL REFERENCES exam(id) ON DELETE NO ACTION,
    question   uuid NOT NULL REFERENCES exam_question(id) ON DELETE NO ACTION,
    app_user   uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    selected   TEXT NULL,
    text       TEXT NULL,
    updated_at BIGINT NOT NULL,
    seq        BIGINT NOT NULL DEFAULT 1,
    CONSTRAINT exam_answer_question_user_seq PRIMARY KEY (question, app_user, seq)
);

CREATE INDEX exam_answer_exam_user ON exam_answer (exam, app_user);
CREATE INDEX exam_answer_exam ON exam_answer (exam);

CREATE TABLE answer_image (
    exam         uuid NOT NULL REFERENCES exam(id) ON DELETE NO ACTION,
    question     uuid NOT NULL REFERENCES exam_question(id) ON DELETE NO ACTION,
    app_user     uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    file         TEXT NOT NULL,
    content_type TEXT NOT NULL,
    size         BIGINT NOT NULL,
    seq          BIGINT NOT NULL DEFAULT 1,
    CONSTRAINT answer_image_question_user_seq PRIMARY KEY (question, app_user, seq)
);

CREATE INDEX answer_image_exam ON answer_image (exam);
CREATE INDEX answer_image_user ON answer_image (app_user);

CREATE TABLE exam_result (
    exam      uuid NOT NULL REFERENCES exam(id) ON DELETE NO ACTION,
    app_user  uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    mark      BIGINT NOT NULL,
    graded_by uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    seq       BIGINT NOT NULL DEFAULT 1,
    CONSTRAINT exam_result_exam_user_seq PRIMARY KEY (exam, app_user, seq)
);

CREATE TABLE pool_question (
    id                uuid PRIMARY KEY,
    asker             uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    title             TEXT NOT NULL,
    body              TEXT NOT NULL,
    status            TEXT NOT NULL DEFAULT 'pending',
    asked_at          BIGINT NOT NULL,
    approved_by       uuid NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    image_file        TEXT NULL,
    image_content_type TEXT NULL,
    image_size        BIGINT NULL
);

CREATE INDEX pool_question_status ON pool_question (status);
CREATE INDEX pool_question_asker ON pool_question (asker);

CREATE TABLE solution (
    id                 uuid PRIMARY KEY,
    question           uuid NOT NULL REFERENCES pool_question(id) ON DELETE NO ACTION,
    author             uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    body               TEXT NOT NULL,
    offered_at         BIGINT NOT NULL,
    image_file         TEXT NULL,
    image_content_type TEXT NULL,
    image_size         BIGINT NULL
);

CREATE INDEX solution_question ON solution (question);

CREATE TABLE homework (
    id          uuid PRIMARY KEY,
    course      uuid NOT NULL REFERENCES course(id) ON DELETE NO ACTION,
    subject     uuid NOT NULL REFERENCES subject(id) ON DELETE NO ACTION,
    title       TEXT NOT NULL,
    description TEXT NULL,
    due_at      BIGINT NOT NULL,
    -- NULL/empty means the whole course.
    assigned    uuid[] NOT NULL DEFAULT '{}',
    created_by  uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    created_at  BIGINT NOT NULL
);

CREATE INDEX homework_course ON homework (course);

CREATE TABLE homework_result (
    id        uuid PRIMARY KEY,
    homework  uuid NOT NULL REFERENCES homework(id) ON DELETE NO ACTION,
    app_user  uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    status    TEXT NOT NULL,
    mark      BIGINT NULL,
    graded_by uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    created_at BIGINT NOT NULL,
    -- The deterministic {homework}_{user} pair: one grade per pair by
    -- construction (grading is one atomic upsert against this constraint).
    CONSTRAINT homework_result_homework_user UNIQUE (homework, app_user)
);

CREATE INDEX homework_result_homework ON homework_result (homework);

CREATE TABLE homework_submission (
    id               uuid PRIMARY KEY,
    homework         uuid NOT NULL REFERENCES homework(id) ON DELETE NO ACTION,
    app_user         uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    text             TEXT NULL,
    submitted_at     BIGINT NOT NULL,
    updated_at       BIGINT NOT NULL,
    file_count       BIGINT NOT NULL DEFAULT 0,
    -- The on-time verdict credited at the first hand-in, never re-judged.
    counted_on_time  BOOLEAN NULL,
    -- The grade that froze this submission; NULL while it is still open.
    graded_by_result uuid NULL REFERENCES homework_result(id) ON DELETE NO ACTION,
    -- Same deterministic pair as homework_result: one submission per
    -- (homework, user) by construction.
    CONSTRAINT homework_submission_homework_user UNIQUE (homework, app_user)
);

CREATE INDEX homework_submission_homework ON homework_submission (homework);

CREATE TABLE homework_file (
    id           uuid PRIMARY KEY,
    submission   uuid NOT NULL REFERENCES homework_submission(id) ON DELETE NO ACTION,
    name         TEXT NOT NULL,
    content_type TEXT NOT NULL,
    size         BIGINT NOT NULL,
    -- The on-disk blob name (GC key) — READONLY in the old schema, app
    -- discipline now.
    file         TEXT NOT NULL,
    created_at   BIGINT NOT NULL
);

CREATE INDEX homework_file_submission ON homework_file (submission);
