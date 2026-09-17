# hezarfen_backend

Rust/axum school-management API (auth, roles, exams, notes, messaging,
courses, attendance, pomodoro, meals) on PostgreSQL (sqlx). Swagger UI at `/swagger`.

## Project docs — README.md, read sections not the whole file

README.md is ~355KB (~90k tokens). Never read it whole: grep for the `## `
heading you need, then read that range. Its `## Endpoints` table is GENERATED
(see Conventions); prose sections are hand-written.

## Code map — orient here, skip discovery greps

Pattern: `src/domain/<x>.rs` = validated newtypes + entity. `src/db/<x>.rs` =
its SQL (the only layer that executes queries). `src/service/<x>.rs` =
workflows (one `tx_with_retry` transaction per invariant). `src/web/<x>.rs` =
DTOs + axum handlers, one file per resource, routes wired in `lib.rs`.

- core: `main`(bootstrap) `lib`(build_router+OpenAPI) `config` `constant`(limits)
  `validate` `error` `database`(PgPool + tx_with_retry + the two sqlx migrator sets)
  `rate_limit`(fixed-window per-IP) `state`
- `domain/`: user role session timestamp profile preferences note note_file
  message parent_link event attendance registration course course_session
  session_attendance enrollment subject term settings work_entry pomodoro
  exam exam_attempt exam_question exam_answer exam_result question_image
  menu menu_dish dietary_profile meal_booking meal_attendance meal_ledger
- `db/`: per-resource SQL; `cap`(count-cap CTE recipes + verdict types)
  `field_update` `page`(PagedList)
- `service/`: per-resource workflows (booking, minting, cascades)
- `ai/`: QUIC bridge to the out-of-process AI services (backend listens,
  services dial in). `protocol`(frames) `server`(AiBridge: listen+dispatch)
  `registry`(workers, capability routing) `tls` `error`. Off unless
  `AI_QUIC_ADDR` is set. Spec: README `## AI bridge (QUIC)`.
- `web/`: `extractor`(CurrentUser/RequireTeacher/Manager/Admin) `dto`(shared
  responses) `page`(pagination) `exam_ws`(exam-room WebSocket) + per-resource:
  auth users notes messages events courses subjects sessions exams marks work
  pomodoro attendance settings terms meals
- `tests/`: integration(oneshot over per-test Postgres databases — tests/common
  mints `init_test_tenants`/`init_test_db`/`deployment_with`, template-clone)
  e2e(real TCP+cookies) rate_limit
  pagination persistence(idempotent re-migration over a live db) ai_bridge(real QUIC + fake AI service)
  ai_protocol(hab/2 wire contract; raw byte-level client, imports no protocol
  types — the only suite that catches a wire-format change)

Note: README `## Layout` lags src/ (missing newer domain files); this map +
live `ls src/` win on conflict.

## Token discipline

- Open-ended "where/how does X work" sweeps: use the Explore subagent — keep
  exploration out of the main context.
- Quiet commands: `cargo test --quiet 2>&1 | tail -30`,
  `cargo build 2>&1 | tail -15`.
- Default for a small change: the scoped suite, `cargo test --test <file>`
  (or `cargo nextest run --test <file>`).
- Full suite: `cargo nextest run` — runs the ~21 test binaries
  process-per-test in parallel instead of binary-serial. This repo has zero
  doctests, so nothing is lost vs `cargo test`, which still works as a
  fallback.

## Conventions

- No `PUT`. Routes use `GET`/`POST`/`DELETE`/`PATCH` only.
- API contract changes move the utoipa annotations and the tag descriptions
  in `lib.rs` by hand. README's `## Endpoints` table is GENERATED — regen with
  `UPDATE_DOCS=1 cargo test --test doc_sync`; plain `cargo test --test
  doc_sync` fails on drift, so the suite gates it. README prose sections stay
  hand-written.
- Pre-customer, so clean breaks are fine — but any validation change needs
  its stale-data impact analyzed first: existing rows may already violate the
  new rule.
- Commits are emoji + conventional type. Vocabulary in use: `feat`, `fix`,
  `docs`, `test`, `style`, `chore`. Never `refactor` (unused in this repo's
  history). `🎨 style` is reserved for `cargo fmt` commits.
- Code, comments and commit messages are English. Turkish survives only where
  it is the data under test (test fixtures, fold tables) or user-visible
  content (product defaults, OpenAPI examples and descriptions).
