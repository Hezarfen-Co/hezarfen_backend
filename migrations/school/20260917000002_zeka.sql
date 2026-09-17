-- ZEKA's output tables, in the school database (Postgres).
--
-- WHERE THIS FILE BELONGS: `migrations/school/` in the backend repo, applied
-- by `Migrator::new(migrations/school)` to the template at boot and to every
-- school database at create. It is APPEND-ONLY relative to the four existing
-- school migrations: it creates new tables and touches none of them.
--
-- SCOPE RULE, unchanged: ZEKA **never writes** to a pre-existing table. It
-- reads school data through the bridge's `ApiRequest` allowlist only, and
-- writes nothing but the `zeka_*` tables below. The FKs point outward, one
-- direction only.
--
-- NO `school` COLUMN: each school already lives in its own database, so the
-- tenant IS the database. The SurrealDB version carried a `school` slug on
-- every row because ZEKA then had a database of its own; that column is gone
-- and every composite key drops its school segment with it.
--
-- Translated from service/schema/zeka.surql, following the same rules the
-- backend's own translation used:
--   * record links   -> uuid FKs, ALL `ON DELETE NO ACTION` (cascades stay
--                       explicit application transactions)
--   * `{a}_{b}` text record keys -> natural composite PRIMARY KEYs
--   * timestamps     -> BIGINT unix-ms UTC, never `timestamptz`
--   * `option<object> FLEXIBLE` -> JSONB, and only where the shape belongs to
--                       the service rather than to this schema (the same line
--                       `rag_output.payload` and `exam_question.choices` draw)
--   * `option<array<...>>` -> a child table with an `ord` column, never a
--                       Postgres array: the backend replaced every `TEXT[]`
--                       and `uuid[]` it had (course_teacher, menu_dish_tag,
--                       blueprint_course, ...), and an array here would be the
--                       one exception in the schema.
--   * entity ids are app-minted uuid v7 (`domain::monotonic_id::next_uuid`),
--     never a DB default — the one id convention in the codebase.
--
-- CHECK constraints carry only the enums ZEKA itself closes (confidence tier,
-- audience role, run status, segment dimension + its labels). `product` and
-- `rule_id` carry NO CHECK: rules are added and versioned far more often than
-- the schema, and a DDL CHECK could not survive that — the same reasoning that
-- left exam kinds and attendance statuses unconstrained.


-- ---------------------------------------------------------------------------
-- 1) zeka_student_summary — one nightly row per student
--
-- PK is the student alone (the `{school}_{user}` key minus its school half),
-- which makes the nightly write an idempotent UPSERT and needs no extra
-- unique index. Same shape as `dietary_profile`, keyed on `app_user`.
-- ---------------------------------------------------------------------------
CREATE TABLE zeka_student_summary (
    student      uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    -- The compute modules' output objects. JSONB because the shape belongs to
    -- the service: it changes with a rule version, and this schema should not
    -- have to move every time it does.
    marks        JSONB NULL,
    attendance   JSONB NULL,
    submission   JSONB NULL,
    study        JSONB NULL,
    -- 'none' | 'exploratory' | 'stable' — inherited from the weakest input.
    confidence   TEXT NOT NULL
        CHECK (confidence IN ('none', 'exploratory', 'stable')),
    computed_at  BIGINT NOT NULL,
    -- Swept after this instant: derived student profile, "term + 1 year".
    retain_until BIGINT NOT NULL,
    CONSTRAINT zeka_student_summary_student PRIMARY KEY (student)
);

CREATE INDEX zeka_student_summary_retain ON zeka_student_summary (retain_until);

-- The attention list, one row per trigger (was an embedded array).
--
-- There is deliberately NO score column and no severity: a single risk number
-- would rank children, and the literature review found no validated causal
-- evidence for that product. `ord` keeps the writer's order, which is
-- alphabetical by trigger and NOT by severity — see `attention.order_items`.
CREATE TABLE zeka_attention_item (
    student  uuid NOT NULL REFERENCES zeka_student_summary(student) ON DELETE NO ACTION,
    -- Which trigger fired: 'attendance' | 'homework' | 'mark_trend'.
    -- No CHECK: a fourth trigger is a rule change, not a schema change.
    trigger  TEXT NOT NULL,
    -- The course the trigger is about, when it has one. NULL = school-wide.
    -- Only the mark-trend trigger is per-course today.
    course   uuid NULL REFERENCES course(id) ON DELETE NO ACTION,
    -- THE SENTENCE THE TEACHER READS. A statement of fact, not a judgement:
    -- "4 of the last 30 days' homework went unsubmitted", never "at risk".
    -- It is written here rather than rebuilt by the reader, so that what was
    -- shown and what was computed can never drift apart.
    fact     TEXT NOT NULL,
    -- The window the fact is about. Kept as columns, not inside `evidence`,
    -- because "is this item about the last 30 days or the last 7" is a
    -- question the reader asks of every item.
    window_from BIGINT NOT NULL,
    window_to   BIGINT NOT NULL,
    -- The numbers behind the item. Never empty: the store
    -- refuses an item without evidence, because an item nobody can explain
    -- cannot be shown to a teacher.
    evidence JSONB NOT NULL,
    ord      SMALLINT NOT NULL,
    CONSTRAINT zeka_attention_item_student_trigger_course
        UNIQUE NULLS NOT DISTINCT (student, trigger, course)
);

CREATE INDEX zeka_attention_item_student ON zeka_attention_item (student);
CREATE INDEX zeka_attention_item_trigger ON zeka_attention_item (trigger);


-- ---------------------------------------------------------------------------
-- 2) zeka_recommendation — generated advice
--
-- Every row carries a reason. Advice that cannot be explained is not shown,
-- so `evidence` is NOT NULL and the store refuses an empty object.
--
-- The old record key was `{school}_{audience}_{product}_{rule_id}[_{scope}]`;
-- minus the school half that is exactly the composite PK below. `scope` is
-- the optional last segment (a course id, usually), and NULLS NOT DISTINCT is
-- what makes "no scope" count as a taken slot rather than an unlimited one —
-- the same device `bank_question_image` uses for its NULL slot.
-- ---------------------------------------------------------------------------
CREATE TABLE zeka_recommendation (
    -- A surrogate id, because the natural key below has a nullable column and
    -- a PRIMARY KEY cannot: `NULLS NOT DISTINCT` is a UNIQUE-only device. It
    -- earns its keep anyway — dismissing a card is a write against one row,
    -- and a client should not have to name four columns to do it.
    id             uuid PRIMARY KEY,
    -- Who sees it.
    audience       uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    -- Which product family: 'O1' | 'O2' | 'O3' | 'O4' | 'T3' | 'T4' today.
    product        TEXT NOT NULL,
    -- Which rule, and which version of it, produced this. A rule may not
    -- change what it says without changing this number.
    rule_id        TEXT NOT NULL,
    rule_version   BIGINT NOT NULL,
    -- The rule's optional scope segment; NULL when the rule has none. TEXT,
    -- not uuid: a segment rule scopes itself by label ("bilissel_talep=analiz",
    -- "<class>_okuma_yuku=yuksek"), not by a row it could point at.
    scope          TEXT NULL,
    -- Who it is about. NULL on a student's own card (audience = about would
    -- be noise); set on every teacher-facing row.
    about          uuid NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    -- The role gate. T4 items are 'teacher' only — never shown to the student
    -- or the parent.
    audience_role  TEXT NOT NULL
        CHECK (audience_role IN ('student', 'teacher', 'parent', 'manager')),
    course         uuid NULL REFERENCES course(id) ON DELETE NO ACTION,
    -- Which numbers, which window. Feeds the "why?" panel.
    evidence       JSONB NOT NULL,
    confidence     TEXT NOT NULL
        CHECK (confidence IN ('none', 'exploratory', 'stable')),
    created_at     BIGINT NOT NULL,
    expires_at     BIGINT NOT NULL,
    -- `expires_at` or 90 days, whichever comes first. The sweep reads this.
    retain_until   BIGINT NOT NULL,
    -- The human-override trail (KVKK art. 11): dismissed, disputed, not
    -- useful. The nightly write MERGES; it must never clear these three.
    dismissed_at   BIGINT NULL,
    dismissed_by   uuid NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    dismiss_reason TEXT NULL,
    -- The idempotency key: the same rule is never duplicated for the same
    -- person, so the nightly write is an UPSERT on this.
    --
    -- `about` IS PART OF THE KEY, and that is not cosmetic. A teacher-facing
    -- rule fires once per student: thirty students under one teacher, one
    -- rule, one course. Without `about` all thirty share a key, the UPSERT
    -- keeps the last, and the teacher is shown one student while twenty-nine
    -- vanish without an error anywhere. (The SurrealDB version keyed on
    -- `scope or course or about` and lost exactly that way.)
    CONSTRAINT zeka_recommendation_key
        UNIQUE NULLS NOT DISTINCT (audience, product, rule_id, about, scope)
);

CREATE INDEX zeka_recommendation_audience ON zeka_recommendation (audience, created_at);
CREATE INDEX zeka_recommendation_about ON zeka_recommendation (about, product);
-- Rule calibration: a rule dismissed as wrong over and over IS wrong. This
-- index is what makes that measurable without a full scan.
CREATE INDEX zeka_recommendation_rule ON zeka_recommendation (rule_id, created_at);
CREATE INDEX zeka_recommendation_retain ON zeka_recommendation (retain_until);


-- ---------------------------------------------------------------------------
-- 3) zeka_run — the nightly job's ledger
--
-- Partial failure and budget accounting rest on this. The key was
-- `{school}_{YYYY-MM-DD}`; minus the school half it is the day itself, so a
-- re-run of the same night overwrites rather than duplicating. TEXT PK, the
-- same choice `settings.id` makes.
-- ---------------------------------------------------------------------------
CREATE TABLE zeka_run (
    run_day          TEXT NOT NULL
        CHECK (run_day ~ '^[0-9]{4}-[0-9]{2}-[0-9]{2}$'),
    started_at       BIGINT NOT NULL,
    finished_at      BIGINT NULL,
    status           TEXT NOT NULL
        CHECK (status IN ('running', 'ok', 'partial', 'failed', 'skipped')),
    duration_ms      BIGINT NULL,
    students_total   BIGINT NOT NULL DEFAULT 0,
    students_ok      BIGINT NOT NULL DEFAULT 0,
    students_failed  BIGINT NOT NULL DEFAULT 0,
    students_skipped BIGINT NOT NULL DEFAULT 0,
    rows_written     BIGINT NOT NULL DEFAULT 0,
    budget_exceeded  BOOLEAN NOT NULL DEFAULT false,
    budget_ms        BIGINT NOT NULL,
    retain_until     BIGINT NOT NULL,
    CONSTRAINT zeka_run_day PRIMARY KEY (run_day)
);

CREATE INDEX zeka_run_started ON zeka_run (started_at);
CREATE INDEX zeka_run_retain ON zeka_run (retain_until);

-- Students the budget ran out on, one row each (was an embedded array). The
-- next run starts here, which is the only reason the list is kept.
CREATE TABLE zeka_run_pending (
    run     TEXT NOT NULL REFERENCES zeka_run(run_day) ON DELETE NO ACTION,
    student uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    ord     INTEGER NOT NULL,
    CONSTRAINT zeka_run_pending_run_student PRIMARY KEY (run, student)
);

-- Modules that failed, one row each (was an embedded array). A partial list
-- is what lets the UI mark a section "missing" instead of showing a hole.
CREATE TABLE zeka_run_failed_module (
    run    TEXT NOT NULL REFERENCES zeka_run(run_day) ON DELETE NO ACTION,
    module TEXT NOT NULL,
    ord    SMALLINT NOT NULL,
    CONSTRAINT zeka_run_failed_module_run_module PRIMARY KEY (run, module)
);


-- ---------------------------------------------------------------------------
-- 4) zeka_question_segment — one cognitive label set per exam question
--
-- This is a question's LABEL, not its STATISTIC. p-value and discrimination
-- need raw answers, and the bridge allowlist carries no exam path, so item
-- analysis still cannot be built. Everything here comes from the question's
-- own text.
--
-- NOT PERSONAL DATA: a row is about a question and reaches no student. It
-- still carries a finite `retain_until`, because a label is the output of one
-- model and one prompt version and goes stale when either moves — and an
-- infinite retention would need an exception in the sweep.
--
-- The composite FK below is the device `exam_answer` uses: naming both the
-- exam and the question means the two can never disagree about the owner.
-- ---------------------------------------------------------------------------
CREATE TABLE zeka_question_segment (
    question       uuid NOT NULL,
    exam           uuid NOT NULL,
    -- Denormalized from `subject.course` so the teacher's
    -- course x cognitive-demand screen needs no join. ZEKA resolves it at
    -- write time; nothing else may set it.
    course         uuid NOT NULL REFERENCES course(id) ON DELETE NO ACTION,
    subject        uuid NOT NULL REFERENCES subject(id) ON DELETE NO ACTION,

    -- --- The dimensions. Label sets match src/segment/rubric.py exactly. ---
    bilissel_talep TEXT NOT NULL
        CHECK (bilissel_talep IN ('hatirlama', 'uygulama', 'analiz')),
    dikkat_tuzagi  TEXT NOT NULL
        CHECK (dikkat_tuzagi IN ('var', 'yok')),
    okuma_yuku     TEXT NOT NULL
        CHECK (okuma_yuku IN ('dusuk', 'yuksek')),
    -- EXPERIMENTAL. Measured 0.607 stability on a hand-written probe; it
    -- scores far higher on our own seed only because the generator writes
    -- multi-step-ness into the text, which makes that an exam we set
    -- ourselves. Labelled and stored, never read downstream.
    adim_sayisi    TEXT NOT NULL
        CHECK (adim_sayisi IN ('tek_adim', 'cok_adim')),

    -- Per-dimension model confidence in [0,1]. One total is not enough: a
    -- question's reading load can be obvious while its cognitive demand is
    -- genuinely ambiguous.
    confidence_bilissel_talep DOUBLE PRECISION NOT NULL
        CHECK (confidence_bilissel_talep BETWEEN 0 AND 1),
    confidence_dikkat_tuzagi  DOUBLE PRECISION NOT NULL
        CHECK (confidence_dikkat_tuzagi BETWEEN 0 AND 1),
    confidence_okuma_yuku     DOUBLE PRECISION NOT NULL
        CHECK (confidence_okuma_yuku BETWEEN 0 AND 1),
    confidence_adim_sayisi    DOUBLE PRECISION NOT NULL
        CHECK (confidence_adim_sayisi BETWEEN 0 AND 1),
    -- Row-level tier, same vocabulary as every other table here. Derived from
    -- the LOWEST confidence among the PRODUCTION dimensions; the experimental
    -- one does not enter it.
    confidence     TEXT NOT NULL
        CHECK (confidence IN ('none', 'exploratory', 'stable')),

    -- The distractor the trap points at (an id from `exam_question.choices`),
    -- when `dikkat_tuzagi = 'var'`. No FK: choices live inside a JSONB column.
    trap_choice    TEXT NULL,
    -- The model's own reasoning, two sentences at most. NOT SHOWN TO THE
    -- STUDENT — see docs/CIKTI-SOZLESMESI.md.
    rationale      TEXT NOT NULL,

    -- Provenance. A silent model or prompt change is forbidden: when either
    -- moves, the row moves with it.
    model          TEXT NOT NULL,
    prompt_version TEXT NOT NULL,
    variant        TEXT NULL,

    computed_at    BIGINT NOT NULL,
    retain_until   BIGINT NOT NULL,

    CONSTRAINT zeka_question_segment_question PRIMARY KEY (question),
    CONSTRAINT zeka_question_segment_exam_question_fkey FOREIGN KEY (exam, question)
        REFERENCES exam_question (exam, id) ON DELETE NO ACTION
);

CREATE INDEX zeka_question_segment_exam ON zeka_question_segment (exam);
CREATE INDEX zeka_question_segment_subject ON zeka_question_segment (subject);
CREATE INDEX zeka_question_segment_talep
    ON zeka_question_segment (course, bilissel_talep);
CREATE INDEX zeka_question_segment_retain ON zeka_question_segment (retain_until);

-- Which dimensions downstream actually uses, and which are experimental (was
-- two embedded arrays). Kept per row on purpose: when `rubric.py` demotes a
-- dimension, that a downstream product no longer reads it is legible from the
-- row itself, with no document to consult.
CREATE TABLE zeka_question_segment_dimension (
    question  uuid NOT NULL
        REFERENCES zeka_question_segment(question) ON DELETE NO ACTION,
    dimension TEXT NOT NULL
        CHECK (dimension IN ('bilissel_talep', 'dikkat_tuzagi',
                             'okuma_yuku', 'adim_sayisi')),
    role      TEXT NOT NULL CHECK (role IN ('downstream', 'experimental')),
    ord       SMALLINT NOT NULL,
    CONSTRAINT zeka_question_segment_dimension_question_dimension
        PRIMARY KEY (question, dimension)
);


-- ---------------------------------------------------------------------------
-- 5) zeka_student_segment_profile — student x dimension x label
--
-- `contrast` IS WHY THIS TABLE EXISTS.
--
-- Raw `accuracy` is not a discriminating measure: a student's hit rate inside
-- any segment mostly reflects their GENERAL ability. A strong student scores
-- high in every segment and a weak one low in every segment, so "this student
-- struggles with analysis questions" DOES NOT FOLLOW from it.
--     contrast = accuracy - overall_accuracy
-- The general level cancels in that difference and what remains is the part
-- specific to the segment. Recommendation rules fire on `contrast` ONLY,
-- never on `accuracy`.
--
-- PERSONAL DATA: derived student profile — "term + 1 year", and deleted on
-- graduation (`Store.purge_departed`).
-- ---------------------------------------------------------------------------
CREATE TABLE zeka_student_segment_profile (
    student   uuid NOT NULL REFERENCES app_user(id) ON DELETE NO ACTION,
    -- PRODUCTION dimensions only. The experimental one is labelled in
    -- `zeka_question_segment` and stays there; it never reaches a student.
    dimension TEXT NOT NULL
        CHECK (dimension IN ('bilissel_talep', 'dikkat_tuzagi', 'okuma_yuku')),
    label     TEXT NOT NULL,

    n_answers BIGINT NOT NULL,
    n_correct BIGINT NOT NULL,
    -- The segment hit rate. NOT SHOWN ALONE and NEVER FIRES A RULE ALONE: it
    -- carries general ability and has no discriminating power.
    accuracy  DOUBLE PRECISION NOT NULL CHECK (accuracy BETWEEN 0 AND 1),

    -- The student's hit rate across ALL labelled items — the subtrahend.
    -- Stored on the row so the evidence panel can show the comparison.
    overall_n_answers BIGINT NOT NULL,
    overall_accuracy  DOUBLE PRECISION NOT NULL
        CHECK (overall_accuracy BETWEEN 0 AND 1),
    -- The honest measure. Negative = behind their OWN general level here.
    contrast  DOUBLE PRECISION NOT NULL CHECK (contrast BETWEEN -1 AND 1),

    confidence TEXT NOT NULL
        CHECK (confidence IN ('none', 'exploratory', 'stable')),

    computed_at  BIGINT NOT NULL,
    retain_until BIGINT NOT NULL,

    CONSTRAINT zeka_student_segment_profile_key
        PRIMARY KEY (student, dimension, label),
    -- n_correct can never exceed n_answers; a row that says otherwise is a
    -- counting bug, and the cheapest place to catch it is here.
    CONSTRAINT zeka_student_segment_profile_counts
        CHECK (n_correct <= n_answers AND n_answers <= overall_n_answers)
);

CREATE INDEX zeka_student_segment_profile_dimension
    ON zeka_student_segment_profile (dimension, label);
CREATE INDEX zeka_student_segment_profile_retain
    ON zeka_student_segment_profile (retain_until);
