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
archive/trash with a read flag the sender sees as a receipt (the only place a
`parent` writes). Attendance is event + attendees: create an event with an **audience**
(the whole school, one role, a course's enrollment, a **class section's**
roster, or a **registration** signup list — omit for school-wide), then teachers mark the expected attendees
present / absent / late / excused (students never self-mark), and a **roster
report** shows who was expected and who missed. Registration lists fill seat
by seat: teachers register students (never the other way round), staff
register only themselves, an optional `capacity` caps the seats, and the list
closes the moment the event starts (or, for an event with only an end time —
a pure signup deadline — the moment that end passes). Every event stays visible to everyone —
the audience is a roster, not a wall. Marks are course-shaped
(Google Classroom style): a teacher creates a course — kind **`course`** (a
regular taught course), **`study`** (a supervised study session — *etüt*), or
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
and a demotion below `teacher` sweeps their assignments away. Students are
grouped into a **class section** (*şube* — 9-A, 10-B) when a school teaches
that way: it is bulk enrollment, not a second kind of membership — attaching a
course to it enrolls the whole roster, adding a member enrolls them into every
course already attached, and what lands are ordinary enrollment rows tagged
with the section that pumped them (untagged = placed by hand, and hand-placed
rows are never adopted and never swept). A course without room for everyone
refuses the whole operation, naming it (see "Class sections (şube)"). A section
may also
name a **homeroom teacher** (*sınıf öğretmeni*, `teacher_id` — any teacher+
account, cleared automatically when that account is demoted), and while every
other class read is teacher+, a student reads their own section at
`GET /classes/me` (staff and a linked parent read anyone's at
`GET /classes/user/{user}`). A grade can also carry a **blueprint** — the
course list every section at that grade takes (`/classes/blueprints`), applied
to each section best-effort, with anything a limit refuses returned in
`skipped` and anything a human attached by hand left alone; a section created
afterwards at that grade is stocked by `POST /classes` itself, which reports it
as `stocked_from`; `GET /classes?grade=<label>` lists the sections carrying a
label (matched exactly, `?grade=` alone the ones carrying none), and
`GET /classes/blueprints/{grade}/status` names, per section, the template
courses it is still missing — which is how a partial pump is chased down after
the response that reported it is gone. Students read a
per-course weighted average and
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
**homework**: assigned to the whole course or a named subset (the unnamed never
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
also carries its own **UI preferences** — theme (`light`/`dark`), language
(`tr`/`en`), and accent color (`palette_color`, a 6-digit hex like `#fefae0` —
any hex, deliberately not a fixed palette) — self-managed, admin-editable for
anyone, `null` until chosen so the client can fall back to the device
preference (or, for the accent, its own default).
Every account also has a **public profile** (`/users/{id}/profile`): a
self-chosen `display_name` and `bio`, one **avatar**, the classes and courses
it belongs to, counters computed at read (finished pomodoro stints, course and
class totals) beside stored lifetime tallies of what the account has done —
homework handed in, exams sat and focus time, lessons attended, high exam
marks and the longest run of consecutive study days for a student; grades
given, lessons held and question-pool approvals for a teacher — and the
**badges** those tallies have earned — auto-earned only, no route awards or
revokes one, and permanent once earned even if the counter later falls. Contact details are deliberately not part of it — email,
phone and birth date keep the gate they already have — and any authenticated
account reads any profile, except a `parent`, who reads their own and their
linked students' only (see "User profiles & avatars").
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
Any account from `student` upwards can also open a **collaborative
whiteboard** (a `parent` gets no whiteboard access at all — it can neither open
one nor be invited onto one): a titled board whose membership is an **ad-hoc
invite list** —
the creator names participant user ids, and no course, session or appointment
is involved. Every participant draws over a WebSocket, the creator alone
clears, locks, closes or deletes, and someone who is not on a board gets a
`404` for it on every route, existence included. Strokes are **append-only and
a clear deletes nothing** — it bumps the board's epoch, so the live canvas
empties while every mark ever drawn stays stored and replayable through the
history reads (see "Collaborative whiteboard").
The school's **food program** is published here too: a manager puts up one
**menu** per calendar day and meal slot (`date` as `YYYY-MM-DD` text, `slot`
drawn from the school's `meal_slots`, unique per pair), lists its **dishes**
with dietary tags from the school's own list, and prices them in **minor
units** (kuruş) as integers — this API never speaks decimals or floats about
money. A student's own **dietary profile** is tagged from that same list
(manager-written — an allergen list is a school record, not a self-service
preference), so every dish a menu read returns names the tags it `conflicts`
with for whoever is reading. Every authenticated user reads what is being
served; students (or their parents) book a seat, and every seat writes a line into an
**append-only** meal ledger whose balance is derived and never stored (see
"Food program: menus, dishes, bookings & the ledger").
School **fees** are tracked the same way, in a ledger of their own: a manager
writes a **fee plan** (a name and 1–60 installments, each an amount in minor
units and a due date that may sit in the past) and **assigns** it to students,
which appends every installment as a `charge` at once — there is no scheduler,
and "overdue" is derived at read time. Payments are recorded against one named
charge, refunds against one named payment, and nothing is ever edited or
deleted: a mistake is corrected by appending the opposing line. **There is no
online payment integration and none is planned** — no gateway, no card data,
`method` is free text. Teachers see no money at all (see "Payments").

Every field is a validated newtype (`Username(String)`, `NoteTitle(String)`, …)
constructed only after its restrictions pass — invalid input can't be
represented. Those restrictions are published rather than left to be guessed:
**`GET /limits`** (no auth) serves every fixed bound and closed value set the
API enforces, read straight from the constants the newtypes use, so a frontend
validates against the server's own rules instead of a hand-kept copy (see
"Validation limits"). Those same types derive `surrealdb::types::SurrealValue`, so one
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
answers `201` either way with the *same* body — `{username, role}`, the echoed
name and the `student` role every fresh account gets. Both outcomes return one
value built before the insert is even attempted, so they are byte-identical by
construction. This is deliberate: the route is unauthenticated, so a `409` (or a
faster reply) would let anyone enumerate the school's users. The password is
hashed *before* the availability check so both outcomes cost the same ~33ms.

The reply carries **no `id`**: on the taken path there is no row to name, and a
fabricated one would leave the client holding an id that matches nothing. Log
in and read `GET /auth/me` to learn who you are. The accepted cost: a caller who
collides with an existing account gets no distinct error and simply cannot log
in with that password — they pick another name. Do not "fix" this back to a
`409`, and do not add an `id` back.

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
- **Times-of-day are UTC too, and staff enter them that way.** A meal slot's
  `serving_minute` (minutes past midnight, `0`–`1439`) is the one clock-face
  value a school types in, and it is read as UTC on the menu's `date` — there
  is deliberately **no school-timezone setting** (a rejected feature), so a
  UTC+3 school enters `540` (09:00 UTC) for a meal served at noon locally.
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

## Validation limits

Every fixed bound the API enforces is published at `GET /limits` (no auth), so
a client never has to hard-code a copy that drifts the day a constant moves.
The handler reads `src/constant.rs` directly — a changed constant changes the
response in the same commit.

```json
{
  "user":          { "min_username_len": 3, "max_username_len": 32,
                     "username_separators": [".", "_", "-"],
                     "reserved_usernames": ["admin", "…"],
                     "min_password_len": 6, "max_password_len": 128,
                     "max_name_len": 100, "max_display_name_len": 50,
                     "max_bio_len": 500,
                     "max_profile_courses": 20, "max_profile_classes": 5,
                     "max_email_len": 254,
                     "min_phone_digits": 7, "max_phone_digits": 15,
                     "roles": ["parent", "student", "teacher", "manager", "admin"],
                     "themes": ["light", "dark"], "languages": ["tr", "en"],
                     "palette_color_pattern": "^#[0-9a-fA-F]{6}$", "palette_color_len": 7,
                     "session_duration_days": 7 },
  "badges":        { "catalog": [ { "id": "homework_submitted_10",
                                    "stat": "homework_submitted", "threshold": 10 },
                                  { "…": 0 } ],
                     "high_mark_min": 90 },
  "note":          { "max_title_len": 200, "max_content_len": 10000, "max_files": 10 },
  "file":          { "max_name_len": 255, "max_content_type_len": 100,
                     "min_max_file_bytes": 1024, "max_max_file_bytes": 26214400,
                     "default_max_file_bytes": 5242880,
                     "image_content_types": ["image/png", "image/jpeg", "image/webp", "image/gif"] },
  "message":       { "max_subject_len": 200, "max_body_len": 10000, "max_label_len": 50 },
  "event":         { "max_title_len": 200, "max_description_len": 2000 },
  "course":        { "kinds": ["course", "study", "club"], "max_title_len": 200, "…": 0 },
  "exam":          { "modes": ["sync", "async", "open"],
                     "question_kinds": ["choice", "text"],
                     "min_duration_ms": 60000, "max_duration_ms": 86400000,
                     "max_attempts": 100, "unlimited_attempts": 0,
                     "min_mark": 0, "max_mark": 100, "…": 0 },
  "homework":      { "statuses": ["done", "incomplete", "missing"],
                     "max_files_per_submission": 10, "max_assigned": 200, "…": 0 },
  "question_pool": { "max_title_len": 200, "max_body_len": 10000, "max_solution_body_len": 10000 },
  "appointment":   { "max_note_len": 500, "max_reason_len": 1000, "max_slot_occurrences": 52 },
  "chatbot":       { "max_message_len": 8000, "max_thread_title_len": 200,
                     "min_max_message_len": 100, "…": 0 },
  "board":         { "max_title_len": 200, "max_participants": 200,
                     "max_stroke_payload_len": 4096,
                     "max_epoch_strokes": 5000, "max_board_strokes": 50000,
                     "max_boards_per_creator": 200,
                     "stroke_kinds": ["stroke", "clear"],
                     "ws_tick_secs": 15, "ws_max_board_id_len": 64 },
  "settings":      { "max_list_len": 20, "max_item_len": 50,
                     "min_exam_kind_weight": 1, "max_exam_kind_weight": 100,
                     "max_grade_bands": 20, "max_grade_label_len": 20,
                     "required_attendance_statuses": ["present", "absent", "late", "excused"] },
  "request":       { "max_page_limit": 500, "schedule_past_grace_ms": 60000,
                     "request_timeout_secs": 30 },
  "rate":          { "window_secs": 60, "auth_per_minute": 10,
                     "api_per_minute": 300, "chatbot_per_minute": 20 }
}
```

The `rate` group is the one part that is **not** compile-time: those tiers are
environment-tunable, so the endpoint serves *this* server's live values (read
from its running limiters), not the shipped defaults. `0` means the tier is
off. A client should read its budget here rather than discovering it by
collecting a `429`.

### Two surfaces, one source

The bounds are published twice, on purpose, because clients consume them two
different ways:

| Surface | Shape | Use it when |
|---------|-------|-------------|
| `GET /limits` | One JSON document, grouped by resource, closed value sets included | Runtime fetch. No codegen step, survives a deploy skew, and carries things OpenAPI holds awkwardly (`reserved_usernames`, `image_content_types`) |
| `/api-docs/openapi.json` | Per-field `maxLength` / `minLength` / `minimum` / `maximum` / `maxItems` on the request schemas | Build-time codegen — `openapi-typescript`, `orval`, and friends turn these into types *and* validators automatically |

Both are generated from the same `src/constant.rs`, and **neither is allowed to
drift from it**, which is enforced rather than asked for:

- `tests/limits_completeness.rs` parses every `pub const` out of `constant.rs`
  and fails unless each one is either referenced by `src/web/limits.rs` or
  listed as a deliberate exclusion *with a reason*. A new constant breaks the
  suite until someone decides, consciously, whether clients need it.
- `tests/spec_bounds.rs` builds the OpenAPI document, reads all 168 published
  bounds back out of the emitted JSON, and asserts each equals its constant.
  This exists because utoipa's `#[schema(max_length = …)]` accepts a **literal
  only** — a `const` there does not compile — so the annotations are
  unavoidably a second copy of each number. The test is what makes that copy
  safe. It also pins the `limit` query parameter's `maximum` to
  `MAX_PAGE_LIMIT`, a duplication `constant.rs` previously only *asked* a human
  to maintain.
- A third check in `limits_completeness.rs` refuses to let a validation bound
  be declared **outside** `constant.rs` at all. Without it a limit can hide in
  a domain module and reach neither surface — which is exactly where
  `MAX_CHATBOT_THREAD_TITLE_LEN` was found sitting, private and unpublished.

Notes:

- **No auth.** The registration and login forms need the username and password
  bounds before a session exists, and none of these are secrets — they are the
  same rules a `400` already spells out in prose.
- **Answers during a database outage.** `/limits` touches no row, so it is
  exempt from the guard that answers `503` while the database reconnects — a
  frontend booting against a degraded backend is exactly when it needs the
  contract. Every route that does read the database still refuses.
- **Fetch once.** The values are compile-time constants, so the response only
  changes with a deploy. Cache it for the session; the `ETag`/`304` path makes
  a revalidation cheap if you'd rather re-check.
- **`/limits` is not `/settings`.** School-adjustable policy — the exam kinds
  and their weights, the attendance statuses, the grade bands, the live
  `max_file_bytes` and chatbot knobs — is on `GET /settings` and changes when a
  manager edits it. What `/limits` carries for those knobs is the fixed range a
  manager may set them *within* (`min_`/`max_`/`default_` prefixes), plus the
  attendance statuses no school may remove.
- **Open sets travel as a pattern.** `palette_color` accepts any hex accent
  color, so `/limits` publishes `palette_color_pattern` (a regular expression)
  and `palette_color_len` instead of a value list — validate the shape, not
  membership in a palette. The pattern describes what the server *accepts*, so
  it matches either case; the stored (and returned) value is lowercased, which
  is the one place a response may differ in case from the request.
- **Closed value sets ride along.** `roles`, `themes`, `languages`, course
  `kinds`, exam `modes`, `question_kinds`, homework `statuses`, board
  `stroke_kinds`, and the
  uploadable `image_content_types` are the exact accepted spellings — build
  pickers from these rather than from a literal list.
- **The badge catalog rides here too.** `badges.catalog[]` is every badge the
  system can auto-award — `{id, stat, threshold}`, 34 of them, in catalog
  order. It is a group (an object with one `catalog` key) rather than a bare
  array because every key of this document is a group, and that uniformity is
  what lets a client walk the response generically. `stat` is the API's name
  for the lifetime counter behind the badge (`homework_submitted`,
  `homework_on_time`, `exam_sat`, `pomodoro_finished`, `pomodoro_focus_ms`,
  `marks_given`, `lessons_held`, `pool_approved`, `pool_published`,
  `lessons_attended`, `high_mark`, `study_streak` — twelve counters over the
  34 badges), deliberately not the database column it is stored in, so storage
  can be renamed without moving a published contract; badges sharing a `stat`
  form a ladder. Every one of them joins the profile's `stats` key of the same
  name plus `_total`, mechanically and without exception — including
  `study_streak_total`, which is a *longest run* rather than a sum and keeps
  the suffix anyway so the join needs no special case. The catalog is compiled
  in, so a threshold moves only with a deploy — the rules stay reviewable in a
  diff instead of editable in a settings row. Labels and icons are **not**
  here: like `roles` and course `kinds`, the id is the whole contract and the
  client owns what it looks like. Beside it, `badges.high_mark_min` (90) is the
  exam mark the `high_mark` ladder counts from — published because
  `high_mark_10` does not say what a high mark is, and deliberately not the
  school's grade bands, whose labels a school renames at will while a badge id
  must mean the same thing in every deployment forever.

**`422` versus `400`.** The two refusals mean different things and a client
must not treat them alike. Every JSON-bodied route can answer **`422`**: the
body never became the type the handler asked for — a field of the wrong type,
or a required field missing. It answers with a **`text/plain` diagnostic**, not
the `{"error": …}` envelope every other failure carries — e.g. `Failed to
deserialize the JSON body into the target type: username: invalid type:
integer 5, expected a string at line 1 column 14` — so a client must not try to
parse it like the other errors: log it, and show the user the field rules
`/limits` publishes. **`400`** is the other failure and it does carry the usual
`{"error": "…"}`: either the bytes were not JSON (`{not json`), or the body
parsed cleanly and then broke a domain rule — `{"username":"a"}` answers
`400 {"error": "username must be at least 3 characters (got 1)"}`. So `422`
says the shape is wrong and `400` says the value is. **Multipart uploads never
answer `422`** — a bad boundary or the wrong content type on a file route is a
`400`, since there is no JSON body to reject. Each JSON operation declares its
`422` in the OpenAPI spec and a drift test (`spec_bounds.rs`) checks that in
both directions, so the spec cannot fall behind the handlers and the multipart
routes cannot quietly gain a status they never return.

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

All three tiers admit from memory — no request ever waits on the database to be
let in — but the window **outlives the process**. A background task folds each
tier's new admits into one shared `rate_limit` row per tier + client + minute
every 2 seconds, then caps the in-memory bucket by the total that comes back,
so a restart mid-minute does not hand every client a fresh budget. One caveat:
a just-started process can spend up to the tier's budget in the 2 seconds
before its first fold tightens it. If the database is down the tiers simply
fall back to their own local budgets — nobody is refused for it.

One client, one bucket, so the bucket map is only as bounded as the client
set — and a single IPv6 /64 is not bounded at all. Past 10 000 live clients a
new key first sweeps out lapsed windows, then evicts the least-spent live
buckets, and never a bucket that has reached its limit (freeing one would hand
back the refusal it was enforcing). If every bucket is exhausted there is
nothing to evict, and a client that has never been seen gets no bucket of its
own: those clients are metered together against one shared budget of 600
requests a minute for all of them. Refusing them outright would let an attacker
who can fill the map `429` the whole school; admitting them freely would make a
filled map the way to buy unmetered throughput. Only reachable under a
deliberate flood, and only newcomers during it are affected — every client
already in the map keeps its own counter.

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
course enrollments. A demotion to `parent` also gives back the seats that
account holds on still-open **event signup lists** — a parent cannot reach
`DELETE /events/{id}/register/{user}` and no one else may free a non-student's
seat, so those seats would stay claimed forever and a capped event would answer
"full" for good. Every other role change leaves signups alone: staff free their
own seats by hand. Seats on lists that have already closed (the event started,
or its `ends_at`-only deadline passed) are never touched — that roster is
history, and re-registering is refused.

The role write and every sweep it implies — class memberships and their
counters, all enrollments and their seats, parent ties on both sides, a
demoted parent's still-freeable event seats, whiteboard rosters, course
staffing and homeroom-teacher columns — commit as **one transaction**. It
either all lands or none of it does: a failure answers `500` with the account
still holding its old role and every grant of it still standing, and the same
`PATCH` retried applies the lot. Signup lists that have already frozen are
still left exactly as they stand.

| Action                                   | Minimum role | Notes                                         |
|------------------------------------------|--------------|-----------------------------------------------|
| Register / login / view own account      | (any)        | Registration always creates a `student`       |
| View events, own notes; CRUD notes + their files | student | Everyone can read events and keep notes; note files (upload/download) are walled per owner like the notes themselves |
| Send / read / file / delete messages     | (any)        | One-to-one, any user to any user (`parent` included — the role's one write); each party only ever touches their own copy |
| Mark event attendance; remove attendance rows | teacher | Only users in the event's **audience** can be marked; students never mark — a teacher+ may mark anyone expected, themselves included |
| Create events                            | teacher      | The audience (school / role / course / class / registration) is set at creation and editable later |
| Register users onto a registration event | teacher      | Teachers place **students** (students never register themselves) and take a seat for **themselves** — never for another staff member. Unregistering mirrors the same rule |
| List an event's attendance or its roster report | teacher | Students read their own tallies via the attendance report |
| Edit / delete an event                   | teacher      | Only the **creator**, or a `manager`+ for any event — in both cases only while still `teacher`+ |
| View a course's sessions                 | student      | Only inside **visible** courses: enrolled, creator, assigned teacher, or `manager`+ |
| List a session's roll call               | teacher      | The **session's teacher** (while still `teacher`+), or anyone with course-management rights |
| Create / edit / delete a course session  | teacher      | Course-management rights (course creator, an assigned teacher, or `manager`+) |
| Take a session's roll call (mark/remove **enrolled students**) | teacher | The **session's teacher** (while still `teacher`+), or anyone with course-management rights; only students sit on a roster |
| Mark / remove the **session teacher's** presence row | manager | Staff presence is management's call — the teacher can't self-mark |
| Work check-in / check-out; view **own** work log | teacher | Instants are server-stamped, never client-supplied |
| View / correct / delete **any** staff work log entry | manager | Corrections only on closed entries |
| Start / finish a pomodoro focus session; view **own** pomodoro log | student | **Students only** start; instants server-stamped; starting discards a dangling unfinished session |
| View **any** user's pomodoro log          | teacher      | Study oversight — same shape as `/pomodoro/me`, incl. the unpaged `total_focus_ms`; a `parent` reads their linked students' |
| Ask into the school question pool        | student      | **Students only** (exact); born `pending` — visible to the asker + teacher+ only; the asker may attach/replace/remove one photo while pending |
| Read the pool; offer / edit / withdraw own solutions | student | Every `approved` question is school-wide (parents stay out); a solution's author edits its body and photo anytime (solutions never freeze) and deletes it, teacher+ delete any |
| Approve a pending pool question; delete any question or solution | teacher | Approval publishes school-wide and **freezes** the content; rejection = deletion — moderation never edits, so teacher+ cannot rewrite a solution |
| Read the school **question bank**; save / instantiate bank questions | teacher | Every bank row is school-wide (reads + copy into an exam); saving one makes the caller its owner |
| Edit / delete a bank question (and its images) | teacher | **Owner only** (admin bypass), and only while still `teacher`+ — owning a template is history, not a standing grant; bank rows carry no exam link, so they never freeze |
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
| Delete a course                          | teacher      | Only the **creator**, or a `manager`+ — an assigned teacher runs the course but doesn't own it; a creator demoted below `teacher` owns nothing |
| Manage inside a course: edit it, enroll/unenroll **students**, add/edit/delete its exams and **subjects**, grade, remove results | teacher | The **course creator**, a teacher **assigned** to it, or a `manager`+ for any course — in every case only while that account is *still* `teacher`+, so a demoted creator keeps nothing; only students can be enrolled; a subject still referenced by exam questions or homework won't delete (`409`) |
| View a course's roster, an exam's result list / statistics | teacher | Course-management rights |
| Create / edit / delete a **class section** (şube); add or remove its members | manager | Reading classes, their rosters and their course lists is teacher+ — except `GET /classes/me`, which any authenticated user reads for themselves, and `GET /classes/user/{user}`, open to teacher+ and a linked parent; a member must be a `student`, a `teacher_id` must be teacher+ |
| Attach / detach a course on a class | teacher | Course-management rights on **that course** — attaching enrolls the whole class into it, so it takes exactly the right enrolling one student takes |
| Read another user's mark report          | teacher      | Narrowed to the caller's managed courses; `manager`+ sees all; a `parent` sees a linked student's in full |
| Grade students                           | teacher      | Target must be an **enrolled student**; grading never targets oneself |
| Assign / edit / delete homework; grade it; read its roster | teacher | Course-management rights; an `assigned` subset (≤ 200 enrolled students) can't be narrowed over existing work (`409`); a grade (`done`/`incomplete`/`missing` + optional mark) freezes the submission until removed |
| Submit **own** homework (text + files); read own submission and grade | student | **Students only** — staff never submit; enrolled + in the audience; editable until graded; ≤ 10 files of any type, each ≤ `max_file_bytes`, always downloaded as attachments |
| Read another user's homework report      | teacher      | Narrowed to the caller's managed courses; `manager`+ sees all; a `parent` sees a linked student's in full — statuses/marks/flags, never files |
| Edit **own** personal info (name, surname, email, phone, birth date) | student | Every account carries the same optional info fields |
| Edit **own** UI preferences (theme, language, accent color) | student | `null` until chosen — the client then follows the device preference |
| Read any user's **public profile** and avatar; upload/remove **own** avatar | student | A `parent` is the exception: own and linked students' only, the link and the target's live role re-read per call. Removing *another* user's avatar is admin-only moderation |
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

A course is a regular taught course (kind `course`, the default), an *etüt* (kind
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
enrollments). That sweep runs once, so an assignment landing in the same
instant would survive it: the assign call therefore re-reads the account's live
role **after** its write and answers `409` — dropping the assignment again — if
it has since fallen below `teacher`. Same guard, same wording, as a class's
homeroom teacher.

Ownership is **not** a standing grant. `creator` is a historical column that no
demotion sweeps (unlike the assignment list above), so every course-management
and course-ownership check re-reads the caller's *live* role first: a creator
demoted to `student` or `parent` keeps neither management nor deletion of the
course they made — it stays reachable to its still-`teacher`+ assignees and to
manager+, who can hand it to someone else. Nothing below is a right a caller
holds while under `teacher`. The column is deliberately **not** swept the way
the assignment list is: it answers "who made this", which stays true after a
demotion — it is the *grant* that is role-gated, not the history. The same
floor applies to the catalogs: a demoted creator's own course drops out of
their `/courses`, `/exams` and `/homework` lists, and comes back only if they
are enrolled in it, as any student would be. A session's `teacher` behaves
identically — teaching a session grants no roll call, and no view of it, once
the account is below `teacher`.

Course data is walled per course. A course, its exams, its sessions, and its
subjects are
**visible** only to its enrolled users, its creator, its assigned teachers,
and manager+ — a student
sees just the classes they were added to, and the `/courses` / `/exams`
catalogs are filtered accordingly. Teacher-level reads *inside* a course
(roster, results, statistics, the question list, answer sheets, the live
monitor) additionally need **course-management rights** (creator, an assigned
teacher, or manager+ — each of them still `teacher`+ today):
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

**Schedule window (`GET /events`, `GET /exams`).** Both lists additionally
accept `?starts_after=` and `?ends_after=`, UTC unix-millis, both optional and
independent (AND-ed when both are sent). `ends_after=T` keeps rows whose window
has not finished — `ends_at > T`, falling back to `starts_at > T` when `ends_at`
is `null`; `starts_after=T` keeps rows with `starts_at > T`. A row with no
schedule at all (an event without times, an exam with no `mode` or in `open`
mode) is excluded by either parameter. Supplying either one flips the order to
**ascending by schedule** (`starts_at`, falling back to `ends_at`; ties by id),
so `?ends_after=<now>&limit=20` gives the twenty *soonest* rows instead of the
twenty newest-created — with neither parameter the list is byte-for-byte the
newest-first list it always was. `total` is the count after visibility *and*
window filtering, before paging; negative values are a `400` naming the field.

| Method | Path                             | Auth    | Description                     |
|--------|----------------------------------|---------|---------------------------------|
| GET    | `/health`                        | no      | Liveness check                  |
| GET    | `/`                              | no      | Same as `/health`               |
| GET    | `/time`                          | no      | Server clock: `{now}` UTC unix-millis (frontend sync) |
| GET    | `/limits`                        | no      | Every fixed validation bound, grouped by resource — see Validation limits |
| GET    | `/swagger`                       | no      | Interactive API docs (Swagger UI) |
| GET    | `/api-docs/openapi.json`         | no      | Raw OpenAPI 3 spec              |
| GET    | `/ai/certificate`                | no      | The AI bridge's certificate (PEM + sha256) for a service to pin; `404` when the bridge is off |
| POST   | `/auth/register`                 | no      | `{username, password}` -> `{username, role}` (no `id`; new users are `student`); always `201`, even if the name was taken — see Auth model |
| POST   | `/auth/login`                    | no      | `{username, password}` -> cookie|
| POST   | `/auth/logout`                   | no      | Clear session (no-op if none)   |
| GET    | `/auth/me`                       | student | Current user (incl. `role` and personal info) |
| PATCH  | `/users/me`                      | student | Update own personal info, `display_name` and `bio` included (see below) |
| PATCH  | `/users/me/preferences`          | student | `{theme?, language?, palette_color?}` — own UI preferences (see below) |
| GET    | `/users/me/profile`              | student | Own public profile — what everyone else sees of the caller |
| POST   | `/users/me/avatar`               | student | Upload/replace own avatar: `multipart/form-data`, one `file` part, raster images only, ≤ the school's `max_file_bytes` -> `{content_type, size}` |
| GET    | `/users/me/avatar`               | student | Own avatar bytes (`404` if there is none) — the self alias of `/users/{id}/avatar` |
| DELETE | `/users/me/avatar`               | student | Remove own avatar (`404` if there is none) |
| GET    | `/users/me/students`             | parent  | The caller's linked students (refs, sorted by username) · paged |
| GET    | `/users/search`                  | teacher | `?q=<fragment>&role=<role?>` — find users by username/name fragment (pickers); refs only, no contact info · paged |
| GET    | `/users`                         | admin   | List all users · paged          |
| GET    | `/users/{id}/profile`            | student | Any user's public profile — a `parent` reads only their own and their linked students' (`403`) |
| GET    | `/users/{id}/avatar`             | student | The avatar bytes (`nosniff`, `private, no-store`); same reach as the profile |
| DELETE | `/users/{id}/avatar`             | admin   | Remove any user's avatar — the moderation path |
| GET    | `/users/{id}`                    | admin   | Get one user                    |
| PATCH  | `/users/{id}/role`               | admin   | `{role}` — set a user's role; promotion out of `student` drops the user's course enrollments (only students enroll), and a demotion to `parent` also frees their seats on still-open event signup lists |
| PATCH  | `/users/{id}/profile`            | admin   | Update any user's personal info, `display_name` and `bio` included |
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
| GET    | `/events`                        | student | List all events · paged · `?starts_after=&ends_after=` window (soonest first) |
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
| PATCH  | `/appointments/{id}/approve`     | teacher | Confirm a pending booking (slot's teacher, or manager+); `409` when settled, **while a counter-proposal stands** (only the requester may approve at a proposed time, via `/reschedule/accept`), when the effective window has already started, or when the time collides with another approved meeting of either side |
| PATCH  | `/appointments/{id}/reject`      | teacher | Turn it down (same rights); the slot frees up |
| PATCH  | `/appointments/{id}/cancel`      | student | `{reason?}` — call it off; **the requester only** (`403` for anyone else, the slot's teacher and manager+ included — they reject, or reschedule then reject); `409` once settled or the meeting has started |
| PATCH  | `/appointments/{id}/reschedule`  | teacher | `{starts_at, ends_at}` — counter-propose another time (same rights); the booking goes back to `pending`; `409` if the proposed window has already started |
| PATCH  | `/appointments/{id}/reschedule/accept` | student | `{proposed_starts_at, proposed_ends_at}` — requester only: accept the counter-proposal **as read**; this *is* approval at that time (the overlap guard runs again); `409` if the proposal has since changed, if none stands, if the booking is no longer pending, if that time has started, or if it collides with another approved meeting of either side |
| PATCH  | `/appointments/{id}/reschedule/decline` | student | Requester only: refuse the proposal — this **cancels** the booking, so the cancel deadline applies (`409` once the meeting's window has started) |
| POST   | `/courses`                       | teacher | `{title, description?, kind?, term_id?, capacity?}` — `kind` is `course` (default), `study` (etüt), or `club` (kulüp); `capacity` caps the roster (creator manages it) |
| GET    | `/courses`                       | student | The caller's visible courses: created + enrolled (manager+: all) · paged |
| GET    | `/courses/me`                    | student | The caller's **enrolled** courses · paged |
| GET    | `/courses/{id}`                  | student | Get course (enrolled, creator, assigned teacher, or manager+) |
| PATCH  | `/courses/{id}`                  | teacher | Edit course incl. `kind` and `capacity` (`null` lifts the cap) (course manager; `409` if the `term_id` it moves off changed since the read — nothing written, re-read and retry) |
| DELETE | `/courses/{id}`                  | teacher | Delete course + its exams, results, subjects, sessions, and homework (submissions, files, and grades included) (creator, or manager+ — **not** an assigned teacher; `409` while anyone is still enrolled) |
| POST   | `/courses/{id}/teachers`         | manager | `{user_id}` — assign a **teacher+** to run the course (idempotent; returns the course; `409` + undo if that account is demoted mid-request) |
| DELETE | `/courses/{id}/teachers/{user}`  | manager | Unassign a teacher (`404` if they weren't assigned) |
| POST   | `/courses/{id}/enrollments`      | teacher | `{user_id}` — enroll a **student** (idempotent upsert; course manager; only students can be enrolled; `409` once a capped course is full); enrolling a class-pumped student clears the row's `source`, so a class sweep can no longer take them back |
| GET    | `/courses/{id}/enrollments`      | teacher | List the course roster (course manager) — each row carries `source`, the class that pumped it or `null` for hand-placed · paged |
| DELETE | `/courses/{id}/enrollments/{user}` | teacher | Unenroll (keeps recorded results; course manager) |
| POST   | `/classes`                       | manager | `{name, grade?, term_id?, teacher_id?}` — create a class section (şube); `grade` is a free-text year label, `teacher_id` the homeroom teacher (sınıf öğretmeni, teacher+; `409` + full rollback if that account is demoted mid-request). Stocked at once from its grade's blueprint when one covers it, so the `201` is `{class, skipped, stocked_from}` |
| GET    | `/classes`                       | teacher | List classes, newest first · paged; `?grade=` narrows to one grade label (matched exactly; `?grade=` alone lists the sections with no grade, an unknown label an empty page) |
| GET    | `/classes/me`                    | any     | The caller's own classes, newest membership first · paged; `creator` is `null` below teacher+ |
| GET    | `/classes/user/{user}`           | teacher | Another user's classes (a parent linked to that student may read it too) · paged; `creator` is `null` below teacher+ |
| GET    | `/classes/{id}`                  | teacher | Get one class                   |
| PATCH  | `/classes/{id}`                  | manager | Edit a class (`null` clears `grade`/`term_id`/`teacher_id`; `409` if the `term_id` it moves off changed since the read — nothing written, re-read and retry) |
| DELETE | `/classes/{id}`                  | manager | Delete a class — `409` while it still holds students or courses |
| POST   | `/classes/{id}/members`          | manager | `{user_id}` — add a **student**; enrolls them into every attached course (`409` if one is full, naming it, if one of them no longer exists — detach that link first — if already a member, or once the class holds `max_class_members`; every one of those carries a machine `code`) |
| GET    | `/classes/{id}/members`          | teacher | List the class roster, newest added first · paged |
| DELETE | `/classes/{id}/members/{user}`   | manager | Remove a member — sweeps only the enrollments **this class** pumped for them (one another class still claims is re-tagged to it; hand-placed rows stay) |
| POST   | `/classes/{id}/courses`          | teacher | `{course_id}` — attach a course (that **course's** manager); enrolls the whole roster (`409` if it cannot hold them all, if already attached, or once the class holds `max_class_courses`; every one of those carries a machine `code`) |
| GET    | `/classes/{id}/courses`          | teacher | List the class's attached courses, newest attached first · paged |
| DELETE | `/classes/{id}/courses/{course}` | teacher | Detach a course (that course's manager) — the same sweep along the course axis; a link left behind by a **deleted** course detaches too, rather than `404`-ing forever |
| POST   | `/classes/blueprints`            | manager | Create a grade's course blueprint and stock every section already at that grade (best-effort; returns `matched` + `skipped`) |
| GET    | `/classes/blueprints`            | manager | List every grade blueprint · paged |
| GET    | `/classes/blueprints/{grade}`    | manager | One grade's blueprint            |
| PATCH  | `/classes/blueprints/{grade}`    | manager | Replace the course list and reconcile every section at that grade (returns `matched` + `skipped`) |
| DELETE | `/classes/blueprints/{grade}`    | manager | Delete the blueprint and detach every attachment it made (409 if the list changed since it was read) |
| GET    | `/classes/blueprints/{grade}/status` | manager | Which sections at that grade are out of sync with the template — read-only, unpaged, `{grade, courses, matched, sections[{class, class_name, missing}]}` |
| POST   | `/classes/{id}/blueprint`        | manager | Stock one section from its grade's blueprint (returns `skipped`) |
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
| DELETE | `/sessions/{id}/attendance/{user}` | teacher | Remove a roll-call row (same rights as marking); a **staff** row — any target whose live role is teacher or higher, not just the session's current teacher — needs manager+ |
| POST   | `/courses/{id}/exams`            | teacher | `{title, description?, kind, mode?, starts_at?, ends_at?, duration_ms?, max_attempts?, allow_rejoin?, allow_review?, draft?}` — add an exam (course manager); its weight comes from the kind; `draft: true` keeps it hidden while it's written |
| GET    | `/courses/{id}/exams`            | student | List the course's exams (enrolled, creator, assigned teacher, or manager+; drafts appear to course managers only) · paged |
| GET    | `/exams`                         | student | The caller's visible exams: their courses' (manager+: all; drafts of managed courses only) · paged · `?starts_after=&ends_after=` window (soonest first) |
| GET    | `/exams/{id}`                    | student | Get exam (enrolled, creator, or manager+; a draft is a `404` for everyone but its course's managers) |
| PATCH  | `/exams/{id}`                    | teacher | Edit exam incl. `kind` (re-weights it), schedule, `max_attempts`, `allow_rejoin`, `allow_review`, `draft` (course manager; `course` immutable, `kind` and `mode` frozen once marks/attempts exist, re-drafting frozen once attempts/results exist — the rest stays live; concurrent edits merge, never silently revert each other) |
| DELETE | `/exams/{id}`                    | teacher | Delete exam + its results, attempts, questions, answers, and question + answer images (course manager) |
| POST   | `/exams/{id}/results`            | teacher | `{mark, user_id}` — grade an **enrolled student** (upsert; course manager; students only; drafts, and exams whose kind the school has removed, can't be graded, `409`) |
| GET    | `/exams/{id}/results`            | teacher | List every result for the exam (course manager) · paged |
| GET    | `/exams/{id}/result`             | student | The caller's **own** result (`404` until graded) |
| DELETE | `/exams/{id}/results/{user}`     | teacher | Remove a student's result (course manager) |
| GET    | `/exams/{id}/statistics`         | teacher | `{graded, average, min, max}` over the exam's results (course manager) |
| POST   | `/exams/{id}/attempt`            | student | Start (`201`), resume (`200`), or retake (`201`, blank sheet) the caller's attempt — students only; enrolled; window open where one exists; `409` once `max_attempts` is spent |
| GET    | `/exams/{id}/attempt`            | student | Own latest attempt: status, `attempt`/`attempts_used`/`max_attempts`, deadline, `remaining_ms`, `left_at`, mark, progress (`answered`/`question_count`), server `now` |
| POST   | `/exams/{id}/attempt/finish`     | student | Submit the attempt (`409` once the deadline passed); allowed even while locked out of the room |
| POST   | `/exams/{id}/questions`          | teacher | `{subject_id, text, kind, points, choices?, correct?}` — add a question tagged with one of the course's subjects (course manager; frozen once attempted) |
| GET    | `/exams/{id}/questions`          | teacher | The full question list, `correct` included (course manager) · paged |
| PATCH  | `/exams/{id}/questions/{qid}`    | teacher | Edit a question — the kind bundle revalidates as a unit (course manager; frozen once attempted; an omitted `subject_id` is filled from the stored row, so *any* edit is a `409` if someone re-tagged the question since the read — nothing written, re-read and retry) |
| DELETE | `/exams/{id}/questions/{qid}`    | teacher | Delete a question + its answers (course manager; frozen once attempted) |
| POST   | `/exams/{id}/questions/from-bank/{bid}` | teacher | Instantiate a **bank question** into this exam — copies it to a fresh exam-scoped question (new id, own images + answers; records the template in `source_bank`); body `{subject_id}` retags it against the course's subjects (`400` cross-course; course manager; `409` once attempted) |
| POST   | `/exams/{id}/questions/{qid}/to-bank` | teacher | Save an existing exam question into the school **question bank** — copies it to a detached bank row (new id, own images; records the origin exam in `source_exam`); the source question is untouched (course manager) |
| POST   | `/exams/{id}/questions/{qid}/refresh-from-bank` | teacher | Re-copy the template's **current** content over this question — text, points, kind, choices (with the template's choice ids), `correct`, illustration and option pictures; keeps the question's own id, exam, `subject` and provenance links, and overwrites any local edit. `400` if it never came from the bank (or the template is gone), `404` if the template is not visible to the caller, `409` once attempts exist or if the question's subject was re-tagged since the read — nothing written, re-read and retry (course manager) |
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
| GET    | `/exams/{id}/review/attempts`    | student | The caller's **own** sitting numbers (answers ⋃ marks), ascending — only when `allow_review` is on and the caller has been marked (`403`/`404` otherwise), and `409` while the caller's latest sitting is still in progress (all four review reads share that gate) |
| GET    | `/exams/{id}/review/attempts/{seq}/answers` | student | One of the caller's **own** sittings, judged: `is_correct` flags + suggested `auto_score` (same review gate) |
| GET    | `/exams/{id}/review/attempts/{seq}/answers/{qid}/image` | student | The caller's **own** drawn-answer bytes for a sitting, inline (same review gate) |
| GET    | `/exams/{id}/attempt/ws`         | student | **WebSocket** exam room (students only): state ticks, autosave, finish; entering clears `left_at`, leaving stamps it (see "Taking an exam") |
| GET    | `/exams/{id}/live`               | teacher | Live monitor snapshot: roster × latest attempts × marks + per-student progress/`left_at`/`attempts_used` + counts; no-shows turn `absent` once the window closes (course manager); poll to keep a monitor current |
| GET    | `/bank-questions`                | teacher | The school-wide question bank — `?subject=<id>` filters by origin subject, `?owner=<id>` (or `?owner=me`) by owner; reusable templates any teacher+ can read, each with its image metas (`image`, `choice_images`) · paged |
| POST   | `/bank-questions`                | teacher | `{subject_id, text, kind, points, choices?, correct?}` — save a reusable question template into the bank (owner = caller; `subject_id` is origin metadata, but must exist — `400` otherwise) |
| GET    | `/bank-questions/{bid}`          | teacher | Get one bank question, `correct` + image metas (`image`, `choice_images`) included (any teacher+) |
| PATCH  | `/bank-questions/{bid}`          | teacher | Edit a bank question — the kind bundle revalidates as a unit (**owner only**, admin bypass; bank rows never freeze; concurrent edits merge, never silently revert each other) |
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
| PATCH  | `/homework/{id}`                 | teacher | Edit title/description/`due_at`/`subject_id`/`assigned` (course manager; a newly set due date is re-checked; `409` if narrowing `assigned` would strand an existing submission or grade — the blockers are named — or if the `subject_id` it re-tags from changed since the read: nothing written, re-read and retry) |
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
| PATCH  | `/settings`                      | manager | Replace any subset of the fields (lists wholesale); concurrent edits merge, never silently revert each other; `400` on "Invalid lists, bands, file limit, or chat limits" — a meal slot name carrying `/ \ ? # %` is refused here too, since the name becomes a menu's URL id (a name the stored list already carries is exempt, so an older list stays editable — it still cannot carry a new menu); `409` when a removed exam kind still has graded exams, or a removed meal slot still has published menus (see "Per-school policy") |
| POST   | `/terms`                         | manager | `{name, starts_at, ends_at}` — past dates allowed (calendar backfill) |
| GET    | `/terms`                         | student | List terms, newest first · paged |
| GET    | `/terms/{id}`                    | student | Get one term                    |
| PATCH  | `/terms/{id}`                    | manager | Edit a term (the merged range must stay ordered) |
| DELETE | `/terms/{id}`                    | manager | Delete a term — `409` while any course or class still links to it |
| POST   | `/meals/menus`                   | manager | `{date, slot, capacity?}` — publish a menu; `date` is `YYYY-MM-DD` text, `slot` must be one of the school's `meal_slots` and may not contain `/ \ ? # %` (it becomes part of the menu's URL id); `409` when that day+slot is already published |
| GET    | `/meals/menus`                   | student | List menus with their dishes, newest day first · `?from=&to=` inclusive `YYYY-MM-DD` range · paged |
| GET    | `/meals/menus/{id}`              | student | One menu with its dishes |
| PATCH  | `/meals/menus/{id}`              | manager | `{capacity}` — the only mutable field (`null` = uncapped); `date` and `slot` are immutable |
| DELETE | `/meals/menus/{id}`              | manager | Unpublish a menu; its dishes and its attendance marks go with it, in the same transaction; `409` while anyone still holds a seat |
| POST   | `/meals/menus/{id}/dishes`       | manager | `{name, description?, price_minor, tags?}` — add a dish (≤ 50 per menu, `409` at the cap); `tags` must come from the school's `dietary_tags` |
| PATCH  | `/meals/dishes/{did}`            | manager | Edit a dish (`tags` replaces the list, `"description": null` clears it); `404` once its menu is gone |
| DELETE | `/meals/dishes/{did}`            | manager | Remove a dish from its menu; `404` once that menu is gone |
| GET    | `/meals/profiles/me`             | student | The caller's own dietary profile (empty when the school recorded none) |
| GET    | `/meals/profiles/{user}`         | student | One student's dietary profile; own id always, otherwise teacher+ or a parent link |
| PATCH  | `/meals/profiles/{user}`         | manager | `{tags?, note?}` — record what a student may not eat (`tags` replaces the list, `"note": null` clears it); **manager+**, a student never edits their own |
| POST   | `/meals/menus/{id}/bookings`     | student | `{student_id?}` — take a seat; a student books for themselves, a parent for a linked student; `409` when the menu is full, its cutoff has passed, or the menu was edited so often mid-booking that the price could not be pinned |
| GET    | `/meals/bookings/me`             | student | The caller's own bookings (seats held for them + for a parent, their currently linked children's), newest first · paged |
| GET    | `/meals/menus/{id}/bookings`     | manager | Every booking on one menu, cancelled ones included · paged |
| DELETE | `/meals/bookings/{bid}`          | student | Cancel a booking (status flip, the row stays) — its student or their parent, **or any manager+**, whose seat and money must stay reachable after a role change; idempotent — cancelling again is a `200` that replays the refund; `409` past the cutoff, which binds **students and parents only** — a manager+ frees a closed meal's seat |
| POST   | `/meals/menus/{id}/attendance`   | teacher | `{student_id, status}` — mark who was served (`served`/`missed`); one row per (menu, student), re-marking flips it; `404` if the menu is unpublished mid-request; **moves no money** |
| GET    | `/meals/menus/{id}/attendance`   | teacher | Who ate off one menu · paged |
| GET    | `/meals/attendance/{user}`       | student | One student's meal-attendance history · `?from=&to=` inclusive `YYYY-MM-DD` range over the menu's day · paged · own id always, otherwise teacher+ or a parent link |
| GET    | `/meals/balance/me`              | student | The caller's meal balance, minor units (negative = owes) |
| GET    | `/meals/balance/{user}`          | student | One student's balance; own id always, otherwise **manager+** or a parent link — a teacher gets a `403`, canteen debt is family debt |
| GET    | `/meals/ledger/{user}`           | student | That student's statement — every charge, credit, reversal — newest first · paged · same gate (manager+, parent link, or own) |
| POST   | `/meals/credits`                 | admin   | `{student_id, amount_minor, method?, note?}` — record money received; **admin only**, appends a `credit` line; the target must be a student, or anyone already carrying ledger lines (a debt outlives a role change) |
| POST   | `/payments/plans`                | manager | `{name, installments}` — write a fee plan (1–60 installments, each `{amount_minor, due_at}`; `due_at` may be in the past); bills nobody |
| GET    | `/payments/plans`                | manager | List fee plans, newest first · paged |
| GET    | `/payments/plans/{id}`           | manager | One fee plan with its schedule |
| PATCH  | `/payments/plans/{id}`           | manager | Edit a plan's `name` and/or `installments` (the schedule replaces wholesale); `409` once anyone is on the plan |
| DELETE | `/payments/plans/{id}`           | manager | Delete a plan; `409` once anyone is on it — its charges name it |
| POST   | `/payments/plans/{id}/assignments` | manager | `{student_ids}` (≤ 200) — place the plan on students, appending **every** installment as a `charge` at once; replay-safe, reported per student as `assigned` / `already_assigned` / `rejected` |
| GET    | `/payments/plans/{id}/assignments` | manager | Who is on this plan, newest first · paged |
| POST   | `/payments/credits`              | manager | `{charge_id, amount_minor, method?, note?, request_key?}` — record money received against one named charge; partials are the norm, `409` past what the charge is worth (advisory). A `request_key` makes the call retry-safe: a replay returns the same line, the same key for different money is a `409` |
| POST   | `/payments/refunds`              | manager | `{credit_id, amount_minor, method?, note?, request_key?}` — hand money back against one named payment; partials allowed, capped by that credit; same `request_key` retry-safety |
| POST   | `/payments/reversals`            | manager | `{line_id, note?}` — undo a `charge` or a `refund` for its exact amount (`400` on any other kind); idempotent, at most one reversal per line |
| GET    | `/payments/ledger/{user}`        | student | One student's raw lines — charges, payments, refunds, reversals — newest first · paged · own id always, otherwise manager+ or a parent link (**a teacher gets a `403`**) |
| GET    | `/payments/statement/me`         | student | The caller's own statement: a row per charge with what it collected, what went back out, what is still owed, and whether it is `overdue`. `?limit=&offset=` pages the rows (`entries` is the envelope); `balance_minor` is the same on every page |
| GET    | `/payments/statement/{user}`     | student | One student's statement; same gate as the ledger, same row paging |
| GET    | `/payments/balance/me`           | student | The caller's fee balance, minor units (negative = owes the school) |
| GET    | `/payments/balance/{user}`       | student | One student's fee balance; same gate as the ledger |
| POST   | `/chatbot/threads`            | student | `{title?}` — start a chatbot thread (every role incl. `parent`, always private to its owner); `409` at `max_chatbot_threads` |
| GET    | `/chatbot/threads`            | student | The caller's threads, newest activity first · paged |
| PATCH  | `/chatbot/threads/{id}`       | student | `{title}` — rename own thread (`null` or blank clears it back to untitled); counts as activity, so the thread moves to the top. Nothing auto-titles a thread |
| DELETE | `/chatbot/threads/{id}`       | student | Delete own thread and every message in it (someone else's is a `404`, never a `403`) |
| GET    | `/chatbot/threads/{id}/messages` | student | The thread's turns, oldest first — user and assistant rows alike · paged |
| POST   | `/chatbot/threads/{id}/messages` | student | `{content}` (≤ `max_chatbot_message_len`) — send a turn; `202 {message_id, status: "pending"}`, the answer is fetched after (`503` when no AI service offers `chat.reply` — nothing is written; `429` + `Retry-After` on the send tier) |
| GET    | `/chatbot/threads/{id}/messages/{mid}` | student | Poll one turn: `pending` until the answer lands, then `complete` + `content` or `failed` + `error_code` |
| GET    | `/chatbot/threads/{id}/messages/{mid}/stream` | student | **SSE** on the same row: `delta` chunks then one `done` — or one `error` — and close; an already-finished answer replays (see "Chatbot") |
| POST   | `/boards`                        | student | `{title, participants?}` — open a whiteboard; the caller becomes its creator, everyone named may draw; `409` at `max_boards_per_creator` |
| GET    | `/boards`                        | student | Boards the caller created or was invited to, newest first. `?open=true\|false` narrows by the closing stamp · paged |
| GET    | `/boards/{id}`                   | student | One board — a `404`, never a `403`, for anyone not on it |
| GET    | `/boards/{id}/strokes`           | student | The live canvas: the current epoch's strokes, oldest first · paged |
| GET    | `/boards/{id}/history`           | student | The whole append-only log, oldest first, `clear` markers included · `?epoch=` for one epoch · paged |
| GET    | `/boards/{id}/epochs`            | student | The epoch index: every `clear` marker (the epoch it closed, that epoch's final stroke count, who cleared, when) · paged |
| PATCH  | `/boards/{id}`                   | student | `{title?, participants?, locked?}` — re-title (any participant); the roster and the lock are the creator's alone (`403`) |
| POST   | `/boards/{id}/invite`            | teacher | **Creator only**, additive: fill the roster from a group — `{kind:"class", class}`, `{kind:"course", course}` (a club is a course), `{kind:"event", event}`. Resolved once, never live; `409` all-or-nothing at `max_participants` |
| POST   | `/boards/{id}/clear`             | student | **Creator only**: bump the epoch, blanking the live canvas and resetting its cap — nothing is deleted; `409` on a closed board, on a **locked** board, and on a canvas that is **already blank** (the marker is a stored row, so a clear has to close at least one mark to be worth one) |
| POST   | `/boards/{id}/close`             | student | **Creator only**: retire the board — permanently read-only, still fully readable; idempotent, and there is no reopen |
| DELETE | `/boards/{id}`                   | student | **Creator only**: delete the board and its whole stroke log; frees one of the creator's board seats |
| GET    | `/boards/{id}/ws`                | student | **WebSocket** board room: a `join` replays the current epoch, every accepted stroke fans out to the other participants (see "Collaborative whiteboard") |

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
`{"kind": "course", "course": "<id>"}` (the course's current enrollment),
`{"kind": "class", "class": "<class id>"}` (the students currently in that
class section — şube), or
`{"kind": "registration", "capacity": 30}` (a signup list; `capacity` optional,
`null` = unlimited) — and is the event's **expected-attendee roster**, resolved
live at read time: role changes, (un)enrollments, class-roster changes, and
(un)registrations move people in and out by themselves. A class audience is
live like the rest: adding a student to the class puts them on every one of its
events' rosters at once, and removing them takes them off. The homeroom teacher
is not implied — a mixed gathering wants a `role` or `registration` audience.
An unknown class id is a `400`, and a class deleted later leaves the event
standing with an empty roster, exactly as a deleted course does. It never hides the event — everyone sees every
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
kind; deleting the event deletes them, in one transaction with the attendance
rows. A demotion to `parent` sweeps that user's signups off **still-open**
lists and hands each seat back (`PATCH /users/{id}/role`): unregistering is
student-or-self only, so a parent's seat had no other way out. Staff free their
own, and a closed list is never rewritten. Pre-existing hand-picked (`users`)
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
seed them. `display_name` (≤ 50) and `bio` (≤ 500) ride the same two patches
under the same rules, `""` clearing either (see "User profiles & avatars").
UI preferences (`theme`: `light`/`dark`, `language`: `tr`/`en`,
`palette_color`: an accent color as `#` plus exactly 6 hex digits) ride on the
same account row and come back on every user response (`/auth/me` included).
`PATCH /users/me/preferences` (or the admin `PATCH /users/{id}/preferences`)
uses the same field semantics as the profile patch: omitted keeps, `""` clears
back to `null` ("never chose" — the client then follows the device
preference, or its own default accent), anything else must be valid or the
whole patch is a `400`. `palette_color` is the one **open** set: any valid hex
passes (mixed case in, stored and returned lowercase), so a new frontend
palette needs no backend change — `/limits` publishes its pattern rather than a
list of colors.

## User profiles & avatars

The public half of an account: who someone is inside the school, readable by
the school. `GET /users/me/profile` is the caller's own copy of it,
`GET /users/{id}/profile` anyone's, and both return the same document:

```json
{
  "id": "01J8XZ0K3Q8G7X2M4N5P6R7S8T",
  "username": "ada",
  "display_name": "Ada Lovelace",
  "role": "student",
  "bio": "Sınıfın en hızlı pomodorocusu.",
  "avatar": { "content_type": "image/png", "size": 20480 },
  "classes": [ { "id": "01J8…", "name": "9-A", "grade": "9" } ],
  "courses": [ { "id": "01J8…", "title": "Matematik", "kind": "course" } ],
  "badges": [ { "id": "homework_submitted_10", "earned_at": 1754300000000 } ],
  "stats": { "pomodoro_sessions": 42, "pomodoro_focus_ms": 63000000,
             "courses": 7, "classes": 1,
             "homework_submitted_total": 12, "homework_on_time_total": 11,
             "exam_sat_total": 5, "pomodoro_finished_total": 44,
             "pomodoro_focus_ms_total": 65400000,
             "marks_given_total": 0, "lessons_held_total": 0,
             "pool_approved_total": 0, "pool_published_total": 3,
             "lessons_attended_total": 58, "high_mark_total": 2,
             "study_streak_total": 9 }
}
```

**Contact fields are absent by design.** `email`, `phone` and `birth_date` are
not on this document at any role — they keep exactly the gate they have today
(`GET /auth/me` for oneself, `GET /users/{id}` for an admin). A school-wide
read surface is not the place to widen where a phone number is reachable.

**Visibility** is every authenticated account reading every profile, with one
exception: a `parent` reads their own and their currently-linked students', and
nothing else (`403`). Both halves of that grant are re-read on each call — the
link *and* the target's live role — so unlinking a student, or promoting one
out of `student`, stops the reads at once rather than at the next login.

**`display_name`** resolves in three steps: the stored `display_name`, else the
`"name surname"` join, else `null`. It does **not** replace `name`/`surname` —
those stay the school-office record, and an admin still edits them — and it
deliberately does not enter the user search fold: `/users/search` finds people
by the name the office knows them under, not by a nickname they picked this
morning. `display_name` and `bio` are patched through the ordinary
`PATCH /users/me` (or the admin `PATCH /users/{id}/profile`) with the same
field semantics as the rest: omitted keeps, `""` clears, anything else is
validated.

**The class and course blocks are a narrowed projection**, not the rows
themselves: id/name/grade for a class, id/title/kind for a course. A class's
`creator` and its homeroom teacher are office data behind the teacher+
`GET /classes/{id}` and stay there — a profile that carried them would widen
that gate by accident. Both blocks are truncated (20 courses, 5 classes, both
published in `/limits` as `max_profile_courses` / `max_profile_classes`); a
profile is a preview, and `GET /courses/me` and `GET /classes/me` are the full
paged lists.

**The courses block follows the profile owner's live role**: teacher+ lists the
courses they teach, everyone else the courses they are enrolled in. It is read
off the role, never off the stored `creator`/`teachers` columns, because
ownership here is a live grant and no demotion sweeps those columns — so a
demoted ex-teacher lists what they are enrolled in, like any other student.

**and it is then cut to what the *reader* may already see**: a course appears
only if the reader would pass the ordinary course-read gate for it
(`GET /courses/{id}` — enrolled, creator, assigned teacher, or manager+). A
course title is not public: a peer who is `403` on the course itself, and gets
an empty `GET /courses`, must not read it off somebody's profile instead. Your
own profile and a manager's read are never cut, since both already see the
whole list, and the 20-course truncation runs *after* the filter, so a reader
gets up to 20 courses they can actually open. `stats.courses` stays the
owner's true total on purpose — it is the motivational counter, a per-reader
number would be meaningless, and a magnitude names no course. One consequence,
by design: a teacher you share nothing with has an empty `courses` block.

**`stats` holds sixteen numbers of two different kinds, and the difference is
worth knowing.** `pomodoro_sessions`, `pomodoro_focus_ms`, `courses` and
`classes` are **computed at read**: they are recounted from the live rows on
every call, so no column can drift out of sync with what is behind it. The
twelve `*_total` keys are **stored lifetime tallies**, maintained at write time
(see below), so a tally can outlive the rows behind it — a student's
`exam_sat_total` still counts an exam a teacher has since deleted, which is the
point of a lifetime counter. Either kind reads a true `0` on a fresh account,
never `null`. Only *finished* pomodoro stints count on either side — a timer
left running is not study time. Exam averages, homework-done rates and
attendance rates are deliberately absent: those tables are indexed for their
own reads, so a per-user aggregate over them is a full table scan, and a
profile is far too cheap a page to pay for one — which is also why the twelve
lifetime counters are tallied at write rather than recounted here.

**Every key is unconditional, whatever the role.** Some counters are a
student's (`exam_sat_total`, `lessons_attended_total`, `high_mark_total`,
`study_streak_total`, the two homework ones), some a teacher's
(`marks_given_total`, `lessons_held_total`, `pool_approved_total`), and a role
that never does the work simply reads `0` — no key appears or disappears with a
role, so a client renders one shape. Each `*_total` is the badge catalog's
`stat` name plus `_total` and nothing else, which is how a client joins a
profile to `badges.catalog[]` at `/limits` without a lookup table.
`study_streak_total` keeps that suffix even though it is a **longest run and
not a sum** — the mechanical spelling is worth more than the accurate one, so
do not "fix" it.

**The avatar** is one picture per account, uploaded as `multipart/form-data`
with the image under a `file` field. Raster types only (`image/png`,
`image/jpeg`, `image/webp`, `image/gif`) — an SVG can carry script, and these
bytes render inline all over the school. The size cap is the school's existing
`max_file_bytes` setting rather than a new knob: one number to raise when a
school wants bigger uploads. Uploading replaces, and the picture it replaced
comes off disk. The owner removes theirs at `DELETE /users/me/avatar`; an admin
removes anyone's at `DELETE /users/{id}/avatar`, the moderation path — an
offensive picture is a school problem. The bytes come back from
`GET /users/{id}/avatar` under the same reach as the profile itself, served
`X-Content-Type-Options: nosniff` and `Cache-Control: private, no-store`;
`GET /users/me/avatar` is the same read for the caller's own picture, so a
client that never learned its own id still has a route.

### Badges

**`badges` is auto-earned and nothing else.** There is no route that awards one
and none that revokes one — a badge appears when a stored counter crosses a
hardcoded threshold, normally on the same write that moved the counter (and, as
a backstop, on a profile read that finds an earned badge missing — the sync
after a write is best-effort, so a failed one heals rather than being lost).
The rule is the only author. Handing a teacher an award button would make
the counters decorative, and a badge that can be given can be argued about.

**A badge is permanent.** Once earned it stays, whatever the counter does
afterwards: withdraw a submission and `homework_submitted_total` falls, but the
badge it earned does not come off. The award records that the student *did* the
thing, not that they still have it. `earned_at` is pinned at the first crossing
and never moved, so the list is a history and stays in order — it comes back
oldest first.

**Ids only.** Each entry is `{id, earned_at}`. The label, the icon and the
description are the frontend's, keyed by the id — the same deal `roles` and
course `kinds` already have. The catalog behind the ids is at `GET /limits` as
`badges.catalog[]` (`{id, stat, threshold}`, 34 badges over twelve counters,
alongside `badges.high_mark_min`); it is compiled into
the binary, so changing a threshold is a deploy, not a settings edit. An id is
never reused for another meaning, and retiring a badge just drops it from the
catalog — awards carrying it stop being served, no migration.

**How the counters move**, since a badge is exactly a threshold on one of them:

- **Homework.** A genuine first hand-in is `+1 homework_submitted_total`, and
  `+1 homework_on_time_total` if that hand-in beat `due_at`. **Withdrawing your
  own submission decrements both** — the one place a counter comes down, and it
  exists because withdrawal is student-callable: without it, submit/delete/
  submit farms one homework into fifty. Editing an existing submission moves
  nothing, so the on-time verdict is fixed at the first hand-in and attaching a
  file after the deadline cannot turn an on-time submission late.
- **Exams.** `+1 exam_sat_total` per attempt created, retakes included —
  sitting an exam twice is two sittings. Resuming an attempt already running is
  not a new one. Deleting an exam removes the attempt rows but does **not**
  decrement anyone: a teacher tidying up does not un-sit the exam.
- **Pomodoro.** Finishing a stint is `+1 pomodoro_finished_total` and its
  duration into `pomodoro_focus_ms_total`. An open stint counts for neither —
  unfinished focus has no honest duration to add.
- **Study streak.** Finishing a stint also extends `study_streak_total`, the
  **longest** run of consecutive days on which the student finished one. A
  second stint the same day extends nothing; a gap of any length starts the run
  over at 1. The badge reads the longest run ever held, never the run in
  progress, so a broken streak takes no badge away and the counter never comes
  down. A day is a **UTC calendar day**: this API stores no school timezone
  anywhere, and the boundary is midnight UTC, the same one every other day
  calculation here uses (meal cutoffs, menu dates, birth dates).
- **Grades given.** `+1 marks_given_total` for the **grader**, the first time
  they record a grade for a given exam sitting or homework submission — exam
  and homework alike, since grading is grading. A regrade of the same pair moves
  nothing: the counter counts work judged, not times the mark was edited.
- **High marks.** `+1 high_mark_total` for the **student** whenever an exam
  mark lands at or above `badges.high_mark_min` (90, published at `/limits`),
  once per sitting — a retake is another sitting and can earn another. Homework
  marks never count here: a homework mark is optional and most grades are
  status-only, so counting them would reward a teacher's habit rather than a
  student's work. The cut is compiled in rather than read from the school's
  grade bands, which are renameable display labels — `high_mark_10` has to mean
  the same thing in every school, forever.
- **Lessons held.** `+1 lessons_held_total` for the **session's teacher**, once
  per lesson, credited by the **first roll call taken for it**. It means "a
  lesson whose roll call was taken", not "a lesson on the timetable" —
  scheduling two hundred lessons and cancelling them all earns nothing. The
  thirtieth student marked in that lesson credits nothing further (a stamp on
  the session row is the guard), and taking one student back off the roll does
  not un-hold the lesson.
- **Lessons attended.** `+1 lessons_attended_total` for a **student** marked
  `present` or `late` at lesson roll call — the same cut the attendance report's
  `rate` uses, so a student's badge and their attendance rate never disagree
  about what attending is; `excused` and every school-added status are neutral.
  It is a **delta, not a tally**: a teacher correcting `present` → `absent`
  moves it back down (floored at 0), and clearing the row does too. Student-only
  — a teacher marked present in their own lesson moves nothing — and course
  roll call only: the daily/event attendance at `/attendance` earns none of it.
- **The question pool.** One pending → approved transition credits two people:
  `+1 pool_approved_total` for the **approver** and `+1 pool_published_total`
  for the **asker** whose question reached the pool. The asker is credited at
  approval and never at asking, because asking is self-service — delete and
  re-ask would farm it — which is why the counter is named `published` rather
  than `asked`. And a teacher **approving their own question** earns neither:
  the approval itself is unchanged (same `200`, same freeze), but a counter
  moves only where one person's work was judged by somebody else, which closes
  the ask-approve-delete farm from the other end too.

**Existing accounts are seeded once.** The first boot of this version tallies
every account from its real history, so nobody starts at zero for work they
already did; the pass is marked done and skipped on every boot after. One
consequence is not fixable and is not an oversight: the seed can only count
rows that still exist, so a student whose exam was deleted before the upgrade
is seeded short on `exam_sat_total`. The live rule (never decrement) and the
seed (count what is there) simply cannot agree about history that is gone —
and the fix is not to make deletion decrement, which would break the tally for
everyone in order to patch it for a few.

**The seven newer counters are not seeded at all**, deliberately:
`marks_given_total`, `high_mark_total`, `lessons_held_total`,
`lessons_attended_total`, `pool_approved_total`, `pool_published_total` and
`study_streak_total` **begin at this deploy**, at zero, and no history is
reconstructed for them. The columns are defined and absent reads `0`, so an
account written before them is indistinguishable from a fresh one; the first
grade recorded, roll call taken, approval landed or stint finished after the
upgrade is what starts each of them counting.

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
else immediately (the slot carries an `occupied` counter, taken by a booking
and handed back in the same transaction as the reject or cancel that settles
it, so the seat is decided by the database rather than by a count two
concurrent bookings can both read as free). The decisions:

- `PATCH /{id}/approve` — the slot's teacher (or manager+) confirms; the
  slot's `teacher` is history, not a standing grant, so an owner demoted below
  `teacher` decides nothing on it any more — manager+ still can, and the
  requester can still cancel. Refused
  (`409`) when the effective window has already started, and refused while a
  counter-proposal stands: the proposal is the teacher's own, so approving it
  here would let them confirm a time the requester never accepted.
- `PATCH /{id}/reject` — turns it down; the slot frees up.
- `PATCH /{id}/cancel` — **the requester only**, from either live state.
  Anyone else is a `403`, the slot's teacher and a manager/admin included (the
  guard compares ids, not roles). A teacher ends a booking by **rejecting** it
  while it is `pending`, and an approved one by counter-proposing another time
  (`/reschedule`, which sends it back to `pending`) and then rejecting it — or
  simply by rescheduling to a time that works. Refused (`409`) once the
  meeting's window has started: a
  meeting that already began is history, not a plan. The guard sits in the
  domain's `cancel` itself, on a fresh read compared-and-set into the row, so
  every way of cancelling — the decline below included — inherits it.
- `PATCH /{id}/reschedule` (`{starts_at, ends_at}`) — the teacher
  **counter-proposes**. The times land on the *same* row as
  `proposed_starts_at`/`proposed_ends_at`, the status goes back to `pending`
  and `decided_by` clears, because leaving it `approved` would silently move a
  confirmed appointment. The slot stays held meanwhile. A window that has
  already opened is refused (`409`): the 60-second grace on the times is for
  clock skew, not for proposing into a meeting already underway — one nobody
  could then cancel. While the proposal stands, `PATCH /{id}/approve` is
  refused (`409`): the effective window is now the teacher's own proposal, so
  confirming it there would commit the requester to a time they never agreed
  to. Only `/reschedule/accept` may approve at a proposed time, and only the
  requester may call it. Re-proposing supersedes the previous proposal, so any
  accept still carrying the old one is refused.
- `PATCH /{id}/reschedule/accept` (`{proposed_starts_at, proposed_ends_at}`) —
  the **requester** only: this *is* approval
  at the new time, so the overlap guard runs again — and so does the
  already-started guard (`409`). Agreeing to a window that has begun does not
  help: the meeting could never be cancelled. The booking stays `pending`, so
  the teacher just proposes a time that can still happen.
  The body **names the proposal being accepted**, copied from the booking's
  `proposed_starts_at`/`proposed_ends_at` as the requester read them. It is not
  a request to move the meeting: it pins *which* proposal this answer is for. A
  teacher may re-propose at any moment and `/reschedule` takes no lock, so
  without the pin the teacher would decide which window the requester's click
  commits them to — propose 10:00, wait for them to open the page, propose
  23:00, and their accept lands on 23:00. A superseded pin answers `409` (*the
  proposed time has changed*); re-read the booking and accept or decline what
  actually stands.
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
  an exam keeping a since-removed kind counts with weight `1` — but it can no
  longer be graded (`409`) until the school offers that kind again, which is
  what keeps "a marked kind cannot be removed" true from both ends. The same
  rule pointed at one exam: an exam that already carries marks keeps its
  `kind` (`PATCH /exams/{id}` answers `409`), because re-pointing it would
  re-weight those marks just as silently.
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
retired status — only **new writes** are held to the current lists (which is
why grading an exam whose kind has left the list is refused).

The kitchen is data too: a manager publishes a **menu** per calendar day and meal slot, with its dishes, their dietary tags, and prices in minor units; booking a seat charges that price as a snapshot and an admin records the cash that comes back in (see "Food program: menus, dishes, bookings & the ledger"). The one shape rule on a **meal slot's name** lives here: it may not contain `/`, `\`, `?`, `#` or `%` (`400`), because the name is copied verbatim into a menu's record id and that id is a URL path segment — a slot the settings accepted but no menu could be addressed under would be a slot the canteen cannot use. A name the stored list *already* carries is exempt, so a list written before the rule existed can still be edited around the offending name — otherwise one bad slot froze the whole list, since re-sending it is a `400` and dropping it is a `409` once a menu used it. The grandfathered name still cannot carry a new menu, and dropping it is still the only way to remove it.

Academic structure is data too. **Terms** (`/terms`) model whatever calendar
the school runs — semester, trimester, quarter systems are just rows with a
name and a date range. Courses *and classes* may link to one via `term_id` (nullable),
and deleting a term is refused with a `409` while any of them still links to
it — unlink them (`PATCH /courses/{id}` or `PATCH /classes/{id}` with
`"term_id": null`) or delete them first, so the calendar never disappears
under them. Term dates may lie in the past,
deliberately: a school adopting the app mid-year backfills its calendar —
unlike exam/lesson/event times, which reject backdating.

What stays fixed is deliberate too: the four roles, the `0`–`100` mark scale,
validation bounds, and the UTC time policy are invariants, not preferences
(rename role labels in the frontend if a school says "principal" instead of
"manager"). Deployment knobs (ports, rate limits, admin seed, CORS) remain
environment variables — the model is **one school per deployment**, which
keeps every school's data physically isolated.

## Food program: menus, dishes, bookings, attendance & the ledger

The school publishes **one menu per calendar day and meal slot** (`POST
/meals/menus`). Two shapes there are deliberate:

- **`date` is text**, `YYYY-MM-DD`, not a timestamp. "One menu per day and
  slot" is an equality test on the school's own day, and a midnight-in-millis
  day is only unique for one timezone. Fixed-width and zero-padded, so the
  text sorts chronologically — which is what the inclusive `?from=&to=` range
  filter and the newest-day-first ordering are built on.
- **`slot` is a snapshot**, not a link into settings. It must be one of the
  school's `meal_slots` (`GET /settings`) *when the menu is written*, and it
  is stored as text — so retiring a slot later never rewrites a menu already
  published under it. The slot's `serving_minute` is *not* snapshotted: the
  booking cutoff resolves it live from the current list. The mirror of that rule lives in settings: a slot any
  menu was published for cannot be removed from the list (`409`), same
  contract exam kinds have with graded exams.
- **A slot name may not contain `/`, `\`, `?`, `#` or `%`** — a `400` at
  `PATCH /settings` when the slot is defined, and again here when a menu is
  published under one. The name goes verbatim into the menu's record id, and
  that id is a URL path segment: a slot called `a/b` would publish a menu at
  `/meals/menus/2026-09-14_a/b`, an address no route can ever match again — the
  menu could not be read, edited or deleted. Ordinary names are untouched;
  spaces and Turkish letters percent-encode into one segment as they always
  have.
- **A name the school's stored list already carries is exempt from that rule.**
  It is younger than the lists it validates, and `PATCH /settings` re-validates
  the *whole* submitted list — so a school that stored `a/b` before the rule
  existed could never edit `meal_slots` again: re-sending the name is a `400`
  and dropping it is a `409` the moment a menu was published under it. The
  grandfathered name is exactly as unusable as it already was — no menu may be
  published under it — and dropping it is still the only way to be rid of it.
  The rule stays in full force for every *new* name.

The day+slot pair is unique: a second publish for the same meal answers `409`
— edit the first one instead. Only `capacity` is patchable (`null` = uncapped)
— `date` and `slot` are immutable, because a menu on another day is another
menu.

**Dishes** hang off a menu (`POST /meals/menus/{id}/dishes`, at most 50, then
`PATCH`/`DELETE /meals/dishes/{did}`). Each carries a name, an optional
description (blank or `null` clears it), dietary `tags` drawn from the
school's `dietary_tags` list — an unknown tag is a `400`, never a stored
string — and a `price_minor`. Every dish write moves its **menu's** revision in
the same transaction (that is what keeps a booking from freezing a price the
menu has already left, below), which also makes the menu's existence part of
the write: editing or deleting a dish whose menu has since been unpublished is
a `404`, not a write onto a menu that is gone.

### Dietary profiles and conflicts

A student carries a **dietary profile**: a list of tags plus a free-text note
for the kitchen, one row per student (the record key *is* the student's id).
The tags come from the *same* `dietary_tags` list a dish is tagged from —
there is deliberately no second vocabulary, because one shared list is exactly
what makes "is this dish a problem for this student" answerable as a set
intersection, without the backend knowing any nutrition.

Writing it is **manager+** (`PATCH /meals/profiles/{user}`, first write
creates the row). A student does *not* edit their own allergen list: it is a
safety record the school keeps on their behalf, and a mistyped — or quietly
removed — allergy would otherwise reach the kitchen with the school's
authority behind it. Reading is `GET /meals/profiles/me`, or
`GET /meals/profiles/{user}` under the usual per-student gate (own id always,
otherwise teacher+ or a parent link). A student who was never recorded reads
back as an *empty* profile rather than a `404` — "nothing known" is an answer.

Every dish a menu read returns therefore carries a **`conflicts`** list: the
intersection of that dish's `tags` with the **calling user's** profile tags.
It is empty when the two do not overlap, and empty for every reader without a
profile — a manager reading a menu sees `conflicts: []`, which is correct and
not a bug (the field answers "may *I* eat this", and there is no "conflicts
for student X" parameter). The caller's profile is loaded **once per request**,
not per dish, so the menu list stays at its fixed query count no matter how
many menus or dishes the page holds.

**Money is minor units** (kuruş) as an integer, at every layer: `4550` is
45,50 ₺. This API never speaks decimals or floats about money, so no rounding
is ever introduced by transport or by a client's JSON number parser.

Reads are open to every authenticated user — a student has to see what is
being served — and every write is manager+. The exception is the money:
`GET /meals/balance/{user}` and `GET /meals/ledger/{user}` are manager+ (or
the student, or their parent), gated exactly like `/payments`. Deleting a menu takes its dishes
**and its attendance marks** with it, in the same transaction as the row: a
dish has no meaning apart from the menu it was published on, and a menu's
record id is deterministic on day+slot, so anything left behind would come back
attached to the *next* menu published for that meal — the kitchen reading
"served" for a student who never came, priced off food nobody re-entered.
Cancelled **bookings** deliberately stay: their `attempt` counter is what keeps
the ledger's `(booking, attempt)` ids unique, and a re-book on a republished
menu would otherwise reuse an id the ledger already holds and bill the seat
nothing.

**Bookings** are seats on a published menu (`POST
/meals/menus/{id}/bookings`). One row per (menu, student), keyed by a
deterministic composite id, so booking twice is the same seat and never a
second one. A **student books only for themselves**; a **parent books for a
student they hold a link to** — the one write a `parent_link` authorises, since
paying for lunch is a parent's job. Nobody else books through this route: a
teacher or manager ordering a child's lunch is a `403`.

- **Cancelling is a status flip, not a delete** (`DELETE
  /meals/bookings/{bid}` answers `200` with the flipped row): the row survives
  as `cancelled` with a `cancelled_at` stamp, so a freed seat stays auditable
  against the money it moved. Only `booked` rows count against `capacity`,
  which is what genuinely frees the seat — and re-booking is the same row
  flipped back. The call is **idempotent**: cancelling an already-cancelled
  seat answers `200` with the row as it stands *and replays the refund*, so a
  cancel cut short between the flip and the reversal (the tab closed, a proxy
  timed out) is recovered by sending it again. Refusing it as a `409` — as it
  once did — made that state permanent: no route could append the missing
  reversal, and the student stayed billed for a seat they no longer held.
- **A manager+ may cancel anybody's seat.** Booking is student-and-parent
  only, and cancelling used to be the same door — which left a seat nobody on
  the API could give back the moment its student was promoted to staff or its
  parent was unlinked: the menu refused its own deletion forever and the charge
  could never be reversed, since cancelling is the only route that reverses
  one. A role change deliberately sweeps no bookings on its own; moving money
  is a decision, not a side effect.
- **The capacity check is a single conditional write, decided by the
  database.** Counting rows and then writing one would be write-skew —
  SurrealDB does not conflict-check a cross-record count against a concurrent
  insert, so 24 students racing for 3 seats would all pass the count — so the
  seat is instead claimed on a counter kept on the **menu row itself**, taken
  and spent by one `UPDATE … WHERE` that also places the booking row in the
  same transaction. No lock is involved and none would help: a duplicate
  `POST` rolls its own seat back rather than costing a stranger their place.
- **One cutoff closes both ends — for the people it is aimed at.** Within the
  school's `meal_cancel_cutoff_minutes` of the meal's serving time, neither a
  new booking nor a cancellation lands (`409`) — the kitchen's headcount has to
  settle at some point. `null` (the default) means no cutoff at all. It binds
  **students and parents only**: a manager+ cancelling somebody's seat is not
  held to it, because the deadline exists to stop students gaming the headcount
  and that is no reason to leave staff holding a seat they cannot free. Past
  the cutoff an uncancellable seat also made its menu undeletable forever (a
  menu with a live booking refuses its own deletion) and its charge
  unreversable, since cancelling is the only route that reverses one. A meal
  already closed is therefore still freeable, and its menu still deletable, by
  manager+ — the same state a school that set no cutoff at all runs under. The
  cutoff is measured back from the menu's `date` **plus the slot's
  `serving_minute`** (`GET /settings`), so a two-hour cutoff on a lunch served
  at `720` closes at 10:00 UTC that morning, not at 22:00 the night before.
  **That minute is UTC**: `date` carries no timezone and the backend stores no
  school timezone — deliberately, it is a rejected feature — so a UTC+3 school
  enters `540` (09:00 UTC) for a meal served at noon locally. A slot with no
  `serving_minute` falls back to **midnight UTC starting the meal's day**, the
  behaviour every booking had before the field existed; the booking is never
  refused for want of a serving time.
- **The serving time is read live, not snapshotted onto the menu.** The cutoff
  minutes are already read live, so freezing the other half of the same
  deadline would make one policy edit apply and its twin not; a kitchen that
  moves lunch an hour later wants today's menus to move with it. The menu
  still snapshots the slot *name* (that is what keeps a retired slot's history
  readable), so a menu whose slot has since left the list simply has no
  serving time and falls back to midnight UTC.
- A menu somebody still holds a seat on **cannot be unpublished** (`409`) —
  cancel the bookings first, so nothing is left pointing at a deleted meal.

`GET /meals/bookings/me` is the caller's own list: the seats held *for* them
plus, for a parent, the seats held for every student they **currently** hold a
link to — so a parent's list is their children's meals. The links are re-read
on every call rather than trusted from the booking's `booked_by` stamp: an
unlinked parent stops seeing the child's seats at once, including the ones
they booked and paid for themselves, exactly like every other parent read. `GET /meals/menus/{id}/bookings` is the kitchen's list for
one menu (manager+), cancelled rows included so the changes are visible.

### Meal attendance

Whoever stands at the canteen door records who actually ate:
`POST /meals/menus/{id}/attendance` with `{student_id, status}`, teacher+ (a
student never marks, not even themselves). The status is the fixed pair
`served` / `missed` — deliberately **not** the school's editable
`attendance_statuses`, since a canteen line has no "late" or "excused", and one
list meaning two things would let a school change one by editing the other.
One row per (menu, student) keyed by a composite id, so a correction re-marks
the same row instead of stacking a second one.

The menu has to still be there: a mark whose menu is unpublished mid-request is
a `404` and writes nothing. The existence check *is* the write's own target
rather than a read the write then trusts, because a menu's id is deterministic
on day+slot — an unconditional mark landing just after the delete committed
would be swept by nothing and would reappear as a mark on the next menu
published for that meal.

**Attendance has zero billing effect.** Booking is the sole charge trigger, so
a student who booked and did not eat still pays — the kitchen bought the food.
There is no no-show penalty, no refund-on-missed, and no auto-reversal:
nothing under `meal_attendance` writes to the ledger. A walk-in with no live
booking *can* be marked `served` (the record is operationally true) and is
likewise not charged for it — charging there would be a ledger write. The
target only has to *exist*: a canteen also feeds staff, so any user may be
marked, not only a student. A mark is a record of what happened, not a check
of who was entitled, and since it moves no money it costs nobody anything.

`GET /meals/menus/{id}/attendance` is the per-menu list (teacher+), and
`GET /meals/attendance/{user}` is one student's history, narrowable with
`?from=&to=` — inclusive `YYYY-MM-DD` bounds compared against the *menu's*
day, a lexical compare that is chronological for that format. Same gate as the
balance and the ledger: the caller's own id always passes, anyone else's needs
teacher+ or a `parent_link` to that student.

### The ledger

The money is an **append-only ledger** (`meal_ledger`): one line per `charge`,
`credit`, or `reversal`, every field `READONLY`, and **no code path anywhere
that updates or deletes one**. A ledger line that can be edited or dropped
silently rewrites a student's financial history with no trace of the rewrite —
this is the one genuinely irreversible part of the food program, so a mistake
is corrected by appending the opposing line, which leaves both the mistake and
the correction visible.

**No balance is stored, anywhere.** It is always derived:

```text
balance = SUM(credit) + SUM(reversal) - SUM(charge)
```

in **minor units** (kuruş) as an `i64` — no float, no decimal, at any layer.
A *negative* balance means the student owes the school; a positive one is
money on account. Amounts are stored positive; the sign lives in the `kind`.

- **Booking is what charges, at a price snapshot.** The menu's dishes are
  summed the moment the seat is taken and that number is frozen onto the
  charge. Editing a dish's price afterwards moves no existing charge — what a
  student owes is what the menu cost the day they booked. The price is
  snapshotted *before* the seat is given, so a menu that cannot be charged
  refuses the booking rather than leaving a booked-but-unbilled row (`400`,
  when the dishes sum past 10 000 000). A free menu writes no line at all —
  but the booking row records that it *was* free, so "free when taken" is
  never confused with "not billed yet": pricing the menu afterwards leaves
  every seat already taken on it free.
- **Charge and reversal are keyed by `(booking, attempt)`.** The seat, its row
  and its money move together in one transaction on *both* sides — the claim
  writes the charge, the flip writes the reversal — and the ledger id is derived
  from the seat plus how many times it has been taken — so eight simultaneous
  `POST`s of one seat write one charge, a retried cancel refunds once, and a
  write that failed halfway heals when the request is repeated. Booking the
  same seat twice bills once because the *identity* is the same, never
  because a scan happened to see the first charge in time.
- **Cancelling appends a `reversal`** for the charge's exact amount, with
  `source` pointing at the charge it undoes, and it is written **by the same
  transaction that flips the seat** — not appended after it. The two as
  separate writes left a crash in between with a seat given back and the
  student still billed, and no later cancel would ever repay it, because the
  money is keyed to the attempt the flip had already consumed. Folded in, the
  seat cannot come back without the money. Booking is folded the same way — the
  charge rides the claim's own transaction, because a charge appended afterwards
  let a cancel land in the gap, reverse nothing (there was no charge yet), and
  the bill arrive anyway: a student holding no seat and owing money. The reversal is written only when
  the charge it undoes is really there, checked inside that same transaction —
  a refund with no charge behind it invents money. The charge row itself stays.
  Repeating a cancel replays the reversal, which is what heals a seat flipped
  before that was true and makes the refund recoverable rather than a one-shot
  the network can lose; the cutoff is deliberately not re-checked on that path,
  since the seat is already given back. Re-booking afterwards is a **fresh**
  charge at the then-current price.
- **The price snapshot is pinned by the menu's revision, not by a lock.** The
  dishes are summed a round trip before the seat is claimed, so a dish landing
  in between would otherwise be frozen onto the row as "the menu was free
  then" — and nothing heals that, since a seat already held is returned as-is,
  never re-priced. Instead every dish write bumps a revision counter on the
  menu, and the claim that takes the seat *also* asserts the menu is still at
  the revision the price was read at: the booking is refused, re-reads, and
  prices and seats itself again together. A lock around the read could not have
  promised this — the price is read a round trip before the claim either way —
  so what is frozen is always a price the menu genuinely carried at the instant
  the seat was taken. A menu edited over and over under one booking gives up
  after a few rounds with a `409`.
- **A no-show still pays.** Meal attendance has zero billing effect — nothing
  in the ledger reads or writes it. The seat was reserved and the food was
  cooked; there is no no-show penalty and no no-show refund.
- **`POST /meals/credits` is admin-only**, not manager: writing down cash
  received is the highest-trust action in the app. The target must be a
  **student, or anyone who already carries meal-ledger lines**: only a student
  runs up a meal balance, so crediting anyone else is a typo and a typo here is
  money in the wrong ledger — but a debt outlives its debtor's role change, and
  the student-only rule alone made a promoted student's debt permanently
  unsettleable, since no other route appends a credit. A mistyped staff id
  carries no lines, so it is still a `400`. It appends a `credit` for a
  student with a positive `amount_minor` (≤ 10 000 000), an optional `method`
  ("cash", "havale", …) and `note`, and records the admin as `recorded_by`.
  There is no payment gateway and no card data, ever. An over-credit is
  corrected with a compensating line, never a fix-up.

`GET /meals/balance/me` is the caller's own balance. `GET
/meals/balance/{user}` and `GET /meals/ledger/{user}` (paged, newest line
first) read a student's: your own id always passes, anyone else's needs
**manager+** or a `parent_link` to that student — **a teacher gets a `403`**,
the one pair in this block narrower than the rest of it. Canteen debt is
family debt: the money follows the `/payments` rule, not the classroom one,
while the dietary profile, booking and attendance reads stay teacher+. A charge's `source` is the
booking id it came from, a reversal's is the charge line it reverses, and a
credit has none.

## Payments: fee plans, assignment & the fee ledger

School fees live in their own tables (`fee_plan`, `fee_plan_assignment`,
`payment_ledger`) and their own balance. **Meal money is separate** — two
ledgers, two balances, and no route folds one into the other: what a family
owes the canteen and what it owes the school are different debts, and mixing
them would make either statement unreadable.

Every write here is **manager+**. Reads are narrower than the other
per-student reports: the student themselves, a parent holding a live link to
them, or manager+ — **a teacher gets a `403` on every `/payments` route**,
because what a family owes the school is not classroom information. The
canteen's balance and ledger follow the same rule, for the same reason.

### Fee plans and what turns them into money

A **fee plan** (`POST /payments/plans`) is a name and 1 to 60 **installments**,
each `{amount_minor, due_at}` — minor units (kuruş) as an integer, and unix
milliseconds. `due_at` **may be in the past**: a school adopting the app
mid-year assigns plans whose first installments were already due, so there is
no future-date rule here.

Writing a plan bills nobody. **Assigning it does** (`POST
/payments/plans/{id}/assignments` with `{student_ids}`, at most 200 per call):
that appends *every* installment as a `charge` line right away, each carrying
its own due date. There is no scheduler, no nightly sweep, and nothing that
wakes up when a date passes — the whole schedule is written once, and lateness
is read off it.

Assignment is **replay-safe by identity**: the assignment row is keyed
(plan, student) and every charge it raises is keyed (plan, student,
installment), so re-assigning bills nothing a second time (that student comes
back `already_assigned`), and an assignment cut short after three of twelve
charges landed completes itself when the call is simply repeated. Only
students carry a fee record, so any other target comes back `rejected` — one
bad id never loses the rest of the batch.

A plan that has been assigned to anyone can no longer be edited or deleted
(`409`). Its charges are frozen copies of the installments as they stood, so
an edit would only make the plan and the money tell different stories — write
a new plan instead.

That freeze is a **counter on the plan row** (`assignment_count`), incremented
in the *same transaction* as the assignment row it counts, and the edit and the
delete are single-record conditional writes against it — the same shape as a
term's `course_count`. It used to be a `SELECT` taken before the write, which a
concurrent assign could land behind, leaving a plan edited *and* assigned, or
deleted with a live assignment naming it. Now the two contend on one record: an
assign either freezes the plan first (and the edit or delete is a `409`) or
arrives after it (and bills the edited plan — the installments a first
assignment charges are re-read from the *stored* plan at the instant it
freezes, never from the copy the request first looked at). A plan deleted out
from under a running batch is `rejected` for that student on, in the same
per-student report as every other outcome — the students it already billed are
never dropped from the answer. A `PATCH`
carrying no field at all writes nothing, so it is not refused.

Because an assignment is never removed, the counter is only ever claimed and
never released: once assigned, a plan stays frozen for good. Plans written
before the counter existed are seeded from their assignment rows at boot,
before the server accepts a request — an absent counter reads as zero, and a
zero would have re-opened every already-assigned plan on an existing volume.

### The ledger

The money is an **append-only ledger** (`payment_ledger`), exactly like the
meal one: every field `READONLY`, and **no code path anywhere that updates or
deletes a line**. A ledger line that can be edited or dropped silently
rewrites a family's financial history with no trace of the rewrite, so a
mistake is corrected by appending the **opposing line**, which leaves both the
mistake and the correction visible.

Four kinds, and the sign lives in the `kind` — amounts are always stored
positive:

| Kind       | Sign | What it records                                  | `source` points at |
| ---------- | ---- | ------------------------------------------------ | ------------------ |
| `charge`   | −1   | An installment billed by a plan assignment       | the assignment     |
| `credit`   | +1   | Money received, against one named charge         | the charge paid    |
| `reversal` | +1   | A `charge` or `refund` entered in error, undone  | the undone line    |
| `refund`   | −1   | Money handed back, against one named payment     | the credit returned |

**No balance is stored, anywhere.** It is always derived:

```text
balance = SUM(credit) + SUM(reversal) - SUM(charge) - SUM(refund)
```

in **minor units** (kuruş) as an `i64` — no float, no decimal, at any layer. A
*negative* balance means the family owes the school; a positive one is money on
account.

- **A payment names the charge it settles.** `POST /payments/credits` takes
  `{charge_id, amount_minor, method?, note?}`: allocation is *recorded*, never
  inferred from a balance, so a statement can say which installment is still
  open rather than only how much is outstanding. **Partial payments are the
  norm** — several credits accumulate against one charge.
- **A refund names the payment it returns.** `POST /payments/refunds` takes
  `{credit_id, …}`, partials allowed, capped by what that credit was worth.
  This is also the **only** way a mistaken credit is corrected: a credit is
  never reversed, so money leaving the school is always spelled the one way.
- **A retry is not a second payment — when the client says so.** Both routes
  take an optional `request_key` (`[A-Za-z0-9-]`, 1 to 64 characters — `_` is
  the separator inside a ledger id, so a key may not carry one). With
  one, the line is keyed `<charge>_k_<key>` (a refund: `<credit>_kr_<key>`), so
  a client retrying after a network timeout gets **the line the first attempt
  wrote**, not a second charge on the family — idempotence by identity, the
  same rule that keeps a replayed assignment from billing twice, never a "has
  this been recorded yet?" scan two concurrent requests can both walk past. The
  replay is answered **before** the over-payment cap is consulted, so a payment
  that filled its charge to the penny still replays as itself instead of coming
  back "already paid in full". Sending that key again with a **different**
  `amount_minor` or a different target is a `409`, never the stored line: that
  is a client bug, and a `201` would bury it. Omit the key and nothing changes
  from before — two identical posts are two payments, which is exactly what a
  desk taking the same amount twice means.
- **A reversal only undoes a `charge` or a `refund`**, for its exact amount and
  nothing else (`400` on any other kind). It is keyed `<line>_r`, so a line has
  at most one reversal however often the call is retried, and the reversed line
  itself stays on the record beside it.
- **A refund frees the charge's room.** The cap on a payment is folded over the
  target's whole source subtree, so refunding a payment gives that charge its
  room back and the charge **can be paid again** — and reversing that refund
  takes the room back with it. Money that came back out is not money the school
  still holds.
- **The over-payment cap is not the database's.** The `409` past a charge's or
  a credit's worth is a cross-record fold, which SurrealDB cannot enforce on
  its own; it holds because the backend is one process and the fold, and the
  append it authorizes, are taken under one lock. Should an over-payment ever
  be recorded anyway it is not a crisis: this is human data entry at an office
  desk, the outcome is an over-paid charge that is plainly visible in the
  statement, and it is undone by appending a refund. Both lines are true
  records of money that really arrived — refusing them would be the worse lie.
- **There is no payment gateway and no card data, ever**, and none is planned:
  nothing here talks to a bank, a PSP, or a card network. `method` is free text
  ("cash", "havale", …) describing how money that already arrived was handed
  over, and `note` is whatever the office needs to remember (a receipt number,
  say).

### Statements, balances and `overdue`

`GET /payments/ledger/{user}` is the raw lines, newest first, paged.
`GET /payments/statement/me` and `/payments/statement/{user}` are the
**per-charge rollup** (its rows paged by the usual opt-in `?limit=&offset=`,
in an `entries: {items, total, limit, offset}` envelope): one row per charge with its plan, the installment's
amount and due date, what it collected, what went back out, what is still
outstanding, whether the charge itself was reversed (such a charge owes
nothing), and whether it is **`overdue`** — still owed, and its `due_at` has
passed.

All of that is folded from the raw lines **on every request and stored
nowhere**, `overdue` included. Paging windows the *returned rows* only, after
that fold: `balance_minor` — and every row's own arithmetic — is identical on
every page, because it is read from all of the student's lines either way. There is no overdue flag, no sweep that sets
one, and no stored rollup: a stored rollup is a second version of the truth,
and the ledger is the first. `GET /payments/balance/me` and
`/payments/balance/{user}` are the same fold reduced to one number.

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
  All four reads are refused (`409`) while the caller's latest sitting is still
  in progress — a mark on sitting 1 must not open the answer key to someone
  midway through sitting 2. Submit the sitting first; an expired one reviews
  fine.

  That gate is *per exam*, and a second, narrower one covers what it cannot see.
  Instantiating one question bank template into two exams copies its `correct`
  verbatim into both (the bank is a copy-into-exam model, not a link), so
  reviewing exam A used to hand out the answer to a question the caller was
  still writing in exam B. Review now blanks those questions instead of closing:
  a question whose bank template also sits under an exam the caller has an
  in-progress sitting on comes back with `correct: null` from `GET
  /exams/{id}/review/questions`, and with `is_correct: null` and no share of
  `auto_score` on the answer sheet. The key is what is hidden, never the
  question or the caller's own answer. Keyed on the template rather than the
  exam deliberately: widening the gate to "no in-progress sitting on *any* exam"
  would close review far too broadly, since `open`-mode sittings never expire on
  their own, whereas this hides only the overlapping questions and gives the key
  back the moment that other sitting is submitted or expires. A question links
  to a template whichever way it got there, and both links are matched: the
  template it was instantiated from (`from_bank`) *and* the template minted by
  saving it into the bank (`banked_as`). So a question written by hand into
  exam A, saved to the bank, and then instantiated into exam B is hidden while
  B is live — the two copies hold the identical `correct` even though A's copy
  never came out of the bank. Only a question with no bank link at all, in
  either direction, is never hidden.

  **Known limit, accepted:** deleting a bank template clears *both* columns —
  `from_bank` on every question it produced and `banked_as` on the question it
  was saved out of — so a pair linked through a template that was since deleted
  keeps the identical `correct` with nothing left to join them by. That overlap
  is invisible to the redaction. It is the only one left: the copy is made at
  instantiate time and the link is written in the same request, so a shared
  `correct` and a live template link are created together and only a template
  delete separates them again.

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
| client →  | `{"type":"answer", "question_id":"…", "selected":1}` or `{"type":"answer", "question_id":"…", "text":"…"}`, optionally `+ "client_seq":7` |
| server →  | `{"type":"saved", question_id, updated_at, client_seq?}` — the autosave ack |
| client →  | `{"type":"finish"}` — submit the attempt |
| server →  | `{"type":"finished", finished_at}`, then Close |
| server →  | `{"type":"expired"}`, then Close — a tick noticed the deadline |
| client →  | `{"type":"ping"}` → server `{"type":"pong"}` |
| server →  | `{"type":"error", message, question_id?, client_seq?}` — bad JSON, wrong kind, deadline, … |

An `answer` may carry a `client_seq`: any number the client picks, echoed verbatim on
that message's `saved` or `error` and on nothing else. The server never reads
it — no uniqueness, no ordering, no dedupe, all the client's business — and
omits the key entirely when the request omitted it, so leaving it out keeps
today's frames byte for byte. It exists because `question_id` is not an
identity: save a question, time out, save it again, and two sends are
outstanding for the same id — the first reply would settle the second and the
UI would claim "Saved" for an answer the server never took. `question_id` on
an `error` is the separate, orthogonal question: whether the failure belongs
to that one question (payload or question) or to the room, in which case every
save in flight is equally refused. An error can carry `client_seq` without
`question_id`.

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
  if (m.type === "saved") settle(m.client_seq, m.question_id);
};
let clientSeq = 0;
const save = (question_id, selected) =>
  ws.send(JSON.stringify({ type: "answer", question_id, selected, client_seq: ++clientSeq }));
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
subject is a `400` (create and PATCH alike). The reverse guard does **not**
hold here, unlike exam questions and homework: a template never blocks a
subject delete, which simply clears `subject` off every template that carried
it (only a template's owner may re-tag it, so a `409` would be one a manager
could never clear — and a private template raising it would leak its
existence). Reads are school-wide: `GET /bank-questions` lists the
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

**Roll call is also what credits two badge counters** (see "Badges"). The
**first** mark taken for a lesson credits its teacher's `lessons_held_total`,
once — a lesson counts as held when its roll call is taken, so one merely
scheduled (and then cancelled) counts for nothing, and the marks after that
first one credit nothing further. And a **student** marked `present` or `late`
gains a `lessons_attended_total`, using the same cut the attendance `rate`
below uses; correcting that mark to any other status, or clearing the row, gives
it back. The session teacher's own presence row moves neither counter — the
attendance one is a student's badge, and a teacher does not attend their own
lesson.

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
Finishing a stint also feeds the **study streak** behind the `study_streak`
badges: consecutive **UTC** days on which one was finished, kept as the longest
run ever held (see "Badges").

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

## Class sections (şube)

A **class section** is the group a school actually teaches in — 9-A, 10-B — and
here it is a bulk-enrollment tool rather than a second kind of membership. Its
row carries a name, an optional free-text `grade` label in the school's own
vocabulary (`"9"`, `"Lise 2"`; `""` means no grade, exactly like omitting it)
and an optional `term_id`; nothing else about a course changes because a class
exists. Schools that run electives or a college-style timetable simply never
create one — individual enrollment is untouched, and classes are a
convenience.

**The homeroom teacher.** A class may also name one — the *sınıf öğretmeni* —
with `teacher_id` on create or `PATCH`, and it rides back out as a `teacher`
person block (`null` when there is none, exactly like `grade`; both `null` and
`""` clear it). The account must exist and hold **teacher, manager or admin**
(anything else is a `400` naming `teacher_id`, the same shape a non-student
member gets). It is a label and nothing more: it grants no rights over the
class, is not counted, and is not a second teacher assignment — courses keep
their own `teachers` list. A role change that takes the account below `teacher`
clears the column on **every** class it held, in the same sweep that drops
their course assignments, so no section ever lists a demoted account.

That sweep runs once, over the rows that exist when it runs — so a demotion
that lands *between* a request's role check and its write would sweep nothing
and leave the assignment standing forever. Both writers close it from the other
end: `POST /classes`, `PATCH /classes/{id}` and `POST /courses/{id}/teachers`
re-read the account's live role **after** their write and answer `409` if it
has since dropped below `teacher`, taking the assignment back (a create is
rolled back whole — no half-made class is left behind). Whichever side is
second catches it; the ordinary path costs one extra read and no extra write.

**The pump.** A class holds **members** (students) and **attached courses**,
and owes the product of the two: every member enrolled in every attached
course. So both writes push the same way — attaching a course
(`POST /classes/{id}/courses`) enrolls the whole roster into it, adding a
member (`POST /classes/{id}/members`) enrolls them into every course the class
already carries — and what gets written is an ordinary `enrollment` row, the
same one `POST /courses/{id}/enrollments` writes, counted against the same
`enrollment_count`. Each row a class writes is tagged with it as the row's
`source`; **no `source` means placed by hand**, and that one bit is what makes
the sweeps below safe. It rides back out on every enrollment response
(`GET /courses/{id}/enrollments`) as the class's id, or `null` for a
hand-placed row — without it no client could tell which of the roster rows it
is showing a class change is about to remove.

**Already enrolled is skipped.** A pair that already has an enrollment row is
left exactly as it stands — no second seat charged, no `source` rewritten. A
student a teacher enrolled by hand *stays* hand-placed when the class later
attaches that course, so the class never quietly adopts someone else's roster
decision.

**Capacity is all-or-nothing.** Attaching a course, or adding a member, claims
one seat per pair that needs a new row, each against its own course's
`capacity` — and if any single course has no room the whole call is a `409`
naming it (`course:<key> is full, so the class cannot take this student`,
`course:<key> cannot hold the whole class`). Nothing lands, not even the seats
claimed earlier in the same run: a half-enrolled class is worse than a refused
one, and the refusal names the course whose capacity to raise.

A second `409` reads off the same claim and is deliberately told apart from it:
one of the class's attached courses **no longer exists**
(`course:<key> no longer exists — detach it from this class first`). A seat
claim that matches nothing means "full" *or* "no such row", and reporting a
stale attachment as a full course would send staff off to raise a capacity that
is not there — on a class every member-add now fails on. Detach the link and
the class works again.

**Removals take back only what the class pumped, and repair before they
delete.** Removing a member (`DELETE /classes/{id}/members/{user}`) or
detaching a course (`DELETE /classes/{id}/courses/{course}`) sweeps the
enrollment rows whose `source` is *this* class and no others — hand-placed
rows survive every class operation. Even a tagged row is not automatically
deleted: if a **second class** still claims that pair (it holds the same
student *and* that course attached), the row is re-tagged to that class
instead, because the row it skipped writing is the row now being swept and the
student is still owed the seat. The heir is the lowest class id among the
claimants, so repeating a sweep lands on the same class. Only a row nobody is
left to claim is deleted and its seat handed back. Sweeps also tolerate rows
that are already gone — deleting a course wipes its enrollments wholesale
while the class memberships survive — so "this class has a member" and "that
member holds a pumped row" are independent facts.

**A manual unenroll wins, permanently.** `DELETE /courses/{id}/enrollments/{user}`
on a pumped student is allowed and sticks: nothing re-pumps them while they
remain a member, because the pump runs on writes, never on a schedule. Staff
put them back by enrolling them by hand (which makes the row hand-placed) or
by detaching and re-attaching the course.

**And a manual enroll wins too, permanently.** `POST /courses/{id}/enrollments`
landing on a row a class pumped takes the row *off* that class — its `source`
is cleared, the response comes back with `"source": null` — so no later class
sweep can undo a placement an operator made on purpose. That is the mirror of
the rule above: hand-placed beats pumped in both directions, and without it
"hand-placed" was a state only a *first* enroll could ever reach.

**Delete guards.** A class still holding members or attached courses refuses
deletion with a `409` ("remove its members and detach its courses first") —
the roster it owes is never dropped out from under the courses silently. A
term linked by any class refuses deletion the same way courses make it refuse.
Deleting a **course** detaches it from every class it was on (its enrollments
go with it), and a role change that takes a user off `student` drops their
class memberships exactly as it drops their enrollments — in one transaction,
because a membership left behind would keep pumping them back into courses,
while an enrollment left behind would stay tagged with a class the released
counters had already made deletable, and nothing could ever sweep it again.

**A link to a deleted course detaches instead of refusing.** Should a
`class_course` row ever be left pointing at a course row that is gone,
`DELETE /classes/{id}/courses/{course}` still answers `204`: management rights
are read *off* the course, so a stale link had no readable owner and the detach
used to `404` forever — which also left the class permanently undeletable, its
attachment counter counting a row nothing could sweep. There is no roster left
to protect, and the caller is already teacher+.

**Who may.** Creating, editing and deleting a class, and adding or removing
its members, is **manager+** — a class is school structure, not classroom
work. Attaching or detaching a course takes management rights on **that
course** (its creator, a teacher assigned to it, or manager+), since the call
writes that course's roster and nothing else: the same right enrolling one
student takes. Every read (`GET /classes`, `/classes/{id}`, its members and
its courses, all paged, newest first) is **teacher+** — and "newest" here means
when the student was *added* or the course *attached*, not the student's or the
course's own id, which is what a link row keyed on the pair would otherwise
sort by. Rows written before that stamp existed carry none, and sort last.

**Except your own section.** Every route above is staff-only, which left a student
with no way to learn which class they are in. Two reads fix that, both paged
and both returning the same class objects: `GET /classes/me` is the caller's
own memberships (**any authenticated role** — staff, who are never members,
simply get an empty page), and `GET /classes/user/{user}` is somebody else's,
gated exactly like the per-student reports — **teacher+, or a parent linked to
that student**; anyone else gets a `403` that leaks no existence, and a user
who does not exist is a `404`. Ordering is by when the membership was added,
newest first, and `total` counts the memberships the window was cut from.

Both of them hide one field: **`creator` is `null` below teacher+**. A class is
created by manager+ only, so shipping the creator to a student (or to their
linked parent) would hand out an office account's username and real name — an
identity `GET /users` (admin-only) and `/users/search` (teacher+) both withhold.
The homeroom teacher is *not* hidden: naming them is the point of the read. On
every staff-facing route (`GET /classes`, `/classes/{id}`, and the create/edit
responses) `creator` is populated exactly as before.

**Codes.** Adding a member or attaching a course answers `201`; a repeat is a
`409` ("the student is already in this class", "the course is already on this
class"). An id in the **body** that names nothing — an unknown `user_id` or
`course_id`, or a member who is not a `student` — is a `400`; an id in the
**path** that names nothing (the class, or a member/course that was not on it)
is a `404`. Bounds: name ≤ 200 characters, grade ≤ 20, **at most 200 students
and 50 courses** per class (`max_class_members`, `max_class_courses`), all four
published in the `course` group of `GET /limits`. Past either ceiling the
add or the attach is a `409` naming it ("this class already holds 200
students") — a standing class someone can make room in, which is why it is not
the `404` a deleted class answers. Those two numbers are not comfort limits:
adding a member writes one enrollment per attached course and attaching a
course writes one per member, both in a *single* transaction, so each axis's
ceiling is the bound on the other axis's write loop — an unbounded class is an
unbounded transaction any manager could trigger.

Which is why each write is refused on the **other** axis too. A class already
standing *above* a ceiling cannot take a member while it holds more than 50
courses ("this class holds more than 50 courses — detach some before adding a
student"), nor a course while it holds more than 200 students ("this class
holds more than 200 students — remove some before attaching a course"), both
`409`. Room on the axis being written is not the question: the write loop's
length is set by the other one, and letting it run because *this* side has
space is exactly the unbounded transaction the ceilings exist to prevent. Only
a class that predates the ceilings can be there, and only shrinking the
overloaded axis clears it.

### Grade blueprints

A Turkish school runs many şube at one grade and stocks each with the same
courses. A blueprint says that list once: `POST /classes/blueprints` with a
`grade` label and `course_ids`, and every section already at that grade is
stocked immediately. `POST /classes/{id}/blueprint` stocks one section from its
grade's template — idempotent, so it is safe on a class that already carries
some of the courses.

The blueprint is a template, not a new kind of membership. Applying it calls
the same attach a manager's own `POST /classes/{id}/courses` does, so what
lands is ordinary `class_course` links and ordinary `enrollment` rows, and an
elective a student takes alone stays an individual enrollment nothing here can
see.

**Editing retro-pumps.** `PATCH /classes/blueprints/{grade}` takes the whole
new list (a set, not a delta) and reconciles every section at the grade,
including the ones that existed before the blueprint did.

**Creating a section stocks it.** The other direction of the same rule:
`POST /classes` at a grade a blueprint covers runs that blueprint's pump on the
new section itself, so a manager opening "9-D" does not have to remember a
second call. Its `201` is therefore `{class, skipped, stocked_from}` — the class
where the bare class object used to be, the pump's usual `skipped` list, and
`stocked_from`, the grade label of the template that stocked it. `stocked_from`
is `null` when **no** template covers the grade, which is what tells that apart
from a template that applied cleanly (`skipped: []` alone reads the same either
way — the ambiguity `matched` closes on the grade-wide pumps).

Stocking is best-effort all the way to the end of that route: a template that
could not be read, or a pump that faulted part-way, leaves `stocked_from` null
rather than turning a class that *exists* into a `500` whose caller never learns
its id — a class id is a ULID and `POST /classes` is the only place it is
returned from, unlike a blueprint, whose id is the grade label the caller sent.
The retry is `POST /classes/{id}/blueprint`, which is idempotent and is also
what a caller runs when there is genuinely no template, so the two cases need
no telling apart. The stocking runs **after** the homeroom-teacher rollback, and
must: a class holding courses refuses deletion, so a section stocked first could
not be rolled back and the `409` would leave one standing behind a promise that
nothing was created.

**Pumping is best-effort.** Each (section, course) pair is one all-or-nothing
transaction. A pair that would breach a limit — the section is at
`max_class_courses`, or the course has no free seat for the whole section — is
skipped and reported in `skipped`, naming the class, the class's name, the
course and the reason; every other section is still stocked. So a blueprint
edit is allowed to leave a partial state: one full course must not stop the
other eleven sections from being set up. Nothing moves on a skipped section —
not its attachment count, not a seat on the course.

**`matched` is how many sections the pump reached**, and it is on both write
responses (`POST /classes/blueprints` and `PATCH /classes/blueprints/{grade}`)
because an empty `skipped` alone cannot be read: a template that stocked every
section and one that found no section at all return the same list. A grade
label is free text and matched exactly, so `"9 "`, `"9-A"` and `"9"` are three
different grades — `matched: 0` with an empty `skipped` means the label does
not match the one those sections carry, and the fix is the label, not the
template. It counts the sections **reached**, not the ones the grade holds:
`blueprint_deleted` ends the run, and then the number is what happened.
`POST /classes/{id}/blueprint` has no `matched` — it pumps the one section in
the path — and neither has `POST /classes`, for the same reason: the one
section it pumps is the one it just created, and `stocked_from` already says
whether a template was found.

**`GET /classes?grade=<label>` is the read on the other side of that count.**
`matched: 0` says no section carries the label; this says which labels the
sections actually carry, so a manager holding a skip list can go find the three
sections at grade `"9"` and fix them. The match is verbatim — no trimming, no
case folding — for the same reason the pump's is: the label *is* a blueprint's
record id, schools spell their ladders differently on purpose, and a filter that
normalized would disagree with the pump it exists to debug. `?grade=` with an
empty value is *no* grade, exactly as an empty `grade` on a write means no
grade, so it lists the sections a blueprint can never cover; omit the parameter
entirely for every class. An unknown label is an empty page, never a `404` —
there is no such thing as a grade that does not exist. The filter is part of the
query, so `total` counts the filtered set and `?limit=&offset=` pages through
that set alone; a label longer than 20 characters is the same `400` a create
gives.

**`GET /classes/blueprints/{grade}/status` is the standing version of that skip
list.** A pump is best-effort, so a partial state is normal — but the skips only
ever existed in the one response body that reported them, and a manager who
refreshed the page had no way left to ask which sections were out of sync. This
read answers it from stored state: `{grade, courses, matched, sections}`, where
each section is `{class, class_name, missing}` and `missing` is the template
courses that section does not carry. `matched` is the same count the pumps
report — how many sections carry the label — so `matched: 0` here means the same
thing it means there. It is **read-only**: nothing is attached, detached or
pruned by looking, and a `404` means no blueprint covers the grade. Unpaged, for
the same reason the pump's own section list is: it is the şube one school runs
at one grade, and the caller is asking about all of them.

A course a human attached by hand **counts as carried**. The template asks for
the course, not for the pump's tag, and a course already on the class is a no-op
for a pump whoever attached it — a status read that disagreed would send
managers chasing rows no pump will ever write. A course in the template that no
longer exists is the one deliberate piece of noise: it reads as missing from
every section, because that is the truth about the section and this route may
not write, so the dangling id stays until the next pump prunes it.

**Finishing a partial pump** is one of two idempotent calls, and neither needs
any new machinery:

- `POST /classes/{id}/blueprint` re-stocks **one** section from its grade's
  template, skipping whatever it already carries.
- `PATCH /classes/blueprints/{grade}` with the **identical** course list re-runs
  the whole grade's pump: the compare-and-set matches the unchanged list, no
  course is dropped (so nothing is detached), and every course is re-attached
  idempotently. Fix whatever caused the skip first — free a seat on the full
  course, take a course off a section standing at its ceiling — and then repeat
  the call.

**A class whose `grade` is edited after it was stocked keeps what it has.** The
courses the *old* grade's blueprint attached stay attached and keep carrying
that blueprint's tag, so dropping one of them from that old template still
detaches them from this class; and the class takes nothing from its *new*
grade's template until somebody runs one of the two calls above (or that
template is next edited, which pumps every section then at the grade). This is
deliberate: reconciling on a label `PATCH` would sweep the old template's
courses and, with them, delete live enrollments as a side effect of a rename.
The drift is real, and this status read is what makes it visible instead of
silent — the moved section shows up under its new grade with the new template's
courses listed as `missing`.

A skip's `reason` is a **machine code**, not a sentence: the client owns the
wording (and the language), the same id-plus-client-label shape roles and
course kinds have. The set is closed, and each code names the record that
actually failed: `class_deleted` (the section vanished mid-pump),
`course_deleted` (the course was deleted; the pump also removes the dangling id
from the template, so the template shrinks and the skip is reported once for the
whole grade — the sections after the first one are not asked again — and never
again on a later pump), `class_at_course_ceiling`, `class_roster_too_large`
(the section holds more students than one attach may enroll at once),
`course_full` (no free
seat for the whole section), and
`blueprint_deleted` (the template itself was deleted while the pump ran —
nothing was attached, and there is nothing left to retry). That last one **ends
the run**: it says nothing about the (section, course) pair it names, so every
remaining section would only repeat it. It is reported once, the sections
already stocked stay stocked, and the call still succeeds.

**The manual attaches answer the same vocabulary.** A `409` from
`POST /classes/{id}/members` or `POST /classes/{id}/courses` is
`{"error": "<sentence>", "code": "<machine code>"}` — the prose unchanged, the
code out of the set above plus two a pump never reports: `duplicate` (already a
member / already attached) and `linked_course_missing` (another course already
attached to that section no longer exists — detach it first), which only
`POST /classes/{id}/members` can meet, since a member add is the one attach
that walks the section's existing course links while a pump attaches a course
it has just proved alive. One cause therefore reads the same whether a manager
hit it by hand or a pump hit it in bulk, which is what lets a bilingual client
branch and word it once. `code` is published on those two routes only and is simply **absent**
from every other error body.

**Removal spares what a human placed.** Every attachment a blueprint makes is
tagged with it. Dropping a course from the list detaches it only where the
blueprint attached it (sweeping the enrollments it pumped, repairing to a rival
class first exactly as a manual detach does), and a course a human attached to
that class by hand carries no tag and is left exactly where it is. Deleting a
blueprint applies that to its whole list.

`DELETE /classes/blueprints/{grade}` removes the row **first** and then sweeps
by that tag, rather than by the list the call read: an edit that adds a course
and pumps it while the delete runs would otherwise leave rows tagged with a
blueprint that no longer exists, and since the grade label *is* the record id,
nothing could ever reach them again. The pump carries the other half of that —
an attach whose blueprint was deleted mid-run writes nothing and is reported as
`blueprint_deleted`. Those two narrow the window rather than close it — a pump
that read the template alive can still commit its link after the sweep has run,
since a read of one record and a write of another are not serialized against
each other — so the delete and each individual attach are also serialized in
process: the delete holds a write lease across its compare-and-set and its
sweep, and a pump takes a read lease one course at a time, so a delete never
waits behind a whole grade. The backend runs single-replica by decision, which
is what makes an in-process lock the complete answer. The delete is also a
compare-and-set on the list the call read: a `409` means somebody edited the
template in between, and nothing was written — though a template deleted and
recreated at the same grade with the same list satisfies that comparison, which
is accepted, since the end state is the one the caller asked for. The remaining
cost is a **process crash** between the delete and its sweep, which no lock
survives: it leaves inert tagged attachments behind, still detachable one at a
time at `DELETE /classes/{id}/courses/{course}`, with every counter exact.

A blueprint names no term — the class names its own. The grade label is the
blueprint's id, so there is one per grade (a second is a `409`), and it must be
non-empty and contain none of `/ \ ? # %`.

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

The `503` ordering matters: availability is checked before the rows exist, so
an unavailable service leaves no dead `pending` row behind, and the rate limit
is charged before that, so a refused turn leaves no trace at all. The check is
the backend's own worker registry, which owns the QUIC sockets and is therefore
the whole truth about what can be answered.

### A turn always settles

Once the two rows exist the answer never stays `pending` forever, through
three independent mechanisms:

- the task that owns the bridge round trip stamps `complete` or `failed` —
  the normal path;
- a reader **projects** a `pending` row older than 300 seconds as
  `failed`/`timed_out` (a read-time projection, not a write — the row is left
  for the task that may still own it);
- a **boot sweep** flips every leftover `pending` past the 300-second horizon
  to `failed`/`interrupted`, which is what a process death mid-inference looks
  like. Younger rows are left alone, since one may still be in flight.

A restart therefore loses a turn that was in flight: it settles
`failed`/`interrupted` rather than being answered. Accepted — the backend is a
single process with stop-the-world deploys, so there is no peer that could have
taken it over.

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

## Collaborative whiteboard

A board is a shared canvas whose membership is an **ad-hoc invite list**: the
creator names participant user ids at `POST /boards` and that is the whole
model — no course, no lesson session, no appointment binds a board. Any account
from `student` upwards may open one. **Every participant draws; the creator
alone clears, locks, closes or deletes.** A teacher who does not want to type
thirty ids fills the same list in one call with `POST /boards/{id}/invite`
(below) — that is a bulk *write* into the ad-hoc list, not a binding: the board
still owns its roster afterwards.

The roster is spelled `participants` everywhere — in the `POST` body, in the
`PATCH` body and in every board response — so the field never changes name
between a request and a reply. That is the *only* thing the shared spelling
buys: unlike the rest of this API, the two board request bodies **reject an
unknown key** rather than ignoring it — the same `422` any JSON-bodied route
answers a malformed body with — because a misspelled roster
would otherwise open a board its author is silently alone on. So a body must
carry **only** the fields that request accepts — a board it just read is not a
legal body, since `id`, `creator`, `locked_by`, `epoch` and the rest are all
unknown to `POST` and `PATCH`. A read-modify-write client sends the writable
fields it changed, never the whole board object back.

**Roster repair.** A roster only ever holds users of at least the `student`
role. A demotion sweeps the user off every board they were on, and the first
boot of this version repaired the rosters an older binary left behind —
parents, and ids of users deleted since. On top of that, a `PATCH` may always
send back the roster it was just served: an id already on the board that has
stopped qualifying is dropped silently rather than refused, so a
read-modify-write never wedges on a value the server itself handed over.
Naming a *new* id that is unknown or a parent is still a `400`, and the whole
call is refused — the roster is never half-applied.

**Bulk invite.** `POST /boards/{id}/invite` fills the roster from a group that
already exists instead of one id at a time. The body is tagged by `kind`, the
same wire shape an event audience uses:

```json
{ "kind": "class",  "class":  "01J…" }   // everyone in a class section (şube)
{ "kind": "course", "course": "01J…" }   // everyone enrolled in a course
{ "kind": "event",  "event":  "01J…" }   // the event's expected-attendee roster
```

A **club is a course** (`kind` `club`), so "invite the whole club" is the
`course` form; so is a study group (`study`). The `event` form resolves exactly
what `GET /events/{id}/roster` resolves, which is the signup list for a
registration event and the class or course roster for the others — a
school-wide or role-wide event will normally overflow the cap, and that is a
`409` rather than a truncation.

It is the creator's call and **additive**: everyone the source names is *added*,
nobody is ever removed by it. Removal stays `PATCH /boards/{id}` — send the
roster you want.

**The ids are resolved once, at the call — this is a snapshot, not a
subscription.** A board keeps a flat list of ids and no memory of where they
came from, so a student who joins that class tomorrow is *not* on today's board,
and nothing the class does afterwards puts back a student the creator took off.
Re-inviting the same source is how a board is topped up after the class changed;
it adds only who is missing, so it is idempotent when nothing did — and it is
also the one thing that undoes a removal, because the source still names that
student. Removing someone from a board you intend to re-invite the class to is
therefore not a ban; there is no per-board exclusion list. Live resolution was the alternative and
was rejected here: it would turn the permission check behind every stroke, the
room's door and "which boards may I open" into cross-table queries, and it would
take the per-person removal away from the creator — which is most of what a
whiteboard roster is for.

Three filters run before the ids land, and all three are **silent**, because one
ineligible member must not fail the invite for the other twenty-nine: ids that
no longer resolve to a user are dropped, anyone below `student` is dropped (the
same cut that keeps `parent` off a whiteboard), and anyone already on the board
— the creator included — is not added twice.

The cap, though, is **all-or-nothing**: if the union would carry the board past
`max_participants` the whole invite is refused with a `409` naming both numbers
and the roster is left exactly as it was. A partial invite would silently pick
which half of a class gets to draw.

**Inviting a group discloses that group.** A board's roster is visible to every
participant, so each source carries the gate its own listing route carries —
teacher+ for a class (`GET /classes/{id}/members`), the course's creator, an
assigned teacher or a manager+ for a course (`GET /courses/{id}/enrollments`),
teacher+ for an event (`GET /events/{id}/roster`). A student may still build a
board one id at a time; they cannot pour a class roster into one. A source that
does not exist is a `400` naming the field, never a `404` — on these routes a
`404` means "no such board, or not yours", and reusing it here would tell a
creator their own board had vanished.

**No parents, anywhere.** The `parent` role is the school's read-only observer,
and it has no whiteboard access at all — not a view-only tier, none. It is
refused `POST /boards` and `GET /boards` with a `403`, and it cannot be *named*
in an invite list either: `resolve_participants` rejects a parent id with a
`400`, so a parent is never on a roster in the first place. That one cut is
what closes the rest — never on a roster means the id-scoped routes and the
room's door already answer the outsider's `404` (never a `403`, which would
confirm the board is there), and no socket opens, so no `stroke` frame can be
sent. The role bar is re-checked on the read path and at the door as well, so a
board row written before this rule that still lists a parent is inert: that
parent sees a `404` for it like any stranger.

**Two doors, two statuses.** A caller who is not on a board gets a `404` on
every route, its existence included — an outsider must never learn a board is
there. A participant who is not the creator has already been told it exists, so
the four creator-only commands answer them a `403`: hiding it at that point
would be a lie their client cannot act on. Over the socket the same refusal is
`error{code:"forbidden"}` and the socket **stays open** — someone clicking a
button they don't own must not lose the canvas they were drawing on.

**A clear deletes nothing.** Strokes are append-only. `POST /boards/{id}/clear`
bumps the board's `epoch` and appends a `clear` **marker** carrying the epoch
it closed, that epoch's final stroke count, who cleared and when — so the live
canvas empties while every mark ever drawn stays stored and replayable. Those
markers *are* the epoch index, which is why there is no epochs table and why
the open (unclosed) epoch is deliberately absent from `GET /boards/{id}/epochs`.
`DELETE /boards/{id}` is the one operation here that really destroys marks.

**A blank canvas cannot be cleared** (`409` — as a closed board is, and as a
**locked** one is, while a participant who is not the creator gets the `403`
the two doors above give them). The marker is a real stored row and it is
charged to the board's
lifetime budget, so a clear has to close at least one mark to be worth one —
without that rule every press of a button the creator can hold down minted a
free row, and the epoch index filled with zero-stroke sessions.

**A locked board cannot be cleared.** The lock is the creator's pause on the
canvas and it holds against every write to it, including their own
`POST /boards/{id}/clear` (and the socket's `clear` command,
`error{code:"locked"}`). A locked board that is also at `max_epoch_strokes` is
therefore recovered by unlock → clear → relock; that cost is deliberate, so
that "locked" means one thing everywhere rather than "locked, except for one
caller".

**Three caps, and only one of them is terminal** (all three published at `GET
/limits` under `board`):

- `max_epoch_strokes` bounds the **live** canvas — `epoch_stroke_count`, which
  every clear resets. Hitting it is **recoverable**: the creator clears, the
  history is kept, drawing resumes.
- `max_board_strokes` bounds the board's **lifetime** storage —
  `total_stroke_count`, which never resets, because a clear keeps its history.
  It counts **rows on the board**, not strokes drawn: a `clear` marker is a
  stored row, so it costs one unit of the budget like any mark. The marker's
  own unit is the one thing not refused at the ceiling — the last stroke of a
  board's budget may well be closed by a clear, and refusing that would drop
  the very marker the history needs to index the final epoch — so a board holds
  at most `max_board_strokes + 1` rows. That held only for boards born after
  the marker started paying, so **boot recomputes every board's
  `total_stroke_count` from its actual rows**: a board an older binary cleared
  is short exactly one per past clear, and an under-counting board is one that
  keeps taking marks past the storage ceiling. This is the one backfill that is
  not `= NONE`-guarded, and the one counter that may be recomputed — it is not
  an opinion the live system maintains, it is the board's row count, so
  recomputing converges instead of overwriting and the next boot writes nothing.
  Hitting it stamps `closed_at` and the board becomes **permanently
  read-only**: still fully readable and replayable, never deleted, and there is
  no reopen — open a new board. `POST /boards/{id}/close` is the manual form of
  the same thing, and idempotent.
- `max_boards_per_creator` bounds how many boards one creator holds
  (`board_count` on the user row). Deleting a board frees a seat; closing one
  does not — a closed board is still stored.

**Two ways to read the marks, and the difference matters most to a client
author.** A socket `join` replays the **current epoch only**; history never
rides the socket. History is an explicit paged REST read:
`GET /boards/{id}/history` (the board's whole life, oldest first, `clear`
markers included, `?epoch=` for a single one) and `GET /boards/{id}/epochs`
(the marker index — enough to offer "replay session 3" without scanning the
log). `GET /boards/{id}/strokes` is the current epoch over REST, the catch-up
and fallback path for the same canvas the socket replays. For a live canvas the
socket is the authority.

**The live canvas never shows a marker.** `GET /boards/{id}/strokes` serves
drawn marks only. The current epoch holds no `clear` marker by construction — a
marker records the epoch it *closed* — but a clear committing between the board
read and the row read would otherwise put one in the page, and the board room's
socket, which replays strokes only, would never show it. The markers are not
lost: `GET /boards/{id}/history` and `GET /boards/{id}/epochs` are where they
live.

**Listing.** `GET /boards?open=true` is every board of the caller's that has
not been closed; `?open=false` is the closed ones; omit it for both. The filter
reaches the database, so `total` is the filtered count and `?open=true&limit=1`
is a cheap "do I have a live board" probe. It reads `closed_at` and only that —
a locked board is still an open board, and so is one whose lifetime stroke
budget is spent but which was never drawn on again, because nothing has stamped
it. A closed board is never deleted, so with `max_boards_per_creator` at 200
this is the only way a heavy creator keeps the list readable.

**The board room (WebSocket)** — `GET /boards/{id}/ws`, cookie-authed like
everything else and, like the exam room, outside the OpenAPI spec (an upgrade
is not describable there); REST remains the full fallback. The gate before the
upgrade is the same `404` door: not a participant, no room. Then JSON text
frames:

| direction | frame |
|-----------|-------|
| client →  | `{"type":"join", "after":"<last stroke id>", "epoch":3}` — both optional; replay the current epoch |
| server →  | `{"type":"state", board, epoch, locked, closed_at, creator, participants, now}` on connect and every `ws_tick_secs` |
| server →  | `{"type":"strokes", epoch, strokes:[{id, author, payload}]}` — one replay chunk, repeated until the epoch is served |
| server →  | `{"type":"synced", epoch, cursor}` — the replay is complete; `cursor` is what to resume from |
| client →  | `{"type":"stroke", "payload":"…"}`, optionally `+ "client_seq":7` — any participant |
| server →  | `{"type":"saved", id, client_seq?}` — your stroke landed |
| server →  | `{"type":"stroke", id, author, payload, epoch}` — someone *else* drew |
| client →  | `{"type":"clear"}` / `{"type":"lock", "locked":true}` — **creator only** |
| server →  | `{"type":"cleared", …}` / `{"type":"locked", locked, by}` — the canvas was emptied / drawing was paused |
| server →  | `{"type":"closed", …}` / `{"type":"deleted"}` — the board is finished; both end the room |
| server →  | `{"type":"participants", creator, participants}` — the roster changed; a socket no longer on it is dropped |
| client →  | `{"type":"ping"}` → server `{"type":"pong"}` |
| server →  | `{"type":"error", code, message, client_seq?}` |

A `stroke` may carry a `client_seq` exactly as an exam-room `answer` does: any
value the client picks, echoed **verbatim** on that message's `saved` or
`error` and on nothing else, omitted entirely when the request omitted it. The
server never reads it.

`error` codes are a closed set: `epoch_full` (the live canvas is full — clear
it and keep drawing), `canvas_blank` (there was nothing on the canvas to clear
— the board is still live and still drawable), `board_closed` (permanently
read-only), `locked`, `forbidden` (a creator-only command from a participant),
`resync` (below), `invalid` (bad JSON, an unknown frame, an over-long payload),
`not_found`, `unauthorized`, `too_many_requests`, `conflict`, `internal`.

The two that look alike are the two that must not be confused: `board_closed`
is **terminal** and `canvas_blank` is **nothing to do**. A client that treats
them alike sends a live room off clearing a canvas it never needed to lose —
which is exactly what a code guessed from a *substring* of the refusal did
("the canvas is already blank" contains the word "closed"), so the server now
matches the refusal constants by value and a test pairs every one of them with
the code its client must act on.

**Persist, then publish.** A stroke is fanned out only after its row has
landed, so the channel can never carry a mark the database refused (a locked
board, a full epoch, a closed board). The database is the canvas; the channel
is a notification *about* it — which is why a client dedupes by stroke `id`,
why a resync deliberately re-serves marks it already drew, and why a frame is
never the authority: every stroke, clear and lock re-reads the board and
re-checks the roster, so a socket that missed a `participants` or `locked`
frame is refused all the same. One consequence of the same read-then-write
shape: a stroke carries the epoch the server read for it, so a clear landing in
that instant can file one stroke under the epoch it just closed — the stroke is
kept and stays replayable in `/history`, it simply is not on the new canvas.

**Reconnecting.** `join` carries a cursor: `after`, the last stroke id already
drawn, plus the `epoch` it belongs to. Same epoch and only what is newer is
replayed; an older epoch — or no cursor at all — gets a `cleared` frame first
and then the whole current epoch, because a cursor into a closed epoch no
longer means anything. If the room falls behind a slow socket the server sends
`error{code:"resync"}` and immediately replays the current epoch **out of the
database**: the client does not re-join, it throws its canvas away and takes
what follows. The database is always the source of truth, never the channel.

```js
const ws = new WebSocket(`${BASE.replace("http", "ws")}/boards/${id}/ws`);
let epoch = null, cursor = null, seq = 0;
ws.onopen = () => ws.send(JSON.stringify({ type: "join", after: cursor, epoch }));
ws.onmessage = (e) => {
  const m = JSON.parse(e.data);
  if (m.type === "cleared") wipeCanvas();
  if (m.type === "strokes") m.strokes.forEach(draw);      // dedupe by m.id
  if (m.type === "synced") ({ epoch, cursor } = m);
  if (m.type === "stroke") draw(m);
  if (m.type === "saved") settle(m.client_seq, m.id);
  if (m.type === "error" && m.code === "resync") wipeCanvas();
};
const send = (payload) =>
  ws.send(JSON.stringify({ type: "stroke", payload, client_seq: ++seq }));
```

## Concurrency model

The backend runs as **one process against one database**, and that buys less
than it sounds like: every request is an async task, dozens are in flight at
once, and each of them awaits the database in the middle of its work. So
"read, decide, write" is never safe on its own — SurrealDB does not
conflict-check a cross-record `count()` against a concurrent insert
(write-skew), and that fires between two tasks in one process exactly as it
would between two machines. The mutex-only design this replaced was already
losing races at one process. Every invariant is therefore guarded where the
database itself decides the winner. Three tiers:

1. **Single-row conditional writes** — compare-and-set (`save_if_unchanged`),
   `UPDATE … WHERE`, stored counters (`domain::cap`). A single-record write is
   atomic, so of N concurrent tasks exactly the allowed number get a non-empty
   result: the database decides the winner, the loser retries
   (`CAS_UPDATE_RETRIES`) or gets a 409.
2. **In-transaction `IF … THROW` gates** — the check runs inside the same
   statement as the write it authorizes, so it is atomic with it. Used where
   the rule reads the row being written (state machines, delete guards).
   "Is anything still attached?" is answered the same way, by a counter on the
   row being deleted rather than a `SELECT` over the children: a course is
   deletable while its `enrollment_count` is zero, a term while its
   `course_count` is (courses claim that reference *before* they write the
   link, and give it back when the link moves or the course is deleted). A fee
   plan reads the same way — editable *and* deletable while its
   `assignment_count` is zero, and frozen for good once it is not, since an
   assignment is never taken back.
   Where the child also carries a deterministic id — one enrollment per
   (course, user), one registration per (event, user), one fee-plan assignment
   per (plan, student) — the seat and the row
   are claimed in one transaction (`cap::claim_and_create`), so a duplicate
   `CREATE` rolls its own seat back instead of costing a stranger their place.
3. **Two accepted races**, reviewed and deliberately left open:
   - *Attempt-seq late save* — an exam-room socket writes into the sitting it
     joined with, a choice made before any lock is taken, so a save racing a
     retake can stamp an answer onto the just-terminal previous sitting.
     Damage: one history row; the grade of record (latest `seq`) is never
     touched.
   - *Approved-overlap* — two approvals landing in the same instant can
     double-book a teacher. Damage: one overlapping half-hour, visible to both
     parties, fixable by cancelling either side.

In-process locks remain, and they are a *second* line, never the guarantee:
`PRESENCE_LOCK` guards in-process socket state (there is no row to conditional
-write), `CLAIM_LOCK` keeps one counter writer at a time (the `WHERE` clause is
the cap — the lock only tames the retry loop, and keeps the tests' in-memory
engine deterministic), `APPOINTMENT_LOCK` collapses the common overlap case in
front of a rule no single-record write can express. Removing one of them costs
throughput or an accepted race; removing the conditional write behind it costs
the invariant.

Boot is unconditional: the schema batches, the backfills and the admin seed all
run on every start, because exactly one process ever starts (the one exception
is the marked one-time repair below). Deployment is
stop-the-world — `podman compose down` then `up`, never overlapping — and a
release that adds or renames a stored counter *requires* it. An old binary
writes rows without touching the new counter, the `= NONE` backfill guard
(correctly) refuses to re-seed, and the resulting permanent under-count lets a
guard approve exactly what it exists to refuse. The one exception is a counter
that is not an opinion but a plain row count — a board's `total_stroke_count`,
recomputed from its strokes on every boot rather than seeded once, which is why
that repair heals an under-count the `= NONE` guard would have skipped.
In-flight work does not survive
a restart either: a chatbot turn mid-inference settles `failed`/`interrupted`.

The one thing boot does *not* redo is tracked in `migration_mark` — one row per
backfill that is genuinely one-time, keyed by the backfill's own name. Most
backfills converge to a `WHERE` that matches nothing and are free to re-run, so
they carry no mark; the board-roster repair is different because its cost is
the **scan** (a `user` pass per board) rather than the write, so it is gated on
`migration_mark:board_roster` and skipped entirely once done. A volume that
never ran the repair holds no mark and still gets repaired on its next boot.

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
  migration_sql.rs the three boot batches as SurrealQL text (PRE_REPAIR,
                   MIGRATION, BACKFILL) + MIGRATION_BATCHES, the only list of them
  rate_limit.rs    fixed-window limiter: per-IP tiers + middleware, per-user chat tier
  state.rs         AppState { db, files_path, cookie_secure, rate_limit,
                   chatbot_limit, exam_presence, board_hub, db_up, ai }
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
    key.rs         sitting(): the deterministic per-sitting record key shared by
                   attempts, answers, answer images and results (seq 1 stays bare)
    cap.rs         claim()/release(): the count caps the database enforces — an
                   atomic `UPDATE parent SET n += 1 WHERE n < cap` on a counter
                   column of the parent row, replacing count-then-write mutexes
                   that could not see a concurrent insert;
                   claim_and_create() commits that seat and the child row in
                   one transaction, for children with a deterministic id
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
    enrollment.rs  EnrollmentId · Enrollment (one row per course+user; `source`
                   names the class that pumped it, absent when hand-placed)
    class_group.rs ClassGroupId · ClassName · ClassGrade · ClassGroup (a class section;
                   deletable only with no members and no attached courses)
    class_member.rs ClassMember (one student in a class; adding them enrolls
                   them into every course the class holds)
    class_course.rs ClassCourse (one course on a class; attaching it enrolls
                   the class's whole roster)
    class_blueprint.rs ClassBlueprintId · ClassBlueprint (a grade's course list;
                   applying it stocks every section at that grade, best-effort)
    class_pump.rs  attach/detach (the shared transaction behind both of those:
                   link row, class counter and the enrollments it implies move
                   together or not at all)
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
    badge.rs       BadgeStat · BadgeStats · earned · BadgeAward (auto-earned,
                   permanent badges off the lifetime counters on the user row)
    preferences.rs Theme · Language · PaletteColor (own UI preferences)
    message.rs     MessageId · MessageSubject · MessageBody · MessageLabel ·
                   Message (per-copy folders: inbox/sent/archive/trash)
    parent_link.rs ParentLinkId · ParentLink (parent↔student tie = the parent's read grant)
    registration.rs RegistrationId · Registration (a seat on a registration event's signup list)
    subject.rs     SubjectId · SubjectName · SubjectDescription · Subject (course curriculum)
    term.rs        TermId · TermName · Term (school term window)
    menu.rs        MenuId · MenuDate · MenuSlot · Menu (one published menu per
                   calendar day + meal slot; slot snapshotted as text)
    menu_dish.rs   MenuDishId · DishName · DishDescription · DishPrice ·
                   DishTags · MenuDish (a dish on a menu; price in minor units)
    dietary_profile.rs DietaryProfileId · DietaryTags · DietaryNote ·
                   DietaryProfile (what one student may not eat; keyed by the
                   student, tagged from the same `dietary_tags` list a dish is)
    meal_booking.rs MealBookingId · MealBookingStatus · MealBooking (one seat
                   per (menu, student); a cancel flips the status, never deletes)
    meal_attendance.rs MealAttendanceId · MealAttendanceStatus · MealAttendance
                   (who actually ate; one row per (menu, student), reporting
                   only — it never touches the ledger)
    meal_ledger.rs MealLedgerId · MealLedgerKind · LedgerAmount · LedgerMethod ·
                   LedgerNote · MealLedger (append-only money; the balance is
                   always the fold, never a stored field)
    fee_plan.rs    FeePlanId · FeePlanName · Installment · FeePlan (a school fee
                   plan; installments embedded on the row, past due dates legal)
    fee_plan_assignment.rs FeePlanAssignmentId · FeePlanAssignment (one plan on
                   one student, keyed `<plan>_<student>`; assigning appends
                   every installment charge, a replay appends nothing)
    payment_ledger.rs PaymentLedgerId · PaymentLedgerKind · PaymentLedger
                   (append-only school fees: charge/credit/reversal/refund, a
                   credit names its charge and a refund its credit; the balance
                   is always the fold; over-payment cap is advisory)
    settings.rs    ExamKindDef · GradeBand · Settings (per-school policy)
    chatbot_thread.rs ChatbotThreadId · ChatbotThreadTitle · ChatbotThread (one
                   chatbot thread, private to its owner)
    chatbot_message.rs ChatbotMessageId · ChatContent · MessageRole · MessageStatus ·
                   ChatbotMessage (one turn; the assistant row is written
                   `pending` before the AI call and settled after)
    board.rs       BoardId · BoardTitle · Board (a collaborative whiteboard:
                   creator + ad-hoc participant list, `epoch`, the two stroke
                   counters and `closed_at`)
    board_stroke.rs BoardStrokeId · BoardStroke (the
                   append-only stroke log; a `clear` row is the marker that
                   closed an epoch, carrying its final count — nothing is
                   ever deleted)
    appointment_slot.rs AppointmentSlotId · SlotSeries · SlotNote ·
                   AppointmentSlot (a teacher's published availability; a
                   recurring publish is expanded into rows sharing a series id)
    appointment.rs AppointmentId · AppointmentStatus · AppointmentReason ·
                   Appointment (a booking on a slot; occupancy is the slot's
                   stored `occupied` cap-1 counter, overlap is derived under
                   APPOINTMENT_LOCK)
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
    board_ws.rs    the collaborative board room WebSocket (join replay, strokes
                   fanned out to every participant, clear/lock)
    room.rs        the plumbing both rooms share: sending a frame, classifying
                   an incoming message, an AppError as a readable code, and a
                   client_seq echoed untouched
    page.rs        PageParams · Page<T> (shared pagination)
    etag.rs        conditional-GET middleware: ETag over a 200 JSON body,
                   If-None-Match → 304 (GET only; SSE and blobs pass through)
    auth.rs  users.rs  notes.rs  messages.rs  events.rs  appointments.rs
    courses.rs  subjects.rs  sessions.rs  exams.rs  homework.rs  questions.rs
    bank_questions.rs  marks.rs  work.rs  pomodoro.rs  attendance.rs
    settings.rs  terms.rs  meals.rs  payments.rs  ai.rs  chatbot.rs
    boards.rs  classes.rs
    limits.rs      GET /limits: every constant.rs bound served as JSON
```

Tests: `cargo test` — unit (in-source), integration (`tower::oneshot` + in-memory
db), rate-limit (both tiers, proxy-header and peer-address keying, shipped
limits over every route, two limiters sharing one budget over one db), e2e
(real TCP + reqwest cookie jar), persistence
(tempfile file engine, including close + reopen), ai-bridge (real QUIC on
loopback against a fake AI service), ai-protocol (the `hab/1` wire contract,
driven by a client that shares no code with the backend).
