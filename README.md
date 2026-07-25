# hezarfen_backend

Note, attendance, course + weighted exam mark backend. **Rust (edition 2024) · axum · SurrealDB 3 (server, WebSocket) · tokio.**

Session-cookie auth with five hierarchical roles (`parent < student < teacher
< manager < admin`). A **`parent`** observes and changes nothing: admins tie
students to a parent account, and the parent reads those students' mark,
attendance, pomodoro, and homework reports — that's the whole role. Notes are per-user and carry **file attachments** (PDFs, documents,
…): blobs live on disk next to the database, metadata in the database, and the
per-file size cap is school policy in settings (`max_file_bytes`, default
5 MiB). Any two users can **message** each other, mail-style — subject +
body into the recipient's inbox, each side filing its own copy through
archive/trash with a read flag the sender sees as a receipt (the one place a
`parent` writes). Attendance is event + attendees: create an event with an **audience**
(the whole school, one role, a course's enrollment, or a **registration**
signup list — omit for school-wide), then teachers mark the expected attendees
present / absent / late / excused (students never self-mark), and a **roster
report** shows who was expected and who missed. Registration lists fill seat
by seat: teachers register students (never the other way round), staff
register only themselves, an optional `capacity` caps the seats, and the list
closes the moment the event starts (or, for an event with only an end time —
a pure signup deadline — the moment that end passes). Every event stays visible to everyone —
the audience is a roster, not a wall. Marks are course-shaped
(Google Classroom style): a teacher creates a course — kind **`course`** (a
regular class), **`study`** (a supervised study session — *etüt*), or
**`club`** (a student club — *kulüp*; same behavior, different label),
optionally capped by a `capacity` that refuses new enrolls once the roster is
full — lays out its **subjects** (curriculum topics —
every exam question must be tagged with one of its course's subjects, so
results can later be read per topic), enrolls students, adds
exams inside it, and grades — enrolling, sitting exams, roll call, and marks are
all student-only, staff never take part. A course is run by whoever created it
plus any teachers a **manager assigns** to it (`POST /courses/{id}/teachers`):
an assigned teacher manages everything inside the course — exams, sessions,
subjects, roster, grading — but can't delete it or change who else teaches it,
and a demotion below `teacher` sweeps their assignments away. Students read a per-course weighted average and
an overall average from their mark report — each exam weighted by its **kind**
(midterms can count double, orals once: weights are set per kind in settings,
not per exam). Exams run **sync** (one
fixed window), **async** (start anytime inside the window, with a personal
time budget), or **open** (sit anytime, optionally timed per attempt);
students *sit* them via attempts — retakes metered by a per-exam limit
(`0` = unlimited), each sitting keeping its own answers, drawings, and mark
so staff can read a student's full attempt history, leaving the exam room
governed by a teacher-controlled rejoin door. Writing an exam takes a while, so it can be saved as a
**draft** — invisible to students, unsittable, ungradable — and published
when it's ready. Questions can carry **images**: any question may hold one
illustration (a map above the prompt), and each option of a choice question
may be a picture of its own (pick the right city off the map) — raster
uploads capped by the same `max_file_bytes` policy as note files. Good
questions get reused, so a teacher+ can bank one in a school-wide **question
bank** — detached templates (with their own images) that any teacher copies
into a fresh exam question across years, editable and deletable only by
whoever saved them. Teachers
watch attendance, per-student remaining time,
sittings, walk-outs, no-shows (`absent` once the window closes), submissions,
and marks land live on a monitor endpoint (snapshot or SSE stream). Courses also hand out
**homework**: assigned to the whole class or a named subset (the unnamed never
even see it), tagged with a course subject, due by a required future `due_at`
— students hand in text and/or **files of any type** (same size cap, 10 per
submission, served back only as forced downloads), editable until the teacher
grades a status (`done`/`incomplete`/`missing`) with an optional 0–100 mark
(grading freezes the hand-in until the grade is removed); lateness is computed
from two stamps (first hand-in vs last touch), never stored, and homework
marks stay out of the weighted `/marks` averages. Courses also carry **lesson
sessions** with teacher-taken roll call (students never self-mark a lesson),
staff clock in/out on a server-stamped **work log**, students track study time
with a server-stamped **pomodoro log** (the timer runs in the frontend; the
backend records the focus stints, and teachers can read any student's log),
and every user has an
**attendance report** (event + per-course lesson tallies with rates).
A school-wide **question pool** runs on moderation: a student asks a question
(optionally attaching one photo of the problem — raster only, same
`max_file_bytes` cap), a teacher+ **approves** it into the pool (or deletes
it — rejection is deletion, there is no rejected state), and every approved
question is readable by the whole school with anyone free to offer
**solutions** under it (parents stay out) — text plus an optional photo of
the worked steps, both editable by the solution's author anytime; pending
questions show only to their asker and to teacher+, and approval freezes the
content so nothing unmoderated ever reaches the pool.
Teachers keep an **appointment calendar**: a teacher+ publishes availability
slots (one-off, or repeating weekly up to an `until` date as a series that
deletes as one), and students and parents book a slot with a reason — the
booking lands `pending` until the teacher approves it, rejects it, or
counter-proposes another time (which sends it back to `pending` for the
requester to accept or decline); the **requester** may cancel until the meeting
starts — a teacher ends a booking by rejecting it, or by counter-proposing and
then rejecting an approved one — one live booking holds a slot, a teacher's own
published windows may not overlap each other, and no approved meeting may
overlap another for the teacher or the requester (see "Appointments").
School-varying policy is data, not code: exam kinds (each with its weight in
course averages), attendance statuses, grade-display bands, the note-file
size limit, and the chatbot's limits live in an editable **settings** singleton, and academic **terms**
are plain rows courses can link to (see "Per-school policy"). Each account
also carries its own **UI preferences** — theme (`light`/`dark`) and language
(`tr`/`en`) — self-managed, admin-editable for anyone, `null` until chosen so
the client can fall back to the device preference.
The **AI features live in separate projects**, so the backend also opens a
QUIC **AI bridge** (`AI_QUIC_ADDR`, off by default): AI services dial in,
register the capabilities they serve, and each request rides its own QUIC
stream on that one connection — no correlation ids, no head-of-line blocking.
The bridge's certificate is published at `GET /ai/certificate` so a service can
pin it before dialling (see "AI bridge (QUIC)").
The first thing riding that bridge is the **chatbot**: every signed-in user
(`parent` included) keeps private threads with an AI service, free-form —
no rule tables, no canned answers, the backend only owns auth, limits,
persistence and the payload format. Sending is asynchronous because an
inference outlives a request: the turn plus an empty `pending` answer are
written *first*, the call goes out after, so a reload never loses an answer;
the client then polls the message or reads it off an SSE stream (see
"Chatbot").

Every field is a validated newtype (`Username(String)`, `NoteTitle(String)`, …)
constructed only after its restrictions pass — invalid input can't be
represented. Those same types derive `surrealdb::types::SurrealValue`, so one
typed value flows from HTTP request into the `SCHEMAFULL` database. Record ids
are ULIDs (time-sortable).

## Run

```sh
surreal start --user root --pass root surrealkv:./data/hezarfen.db   # the DB server
cp .env.example .env      # optional
cargo run
```

Boots on `http://127.0.0.1:8080`, talking to the SurrealDB server at
`DB_URL` (default `ws://127.0.0.1:8000`, root credentials via
`DB_USER`/`DB_PASS`) and storing uploaded note files in
`./data/files/` (`FILES_PATH`, created at startup). Interactive API docs
(Swagger UI) are served at `/swagger`, the raw OpenAPI spec at
`/api-docs/openapi.json`.

## Run in a container (podman)

```sh
podman compose up -d --build   # build + start, http://127.0.0.1:8080
podman compose logs -f backend
podman compose down            # stop (data survives in the volume)
```

The `Containerfile` is a two-stage build (Rust builder with cargo cache
mounts, `debian:trixie-slim` runtime, non-root user). Two services: the
`surrealdb` server (official image, surrealkv storage) and the backend, which
waits for the server's healthcheck and connects over `ws://surrealdb:8000`.
Each has its own named volume — `surreal-data` holds the database,
`hezarfen-data` the uploaded note files (`/data/files`). `HOST` is forced to `0.0.0.0` inside the
container so the published port works. Production knobs (`COOKIE_SECURE`,
`CORS_ALLOWED_ORIGINS`, rate limits, `TRUST_PROXY`) are commented in
`compose.yaml` — uncomment as needed. Leaving `CORS_ALLOWED_ORIGINS` unset
means dev mirror mode without credentials; a cookie-using browser frontend
must be allowlisted explicitly. Works with `docker compose` too.

The backend survives the database going away, at boot and at runtime.

At boot it retries the connection (1s doubling to 5s) until the server
answers, rather than exiting. Exiting looks tidier but is worse: the container
runtime restarts the process, it fails again in milliseconds, and a few
seconds of startup skew burns the whole restart budget and leaves the backend
down for good.

At runtime a keepalive query every 5s doubles as a liveness probe. This
matters more than it sounds: a query issued while the socket is down does
*not* fail — the SDK's reconnect loop stops draining its request queue, so the
query parks until the database returns and only *then* runs. Left alone, a
request waits out the entire outage and any write it carries lands long after
the caller gave up.

So while the probe says the socket is down, requests are refused at the edge
with `503` and `Retry-After: 1`, before they can reach the database. Nothing
is queued, which is what makes that retry safe. Requests that slip through in
the window between the socket dying and the probe noticing are capped at 30s
and answer `503` *without* `Retry-After`, with a message saying the write may
or may not have applied — they were already queued, so retrying them could
apply the same write twice. WebSocket and SSE routes are unaffected: both
return their response immediately and stream afterwards.

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

**Register never reveals whether a username is taken.** `POST /auth/register`
answers `201` either way, with a body shaped exactly like a fresh
registration's — same status, same fields, and a throwaway `id` that resolves
to no row. This is deliberate: the route is unauthenticated, so a `409` (or a
faster reply) would let anyone enumerate the school's users. The password is
hashed *before* the availability check so both outcomes cost the same ~33ms.
The accepted cost: a caller who collides with an existing account gets no
distinct error and simply cannot log in with that password — they pick another
name. Do not "fix" this back to a `409`.

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

Sending a chatbot message adds a third tier, and it is keyed by **user id**,
not by IP (`RATE_LIMIT_CHATBOT_PER_MINUTE`, default 20), because an inference
costs the school real money and a shared-IP classroom must not spend one
student's budget on another's. `0` disables it like the other two. It is
charged before anything is written, so a `429` leaves no trace (see
"Chatbot").

Behind a reverse proxy every connection carries the proxy's address, so also
set `TRUST_PROXY=true` to key clients by the rightmost `X-Forwarded-For` entry
(the one the proxy itself appends). Leave it off when clients reach the server
directly — the header is client-forged in that case.

## CORS

Cookie-authenticated APIs can't use a wildcard `Access-Control-Allow-Origin`
(browsers refuse it on credentialed requests), so origins listed in the
comma-separated `CORS_ALLOWED_ORIGINS` allowlist are echoed back with
`Access-Control-Allow-Credentials: true`. When the allowlist is unset the
server reflects the request's `Origin` — dev-friendly — but **without**
credentials: mirroring with credentials would let any website ride a
visitor's session cookie. A browser frontend that needs the session cookie
cross-origin must therefore be allowlisted explicitly.

## Roles & access control

Every user has one of five roles, ranked lowest to highest:

```
parent  <  student  <  teacher  <  manager  <  admin
```

The check is **hierarchical** — a higher role satisfies any lower requirement
(an admin can do anything a teacher can). New accounts always register as
`student`; a role read is re-checked on every request, so a role change takes
effect on the user's very next call (no re-login).

`parent` is the read-only observer at the bottom of the ladder: an admin ties
any number of students to a parent account (`POST /users/{id}/students`), and
the tie is the parent's whole power — they list their students
(`GET /users/me/students`) and read each one's mark, attendance, pomodoro,
and homework reports in full. Student-only checks are exact (`role == student`), so a
parent can never enroll, sit an exam, be graded, or land on a roll call; and
sitting below every staff bar, they can't touch anything else either — except
messages, which any role sends and receives (that's how a parent reaches a
teacher). A role
change off either end of a tie (the parent stops being a `parent`, the
student stops being a `student`) drops the tie, exactly like promotion drops
course enrollments.

| Action                                   | Minimum role | Notes                                         |
|------------------------------------------|--------------|-----------------------------------------------|
| Register / login / view own account      | (any)        | Registration always creates a `student`       |
| View events, own notes; CRUD notes + their files | student | Everyone can read events and keep notes; note files (upload/download) are walled per owner like the notes themselves |
| Send / read / file / delete messages     | (any)        | One-to-one, any user to any user (`parent` included — the role's one write); each party only ever touches their own copy |
| Mark event attendance; remove attendance rows | teacher | Only users in the event's **audience** can be marked; students never mark — a teacher+ may mark anyone expected, themselves included |
| Create events                            | teacher      | The audience (school / role / course / registration) is set at creation and editable later |
| Register users onto a registration event | teacher      | Teachers place **students** (students never register themselves) and take a seat for **themselves** — never for another staff member. Unregistering mirrors the same rule |
| List an event's attendance or its roster report | teacher | Students read their own tallies via the attendance report |
| Edit / delete an event                   | teacher      | Only the **creator**, or a `manager`+ for any event |
| View a course's sessions                 | student      | Only inside **visible** courses: enrolled, creator, assigned teacher, or `manager`+ |
| List a session's roll call               | teacher      | The **session's teacher**, or anyone with course-management rights |
| Create / edit / delete a course session  | teacher      | Course-management rights (course creator, an assigned teacher, or `manager`+) |
| Take a session's roll call (mark/remove **enrolled students**) | teacher | The **session's teacher**, or anyone with course-management rights; only students sit on a roster |
| Mark / remove the **session teacher's** presence row | manager | Staff presence is management's call — the teacher can't self-mark |
| Work check-in / check-out; view **own** work log | teacher | Instants are server-stamped, never client-supplied |
| View / correct / delete **any** staff work log entry | manager | Corrections only on closed entries |
| Start / finish a pomodoro focus session; view **own** pomodoro log | student | **Students only** start; instants server-stamped; starting discards a dangling unfinished session |
| View **any** user's pomodoro log          | teacher      | Study oversight — same shape as `/pomodoro/me`, incl. the unpaged `total_focus_ms`; a `parent` reads their linked students' |
| Ask into the school question pool        | student      | **Students only** (exact); born `pending` — visible to the asker + teacher+ only; the asker may attach/replace/remove one photo while pending |
| Read the pool; offer / edit / withdraw own solutions | student | Every `approved` question is school-wide (parents stay out); a solution's author edits its body and photo anytime (solutions never freeze) and deletes it, teacher+ delete any |
| Approve a pending pool question; delete any question or solution | teacher | Approval publishes school-wide and **freezes** the content; rejection = deletion — moderation never edits, so teacher+ cannot rewrite a solution |
| Read the school **question bank**; save / instantiate bank questions | teacher | Every bank row is school-wide (reads + copy into an exam); saving one makes the caller its owner |
| Edit / delete a bank question (and its images) | teacher | **Owner only** (admin bypass); bank rows carry no exam link, so they never freeze |
| Read **own** attendance report           | student      |                                               |
| Read another user's attendance report    | teacher      | Narrowed to the caller's managed courses; `manager`+ sees all; a `parent` sees a linked student's in full |
| View **visible** courses/exams and a course's subjects; read **own** result, courses, mark report | student | Visible = enrolled (teachers: + created + assigned; `manager`+: all); exam **drafts** show only to the course's managers |
| Sit a sittable exam (`sync`/`async`/`open`): start / resume / retake / read / submit **own** attempt | student | **Students only** — staff never sit; must be enrolled; window (where one exists) and `max_attempts` enforced by the server |
| Answer questions inside **own** attempt (REST autosave or the exam-room WebSocket) | student | **Students only**; attempt must be `in_progress`; deadline judged by the server clock; blocked after leaving the room while `allow_rejoin` is off |
| Author an exam's questions (add/edit/delete, incl. question + option images) | teacher | Course-management rights; frozen once anyone has an attempt |
| Read a question list (with `correct`) or a student's answer sheet | teacher | Course-management rights — one teacher can't read another's answer key |
| Watch an exam's live monitor (poll the snapshot) | teacher | Course-management rights |
| Create courses                           | teacher      | The creator manages the course, and owns it for good |
| Assign / unassign a course's teachers    | manager      | Staffing is the office's call — a course's own creator cannot hand rights to peers; the assignee must be `teacher`+ |
| Delete a course                          | teacher      | Only the **creator**, or a `manager`+ — an assigned teacher runs the course but doesn't own it |
| Manage inside a course: edit it, enroll/unenroll **students**, add/edit/delete its exams and **subjects**, grade, remove results | teacher | The **course creator**, a teacher **assigned** to it, or a `manager`+ for any course; only students can be enrolled; a subject still referenced by exam questions or homework won't delete (`409`) |
| View a course's roster, an exam's result list / statistics | teacher | Course-management rights |
| Read another user's mark report          | teacher      | Narrowed to the caller's managed courses; `manager`+ sees all; a `parent` sees a linked student's in full |
| Grade students                           | teacher      | Target must be an **enrolled student**; grading never targets oneself |
| Assign / edit / delete homework; grade it; read its roster | teacher | Course-management rights; an `assigned` subset (≤ 200 enrolled students) can't be narrowed over existing work (`409`); a grade (`done`/`incomplete`/`missing` + optional mark) freezes the submission until removed |
| Submit **own** homework (text + files); read own submission and grade | student | **Students only** — staff never submit; enrolled + in the audience; editable until graded; ≤ 10 files of any type, each ≤ `max_file_bytes`, always downloaded as attachments |
| Read another user's homework report      | teacher      | Narrowed to the caller's managed courses; `manager`+ sees all; a `parent` sees a linked student's in full — statuses/marks/flags, never files |
| Edit **own** personal info (name, surname, email, phone, birth date) | student | Every account carries the same optional info fields |
| Edit **own** UI preferences (theme, language) | student | `null` until chosen — the client then follows the device preference |
| List **own** linked students             | parent       | Read-only: the list plus each student's mark/attendance/pomodoro/homework reports — a parent changes nothing, anywhere |
| Read the school settings and the term list | student | Clients need them to render kind/status pickers, grades, and the calendar |
| Edit school settings; create / edit / delete terms | manager | School policy (exam kinds, attendance statuses, grade bands, the note-file size limit) and the academic calendar are management's call |
| List users; look up one user; change a user's role; edit **any** user's personal info or UI preferences; tie/untie students to a `parent` account | admin | An admin cannot change **their own** role |

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
sole admin locked themselves out): run

```surql
UPDATE user SET role = 'admin' WHERE username = 'ada';
```

against the SurrealDB server — e.g.
`surreal sql --conn ws://127.0.0.1:8000 --user root --pass root --ns hezarfen --db hezarfen`
(in the container setup, `podman exec -it hezarfen-surrealdb /surreal sql ...`).

## Endpoints

`Auth` is the minimum role; `no` means no session required, `student` means any
logged-in user.

A course is a regular class (kind `course`, the default), an *etüt* (kind
`study` — a supervised study session), or a *kulüp* (kind `club` — a student
club). The three kinds behave identically everywhere — enrollment, exams,
sessions, marks — the kind is a label the UI renders differently, settable at
creation and editable later. A course may also carry a `capacity`: once the
roster reaches it, new enrolls are refused with `409` (members already on the
roster are unaffected, even if the cap is later lowered below them; `null`
lifts the cap).

**Who runs a course.** Its **creator** owns it for good — only they (or a
manager+) may delete it. On top of that, a manager can **assign** any
`teacher`+ to the course with `POST /courses/{id}/teachers` (`{user_id}`,
idempotent) and drop them again with `DELETE /courses/{id}/teachers/{user}`.
An assigned teacher gets full **course-management rights** — edit the course,
enroll and unenroll students, add exams, sessions and subjects, grade, take
roll call — but *not* the two owner powers: they cannot delete the course, and
they cannot change who else teaches it. Staffing is deliberately the office's
call, so a course's own creator cannot hand rights to their peers; only
manager+ may touch the list. Every course response carries its `teachers`
array alongside `creator`, and a user demoted below `teacher` is swept off
every course they were assigned to (the mirror of promotion dropping
enrollments).

Course data is walled per course. A course, its exams, its sessions, and its
subjects are
**visible** only to its enrolled users, its creator, its assigned teachers,
and manager+ — a student
sees just the classes they were added to, and the `/courses` / `/exams`
catalogs are filtered accordingly. Teacher-level reads *inside* a course
(roster, results, statistics, the question list, answer sheets, the live
monitor) additionally need **course-management rights** (creator, an assigned
teacher, or manager+):
one teacher cannot look into another teacher's course, and the per-user
marks/attendance reports narrow to the courses the caller manages.

**Paging.** Every list endpoint below (the rows tagged **· paged**) accepts
`?limit=&offset=` and returns a `{ items, total, limit, offset }` envelope
rather than a bare array. `total` is the full row count *before* the window, so
a frontend can show "100 of 256" and page with `offset`. Paging is **opt-in**:
omit `limit` and you get every remaining row (the echoed `limit` is then
`null`), so a caller that sends no parameters still receives the whole list —
nothing silently truncates. `limit` must be `1`–`500`, `offset` defaults to `0`
and must be ≥ 0 (bad values are `400`), and an `offset` at or past the end
returns an empty `items` (not an error). Deliberately **not** paged — these keep
their existing shapes: the student exam-room reads
`/exams/{id}/attempt/questions` and `/attempt/answers`, the `/marks` and
`/attendance` report objects, the `/exams/{id}/live` monitor, and `/settings`.

| Method | Path                             | Auth    | Description                     |
|--------|----------------------------------|---------|---------------------------------|
| GET    | `/health`                        | no      | Liveness check                  |
| GET    | `/`                              | no      | Same as `/health`               |
| GET    | `/time`                          | no      | Server clock: `{now}` UTC unix-millis (frontend sync) |
| GET    | `/swagger`                       | no      | Interactive API docs (Swagger UI) |
| GET    | `/api-docs/openapi.json`         | no      | Raw OpenAPI 3 spec              |
| GET    | `/ai/certificate`                | no      | The AI bridge's certificate (PEM + sha256) for a service to pin; `404` when the bridge is off |
| POST   | `/auth/register`                 | no      | `{username, password}` (new users are `student`); always `201`, even if the name was taken — see Auth model |
| POST   | `/auth/login`                    | no      | `{username, password}` -> cookie|
| POST   | `/auth/logout`                   | no      | Clear session (no-op if none)   |
| GET    | `/auth/me`                       | student | Current user (incl. `role` and personal info) |
| PATCH  | `/users/me`                      | student | Update own personal info (see below) |
| PATCH  | `/users/me/preferences`          | student | `{theme?, language?}` — own UI preferences (see below) |
| GET    | `/users/me/students`             | parent  | The caller's linked students (refs, sorted by username) · paged |
| GET    | `/users/search`                  | teacher | `?q=<fragment>&role=<role?>` — find users by username/name fragment (pickers); refs only, no contact info · paged |
| GET    | `/users`                         | admin   | List all users · paged          |
| GET    | `/users/{id}`                    | admin   | Get one user                    |
| PATCH  | `/users/{id}/role`               | admin   | `{role}` — set a user's role; promotion out of `student` drops the user's course enrollments (only students enroll) |
| PATCH  | `/users/{id}/profile`            | admin   | Update any user's personal info |
| PATCH  | `/users/{id}/preferences`        | admin   | Update any user's UI preferences |
| POST   | `/users/{id}/students`           | admin   | `{user_id}` — tie a **student** to a **parent** account `{id}` (idempotent); the tie is the parent's read grant |
| GET    | `/users/{id}/students`           | admin   | List a parent's linked students · paged |
| DELETE | `/users/{id}/students/{student}` | admin   | Untie a student from a parent (student data untouched) |
| POST   | `/notes`                         | student | `{title, content?}`             |
| GET    | `/notes`                         | student | List own notes · paged          |
| GET    | `/notes/{id}`                    | student | Get own note                    |
| PATCH  | `/notes/{id}`                    | student | `{title?, content?}`            |
| DELETE | `/notes/{id}`                    | student | Delete own note (its files go with it) |
| POST   | `/notes/{id}/files`              | student | Attach a file: `multipart/form-data`, one `file` part (`filename` required) — ≤ the school's `max_file_bytes`, ≤ 10 files per note |
| GET    | `/notes/{id}/files`              | student | List a note's files (metadata: `{id, name, content_type, size}`) · paged |
| GET    | `/notes/{id}/files/{file_id}`    | student | Download the bytes (original filename + content type in the headers) |
| DELETE | `/notes/{id}/files/{file_id}`    | student | Delete one file                 |
| POST   | `/messages`                      | student | `{recipient_id, subject, body?, label?}` — send to any user (every role incl. `parent`; not yourself); `label` is a free-text badge tag |
| GET    | `/messages`                      | student | `?folder=inbox\|sent\|archive\|trash` (default `inbox`) `&read=` — the caller's folder, newest first; `?folder=inbox&read=false&limit=1` → `total` is the unread badge · paged |
| PATCH  | `/messages/{id}`                 | student | `{read?, folder?}` — read flag (recipient only) and/or move **own copy** (recipient: `inbox`/`archive`/`trash`; sender: `sent`/`archive`/`trash`) |
| DELETE | `/messages/{id}`                 | student | Permanently delete **own copy** — only from the trash (`409` elsewhere); the row vanishes once both sides deleted |
| POST   | `/events`                        | teacher | `{title, description?, audience?, starts_at?, ends_at?}` — `audience` defaults to school-wide |
| GET    | `/events`                        | student | List all events · paged         |
| GET    | `/events/{id}`                   | student | Get event                       |
| PATCH  | `/events/{id}`                   | teacher | Edit event (creator, or manager+ for any); a sent `audience` replaces the old one wholesale |
| DELETE | `/events/{id}`                   | teacher | Delete event (creator, or manager+ for any) |
| POST   | `/events/{id}/attendance`        | teacher | `{status, user_id?}` — mark someone in the event's **audience** (the caller when `user_id` omitted); students never mark |
| GET    | `/events/{id}/attendance`        | teacher | List recorded attendance for event (students read their own tallies via `/attendance/me`) · paged |
| GET    | `/events/{id}/roster`            | teacher | Who-missed report: every expected attendee with their status (`null` = never marked) + `marked_by` · paged |
| DELETE | `/events/{id}/attendance/{user}` | teacher | Remove a user's attendance      |
| POST   | `/events/{id}/register`          | teacher | `{user_id?}` — seat a **student** (or yourself when omitted) on a registration event's signup list; idempotent, `409` once full or started |
| DELETE | `/events/{id}/register/{user}`   | teacher | Free a seat (same self-or-student rule); `409` once the event started |
| POST   | `/appointments/slots`            | teacher | `{starts_at, ends_at, note?, repeat_weekly?, until?}` — publish availability on **own** calendar; always answers an **array** (one element for a one-off, one per weekly occurrence, ≤ 52, sharing a `series`); `400` if a weekly shift would run off the end of time; `409` if the window overlaps one the caller already published (half-open, so back-to-back is fine) — a weekly publish is all-or-nothing |
| GET    | `/appointments/slots`            | student | Teacher+: own calendar (past included). Everyone else: the bookable calendar — future slots only, demoted teachers' slots left out · paged |
| DELETE | `/appointments/slots/{id}`       | teacher | Withdraw one slot (its teacher, or manager+); `409` while a pending/approved booking sits on it |
| DELETE | `/appointments/slots/series/{series}` | teacher | Withdraw a whole recurring publish (same rights); `409` if **any** occurrence has a live booking |
| POST   | `/appointments`                  | student | `{slot, reason}` — book a slot; **students and parents only** (a parent books for themselves); lands `pending`, `409` if the slot's window has already started, the slot is taken, the caller is busy at that time, or its teacher is no longer staff |
| GET    | `/appointments`                  | student | Teacher+: bookings on own slots (the request inbox). Everyone else: own requests · paged |
| PATCH  | `/appointments/{id}/approve`     | teacher | Confirm a pending booking (slot's teacher, or manager+); `409` when settled, when the effective window has already started, or when the time collides with another approved meeting of either side |
| PATCH  | `/appointments/{id}/reject`      | teacher | Turn it down (same rights); the slot frees up |
| PATCH  | `/appointments/{id}/cancel`      | student | `{reason?}` — call it off; **the requester only** (`403` for anyone else, the slot's teacher and manager+ included — they reject, or reschedule then reject); `409` once settled or the meeting has started |
| PATCH  | `/appointments/{id}/reschedule`  | teacher | `{starts_at, ends_at}` — counter-propose another time (same rights); the booking goes back to `pending`; `409` if the proposed window has already started |
| PATCH  | `/appointments/{id}/reschedule/accept` | student | Requester only: approval at the proposed time (the overlap guard runs again) |
| PATCH  | `/appointments/{id}/reschedule/decline` | student | Requester only: refuse the proposal — this **cancels** the booking, so the cancel deadline applies (`409` once the meeting's window has started) |
| POST   | `/courses`                       | teacher | `{title, description?, kind?, term_id?, capacity?}` — `kind` is `course` (default), `study` (etüt), or `club` (kulüp); `capacity` caps the roster (creator manages it) |
| GET    | `/courses`                       | student | The caller's visible courses: created + enrolled (manager+: all) · paged |
| GET    | `/courses/me`                    | student | The caller's **enrolled** courses · paged |
| GET    | `/courses/{id}`                  | student | Get course (enrolled, creator, assigned teacher, or manager+) |
| PATCH  | `/courses/{id}`                  | teacher | Edit course incl. `kind` and `capacity` (`null` lifts the cap) (course manager) |
| DELETE | `/courses/{id}`                  | teacher | Delete course + its exams, results, subjects, sessions, and homework (submissions, files, and grades included) (creator, or manager+ — **not** an assigned teacher; `409` while anyone is still enrolled) |
| POST   | `/courses/{id}/teachers`         | manager | `{user_id}` — assign a **teacher+** to run the course (idempotent; returns the course) |
| DELETE | `/courses/{id}/teachers/{user}`  | manager | Unassign a teacher (`404` if they weren't assigned) |
| POST   | `/courses/{id}/enrollments`      | teacher | `{user_id}` — enroll a **student** (idempotent upsert; course manager; only students can be enrolled; `409` once a capped course is full) |
| GET    | `/courses/{id}/enrollments`      | teacher | List the course roster (course manager) · paged |
| DELETE | `/courses/{id}/enrollments/{user}` | teacher | Unenroll (keeps recorded results; course manager) |
| POST   | `/courses/{id}/sessions`         | teacher | `{topic?, teacher_id?, starts_at, ends_at?}` — add a lesson (course manager; teacher defaults to the caller) |
| GET    | `/courses/{id}/sessions`         | student | List the course's sessions, most recent first (enrolled, creator, assigned teacher, or manager+) · paged |
| POST   | `/courses/{id}/subjects`         | teacher | `{name, description?}` — add a curriculum subject (course manager) |
| GET    | `/courses/{id}/subjects`         | student | List the course's subjects, creation order (enrolled, creator, assigned teacher, or manager+) · paged |
| GET    | `/subjects/{id}`                 | student | Get subject (enrolled, creator, or manager+) |
| PATCH  | `/subjects/{id}`                 | teacher | Edit a subject's name/description (course manager; its course is fixed) |
| DELETE | `/subjects/{id}`                 | teacher | Delete a subject (course manager); `409` while exam questions or homework reference it |
| GET    | `/sessions/{id}`                 | student | Get session (enrolled, session teacher, or course manager) |
| PATCH  | `/sessions/{id}`                 | teacher | Edit session (course manager; `null` clears `ends_at`) |
| DELETE | `/sessions/{id}`                 | teacher | Delete session + its roll call (course manager) |
| POST   | `/sessions/{id}/attendance`      | teacher | `{status, user_id}` — roll call: session teacher/course manager mark **enrolled students** (students only); the teacher's own row needs manager+ |
| GET    | `/sessions/{id}/attendance`      | teacher | List the session's roll call (session teacher or course manager) · paged |
| DELETE | `/sessions/{id}/attendance/{user}` | teacher | Remove a roll-call row (same rights as marking) |
| POST   | `/courses/{id}/exams`            | teacher | `{title, description?, kind, mode?, starts_at?, ends_at?, duration_ms?, max_attempts?, allow_rejoin?, allow_review?, draft?}` — add an exam (course manager); its weight comes from the kind; `draft: true` keeps it hidden while it's written |
| GET    | `/courses/{id}/exams`            | student | List the course's exams (enrolled, creator, assigned teacher, or manager+; drafts appear to course managers only) · paged |
| GET    | `/exams`                         | student | The caller's visible exams: their courses' (manager+: all; drafts of managed courses only) · paged |
| GET    | `/exams/{id}`                    | student | Get exam (enrolled, creator, or manager+; a draft is a `404` for everyone but its course's managers) |
| PATCH  | `/exams/{id}`                    | teacher | Edit exam incl. `kind` (re-weights it), schedule, `max_attempts`, `allow_rejoin`, `allow_review`, `draft` (course manager; `course` immutable, `mode` frozen once attempted, re-drafting frozen once attempts/results exist — the rest stays live) |
| DELETE | `/exams/{id}`                    | teacher | Delete exam + its results, attempts, questions, answers, and question + answer images (course manager) |
| POST   | `/exams/{id}/results`            | teacher | `{mark, user_id}` — grade an **enrolled student** (upsert; course manager; students only; drafts can't be graded, `409`) |
| GET    | `/exams/{id}/results`            | teacher | List every result for the exam (course manager) · paged |
| GET    | `/exams/{id}/result`             | student | The caller's **own** result (`404` until graded) |
| DELETE | `/exams/{id}/results/{user}`     | teacher | Remove a student's result (course manager) |
| GET    | `/exams/{id}/statistics`         | teacher | `{graded, average, min, max}` over the exam's results (course manager) |
| POST   | `/exams/{id}/attempt`            | student | Start (`201`), resume (`200`), or retake (`201`, blank sheet) the caller's attempt — students only; enrolled; window open where one exists; `409` once `max_attempts` is spent |
| GET    | `/exams/{id}/attempt`            | student | Own latest attempt: status, `attempt`/`attempts_used`/`max_attempts`, deadline, `remaining_ms`, `left_at`, mark, progress (`answered`/`question_count`), server `now` |
| POST   | `/exams/{id}/attempt/finish`     | student | Submit the attempt (`409` once the deadline passed); allowed even while locked out of the room |
| POST   | `/exams/{id}/questions`          | teacher | `{subject_id, text, kind, points, choices?, correct?}` — add a question tagged with one of the course's subjects (course manager; frozen once attempted) |
| GET    | `/exams/{id}/questions`          | teacher | The full question list, `correct` included (course manager) · paged |
| PATCH  | `/exams/{id}/questions/{qid}`    | teacher | Edit a question — the kind bundle revalidates as a unit (course manager; frozen once attempted) |
| DELETE | `/exams/{id}/questions/{qid}`    | teacher | Delete a question + its answers (course manager; frozen once attempted) |
| POST   | `/exams/{id}/questions/from-bank/{bid}` | teacher | Instantiate a **bank question** into this exam — copies it to a fresh exam-scoped question (new id, own images + answers; records the template in `source_bank`); body `{subject_id}` retags it against the course's subjects (`400` cross-course; course manager; `409` once attempted) |
| POST   | `/exams/{id}/questions/{qid}/to-bank` | teacher | Save an existing exam question into the school **question bank** — copies it to a detached bank row (new id, own images; records the origin exam in `source_exam`); the source question is untouched (course manager) |
| POST   | `/exams/{id}/questions/{qid}/refresh-from-bank` | teacher | Re-copy the template's **current** content over this question — text, points, kind, choices (with the template's choice ids), `correct`, illustration and option pictures; keeps the question's own id, exam, `subject` and provenance links, and overwrites any local edit. `400` if it never came from the bank (or the template is gone), `404` if the template is not visible to the caller, `409` once attempts exist (course manager) |
| GET    | `/exams/{id}/attempt/questions`  | student | The sitting view: no `correct`, own answers embedded, image metadata included (requires enrollment + an attempt) |
| POST   | `/exams/{id}/questions/{qid}/image` | teacher | Attach/replace the question's illustration: `multipart/form-data`, one `file` part — raster images only (`png`/`jpeg`/`webp`/`gif`), ≤ `max_file_bytes` (course manager; frozen once attempted) |
| GET    | `/exams/{id}/questions/{qid}/image` | student | The illustration bytes (course manager anytime; students enrolled + attempt started) |
| DELETE | `/exams/{id}/questions/{qid}/image` | teacher | Remove the illustration (course manager; frozen once attempted) |
| POST   | `/exams/{id}/questions/{qid}/choices/{choice_id}/image` | teacher | Attach/replace one option's picture, keyed by the `id` carried on that choice (`choice` questions; same form and limits as above) |
| GET    | `/exams/{id}/questions/{qid}/choices/{choice_id}/image` | student | The option picture's bytes (same access as the illustration) |
| DELETE | `/exams/{id}/questions/{qid}/choices/{choice_id}/image` | teacher | Remove one option picture (course manager; frozen once attempted) |
| POST   | `/exams/{id}/attempt/answers`    | student | `{question_id, selected? \| text?}` — autosave one answer while a student, enrolled, and `in_progress` (and not locked out by a closed rejoin door) |
| GET    | `/exams/{id}/attempts/{user}/answers` | teacher | A student's answer sheet: `is_correct` flags + suggested `auto_score` (course manager) |
| POST   | `/exams/{id}/attempt/answers/{qid}/image` | student | Attach/replace the caller's drawn answer to a question: `multipart/form-data`, one `file` part — raster images only (`png`/`jpeg`/`webp`/`gif`), ≤ `max_file_bytes` (own in-progress attempt; students only, enrolled, rejoin door open) |
| GET    | `/exams/{id}/attempt/answers/{qid}/image` | student | The caller's own drawn-answer bytes, served inline (same wall as the sitting view: enrolled + attempt started) |
| DELETE | `/exams/{id}/attempt/answers/{qid}/image` | student | Clear the caller's own drawn answer (own in-progress attempt) |
| GET    | `/exams/{id}/attempts/{user}/answers/{qid}/image` | teacher | A student's drawn-answer bytes, inline, for the grader (course manager) |
| GET    | `/exams/{id}/students/{user}/attempts` | teacher | The sitting numbers a student has left (answers ⋃ marks), ascending — the per-attempt history picker (course manager) |
| GET    | `/exams/{id}/students/{user}/attempts/{seq}/answers` | teacher | One prior sitting's answer sheet: `is_correct` flags + suggested `auto_score` (course manager) |
| GET    | `/exams/{id}/students/{user}/attempts/{seq}/answers/{qid}/image` | teacher | One prior sitting's drawn-answer bytes, inline (course manager) |
| GET    | `/exams/{id}/students/{user}/marks` | teacher | A student's full per-sitting mark history, oldest first — the latest seq is the grade-of-record (course manager) |
| GET    | `/exams/{id}/review/questions`   | student | The exam's question list **with `correct`** — the answer key the caller checks their own sheet against (same review gate) · paged |
| GET    | `/exams/{id}/review/attempts`    | student | The caller's **own** sitting numbers (answers ⋃ marks), ascending — only when `allow_review` is on and the caller has been marked (`403`/`404` otherwise) |
| GET    | `/exams/{id}/review/attempts/{seq}/answers` | student | One of the caller's **own** sittings, judged: `is_correct` flags + suggested `auto_score` (same review gate) |
| GET    | `/exams/{id}/review/attempts/{seq}/answers/{qid}/image` | student | The caller's **own** drawn-answer bytes for a sitting, inline (same review gate) |
| GET    | `/exams/{id}/attempt/ws`         | student | **WebSocket** exam room (students only): state ticks, autosave, finish; entering clears `left_at`, leaving stamps it (see "Taking an exam") |
| GET    | `/exams/{id}/live`               | teacher | Live monitor snapshot: roster × latest attempts × marks + per-student progress/`left_at`/`attempts_used` + counts; no-shows turn `absent` once the window closes (course manager); poll to keep a monitor current |
| GET    | `/bank-questions`                | teacher | The school-wide question bank — `?subject=<id>` filters by origin subject, `?owner=<id>` (or `?owner=me`) by owner; reusable templates any teacher+ can read, each with its image metas (`image`, `choice_images`) · paged |
| POST   | `/bank-questions`                | teacher | `{subject_id, text, kind, points, choices?, correct?}` — save a reusable question template into the bank (owner = caller; `subject_id` is origin metadata, but must exist — `400` otherwise) |
| GET    | `/bank-questions/{bid}`          | teacher | Get one bank question, `correct` + image metas (`image`, `choice_images`) included (any teacher+) |
| PATCH  | `/bank-questions/{bid}`          | teacher | Edit a bank question — the kind bundle revalidates as a unit (**owner only**, admin bypass; bank rows never freeze) |
| DELETE | `/bank-questions/{bid}`          | teacher | Delete a bank question + its images (**owner only**, admin bypass) |
| POST   | `/bank-questions/{bid}/image`    | teacher | Attach/replace the bank question's illustration: `multipart/form-data`, one `file` part — raster images only (`png`/`jpeg`/`webp`/`gif`), ≤ `max_file_bytes` (**owner only**, admin bypass) |
| GET    | `/bank-questions/{bid}/image`    | teacher | The illustration bytes (any teacher+) |
| DELETE | `/bank-questions/{bid}/image`    | teacher | Remove the illustration (**owner only**, admin bypass) |
| POST   | `/bank-questions/{bid}/choices/{choice_id}/image` | teacher | Attach/replace one option's picture, keyed by the `id` carried on that choice (`choice` questions; same form and limits as above; **owner only**, admin bypass) |
| GET    | `/bank-questions/{bid}/choices/{choice_id}/image` | teacher | The option picture's bytes (any teacher+) |
| DELETE | `/bank-questions/{bid}/choices/{choice_id}/image` | teacher | Remove one option picture (**owner only**, admin bypass) |
| POST   | `/courses/{id}/homework`         | teacher | `{title, description?, subject_id, due_at, assigned?}` — assign homework tagged with a course subject, due in the future; `assigned` names an enrolled-student subset, ≤ 200 (omit/`[]` = the whole course) (course manager) |
| GET    | `/courses/{id}/homework`         | student | List the course's homework, newest first (enrolled, creator, assigned teacher, or manager+; students see only what they're assigned) · paged |
| GET    | `/homework`                      | student | The caller's cross-course homework: their courses' (manager+: all; students only what they're assigned) · paged |
| GET    | `/homework/{id}`                 | student | Get homework (course viewers; a subset homework is a `404` to students it doesn't name) |
| PATCH  | `/homework/{id}`                 | teacher | Edit title/description/`due_at`/`subject_id`/`assigned` (course manager; a newly set due date is re-checked; `409` if narrowing `assigned` would strand an existing submission or grade — the blockers are named) |
| DELETE | `/homework/{id}`                 | teacher | Delete homework + its submissions, files, and grades — file blobs included (course manager) |
| POST   | `/homework/{id}/submission`      | student | `{text?}` — hand in / re-edit own work (**students only**, enrolled, assigned): text replaces whole (omit clears), `submitted_at` pins the first hand-in, `updated_at` moves; `409` once graded |
| GET    | `/homework/{id}/submission`      | student | Own submission: text, both stamps, computed `late`, files, grade-if-any (`404` until submitted) |
| DELETE | `/homework/{id}/submission`      | student | Withdraw own submission + its files and blobs (`409` once graded) |
| POST   | `/homework/{id}/submission/files` | student | Attach a file to own submission: `multipart/form-data`, one `file` part — **any** content type, ≤ `max_file_bytes`, ≤ 10 per submission; auto-creates the submission (a photo-only homework is one request); `409` once graded or full |
| GET    | `/homework/{id}/submission/files/{fid}` | student | The file bytes as a **forced download** (`Content-Disposition: attachment`) — the owning student, or a course manager; observers never |
| DELETE | `/homework/{id}/submission/files/{fid}` | student | Remove own file, row then blob (`409` once graded) |
| GET    | `/homework/{id}/submissions`     | teacher | The grading roster: one row per audience student × submission/files/grade + computed `late`/`missing`/`unenrolled` — a straggler's stale work stays visible (course manager) · paged |
| POST   | `/homework/{id}/results`         | teacher | `{user, status, mark?}` — grade `done`/`incomplete`/`missing` (+ optional 0–100 mark) onto an enrolled, assigned **student** (upsert; course manager; never yourself; freezes the submission; unsubmitted work gradable — that's `missing`) |
| GET    | `/homework/{id}/result`          | student | The caller's **own** grade (`404` until graded) — readable even when nothing was submitted (a `missing` verdict) |
| DELETE | `/homework/{id}/results/{user}`  | teacher | Remove a student's grade — un-grading reopens their submission (course manager) |
| GET    | `/homework/report/{user}`        | teacher* | Per-homework report rows — submitted/late/missing + the grade (manager+: full; a teacher: their managed courses); *or a `parent` linked to `{user}` — full, statuses and marks only, never files · paged |
| POST   | `/questions`                     | student | `{title, body}` — ask into the school question pool (**students only**); born `pending`, visible to the asker + teacher+ |
| GET    | `/questions`                     | student | `?status=pending\|approved` — the pool questions visible to the caller, each with its `solution_count` · paged |
| GET    | `/questions/{id}`                | student | Get one (a `pending` question is a `404` unless asker or teacher+) |
| POST   | `/questions/{id}/approve`        | teacher | Approve a pending question — publishes it school-wide; content frozen after (`409` if already approved) |
| DELETE | `/questions/{id}`                | student | Delete own question (teacher+: any); its solutions go with it |
| POST   | `/questions/{id}/image`          | student | Attach/replace the problem photo (asker only): `multipart/form-data`, one `file` part, raster only, ≤ `max_file_bytes` |
| GET    | `/questions/{id}/image`          | student | The photo bytes |
| DELETE | `/questions/{id}/image`          | student | Remove the photo (asker only) |
| POST   | `/questions/{id}/solutions`      | student | `{body}` — offer a solution on an approved question (anyone in the school) |
| GET    | `/questions/{id}/solutions`      | student | The question's solutions, oldest first — a discussion thread · paged |
| DELETE | `/questions/{id}/solutions/{sid}` | student | Delete a solution (author, or teacher+); its photo blob goes with it |
| PATCH  | `/questions/{id}/solutions/{sid}` | student | `{body}` — edit own solution (author only, teacher+ included out; solutions never freeze) |
| POST   | `/questions/{id}/solutions/{sid}/image` | student | Attach/replace the solution's photo (author only, anytime): `multipart/form-data`, one `file` part, raster only, ≤ `max_file_bytes` |
| GET    | `/questions/{id}/solutions/{sid}/image` | student | The solution photo's bytes (access follows the question) |
| DELETE | `/questions/{id}/solutions/{sid}/image` | student | Remove the solution's photo (author only) |
| GET    | `/marks/me`                      | student | The caller's mark report (per-course + overall averages) |
| GET    | `/marks/{user}`                  | teacher* | A user's mark report, narrowed to the caller's courses (manager+: full); *or a `parent` linked to `{user}` — full |
| POST   | `/work/check-in`                 | teacher | Open a work stint (server-stamped; `409` if already open) |
| POST   | `/work/check-out`                | teacher | Close the open stint (`409` if none open) |
| GET    | `/work/me`                       | teacher | Own work log, newest first (open stint has `check_out: null`) · paged |
| GET    | `/work/{user}`                   | manager | A staff member's work log · paged |
| PATCH  | `/work/entries/{id}`             | manager | `{check_in?, check_out?}` — correct a **closed** stint (`409` on open) |
| DELETE | `/work/entries/{id}`             | manager | Delete a work entry (open or closed) |
| POST   | `/pomodoro/start`                | student | Start a focus session (server-stamped; **students only** — a dangling unfinished session is discarded and replaced) |
| POST   | `/pomodoro/finish`               | student | Close the running session (`409` if none running) |
| GET    | `/pomodoro/me`                   | student | Own pomodoro log, newest first, + unpaged `total_focus_ms` · paged |
| GET    | `/pomodoro/{user}`               | teacher* | A user's pomodoro log, same shape · paged; *or a `parent` linked to `{user}` |
| GET    | `/attendance/me`                 | student | Own attendance report: events + sessions + per-course tallies |
| GET    | `/attendance/{user}`             | teacher* | A user's attendance report, narrowed to the caller's courses (manager+: full); *or a `parent` linked to `{user}` — full |
| GET    | `/settings`                      | student | The school's policy: `exam_kinds` (`{name, weight}` each), `attendance_statuses`, `grade_bands`, `max_file_bytes`, `chatbot_history_turns`, `max_chatbot_threads`, `max_chatbot_message_len` |
| PATCH  | `/settings`                      | manager | Replace any subset of the fields (lists wholesale); concurrent edits merge, never silently revert each other; `400` on "Invalid lists, bands, file limit, or chat limits"; `409` when a removed exam kind still has graded exams (see "Per-school policy") |
| POST   | `/terms`                         | manager | `{name, starts_at, ends_at}` — past dates allowed (calendar backfill) |
| GET    | `/terms`                         | student | List terms, newest first · paged |
| GET    | `/terms/{id}`                    | student | Get one term                    |
| PATCH  | `/terms/{id}`                    | manager | Edit a term (the merged range must stay ordered) |
| DELETE | `/terms/{id}`                    | manager | Delete a term — `409` while any course still links to it |
| POST   | `/chatbot/threads`            | student | `{title?}` — start a chatbot thread (every role incl. `parent`, always private to its owner); `409` at `max_chatbot_threads` |
| GET    | `/chatbot/threads`            | student | The caller's threads, newest activity first · paged |
| PATCH  | `/chatbot/threads/{id}`       | student | `{title}` — rename own thread (`null` or blank clears it back to untitled); counts as activity, so the thread moves to the top. Nothing auto-titles a thread |
| DELETE | `/chatbot/threads/{id}`       | student | Delete own thread and every message in it (someone else's is a `404`, never a `403`) |
| GET    | `/chatbot/threads/{id}/messages` | student | The thread's turns, oldest first — user and assistant rows alike · paged |
| POST   | `/chatbot/threads/{id}/messages` | student | `{content}` (≤ `max_chatbot_message_len`) — send a turn; `202 {message_id, status: "pending"}`, the answer is fetched after (`503` when no AI service offers `chat.reply` — nothing is written; `429` + `Retry-After` on the send tier) |
| GET    | `/chatbot/threads/{id}/messages/{mid}` | student | Poll one turn: `pending` until the answer lands, then `complete` + `content` or `failed` + `error_code` |
| GET    | `/chatbot/threads/{id}/messages/{mid}/stream` | student | **SSE** on the same row: `delta` chunks then one `done` — or one `error` — and close; an already-finished answer replays (see "Chatbot") |

`status` must be one of the school's attendance statuses (`GET /settings`);
the core four `present | absent | late | excused` always exist, plus whatever
the school added.
`kind` must be one of the school's exam kinds (`GET /settings`; defaults:
`homework | quiz | midterm | final | project | oral`). The kind carries the
exam's weight in the course average — an integer `1`–`100` set per **kind** in
settings (defaults all `1`), resolved when a report is read; an exam whose
kind was later removed from settings counts with weight `1`.
A course may carry a `term_id` (`null` = unassigned); on `PATCH
/courses/{id}`, an omitted `term_id` keeps the link and an explicit `null`
clears it.
`role` ∈ `parent | student | teacher | manager | admin`. Ids in responses are
ULIDs. `parent` accounts are made by an admin (register as `student`, then
`PATCH /users/{id}/role`) and observe only the students an admin tied to them
— see "Roles & access control".
An exam `mark` is an integer `0`–`100`; it lives in its own `exam_result` row,
never in a `note`. Students never grade anyone — grading is teacher+ with
course-management rights, and the target must be enrolled in the exam's course;
a student reads just their own mark via `GET /exams/{id}/result`.
A course average is `Σ(mark×weight) / Σ(weight)` over the student's **graded**
exams in that course (`null` while nothing is graded — ungraded exams are
skipped, not zeroed). The overall average is the plain mean of the non-null
course averages. Unenrolling keeps result rows: the marks drop out of the
report until re-enrollment, but stay visible on the exam itself. A course
with anyone still on its roster refuses deletion (`409`) — empty the roster
first; once empty, deleting it cascades its exams, their results, its
sessions and roll call, and its subjects.
A **subject** is one topic of a course's curriculum (`name` ≤ 200 chars,
optional `description` ≤ 2000): every exam question carries a mandatory
`subject_id` naming one of *its own course's* subjects (an unknown or
foreign-course subject is a `400`), so exam content is always attributable to
a topic. A subject's course link is fixed at creation, and a subject still
referenced by questions refuses deletion with a `409` — re-tag (PATCH the
questions' `subject_id`) or delete those questions first. Questions written
before subjects existed are **destroyed on boot** (answers first) — the clean
break instead of inventing a placeholder topic.
Event `starts_at`/`ends_at` are optional **unix-millisecond** integers; if both
are given, `ends_at` must not precede `starts_at`, and neither may be set in
the past (else `400`). On `PATCH`, an omitted time keeps its value and an
explicit `null` clears it.
An event's `audience` is a tagged object — `{"kind": "school"}` (the default),
`{"kind": "role", "role": "student"}` (that exact role, no "and above"),
`{"kind": "course", "course": "<id>"}` (the course's current enrollment), or
`{"kind": "registration", "capacity": 30}` (a signup list; `capacity` optional,
`null` = unlimited) — and is the event's **expected-attendee roster**, resolved
live at read time: role changes, (un)enrollments, and (un)registrations move
people in and out by themselves. It never hides the event — everyone sees every
event. Only audience members can be marked; `GET /events/{id}/roster` joins the
live roster with the recorded marks (`status: null` = expected but never
marked). Attendance rows for people a later audience edit (or unenrollment /
role change) dropped stay stored and listed under `/events/{id}/attendance`,
but leave the roster report.
A registration list fills through `POST /events/{id}/register`: teachers place
students (a student never registers, not even themselves) and take seats only
for themselves — registering another staff member is refused. Re-registering
someone is a no-op, a full list is a `409`, and the list freezes the moment
the event starts — or, when only `ends_at` is set (a pure signup deadline),
the moment it passes (register and unregister both). Signup rows survive an
audience switch inertly and resurface if the event returns to the registration
kind; deleting the event deletes them. Pre-existing hand-picked (`users`)
audiences convert on boot: each listed user becomes a signup row credited to
the event's creator, and the audience becomes an uncapped registration list.
Reading a child collection of a missing parent (`/events/{id}/attendance`,
`/exams/{id}/results`, `/courses/{id}/enrollments`, `/courses/{id}/exams`,
`/courses/{id}/sessions`, `/courses/{id}/subjects`, `/courses/{id}/homework`,
`/homework/{id}/submissions`, `/sessions/{id}/attendance`) is a `404`, not an
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
seed them.
UI preferences (`theme`: `light`/`dark`, `language`: `tr`/`en`) ride on the
same account row and come back on every user response (`/auth/me` included).
`PATCH /users/me/preferences` (or the admin `PATCH /users/{id}/preferences`)
uses the same field semantics as the profile patch: omitted keeps, `""` clears
back to `null` ("never chose" — the client then follows the device
preference), anything else must be one of the listed values or the whole patch
is a `400`.

## Appointments

Office hours and parent-teacher conferences, booked instead of arranged by
message. A teacher+ **publishes availability**; a student or a parent
**books** one of those windows with a reason; the teacher **decides**. Two
rows carry it: an `appointment_slot` (the offer) and an `appointment` (the
booking sitting on it) — a slot is never "half booked", and a refused request
stays readable instead of vanishing.

A slot is a window on one teacher's calendar: `starts_at`/`ends_at` (unix
milliseconds, `starts_at` strictly before `ends_at`, neither in the past — the
usual 60-second grace) plus an optional `note` (≤ 500 chars) shown to
requesters ("office hours", "veli görüşmesi"). Windows are **half-open**, so
10:00–10:30 and 10:30–11:00 are two slots, not a collision. A window that *does*
overlap one the same teacher has already published is refused with a `409` — the
guard is **per-teacher** (two teachers may hold office hours at the same hour)
and boundary-touching windows are legal by that same half-open rule, which is
how an hour gets carved into back-to-back slots. `POST
/appointments/slots` publishes on the caller's *own* calendar (managers and
admins included — the calendar always belongs to whoever posted) and **always
answers an array**: one element for a one-off, one per occurrence for a
recurring publish.

`repeat_weekly: true` with an `until` expands the same window every seven days
up to and including `until`, server-side, into **concrete rows** sharing a
`series` id — no recurrence rule is stored, so one week can be cancelled
without touching the rest. At most 52 occurrences (a year); asking for more is
a `400`, as is `repeat_weekly` without `until` — and so is a window sitting so
far ahead that shifting it by a week would run off the end of representable
time (the shift is checked, never wrapped: a wrapped end would land *before*
its start, and an inverted window can never overlap anything, which would
quietly disable the double-booking guard). A recurring publish is
**all-or-nothing**: every occurrence is checked — against the slots already
stored *and* against the earlier occurrences of the same batch, which a window
longer than a week overlaps itself — before a single row is written, so a
mid-series collision answers `409` and leaves no stray weeks behind. Delete one
occurrence with
`DELETE /appointments/slots/{id}`, the whole publish with `DELETE
/appointments/slots/series/{series}`.

```json
POST /appointments/slots
{ "starts_at": 1900000000000, "ends_at": 1900001800000,
  "note": "office hours", "repeat_weekly": true, "until": 1901209600000 }

201 [ { "id": "01J8…A", "teacher": {"id": "01J8…T", "username": "ayse",
        "display_name": "Ayşe Yılmaz"},
        "starts_at": 1900000000000, "ends_at": 1900001800000,
        "note": "office hours", "series": "01J8…S", "created_at": 1899… },
      { "id": "01J8…B", …, "starts_at": 1900604800000, "series": "01J8…S" } ]
```

`GET /appointments/slots` is two lists behind one path: a teacher+ reads
**their own** calendar, past occurrences included; everyone else reads the
**bookable** calendar — future slots only, earliest first. A teacher demoted
after publishing leaves **inert** slots: their live role is re-read, so those
slots drop out of the bookable list and booking one is refused with a `409`.
The list does not say whether a slot is already taken — booking a taken one
answers `409`.

**Booking.** `POST /appointments` (`{slot, reason}` — the reason is required,
≤ 1000 chars) is for **students and parents only**; staff arrange between
themselves off this API. A parent books for *themselves* — this is the
parent-teacher conference, not a booking on behalf of a child. The request
lands `pending`, always: publishing availability is not consent to a
particular person and a particular topic, so approve/reject still applies.

A booking is `pending`, `approved`, `rejected`, or `cancelled`. The first two
are **live** and hold the slot; rejecting or cancelling frees it for someone
else immediately (occupancy is counted live, never stored as a flag). The
decisions:

- `PATCH /{id}/approve` — the slot's teacher (or manager+) confirms. Refused
  (`409`) when the effective window has already started.
- `PATCH /{id}/reject` — turns it down; the slot frees up.
- `PATCH /{id}/cancel` — **the requester only**, from either live state.
  Anyone else is a `403`, the slot's teacher and a manager/admin included (the
  guard compares ids, not roles). A teacher ends a booking by **rejecting** it
  while it is `pending`, and an approved one by counter-proposing another time
  (`/reschedule`, which sends it back to `pending`) and then rejecting it — or
  simply by rescheduling to a time that works. Refused (`409`) once the
  meeting's window has started: a
  meeting that already began is history, not a plan. The guard sits in the
  domain's `cancel` itself, under the appointment lock and on a fresh read, so
  every way of cancelling — the decline below included — inherits it.
- `PATCH /{id}/reschedule` (`{starts_at, ends_at}`) — the teacher
  **counter-proposes**. The times land on the *same* row as
  `proposed_starts_at`/`proposed_ends_at`, the status goes back to `pending`
  and `decided_by` clears, because leaving it `approved` would silently move a
  confirmed appointment. The slot stays held meanwhile. A window that has
  already opened is refused (`409`): the 60-second grace on the times is for
  clock skew, not for proposing into a meeting already underway — one nobody
  could then cancel.
- `PATCH /{id}/reschedule/accept` — the **requester** only: this *is* approval
  at the new time, so the overlap guard runs again — and so does the
  already-started guard (`409`). Agreeing to a window that has begun does not
  help: the meeting could never be cancelled. The booking stays `pending`, so
  the teacher just proposes a time that can still happen.
- `PATCH /{id}/reschedule/decline` — the requester only, and it **cancels the
  booking**: the proposal replaced the time that was asked for, so there is
  nothing to fall back to. The row stays readable as `cancelled` with the
  refused proposal still on it — book another slot instead. Declining *is* a
  cancel, so it answers to the cancel deadline too (`409` once the effective
  window has started).

**Who settled it, and why.** Every settled booking carries its own audit trail
on the row: `decided_by` (who approved or rejected — cleared again by a
counter-proposal), `cancelled_by` (who called it off, stamped by `cancel` and by
a declined counter-proposal), plus the optional free text that came with the
decision — `reject_reason` on `PATCH /{id}/reject` and `cancel_reason` on
`PATCH /{id}/cancel` and `/reschedule/decline`. Both reasons are optional: the
whole body may be omitted, a blank one records nothing (`null`), and a present
one is validated like the booking's own reason (≤ 1000 chars, `400` past that)
before it reaches the row. They are **not public**: bookings are only ever
rendered to the person who requested them and to the slot's teacher (a
manager/admin acting on one by id sees the response to their own call), so
"couldn't make it, sorry" goes no further than the two people it concerns. Rows
written before these fields existed simply read back `null`.

A booking's effective window is the accepted counter-proposal when there is
one and the slot's own window otherwise; that is what `starts_at`/`ends_at` on
an `AppointmentResponse` report, and what the cancel deadline is judged on.

**No double-booking.** Two `approved` meetings may never overlap for the same
teacher *or* the same requester (`409`). The guard runs at approval — the
moment anyone is actually committed — and again on accepting a counter-proposal,
because the time moved. Approval also refuses (`409`) an **effective window
that has already started** (the proposal's when one stands, the slot's
otherwise), for the same reason booking one is refused. Requesting is checked more loosely: a booking is
refused up front only when the slot's window has already started (booking it
would create a meeting `cancel` refuses to undo — the 60-second publish grace
is for clock skew, not for booking into the past), when the slot is already
taken, or when the *requester* is already committed at that hour; a clash on the teacher's side is theirs to
resolve when they decide, since refusing a request against a window they
themselves published would only confuse. Touching windows never conflict.

Deletion follows the same rule as the rest of the codebase: a slot carrying a
live (pending or approved) booking refuses to go (`409`) — reject or cancel
that booking first — and a series delete is all-or-nothing, `409` if *any*
occurrence still carries one, so the person waiting is dealt with rather than
left stranded on a stray week. Settled (rejected/cancelled) bookings cascade
away with their slot.

`GET /appointments` is likewise two lists: a teacher+ sees the bookings aimed
at their own slots (their request inbox), everyone else the ones they
requested, newest first, paged. Managers and admins read their own inbox here
too — they may still decide any booking by id.

## Messaging

One-to-one, mail-style (subject + body + an optional free-text `label` the UI
renders as a badge — "Etüt", "Sınav"; no threads): any user writes to any
user — student to teacher, parent to teacher, teacher to student; only
messaging yourself is refused. A single stored message serves both parties,
but each **owns their copy independently**: the recipient's moves through
`inbox` → `archive`/`trash` and carries the `read` flag (the sender sees it
as a read receipt); the sender's moves through `sent` → `archive`/`trash`. Filing or
deleting your copy never changes the other side's view.

Every copy remembers where it was filed from, so it can go back there. Moving
into `archive`/`trash` stamps the folder you left onto the copy's
`previous_folder`, and restoring is a `PATCH` of `folder` back to that value:
an inbox message you archive and then trash returns to `archive`, and from
there to `inbox`. Two folders are never stamped — the trash itself (pulling a
copy out of the trash must not leave it pointing back at the discard pile) and
the folder you are already in — and moving back to `inbox`/`sent` clears the
stamp. `previous_folder` is therefore `null` for any copy sitting in its home
folder, and for copies filed before the field existed; both mean "restore to
`inbox`" (`sent` for the sender's copy).

Listing is per folder — `GET /messages?folder=` with `inbox` (default),
`sent`, `archive`, or `trash` (trash shows both received and sent copies you
trashed) — newest first, paged, with sender/recipient rendered as person refs
plus their role. `?read=false` narrows to unread (`true` to read), and since
`total` counts the filtered view, `?folder=inbox&read=false&limit=1` is the
one-row unread-badge query. `PATCH /messages/{id}` flips `read` (recipient only) or
moves your copy (`folder`), restoring included. `DELETE` is
permanent, allowed only while your copy sits in the trash (`409` otherwise),
and physically removes the row once both sides have deleted theirs. Replying
is just sending a new message back — the frontend prefixes the subject if it
wants an `Re:`.

## Question pool

Students get stuck; the pool is where the school unsticks them — with a
moderation gate so nothing unreviewed goes school-wide. `POST /questions`
(students only, exact role) creates the question `pending`: only the asker
and teacher+ can see it (anyone else gets a `404`, not a `403` — a pending
question's existence is nobody else's business), and `GET
/questions?status=pending` is a teacher's approval queue. The asker may
attach **one photo** of the problem (`POST /questions/{id}/image`,
`multipart/form-data` with a `file` part — raster types only, no SVG, ≤ the
school's `max_file_bytes`), replace it, or remove it — while pending only.

`POST /questions/{id}/approve` (teacher+) publishes it: the question becomes
readable by every signed-in user above `parent`, and its content — title,
body, photo — **freezes**, because an edit after approval would bypass the
moderation that just happened. There is no rejected state and no edit
endpoint: to fix a typo the asker deletes and re-asks; to reject, a teacher+
deletes. Approving twice is a `409`; the response records `approved_by`.

Anyone in the school (student through admin — not parents) may then offer a
**solution**: `POST /questions/{id}/solutions` with a text body, listed
oldest-first like a discussion thread (`GET`, paged). Solutions are the
unmoderated half of the pool, so nothing about them ever freezes: the author
— and only the author, teacher+ included out — may edit the body (`PATCH
/questions/{id}/solutions/{sid}`) and attach one **photo** of the worked
steps (`POST /questions/{id}/solutions/{sid}/image`, same raster-only rules
and `max_file_bytes` cap as the question's photo), replace it, or remove it,
at any time; moderation stays delete-only — a teacher+ removes a bad
solution, never rewrites someone else's words. A solution is deleted by its
author or by teacher+, its photo blob going with it; deleting a question —
asker withdrawing, or teacher+ moderating — takes its solutions and every
photo blob (its own and its solutions') with it. Each question row reports
its `solution_count`, and questions and solutions embed their people as
person refs (`{id, username, display_name}`), so the UI never shows a raw
ULID.

## Per-school policy (settings & terms)

Every school runs differently; the parts that vary are data, not code. One
editable `settings` singleton (`GET /settings` for any signed-in user,
`PATCH /settings` for manager+) carries seven knobs:

- **`exam_kinds`** — the accepted `kind` values for new exams, each an
  object `{name, weight}`. The weight (`1`–`100`) is how many times an exam
  of that kind counts into its course average — weighting is school policy,
  set once per kind, never per exam. Defaults to `homework, quiz, midterm,
  final, project, oral`, all weighing `1` (a plain average); replace the
  list with whatever the school grades and weighs (`{"name": "final",
  "weight": 3}`, …). Reports resolve weights live: editing a weight
  re-weights every exam of that kind at once. For the same reason a kind
  whose exams already carry marks cannot be removed from the list (`409`):
  those marks would silently re-weight. An unmarked kind leaves freely, and
  an exam keeping a since-removed kind counts with weight `1`.
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
- **`max_file_bytes`** — the per-file size cap for uploads (note files and
  exam question images alike), in bytes: `1024` (1 KiB) to `26214400` (25 MiB;
  a server hard cap — uploads buffer in memory), default `5242880` (5 MiB).
  Checked at upload time only: lowering it never touches already-stored files.
- **`chatbot_history_turns`** — how many prior **messages** of a thread the
  chatbot is given as context, `1`–`50` (default `10`). The unit is messages,
  not exchanges: a question and its answer are two, so the default remembers
  five question/answer pairs. Every one of them is re-sent on every reply, so
  the number is both what the bot remembers and what each answer costs.
- **`max_chatbot_threads`** — how many chatbot threads one user may
  keep, `1`–`500` (default `50`); at the cap the user deletes an old one
  first. Storage protection, not a usage quota — that is the per-minute
  message limit.
- **`max_chatbot_message_len`** — character limit on one chat message,
  `100`–`8000` (default `4000`); the ceiling is a server hard cap, because
  the message and its history must fit one bridge frame.

A `PATCH` replaces only the fields it carries, each wholesale, and validation
is all-or-nothing. Concurrent edits are safe: each save applies only if the
policy still matches the snapshot it merged from (retrying over the fresh row
otherwise), so two managers patching different fields both land instead of
the later write silently reverting the earlier one. Editing a list never
rewrites history: an exam keeps its retired kind, a roll-call row keeps its
retired status — only **new writes** are held to the current lists.

Academic structure is data too. **Terms** (`/terms`) model whatever calendar
the school runs — semester, trimester, quarter systems are just rows with a
name and a date range. Courses may link to one via `term_id` (nullable), and
deleting a term is refused with a `409` while any course still links to it —
unlink those courses (`PATCH /courses/{id}` with `"term_id": null`) or delete
them first, so the calendar never disappears under them. Term dates may lie in the past,
deliberately: a school adopting the app mid-year backfills its calendar —
unlike exam/lesson/event times, which reject backdating.

What stays fixed is deliberate too: the four roles, the `0`–`100` mark scale,
validation bounds, and the UTC time policy are invariants, not preferences
(rename role labels in the frontend if a school says "principal" instead of
"manager"). Deployment knobs (ports, rate limits, admin seed, CORS) remain
environment variables — the model is **one school per deployment**, which
keeps every school's data physically isolated.

## Exam modes, attempts, retakes, rejoin & live monitoring

Preparing an exam is slow work, so an exam can be created as a **draft**
(`draft: true`): only the course's managers see it (to students it's a `404`
that might as well not exist — lists, direct reads, and attempts all hide
it), nobody can sit it, and grading it is a `409`. The teacher builds the
questions in peace and publishes with `PATCH /exams/{id}` `{"draft": false}`.
An exam can go back into hiding the same way — but only while it has **no
attempts and no results**; after that, re-drafting is a `409` (students never
lose sight of an exam they've sat or been graded on). Existing exams (and
those created without the flag) are published from the start.

Separately from drafts, a published exam without a `mode` (all schedule
fields `null`) is **offline-graded** — a paper exam whose marks are entered
by hand; students see it and their marks, but attempts on it are a `409`
("nothing to sit"). Making an exam sittable means giving it a `mode`, as one
consistent unit (validated together on create and after every `PATCH` merge):

- `mode: "sync"` + `starts_at` + `ends_at` — everyone sits inside one window;
  every attempt's deadline is `ends_at`.
- `mode: "async"` + `starts_at` + `ends_at` + `duration_ms` — each student
  starts anywhere inside the window and gets
  `min(started_at + duration_ms, ends_at)` as their personal deadline.
- `mode: "open"` — no window at all: students sit anytime. `duration_ms` is
  *optional* — set it for a per-attempt countdown (`started_at +
  duration_ms`), omit it for unlimited time (the attempt only ends by
  submission).
- `duration_ms` is 1 minute to 24 hours wherever it appears, and for `async`
  cannot exceed the `starts_at`..`ends_at` window (equal to it is fine).
  `ends_at` must be strictly after `starts_at`, and neither may be *set* in
  the past — on create or `PATCH` (kept values are exempt, so a running exam
  stays editable). All instants are the usual UTC unix-milliseconds, judged
  only by the server clock (`GET /time` for sync).

Two per-exam policy knobs ride along, both **live-editable** at any point:

- `max_attempts` (default `1`, `0` = unlimited) — how many sittings each
  student gets. Raising it mid-exam grants retakes on the spot; lowering it
  never kills a running attempt, it only blocks future starts.
- `allow_rejoin` (default `true`) — whether a student who *left the exam room*
  may come back in and keep answering (see the exam-room section).
- `allow_review` (default `false`) — opt-in door to the student self-review
  reads (`GET /exams/{id}/review/attempts[/{seq}/answers[/{qid}/image]]`). A
  student may review only when this is on **and** the teacher has marked them
  (an `ExamResult` row exists); own-scoped, so no student reads another's sheet.

A student **sits** an exam through attempts (sitting 1, 2, … — each row id is
the composite `exam_user[_seq]` key, so a sitting exists at most once by
construction):

- `POST /exams/{id}/attempt` starts, resumes, or retakes (students only —
  staff never sit; enrolled; window open where one exists — `open` exams start
  anytime). While the latest
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
anyone has started is only `mode` (including back to unscheduled) — swapping
the deadline rules mid-sitting would be a different exam (`409`).

The course's manager (its creator, or manager+) watches it all live:
`GET /exams/{id}/live` returns one snapshot —
the enrolled roster joined with attempts and marks (`not_started` | `absent` |
`in_progress` | `submitted` | `expired`, per-student `attempt` /
`attempts_used` / `left_at` / `deadline` / `remaining_ms` / `mark`, always the
latest sitting) plus summary counts, all judged at a single `now`. Once the
window closes, `not_started` hardens into `absent` — the no-shows, flagged
right in the roster (open exams have no window, so never an `absent`). The
flag is informational: an absent student still has no mark until the teacher
records one through `POST /exams/{id}/results`. Poll `GET /exams/{id}/live` to
keep a monitor current — attendance, ticking clocks, submissions, and marks all
ride in each snapshot.

Deleting an exam (or its course) cascades attempts, questions, answers, and
question + answer images (blobs included) along with results; unenrolling mid-exam
hides the student from the monitor roster but keeps the attempt and mark
rows, mirroring the marks report.

> **Upgrading a pre-course database**: `exam` rows created before courses
> existed lack the now-required `course` field and will fail to deserialize.
> For a dev database, delete `./data/hezarfen.db` and reboot; to keep data,
> backfill manually with the SurrealDB CLI (server stopped), e.g.
> `UPDATE exam SET course = course:<id> WHERE course = NONE;`
> after creating a course to attach them to.
>
> **Upgrading to the attempts/rejoin build needs nothing manual**: the boot
> migration backfills `max_attempts = 1`, `allow_rejoin = true`, and attempt
> `seq = 1` on existing rows, and swaps the single-attempt unique index for
> the per-sitting one. Pre-upgrade attempts keep their record ids and count
> as sitting #1.
>
> **Upgrading to the kind-weight build is a clean break**: weights moved off
> exams onto the settings `exam_kinds` entries (`{name, weight}` objects) with
> no data migration. A database from an older build has string kinds and a
> per-exam `weight` column the code no longer understands — delete
> `./data/hezarfen.db` and reboot.

## Taking an exam: questions, answers & the exam room

Teachers author a question list per exam; students answer inside their
attempt, autosaved as they go; choice questions are machine-checked as a
**suggestion** — the final mark stays a human call through the existing
`POST /exams/{id}/results`.

**Questions** (`POST/GET/PATCH/DELETE /exams/{id}/questions[/{qid}]`,
course-management rights): each has a mandatory `subject_id` (one of the
course's subjects, `GET /courses/{id}/subjects` — the topic the question
belongs to), `text` (≤ 2000 chars), `points` `1`–`100`
(its share of the auto-score), and a `kind`:

- `kind: "choice"` — carries `choices` (2–10 options, each ≤ 500 chars) and
  `correct`, the zero-based index of the right option. Auto-scorable.
- `kind: "text"` — free text, judged by the grader; carries neither.

The kind bundle is validated as a unit (create and after every `PATCH` merge):
switching a question to `text` needs explicit `"choices": null, "correct":
null`, switching to `choice` must bring both along. Presentation order is
creation order. The whole list **freezes once anyone has started an attempt**
(`409` on create/edit/delete) — editing questions under a sitting student
would fork what "the exam" means. Deleting a question cascades its answers
and images.

**Question images**: any question may carry one **illustration** (`POST
/exams/{id}/questions/{qid}/image` — the map the prompt asks about, on
`choice` and `text` questions alike), and each option of a `choice` question
may carry a **picture** of its own (`POST .../choices/{choice_id}/image` — so
the options themselves can be images: four map crops, pick the right one).
Uploads are `multipart/form-data` with a single `file` part, capped by the
school's `max_file_bytes`; the declared content type must be `image/png`,
`image/jpeg`, `image/webp`, or `image/gif` (rasters only — SVG can script,
and these bytes render inline for the whole class). One image per slot:
re-uploading replaces, `DELETE` on the same paths removes, and replacing a
question's `choices` list drops all its option pictures (the illustration
stays — re-upload against the new list). Image writes follow question
authoring exactly: course-management rights, frozen once attempts exist.
Both question views (`GET /exams/{id}/questions` and the sitting view)
embed the metadata as `image: {content_type, size} | null` and
`choice_images: [...|null]` aligned with `choices`; the bytes come from the
`GET` endpoints above — course managers anytime, students through the same
enrollment + started-attempt wall as the sitting view (no early peek at the
pictures either), served with `Cache-Control: private, no-store`.

**Answering** (student, attempt `in_progress`, deadline judged by the server
clock on every save):

- `GET /exams/{id}/attempt/questions` — the sitting view. Requires
  enrollment (`403` — the questions are course content, so leaving the course
  closes them) and an attempt (`404` before `POST /exams/{id}/attempt`; also
  the anti-peek gate), never contains `correct`, and embeds the caller's own
  saved answer per question (`{selected, text, updated_at}` or `null`). Still
  readable after submitting/expiry, for review.
- `POST /exams/{id}/attempt/answers` `{question_id, selected? | text?}` — an
  upsert: one row per question+user, re-answering overwrites. The payload must
  match the question's kind (`selected` indexing a choice, or `text`
  ≤ 10 000 chars — empty clears the draft); mismatches are `400`s. Requires
  the student role and enrollment (`403`): a promotion out of `student` or an
  unenrollment mid-exam closes the sheet (the exam room's door checks,
  re-applied to every save — finishing stays open). Once the
  attempt is submitted or past its deadline every save is a `409` — likewise
  while the student has left the exam room with `allow_rejoin` off; answers
  saved in time survive untouched for grading. Each sitting keeps its own
  answers, drawings, and mark, keyed by the attempt's `seq` — a retake starts
  from a blank sheet without erasing the prior one, so the full per-attempt
  history stays readable (see the grading view).

**Answer images**: the student side mirrors the teacher's question images. A
student may attach one **drawing** to any question in their attempt — a
sketched answer or worked steps — via `POST
/exams/{id}/attempt/answers/{qid}/image` (`multipart/form-data`, one `file`
part, the same raster allowlist and `max_file_bytes` cap as question images).
One image per question: re-uploading replaces, `DELETE` on the same path
removes. Writes ride the exact `POST /exams/{id}/attempt/answers` gate — the
student role, enrollment, an `in_progress` attempt, and the rejoin door — so a
drawing is just another autosaved answer; a drawing-only answer (nothing
typed) still surfaces, and clearing it grooms the empty row away. The bytes
come from `GET` on the same path (the caller's own, behind the sitting-view
wall) and, for the grader, `GET /exams/{id}/attempts/{user}/answers/{qid}/image`
(course-management rights); both serve inline with `Cache-Control: private,
no-store`. The metadata rides the answer as `answer_image: {content_type,
size} | null` on both the sitting view and the grading sheet. To the backend
it is a normal raster PNG; the frontend piggybacks its editable stroke data in
a PNG `tEXt` chunk, opaque here — stored on disk under `Config::files_path`
by a ULID, metadata in the `answer_image` table, structurally identical to
question images.

**Grading view** (course-management rights): `GET /exams/{id}/attempts/{user}/answers` returns
the student's **latest** sitting's sheet — every saved answer with `is_correct`
(`true`/`false` for choice, `null` for text: that's the grader's call) plus
`auto_score: {earned, possible}` summing the choice questions' points. It is a
suggestion to read while grading, never written anywhere.

**Per-attempt history** (course-management rights): prior sittings stay
readable. `GET /exams/{id}/students/{user}/attempts` lists the sitting numbers
a student has (every `seq` carrying answers or a mark, ascending);
`GET /exams/{id}/students/{user}/attempts/{seq}/answers` is the grading sheet
for one of them (drawing bytes at
`GET /exams/{id}/students/{user}/attempts/{seq}/answers/{qid}/image`); and
`GET /exams/{id}/students/{user}/marks` is the full per-sitting mark history,
oldest first. Grading (`POST /exams/{id}/results`) always lands on the current
sitting, and the latest seq is the grade-of-record — the roster, report, and
statistics reads all show it. Every result payload carries its `seq`, so a mark
links straight to that sitting's answer sheet — without it a marked retake's
answers would be unreachable from the mark.

**The exam room (WebSocket)** — `GET /exams/{id}/attempt/ws`, cookie-authed
like everything else; REST above remains the full fallback. Gates run before
the upgrade: unknown or draft exam `404`, unscheduled (no mode) `409`, not a
student `403`, not enrolled `403`, no attempt yet `404` (start it first),
submitted/expired `409`, left while rejoin is closed `409`. Then JSON text
frames:

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
past its real deadline — the socket shares the exact REST write path. Each
room acts only on the sitting it was opened for: once that sitting is over,
its `answer`/`finish` are `error` frames, so a lingering socket can never
scribble on (or instantly submit) a retake started elsewhere.

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

## Question bank

Writing good questions is slow, and the same one gets asked year after year, so
the school keeps a reusable **question bank**. A bank question is an
`ExamQuestion` cut loose from any exam: it has the same shape (`subject_id`,
`text`, `kind`, `points`, and — for a `choice` — `choices` + `correct`), the
same image slots (one illustration, plus a picture per option), and the same
newtype validation, but it carries **no exam link**. Because nothing sits it,
a bank row **never freezes** — its owner can edit or delete it forever.

`POST /bank-questions` (teacher+) saves one, stamping the caller as its
**owner**. `subject_id` is origin metadata, but it must **exist** — an unknown
subject is a `400` (create and PATCH alike); the reverse guard holds too, so
deleting a subject a template still references is a `409` (same as exam
questions and homework). Reads are school-wide: `GET /bank-questions` lists the
whole bank (paged, `?subject=<id>` narrows by origin subject, `?owner=<id>` — or
`?owner=me` — by owner) and `GET /bank-questions/{bid}` fetches one with
`correct` included — any teacher+ can browse and reuse what any colleague
banked. Each response carries its **image metadata** — an `image` meta for the
illustration and a `choice_images` array aligned with `choices` (per-slot
`{content_type, size}`, `null` where a slot has no picture) — the same shape an
exam question's response uses; the bytes still come from the image endpoints.
Mutation is **owner-only** (an `admin` bypasses): `PATCH /bank-questions/{bid}`
revalidates the kind bundle as a unit, `DELETE /bank-questions/{bid}` drops the
row and its image blobs. The illustration and option pictures work exactly like
an exam question's — `POST|GET|DELETE /bank-questions/{bid}/image` and
`…/choices/{choice_id}/image`, `multipart/form-data` with one `file` part, raster
only (`png`/`jpeg`/`webp`/`gif`), ≤ `max_file_bytes` — reads school-wide,
writes owner-only.

The bank feeds exams by **copying**, never linking. `POST
/exams/{id}/questions/from-bank/{bid}` instantiates a bank question into the
exam: it mints a **fresh** exam-scoped question (new id, its own answer rows,
its image blobs copied to new files), so later edits on either side never
touch the other. The subject on the bank row is origin metadata only — the
call takes its own `{subject_id}`, re-checked against the target course's
subjects (`400` on a cross-course subject), and instantiating into an exam
that already has attempts is a `409`, the same freeze that guards hand-authored
questions. The instantiated question records the template it came from in
`source_bank`. The reverse, `POST /exams/{id}/questions/{qid}/to-bank`, copies
an existing exam question **into** the bank (new detached row, images copied to
new files, owner = caller); the source question is left untouched, and the new
template records the origin exam in `source_exam`. Both provenance fields are
nullable — a hand-authored question or template carries `null` — and one-way
metadata (the copies stay fully detached). All bank routes are course-agnostic
and require teacher+.

Copying means a template edit never reaches the copies, so there is one escape
hatch for that divergence: `POST /exams/{id}/questions/{qid}/refresh-from-bank`
re-copies the template's *current* content over one exam question — text,
points, kind, choices (adopting the **template's** choice ids, so the option
pictures re-land on the right options), `correct`, the illustration and the
option pictures. The question keeps its own id, its exam, its `subject` (the
template's is unrelated origin metadata) and its provenance links; anything
edited on the copy is overwritten, deliberately. A question with no
`source_bank` — hand-authored, or its template deleted — is a `400`, a template
the caller may not see is a `404` (never a 403), and the usual freeze applies:
once any attempt exists, `409`. Every source blob is read before anything is
written, so a missing blob is a `500` with the question untouched rather than a
half-applied refresh.

## Homework

A course hands out **homework**: `POST /courses/{id}/homework` with a title,
an optional description, a **required subject** (one of the course's own — and
the same delete guard questions have: a subject with homework refuses deletion
with a `409` until the homework is re-tagged via `PATCH /homework/{id}` or
deleted), and a **required `due_at`** that must not lie in the past (same 60s
grace as every schedule field; late *submissions* are fine — a late
*assignment* is not).

**Audience.** By default the whole course, resolved live: whoever is enrolled
*when they act* — a student enrolled after the assignment sees and submits it
like everyone else. `assigned` (at create or PATCH) instead pins a named
subset of currently enrolled students (at most 200). The unnamed must not even
learn a subset exists: lists omit it, and direct reads and submits answer
`404` — the same no-leak a hidden exam draft gets. Narrowing the subset later
is refused with a `409` that names the blockers while any submission or grade
belongs to a student the new list would strand.

**Submitting.** Students only — staff and parents never submit — enrolled and
in the audience, re-checked on every write. A submission is optional text plus
up to **10 files of any content type** (each ≤ the school's
`max_file_bytes`): text is replaced whole on each `POST` (omit to clear),
files are added and removed one by one, and a file upload with no prior
submission auto-creates an empty one, so a photo-only homework is a single
request. Two stamps tell the lateness story: `submitted_at` pins the **first**
hand-in forever, `updated_at` moves on every text edit and file add/remove,
and `late` is **computed on read** (`updated_at > due_at`), never stored — so
touching work after the deadline flips it late, and a `due_at` edit re-grades
lateness for free. Late submissions are always accepted, only flagged. File
bytes always come back as a forced download (`Content-Disposition:
attachment`): homework accepts any content type, so rendering an uploaded
HTML/SVG inline would run it in the viewer's session.

**Grading.** A course manager records a status — `done`, `incomplete`, or
`missing` — plus an optional 0–100 `mark` per student
(`POST /homework/{id}/results`, one upsert row per homework+student; the
exam walls apply: live students only, enrolled, in the audience, never the
grader themselves). A stored grade **freezes** that submission — text edits,
file changes, withdrawal all `409` — until
`DELETE /homework/{id}/results/{user}` removes it and reopens editing.
Grading before the due date, or before anything was submitted, is allowed —
the latter is how never-handed-in work gets its `missing` verdict, and
`GET /homework/{id}/result` is where its student reads it (their submission
endpoints have nothing to show). The teacher-set `missing` **status** is a
deliberate verdict; the roster and the report *also* compute a `missing`
**flag** for anyone unsubmitted past due — the two are independent. Homework
marks stay out of `/marks`: the weighted course average remains exam-only
(the exam *kind* named `homework` still lives there), so nothing
double-counts.

**Roster & report.** `GET /homework/{id}/submissions` (course manager) is the
grading table: one row per audience student — plus any straggler outside the
audience who still owns a submission or grade (an unenrollment, a promotion,
or an audience change leaves work behind; it stays visible, flagged
`unenrolled`, though a stale student can neither submit nor be graded).
`GET /homework/report/{user}` is the fourth observer read beside marks,
attendance, and pomodoro: teacher+ (an exactly-teacher caller narrowed to the
courses they manage), or a **parent linked** to the student — per-homework
rows of submitted/late/missing plus the grade; statuses and marks only,
**never the files**.

Every delete collects its garbage: withdrawing a submission, deleting a
homework, and deleting the whole course each cascade the rows (submissions,
files, grades) and unlink the file blobs from disk. Role changes never sweep
homework rows: a promoted student's work stays readable on the roster behind
the `unenrolled` flag, while the live-role gates refuse fresh edits and
grades.

## Lesson sessions, roll call, the work log, pomodoro & attendance reports

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
manager marks **enrolled** students — only students attend classes, so a
non-student or unenrolled target is a `400` — and the
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

The **pomodoro log** (`/pomodoro`) is the students' study-time twin of the
work log. The frontend owns the timer — the visible countdown, the work/break
rhythm, the durations; the backend stores no timing policy and records only
**focus stints**, stamped by the server clock exactly like work stints.
`POST /pomodoro/start` opens a session and — unlike a work check-in — always
succeeds for a student: a dangling unfinished session (a laptop closed
mid-timer) is **discarded and replaced**, because it recorded no focus and
must not lock the student out of the next one. `POST /pomodoro/finish` closes
the running session (`409` when nothing runs); breaks are never reported.
Logs return the usual page envelope plus `total_focus_ms`, the unpaged sum of
every finished session's duration: `GET /pomodoro/me` for your own,
`GET /pomodoro/{user}` for teacher+ (study oversight). Only students start
sessions — pomodoro is the study tool, the work log is the staff timesheet.

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

# note files (multipart; -F sets filename + content type from the file)
NO=$(curl -s -b $JAR $BASE/notes -H 'content-type: application/json' \
  -d '{"title":"with file"}' | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
FI=$(curl -s -b $JAR $BASE/notes/$NO/files -F 'file=@plan.pdf' \
  | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
curl -s -b $JAR $BASE/notes/$NO/files            # list metadata
curl -s -b $JAR -OJ $BASE/notes/$NO/files/$FI    # download, original filename

# events + attendance
# Creating events needs teacher+; grant it first (see "Bootstrapping" above —
# e.g. UPDATE user SET role='teacher' WHERE username='ali'), else this is 403.
EV=$(curl -s -b $JAR $BASE/events -H 'content-type: application/json' \
  -d '{"title":"standup"}' | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
curl -s -b $JAR $BASE/events/$EV/attendance -H 'content-type: application/json' \
  -d '{"status":"present"}'
curl -s -b $JAR $BASE/events/$EV/attendance

# courses + weighted marks (as a teacher)
# optional, manager+: weigh midterms triple — school policy, per kind
curl -s -b $JAR -X PATCH $BASE/settings -H 'content-type: application/json' \
  -d '{"exam_kinds":[{"name":"midterm","weight":3},{"name":"quiz","weight":1}]}'
# find the student to enroll (here: a registered user "veli"): fragment
# search over username/name, role-narrowed (teacher+)
SID=$(curl -s -b $JAR "$BASE/users/search?q=vel&role=student" \
  | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
CO=$(curl -s -b $JAR $BASE/courses -H 'content-type: application/json' \
  -d '{"title":"algebra"}' | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
curl -s -b $JAR $BASE/courses/$CO/enrollments -H 'content-type: application/json' \
  -d "{\"user_id\":\"$SID\"}"
EX=$(curl -s -b $JAR $BASE/courses/$CO/exams -H 'content-type: application/json' \
  -d '{"title":"midterm","kind":"midterm"}' | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
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
# pomodoro: start when the timer starts, finish when it rings
curl -s -b $STUDENT_JAR -X POST $BASE/pomodoro/start
curl -s -b $STUDENT_JAR -X POST $BASE/pomodoro/finish
curl -s -b $STUDENT_JAR $BASE/pomodoro/me
```

## AI bridge (QUIC)

The AI features are separate projects — separate repos, separate processes,
probably not Rust. They talk to this backend over a QUIC bridge rather than
HTTP.

Why QUIC: stream multiplexing lives in the transport. Each request rides its
own **bidirectional stream** on the service's single long-lived connection, so
concurrent requests need no correlation-id bookkeeping and never head-of-line
block each other — a 30-second inference on one stream does not delay the
answer on another. Connection setup (and the TLS handshake) happens once, not
per request.

**The backend listens; the services dial in.** A service can therefore sit
behind NAT, restart without the backend knowing its address, and scale out by
opening a second connection — each connection is an independent worker.

Off by default: with `AI_QUIC_ADDR` unset the bridge never starts and the rest
of the API is unaffected.

### Handshake

The service connects with ALPN `hab/1`, then opens the **control stream** (the
first client-initiated bidi stream) and writes one `Hello`:

```json
{ "protocol": "hab/1", "service": "ocr", "capabilities": ["ocr.extract"],
  "token": "<AI_SHARED_TOKEN>", "max_concurrent": 8 }
```

The backend answers one `Greeting` and leaves the stream open:

```json
{ "type": "welcome", "worker_id": "01J...", "protocol": "hab/1" }
{ "type": "rejected", "code": "unauthorized", "message": "invalid token" }
```

Reject codes: `unsupported_protocol`, `unauthorized`, `no_capabilities`,
`malformed`. The token is compared in constant time; the handshake must
complete within 10s.

The control stream carries no further frames. **Its closure is the goodbye** —
there is no heartbeat. A service that dies silently is dropped by the QUIC idle
timeout (30s, with a 10s keepalive) and deregistered then.

`max_concurrent` is advisory and clamped to `1..=64` (default 8). Requests
beyond it are refused with "busy" rather than queued.

### Requests

For each request the **backend** opens a bidi stream, writes one `Request`,
finishes its send side, and reads one `Response`:

```json
{ "id": "01J...", "capability": "ocr.extract", "deadline_ms": 30000,
  "payload": { "image": "<base64>" } }
```

```json
{ "status": "ok",  "id": "01J...", "payload": { "text": "..." } }
{ "status": "err", "id": "01J...", "code": "unsupported_image",
  "message": "only png and jpeg" }
```

`id` is a trace id for logs on both sides — correlation is the stream, not the
id. It must still be echoed: an answer carrying a different id means the
service lost track of whose work it is, and the payload is refused. `payload`
is opaque to the transport; its shape belongs to the capability.

`deadline_ms` is when the backend gives up. A service should abandon the work
rather than answer late. A handled failure is an `err` frame; a crash is just a
dropped stream.

### Framing

`u32` big-endian byte length, then that many bytes of JSON. One frame per
message. The length is validated against the 8 MiB cap **before** any buffer is
allocated. JSON rather than a binary codec so a service in any language can
speak it in ~20 lines.

### Routing

Requests route by exact capability string to the **least-loaded** worker
offering it (ties broken by worker id). Least-inflight rather than
round-robin because AI request costs are wildly uneven — round-robin would
pile a second long inference onto a busy worker while an idle one sits next
to it. Selection and the in-flight increment happen under one lock, so
concurrent dispatches cannot overshoot `max_concurrent`.

Failure modes the caller sees: `NoWorker` (nothing registered — retryable),
`Busy` (all at capacity — retryable), `Timeout` (may still be running on the
far side, so it promises nothing), `Remote` (the service's considered no —
not retryable), `Protocol` / `IdMismatch` (broken peer), `Transport`.

### Conformance suite

`tests/ai_protocol.rs` is the wire contract, enforced on every `cargo test`. It
speaks `hab/1` with a client that imports none of the crate's protocol types —
frames built as byte literals, answers parsed as untyped JSON — so it fails on
exactly the changes a service in another language would notice: a renamed
field, a re-tagged enum, a flipped length-prefix endianness, a newly-required
`Hello` field. (`tests/ai_bridge.rs` drives real QUIC clients too, but shares
the Rust structs with the backend, so it cannot see those.)

It is also the reference implementation: its `raw` module is the whole client
side of the protocol in about a hundred lines — handshake, framing, request
loop — and is the shortest thing to port when writing a new service.

Guarantees it pins beyond the frame shapes: unknown fields in `Hello` and in a
response are accepted (a newer service may run against an older backend);
payloads pass through byte-for-byte whatever their JSON shape; request ids are
unique; answers may come back in any order; a service that drops one request
stream fails only that request; a service that ignores `deadline_ms` finds its
stream torn down rather than delivering a late answer.

### TLS, auth & finding the certificate

QUIC has no plaintext mode. With `AI_TLS_CERT`/`AI_TLS_KEY` unset, a
self-signed certificate is generated at boot and its sha256 fingerprint logged
— services pin that instead of installing a CA. The certificate authenticates
*the backend*; the shared token in `Hello` authenticates *the service*.

So a service does not have to be handed a file out of band, the certificate is
published over HTTP:

```
GET /ai/certificate          # no auth; 404 when the bridge is disabled
{ "protocol": "hab/1",
  "certificate_pem": "-----BEGIN CERTIFICATE-----\n...",
  "fingerprint_sha256": "6745e8..." }
```

Unauthenticated on purpose: a server certificate is handed to every peer during
the TLS handshake anyway, so publishing it discloses nothing. The private key
never leaves the process, and the shared token — the thing that actually
authenticates a service — is *not* served here.

**A service must re-fetch this on every reconnect, not once at startup.** With
no PEM pair configured the bridge re-selfsigns at each boot, so a certificate
pinned once goes stale the moment the backend restarts; a reconnect loop that
only redials would then fail forever. The service startup sequence is: `GET
/ai/certificate` → trust that PEM → dial QUIC → `Hello` → serve; on any
connection loss, start again from the fetch.

Note what this is not: fetch-then-pin over plain HTTP is trust-on-first-use,
only as trustworthy as that HTTP hop. On an untrusted network set
`AI_TLS_CERT`/`AI_TLS_KEY` to a real certificate and distribute it out of band
— the endpoint then simply serves that, stable across restarts.

## Chatbot

Every signed-in user — `parent` included — keeps private threads with an
AI service. The backend is a **relay**: it owns auth, the limits, the thread,
the payload format, and the last look at the answer before anyone sees it; the
words are the service's. There are deliberately **no rule, intent or
canned-answer tables** — the thread is free-form, and a school that wants
different behaviour changes its AI service, not this backend. A thread is
private to its owner: no teacher, no admin, nobody reads someone else's, and
another user's thread is a `404`, never a `403`.

### Sending is two-phase, and that is the point

`POST /chatbot/threads/{id}/messages` answers `202 {message_id, status:
"pending"}` instead of the answer, because an inference outlives a normal
request (and the request-timeout layer that guards every handler). Before the
AI is even called, two rows are written: the user's turn, and an **empty
`pending` assistant row** — `message_id` is that row. That order is the whole
design: a reload, a dropped connection, a closed laptop can no longer lose an
answer, because the place it will land already exists and the answering task
outlives the HTTP request.

The client then reads that row either way it likes:

- **poll** `GET /chatbot/threads/{id}/messages/{mid}` — `pending` until it
  settles, then `complete` with `content` or `failed` with `error_code`;
- **stream** `GET /chatbot/threads/{id}/messages/{mid}/stream` — the same
  row over Server-Sent Events.

Both read the same row, so they can never disagree, in any state.

### The SSE stream

```
event: delta
data: {"text":"Newton'un ikinci yasası "}

event: delta
data: {"text":"kuvvet ile ivme arasındaki …"}

event: done
data: {"message":{ …the message DTO… }}
```

One to eight `delta` events, then exactly one `done` carrying the finished
message; or, if the turn failed, a single `error` event
(`{"code":"timed_out","message":"…"}`) and nothing else. Then the stream
closes. A stream opened *after* the answer already landed still emits
`delta`s and a `done` — the replay is deliberate, so a client that connects
late (or reconnects) never hangs waiting for events that already happened.

**The deltas are sliced by the backend from the finished answer.** `hab/1` is
unary — the service returns the whole reply text, not tokens — so today the
chunking is cosmetic pacing, not real streaming. The event shape exists now so
that the day a service streams, it becomes a drop-in change behind an
unchanged frontend.

### The message DTO

```json
{ "id": "01J8…", "thread_id": "01J8…", "role": "assistant",
  "status": "complete", "content": "…", "truncated": false, "error_code": null,
  "created_at": 1761820800000, "completed_at": 1761820803120 }
```

`role` is `user` or `assistant`; `status` is `pending`, `complete`, or
`failed` (a user turn is always `complete`). `content` is empty while pending,
`error_code` is set only when failed, `completed_at` is `null` until the turn
settles. `truncated` is `true` when `content` is only the first part of what
the assistant answered — the rest was over `max_chatbot_message_len` and was cut;
it is always `false` for a user turn and for a failed one. Show it: a clipped
answer that looks whole is worse than one the reader knows is clipped. The same
DTO is what the SSE `done` event carries, so the streaming and polling reads
agree about `truncated` exactly as they do about every other field.

### Status codes

| code | when |
| --- | --- |
| `202` | the turn was accepted; the answer is on its way |
| `400` | empty message, or longer than the school's `max_chatbot_message_len` |
| `404` | no such thread or message — or not the caller's |
| `409` | already at `max_chatbot_threads`; delete a thread first |
| `429` | over `RATE_LIMIT_CHATBOT_PER_MINUTE` messages/minute for this **user**; see `Retry-After` |
| `503` | no connected service offers `chat.reply` — **nothing was written**, retry later |

The `503` ordering matters: the bridge is checked before the rows exist, so an
unavailable service leaves no dead `pending` row behind, and the rate limit is
charged before that, so a refused turn leaves no trace at all.

### A turn always settles

Once the two rows exist the answer never stays `pending` forever, through
three independent mechanisms:

- the answering task stamps `complete` or `failed` itself — the normal path;
- a reader **projects** a `pending` row older than 300 seconds as
  `failed`/`timed_out` (a read-time projection, not a write — the row is left
  for the task that may still own it);
- a **boot sweep** flips every leftover `pending` to `failed`/`interrupted`,
  which is what a process death mid-inference looks like.

Two more verdicts come from inspecting the answer: an **empty** reply is
reported as `failed`/`empty_reply` (a blank bubble is indistinguishable from a
bug), and an **over-long** one is truncated to `max_chatbot_message_len` — a
clipped answer still helps, a discarded one does not — with `truncated: true`
on the stored turn, so the clip is never passed off as the whole answer.

`error_code` values a client may see: `unavailable` (no worker, or the bridge
is off), `busy`, `timed_out`, `transport`, `protocol`, `bad_reply` (the
service's payload did not parse), `empty_reply`, `service_error`,
`interrupted`, `not_found`, `internal` — **or a code the AI service itself
returned**, verbatim. Treat it as an open set: branch on the ones you handle,
fall back to the human-readable text for the rest.

### The `chat.reply` capability (for AI-service authors)

A chat service declares `chat.reply` in its `Hello` (see "AI bridge (QUIC)").
Requests then arrive as ordinary `hab/1` `Request` frames whose `payload` is:

```json
{ "message": "and the second law?",
  "history": [ {"role":"user","content":"what is the first law?"},
               {"role":"assistant","content":"an object at rest …"} ] }
```

`history` is **oldest first** (index 0 is furthest back), optional (absent or
`[]` = a fresh thread), and **excludes** the new message — that one is
`message`. It carries the last `chatbot_history_turns` **settled messages** —
entries, not exchanges: the default `10` is five question/answer pairs, and
`history` is exactly that many entries long whenever the thread holds enough
of them. Only settled turns count: a failed answer is skipped and the window
reaches further back for a usable one, rather than coming up short. All of it
is re-sent on every request, which is why the service may be stateless: it can
restart, scale out, or be replaced mid-thread without losing context.

The answer is a `Response::Ok` whose payload is:

```json
{ "text": "the complete reply, not chunked, not partial" }
```

Failures reuse the existing `Response::Err {code, message}` — one failure
shape on the bridge for every capability — and that `code` reaches the client
as the message's `error_code`.

**Unknown extra keys are ignored on both sides, on purpose.** A service may
add fields (a model name, token counts) and a future backend may send more
context without either end having to be redeployed in lockstep.

## Layout

```
src/
  main.rs          bootstrap: config, db, router, serve
  lib.rs           build_router: routes + OpenAPI/Swagger + CORS + /health
  config.rs        env-sourced Config
  constant.rs      validation limits
  validate.rs      field validators (used by every newtype's try_new)
  error.rs         ValidationError + AppError -> HTTP responses
  database.rs      SurrealDB server connect (ws) + SCHEMAFULL migration
  rate_limit.rs    fixed-window limiter: per-IP tiers + middleware, per-user chat tier
  state.rs         AppState { db, files_path, cookie_secure, rate_limit, ai }
  ai/              QUIC bridge to the out-of-process AI services
                   (see "AI bridge (QUIC)"; the HTTP half is web/ai.rs)
    protocol.rs    Hello/Greeting/Request/Response + length-prefixed JSON framing
    server.rs      AiBridge: listener, handshake, dispatch over per-request streams
    registry.rs    connected workers, capability routing, least-inflight leases
    tls.rs         listener certificate (PEM or self-signed) + fingerprint
    error.rs       AiError
    chat.rs        the `chat.reply` payload contract (ChatRequestPayload/ChatReplyPayload)
  domain/          validated newtypes + entities (derive SurrealValue),
                   each owning its persistence
    user.rs        UserId · Username · Password · PasswordHash · User (has role)
    role.rs        Role enum (student < teacher < manager < admin), at_least()
    field_update.rs FieldUpdate: one UPDATE ... SET built from only the fields a
                   PATCH actually carried (an omitted field is never written)
    monotonic_id.rs next_ulid: ids that sort in write order — one process-wide
                   Generator, so same-millisecond rows never scramble
    text_fold.rs   case- and diacritic-insensitive folding for search, shared by
                   the Rust needle and the SurrealQL column (Turkish İ/ı, ü, ö…)
    session.rs     SessionId · SessionToken · Session (7-day expiry)
    timestamp.rs   Timestamp (unix-millisecond instant)
    note.rs        NoteId · NoteTitle · NoteContent · Note
    note_file.rs   NoteFileId · FileName · FileContentType · NoteFile (metadata row;
                   blob on disk under FILES_PATH, named by the row's ULID)
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
    question_image.rs QuestionImageId · QuestionImage (question/choice picture
                   metadata; bytes on disk under FILES_PATH)
    answer_image.rs AnswerImageId · AnswerImage (student answer-drawing
                   metadata; bytes on disk under FILES_PATH)
    bank_question.rs BankQuestionId · BankQuestion (school-wide reusable
                   question template, copied into exams; no exam FK)
    bank_question_image.rs BankQuestionImageId · BankQuestionImage (bank
                   question/choice picture metadata; bytes on disk under FILES_PATH)
    profile.rs     PersonName · Email · Phone · BirthDate (personal-info newtypes)
    preferences.rs Theme · Language (own UI preferences)
    message.rs     MessageId · MessageSubject · MessageBody · MessageLabel ·
                   Message (per-copy folders: inbox/sent/archive/trash)
    parent_link.rs ParentLinkId · ParentLink (parent↔student tie = the parent's read grant)
    registration.rs RegistrationId · Registration (a seat on a registration event's signup list)
    subject.rs     SubjectId · SubjectName · SubjectDescription · Subject (course curriculum)
    term.rs        TermId · TermName · Term (school term window)
    settings.rs    ExamKindDef · GradeBand · Settings (per-school policy)
    chatbot_thread.rs ChatbotThreadId · ChatbotThreadTitle · ChatbotThread (one
                   chatbot thread, private to its owner)
    chatbot_message.rs ChatbotMessageId · ChatContent · MessageRole · MessageStatus ·
                   ChatbotMessage (one turn; the assistant row is written
                   `pending` before the AI call and settled after)
    appointment_slot.rs AppointmentSlotId · SlotSeries · SlotNote ·
                   AppointmentSlot (a teacher's published availability; a
                   recurring publish is expanded into rows sharing a series id)
    appointment.rs AppointmentId · AppointmentStatus · AppointmentReason ·
                   Appointment (a booking on a slot; occupancy and overlap are
                   derived under APPOINTMENT_LOCK, never stored)
    pomodoro.rs    PomodoroSessionId · PomodoroSession (student focus log)
    pool_question.rs PoolQuestionId · PoolQuestionTitle · PoolQuestionBody ·
                   PoolQuestion (student-asked question; teacher-approved into
                   the school-wide pool; optional photo as metadata + disk blob)
    solution.rs    SolutionId · SolutionBody · Solution (discussion thread on an
                   approved pool question; dies with the question)
    homework.rs    HomeworkId · HomeworkTitle · HomeworkDescription · Homework
                   (per-course assignment; required subject; optional `assigned`
                   student subset — absent/empty = the whole enrolled course)
    homework_submission.rs HomeworkSubmissionId · SubmissionText ·
                   HomeworkSubmission (one row per homework+user; immutable
                   submitted_at + moving updated_at — `late` is computed)
    homework_file.rs HomeworkFileId · HomeworkFile (submission attachment
                   metadata; any content type; bytes on disk under FILES_PATH)
    homework_result.rs HomeworkResultId · HomeworkStatus · HomeworkResult
                   (teacher grade: done/incomplete/missing + optional Mark; one
                   row per homework+user — its existence freezes the submission)
  web/             axum layer: DTOs (serde + OpenAPI schemas) + handlers +
                   auth extractors
    extractor.rs   CurrentUser · RequireTeacher · RequireManager · RequireAdmin
    dto.rs         shared UserResponse · CourseResponse · ExamResponse · SessionResponse schemas
    exam_ws.rs     the student exam-room WebSocket (state ticks, autosave, finish)
    page.rs        PageParams · Page<T> (shared pagination)
    etag.rs        conditional-GET middleware: ETag over a 200 JSON body,
                   If-None-Match → 304 (GET only; SSE and blobs pass through)
    auth.rs  users.rs  notes.rs  messages.rs  events.rs  appointments.rs
    courses.rs  subjects.rs  sessions.rs  exams.rs  homework.rs  questions.rs
    bank_questions.rs  marks.rs  work.rs  pomodoro.rs  attendance.rs
    settings.rs  terms.rs  ai.rs  chatbot.rs
```

Tests: `cargo test` — unit (in-source), integration (`tower::oneshot` + in-memory
db), rate-limit (both tiers, proxy-header and peer-address keying, shipped
limits over every route), e2e (real TCP + reqwest cookie jar), persistence
(tempfile file engine, including close + reopen), ai-bridge (real QUIC on
loopback against a fake AI service), ai-protocol (the `hab/1` wire contract,
driven by a client that shares no code with the backend).
