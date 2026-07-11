# hezarfen_backend

Note, attendance, course + weighted exam mark backend. **Rust (edition 2024) · axum · SurrealDB 3 (embedded surrealkv) · tokio.**

Session-cookie auth with four hierarchical roles (`student < teacher < manager
< admin`). Notes are per-user. Attendance is event + attendees: create an event,
then mark users present / absent / late / excused. Marks are course-shaped
(Google Classroom style): a teacher creates a course, enrolls students, adds
weighted exams inside it, and grades; students read a per-course weighted
average and an overall average from their mark report.

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
| View courses/exams; read **own** result, courses, mark report | student |                            |
| Create courses                           | teacher      | The creator manages the course                 |
| Manage inside a course: edit/delete it, enroll/unenroll, add/edit/delete its exams, grade, remove results | teacher | Only the **course creator**, or a `manager`+ for any course |
| View rosters, full result lists, exam statistics, any user's mark report | teacher | Staff-wide reads |
| Grade students                           | teacher      | Target must be **enrolled**; grading never targets oneself |
| Edit **own** personal info (name, surname, email, phone, birth date) | student | Every account carries the same optional info fields |
| List users; look up one user; change a user's role; edit **any** user's personal info | admin | An admin cannot change **their own** role |

### Bootstrapping the first admin

There is no self-service path to `admin` — registration always creates a
`student`. The first admin comes from the startup seed: set both

```sh
ADMIN_USERNAME=admin
ADMIN_PASSWORD=admin123   # local-dev default used by compose.yaml; change it
```

and on boot the account is created with the `admin` role **if the username
doesn't exist yet**. The seed is idempotent and deliberately conservative: it
never promotes or rewrites an existing account (if the name is taken by a
non-admin it logs a warning and leaves it alone — promoting someone else's
account would be an escalation). Setting only one of the two variables aborts
startup. `compose.yaml` ships with the credentials above for local dev.

That admin can then promote everyone else through `PATCH /users/{id}/role`.

Manual fallback (also the recovery path if the seeded name was squatted or the
sole admin locked themselves out): with the server stopped, since the embedded
store is single-writer, run

```surql
UPDATE user SET role = 'admin' WHERE username = 'ada';
```

against the embedded database — e.g. with the SurrealDB CLI pointed at the same
surrealkv path (`./data/hezarfen.db`, namespace/database `hezarfen`).

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
| GET    | `/auth/me`                       | student | Current user (incl. `role` and personal info) |
| PATCH  | `/users/me`                      | student | Update own personal info (see below) |
| GET    | `/users`                         | admin   | List all users                  |
| GET    | `/users/{id}`                    | admin   | Get one user                    |
| PATCH  | `/users/{id}/role`               | admin   | `{role}` — set a user's role    |
| PATCH  | `/users/{id}/profile`            | admin   | Update any user's personal info |
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
| POST   | `/courses`                       | teacher | `{title, description?}` (creator manages it) |
| GET    | `/courses`                       | student | List all courses                |
| GET    | `/courses/me`                    | student | The caller's **enrolled** courses |
| GET    | `/courses/{id}`                  | student | Get course                      |
| PATCH  | `/courses/{id}`                  | teacher | Edit course (creator, or manager+ for any) |
| DELETE | `/courses/{id}`                  | teacher | Delete course + its exams, results, enrollments (creator, or manager+) |
| POST   | `/courses/{id}/enrollments`      | teacher | `{user_id}` — enroll a user (idempotent upsert; course manager) |
| GET    | `/courses/{id}/enrollments`      | teacher | List the course roster          |
| DELETE | `/courses/{id}/enrollments/{user}` | teacher | Unenroll (keeps recorded results; course manager) |
| POST   | `/courses/{id}/exams`            | teacher | `{title, description?, kind, weight}` — add an exam (course manager) |
| GET    | `/courses/{id}/exams`            | student | List the course's exams         |
| GET    | `/exams`                         | student | List all exams                  |
| GET    | `/exams/{id}`                    | student | Get exam                        |
| PATCH  | `/exams/{id}`                    | teacher | Edit exam incl. `weight` (course manager; `course` immutable) |
| DELETE | `/exams/{id}`                    | teacher | Delete exam + its results (course manager) |
| POST   | `/exams/{id}/results`            | teacher | `{mark, user_id}` — grade an **enrolled** student (upsert; course manager) |
| GET    | `/exams/{id}/results`            | teacher | List every result for the exam  |
| GET    | `/exams/{id}/result`             | student | The caller's **own** result (`404` until graded) |
| DELETE | `/exams/{id}/results/{user}`     | teacher | Remove a student's result (course manager) |
| GET    | `/exams/{id}/statistics`         | teacher | `{graded, average, min, max}` over the exam's results |
| GET    | `/marks/me`                      | student | The caller's mark report (per-course + overall averages) |
| GET    | `/marks/{user}`                  | teacher | Any user's mark report          |

`status` ∈ `present | absent | late | excused`.
`kind` ∈ `homework | quiz | midterm | final | project | oral` — informational
metadata; the average is driven by `weight`, an integer `1`–`100` set per exam.
`role` ∈ `student | teacher | manager | admin`. Ids in responses are ULIDs.
An exam `mark` is an integer `0`–`100`; it lives in its own `exam_result` row,
never in a `note`. Students never grade anyone — grading is teacher+ with
course-management rights, and the target must be enrolled in the exam's course;
a student reads just their own mark via `GET /exams/{id}/result`.
A course average is `Σ(mark×weight) / Σ(weight)` over the student's **graded**
exams in that course (`null` while nothing is graded — ungraded exams are
skipped, not zeroed). The overall average is the plain mean of the non-null
course averages. Unenrolling keeps result rows: the marks drop out of the
report until re-enrollment, but stay visible on the exam itself. Deleting a
course cascades its exams, their results, and all enrollments.
Event `starts_at`/`ends_at` are optional **unix-millisecond** integers; if both
are given, `ends_at` must not precede `starts_at` (else `400`). On `PATCH`, an
omitted time keeps its value and an explicit `null` clears it.
Reading a child collection of a missing parent (`/events/{id}/attendance`,
`/exams/{id}/results`, `/courses/{id}/enrollments`, `/courses/{id}/exams`) is
a `404`, not an empty list.
Personal info (`name`, `surname`, `email`, `phone`, `birth_date`) is the same
optional set on every account, whatever the role, and is `null` until filled
in. On `PATCH /users/me` (or the admin `PATCH /users/{id}/profile`) each field
is independent: omitted (or `null`) keeps the current value, an empty string
`""` clears it, anything else is validated — email must look like
`name@example.com`, phone is 7–15 digits with an optional `+` and cosmetic
separators, `birth_date` is a real `YYYY-MM-DD` calendar date not in the
future. Names allow unicode; usernames stay ASCII.

> **Upgrading a pre-course database**: `exam` rows created before courses
> existed lack the now-required `course` and `weight` fields and will fail to
> deserialize. For a dev database, delete `./data/hezarfen.db` and reboot; to
> keep data, backfill manually with the SurrealDB CLI (server stopped), e.g.
> `UPDATE exam SET course = course:<id>, weight = 1 WHERE course = NONE;`
> after creating a course to attach them to.

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

# courses + weighted marks (as a teacher; $SID is a student's user id)
CO=$(curl -s -b $JAR $BASE/courses -H 'content-type: application/json' \
  -d '{"title":"algebra"}' | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
curl -s -b $JAR $BASE/courses/$CO/enrollments -H 'content-type: application/json' \
  -d "{\"user_id\":\"$SID\"}"
EX=$(curl -s -b $JAR $BASE/courses/$CO/exams -H 'content-type: application/json' \
  -d '{"title":"midterm","kind":"midterm","weight":3}' | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
curl -s -b $JAR $BASE/exams/$EX/results -H 'content-type: application/json' \
  -d "{\"mark\":90,\"user_id\":\"$SID\"}"
curl -s -b $JAR $BASE/exams/$EX/statistics
# ...and as the student:
curl -s -b $STUDENT_JAR $BASE/marks/me
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
    course.rs      CourseId · CourseTitle · CourseDescription · Course
    enrollment.rs  EnrollmentId · Enrollment (one row per course+user)
    exam.rs        ExamId · ExamTitle · ExamDescription · ExamKind · ExamWeight · Exam (belongs to a course)
    exam_result.rs ExamResultId · Mark · ExamResult (one row per exam+user)
  web/             axum layer: DTOs (serde + OpenAPI schemas) + handlers +
                   auth extractors
    extractor.rs   CurrentUser · RequireTeacher · RequireAdmin
    dto.rs         shared UserResponse · CourseResponse · ExamResponse schemas
    auth.rs  users.rs  notes.rs  events.rs  courses.rs  exams.rs  marks.rs
```

Tests: `cargo test` — unit (in-source), integration (`tower::oneshot` + in-memory
db), rate-limit (both tiers, proxy-header and peer-address keying, shipped
limits over every route), e2e (real TCP + reqwest cookie jar), persistence
(tempfile file engine, including close + reopen).
