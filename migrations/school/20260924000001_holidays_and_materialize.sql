-- Holiday calendar + weekly-plan materializer support.
--
-- `holiday` is the school-wide non-teaching calendar: named instant ranges
-- (the same shape `term`, `event`, `exam` and `academic_year` already use —
-- four of five dated resources are instants; no day type is introduced).
-- The weekly-plan materializer reads it to skip non-teaching days.
--
-- `offering_slot` / `class_course_slot` gain an optional `topic`: without it
-- every generated lesson of a section shares one topic, which is useless for
-- attendance and homework.
--
-- `course_session (class_course, starts_at)` becomes UNIQUE: two lessons of
-- one section cannot start at the same instant. This is what makes a
-- concurrent materialize idempotent (`ON CONFLICT DO NOTHING`), and it turns
-- the duplicate-start instant from a silent twin lesson into a conflict the
-- single-session create route answers with a coded 409. The deployed demo
-- school held no duplicate pair when this landed.
--
-- No tenant column: tenancy is one Postgres database per school.

CREATE TABLE holiday (
    id         uuid PRIMARY KEY,
    name       TEXT NOT NULL,
    starts_at  BIGINT NOT NULL,
    ends_at    BIGINT NOT NULL,
    kind       TEXT NOT NULL CHECK (kind IN ('resmi', 'dini', 'idari', 'ara')),
    creator    uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    created_at BIGINT NOT NULL
);
CREATE INDEX holiday_starts_at ON holiday (starts_at);

ALTER TABLE offering_slot     ADD COLUMN topic TEXT NULL;
ALTER TABLE class_course_slot ADD COLUMN topic TEXT NULL;

CREATE UNIQUE INDEX course_session_class_course_starts_at
    ON course_session (class_course, starts_at);
