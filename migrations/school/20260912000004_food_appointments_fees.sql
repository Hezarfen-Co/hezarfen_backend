-- School-database schema, part 4: appointments, food program, fees, payments
-- (Postgres).
--
-- DISJOINTNESS INVARIANT: the tables in migrations/school/*.sql and the ones
-- in migrations/control/*.sql together form ONE union schema, applied to a
-- single prepare database by scripts/prepare_db.sh for sqlx's compile-time
-- query macros. The two sets must never collide on a table name.
--
-- Same translation rules as parts 1-3. New DDL with no Surreal source:
-- the btree_gist extension and the appointment_slot EXCLUDE constraint —
-- the publish-overlap guard that replaces the old APPOINTMENT_LOCK publish
-- path (SQLSTATE 23505 maps to the overlap 409 at the call site).
--
-- `menu` carries an app-minted uuid id plus the natural UNIQUE (date, slot):
-- its children (menu_dish, meal_booking, meal_attendance) reference it by a
-- single column, which a composite (date, slot) PK could not serve. The
-- publish upsert keys on the natural pair exactly as before.

CREATE EXTENSION IF NOT EXISTS btree_gist;

CREATE TABLE appointment_slot (
    id         uuid PRIMARY KEY,
    teacher    uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    starts_at  BIGINT NOT NULL,
    ends_at    BIGINT NOT NULL,
    occupied   BIGINT NOT NULL DEFAULT 0,
    note       TEXT NULL,
    -- Groups the occurrences a weekly repeat expanded into; NULL on a
    -- one-off slot.
    series     TEXT NULL,
    created_at BIGINT NOT NULL,
    -- Derived window for the exclusion constraint below.
    span       int8range GENERATED ALWAYS AS (int8range(starts_at, ends_at)) STORED,
    -- A teacher cannot publish two overlapping windows: the old
    -- APPOINTMENT_LOCK publish path, now a database guarantee (23505).
    CONSTRAINT appointment_slot_teacher_span EXCLUDE USING gist (teacher WITH =, span WITH &&)
);

CREATE INDEX appointment_slot_teacher_starts ON appointment_slot (teacher, starts_at);
CREATE INDEX appointment_slot_series ON appointment_slot (series);

CREATE TABLE appointment (
    id                 uuid PRIMARY KEY,
    slot               uuid NOT NULL REFERENCES appointment_slot(id) ON DELETE NO ACTION,
    requester          uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    status             TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'approved', 'rejected', 'cancelled')),
    reason             TEXT NOT NULL,
    -- A teacher's counter-proposal on the same row, until the requester
    -- accepts.
    proposed_starts_at BIGINT NULL,
    proposed_ends_at   BIGINT NULL,
    proposed_by        uuid NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    decided_by         uuid NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    cancelled_by       uuid NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    cancel_reason      TEXT NULL,
    reject_reason      TEXT NULL,
    created_at         BIGINT NOT NULL
);

CREATE INDEX appointment_slot_ref ON appointment (slot);
CREATE INDEX appointment_requester ON appointment (requester);
CREATE INDEX appointment_status ON appointment (status);

-- A published menu per day+slot. `date` is a calendar day as YYYY-MM-DD
-- text (an equality key, not a timestamp); `slot` is snapshotted text, so
-- retiring a slot in settings never rewrites a past menu.
CREATE TABLE menu (
    -- The {date}_{slot} derived key — it IS the URL segment — with the two
    -- components kept as their own columns (the unique pair keeps the old
    -- index name, which is what surfaces in SQLSTATE 23505).
    id           TEXT PRIMARY KEY,
    date         TEXT NOT NULL,
    slot         TEXT NOT NULL,
    capacity     BIGINT NULL,
    seats_booked BIGINT NOT NULL DEFAULT 0,
    -- The menu's revision: a booking claims its seat only while the menu
    -- still stands at the revision it read the price at. A stamp, not a
    -- counter: NULL reads as revision zero at the Rust boundary.
    version      BIGINT NULL,
    created_by   uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    created_at   BIGINT NOT NULL,
    CONSTRAINT menu_date_slot UNIQUE (date, slot)
);

CREATE TABLE menu_dish (
    id          uuid PRIMARY KEY,
    menu        TEXT NOT NULL REFERENCES menu(id) ON DELETE NO ACTION,
    name        TEXT NOT NULL,
    description TEXT NULL,
    -- Money is minor units (kuruş) as an integer, everywhere. Never decimal.
    price_minor BIGINT NOT NULL,
    tags        TEXT[] NOT NULL DEFAULT '{}',
    created_at  BIGINT NOT NULL
);

CREATE INDEX menu_dish_menu ON menu_dish (menu);

CREATE TABLE dietary_profile (
    student    uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    tags       TEXT[] NOT NULL DEFAULT '{}',
    note       TEXT NULL,
    updated_by uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    updated_at BIGINT NOT NULL,
    CONSTRAINT dietary_profile_student PRIMARY KEY (student)
);

-- A cancel flips `status` and stamps `cancelled_at`; the row stays so the
-- freed seat is still auditable against the ledger line it charged.
CREATE TABLE meal_booking (
    menu         TEXT NOT NULL REFERENCES menu(id) ON DELETE NO ACTION,
    student      uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    booked_by    uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    status       TEXT NOT NULL DEFAULT 'booked'
        CHECK (status IN ('booked', 'cancelled')),
    -- How many times this seat has been taken, and what it cost when the
    -- current attempt took it (NULL = the menu was free then).
    attempt      BIGINT NOT NULL DEFAULT 1,
    price_minor  BIGINT NULL,
    cancelled_at BIGINT NULL,
    created_at   BIGINT NOT NULL,
    -- One row per (menu, student); a re-booking re-uses the row and bumps
    -- `attempt`.
    CONSTRAINT meal_booking_menu_student PRIMARY KEY (menu, student)
);

CREATE INDEX meal_booking_menu ON meal_booking (menu);
CREATE INDEX meal_booking_student ON meal_booking (student);

CREATE TABLE meal_attendance (
    menu      TEXT NOT NULL REFERENCES menu(id) ON DELETE NO ACTION,
    student   uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    status    TEXT NOT NULL,
    marked_by uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    marked_at BIGINT NOT NULL,
    CONSTRAINT meal_attendance_menu_student PRIMARY KEY (menu, student)
);

-- APPEND-ONLY by design: a mistake is corrected with an opposing reversal
-- line, never by editing or deleting one. `source` is deliberately FK-less —
-- a charge points at the booking that caused it, a reversal at the line it
-- undoes (polymorphic, so no single REFERENCES target exists).
CREATE TABLE meal_ledger (
    -- The {menu}_{student}_c{attempt} derived key: billing idempotence by
    -- identity, not by scanning for an outstanding charge.
    id           TEXT PRIMARY KEY,
    student      uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    kind         TEXT NOT NULL CHECK (kind IN ('charge', 'credit', 'reversal')),
    amount_minor BIGINT NOT NULL,
    -- Polymorphic pointer (booking row, or the line it undoes) — TEXT
    -- because it can name a uuid-keyed or a derived-key row; no FK.
    source       TEXT NULL,
    method       TEXT NULL,
    note         TEXT NULL,
    recorded_by  uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    created_at   BIGINT NOT NULL
);

CREATE INDEX meal_ledger_student ON meal_ledger (student);

-- A school fee plan: a name and its installments (embedded JSONB — a charge
-- line copies the amount it was assigned at, so child rows would buy
-- nothing).
CREATE TABLE fee_plan (
    id               uuid PRIMARY KEY,
    name             TEXT NOT NULL,
    installments     JSONB NOT NULL,
    created_by       uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    created_at       BIGINT NOT NULL,
    assignment_count BIGINT NOT NULL DEFAULT 0
);

-- One plan on one student; the deterministic {plan}_{student} pair.
CREATE TABLE fee_plan_assignment (
    plan       uuid NOT NULL REFERENCES fee_plan(id) ON DELETE NO ACTION,
    student    uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    assigned_by uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    created_at BIGINT NOT NULL,
    CONSTRAINT fee_plan_assignment_plan PRIMARY KEY (plan, student)
);

CREATE INDEX fee_plan_assignment_student ON fee_plan_assignment (student);

-- School fees, APPEND-ONLY exactly like meal_ledger. `source` is FK-less for
-- the same polymorphic reason (charge -> assignment, credit -> charge,
-- refund -> credit, reversal -> line); `due_at` is set on charges alone.
CREATE TABLE payment_ledger (
    -- The {plan}_{student}_c{n} / {line}_r / {target}_k_{request_key}
    -- derived key: append-only rows addressed by identity.
    id           TEXT PRIMARY KEY,
    student      uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    kind         TEXT NOT NULL CHECK (kind IN ('charge', 'credit', 'reversal', 'refund')),
    amount_minor BIGINT NOT NULL,
    -- Polymorphic pointer (assignment, charge, credit, or line it undoes);
    -- TEXT for the same reason as meal_ledger.source. No FK.
    source       TEXT NULL,
    due_at       BIGINT NULL,
    method       TEXT NULL,
    note         TEXT NULL,
    recorded_by  uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    created_at   BIGINT NOT NULL
);

CREATE INDEX payment_ledger_student ON payment_ledger (student);
CREATE INDEX payment_ledger_source ON payment_ledger (source);
