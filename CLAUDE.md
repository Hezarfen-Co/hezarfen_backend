# hezarfen_backend

Rust/axum school-management API (auth, roles, exams, notes, messaging,
courses, attendance, pomodoro, meals) on SurrealDB v3. Swagger UI at `/swagger`.

## Project docs — query the knowledge base, do NOT read README.md

README.md is ~355KB (~90k tokens). Its `##` sections live in an embedded
SurrealDB knowledge base. Fetch only what you need:

```bash
KB() { ~/.surrealdb/surreal sql -e "surrealkv://$HOME/.claude/kb/kb.db" \
      --ns claude --db hezarfen_backend --json --hide-welcome --log error; }
# list sections
echo 'SELECT id, title FROM doc;' | KB
# fetch one section body
echo 'SELECT body FROM doc:`time-policy`;' | KB
# full-text search, best sections first
echo "SELECT id, title, search::score(0) AS s FROM doc WHERE body @0@ 'retake' ORDER BY s DESC LIMIT 3;" | KB
# routes for ONE resource (auth, users, notes, messages, events, courses,
# subjects, sessions, exams, marks, work, pomodoro, attendance, settings, terms)
echo 'SELECT routes FROM endpoint:exams;' | KB
```

Staleness: if `sha256sum README.md` differs from
`echo 'SELECT sha256 FROM meta:readme;' | KB`, re-ingest:

```bash
python3 ~/.claude/kb/ingest.py README.md | KB
python3 ~/.claude/kb/ingest-endpoints.py README.md | KB
```

The project's own SurrealDB (container `hezarfen-surrealdb`, ns/db
`hezarfen`) is runtime data — never write knowledge-base rows there.

## Code map — orient here, skip discovery greps

Pattern: `src/domain/<x>.rs` = validated newtypes + entity + its own
persistence (SurrealValue). `src/web/<x>.rs` = DTOs + axum handlers, one file
per resource, routes wired in `lib.rs`.

- core: `main`(bootstrap) `lib`(build_router+OpenAPI) `config` `constant`(limits)
  `validate` `error` `database`(connect+SCHEMAFULL migration)
  `rate_limit`(fixed-window per-IP) `state`
- `domain/`: user role session timestamp profile preferences note note_file
  message parent_link event attendance registration course course_session
  session_attendance enrollment subject term settings work_entry pomodoro
  exam exam_attempt exam_question exam_answer exam_result question_image
  menu menu_dish dietary_profile meal_booking meal_attendance meal_ledger
- `ai/`: QUIC bridge to the out-of-process AI services (backend listens,
  services dial in). `protocol`(frames) `server`(AiBridge: listen+dispatch)
  `registry`(workers, capability routing) `tls` `error`. Off unless
  `AI_QUIC_ADDR` is set. Spec: README `## AI bridge (QUIC)`.
- `web/`: `extractor`(CurrentUser/RequireTeacher/Manager/Admin) `dto`(shared
  responses) `page`(pagination) `exam_ws`(exam-room WebSocket) + per-resource:
  auth users notes messages events courses subjects sessions exams marks work
  pomodoro attendance settings terms meals
- `tests/`: integration(oneshot+mem db) e2e(real TCP+cookies) rate_limit
  pagination persistence ai_bridge(real QUIC + fake AI service)
  ai_protocol(hab/1 wire contract; raw byte-level client, imports no protocol
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
