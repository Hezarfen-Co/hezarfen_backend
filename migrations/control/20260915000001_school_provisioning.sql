-- A school is `provisioning` from the instant its registry row lands until its
-- database exists and carries the school schema.
--
-- The row commits before the database does — `CREATE DATABASE` cannot join the
-- transaction that claims the slug, so the two are never one write — and a boot
-- that dies in between used to leave a school the API advertised as ready while
-- its database had no tables at all (every request against it a 500, and
-- nothing able to tell "half-made" from "shipped"). With the third state, the
-- row says which it is, the boot finishes what the previous one started (see
-- `Tenants::reconcile_provisioning`), and the school's own doors refuse it
-- until then.
--
-- `active` and `suspended` remain the only two a client may ask for: the
-- PATCH body's parser still refuses anything else (see `SchoolStatus::try_from_str`).
ALTER TABLE school DROP CONSTRAINT school_status;
ALTER TABLE school
    ADD CONSTRAINT school_status CHECK (status IN ('active', 'suspended', 'provisioning'));