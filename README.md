# hezarfen_backend

Note, attendance, course + weighted exam mark backend. **Rust (edition 2024) · axum · SurrealDB 3 (embedded surrealkv) · tokio.**

Session-cookie auth with four hierarchical roles (`student < teacher < manager
< admin`). Notes are per-user. Attendance is event + attendees: create an event,
then mark users present / absent / late / excused. Marks are course-shaped
(Google Classroom style): a teacher creates a course, enrolls students, adds
weighted exams inside it, and grades; students read a per-course weighted
average and an overall average from their mark report. Exams run **sync** (one
fixed window), **async** (start anytime inside the window, with a personal
time budget), or **open** (sit anytime, optionally timed per attempt);
students *sit* them via attempts — retakes metered by a per-exam limit
(`0` = unlimited), leaving the exam room governed by a teacher-controlled
rejoin door — and teachers watch attendance, per-student remaining time,
sittings, walk-outs, submissions, and marks land live on a monitor endpoint
(snapshot or SSE stream). Courses also carry **lesson
sessions** with teacher-taken roll call (students never self-mark a lesson),
staff clock in/out on a server-stamped **work log**, and every user has an
**attendance report** (event + per-course lesson tallies with rates).
School-varying policy is data, not code: exam kinds, attendance statuses, and
grade-display bands live in an editable **settings** singleton, and academic
**terms** are plain rows courses can link to (see "Per-school policy").

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

## Time policy

Timezones and clock differences cannot corrupt data, by construction:

- **Every instant is a UTC unix-millisecond `i64`** (`domain::timestamp::Timestamp`),
  stored as an `int`, sent as a plain number. Nothing stores or parses a
  timezone, so the server's `TZ`, the container's clock config, and the
  client's locale are all irrelevant — clients convert millis to local time
  for display only.
- **One clock choke point.** `Timestamp::now()` is the only wall-clock read.
  `clippy.toml` bans every other source (`chrono::Local`/`Utc::now`,
  `SystemTime::now`, `time::OffsetDateTime::now_*`, absolute cookie
  `Expires`) and `[lints.clippy]` raises that to a hard `cargo clippy` error,
  so local-time bugs can't be reintroduced.
- **Expiry is server-authoritative.** Sessions expire by comparing the stored
  millis against the server clock; the cookie carries a relative `Max-Age`
  (never an absolute `Expires`), so a wrong client clock changes nothing.
- **Pacing uses the monotonic clock.** The rate limiter runs on `Instant`,
  immune to NTP steps and wall-clock jumps.
- **Calendar dates get a timezone grace.** A birth date is a calendar date on
  the writer's wall, not an instant: validation accepts up to UTC-tomorrow,
  since a client ahead of UTC (up to UTC+14) legitimately writes a date the
  server's UTC calendar hasn't reached yet.
- **Nothing schedules in the past.** A request-supplied schedule instant — an
  exam window, a lesson's `starts_at`/`ends_at`, an event time — must not lie
  before the server's now, on create and on every `PATCH` that sets it: a
  deadline that starts in the past is dead on arrival, so it's a `400`. A
  60-second grace absorbs request latency and client-clock skew ("starts now"
  survives its own round trip). Values a `PATCH` merely keeps are exempt — a
  running exam's `starts_at` is legitimately past, and a rename or deadline
  extension must not trip over it. (Work-log corrections are records of past
  work, not schedules, and stay free.)
- **Frontends can sync to the server clock.** `GET /time` (no auth) returns
  `{"now": <UTC unix-millis>}`. Fetch once, keep
  `offset = now - Date.now()`, and use `Date.now() + offset` for countdowns
  and past/future checks instead of trusting the device clock.

Keep the single server's clock NTP-synced; that's the only clock that matters.

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
| View events, own notes; CRUD notes       | student      | Everyone can read events and keep notes       |
| Mark **own** attendance                  | student      | Anyone can mark themselves                     |
| Mark **another user's** attendance       | teacher      |                                               |
| Create events; remove attendance rows    | teacher      |                                               |
| List an event's attendance roster        | teacher      | Students read their own tallies via the attendance report |
| Edit / delete an event                   | teacher      | Only the **creator**, or a `manager`+ for any event |
| View a course's sessions                 | student      | Only inside **visible** courses: enrolled, creator, or `manager`+ |
| List a session's roll call               | teacher      | The **session's teacher**, or anyone with course-management rights |
| Create / edit / delete a course session  | teacher      | Course-management rights (course creator, or `manager`+) |
| Take a session's roll call (mark/remove **enrolled students**) | teacher | The **session's teacher**, or anyone with course-management rights |
| Mark / remove the **session teacher's** presence row | manager | Staff presence is management's call — the teacher can't self-mark |
| Work check-in / check-out; view **own** work log | teacher | Instants are server-stamped, never client-supplied |
| View / correct / delete **any** staff work log entry | manager | Corrections only on closed entries |
| Read **own** attendance report           | student      |                                               |
| Read another user's attendance report    | teacher      | Narrowed to the caller's managed courses; `manager`+ sees all |
| View **visible** courses/exams; read **own** result, courses, mark report | student | Visible = enrolled (teachers: + created; `manager`+: all) |
| Sit a sittable exam (`sync`/`async`/`open`): start / resume / retake / read / submit **own** attempt | student | Must be enrolled; window (where one exists) and `max_attempts` enforced by the server |
| Answer questions inside **own** attempt (REST autosave or the exam-room WebSocket) | student | Attempt must be `in_progress`; deadline judged by the server clock; blocked after leaving the room while `allow_rejoin` is off |
| Author an exam's questions (add/edit/delete)  | teacher | Course-management rights; frozen once anyone has an attempt |
| Read a question list (with `correct`) or a student's answer sheet | teacher | Course-management rights — one teacher can't read another's answer key |
| Watch an exam's live monitor (snapshot or SSE stream) | teacher | Course-management rights |
| Create courses                           | teacher      | The creator manages the course                 |
| Manage inside a course: edit/delete it, enroll/unenroll, add/edit/delete its exams, grade, remove results | teacher | Only the **course creator**, or a `manager`+ for any course |
| View a course's roster, an exam's result list / statistics | teacher | Course-management rights |
| Read another user's mark report          | teacher      | Narrowed to the caller's managed courses; `manager`+ sees all |
| Grade students                           | teacher      | Target must be **enrolled**; grading never targets oneself |
| Edit **own** personal info (name, surname, email, phone, birth date) | student | Every account carries the same optional info fields |
| Read the school settings and the term list | student | Clients need them to render kind/status pickers, grades, and the calendar |
| Edit school settings; create / edit / delete terms | manager | School policy (exam kinds, attendance statuses, grade bands) and the academic calendar are management's call |
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

Course data is walled per course. A course, its exams, and its sessions are
**visible** only to its enrolled users, its creator, and manager+ — a student
sees just the classes they were added to, and the `/courses` / `/exams`
catalogs are filtered accordingly. Teacher-level reads *inside* a course
(roster, results, statistics, the question list, answer sheets, the live
monitor) additionally need **course-management rights** (creator or manager+):
one teacher cannot look into another teacher's course, and the per-user
marks/attendance reports narrow to the courses the caller manages.

| Method | Path                             | Auth    | Description                     |
|--------|----------------------------------|---------|---------------------------------|
| GET    | `/health`                        | no      | Liveness check                  |
| GET    | `/`                              | no      | Same as `/health`               |
| GET    | `/time`                          | no      | Server clock: `{now}` UTC unix-millis (frontend sync) |
| GET    | `/swagger`                       | no      | Interactive API docs (Swagger UI) |
| GET    | `/api-docs/openapi.json`         | no      | Raw OpenAPI 3 spec              |
| POST   | `/auth/register`                 | no      | `{username, password}` (new users are `student`) |
| POST   | `/auth/login`                    | no      | `{username, password}` -> cookie|
| POST   | `/auth/logout`                   | no      | Clear session (no-op if none)   |
| GET    | `/auth/me`                       | student | Current user (incl. `role` and personal info) |
| PATCH  | `/users/me`                      | student | Update own personal info (see below) |
| GET    | `/users/search`                  | teacher | `?q=<fragment>&role=<role?>` — find users by username/name fragment (pickers); ≤10 refs, no contact info |
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
| GET    | `/events/{id}/attendance`        | teacher | List attendance for event (students read their own tallies via `/attendance/me`) |
| DELETE | `/events/{id}/attendance/{user}` | teacher | Remove a user's attendance      |
| POST   | `/courses`                       | teacher | `{title, description?}` (creator manages it) |
| GET    | `/courses`                       | student | The caller's visible courses: created + enrolled (manager+: all) |
| GET    | `/courses/me`                    | student | The caller's **enrolled** courses |
| GET    | `/courses/{id}`                  | student | Get course (enrolled, creator, or manager+) |
| PATCH  | `/courses/{id}`                  | teacher | Edit course (creator, or manager+ for any) |
| DELETE | `/courses/{id}`                  | teacher | Delete course + its exams, results, enrollments (creator, or manager+) |
| POST   | `/courses/{id}/enrollments`      | teacher | `{user_id}` — enroll a user (idempotent upsert; course manager) |
| GET    | `/courses/{id}/enrollments`      | teacher | List the course roster (course manager) |
| DELETE | `/courses/{id}/enrollments/{user}` | teacher | Unenroll (keeps recorded results; course manager) |
| POST   | `/courses/{id}/sessions`         | teacher | `{topic?, teacher_id?, starts_at, ends_at?}` — add a lesson (course manager; teacher defaults to the caller) |
| GET    | `/courses/{id}/sessions`         | student | List the course's sessions, most recent first (enrolled, creator, or manager+) |
| GET    | `/sessions/{id}`                 | student | Get session (enrolled, session teacher, or course manager) |
| PATCH  | `/sessions/{id}`                 | teacher | Edit session (course manager; `null` clears `ends_at`) |
| DELETE | `/sessions/{id}`                 | teacher | Delete session + its roll call (course manager) |
| POST   | `/sessions/{id}/attendance`      | teacher | `{status, user_id}` — roll call: session teacher/course manager mark **enrolled** students; the teacher's own row needs manager+ |
| GET    | `/sessions/{id}/attendance`      | teacher | List the session's roll call (session teacher or course manager) |
| DELETE | `/sessions/{id}/attendance/{user}` | teacher | Remove a roll-call row (same rights as marking) |
| POST   | `/courses/{id}/exams`            | teacher | `{title, description?, kind, weight, mode?, starts_at?, ends_at?, duration_ms?, max_attempts?, allow_rejoin?}` — add an exam (course manager) |
| GET    | `/courses/{id}/exams`            | student | List the course's exams (enrolled, creator, or manager+) |
| GET    | `/exams`                         | student | The caller's visible exams: their courses' (manager+: all) |
| GET    | `/exams/{id}`                    | student | Get exam (enrolled, creator, or manager+) |
| PATCH  | `/exams/{id}`                    | teacher | Edit exam incl. `weight`, schedule, `max_attempts`, `allow_rejoin` (course manager; `course` immutable, `mode` frozen once attempted — the rest stays live) |
| DELETE | `/exams/{id}`                    | teacher | Delete exam + its results, attempts, questions, and answers (course manager) |
| POST   | `/exams/{id}/results`            | teacher | `{mark, user_id}` — grade an **enrolled** student (upsert; course manager) |
| GET    | `/exams/{id}/results`            | teacher | List every result for the exam (course manager) |
| GET    | `/exams/{id}/result`             | student | The caller's **own** result (`404` until graded) |
| DELETE | `/exams/{id}/results/{user}`     | teacher | Remove a student's result (course manager) |
| GET    | `/exams/{id}/statistics`         | teacher | `{graded, average, min, max}` over the exam's results (course manager) |
| POST   | `/exams/{id}/attempt`            | student | Start (`201`), resume (`200`), or retake (`201`, blank sheet) the caller's attempt — enrolled; window open where one exists; `409` once `max_attempts` is spent |
| GET    | `/exams/{id}/attempt`            | student | Own latest attempt: status, `attempt`/`attempts_used`/`max_attempts`, deadline, `remaining_ms`, `left_at`, mark, progress (`answered`/`question_count`), server `now` |
| POST   | `/exams/{id}/attempt/finish`     | student | Submit the attempt (`409` once the deadline passed); allowed even while locked out of the room |
| POST   | `/exams/{id}/questions`          | teacher | `{text, kind, points, choices?, correct?}` — add a question (course manager; frozen once attempted) |
| GET    | `/exams/{id}/questions`          | teacher | The full question list, `correct` included (course manager) |
| PATCH  | `/exams/{id}/questions/{qid}`    | teacher | Edit a question — the kind bundle revalidates as a unit (course manager; frozen once attempted) |
| DELETE | `/exams/{id}/questions/{qid}`    | teacher | Delete a question + its answers (course manager; frozen once attempted) |
| GET    | `/exams/{id}/attempt/questions`  | student | The sitting view: no `correct`, own answers embedded (requires an attempt) |
| POST   | `/exams/{id}/attempt/answers`    | student | `{question_id, selected? \| text?}` — autosave one answer while `in_progress` (and not locked out by a closed rejoin door) |
| GET    | `/exams/{id}/attempts/{user}/answers` | teacher | A student's answer sheet: `is_correct` flags + suggested `auto_score` (course manager) |
| GET    | `/exams/{id}/attempt/ws`         | student | **WebSocket** exam room: state ticks, autosave, finish; entering clears `left_at`, leaving stamps it (see "Taking an exam") |
| GET    | `/exams/{id}/live`               | teacher | Live monitor snapshot: roster × latest attempts × marks + per-student progress/`left_at`/`attempts_used` + counts (course manager) |
| GET    | `/exams/{id}/live/stream`        | teacher | The same snapshot as SSE `snapshot` events every ~2s (course manager) |
| GET    | `/marks/me`                      | student | The caller's mark report (per-course + overall averages) |
| GET    | `/marks/{user}`                  | teacher | A user's mark report, narrowed to the caller's courses (manager+: full) |
| POST   | `/work/check-in`                 | teacher | Open a work stint (server-stamped; `409` if already open) |
| POST   | `/work/check-out`                | teacher | Close the open stint (`409` if none open) |
| GET    | `/work/me`                       | teacher | Own work log, newest first (open stint has `check_out: null`) |
| GET    | `/work/{user}`                   | manager | A staff member's work log       |
| PATCH  | `/work/entries/{id}`             | manager | `{check_in?, check_out?}` — correct a **closed** stint (`409` on open) |
| DELETE | `/work/entries/{id}`             | manager | Delete a work entry (open or closed) |
| GET    | `/attendance/me`                 | student | Own attendance report: events + sessions + per-course tallies |
| GET    | `/attendance/{user}`             | teacher | A user's attendance report, narrowed to the caller's courses (manager+: full) |
| GET    | `/settings`                      | student | The school's policy: `exam_kinds`, `attendance_statuses`, `grade_bands` |
| PATCH  | `/settings`                      | manager | Replace any subset of the three lists, each wholesale (see "Per-school policy") |
| POST   | `/terms`                         | manager | `{name, starts_at, ends_at}` — past dates allowed (calendar backfill) |
| GET    | `/terms`                         | student | List terms, newest first        |
| GET    | `/terms/{id}`                    | student | Get one term                    |
| PATCH  | `/terms/{id}`                    | manager | Edit a term (the merged range must stay ordered) |
| DELETE | `/terms/{id}`                    | manager | Delete a term — linked courses are unlinked, never deleted |

`status` must be one of the school's attendance statuses (`GET /settings`);
the core four `present | absent | late | excused` always exist, plus whatever
the school added.
`kind` must be one of the school's exam kinds (`GET /settings`; defaults:
`homework | quiz | midterm | final | project | oral`) — informational
metadata; the average is driven by `weight`, an integer `1`–`100` set per exam.
A course may carry a `term_id` (`null` = unassigned); on `PATCH
/courses/{id}`, an omitted `term_id` keeps the link and an explicit `null`
clears it.
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
are given, `ends_at` must not precede `starts_at`, and neither may be set in
the past (else `400`). On `PATCH`, an omitted time keeps its value and an
explicit `null` clears it.
Reading a child collection of a missing parent (`/events/{id}/attendance`,
`/exams/{id}/results`, `/courses/{id}/enrollments`, `/courses/{id}/exams`,
`/courses/{id}/sessions`, `/sessions/{id}/attendance`) is a `404`, not an
empty list.
Personal info (`name`, `surname`, `email`, `phone`, `birth_date`) is the same
optional set on every account, whatever the role, and is `null` until filled
in. On `PATCH /users/me` (or the admin `PATCH /users/{id}/profile`) each field
is independent: omitted (or `null`) keeps the current value, an empty string
`""` clears it, anything else is validated — email must look like
`name@example.com`, phone is 7–15 digits with an optional `+` and cosmetic
separators, `birth_date` is a real `YYYY-MM-DD` calendar date not in the
future. Names allow unicode; usernames are strict: lowercase letters and
digits plus non-consecutive interior `.`, `_`, `-` separators, starting and
ending with a letter or digit (3–32 chars). Staff-looking names (`admin`,
`administrator`, `root`, `support`, `system`, `moderator`, `staff`) are
rejected at `/auth/register` only — the `ADMIN_USERNAME` bootstrap may still
seed them. A duplicate username on register is a `409`; that this reveals the
name is taken is a deliberate tradeoff (usernames are public handles here,
unlike emails).

## Per-school policy (settings & terms)

Every school runs differently; the parts that vary are data, not code. One
editable `settings` singleton (`GET /settings` for any signed-in user,
`PATCH /settings` for manager+) carries three knobs:

- **`exam_kinds`** — the accepted `kind` values for new exams. Defaults to
  `homework, quiz, midterm, final, project, oral`; replace the list with
  whatever the school grades (`lab`, `presentation`, …). Kinds stay
  informational — `weight` drives every average.
- **`attendance_statuses`** — what attendance marking accepts. The core four
  (`present`, `absent`, `late`, `excused`) are mandatory because the
  attendance rate is defined over them (`(present+late) /
  (present+absent+late)`); school extras (say `online`) are **rate-neutral**
  and tally under `custom` in the attendance reports.
- **`grade_bands`** — how numeric marks display: a list of `{min, label}`
  bands (`85 → "AA"`, `50 → "CC"`, …). One band must start at `0` so every
  mark maps; an empty list (the default) means numeric-only. Storage and
  averaging stay `0`–`100` forever — bands only add `grade`,
  `average_grade`, and `overall_grade` labels to the mark report, so a school
  can switch display scales without touching a single stored mark.

A `PATCH` replaces only the fields it carries, each wholesale, and validation
is all-or-nothing. Editing a list never rewrites history: an exam keeps its
retired kind, a roll-call row keeps its retired status — only **new writes**
are held to the current lists.

Academic structure is data too. **Terms** (`/terms`) model whatever calendar
the school runs — semester, trimester, quarter systems are just rows with a
name and a date range. Courses may link to one via `term_id` (nullable), and
deleting a term only unlinks its courses. Term dates may lie in the past,
deliberately: a school adopting the app mid-year backfills its calendar —
unlike exam/lesson/event times, which reject backdating.

What stays fixed is deliberate too: the four roles, the `0`–`100` mark scale,
validation bounds, and the UTC time policy are invariants, not preferences
(rename role labels in the frontend if a school says "principal" instead of
"manager"). Deployment knobs (ports, rate limits, admin seed, CORS) remain
environment variables — the model is **one school per deployment**, which
keeps every school's data physically isolated.

## Exam modes, attempts, retakes, rejoin & live monitoring

An exam is a **draft** by default (all schedule fields `null`) — graded
offline; attempts on it are a `409` ("nothing to sit"). Making it sittable
means giving it a `mode`, as one consistent unit (validated together on
create and after every `PATCH` merge):

- `mode: "sync"` + `starts_at` + `ends_at` — everyone sits inside one window;
  every attempt's deadline is `ends_at`.
- `mode: "async"` + `starts_at` + `ends_at` + `duration_ms` — each student
  starts anywhere inside the window and gets
  `min(started_at + duration_ms, ends_at)` as their personal deadline.
- `mode: "open"` — no window at all: students sit anytime. `duration_ms` is
  *optional* — set it for a per-attempt countdown (`started_at +
  duration_ms`), omit it for unlimited time (the attempt only ends by
  submission).
- `duration_ms` is 1 minute to 24 hours wherever it appears. `ends_at` must be
  strictly after `starts_at`, and neither may be *set* in the past — on create
  or `PATCH` (kept values are exempt, so a running exam stays editable). All
  instants are the usual UTC unix-milliseconds, judged only by the server
  clock (`GET /time` for sync).

Two per-exam policy knobs ride along, both **live-editable** at any point:

- `max_attempts` (default `1`, `0` = unlimited) — how many sittings each
  student gets. Raising it mid-exam grants retakes on the spot; lowering it
  never kills a running attempt, it only blocks future starts.
- `allow_rejoin` (default `true`) — whether a student who *left the exam room*
  may come back in and keep answering (see the exam-room section).

A student **sits** an exam through attempts (sitting 1, 2, … — each row id is
the composite `exam_user[_seq]` key, so a sitting exists at most once by
construction):

- `POST /exams/{id}/attempt` starts, resumes, or retakes (enrolled; window
  open where one exists — `open` exams start anytime). While the latest
  sitting runs, re-posting returns it unchanged (`200`, not `201`):
  reconnecting never resets the clock. Once it is submitted or expired,
  re-posting mints the next sitting (`201`) **from a blank answer sheet** —
  the previous sitting's answers are wiped — until `max_attempts` is spent
  (`409` after that). Starting is the live-attendance signal.
- `GET /exams/{id}/attempt` is the student's exam screen: the latest sitting's
  `status` (`in_progress` | `submitted` | `expired`), `attempt` (its number),
  `attempts_used`/`max_attempts`, `deadline`/`remaining_ms` (`null` for an
  untimed open exam), `left_at`, own `mark` once graded, and the server `now`.
- `POST /exams/{id}/attempt/finish` submits. After the deadline the attempt is
  `expired` — a valid terminal state (the student used their full time), and
  finishing answers `409`. Grading stays per exam+user (one mark), whatever
  the sitting count — the sheet a grader sees is always the latest sitting's.

Deadlines are **recomputed from the exam's current schedule on every read**,
never stored: a teacher who `PATCH`es `ends_at` (or a `duration_ms`)
while the exam runs moves every running deadline instantly. What's frozen once
anyone has started is only `mode` (including back to a draft) — swapping the
deadline rules mid-sitting would be a different exam (`409`).

The course's manager (its creator, or manager+) watches it all live:
`GET /exams/{id}/live` returns one snapshot —
the enrolled roster joined with attempts and marks (`not_started` |
`in_progress` | `submitted` | `expired`, per-student `attempt` /
`attempts_used` / `left_at` / `deadline` / `remaining_ms` / `mark`, always the
latest sitting) plus summary counts, all judged at a single `now`.
`GET /exams/{id}/live/stream` is the same JSON as Server-Sent Events: a
`snapshot` event immediately on connect, then every ~2 s — attendance, ticking
clocks, submissions, and marks land without polling:

```js
const es = new EventSource(`${BASE}/exams/${id}/live/stream`, { withCredentials: true });
es.addEventListener("snapshot", (e) => render(JSON.parse(e.data)));
```

Deleting an exam (or its course) cascades attempts, questions, and answers
along with results; unenrolling mid-exam hides the student from the monitor
roster but keeps the attempt and mark rows, mirroring the marks report.

> **Upgrading a pre-course database**: `exam` rows created before courses
> existed lack the now-required `course` and `weight` fields and will fail to
> deserialize. For a dev database, delete `./data/hezarfen.db` and reboot; to
> keep data, backfill manually with the SurrealDB CLI (server stopped), e.g.
> `UPDATE exam SET course = course:<id>, weight = 1 WHERE course = NONE;`
> after creating a course to attach them to.
>
> **Upgrading to the attempts/rejoin build needs nothing manual**: the boot
> migration backfills `max_attempts = 1`, `allow_rejoin = true`, and attempt
> `seq = 1` on existing rows, and swaps the single-attempt unique index for
> the per-sitting one. Pre-upgrade attempts keep their record ids and count
> as sitting #1.

## Taking an exam: questions, answers & the exam room

Teachers author a question list per exam; students answer inside their
attempt, autosaved as they go; choice questions are machine-checked as a
**suggestion** — the final mark stays a human call through the existing
`POST /exams/{id}/results`.

**Questions** (`POST/GET/PATCH/DELETE /exams/{id}/questions[/{qid}]`,
course-management rights): each has `text` (≤ 2000 chars), `points` `1`–`100`
(its share of the auto-score), and a `kind`:

- `kind: "choice"` — carries `choices` (2–10 options, each ≤ 500 chars) and
  `correct`, the zero-based index of the right option. Auto-scorable.
- `kind: "text"` — free text, judged by the grader; carries neither.

The kind bundle is validated as a unit (create and after every `PATCH` merge):
switching a question to `text` needs explicit `"choices": null, "correct":
null`, switching to `choice` must bring both along. Presentation order is
creation order. The whole list **freezes once anyone has started an attempt**
(`409` on create/edit/delete) — editing questions under a sitting student
would fork what "the exam" means. Deleting a question cascades its answers.

**Answering** (student, attempt `in_progress`, deadline judged by the server
clock on every save):

- `GET /exams/{id}/attempt/questions` — the sitting view. Requires an attempt
  (`404` before `POST /exams/{id}/attempt`; also the anti-peek gate), never
  contains `correct`, and embeds the caller's own saved answer per question
  (`{selected, text, updated_at}` or `null`). Still readable after
  submitting/expiry, for review.
- `POST /exams/{id}/attempt/answers` `{question_id, selected? | text?}` — an
  upsert: one row per question+user, re-answering overwrites. The payload must
  match the question's kind (`selected` indexing a choice, or `text`
  ≤ 10 000 chars — empty clears the draft); mismatches are `400`s. Once the
  attempt is submitted or past its deadline every save is a `409` — likewise
  while the student has left the exam room with `allow_rejoin` off; answers
  saved in time survive untouched for grading (until a retake wipes the sheet
  for the next sitting).

**Grading view** (course-management rights): `GET /exams/{id}/attempts/{user}/answers` returns
the student's sheet — every saved answer with `is_correct` (`true`/`false` for
choice, `null` for text: that's the grader's call) plus
`auto_score: {earned, possible}` summing the choice questions' points. It is a
suggestion to read while grading, never written anywhere.

**The exam room (WebSocket)** — `GET /exams/{id}/attempt/ws`, cookie-authed
like everything else; REST above remains the full fallback. Gates run before
the upgrade: unknown exam `404`, draft with no mode `409`, not enrolled `403`,
no attempt yet `404` (start it first), submitted/expired `409`, left while
rejoin is closed `409`. Then JSON text frames:

| direction | frame |
|-----------|-------|
| server →  | `{"type":"state", status, attempt, deadline, remaining_ms, now, answered, question_count}` on connect, every ~2 s, and after each save |
| client →  | `{"type":"answer", "question_id":"…", "selected":1}` or `{"type":"answer", "question_id":"…", "text":"…"}` |
| server →  | `{"type":"saved", question_id, updated_at}` — the autosave ack |
| client →  | `{"type":"finish"}` — submit the attempt |
| server →  | `{"type":"finished", finished_at}`, then Close |
| server →  | `{"type":"expired"}`, then Close — a tick noticed the deadline |
| client →  | `{"type":"ping"}` → server `{"type":"pong"}` |
| server →  | `{"type":"error", message}` — bad JSON, wrong kind, deadline, … |

Every tick and every save re-read the exam, so a mid-exam `ends_at` extension
moves the room's countdown on the next tick, and no stale socket can write
past its real deadline — the socket shares the exact REST write path.

The room is also the presence signal behind the **rejoin door**: connecting
clears the attempt's `left_at`, and the *last* socket of the sitting to close
while it is still running stamps it (visible on the live monitor — "left the
room three minutes ago" at a glance). Closing one of two tabs is not leaving,
and a lingering socket from an already-finished sitting never marks a later
retake as left — each room is bound to the sitting it was opened for. With the exam's `allow_rejoin` off, a stamped
`left_at` refuses re-entry *and* any further saves, over the socket or REST,
until the teacher flips the door back open (`PATCH /exams/{id}
{"allow_rejoin": true}` — live, like the times). Finishing stays allowed: a
locked-out student can always submit what they saved. The timer never pauses
either way. Students who only ever use REST never trip the door — the room is
what marks leaving.
Example client:

```js
const ws = new WebSocket(`${BASE.replace("http", "ws")}/exams/${id}/attempt/ws`);
ws.onmessage = (e) => {
  const m = JSON.parse(e.data);
  if (m.type === "state") renderCountdown(m.remaining_ms, m.answered, m.question_count);
  if (m.type === "saved") markSaved(m.question_id);
};
const save = (question_id, selected) =>
  ws.send(JSON.stringify({ type: "answer", question_id, selected }));
```

The live monitor rides along: each roster row now carries `answered` and
`last_activity` (latest save instant), and the snapshot a top-level
`question_count`, so "stuck at 3/10 for five minutes" is visible at a glance.
The student's own `GET /exams/{id}/attempt` echoes the same
`answered`/`question_count` pair.

## Lesson sessions, roll call, the work log & attendance reports

Events cover ad-hoc gatherings; **sessions** are a course's lessons. A session
belongs to a course and carries a `teacher` (defaults to whoever creates it;
any explicit `teacher_id` must hold teacher+ — a student cannot teach), an
optional `topic`, a required `starts_at`, and an optional `ends_at` (when both
are set, `ends_at` must not precede `starts_at`; neither may be set in the
past). Sessions are created, edited,
and deleted under course-management rights, exactly like exams; session lists
are ordered by `starts_at` (a timetable, not a creation log).

**Roll call** (`/sessions/{id}/attendance`) deliberately differs from event
attendance: students never mark themselves. The session's teacher or a course
manager marks **enrolled** students (an unenrolled target is a `400`), and the
**session teacher's own** presence row can only be written or removed by
manager+ — staff presence is management's call, so a teacher can't declare
themselves present. Listing the roll call follows the same rights as taking
it; a student reads their own tallies through the attendance report instead.
Re-marking overwrites: one row per session+user, by construction. Deleting a
session (or its course) cascades its roll-call rows.

The **work log** (`/work`) is the staff timesheet, for teachers and above.
`POST /work/check-in` opens a stint and `POST /work/check-out` closes it, both
stamped by the **server clock** — a request never carries an instant, so a
wrong device clock (or a crafted request) can't forge presence. At most one
open stint per user holds atomically: double check-ins and check-outs without
an open stint answer `409`, and a reconnecting double-click can't reset a
running clock. `GET /work/me` lists your stints newest-first (the open one has
`check_out: null`, closed ones a convenience `duration_ms`); manager+ reads
anyone's log (`GET /work/{user}`), corrects a **closed** stint's instants
(`PATCH /work/entries/{id}`, `409` while open — check out or delete instead),
and deletes entries.

**Attendance reports** mirror the marks report: `GET /attendance/me` for any
logged-in user, `GET /attendance/{user}` for teacher+ — narrowed to the
courses the caller manages (manager+ sees every course; event tallies are
school-wide either way). The report tallies event attendance and lesson roll
call separately, plus a per-course breakdown:

```json
{
  "user": "01J…",
  "events":   { "present": 4, "absent": 1, "late": 0, "excused": 1, "total": 6, "rate": 0.8 },
  "sessions": { "present": 9, "absent": 2, "late": 1, "excused": 0, "total": 12, "rate": 0.8333 },
  "courses": [ { "course": { "id": "01J…", "title": "algebra", … }, "counts": { … } } ]
}
```

`rate = (present + late) / (present + absent + late)`: arriving late still
counts as attending, and an excused absence counts against no one (`rate` is
`null` when every row is excused, or there are none). Per-course blocks appear
for every course the user has roll-call rows in — attendance is a historical
record, so unenrolling hides marks from the marks report but never hides an
absence.

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

# courses + weighted marks (as a teacher)
# find the student to enroll (here: a registered user "veli"): fragment
# search over username/name, role-narrowed (teacher+)
SID=$(curl -s -b $JAR "$BASE/users/search?q=vel&role=student" \
  | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
CO=$(curl -s -b $JAR $BASE/courses -H 'content-type: application/json' \
  -d '{"title":"algebra"}' | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
curl -s -b $JAR $BASE/courses/$CO/enrollments -H 'content-type: application/json' \
  -d "{\"user_id\":\"$SID\"}"
EX=$(curl -s -b $JAR $BASE/courses/$CO/exams -H 'content-type: application/json' \
  -d '{"title":"midterm","kind":"midterm","weight":3}' | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
curl -s -b $JAR $BASE/exams/$EX/results -H 'content-type: application/json' \
  -d "{\"mark\":90,\"user_id\":\"$SID\"}"
curl -s -b $JAR $BASE/exams/$EX/statistics

# lesson sessions + roll call (course manager creates; the session's teacher
# or a course manager marks enrolled students)
SE=$(curl -s -b $JAR $BASE/courses/$CO/sessions -H 'content-type: application/json' \
  -d '{"topic":"limits","starts_at":1900000000000}' | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
curl -s -b $JAR $BASE/sessions/$SE/attendance -H 'content-type: application/json' \
  -d "{\"status\":\"present\",\"user_id\":\"$SID\"}"

# staff work log (instants are server-stamped)
curl -s -b $JAR -X POST $BASE/work/check-in
curl -s -b $JAR -X POST $BASE/work/check-out
curl -s -b $JAR $BASE/work/me

# ...and as the student:
curl -s -b $STUDENT_JAR $BASE/marks/me
curl -s -b $STUDENT_JAR $BASE/attendance/me
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
    course_session.rs CourseSessionId · SessionTopic · CourseSession (a course's lesson)
    session_attendance.rs SessionAttendanceId · SessionAttendance (roll call; one row per session+user)
    work_entry.rs  WorkEntryId · WorkEntry (staff stint; one open per user by construction)
    enrollment.rs  EnrollmentId · Enrollment (one row per course+user)
    exam.rs        ExamId · ExamTitle · ExamDescription · ExamKind · ExamWeight ·
                   ExamMode · ExamDuration · ExamSchedule · Exam (belongs to a course)
    exam_attempt.rs ExamAttemptId · AttemptStatus · ExamAttempt (numbered sittings per exam+user, metered by max_attempts)
    exam_question.rs ExamQuestionId · QuestionText · QuestionKind · QuestionPoints ·
                   ChoiceText · QuestionSpec · ExamQuestion (choice|text, per exam)
    exam_answer.rs ExamAnswerId · AnswerText · ExamAnswer (one row per question+user) ·
                   auto_score (choice-question suggestion)
    exam_result.rs ExamResultId · Mark · ExamResult (one row per exam+user)
  web/             axum layer: DTOs (serde + OpenAPI schemas) + handlers +
                   auth extractors
    extractor.rs   CurrentUser · RequireTeacher · RequireManager · RequireAdmin
    dto.rs         shared UserResponse · CourseResponse · ExamResponse · SessionResponse schemas
    exam_ws.rs     the student exam-room WebSocket (state ticks, autosave, finish)
    auth.rs  users.rs  notes.rs  events.rs  courses.rs  sessions.rs  exams.rs
    marks.rs  work.rs  attendance.rs
```

Tests: `cargo test` — unit (in-source), integration (`tower::oneshot` + in-memory
db), rate-limit (both tiers, proxy-header and peer-address keying, shipped
limits over every route), e2e (real TCP + reqwest cookie jar), persistence
(tempfile file engine, including close + reopen).
