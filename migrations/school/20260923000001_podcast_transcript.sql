-- The episode transcript, as the service reported it on `podcast.report`:
-- one TEXT blob, chapters already joined with blank lines.
--
-- WHERE THIS FILE BELONGS: `migrations/school/` in the backend repo. APPEND-ONLY
-- relative to the existing school migrations: one new nullable column, no
-- backfill. A job finished before this column existed stays NULL, and a report
-- that omits the field (a service that has not learned it yet) leaves it NULL.
-- The report statement fills it once (`COALESCE`), the same way `format` is
-- filled, and never rewrites a value that already landed.
ALTER TABLE podcast_job ADD COLUMN transcript TEXT NULL;
