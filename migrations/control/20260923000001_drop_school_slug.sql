-- Cookie prefix, builder routes and blob directories cut over to the school
-- uuid in the same deploy as this column drop. Existing session cookies
-- cannot resolve after this migration; users log in again.
ALTER TABLE school DROP CONSTRAINT school_slug, DROP COLUMN slug;
