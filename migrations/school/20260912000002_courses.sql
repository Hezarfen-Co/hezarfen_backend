-- School-database schema, part 2: courses, terms, classes, enrollment,
-- events, attendance (Postgres).
--
-- DISJOINTNESS INVARIANT: the tables in migrations/school/*.sql and the ones
-- in migrations/control/*.sql together form ONE union schema, applied to a
-- single prepare database by scripts/prepare_db.sh for sqlx's compile-time
-- query macros. The two sets must never collide on a table name.
--
-- Same translation rules as part 1: table `user` -> app_user (and every
-- column named `user`), record refs -> uuid FKs ON DELETE NO ACTION, natural
-- composite PKs replace the old `{a}_{b}` text record keys (the PK constraint
-- carries the old unique-index name — it is what surfaces in SQLSTATE 23505),
-- counters are BIGINT NOT NULL DEFAULT 0 and stay out of the Rust structs.

CREATE TABLE term (
    id           uuid PRIMARY KEY,
    name         TEXT NOT NULL,
    starts_at    BIGINT NOT NULL,
    ends_at      BIGINT NOT NULL,
    -- When the term was archived; NULL = open.
    archived_at  BIGINT NULL,
    course_count BIGINT NOT NULL DEFAULT 0,
    class_count  BIGINT NOT NULL DEFAULT 0
);
-- A term is dropped, not cascaded: the nullable `term` FKs on course and
-- class_group (ON DELETE NO ACTION) must be cleared to NULL first — a delete
-- with a reference still standing is a 23503, not a sweep. The stored
-- course_count/class_count are the pre-flight counters that refuse the drop
-- before the FKs get the chance.

-- db::course::delete sweeps every child in ONE transaction — a crash between
-- statements must orphan nothing — so the guard is the course row itself:
-- locked FOR UPDATE and refused while its enrollment_count is non-zero. The
-- child order is the FK-safest one: exam results/attempts/answers/images and
-- questions, homework files/submissions/results, rag output, note files and
-- notes, class_course links (each class gets its class_course_count back and
-- every blueprint drops the id from its list), session_attendance and
-- course_session, enrollment (each seat given back), then the exam, homework
-- and subject rows themselves. Bank questions keep their templates; only
-- their subject/source_exam links are cleared.
CREATE TABLE course (
    id               uuid PRIMARY KEY,
    creator          uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    teachers         uuid[] NOT NULL DEFAULT '{}',
    title            TEXT NOT NULL,
    description      TEXT NOT NULL,
    kind             TEXT NOT NULL DEFAULT 'course',
    term             uuid NULL REFERENCES term(id) ON DELETE NO ACTION,
    capacity         BIGINT NULL,
    enrollment_count BIGINT NOT NULL DEFAULT 0
);
-- No GIN on teachers, faithful to the Surreal schema: the teachers-membership
-- read stays a scan, exactly as today (see src/db/course.rs — the Surreal
-- per-element index form returned zero rows and was never defined).

CREATE TABLE subject (
    id                  uuid PRIMARY KEY,
    course              uuid NOT NULL REFERENCES course(id) ON DELETE NO ACTION,
    name                TEXT NOT NULL,
    description         TEXT NOT NULL,
    exam_question_count BIGINT NOT NULL DEFAULT 0,
    homework_count      BIGINT NOT NULL DEFAULT 0
);

CREATE INDEX subject_course ON subject (course);

CREATE TABLE course_note (
    id         uuid PRIMARY KEY,
    course     uuid NOT NULL REFERENCES course(id) ON DELETE NO ACTION,
    author     uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    title      TEXT NOT NULL,
    content    TEXT NOT NULL,
    file_count BIGINT NOT NULL DEFAULT 0
);

CREATE INDEX course_note_course ON course_note (course);

CREATE TABLE course_note_file (
    id           uuid PRIMARY KEY,
    course_note  uuid NOT NULL REFERENCES course_note(id) ON DELETE NO ACTION,
    name         TEXT NOT NULL,
    content_type TEXT NOT NULL,
    size         BIGINT NOT NULL
);

CREATE INDEX course_note_file_note ON course_note_file (course_note);

-- What an AI service produced for a course note. Derived and disposable;
-- payload is JSONB because its shape belongs to the service, not this schema.
CREATE TABLE rag_output (
    id          uuid PRIMARY KEY,
    course_note uuid NOT NULL REFERENCES course_note(id) ON DELETE NO ACTION,
    course      uuid NOT NULL REFERENCES course(id) ON DELETE NO ACTION,
    sources     uuid[] NOT NULL DEFAULT '{}',
    payload     JSONB NOT NULL,
    generated_at BIGINT NOT NULL
);

CREATE INDEX rag_output_note ON rag_output (course_note);
-- The source cascade asks `sources CONTAINS $file`; the GIN index makes it a
-- lookup instead of a scan.
CREATE INDEX rag_output_source ON rag_output USING GIN (sources);

CREATE TABLE course_session (
    id              uuid PRIMARY KEY,
    course          uuid NOT NULL REFERENCES course(id) ON DELETE NO ACTION,
    teacher         uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    topic           TEXT NOT NULL,
    starts_at       BIGINT NOT NULL,
    ends_at         BIGINT NULL,
    -- The once-per-session guard behind lessons_held_total: stamped by the
    -- first roll call taken for the lesson.
    held_counted_at BIGINT NULL
);

CREATE INDEX course_session_course ON course_session (course);

CREATE TABLE session_attendance (
    session   uuid NOT NULL REFERENCES course_session(id) ON DELETE NO ACTION,
    app_user  uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    course    uuid NOT NULL REFERENCES course(id) ON DELETE NO ACTION,
    status    TEXT NOT NULL,
    marked_by uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    marked_at BIGINT NOT NULL,
    CONSTRAINT session_attendance_session_user PRIMARY KEY (session, app_user)
);

CREATE INDEX session_attendance_user ON session_attendance (app_user);
CREATE INDEX session_attendance_course ON session_attendance (course);

CREATE TABLE class_group (
    id                 uuid PRIMARY KEY,
    name               TEXT NOT NULL,
    grade              TEXT NULL,
    term               uuid NULL REFERENCES term(id) ON DELETE NO ACTION,
    creator            uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    teacher            uuid NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    class_member_count BIGINT NOT NULL DEFAULT 0,
    class_course_count BIGINT NOT NULL DEFAULT 0
);

CREATE INDEX class_group_grade ON class_group (grade);

-- A grade's course template. The grade label is the identity the whole API
-- speaks and it stays UNIQUE, but the row carries a surrogate uuid PK: it is
-- what class_course.source references, and a delete-and-recreate of the same
-- grade mints a new id, so links tagged by the old one were swept by that
-- delete instead of being adopted by the new blueprint (a TEXT grade tag
-- made the two indistinguishable).
CREATE TABLE class_blueprint (
    id      uuid PRIMARY KEY,
    grade   TEXT NOT NULL CONSTRAINT class_blueprint_grade_key UNIQUE,
    courses uuid[] NOT NULL DEFAULT '{}',
    creator uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION
);

CREATE TABLE class_member (
    class    uuid NOT NULL REFERENCES class_group(id) ON DELETE NO ACTION,
    app_user uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    added_by uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    -- When the student was put in; NULL on rows of unknown age.
    added_at BIGINT NULL,
    CONSTRAINT class_member_class_user PRIMARY KEY (class, app_user)
);

CREATE INDEX class_member_user ON class_member (app_user);

CREATE TABLE class_course (
    class       uuid NOT NULL REFERENCES class_group(id) ON DELETE NO ACTION,
    course      uuid NOT NULL REFERENCES course(id) ON DELETE NO ACTION,
    attached_by uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    attached_at BIGINT NULL,
    -- The blueprint that attached this course; NULL = attached by hand, which
    -- keeps a hand-attached course unreachable by every blueprint sweep.
    source      uuid NULL REFERENCES class_blueprint(id) ON DELETE NO ACTION,
    CONSTRAINT class_course_class_course PRIMARY KEY (class, course)
);

CREATE INDEX class_course_course ON class_course (course);

CREATE TABLE enrollment (
    course      uuid NOT NULL REFERENCES course(id) ON DELETE NO ACTION,
    app_user    uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    enrolled_by uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    created_at  BIGINT NOT NULL,
    -- The class that pumped this row; NULL when a human placed the student.
    source      uuid NULL REFERENCES class_group(id) ON DELETE NO ACTION,
    CONSTRAINT enrollment_course_user PRIMARY KEY (course, app_user)
);

CREATE INDEX enrollment_user ON enrollment (app_user);

CREATE TABLE event (
    id                 uuid PRIMARY KEY,
    creator            uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    title              TEXT NOT NULL,
    description        TEXT NOT NULL,
    -- The EventAudience enum flattened into columns (tag + per-variant data).
    audience_kind      TEXT NOT NULL
        CHECK (audience_kind IN ('school', 'role', 'course', 'class', 'registration')),
    audience_role      TEXT NULL,
    audience_course    uuid NULL REFERENCES course(id) ON DELETE NO ACTION,
    audience_class     uuid NULL REFERENCES class_group(id) ON DELETE NO ACTION,
    audience_capacity  BIGINT NULL,
    registration_count BIGINT NOT NULL DEFAULT 0,
    starts_at          BIGINT NULL,
    ends_at            BIGINT NULL
);

CREATE TABLE attendance (
    event     uuid NOT NULL REFERENCES event(id) ON DELETE NO ACTION,
    app_user  uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    status    TEXT NOT NULL,
    marked_by uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    marked_at BIGINT NOT NULL,
    CONSTRAINT attendance_event_user PRIMARY KEY (event, app_user)
);

CREATE INDEX attendance_user ON attendance (app_user);

CREATE TABLE registration (
    event         uuid NOT NULL REFERENCES event(id) ON DELETE NO ACTION,
    app_user      uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    registered_by uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    created_at    BIGINT NOT NULL,
    CONSTRAINT registration_event_user PRIMARY KEY (event, app_user)
);

CREATE INDEX registration_event ON registration (event);
