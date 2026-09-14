-- Control-database schema (Postgres).
--
-- DISJOINTNESS INVARIANT: the tables here (school, builder, builder_session,
-- rate_limit) and every table in migrations/school/*.sql together form ONE
-- union schema, applied to a single prepare database by
-- scripts/prepare_db.sh for sqlx's compile-time query macros. The two sets
-- must never collide on a table name; the prepare script fails loudly if they
-- do.
--
-- Translated from src/migration_sql.rs CONTROL_MIGRATION (SurrealDB), per the
-- plan's schema translation rules. Entity ids are app-minted UUID v7 (no DB
-- default); timestamps stay BIGINT unix-ms. All FKs are ON DELETE NO ACTION —
-- cascades stay explicit application transactions.
--
-- Renames (reserved words, never quoted): table `school` keeps its name. Its
-- Surreal record id WAS the slug; the Postgres table instead carries an
-- app-minted uuid PRIMARY KEY (`id`) so the slug is a renameable label, with
-- the old PK index name `school_slug` kept as the UNIQUE constraint on slug.
--
-- `rate_limit` exists ONLY here, not in the school schema: rate limiting is
-- billed against the control handle before any cookie names a school (see the
-- CONTROL_MIGRATION doc), and the school-side `rate_limit` DEFINE in the old
-- Surreal migration had no school-side reader. Its id is the derived
-- `tier:client:window` key, so the PK is TEXT.

CREATE TABLE school (
    -- Surrogate identity, app-minted uuid v7 (no DB default): the per-school
    -- database name is minted from it (see tenant::school_db_name), so a slug
    -- rename — once offered — can never orphan a database. Immutable keys
    -- belong to the uuid, never to the label.
    id         uuid PRIMARY KEY,
    slug       TEXT NOT NULL,
    name       TEXT NOT NULL,
    status     TEXT NOT NULL CONSTRAINT school_status CHECK (status IN ('active', 'suspended')),
    created_at BIGINT NOT NULL,
    CONSTRAINT school_slug UNIQUE (slug)
);

-- One row per product module the school has bought; names bound from
-- Module::ALL at write time, never spelled here. The pair is the key: the
-- membership is the link, and a read joins it back onto the school row.
CREATE TABLE school_module (
    school uuid NOT NULL REFERENCES school(id) ON DELETE NO ACTION,
    module TEXT NOT NULL,
    PRIMARY KEY (school, module)
);

CREATE TABLE builder (
    id            uuid PRIMARY KEY,
    username      TEXT NOT NULL,
    password_hash TEXT NOT NULL,
    created_at    BIGINT NOT NULL,
    CONSTRAINT builder_username UNIQUE (username)
);

CREATE TABLE builder_session (
    id         uuid PRIMARY KEY,
    builder    uuid NOT NULL REFERENCES builder(id) ON DELETE NO ACTION,
    token      TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL,
    CONSTRAINT builder_session_token UNIQUE (token)
);

CREATE INDEX builder_session_expires ON builder_session (expires_at);

-- One row per tier+client+window; the durable fold target of the in-memory
-- bucket (see src/rate_limit.rs).
CREATE TABLE rate_limit (
    id           TEXT PRIMARY KEY,
    hits         BIGINT NOT NULL DEFAULT 0,
    window_start BIGINT NOT NULL
);
