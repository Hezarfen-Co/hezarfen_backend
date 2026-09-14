-- Person identity (control database).
--
-- DISJOINTNESS INVARIANT: the tables here (person, person_school,
-- person_session) and every other control table together with all of
-- migrations/school/*.sql form ONE union schema, applied to a single prepare
-- database by scripts/prepare_db.sh for sqlx's compile-time query macros.
-- The sets must never collide on a table name; the prepare script fails
-- loudly if they do.
--
-- A person is the global account behind a school's `app_user` rows: one
-- username + password in the control plane, any number of school
-- memberships. Login verifies `person.password_hash` only; the school-side
-- copy stays written (same hash) so school queries keep compiling. Timestamps
-- stay BIGINT unix-ms; all FKs ON DELETE NO ACTION — cascades stay explicit
-- application transactions (Tenants::drop deletes person_school before the
-- school row).

CREATE TABLE person (
    id            uuid PRIMARY KEY,
    username      TEXT NOT NULL,
    password_hash TEXT NOT NULL,
    created_at    BIGINT NOT NULL,
    -- Globally unique: the same username in two school databases is one and
    -- the same person, copied into each school's `app_user`.
    CONSTRAINT person_username UNIQUE (username)
);

CREATE TABLE person_school (
    person uuid NOT NULL REFERENCES person(id) ON DELETE NO ACTION,
    school TEXT NOT NULL REFERENCES school(slug) ON DELETE NO ACTION,
    created_at BIGINT NOT NULL,
    CONSTRAINT person_school_person_school PRIMARY KEY (person, school)
);

CREATE TABLE person_session (
    id         uuid PRIMARY KEY,
    person     uuid NOT NULL REFERENCES person(id) ON DELETE NO ACTION,
    token      TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL,
    CONSTRAINT person_session_token UNIQUE (token)
);

CREATE INDEX person_session_expires ON person_session (expires_at);

CREATE INDEX person_session_person ON person_session (person);
