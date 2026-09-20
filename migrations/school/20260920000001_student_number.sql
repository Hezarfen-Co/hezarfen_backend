-- The student number: the school's own identifier for a student ("1234",
-- "9-B/17"), carried on the account row.
--
-- WHERE THIS FILE BELONGS: `migrations/school/` in the backend repo. APPEND-ONLY
-- relative to the existing school migrations: it touches no other table.
--
-- WHY ON `app_user`: the number belongs to the *account in this school*, not to
-- the global person — two schools may issue the same number to two different
-- students, and a person's membership in one school must not constrain another.
-- `NULL` on every account that holds none, which is every staff account and
-- every student registered before the school got round to issuing numbers.
--
-- UNIQUENESS IS PER SCHOOL BY CONSTRUCTION: each school already lives in its own
-- database, so a plain partial unique index is the whole rule — no school column,
-- exactly like `app_user_username` and `app_user_person`. The partial predicate
-- (`WHERE student_number IS NOT NULL`) is what keeps unlimited accounts without
-- a number legal: in Postgres, NULLs in a plain UNIQUE index would be distinct
-- anyway, but the partial index also keeps the index off the rows that carry no
-- value.
--
-- Only a student row may hold one. That rule has three enforcement points and
-- they are not redundant: the web layer refuses the request (a 400 with the
-- live role it read), the role cascade clears the column in the same statement
-- that leaves the student role, and the CHECK below is what holds when the two
-- race — a profile write carrying a number from a snapshot read while the role
-- write commits first would otherwise land a number on a promoted row, and the
-- handler-side gate cannot see that. A `CHECK` may reference other columns of
-- its own row (unlike a `UNIQUE`), so the row's own role is exactly the
-- predicate; the column is new and NULL on every existing row, so no row can
-- be in violation at creation time. A violation answers the same 400 the
-- web gate does (`crate::db::user` maps 23514 to it), never a 500.
ALTER TABLE app_user ADD COLUMN IF NOT EXISTS student_number TEXT NULL;

ALTER TABLE app_user
    ADD CONSTRAINT app_user_student_number_student
    CHECK (student_number IS NULL OR role = 'student');

CREATE UNIQUE INDEX IF NOT EXISTS app_user_student_number
    ON app_user (student_number) WHERE student_number IS NOT NULL;
