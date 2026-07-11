# hezarfen_backend

Note, attendance + exam backend. **Rust (edition 2024) · axum · SurrealDB 3 (embedded surrealkv) · tokio.**

Session-cookie auth with four hierarchical roles (`student < teacher < manager
< admin`). Notes are per-user. Attendance is event + attendees: create an event,
then mark users present / absent / late / excused.

Every field is a validated newtype (`Username(String)`, `NoteTitle(String)`, …)
constructed only after its restrictions pass — invalid input can't be
represented. Those same types derive `surrealdb::types::SurrealValue`, so one
typed value flows from HTTP request into the `SCHEMAFULL` database. Record ids
are ULIDs (time-sortable).

## Run

```sh
cp .env.example .env      # optional
cargo run
```

Boots on `http://127.0.0.1:8080`, storing data in `./data/hezarfen.db`
(embedded surrealkv — no external DB server). Interactive API docs (Swagger UI)
are served at `/swagger`, the raw OpenAPI spec at `/api-docs/openapi.json`.

## Run in a container (podman)

```sh
podman compose up -d --build   # build + start, http://127.0.0.1:8080
podman compose logs -f backend
podman compose down            # stop (data survives in the volume)
```

The `Containerfile` is a two-stage build (Rust builder with cargo cache
mounts, `debian:trixie-slim` runtime, non-root user). Since the database is
embedded there is only one service; its data lives in the named volume
`hezarfen-data`, mounted at `/data`. `HOST` is forced to `0.0.0.0` inside the
container so the published port works. Production knobs (`COOKIE_SECURE`,
`CORS_ALLOWED_ORIGINS`, rate limits, `TRUST_PROXY`) are commented in
`compose.yaml` — uncomment as needed. Works with `docker compose` too.

Without compose:

```sh
podman build -t hezarfen-backend .
podman run -d --name hezarfen -p 8080:8080 -v hezarfen-data:/data hezarfen-backend
```

## Auth model

Login sets an `HttpOnly`, `SameSite=Lax` `session` cookie (7-day expiry, stored
server-side). Send it back on later requests. Every endpoint below whose `Auth`
column names a role requires a valid session; the ones marked `no` (`/health`,
the docs pages, `register` / `login` / `logout`) don't (`logout` is idempotent —
it clears the session if one is present). Set `COOKIE_SECURE=true` when serving
behind TLS to add the cookie's `Secure` attribute.

## Rate limiting

Requests are limited per client IP over a fixed 60-second window, in two tiers:
`/auth/login` + `/auth/register` get a strict budget
(`RATE_LIMIT_AUTH_PER_MINUTE`, default 10) against credential brute-force,
and every route — Swagger included — shares a generous catch-all
(`RATE_LIMIT_API_PER_MINUTE`, default 300). Exceeding either answers `429` with
a `Retry-After` header (seconds). Set a limit to `0` to disable that tier —
useful for load tests.

Behind a reverse proxy every connection carries the proxy's address, so also
set `TRUST_PROXY=true` to key clients by the rightmost `X-Forwarded-For` entry
(the one the proxy itself appends). Leave it off when clients reach the server
directly — the header is client-forged in that case.

## CORS

Cookie-authenticated APIs can't use a wildcard `Access-Control-Allow-Origin`
(browsers refuse it on credentialed requests), so the server always advertises
`Access-Control-Allow-Credentials: true` and, by default, reflects the
request's `Origin` — dev-friendly, effectively open. In production set
`CORS_ALLOWED_ORIGINS` to a comma-separated allowlist to restrict which
browser origins may call the API.

## Roles & access control

Every user has one of four roles, ranked lowest to highest:

```
student  <  teacher  <  manager  <  admin
```

The check is **hierarchical** — a higher role satisfies any lower requirement
(an admin can do anything a teacher can). New accounts always register as
`student`; a role read is re-checked on every request, so a role change takes
effect on the user's very next call (no re-login).

| Action                                   | Minimum role | Notes                                         |
|------------------------------------------|--------------|-----------------------------------------------|
| Register / login / view own account      | (any)        | Registration always creates a `student`       |
| View events, roster, own notes; CRUD notes | student    | Everyone can read events and keep notes       |
| Mark **own** attendance                  | student      | Anyone can mark themselves                     |
| Mark **another user's** attendance       | teacher      |                                               |
| Create events; remove attendance rows    | teacher      |                                               |
| Edit / delete an event                   | teacher      | Only the **creator**, or a `manager`+ for any event |
| View exams; read **own** exam result     | student      |                                               |
| Create exams; grade students; view all results | teacher | Grading never targets oneself — teachers grade students |
| Edit / delete an exam                     | teacher      | Only the **creator**, or a `manager`+ for any exam |
| List users; change a user's role         | admin        | An admin cannot change **their own** role     |

### Bootstrapping the first admin

There is no self-service path to `admin` — the first one is granted out-of-band,
directly in the database. Register the account through the API, then (with the
server stopped, since the embedded store is single-writer) run:

```surql
UPDATE user SET role = 'admin' WHERE username = 'ada';
```

against the embedded database — e.g. with the SurrealDB CLI pointed at the same
surrealkv path (`./data/hezarfen.db`, namespace/database `hezarfen`). That admin
can then promote everyone else through `PATCH /users/{id}/role`.

## Endpoints

`Auth` is the minimum role; `no` means no session required, `student` means any
logged-in user.

| Method | Path                             | Auth    | Description                     |
|--------|----------------------------------|---------|---------------------------------|
| GET    | `/health`                        | no      | Liveness check                  |
| GET    | `/`                              | no      | Same as `/health`               |
| GET    | `/swagger`                       | no      | Interactive API docs (Swagger UI) |
| GET    | `/api-docs/openapi.json`         | no      | Raw OpenAPI 3 spec              |
| POST   | `/auth/register`                 | no      | `{username, password}` (new users are `student`) |
| POST   | `/auth/login`                    | no      | `{username, password}` -> cookie|
| POST   | `/auth/logout`                   | no      | Clear session (no-op if none)   |
| GET    | `/auth/me`                       | student | Current user (incl. `role`)     |
| GET    | `/users`                         | admin   | List all users                  |
| PATCH  | `/users/{id}/role`               | admin   | `{role}` — set a user's role    |
| POST   | `/notes`                         | student | `{title, content?}`             |
| GET    | `/notes`                         | student | List own notes                  |
| GET    | `/notes/{id}`                    | student | Get own note                    |
| PATCH  | `/notes/{id}`                    | student | `{title?, content?}`            |
| DELETE | `/notes/{id}`                    | student | Delete own note                 |
| POST   | `/events`                        | teacher | `{title, description?, starts_at?, ends_at?}` |
| GET    | `/events`                        | student | List all events                 |
| GET    | `/events/{id}`                   | student | Get event                       |
| PATCH  | `/events/{id}`                   | teacher | Edit event (creator, or manager+ for any) |
| DELETE | `/events/{id}`                   | teacher | Delete event (creator, or manager+ for any) |
| POST   | `/events/{id}/attendance`        | student | `{status, user_id?}` — self if `user_id` omitted; marking others needs teacher+ |
| GET    | `/events/{id}/attendance`        | student | List attendance for event       |
| DELETE | `/events/{id}/attendance/{user}` | teacher | Remove a user's attendance      |
| POST   | `/exams`                         | teacher | `{title, description?, kind}` (creator owns it) |
| GET    | `/exams`                         | student | List all exams                  |
| GET    | `/exams/{id}`                    | student | Get exam                        |
| PATCH  | `/exams/{id}`                    | teacher | Edit exam (creator, or manager+ for any) |
| DELETE | `/exams/{id}`                    | teacher | Delete exam + its results (creator, or manager+) |
| POST   | `/exams/{id}/results`            | teacher | `{mark, user_id}` — grade a student (upsert) |
| GET    | `/exams/{id}/results`            | teacher | List every result for the exam  |
| GET    | `/exams/{id}/result`             | student | The caller's **own** result (`404` until graded) |
| DELETE | `/exams/{id}/results/{user}`     | teacher | Remove a student's result       |

`status` ∈ `present | absent | late | excused`. `kind` ∈ `homework | quiz`.
`role` ∈ `student | teacher | manager | admin`. Ids in responses are ULIDs.
An exam `mark` is an integer `0`–`100`; it lives in its own `exam_result` row,
never in a `note`. Students never grade anyone — grading is teacher+; a
student reads just their own mark via `GET /exams/{id}/result`.
Event `starts_at`/`ends_at` are optional **unix-millisecond** integers; if both
are given, `ends_at` must not precede `starts_at` (else `400`). On `PATCH`, an
omitted time keeps its value and an explicit `null` clears it.
Reading a child collection of a missing parent (`/events/{id}/attendance`,
`/exams/{id}/results`) is a `404`, not an empty list.

## Quick tour (curl)

```sh
BASE=http://127.0.0.1:8080
JAR=/tmp/hz.cookies

curl -s $BASE/auth/register -H 'content-type: application/json' \
  -d '{"username":"ali","password":"secret1"}'

curl -s -c $JAR $BASE/auth/login -H 'content-type: application/json' \
  -d '{"username":"ali","password":"secret1"}'

# notes
curl -s -b $JAR $BASE/notes -H 'content-type: application/json' \
  -d '{"title":"first","content":"hello"}'
curl -s -b $JAR $BASE/notes

# events + attendance
# Creating events needs teacher+; grant it first (see "Bootstrapping" above —
# e.g. UPDATE user SET role='teacher' WHERE username='ali'), else this is 403.
EV=$(curl -s -b $JAR $BASE/events -H 'content-type: application/json' \
  -d '{"title":"standup"}' | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
curl -s -b $JAR $BASE/events/$EV/attendance -H 'content-type: application/json' \
  -d '{"status":"present"}'
curl -s -b $JAR $BASE/events/$EV/attendance
```

## Layout

```
src/
  main.rs          bootstrap: config, db, router, serve
  lib.rs           build_router: routes + OpenAPI/Swagger + CORS + /health
  config.rs        env-sourced Config
  constant.rs      validation limits
  validate.rs      field validators (used by every newtype's try_new)
  error.rs         ValidationError + AppError -> HTTP responses
  database.rs      embedded surrealkv connect + SCHEMAFULL migration
  rate_limit.rs    fixed-window per-IP limiter (both tiers) + middleware
  state.rs         AppState { db, cookie_secure, rate_limit }
  domain/          validated newtypes + entities (derive SurrealValue),
                   each owning its persistence
    user.rs        UserId · Username · Password · PasswordHash · User (has role)
    role.rs        Role enum (student < teacher < manager < admin), at_least()
    session.rs     SessionId · SessionToken · Session (7-day expiry)
    timestamp.rs   Timestamp (unix-millisecond instant)
    note.rs        NoteId · NoteTitle · NoteContent · Note
    event.rs       EventId · EventTitle · EventDescription · Event
    attendance.rs  AttendanceId · AttendanceStatus · Attendance
    exam.rs        ExamId · ExamTitle · ExamDescription · ExamKind · Exam
    exam_result.rs ExamResultId · Mark · ExamResult (one row per exam+user)
  web/             axum layer: DTOs (serde + OpenAPI schemas) + handlers +
                   auth extractors
    extractor.rs   CurrentUser · RequireTeacher · RequireAdmin
    dto.rs         shared UserResponse (id · username · role) + Role schema
    auth.rs  users.rs  notes.rs  events.rs  exams.rs
```

Tests: `cargo test` — unit (in-source), integration (`tower::oneshot` + in-memory
db), rate-limit (both tiers, proxy-header and peer-address keying, shipped
limits over every route), e2e (real TCP + reqwest cookie jar), persistence
(tempfile file engine, including close + reopen).
