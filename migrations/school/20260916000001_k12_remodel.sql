-- School-database schema, part 5: the Turkish-K12 remodel — the class×course
-- *instance* (`class_course`) becomes the academic anchor.
--
-- Before this migration `course` was a school-wide singleton and every
-- downstream row keyed off it (`enrollment (course, app_user)`,
-- `exam.course`, `course_session.course`, `session_attendance.course`,
-- `homework.course`), while `class_course` was a hollow join carrying only
-- (`class`, `course`, `attached_by`, `source`). Two şubeler attaching the
-- same course therefore shared one roster, one exam set and one timetable.
--
-- After it: `class_course` is the instance (it carries the teachers, the
-- weekly `ders_saati` and the karne weight), exams/sessions/roll-call/
-- homework/enrollment re-key onto it, a şube belongs to an `academic_year`
-- (a dönem is a grading slice inside the year), and `course` is a pure
-- catalog.
--
-- Compatibility is waived (pre-customer): the identity-changing tables below
-- are dropped and recreated empty rather than backfilled, which keeps every
-- refcount seeded in this file consistent at 0. The drop list therefore also
-- includes the tables that merely *reference* a dropped one (bank_question,
-- question_image, answer_image, homework_assignment) so their foreign keys
-- are recreated instead of silently lost to CASCADE.
--
-- Same translation rules as parts 1-4: entity ids are app-minted UUID v7 (no
-- DB default), timestamps stay BIGINT unix-ms, counter columns are BIGINT
-- NOT NULL DEFAULT 0 and stay out of the Rust structs, all FKs ON DELETE NO
-- ACTION.

-- ---------------------------------------------------------------------------
-- 1. Academic year, above Term.
-- ---------------------------------------------------------------------------
CREATE TABLE academic_year (
    id               uuid PRIMARY KEY,
    name             TEXT NOT NULL CONSTRAINT academic_year_name_key UNIQUE,
    starts_at        BIGINT NOT NULL,
    ends_at          BIGINT NOT NULL,
    archived_at      BIGINT NULL,
    creator          uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    -- Sınıf geçme policy: [{from_grade, to_grade}, …]. A grade absent from
    -- this list is not rolled over — that is how graduation is expressed.
    grade_promotions JSONB NOT NULL DEFAULT '[]',
    class_count      BIGINT NOT NULL DEFAULT 0,
    term_count       BIGINT NOT NULL DEFAULT 0
);

-- ---------------------------------------------------------------------------
-- 2. Backfill one default year, bind term to it, move the class counter and
--    drop the term's course counter (course.term is removed in §6).
-- ---------------------------------------------------------------------------
-- Only on a database that already has terms: a fresh one needs no synthetic
-- year, and `creator` is NOT NULL so the row needs a real app_user to name.
INSERT INTO academic_year (id, name, starts_at, ends_at, creator, grade_promotions)
SELECT '00000000-0000-0000-0000-000000000001', 'Varsayılan Eğitim Yılı',
       COALESCE((SELECT min(starts_at) FROM term), 0),
       COALESCE((SELECT max(ends_at)   FROM term), 0),
       (SELECT id FROM app_user ORDER BY created_at LIMIT 1),
       '[]'
WHERE EXISTS (SELECT 1 FROM term);

ALTER TABLE term ADD COLUMN year uuid REFERENCES academic_year(id) ON DELETE NO ACTION;
UPDATE term SET year = '00000000-0000-0000-0000-000000000001';
ALTER TABLE term ALTER COLUMN year SET NOT NULL;

-- The term now tracks how many exams sit in it; the class counter moves to
-- the academic year (§3) and the course counter had no referent.
ALTER TABLE term ADD COLUMN exam_count BIGINT NOT NULL DEFAULT 0;
ALTER TABLE term DROP COLUMN course_count;
ALTER TABLE term DROP COLUMN class_count;

UPDATE academic_year SET term_count = (SELECT count(*) FROM term);

-- ---------------------------------------------------------------------------
-- 3. Şube belongs to the year, not to a term.
-- ---------------------------------------------------------------------------
ALTER TABLE class_group ADD COLUMN year uuid REFERENCES academic_year(id) ON DELETE NO ACTION;
UPDATE class_group c SET year = t.year FROM term t WHERE t.id = c.term;
ALTER TABLE class_group DROP COLUMN term;

UPDATE academic_year SET class_count = (SELECT count(*) FROM class_group WHERE year = academic_year.id);

-- ---------------------------------------------------------------------------
-- 4. course is a pure catalog: no capacity, no seat count, no term.
-- ---------------------------------------------------------------------------
ALTER TABLE course DROP COLUMN capacity;
ALTER TABLE course DROP COLUMN enrollment_count;
ALTER TABLE course DROP COLUMN term;
ALTER TABLE course ADD COLUMN class_course_count     BIGINT NOT NULL DEFAULT 0;
ALTER TABLE course ADD COLUMN course_membership_count BIGINT NOT NULL DEFAULT 0;

-- ---------------------------------------------------------------------------
-- 5. Drop the identity-changing tables (and every table that references one),
--    then recreate them empty against the instance key.
-- ---------------------------------------------------------------------------
DROP TABLE IF EXISTS
    exam_audience,
    exam_answer,
    answer_image,
    exam_attempt,
    exam_result,
    question_image,
    exam_question,
    bank_question_image,
    bank_question,
    exam,
    homework_assignment,
    homework_file,
    homework_submission,
    homework_result,
    homework,
    session_attendance,
    course_session,
    enrollment,
    course_membership,
    class_course_teacher,
    class_course,
    class_member,
    course_teacher
CASCADE;

-- ---------------------------------------------------------------------------
-- 5b. The emptied tables leave phantom counts on the KEPT ones.
--     Every counter below lives on a table this migration keeps but counts rows
--     in a table that was just emptied. Left alone on a non-empty database, the
--     phantom blocks the guard that reads it: a class_group would refuse its own
--     `class_member_count = 0 AND class_course_count = 0` delete guard (and eat
--     MAX_CLASS_MEMBERS seats) over members that no longer exist, a subject would
--     refuse its delete over questions/homework that are gone, and a kind_ref
--     would refuse its kind's retirement over marks that went with exam_result.
--     Counters on columns this migration *adds* (`course.class_course_count`,
--     `course.course_membership_count`, `term.exam_count`) are not listed: they
--     are born with DEFAULT 0.
-- ---------------------------------------------------------------------------
UPDATE class_group SET class_member_count = 0, class_course_count = 0;
UPDATE subject SET exam_question_count = 0, homework_count = 0;
UPDATE kind_ref SET count = 0;

-- The instance. It now carries the teachers (a junction), the weekly hours
-- and whether it counts toward the karne; `source` still marks blueprint vs
-- hand-placed. `enrollment_count` is the instance roster size — a counter,
-- not a capacity gate (the seat-claim 409 is deleted with D4/D5).
CREATE TABLE class_course (
    id                  uuid PRIMARY KEY,
    class               uuid NOT NULL REFERENCES class_group(id) ON DELETE NO ACTION,
    course              uuid NOT NULL REFERENCES course(id) ON DELETE NO ACTION,
    attached_by         uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    source              uuid NULL REFERENCES class_blueprint(id) ON DELETE NO ACTION,
    ders_saati          SMALLINT NOT NULL DEFAULT 1,
    counts_toward_karne BOOLEAN NOT NULL DEFAULT TRUE,
    enrollment_count    BIGINT NOT NULL DEFAULT 0,
    attached_at         BIGINT NOT NULL,
    CONSTRAINT class_course_class_course UNIQUE (class, course)
);

CREATE INDEX class_course_course ON class_course (course);
CREATE INDEX class_course_class  ON class_course (class);

-- D6: teacher assignment moves off the catalog course onto the instance.
CREATE TABLE class_course_teacher (
    class_course uuid NOT NULL REFERENCES class_course(id) ON DELETE NO ACTION,
    teacher      uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    CONSTRAINT class_course_teacher_pk PRIMARY KEY (class_course, teacher)
);

CREATE INDEX class_course_teacher_teacher ON class_course_teacher (teacher);

-- D11: a history table with a surrogate id. `left_at` NULL is the live stint;
-- the partial unique index enforces one live stint per (class, user) pair
-- while preserving rejoin history. A soft-left row holds no seat, so the
-- roster read and MAX_CLASS_MEMBERS always filter on `left_at IS NULL`.
CREATE TABLE class_member (
    id                 uuid PRIMARY KEY,
    class              uuid NOT NULL REFERENCES class_group(id) ON DELETE NO ACTION,
    app_user           uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    added_by           uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    joined_at          BIGINT NOT NULL,
    left_at            BIGINT NULL,
    source_class_group uuid NULL REFERENCES class_group(id) ON DELETE NO ACTION
);

CREATE UNIQUE INDEX class_member_live_pair ON class_member (class, app_user) WHERE left_at IS NULL;
CREATE INDEX class_member_user ON class_member (app_user);

-- D4: the roster is pumped per instance; individual (seçmeli) enrollment is
-- the same row shape, hand-placed (`source` NULL).
CREATE TABLE enrollment (
    class_course uuid NOT NULL REFERENCES class_course(id) ON DELETE NO ACTION,
    app_user     uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    enrolled_by  uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    source       uuid NULL REFERENCES class_group(id) ON DELETE NO ACTION,
    created_at   BIGINT NOT NULL,
    CONSTRAINT enrollment_class_course_user PRIMARY KEY (class_course, app_user)
);

-- D9: clubs/etüt are school-scoped memberships, not class-delivered.
CREATE TABLE course_membership (
    course     uuid NOT NULL REFERENCES course(id) ON DELETE NO ACTION,
    app_user   uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    added_by   uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    created_at BIGINT NOT NULL,
    CONSTRAINT course_membership_course_user PRIMARY KEY (course, app_user)
);

-- Exam now hangs off the instance and the dönem it belongs to.
CREATE TABLE exam (
    id            uuid PRIMARY KEY,
    creator       uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    class_course  uuid NOT NULL REFERENCES class_course(id) ON DELETE NO ACTION,
    term          uuid NOT NULL REFERENCES term(id) ON DELETE NO ACTION,
    title         TEXT NOT NULL,
    description   TEXT NOT NULL,
    kind          TEXT NOT NULL,
    mode          TEXT NULL,
    starts_at     BIGINT NULL,
    ends_at       BIGINT NULL,
    duration_ms   BIGINT NULL,
    max_attempts  BIGINT NOT NULL,
    allow_rejoin  BOOLEAN NOT NULL,
    allow_review  BOOLEAN NOT NULL,
    draft         BOOLEAN NOT NULL,
    result_count  BIGINT NOT NULL DEFAULT 0
);

CREATE INDEX exam_class_course ON exam (class_course);
CREATE INDEX exam_term         ON exam (term);

-- The sitting instances of one exam. The owning instance always gets a row,
-- so every exam read is one path: JOIN exam_audience.
CREATE TABLE exam_audience (
    exam         uuid NOT NULL REFERENCES exam(id) ON DELETE NO ACTION,
    class_course uuid NOT NULL REFERENCES class_course(id) ON DELETE NO ACTION,
    CONSTRAINT exam_audience_pk PRIMARY KEY (exam, class_course)
);

-- ---------------------------------------------------------------------------
-- 6. Recreate the exam / question-bank subtree verbatim (no key change: it
--    keys on exam, which now carries the instance).
-- ---------------------------------------------------------------------------
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
    visibility   TEXT NOT NULL DEFAULT 'private',
    created_at   BIGINT NOT NULL
);

CREATE INDEX bank_question_owner ON bank_question (owner);
CREATE INDEX bank_question_subject ON bank_question (subject);

CREATE TABLE bank_question_image (
    bank_question uuid NOT NULL REFERENCES bank_question(id) ON DELETE NO ACTION,
    slot          TEXT NULL,
    file          TEXT NOT NULL,
    content_type  TEXT NOT NULL,
    size          BIGINT NOT NULL,
    CONSTRAINT bank_question_image_question_slot UNIQUE NULLS NOT DISTINCT (bank_question, slot)
);

CREATE TABLE exam_question (
    id        uuid PRIMARY KEY,
    exam      uuid NOT NULL REFERENCES exam(id) ON DELETE NO ACTION,
    text      TEXT NOT NULL,
    kind      TEXT NOT NULL,
    points    BIGINT NOT NULL,
    choices   JSONB NULL,
    correct   TEXT NULL,
    subject   uuid NOT NULL REFERENCES subject(id) ON DELETE NO ACTION,
    from_bank uuid NULL REFERENCES bank_question(id) ON DELETE NO ACTION,
    banked_as uuid NULL REFERENCES bank_question(id) ON DELETE NO ACTION,
    CONSTRAINT exam_question_id_exam UNIQUE (id, exam)
);

CREATE INDEX exam_question_exam ON exam_question (exam);
CREATE INDEX exam_question_subject ON exam_question (subject);

CREATE TABLE question_image (
    exam         uuid NOT NULL,
    question     uuid NOT NULL,
    slot         TEXT NULL,
    file         TEXT NOT NULL,
    content_type TEXT NOT NULL,
    size         BIGINT NOT NULL,
    CONSTRAINT question_image_question_slot UNIQUE NULLS NOT DISTINCT (question, slot),
    CONSTRAINT question_image_exam_question_fkey FOREIGN KEY (exam, question)
        REFERENCES exam_question (exam, id) ON DELETE NO ACTION
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

CREATE TABLE exam_answer (
    exam       uuid NOT NULL,
    question   uuid NOT NULL,
    app_user   uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    selected   TEXT NULL,
    text       TEXT NULL,
    updated_at BIGINT NOT NULL,
    seq        BIGINT NOT NULL DEFAULT 1,
    CONSTRAINT exam_answer_question_user_seq PRIMARY KEY (question, app_user, seq),
    CONSTRAINT exam_answer_exam_question_fkey FOREIGN KEY (exam, question)
        REFERENCES exam_question (exam, id) ON DELETE NO ACTION
);

CREATE INDEX exam_answer_exam_user ON exam_answer (exam, app_user);

CREATE TABLE answer_image (
    exam         uuid NOT NULL,
    question     uuid NOT NULL,
    app_user     uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    file         TEXT NOT NULL,
    content_type TEXT NOT NULL,
    size         BIGINT NOT NULL,
    seq          BIGINT NOT NULL DEFAULT 1,
    CONSTRAINT answer_image_question_user_seq PRIMARY KEY (question, app_user, seq),
    CONSTRAINT answer_image_exam_question_fkey FOREIGN KEY (exam, question)
        REFERENCES exam_question (exam, id) ON DELETE NO ACTION
);

CREATE INDEX answer_image_exam ON answer_image (exam);
CREATE INDEX answer_image_user ON answer_image (app_user);

CREATE TABLE exam_result (
    exam      uuid NOT NULL REFERENCES exam(id) ON DELETE NO ACTION,
    app_user  uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    mark      BIGINT NOT NULL,
    graded_by uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    graded_at BIGINT NOT NULL,
    seq       BIGINT NOT NULL DEFAULT 1,
    CONSTRAINT exam_result_exam_user_seq PRIMARY KEY (exam, app_user, seq)
);

-- ---------------------------------------------------------------------------
-- 7. Sessions, roll-call and homework re-key onto the instance.
-- ---------------------------------------------------------------------------
CREATE TABLE course_session (
    id              uuid PRIMARY KEY,
    class_course    uuid NOT NULL REFERENCES class_course(id) ON DELETE NO ACTION,
    teacher         uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    topic           TEXT NOT NULL,
    starts_at       BIGINT NOT NULL,
    ends_at         BIGINT NULL,
    held_counted_at BIGINT NULL
);

CREATE INDEX course_session_class_course ON course_session (class_course);

CREATE TABLE session_attendance (
    session      uuid NOT NULL REFERENCES course_session(id) ON DELETE NO ACTION,
    app_user     uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    class_course uuid NOT NULL REFERENCES class_course(id) ON DELETE NO ACTION,
    status       TEXT NOT NULL,
    marked_by    uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    marked_at    BIGINT NOT NULL,
    CONSTRAINT session_attendance_session_user PRIMARY KEY (session, app_user)
);

CREATE INDEX session_attendance_user ON session_attendance (app_user);
CREATE INDEX session_attendance_class_course ON session_attendance (class_course);

CREATE TABLE homework (
    id          uuid PRIMARY KEY,
    class_course uuid NOT NULL REFERENCES class_course(id) ON DELETE NO ACTION,
    subject     uuid NOT NULL REFERENCES subject(id) ON DELETE NO ACTION,
    title       TEXT NOT NULL,
    description TEXT NULL,
    due_at      BIGINT NOT NULL,
    created_by  uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    created_at  BIGINT NOT NULL
);

CREATE INDEX homework_class_course ON homework (class_course);

CREATE TABLE homework_assignment (
    homework uuid NOT NULL REFERENCES homework(id) ON DELETE NO ACTION,
    student  uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    CONSTRAINT homework_assignment_homework_student PRIMARY KEY (homework, student)
);

CREATE INDEX homework_assignment_student ON homework_assignment (student);

CREATE TABLE homework_result (
    id        uuid PRIMARY KEY,
    homework  uuid NOT NULL REFERENCES homework(id) ON DELETE NO ACTION,
    app_user  uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    status    TEXT NOT NULL,
    mark      BIGINT NULL,
    graded_by uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    created_at BIGINT NOT NULL,
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
    counted_on_time  BOOLEAN NULL,
    graded_by_result uuid NULL REFERENCES homework_result(id) ON DELETE NO ACTION,
    CONSTRAINT homework_submission_homework_user UNIQUE (homework, app_user)
);

CREATE INDEX homework_submission_homework ON homework_submission (homework);

CREATE TABLE homework_file (
    id           uuid PRIMARY KEY,
    submission   uuid NOT NULL REFERENCES homework_submission(id) ON DELETE NO ACTION,
    name         TEXT NOT NULL,
    content_type TEXT NOT NULL,
    size         BIGINT NOT NULL,
    file         TEXT NOT NULL,
    created_at   BIGINT NOT NULL
);

CREATE INDEX homework_file_submission ON homework_file (submission);

-- ---------------------------------------------------------------------------
-- 8. Karne snapshot: the frozen per-student per-dönem report written when the
--    dönem is archived (karne is otherwise a computed read).
-- ---------------------------------------------------------------------------
CREATE TABLE karne_snapshot (
    term       uuid NOT NULL REFERENCES term(id) ON DELETE NO ACTION,
    app_user   uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    payload    JSONB NOT NULL,
    created_at BIGINT NOT NULL,
    CONSTRAINT karne_snapshot_term_user PRIMARY KEY (term, app_user)
);

-- ---------------------------------------------------------------------------
-- 9. Settings and profile additions (branş, absence limits, timezone).
-- ---------------------------------------------------------------------------
ALTER TABLE settings ADD COLUMN branches                   JSONB NULL;
ALTER TABLE settings ADD COLUMN excuse_kinds               JSONB NULL;
ALTER TABLE settings ADD COLUMN max_excused_absent_days    BIGINT NULL;
ALTER TABLE settings ADD COLUMN max_unexcused_absent_days  BIGINT NULL;
ALTER TABLE settings ADD COLUMN timezone                   TEXT NULL;

ALTER TABLE app_user ADD COLUMN branch TEXT NULL;
