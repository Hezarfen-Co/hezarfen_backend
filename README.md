# hezarfen_backend

Note, attendance, course + weighted exam mark backend. **Rust (edition 2024) · axum · PostgreSQL (sqlx) · tokio.**

Session-cookie auth with five hierarchical roles (`parent < student < teacher
< manager < admin`). A **`parent`** observes and changes nothing: admins tie
students to a parent account, and the parent reads those students' mark,
attendance, pomodoro, and homework reports — that's the whole role, bar the
two things every account keeps whatever its role: **messages** and its own
**notes**. Notes are per-user and carry **file attachments** (PDFs, documents,
…): blobs live on disk next to the database, metadata in the database, and the
per-file size cap is school policy in settings (`max_file_bytes`, default
5 MiB). Any two users can **message** each other, mail-style — subject +
body into the recipient's inbox, each side filing its own copy through
archive/trash with a read flag the sender sees as a receipt (the only place a
`parent` writes). Attendance is event + attendees: create an event with an **audience**
(the whole school, one role, a course **instance's** enrollment, a **class
section's** roster, or a **registration** signup list — omit for school-wide), then teachers mark the expected attendees
present / absent / late / excused (students never self-mark), and a **roster
report** shows who was expected and who missed. Registration lists fill seat
by seat: teachers register students (never the other way round), staff
register only themselves, an optional `capacity` caps the seats, and the list
closes the moment the event starts (or, for an event with only an end time —
a pure signup deadline — the moment that end passes). Every event stays visible to everyone —
the audience is a roster, not a wall. Marks are **instance-shaped**. A **course** in `/courses` is a
school-wide catalog row — kind **`course`** (a
regular taught course), **`study`** (a supervised study session — *etüt*), or
**`club`** (a student club — *kulüp*; same behavior, different label) — and it
owns its **subjects** (curriculum topics —
every exam question must be tagged with one of its course's subjects, so
results can later be read per topic). Nobody is enrolled in a catalog course:
teaching happens in the **instance** — one class section × one course, created
when the course is attached to the section (`POST /classes/{id}/instances`) and
living at `/instances/{id}` with its own `ders_saati` (weekly lesson hours, its
weight in the karne average), `counts_toward_karne` flag, teachers, roster,
exams, lesson sessions and homework. Two sections that attach Matematik get
**two** instances and share nothing: adding a student to 5-A enrolls them in
5-A's Matematik only, and an exam written on 5-A's Matematik is invisible to
5-B — unless the exam is **announced** to 5-B as an *ortak sınav*
(`POST /exams/{id}/audience`): one sitting, one mark, standing in every
addressed section's exam list, marks report and karne (see "Exam modes…").
The catalog row is office-owned (creator or manager+; clubs and etüt are
the exception — see "Courses (catalog)"), while an instance is run by the
teachers a **manager assigns** to it (`POST /instances/{id}/teachers`) plus the
section's homeroom teacher: an assigned teacher manages everything inside the
instance — exams, sessions, subjects, roster, grading — and a demotion below
`teacher` sweeps their assignments away. Students are
grouped into a **class section** (*şube* — 9-A, 10-B) when a school teaches
that way; the şube belongs to an **academic year** (`/academic-years`), and a
term is a grading slice inside that year. Membership is a live stint: leaving
(`DELETE /classes/{id}/members/{user}`) stamps `left_at` and gives the seat
back, and re-adding inserts a fresh row — the section's history is never
erased. Attaching a course to a section enrolls the whole roster, adding a
member enrolls them into every instance already attached, and what lands are
ordinary enrollment rows tagged with the section that pumped them (untagged =
placed by hand; a şube sweep never adopts or removes those). A section may also
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
per-instance weighted average and
an overall average from their mark report — each exam weighted by its **kind**
(a yazılı can count double, a sözlü once: weights are set per kind in settings,
not per exam) — and `GET /marks/karne` rolls the whole year up: per-instance
averages mapped to the school's grade bands, a year-to-date average weighting
each instance by its `ders_saati`, and a `gecti`/`kaldi` verdict read off the
band labelled `"2"`. Once a term is archived the karne serves the **frozen
snapshot** taken at archive time, so a past report never changes under a staff
edit. Exams run **sync** (one
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
even see it, and the named see only themselves in it), tagged with a course
subject, due by a required future `due_at`
— students hand in text and/or **files of any type** (same size cap, 10 per
submission, served back only as forced downloads), editable until the teacher
grades a status (`done`/`incomplete`/`missing`) with an optional 0–100 mark
(grading freezes the hand-in until the grade is removed); lateness is computed
from two stamps (first hand-in vs last touch), never stored, and homework
marks stay out of the weighted `/marks` averages. Courses also carry **lesson
sessions** with teacher-taken roll call (students never self-mark a lesson),
staff clock in/out on a server-stamped **work log**, students track study time
with a server-stamped **pomodoro log** (the timer runs in the frontend; the
backend records the focus stints — every one of them, though only stints of at
least 5 minutes and at most 16 a UTC day *count* towards the badges — and
teachers can read any student's log),
and every user has an
**attendance report** (event + per-instance lesson tallies with rates, plus
the per-dönem devamsızlık day counts).
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
linked students' only. The two blocks inside it are cut to what the *reader*
already reaches elsewhere: courses to the ones they would pass
`GET /courses/{id}` on, classes to nothing at all unless they are teacher+, a
linked parent, or the owner — the bar `GET /classes/user/{id}` holds (see
"User profiles & avatars").
The **AI features live in separate projects**, so the backend also opens a
QUIC **AI bridge** (`AI_QUIC_ADDR`, off by default): AI services dial in,
register the capabilities they serve, and each request rides its own QUIC
stream on that one connection — no correlation ids, no head-of-line blocking.
A service reads school data back over that same connection: it opens a stream
of its own for a `GET` against a deny-by-default allowlist of read endpoints,
optionally *on behalf of* a named user, so an AI feature needs no HTTP session
and no password of its own.
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
deleted: a mistake is corrected by appending the opposing line. Both bulk
operations are bounded: one assignment request may raise at most 3 000 charges
(`400` past that, nothing written), and one ledger line may carry at most 20
lines applied to it (`409` past that). **There is no
online payment integration and none is planned** — no gateway, no card data,
`method` is free text. Teachers see no money at all (see "Payments").

Every field is a validated newtype (`Username(String)`, `NoteTitle(String)`, …)
constructed only after its restrictions pass — invalid input can't be
represented. Those restrictions are published rather than left to be guessed:
**`GET /limits`** (no auth) serves every fixed bound and closed value set the
API enforces, read straight from the constants the newtypes use, so a frontend
validates against the server's own rules instead of a hand-kept copy (see
"Validation limits"). Persistence is PostgreSQL through sqlx: every statement
is checked at compile time against the schema (see `## Run`), and a row is a
plain typed struct in `src/db/`. Ids are UUIDv7s (time-sortable), served in
the hyphenated wire form.

## Run

```sh
podman compose up -d postgres   # PostgreSQL 18 on 127.0.0.1:5432 (hezarfen/hezarfen)
cp .env.example .env      # recommended: DATABASE_URL feeds the build too
cargo run
```

Boots on `http://127.0.0.1:7656`, talking to PostgreSQL at `DATABASE_URL`
(default `postgres://hezarfen:hezarfen@127.0.0.1:5432/hezarfen_control` —
the control database) and storing uploaded note files in `./data/files/`
(`FILES_PATH`, created at startup). `DATABASE_URL` is also read by sqlx's
compile-time query macros, so `cargo check` prepares every query against a
live database; `scripts/prepare_db.sh` refreshes the one union-schema
prepare database those checks run against, and the committed `.sqlx` cache
lets a build run with `SQLX_OFFLINE=true` and no database at all. On boot
the sqlx migrator applies `migrations/control` to the control database and
ensures the school template; each school database runs `migrations/school`
the same way at mint. A school is registered as `provisioning` until that
migration has run, so a boot cut short mid-create leaves a row the next boot
finishes rather than a school that answers every request with a `500`.
Interactive API docs (Swagger UI) are served at
`/swagger`, the raw OpenAPI spec at `/api-docs/openapi.json`.

## Run in a container (podman)

```sh
podman compose up -d --build   # build + start, http://127.0.0.1:7656
podman compose logs -f backend
podman compose down            # stop (data survives in the volume)
```

The `Containerfile` is a two-stage build (Rust builder with cargo cache
mounts, `debian:trixie-slim` runtime, non-root user, `SQLX_OFFLINE=true`
so the build never touches a database — the macros read the committed
`.sqlx` cache). Two primary services: `postgres` (the official
`postgres:18-alpine` image, `max_connections=500` so the test suite's
parallel control+school pools fit) and the backend, which waits for the
database's healthcheck and connects over `DATABASE_URL`
(`postgres://hezarfen:hezarfen@postgres:5432/hezarfen_control`). The backend
port is published on loopback only — `127.0.0.1:7656:7656` — so a server
deployment sits behind a reverse proxy and the API is never directly
reachable (`HOST` is forced to `0.0.0.0` inside the container so the publish
works). Each service has its own named volume: `pgdata` holds the database
— it is mounted at `/var/lib/postgresql`, which the postgres:18 image keeps
its cluster under (`/var/lib/postgresql/18/docker`), so the data really
lands in the volume — and `hezarfen_backend_data` holds the
uploaded note files (`/data/files`). Production knobs (`COOKIE_SECURE`,
`CORS_ALLOWED_ORIGINS`, rate limits, `TRUST_PROXY`) reach the backend through
the env file (below), not through `compose.yaml`. Leaving
`CORS_ALLOWED_ORIGINS` unset means dev mirror mode without credentials; a
cookie-using browser frontend must be allowlisted explicitly. Works with
`docker compose` too.

**Credentials live in three places and are never mixed.** CI's postgres
sidecar uses a throwaway `hezarfen`/`hezarfen` pair declared in the workflow
— no GitHub secrets involved. Local compose interpolates dev defaults
(`hezarfen`/`hezarfen` for postgres, `builder`/`builder123` for the builder
account, `admin@hezarfen.local` / `Hezarfen_dev1!` for OpenObserve), so the
`up` above works with zero extra files. Production is the operator's job:
ssh to the VPS, copy `deploy/hezarfen_backend.env.example` to
`$HOME/hezarfen_backend/hezarfen_backend.env`, `chmod 0600` it, and set the
real passwords — the deploy pipeline never creates, overwrites or uploads
that file, it only refuses to run unless the file exists at mode `0600` with
non-empty `POSTGRES_PASSWORD=`, `BUILDER_PASSWORD=`, `ZO_ROOT_USER_EMAIL=` and
`ZO_ROOT_USER_PASSWORD=` values (prod never silently falls back to the dev defaults). The first `podman compose up` bakes
`POSTGRES_PASSWORD` into the `pgdata` volume: changing the env file
afterwards does not re-key the database, so the real password must be in
place before that first up, and it must be URL-safe (no `@`, `:`, `/` — it
is interpolated into `DATABASE_URL`). The same file is the backend's
optional `env_file`, so extra keys it carries (`COOKIE_SECURE`,
`TRUST_PROXY`, `CORS_*`, `RATE_LIMIT_*`, `OTEL_*`) reach the container
without being listed in `environment:`.

One more service is the telemetry sink: `openobserve`, a single Rust binary
that stores logs, metrics and traces itself — no sidecar databases and no
container socket. Its UI, search API and OTLP/HTTP ingest share
`127.0.0.1:5080`; OTLP/gRPC listens on `127.0.0.1:5081`. Log in at
http://127.0.0.1:5080 as the `ZO_ROOT_USER_EMAIL`/`ZO_ROOT_USER_PASSWORD` pair
set on the service (`admin@hezarfen.local` / `Hezarfen_dev1!` for local dev —
OpenObserve rejects a weaker password at boot). Retention is per stream in
days, defaulted globally to 90 by `ZO_COMPACT_DATA_RETENTION_DAYS`. Traces,
metrics and logs start landing when the backend's environment carries
`OTEL_EXPORTER_OTLP_ENDPOINT` (`http://openobserve:5081`) — auth is minted
from the same `ZO_ROOT_USER_EMAIL` / `ZO_ROOT_USER_PASSWORD` the OpenObserve
container already has, so there is no hand-rolled base64 header. Set
`OTEL_EXPORTER_OTLP_HEADERS` only for a different collector. Those keys belong
in the env file (production: `hezarfen_backend.env`; locally: an optional file
of the same name beside `compose.yaml`), never in `compose.yaml` itself, where
an empty OTEL value would switch export on and fail the boot. The OTLP/HTTP
variant (`http://openobserve:5080/api/default` with
`OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`) is the alternative.

Running a server means running its backups too. `./backup-postgres.sh` —
shipped with the deploy artifact and living next to `compose.yaml` on the VPS
— streams `pg_dumpall` out of the postgres container into
`backups/hezarfen-<UTC>.sql.gz`, mode `0600`. `pg_dumpall` rather than
`pg_dump`, because every school is its own database and one dump has to cover
the control database and all of them. Restore with:

```sh
gunzip -c backups/FILE.sql.gz | podman exec -i hezarfen_backend_postgres psql -U hezarfen -d postgres
```

Deploying is GitHub Actions' job, not the server's: the workflow packs the
already-built binary and the migrations into a runtime image
(`deploy/Containerfile.runtime`), ships the image tarball together with
`compose.yaml`, the `hezarfen_backend_compose.service` unit and the backup script as
an artifact, and the deploy job loads it on the VPS and starts the stack with
`podman compose up -d --no-build` — the image tag travels in a deploy-owned
`stack.env`, so a release never rewrites the operator's secrets file. The
operator's only manual step is writing the env file once (above). Reboots
heal themselves: the user session lingers (`loginctl enable-linger`) and the
`hezarfen_backend_compose.service` user unit runs compose again at boot.

The backend survives the database going away, at boot and at runtime.

At boot it retries the connection (a one-second cadence, indefinitely)
until the server answers, rather than exiting. Exiting looks tidier but is
worse: the container runtime restarts the process, it fails again in
milliseconds, and a few seconds of startup skew burns the whole restart
budget and leaves the backend down for good.

At runtime nothing queues behind a dead server: a request that cannot get
a connection is refused with `503`. The two flavors are distinguished
honestly — an acquire that times out means nothing executed, so the
answer is retry-safe; a request whose connection died mid-flight may have
applied server-side (the commit can race the connection dropping), so its
503 says the write may or may not have landed and must not be blindly
retried. When the database comes back the pool dials again on the next
request; there is nothing to re-arm.

Without compose:

```sh
podman build -t hezarfen_backend .
podman run -d --name hezarfen -p 7656:7656 -v hezarfen_backend_data:/data hezarfen_backend
```

## Multi-school (SaaS)

One deployment serves many schools. Every school gets a PostgreSQL
**database** of its own, named `{control}_school_{slug}` after the
**control** database `DATABASE_URL` points at (`hezarfen_control` by
default), which holds the school registry, the builder accounts and the
shared rate-limit window. Isolation is the store's, not the handlers': a
school database sees only its own rows, so no query carries a
`WHERE school = ...` somebody could forget.

**Slugs.** 2-32 characters of `a-z`, `0-9` and `-`, starting with a letter or
digit (`MIN_SLUG_LEN`/`MAX_SLUG_LEN`); `builder`, `control` and `person` are
reserved and refused. A slug names the school's database, its blob directory
and its cookie prefix, so it is immutable once taken — a rename changes the
display name only.

**Cookies.** A school session is `session=<slug>.<token>`, the vendor's is
`session=builder.<token>`, and a person who still has to pick a school carries
`session=person.<token>` — all split at the *first* dot so a token can never
be read as a slug. No cookie is accepted on another's surface (`401` all
ways), and a cookie with no dot names no school and is refused everywhere.

**How a request finds its school.** Every school-scoped handler takes the
tenancy `State` (`web::tenant_state`), which reads the cookie's prefix, looks
the school up in the registry, and hands the handler a connection pinned to
that database plus `FILES_PATH/<slug>/` as its blob directory. The AI bridge is
the one caller with no cookie: it injects the school as a request extension
instead, which nothing outside the process can forge. In-process state is keyed
by school too — the exam presence map and the whiteboard hub — so two schools
never share a room.

**Suspension is immediate and total** for a school's own users: the registry
row is read on every request, so the next call after the switch answers `403`,
live session or not, `POST /auth/login` included. The builder surface keeps
working on a suspended school — that is how it comes back — except
`POST /schools/{slug}/enter`, which is one of the school's own doors.

**The builder lifecycle.** `BUILDER_USERNAME` + `BUILDER_PASSWORD` seed the
operator account at boot (both or neither; half a pair aborts startup, and an
existing account is never rewritten). From there: `POST /builder/login` →
`POST /schools` (registry row, database, schema and the school's first admin —
one call or none of it) → that admin (a **person** in the control database)
logs in at `POST /auth/login` with just username + password, and is entered
into the school straight away → `PATCH /schools/{slug}` renames, suspends or
resumes →
`POST /schools/{slug}/admin-password` re-keys a locked-out admin and revokes
every session it held → `POST /schools/{slug}/enter` mints an ordinary school
session for one of its admins (support access, no builder power inside) →
`DELETE /schools/{slug}` destroys the school's database, its registry row and
its uploaded files. Irreversible on purpose: suspension is the reversible door.

**AI frames name the school.** One AI service serves the whole deployment, so
every `hab/2` frame carries a `school` field — see "AI bridge (QUIC)".

### Modules

A **module** is one router nest (`/meals`, `/exams`, `/boards`, …) sold as a
unit: the thing a school buys and the thing the router refuses are the same
thing, so there is no per-route entitlement list to keep in step with the
routes. A school's set is stored on its registry row, defaults to everything on
`POST /schools` (pass `modules` there to sell less), and is the vendor's to
change afterwards.

There are 21 modules, bundled into four packages. A package is only a name for
a set of modules — entitlement is always stored per module, so re-packaging
never migrates a school's row. `requires` is structural, never commercial:
every edge below is a stored `record<…>` reference into the other module's data
(or a report that reads it), which is why a set that breaks one is refused.

| Module | Package | Requires |
| --- | --- | --- |
| `attendance` | `academics` | `events`, `sessions` |
| `bank_questions` | `academics` | — |
| `classes` | `academics` | `courses` |
| `course_notes` | `academics` | `courses` |
| `courses` | `academics` | — |
| `exams` | `academics` | `courses`, `subjects` |
| `homework` | `academics` | `courses`, `subjects` |
| `marks` | `academics` | `exams` |
| `sessions` | `academics` | `courses` |
| `subjects` | `academics` | `courses` |
| `appointments` | `communication` | — |
| `boards` | `communication` | — |
| `events` | `communication` | — |
| `messages` | `communication` | — |
| `notes` | `communication` | — |
| `questions` | `communication` | — |
| `meals` | `operations` | — |
| `payments` | `operations` | — |
| `pomodoro` | `operations` | — |
| `work` | `operations` | — |
| `chatbot` | `ai` | — |

`GET /modules/catalog` publishes exactly this table (unauthenticated and
deploy-constant, like `GET /limits`), and `GET /modules` answers any signed-in
user with their own school's enabled set — so a client hides a nest the school
never bought instead of discovering it as a `403`. Both are ungated on purpose:
an entitlement lookup a disabled module could switch off would be unusable
exactly when it is needed.

**The dependency rule.** A module may not be on without what it requires, and
may not be taken back while something the school still has requires it. Both
refusals are `409` and both name *every* violation at once, so a caller fixing
a set does not discover the problems one round trip at a time:

```json
{"error": "conflict: exams requires courses, which is not enabled; exams requires subjects, which is not enabled"}
{"error": "conflict: courses is required by exams, subjects"}
```

**Selling.** `GET /schools/{slug}/modules` returns both halves (`enabled` +
`disabled`, each sorted — together they are the whole catalog).
`POST|DELETE /schools/{slug}/modules/{module}` moves one module and is
idempotent: a module the school already has (or already lacks) is a `200` with
the unchanged set. An unknown module name in the path is a `404`, the same
verdict an unknown school gets.

`PATCH /schools/{slug}/modules` re-sells the whole shelf in one call, with any
mix of the four optional lists `enable`, `disable`, `enable_packages`,
`disable_packages` (a package expands to its modules). The lists are folded
into **one** resulting set, which is validated **once** and written **once or
not at all** — so a `PATCH` enabling `exams` and `subjects` together succeeds
where two single calls would refuse the first, and a rejected request leaves the
row untouched. An empty body, or a set equal to the current one, is a no-op
`200`. An unknown module or package name is a `400`, and so is a name pulled
both ways at once — picking a side silently would sell (or unsell) a module the
caller also asked for the opposite of:

```json
{"error": "module: `kantin` is not a known module"}
{"error": "module: `meals` is asked for in both directions at once"}
```

**What a disabled module looks like from inside the school.** Every route in
its nest answers:

```json
{"error": "module disabled", "module": "meals"}
```

with `403`. The gate is a `route_layer`, so a path that does not exist inside a
disabled nest is still a `404` — a disabled module is a refusal on the routes
that exist, not a wall around a URL space. The four child routes under
`/courses/{id}` that belong elsewhere (`/exams`, `/sessions`, `/subjects`,
`/homework`) carry the child module's gate as well as the `courses` one, so
either being off refuses. Nothing is deleted: a disabled module's rows stay put
and come back untouched when it is re-enabled.

**Core is never gated.** Auth, users, settings, terms, `GET /limits`, the
module lookups themselves, AI discovery and the whole builder surface answer
whatever a school has bought — they are how a school logs in, is configured and
is fixed.

**A change takes effect on the school user's next request**, with the same
cookie and no re-login: the registry row is read on every request, exactly like
suspension.

**The AI bridge obeys the same entitlements.** An api-read frame is dispatched
into the router carrying the school's module set, so a read into a disabled
nest comes back as that same `403` body. Beyond that, `chatbot` — the `ai`
package's only module — gates every *outbound* dispatch, not just the
`/chatbot` nest: a school that did not buy `ai` sends no data to an AI service
at all. Course-note indexing (`rag.index`) is a silent no-op for it — the note
is stored, nothing is dispatched and no `rag_output` row appears — and the blob
stream, the one surface that bypasses the router, checks `course_notes` **and**
`chatbot` by hand and refuses with code `module_disabled` when either is off.

A walk-through — the vendor takes `meals` away and gives it back:

```bash
curl -c v.txt -X POST localhost:6060/builder/login -H 'content-type: application/json' -d '{"username":"builder","password":"correct horse battery"}'
curl -b v.txt -X DELETE localhost:6060/schools/demo/modules/meals      # 200, meals now in "disabled"
curl -b s.txt localhost:6060/meals/menus                               # 403 {"error":"module disabled","module":"meals"}
curl -b v.txt -X POST localhost:6060/schools/demo/modules/meals        # 200, and the student's next call works again
```

## Auth model

The account is a **person**: one global username + password, held in the
control database, that can belong to any number of schools. Login never names
a school — `POST /auth/login` takes `{username, password}` alone. A person
with exactly one active membership is logged straight into it: the response is
the full user object and the cookie is the school's own
`session=<school-slug>.<token>`. A person with two or more active memberships
answers `{username, schools: [{slug, name}]}` with a `session=person.<token>`
cookie instead, and names a school with `POST /auth/school`
(`{"school": "<slug>"}`), which swaps the cookie for that school's
`<slug>.<token>` and revokes the person session. Register still names the
school: `POST /auth/register` takes `{school, username, password}` (see
"Multi-school (SaaS)").

Login sets an `HttpOnly`, `SameSite=Lax` `session` cookie (7-day expiry,
stored server-side), split at the *first* dot so a token can never be read as
a slug. Send the cookie back on later requests. Every endpoint below whose
`Auth` column names a role requires a valid school session; the ones marked
`no` (`/health`, the docs pages, `register` / `login` / `school` / `logout`)
don't (`logout` is idempotent — it clears whichever session the cookie names:
school, person, or builder). `GET /auth/me` is school-cookie only: a person
cookie answers `401`, because who you *are* depends on the school you have not
picked yet. Set `COOKIE_SECURE=true` when serving behind TLS to add the
cookie's `Secure` attribute.

**Register never reveals anything.** `POST /auth/register` answers `201` in
every outcome with the *same* body — `{username, role}`, the echoed name and
the `student` role every fresh account gets. A username that is new creates
the person plus the school's `app_user`; a person who already exists is
attached to this school as one more membership — but only when the password
matches the person credential, and a wrong password joins nothing while the
reply stays the same. All outcomes return one value built before any branch,
and the password is hashed before any lookup, so they cost the same ~33ms
up front. This is deliberate: the route is unauthenticated, so a `409` (or a
faster reply) would let anyone enumerate accounts. The same holds on login:
an unknown username burns the same argon2 work against a decoy hash as a
wrong password, and a person with no memberships left to enter answers the
same `401`. Do not "fix" any of this back to a distinct error, and do not add
an `id` to the register reply.

The reply carries **no `id`**: on the taken path there is no row to name, and
a fabricated one would leave the client holding an id that matches nothing.
Log in and read `GET /auth/me` to learn who you are. The accepted cost: a
caller who collides with an existing person's name gets no distinct error and
cannot join another school with that name without the right password — they
pick another name.

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
  "pomodoro":      { "min_counted_ms": 300000, "max_counted_per_day": 16 },
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
                     "request_timeout_secs": 30, "max_request_id_len": 64 },
  "rate":          { "window_secs": 60, "auth_per_minute": 10,
                     "api_per_minute": 300, "chatbot_per_minute": 20 }
}
```

The `pomodoro` group is the one that bounds no input: `POST /pomodoro/finish`
never refuses a stint over it, it records the stint and answers
`counted: false`. It is published for the same reason as the rest — a client
that has to guess what a session was worth is a client with a hard-coded copy.

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
- `tests/spec_bounds.rs` builds the OpenAPI document, reads all 222 published
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
`student`; an admin may also mint one outright at `POST /users`, optionally
born with the role it is named (`student` by default, so a new teacher never
passes through a student state). A role read is re-checked on every request, so
a role change takes effect on the user's very next call (no re-login).

`parent` is the read-only observer at the bottom of the ladder: an admin ties
any number of students to a parent account (`POST /users/{id}/students`), and
the tie is the parent's whole power — they list their students
(`GET /users/me/students`) and read each one's mark, attendance, pomodoro,
and homework reports in full. Student-only checks are exact (`role == student`), so a
parent can never enroll, sit an exam, be graded, or land on a roll call; and
sitting below every staff bar, they can't touch anything else either — except
messages, which they send *upward* only — to a teacher, manager or admin,
never to a student or another parent (the same bar a student holds, issue
#23) — and their own **notes**, which carry no role bar at all: a note is
private to one person, so a role change must not confiscate it. It used to.
While the note routes required `student`, a demotion locked the owner out of
their own notes permanently — and since nothing else in the crate reads a note
and no route cascades one, the rows and their on-disk blobs were left
unreachable and undeletable by everybody, without even a per-user cap to bound
the wreckage. Ownership is now the whole check there. A role
change off either end of a tie (the parent stops being a `parent`, the
student stops being a `student`) drops the tie, exactly like promotion drops
course enrollments. That sweep runs once, so a tie written in the same instant
as the role change can outlive it; a surviving row grants nothing regardless —
every gate re-reads the student's **live** role, and the student list itself
skips anyone who no longer holds the `student` role. A demotion to `parent` also gives back the seats that
account holds on still-open **event signup lists** — a parent cannot reach
`DELETE /events/{id}/register/{user}` itself, so an unswept seat would stay
claimed and a capped event would answer "full" for good. Two things keep that
from happening: a registration written while the demotion runs claims the
holder's own row, so either the sweep sees the seat or the seat sees the
`parent` (a `403`); and a teacher+ may free a seat a `parent` holds, which is
the way back for any seat an older build stranded. Every other role change leaves signups alone: staff free their
own seats by hand. Seats on lists that have already closed (the event started,
or its `ends_at`-only deadline passed) are never touched — that roster is
history, and re-registering is refused.

A demotion to `parent` also **closes every whiteboard that account created**,
on top of taking it off the rosters it was invited to. The whiteboard is shut to
parents outright, so the creator is answered `404` on their own board; clear,
lock, close and delete are the creator's alone, so every other participant is
answered `403`; and no route lists a board the caller is not on, so not even an
admin can find its id. Left open it is a room nobody can end while its
participants keep drawing on it. Closed and not deleted, because the marks are
their work too: the board, its history and its epochs stay readable, only writes
are refused (`board_closed`), and the creator's board seat stays taken — a
closed board is still a stored board.

A demotion below `teacher` additionally **withdraws the published appointment
calendar** and cancels the live bookings on it. Nothing else could: a slot is
listed only on its own teacher's calendar and deleted only by a teacher+, so
after the demotion no route yields its id, and a booking on it can be decided
only by a teacher+ and cancelled only by its requester — who is refused once
the window opens, leaving the slot's seat held for good. The requester keeps
the booking, `cancelled`, with the ex-teacher on `cancelled_by` and the reason
on `cancel_reason`; its slot is gone, so it renders without a window. A slot
published while the demotion runs claims the publisher's own row, so either
the sweep sees the slot or the publish sees the new role (a `403`), and a
booking racing it collides on the slot row the sweep deletes.

The role write and every sweep it implies — class memberships and their
counters, all enrollments and their seats, parent ties on both sides, a
demoted parent's still-freeable event seats, whiteboard rosters and the boards
that account created, course staffing and homeroom-teacher columns, the
appointment calendar and the bookings on it — commit as **one transaction**. It
either all lands or none of it does: a failure answers `500` with the account
still holding its old role and every grant of it still standing, and the same
`PATCH` retried applies the lot. Signup lists that have already frozen are
still left exactly as they stand.

| Action                                   | Minimum role | Notes                                         |
|------------------------------------------|--------------|-----------------------------------------------|
| Register / login / view own account      | (any)        | Registration always creates a `student`       |
| View events                              | student      | Everyone from `student` up can read events    |
| CRUD own notes + their files             | (any)        | A note is private to its owner and has no other reader, so there is no role bar: ownership *is* the authorization, `parent` included. Note files (upload/download/delete) are walled per owner like the notes themselves — a file is only ever reached through its own note |
| Send / read / file / delete messages     | (any)        | One-to-one, any user to any user (`parent` included — the role's one write); each party only ever touches their own copy |
| Mark event attendance; remove attendance rows | teacher | Only users in the event's **audience** can be marked; students never mark — a teacher+ may mark anyone expected, themselves included |
| Create events                            | teacher      | The audience (school / role / course / class / registration) is set at creation and editable later |
| Register users onto a registration event | teacher      | Teachers place **students** (students never register themselves) and take a seat for **themselves** — never for another staff member. Unregistering mirrors the same rule, plus any seat a `parent` was left holding — that account can reach no route to free it itself |
| List an event's attendance or its roster report | teacher | Students read their own tallies via the attendance report |
| Edit / delete an event                   | teacher      | Only the **creator**, or a `manager`+ for any event — in both cases only while still `teacher`+ |
| View a course's sessions                 | student      | Only inside **visible** courses: enrolled, creator, assigned teacher, or `manager`+ |
| List a session's roll call               | teacher      | The **session's teacher** (while still `teacher`+), or anyone with course-management rights |
| Create / edit / delete a course session  | teacher      | Course-management rights (course creator, an assigned teacher, or `manager`+) |
| Take a session's roll call (mark/remove **enrolled students**) | teacher | The **session's teacher** (while still `teacher`+), or anyone with course-management rights; only students sit on a roster |
| Mark / remove the **session teacher's** presence row | manager | Staff presence is management's call — the teacher can't self-mark |
| Work check-in / check-out; view **own** work log | teacher | Instants are server-stamped, never client-supplied |
| View / correct / delete **any** staff work log entry | manager | Corrections only on closed entries |
| Start / finish a pomodoro focus session; view **own** pomodoro log | student | **Students only** start; instants server-stamped; starting discards a dangling unfinished session; a stint moves the badge counters only if it is long enough and within the day's quota (`counted`) |
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
| Read any user's **public profile** and avatar; upload/remove **own** avatar | student | A `parent` is the exception: own and linked students' only, the link and the target's live role re-read per call. Removing *another* user's avatar is admin-only moderation. The profile's `classes` block is empty unless the reader is teacher+, a parent linked to that student, or the owner |
| List **own** linked students             | parent       | Read-only: the list plus each student's mark/attendance/pomodoro/homework reports — a parent changes nothing, anywhere |
| Read the school settings and the term list | student | Clients need them to render kind/status pickers, grades, and the calendar |
| Edit school settings; create / edit / delete terms | manager | School policy (exam kinds, attendance statuses, grade bands, the note-file size limit) and the academic calendar are management's call |
| Create a user; list users; look up one user; change a user's role; edit **any** user's personal info or UI preferences; tie/untie students to a `parent` account | admin | An admin cannot change **their own** role, and nobody can demote the school's **last** admin (`409`) |

### Bootstrapping the first admin

There is no self-service path to `admin` — registration always creates a
`student`. A school's first admin is created *with* the school: the builder
posts `POST /schools` with `{slug, name, admin_username, admin_password}` and
that account lands inside the new school's database holding `admin`. The
builder itself comes from the startup seed: set both

```sh
BUILDER_USERNAME=builder
BUILDER_PASSWORD=builder123   # local-dev default used by compose.yaml; change it
```

and on boot the builder account is created in the control database **if the
username doesn't exist yet**. The seed is idempotent and deliberately
conservative: it never rewrites an existing account, so changing the password
here does not re-key a live builder. Setting only one of the two aborts
startup. `compose.yaml` ships with the credentials above for local dev. See
"Multi-school (SaaS)" for the rest of the lifecycle.

That admin can then promote everyone else through `PATCH /users/{id}/role`.
The same admin can also mint accounts directly with `POST /users`
(`{username, password}`, optionally `role` — `student` by default), which is
how staff are added without the register-then-promote two-step.

The admin set cannot be emptied through the API: an admin never changes their
own role, and a `PATCH /users/{id}/role` that would demote the last admin
answers `409` — including when two admins demote each other at the same
instant, since the guard is a predicate on the role write itself: the
`UPDATE` that lowers the row counts the remaining admins in the same
statement, so the count and the write it guards contend on the row lock
like any other write.

A locked-out school is the builder's `POST /schools/{slug}/admin-password`,
which re-keys an admin that still exists. Manual fallback (the recovery path
if an older build already emptied a school's admin set): run

```sql
UPDATE app_user SET role = 'admin' WHERE username = 'ada';
```

against the **school's** database (`{control}_school_{uuid hex}` — the hex of
the school's registry `id`, no dashes), not the control one. List the school
databases with `\l` — e.g. `podman exec -it hezarfen_backend_postgres psql -U hezarfen
-d hezarfen_control -c "SELECT slug, id FROM school"` names each school and
its database suffix for the compose stack.

## Endpoints

`Auth` is the minimum role; `no` means no session required, `student` means any
logged-in user. `builder` is not a school role: it means the deployment's
vendor account (see the `builder` tag), whose cookie is refused on every other
endpoint here exactly as a school's cookie is refused on its. The table lists
every route this deployment serves; a route in a module the school has not
bought answers `403 {"error": "module disabled", …}` instead — see "Modules".

A course is a regular taught course (kind `course`, the default), an *etüt* (kind
`study` — a supervised study session), or a *kulüp* (kind `club` — a student
club). The three kinds behave identically everywhere, and the kind is a label
the UI renders differently, settable at creation and editable later. A course
is a **catalog row**: it owns its **subjects** (curriculum topics) and carries
counters — `class_course_count`, `course_membership_count` — never a roster of
its own. There is no `capacity` anywhere: no route refuses an enroll for room,
and a counter is a count, not a cap. Teaching happens in the **instance**
(`/instances`): one class × course row, minted by attaching the course to a
section, carrying its own teachers, hours, roster, exams, sessions and homework.

A catalog course is **office-owned**: only its **creator** (or a manager+) may
edit or delete it. The two kinds a section does not teach are the exception —
a *kulüp* or an *etüt* is joined directly, with `POST /courses/{id}/members`
(the school-scoped membership a student may join and leave on their own). A
`course`-kind row refuses that join (`409`): a ders is taken through a şube's
instance, never directly.

**Who runs an instance.** Its **teachers** — each a `teacher`+ account a
manager assigned with `POST /instances/{id}/teachers` (`{user_id}`, idempotent;
drop them with `DELETE /instances/{id}/teachers/{user}`) — plus the **homeroom
teacher of its class**. Instance teachers manage everything inside it: its
`ders_saati` and karne weight, its roster, its exams, sessions and homework,
grading and roll call. Staffing is deliberately the office's call; a manager+
caller may always do the same, and nobody else may touch the teacher list.
Every instance response carries its `teachers` array. A user demoted below
`teacher` is swept off every instance they were assigned to (the mirror of
promotion dropping enrollments) and off every homeroom they held. That sweep
runs once, so an assignment landing in the same instant would survive it: the
assign call therefore re-reads the account's live role **after** its write and
answers `409` — dropping the assignment again — if it has since fallen below
`teacher`. Same guard, same wording, as a class's homeroom teacher.

Ownership is **not** a standing grant. `creator` is a historical column that no
demotion sweeps (unlike the assignment list above), so every course-management
and course-ownership check re-reads the caller's *live* role first: a creator
demoted to `student` or `parent` keeps neither management nor deletion of the
course they made — it stays reachable to its still-`teacher`+ assignees and to
manager+, who can hand it to someone else. Nothing below is a right a caller
holds while under `teacher`. The column is deliberately **not** swept the way
the assignment list is: it answers "who made this", which stays true after a
demotion — it is the *grant* that is role-gated, not the history. The same
floor applies to the catalogs: a demoted creator's own catalog row drops out
of their `/courses` list, and comes back only if they are enrolled in one of
its instances, as any student would be. A session's `teacher` behaves
identically — teaching a session grants no roll call, and no view of it, once
the account is below `teacher`.

Teaching data is walled **per instance**. An instance, its exams, its sessions,
its homework and its rosters are
**visible** only to its enrolled students, its assigned teachers, its class's
homeroom teacher, and manager+ — a student
sees just the instances their sections carry or they joined, and `/exams`,
`/homework` and `/instances/me` are filtered accordingly. The catalog row, its
subjects and its membership list are readable by that same audience (a student
enrolled in any of its instances, a teacher assigned to one, a member of the
catalog row itself, its creator, manager+). Teacher-level reads *inside* an
instance
(roster, results, statistics, the question list, answer sheets, the live
monitor) additionally need **instance-management rights** (manager+, one of its
assigned teachers, or its class's homeroom teacher — each of them still
`teacher`+ today):
one teacher cannot look into another teacher's instance, and the per-user
marks/attendance reports narrow to the instances the caller manages.

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

<!-- BEGIN GENERATED: endpoint-table -->

| Method | Path                                                             | Auth    | Description |
|--------|------------------------------------------------------------------|---------|-------------|
| GET    | `/`                                                              | no      | Same as `/health` |
| GET    | `/academic-years`                                                | teacher | List every academic year, newest first. Requires teacher+. Paged via `?limit=&offset=` (omit `limit` for the full list); returns a `{items, total, limit, offset}` envelope. |
| POST   | `/academic-years`                                                | manager | Create an academic year. Requires manager+. Past dates are allowed — years are calendar structure, not schedules. `grade_promotions` is the sınıf-geçme policy the rollover applies. |
| GET    | `/academic-years/{id}`                                           | teacher | Fetch a single academic year by id. Requires teacher+. |
| PATCH  | `/academic-years/{id}`                                           | manager | Update an academic year. Requires manager+. Omitted fields keep their value; `grade_promotions` is replaced as a whole when sent. An archived year is read-only. |
| DELETE | `/academic-years/{id}`                                           | manager | Delete an academic year. Requires manager+. Refused with a 409 while any şube or dönem still links it — move or delete them first. An archived year must be re-opened (`PATCH` is not enough: there is no unarchive here, so the row is only deletable while open) like every other past-structure write. |
| POST   | `/academic-years/{id}/archive`                                   | manager | Archive an academic year. Requires manager+. Stamps `archived_at`, and from then on the whole year is past structure: no new şube, no new dönem, no exam inside it, and no edit or delete of the year itself. Idempotent — a second archive answers `200` with the stamp that already stood. There is no unarchive route: the row is deletable only while open. |
| POST   | `/academic-years/{id}/rollover`                                  | manager | Carry another year's şubeler into this one. Requires manager+. Each şube of `from_year` whose grade the target year promotes is planted afresh here — same name, mapped grade, and copies of its instances (`ders_saati`, karne policy, teachers) and of every live member, who are also enrolled into the new instances. A grade with no promotion entry stays behind: that is graduation, and `graduated` names it. The target must be empty (a second rollover into it is a 409, which is what makes the command idempotent), open, and different from the source. |
| GET    | `/ai/capabilities`                                               | student | The capabilities connected AI services are serving right now, per capability, with the work in flight — a discovery read for an operator or a service deciding what it may ask this bridge to do. |
| GET    | `/ai/certificate`                                                | no      | The AI bridge's certificate (PEM + SHA-256 fingerprint) for a service to pin; `404` when the bridge is off. |
| GET    | `/api-docs/openapi.json`                                         | no      | Raw OpenAPI 3 spec |
| GET    | `/appointments`                                                  | student | List bookings, newest first. Students and parents see the ones they requested; teacher+ see the ones aimed at their own slots — their request inbox. Managers and admins read their own inbox too (they may still decide any booking by id). Paged via `?limit=&offset=` (omit `limit` for every booking); returns a `{items, total, limit, offset}` envelope. |
| POST   | `/appointments`                                                  | student | Ask for a meeting on a published slot. Students and parents only — a parent books for *themselves* (this is the parent-teacher conference), never on behalf of a child, and staff arrange between themselves off this API. The booking lands `pending`: publishing availability is not consent to a particular person and topic. Refused (`409`) when the slot's window has already opened, when it is already taken, when the requester is already committed at that time, or when the slot's teacher no longer holds a teaching role. |
| GET    | `/appointments/slots`                                            | student | List slots, earliest first. Teacher+ see their own calendar, past occurrences included; everyone else sees every slot that has not started yet — the bookable calendar, bounded exactly as booking is, so a slot already underway is left out rather than offered for a request that could only answer `409`. A demotion below `teacher` withdraws that account's calendar outright, so there is normally nothing of theirs left to list; a slot an older build stranded is left out here too (and refused at book time). Whether a slot is already taken is not carried here: booking a taken one answers `409`. Paged via `?limit=&offset=` (omit `limit` for every slot); returns a `{items, total, limit, offset}` envelope. |
| POST   | `/appointments/slots`                                            | teacher | Publish availability. Requires the `teacher` role or higher; the slot lands on the caller's own calendar. With `repeat_weekly` the same window is expanded into one row per week up to and including `until` (at most 52), all sharing a `series` id — each occurrence is then independently bookable and independently deletable. The response is always an array: one element for a one-off publish, one per occurrence for a weekly one. |
| DELETE | `/appointments/slots/series/{series}`                            | teacher | Withdraw a whole recurring publish. Same rights as the single delete, and all-or-nothing: if *any* occurrence still carries a pending or approved booking the entire series is refused (`409`), so the person waiting is dealt with rather than silently left on a stray week. |
| DELETE | `/appointments/slots/{id}`                                       | teacher | Withdraw one slot. Requires teacher+; the publishing teacher may drop their own and managers/admins anyone's. A slot carrying a pending or approved booking is refused (`409`) — reject or cancel that booking first. Settled bookings (rejected/cancelled) go with the slot. |
| PATCH  | `/appointments/{id}/approve`                                     | teacher | Confirm a pending booking. Requires teacher+ and ownership of the slot (or manager/admin). The double-booking guard runs here, against both sides: approval is what commits anyone, so a time colliding with another approved meeting of the teacher or of the requester is refused (`409`), as is a window that has already started — that meeting could never be cancelled. |
| PATCH  | `/appointments/{id}/cancel`                                      | student | Call a meeting off. **The requester only** — the person who asked for it — from either live state, so an approved meeting can still be dropped and the slot freed. Anyone else is a `403`, the slot's teacher and a manager/admin included (the guard compares ids, not roles): a teacher ends a booking by **rejecting** it while it is `pending`, and an already-approved one by counter-proposing another time (`PATCH /{id}/reschedule` — which sends it back to `pending`) and then rejecting it. Refused (`409`) once the meeting's window has started: a meeting that already began is history, not a plan. |
| PATCH  | `/appointments/{id}/reject`                                      | teacher | Turn a pending booking down. Requires teacher+ and ownership of the slot (or manager/admin). The slot frees up immediately — occupancy counts live bookings only, so someone else may take it. |
| PATCH  | `/appointments/{id}/reschedule`                                  | teacher | Counter-propose another time for a booking, on the same row (the reason and the history stay in one place). Requires teacher+ and ownership of the slot (or manager/admin). |
| PATCH  | `/appointments/{id}/reschedule/accept`                           | student | Accept the teacher's counter-proposal. The requester's call alone — it is their commitment. Accepting *is* approval at the proposed time, so the double-booking guard runs again for both sides (`409` if the moved time now collides with something else, or has already started — agreeing to a window that began would mint a meeting nobody can cancel). |
| PATCH  | `/appointments/{id}/reschedule/decline`                          | student | Refuse the teacher's counter-proposal. The requester's call alone, and it **cancels the booking**: the proposal replaced the time that was asked for, so there is nothing left to fall back to — book another slot instead. The slot frees up, and the original request stays readable as `cancelled` with the refused proposal still on it. Declining is a cancel, so it answers to the same deadline: `409` once the meeting's effective window has started. |
| GET    | `/attendance/me`                                                 | student | The current user's attendance report: event tallies, lesson roll-call tallies, a per-instance breakdown with attendance rates, and the per-dönem devamsızlık. |
| GET    | `/attendance/{user}`                                             | teacher | Any user's attendance report. Requires teacher+, or a parent tied to the target student. Managers, admins, and parents see every instance; a teacher sees the event tallies plus only the roll-call blocks — and the devamsızlık days — of the instances they run. |
| POST   | `/auth/login`                                                    | no      | Log in with username + password — no school. Sets a `session` cookie on success: exactly one active membership enters that school right away (`<slug>.<token>` and the full [`UserResponse`], unchanged for single-school clients), several answer a [`SchoolChoiceResponse`] with a `person.<token>` cookie that `POST /auth/school` binds. |
| POST   | `/auth/logout`                                                   | no      | Log out: revoke the current session (if any) and clear the cookie. Idempotent — no session required; answers `204` either way. |
| GET    | `/auth/me`                                                       | student | Return the currently authenticated user. |
| POST   | `/auth/register`                                                 | no      | Register a new user account, or attach an existing person to one more school: `{school, username, password}` in, `{username, role}` back (no `id`; new accounts are `student`). Always `201` — see below. |
| POST   | `/auth/school`                                                   | no      | Bind a `person.<token>` session to one of the person's schools: the cookie is replaced with that school's own `<slug>.<token>` and the person session is revoked. Deliberately outside the credential rate-limit tier — this is not a credential guess, and it requires a session cookie already. |
| GET    | `/bank-questions`                                                | teacher | The bank the caller may see — their own templates plus the ones published to the school (admins see every one), **newest first**. `?subject=` narrows to one origin subject; `?owner=` to one owner (a user id, or `me` for the caller); `?q=` to a case-insensitive fragment of the question text; `?visibility=private\|school` to one shelf — it narrows what the caller may already see and never widens it, so `private` is "my drafts" and `school` the published library. Paged via `?limit=&offset=` (omit `limit` for all of them); returns a `{items, total, limit, offset}` envelope, where `total` counts every match under the same filters, not just this page. Each item carries the resolved `subject_name`/`owner_name` so a client needn't look them up per row, plus `used_count` — how many exam questions were copied out of that template (one grouped query for the page, not one per row). |
| POST   | `/bank-questions`                                                | teacher | Add a template to the bank. Requires teacher+. `subject_id` is origin metadata (any subject — the same-course rule lives at instantiate time), so it need only exist (an unknown subject is a `400`). `choice` templates carry 2–10 `choices` plus `correct` naming one of them by id; `text` templates carry neither. The caller becomes the owner. |
| GET    | `/bank-questions/{bid}`                                          | teacher | One template by id. Visible ones only: a `private` template belonging to someone else is a 404, not a 403 — a 403 would confirm it exists. |
| PATCH  | `/bank-questions/{bid}`                                          | teacher | Edit a template. Owner only (admins aside — 403 otherwise). Concurrent edits of *different* fields merge instead of reverting each other (the save is conditioned on the snapshot it merged over, and re-merges when it loses). Omitted fields keep their value; `kind`/`choices`/`correct` are re-validated as a unit, so a kind switch must bring the matching fields along. Replacing or clearing `choices` drops the old options' pictures. Bank templates never freeze — they have no exam tie. |
| DELETE | `/bank-questions/{bid}`                                          | teacher | Delete a template. Owner only (admins aside). Cascades its image rows and takes their blobs off disk. |
| GET    | `/bank-questions/{bid}/choices/{choice_id}/image`                | teacher | One option's picture bytes. Readable by anyone who may see the template — 404 otherwise, never a 403. |
| POST   | `/bank-questions/{bid}/choices/{choice_id}/image`                | teacher | Attach (or replace) one option's picture on a `choice` template. Owner only (admins aside). Same form, limits, and rules as the illustration upload; `choice_id` is the `id` carried on that choice, as returned in the template's `choices` (not a position — an unknown id is a `400`). Replacing the `choices` list drops all its option pictures. |
| DELETE | `/bank-questions/{bid}/choices/{choice_id}/image`                | teacher | Remove one option's picture. Owner only (admins aside). |
| GET    | `/bank-questions/{bid}/image`                                    | teacher | A template's illustration bytes. Readable by anyone who may see the template — 404 otherwise, never a 403. |
| POST   | `/bank-questions/{bid}/image`                                    | teacher | Attach (or replace) a template's illustration. Owner only (admins aside). `multipart/form-data` with the image under a `file` field; the declared content type must be `image/png`, `image/jpeg`, `image/webp`, or `image/gif` (rasters only — no SVG), the bytes at most the school's `max_file_bytes` (settings). |
| DELETE | `/bank-questions/{bid}/image`                                    | teacher | Remove a template's illustration. Owner only (admins aside). |
| GET    | `/boards`                                                        | student | Every board the caller may open — the ones they created and the ones they were invited to — newest first. `?open=` narrows by the closed flag, and `total` counts the filtered list. Paged via `?limit=&offset=` (omit `limit` for all of them). |
| POST   | `/boards`                                                        | student | Open a whiteboard. The caller becomes its creator — the only one who may clear, lock, close or delete it — and everyone named in `participants` may draw on it. Student and above: the `parent` role has no whiteboard access at all, neither as a creator nor as a participant. `409` once the caller holds `max_boards_per_creator` boards (`GET /limits`); delete one to free a seat. |
| GET    | `/boards/{id}`                                                   | student | Fetch one board. A caller who is neither its creator nor one of its participants gets a `404`, not a `403` — an outsider must not learn that a board exists. |
| PATCH  | `/boards/{id}`                                                   | student | Edit a board. Re-titling is open to every participant; changing the invite list or the lock is the creator's alone (`403` for anyone else on the board). Omitted fields keep their value. |
| DELETE | `/boards/{id}`                                                   | student | Delete the board and its whole stroke history — creator only, and the one operation in this system that really destroys marks (a clear never does). It also frees the creator's board seat. Use `/close` to retire a board while keeping it readable. |
| POST   | `/boards/{id}/clear`                                             | student | Empty the canvas — creator only. **Nothing is deleted**: the board's epoch is bumped and a `clear` marker is appended carrying the closed epoch's final stroke count, so the live canvas is blank while every mark ever drawn stays readable through `/history`. Also resets the live-canvas cap, which is how a board that answered "clear it to keep drawing" is recovered. `409` on a closed board, on a **locked** one — the pause holds against its own creator, so a locked full board is recovered by unlock, clear, relock — and on a canvas that is **already blank**: the marker is a real stroke row charged to the board's lifetime cap, so a clear has to close at least one mark to be worth a row. A board that is both locked and closed answers **closed** — here and on every stroke path alike, since there is no reopen and the pause can never lift. |
| POST   | `/boards/{id}/close`                                             | student | Retire the board — creator only. It becomes permanently read-only: no more strokes and no more clears, while every stroke and every epoch stays readable. Idempotent: closing an already-closed board returns it with its original `closed_at` rather than re-stamping. There is no reopen — a closed board is finished, and a new one is cheap. |
| GET    | `/boards/{id}/epochs`                                            | student | The epoch index: every `clear` marker this board has, oldest first. Each one carries the epoch it closed (`epoch`), that epoch's final stroke count (`count`), who cleared and when — which is all a client needs to offer "replay session 3" without scanning the log. The markers *are* the index, so the current (unclosed) epoch is deliberately absent. Paged via `?limit=&offset=`. |
| GET    | `/boards/{id}/history`                                           | student | The whole stroke log, oldest first — every epoch the board ever had, or the single `?epoch=` named. A clear deletes nothing, so this replays the entire session; the `clear` markers in the stream are where one epoch ended. Paged via `?limit=&offset=`. |
| POST   | `/boards/{id}/invite`                                            | student | Invite a whole roster at once — creator only, and additive: everyone the source names is **added** to the invite list, nobody is ever removed by it. Re-inviting the same source is therefore how a board is topped up after the class gained a student, and it is idempotent when nothing changed. |
| GET    | `/boards/{id}/strokes`                                           | student | The live canvas: the current epoch's strokes, oldest first, paged via `?limit=&offset=`. This is what a client draws to catch up — earlier epochs are still stored, and read through `/history`. Drawn marks only: a `clear` marker never appears here, exactly as it never appears on the board room's socket. Read `/history` or `/epochs` for the markers. |
| GET    | `/boards/{id}/ws`                                                | student | **WebSocket** board room: a `join` replays the current epoch, every accepted stroke fans out to the other participants (see "Collaborative whiteboard") |
| POST   | `/builder/login`                                                 | no      | Log in as the deployment's builder. Sets a `session` cookie (`builder.<token>`) that works on this surface and nowhere else. |
| POST   | `/builder/logout`                                                | no      | Log out a builder: revoke the session (if any) and clear the cookie. Idempotent — answers `204` either way. |
| GET    | `/builder/me`                                                    | builder | The current builder. |
| GET    | `/chatbot/threads`                                               | student | The caller's own threads, most recently active first. Paged via `?limit=&offset=`. Nobody — no teacher, no admin — reads anyone else's. |
| POST   | `/chatbot/threads`                                               | student | Start a new chatbot thread, optionally named. Every authenticated role may chat, parents included. A user may keep up to the school's `max_chatbot_threads` threads; at the cap the request is refused (409) until an old thread is deleted — the cap is storage protection, not a usage quota (that is the per-minute message limit). |
| PATCH  | `/chatbot/threads/{id}`                                          | student | Rename a thread, or clear its name (`title: null`). Owner only; someone else's thread is a `404`, never a `403`. The edit counts as activity, so the thread moves to the top of the list — renaming is how a user files a thread, and a rename that left it buried would be useless. |
| DELETE | `/chatbot/threads/{id}`                                          | student | Delete a thread and every turn in it, permanently. Owner only; someone else's thread is a `404`, never a `403` (its existence is not leaked). |
| GET    | `/chatbot/threads/{id}/messages`                                 | student | The whole thread, oldest first. Paged via `?limit=&offset=`. Owner only. |
| POST   | `/chatbot/threads/{id}/messages`                                 | student | Ask the chatbot: `{content}`, at most the school's `max_chatbot_message_len`. Answers `202 {message_id, status: "pending"}` the moment both rows are written — the answer itself lands later, in the reserved assistant row. `503` when no AI service offers `chat.reply`, and nothing is written; `429` + `Retry-After` over the per-user send limit. |
| GET    | `/chatbot/threads/{id}/messages/{mid}`                           | student | Poll one turn. The non-SSE fallback for `/stream`, reading the same row — including the projection that presents a long-stale `pending` as `failed`, so the two can never disagree about a turn's state. |
| GET    | `/chatbot/threads/{id}/messages/{mid}/stream`                    | student | Watch one turn as Server-Sent Events: `delta` chunks of the answer, then a single `done` carrying the finished message, or one `error`. The stream closes after `done`/`error` — one stream per turn, not per thread. |
| GET    | `/classes`                                                       | teacher | List every class, newest first. Requires teacher+. `?grade=` narrows to one grade label, matched exactly as written — the label a blueprint is keyed by, so this is the read that shows which sections a `POST /classes/blueprints` pump covered (and `?grade=` on its own lists the sections with no grade at all). An unknown label is an empty page, not a `404`. Paged via `?limit=&offset=` (omit `limit` for the full list); returns a `{items, total, limit, offset}` envelope whose `total` counts every class under the same filter, not just this page. |
| POST   | `/classes`                                                       | manager | Create a class. Requires manager+ — a class is school structure, not a teacher's own room. `grade` is a free-text label for the year ("9", "10-A"), `year` links the academic year (which is what binds the şube to a karne and to the rollover), `teacher_id` names the homeroom teacher (sınıf öğretmeni, a teacher+ account); all optional. |
| GET    | `/classes/blueprints`                                            | manager | List every grade blueprint, by grade label. Requires manager+. Paged via `?limit=&offset=` (omit `limit` for all of them). |
| POST   | `/classes/blueprints`                                            | manager | Create a grade's course blueprint and stock every class section already at that grade with it. Requires manager+. |
| GET    | `/classes/blueprints/{grade}`                                    | manager | Fetch one grade's blueprint. Requires manager+. |
| PATCH  | `/classes/blueprints/{grade}`                                    | manager | Replace a blueprint's course list and reconcile every class section at that grade with it. Requires manager+. |
| DELETE | `/classes/blueprints/{grade}`                                    | manager | Delete a grade's blueprint. Requires manager+. Every attachment the blueprint made is detached with it (their pumped enrollments swept the usual way); a course a human attached to one of those classes by hand carries no blueprint tag and survives. |
| GET    | `/classes/blueprints/{grade}/status`                             | manager | Which sections at a grade are out of sync with its blueprint. Requires manager+. Reads only — nothing is attached, detached or pruned. |
| GET    | `/classes/me`                                                    | student | The classes the caller is a member of, newest membership first. Any authenticated role — this is the one class read a student (or a parent, for themselves) can make, since every other `/classes` route is teacher+. Paged via `?limit=&offset=` (omit `limit` for all of them); returns a `{items, total, limit, offset}` envelope. Staff, who are never class members, simply get an empty page. |
| GET    | `/classes/user/{user}`                                           | teacher | Another user's classes. Requires teacher+, or a parent tied to the target student — the same bar the per-student reports hold, and the same 403 for everyone else (a student reads their own at `GET /classes/me`). |
| GET    | `/classes/{id}`                                                  | teacher | Fetch a single class by id. Requires teacher+. |
| PATCH  | `/classes/{id}`                                                  | manager | Update a class. Requires manager+. Omitted fields keep their value; `grade`, `year` and `teacher_id` are nullable, so an explicit `null` clears them. |
| DELETE | `/classes/{id}`                                                  | manager | Delete a class. Requires manager+. Refused with a 409 while it still holds students or courses — nothing cascades, because dropping the class silently would leave the enrollments it pumped with nothing left to sweep them. |
| POST   | `/classes/{id}/blueprint`                                        | manager | Stock one class section from its grade's blueprint. Requires manager+ — this writes the roster of every course in the template, which is the office's call, not one course owner's. |
| GET    | `/classes/{id}/instances`                                        | teacher | List the instances a class carries — each a catalog course as this section teaches it, with its hours, karne policy and teachers — newest first, paged via `?limit=&offset=` (omit `limit` for all of them). Requires teacher+. Returns a `{items, total, limit, offset}` envelope. `POST /instances/{id}` edits one; this is the read that names them. |
| POST   | `/classes/{id}/instances`                                        | teacher | Attach a course to a class: this is what **mints the instance** — the class×course row every exam, session, lesson and roster under this class's course now keys on. Requires teacher+ and catalog rights on that course (its creator, or a manager/admin): attaching writes that course's roster for this section, and the weekly hours and karne policy the instance starts with. The class's whole roster is enrolled in one go, and students already enrolled by hand keep their own rows. Attaching the same course twice is a 409. |
| DELETE | `/classes/{id}/instances/{instance}`                             | teacher | Detach an instance from a class: the instance and everything the class taught under it — exams (with results, questions and images), homework (with submissions and grades), sessions and roll call, the roster it pumped and its teacher links — are swept, and the uploaded files those rows named are unlinked from disk. Requires teacher+ and catalog rights on the course the instance teaches (its creator, or a manager/admin): this is the inverse of attaching. |
| GET    | `/classes/{id}/members`                                          | teacher | List a class's roster, newest first, paged via `?limit=&offset=` (omit `limit` for the whole roster). Requires teacher+. Returns a `{items, total, limit, offset}` envelope. |
| POST   | `/classes/{id}/members`                                          | manager | Put a student in a class. Requires manager+. They are enrolled into every instance the class already carries, in one go, and a student already enrolled by hand keeps the row they have (and it survives their removal from the class). Adding the same student twice is a 409; a student who *left* is added afresh — the roster is a history, and the partial index only holds one live stint per pair. |
| DELETE | `/classes/{id}/members/{user}`                                   | manager | Take a student out of a class. Requires manager+. The enrollments the class pumped for them are swept with it — except rows placed by hand, which are left standing. There is no heir to hand a swept row to: a second section teaching the same course holds its **own** instance, so its roster is its own row and this one is released. A student who was not in the class is a 404. |
| GET    | `/course-notes`                                                  | student | List a course's notes, newest first. Visible to whoever can view the course (its creator, a manager/admin, or anyone the course reaches). Paged via `?limit=&offset=`. |
| POST   | `/course-notes`                                                  | teacher | Create a note on a course. Requires teacher+ and catalog rights over the course (its creator or a manager/admin). |
| GET    | `/course-notes/{id}`                                             | student | Fetch a single course note by id. Visible to whoever can view its course. |
| PATCH  | `/course-notes/{id}`                                             | teacher | Update a course note's title and/or content. Omitted fields keep their value. Requires teacher+ and management rights over the course. |
| DELETE | `/course-notes/{id}`                                             | teacher | Delete a course note, along with its files. Requires teacher+ and management rights over the course. |
| GET    | `/course-notes/{id}/files`                                       | student | List a course note's files (metadata only), newest first. Paged via `?limit=&offset=`. |
| POST   | `/course-notes/{id}/files`                                       | teacher | Attach a file to a course note. `multipart/form-data` with the file under a `file` field; its `filename` is required. At most 10 files per note, each at most the school's `max_file_bytes` (settings, default 5 MiB). Requires teacher+ and management rights over the course. |
| GET    | `/course-notes/{id}/files/{file_id}`                             | student | Download a course note file's bytes. `Content-Type` is the one declared on upload; `Content-Disposition` carries the original filename. |
| DELETE | `/course-notes/{id}/files/{file_id}`                             | teacher | Delete a course note file (row first, then its blob). Requires teacher+ and management rights over the course. |
| GET    | `/course-notes/{id}/rag`                                         | student | List a course note's AI outputs, newest first. Visible to whoever can view the course. Paged via `?limit=&offset=`. |
| POST   | `/course-notes/{id}/rag/reindex`                                 | teacher | Re-index a course note now, without waiting for its next write. |
| DELETE | `/course-notes/{id}/rag/{output_id}`                             | teacher | Delete one stored AI output. Requires teacher+ and management rights over the course. Dropping an output does not stop the next note or file change from regenerating one. |
| GET    | `/courses`                                                       | student | List the catalog courses visible to the caller: every course for manager+, otherwise the courses they teach somewhere plus the ones they're enrolled in. Paged via `?limit=&offset=` (omit `limit` for the full list); returns a `{items, total, limit, offset}` envelope. |
| POST   | `/courses`                                                       | teacher | Create a catalog course owned by the current user. Requires the `teacher` role or higher. `kind` picks the flavor — `course` (a regular class, the default), `study` (a supervised study session — etüt), or `club` (a student club — kulüp). A catalog row teaches nobody by itself: a şube attaches it into an instance (`POST /classes/{id}/instances`), and a `study`/`club` is joined school-wide (`POST /courses/{id}/members`). |
| GET    | `/courses/me`                                                    | student | The catalog courses the current user is reached by, paged via `?limit=&offset=` (omit `limit` for all of them); returns a `{items, total, limit, offset}` envelope. |
| GET    | `/courses/{id}`                                                  | student | Fetch a single catalog course by id. Visible to the people it reaches — students enrolled in any of its instances, members of the course itself — and to its creator and managers/admins while those accounts are still `teacher`+. |
| PATCH  | `/courses/{id}`                                                  | teacher | Update a catalog course. Requires teacher+ and catalog rights — its creator, or a manager/admin. Omitted fields keep their value. |
| DELETE | `/courses/{id}`                                                  | teacher | Delete a catalog course. Requires teacher+; only its creator or a manager/admin may delete it. Refused with a 409 while the course is still taught anywhere — detach it from every şube (`DELETE /classes/{id}/instances/{instance}`) and remove its individual members first, so a course that carries teaching is never dropped by accident. Once free, it cascades the instances' exams (with their results, questions, answers, and question images), homework (with submissions, submission files, and grades), sessions and roll call, its individual memberships, its subjects, and the teacher links. It also strikes its id out of every class blueprint that named it — a template holding a course nothing can resolve is a stocking run that skips it and a `PATCH` that refuses the very list the template already holds. |
| GET    | `/courses/{id}/members`                                          | teacher | List a club/etüt's members, newest first, paged via `?limit=&offset=` (omit `limit` for the whole list). Requires teacher+ and catalog rights. Returns a `{items, total, limit, offset}` envelope. An instance's roster is `GET /instances/{id}/enrollments`. |
| POST   | `/courses/{id}/members`                                          | teacher | Add a user to a club or etüt — the **school-scoped** membership tier. Requires teacher+ and catalog rights (its creator, or a manager/admin). Only students can be added, and only to a `study` (etüt) or `club` (kulüp): a regular ders (`kind` `course`) has no school-wide roster — its students come from the şubeler that teach it, and that join is `POST /instances/{id}/enrollments` (400 here). Idempotent: a pair that already holds a membership is returned as-is. |
| DELETE | `/courses/{id}/members/{user}`                                   | teacher | Remove a user from a club or etüt. Requires teacher+ and catalog rights. Existing exam results and badges are untouched — the membership is a door, not a record of what happened inside. A pair holding no membership is a 404. |
| GET    | `/courses/{id}/subjects`                                         | student | List a course's subjects in creation order, paged via `?limit=&offset=` (omit `limit` for all of them). Visible to its creator, to managers/admins, and to anyone the course reaches (a student enrolled in one of its instances, or a member of it). Returns a `{items, total, limit, offset}` envelope. |
| POST   | `/courses/{id}/subjects`                                         | teacher | Create a subject inside a course. Requires teacher+ and catalog rights (its creator, or a manager/admin). Subjects are the curriculum topics of the *catalog* row — every exam of every instance teaching it tags its questions with one of them. |
| GET    | `/events`                                                        | student | List all events, newest first. Paged via `?limit=&offset=` (omit `limit` for every event); returns a `{items, total, limit, offset}` envelope. |
| POST   | `/events`                                                        | teacher | Create an event owned by the current user. Requires the `teacher` role or higher. `audience` targets it at a role, a course's enrollment, a class section's roster, or a registration list (filled via `POST /events/{id}/register`); omitted it is school-wide. Everyone still sees every event — the audience is the expected-attendee roster, not a visibility wall. |
| GET    | `/events/{id}`                                                   | student | Fetch a single event by id. |
| PATCH  | `/events/{id}`                                                   | teacher | Update an event. Requires teacher+; the creator may edit their own event and managers/admins may edit anyone's. Omitted fields keep their value; an explicit `null` clears `starts_at`/`ends_at`. A provided `audience` replaces the current one wholesale — attendance and signup rows for people it drops stay stored but leave the roster report (signups resurface if the event is switched back to the registration kind). |
| DELETE | `/events/{id}`                                                   | teacher | Delete an event. Requires teacher+; the creator may delete their own event and managers/admins may delete anyone's. |
| GET    | `/events/{id}/attendance`                                        | teacher | List the attendance roster for an event, paged via `?limit=&offset=` (omit `limit` for the whole roster). Requires teacher+ — students see their own tallies via `GET /attendance/me`. Returns a `{items, total, limit, offset}` envelope. |
| POST   | `/events/{id}/attendance`                                        | teacher | Mark attendance for a user on an event (defaults to the caller). Taking attendance is a teacher+ action — students never mark, not even themselves — and the target must be in the event's audience. |
| DELETE | `/events/{id}/attendance/{user}`                                 | teacher | Remove a user's attendance record from an event. Requires teacher+. |
| POST   | `/events/{id}/register`                                          | teacher | Put a user on a registration-audience event's signup list. Requires teacher+. `user_id` must name a student — students are placed by staff and never register themselves; omit it to take a seat yourself (staff self-serve, so registering another teacher/manager is refused). Registering the same person twice is a no-op returning the existing seat. The list closes when the event starts (or its ends_at-only deadline passes) and refuses to grow past `capacity`. |
| DELETE | `/events/{id}/register/{user}`                                   | teacher | Take a user off the signup list — the register rules mirrored: teacher+, students' seats or your own (another *staff* member's seat only if their account no longer exists), and only while the list is open (the event hasn't started or, ends_at-only, passed). A seat held by a `parent` is freeable by any teacher+ as well: no route lets that account free it itself, so a stranded seat needs a door. Attendance already marked stays recorded. |
| GET    | `/events/{id}/roster`                                            | teacher | The event's expected-attendee roster joined with its attendance marks — the who-came/who-missed report. Resolved live from the audience (today's role holders, current enrollment, the current class roster, the current signup list), so it always reflects the present roster; attendance rows for people no longer in the audience are omitted here (they remain in `GET /events/{id}/attendance`). Requires teacher+. Paged via `?limit=&offset=` (omit `limit` for the whole roster). |
| GET    | `/exams`                                                         | student | List the exams visible to the caller: every exam for manager+, otherwise the exams of the instances they teach or are enrolled in — minus other people's drafts (a draft shows only to its instance's managers). Paged via `?limit=&offset=` (omit `limit` for the full list); returns a `{items, total, limit, offset}` envelope. |
| GET    | `/exams/{id}`                                                    | student | Fetch a single exam by id. Visible to **any instance the exam is addressed to** — its owner's enrolled students and teachers, and, for an announced exam (ortak sınav), each addressed section's alike — plus managers/admins; drafts only show to the managers of an addressed instance (everyone else gets a `404`, as if the exam doesn't exist yet — because it doesn't, officially). |
| PATCH  | `/exams/{id}`                                                    | teacher | Update an exam. Requires teacher+ and management rights over the exam's instance (an assigned teacher, its class's homeroom teacher, or a manager/admin). Omitted fields keep their value; an explicit `null` clears a schedule field; the instance an exam hangs off is not updatable here. The schedule must stay consistent as a whole (see the create endpoint), and `mode` is frozen once anyone has started an attempt — times, duration, `max_attempts`, and `allow_rejoin` stay editable so a running exam can be extended, granted retakes, or have its rejoin door opened live. `draft: false` publishes a draft; `draft: true` re-hides an exam, but only while it has no attempts and no results (`409` otherwise) — students never lose sight of an exam they've already sat or been graded on. |
| DELETE | `/exams/{id}`                                                    | teacher | Delete an exam. Requires teacher+ and management rights over the exam's instance (an assigned teacher, its class's homeroom teacher, or a manager/admin). Cascades the exam's results, attempts, questions, answers, and question + answer images (blobs included). |
| GET    | `/exams/{id}/attempt`                                            | student | The caller's own (latest) attempt: status, deadline, remaining time, and mark once graded — everything a student's live exam screen needs, judged by the server clock. `404` until an attempt is started. |
| POST   | `/exams/{id}/attempt`                                            | student | Start, resume, or retake the caller's attempt. Requires the student role (staff run exams, they don't sit them), enrollment in the exam's instance, a sittable exam (`sync`/`async`/`open` mode), and — when a window exists — the window to be open. A still-running attempt is returned as-is (`200` instead of `201`), so a reconnecting client gets its original clock back — re-starting never resets the time. Once the latest attempt is submitted or expired, re-posting starts the next sitting (`201`, blank answer sheet) while the exam's `max_attempts` (0 = unlimited) allows it. |
| POST   | `/exams/{id}/attempt/answers`                                    | student | Save (or overwrite) one answer in the caller's in-progress attempt. `choice` questions take `selected`; `text` questions take `text`. Requires the student role and enrollment in the exam's instance — an unenrollment (or a promotion out of `student`) mid-exam closes the sheet. Rejected once the attempt is submitted or its deadline has passed — the server clock, not the client's, is the judge — and rejected while the student has left the exam room with the rejoin door closed. |
| GET    | `/exams/{id}/attempt/answers/{qid}/image`                        | student | The caller's own drawn-answer bytes. Same visibility wall as the sitting question view — enrollment plus a started attempt (404 before that). |
| POST   | `/exams/{id}/attempt/answers/{qid}/image`                        | student | Attach (or replace) the caller's drawn answer to a question inside their in-progress attempt. `multipart/form-data` with the drawing under a `file` field; the declared content type must be `image/png`, `image/jpeg`, `image/webp`, or `image/gif` (rasters only — no SVG), the bytes at most the school's `max_file_bytes`. Rides the exact `POST /exams/{id}/attempt/answers` gate chain: the student role, an in-progress attempt, current enrollment, and the rejoin door. |
| DELETE | `/exams/{id}/attempt/answers/{qid}/image`                        | student | Clear the caller's drawn answer to a question. Same writable-attempt gate chain as the upload. |
| POST   | `/exams/{id}/attempt/finish`                                     | student | Submit the caller's attempt. Allowed while the deadline hasn't passed; after it, the attempt is already `expired` (a valid terminal state — the student used their full time) and submitting is a `409`. |
| GET    | `/exams/{id}/attempt/questions`                                  | student | The exam's questions as the sitting student sees them: `correct` stripped, their own saved answers embedded — the latest sitting's, since a retake starts from a blank sheet. Requires enrollment in the exam's instance (the questions are the instance's content — leaving it closes them) and an attempt — start one with `POST /exams/{id}/attempt` first (404 until then). Readable in every attempt state, so a submitted student can still review what they wrote. |
| GET    | `/exams/{id}/attempt/ws`                                         | student | **WebSocket** exam room (students only): state ticks, autosave, finish; entering clears `left_at`, leaving stamps it (see "Taking an exam") |
| GET    | `/exams/{id}/attempts/{user}/answers`                            | teacher | One student's answer sheet with correctness flags — always the *latest* sitting's answers (a retake starts from a blank sheet). Every row carries the saved answer plus `is_correct` (`null` for text questions — those are the grader's call), and the machine's `auto_score` over the choice questions is attached as a *suggestion*: the final mark stays human, via `POST /exams/{id}/results`. Requires teacher+ and management rights over the exam's instance. |
| GET    | `/exams/{id}/attempts/{user}/answers/{qid}/image`                | teacher | One student's drawn-answer bytes, for the grader. Requires teacher+ and management rights over the exam's instance — the `attempt_answers` gate. |
| GET    | `/exams/{id}/audience`                                           | student | List the instances an exam is announced to, its owner included — the read behind the announce routes' answer, useful on its own to a client that wants to show where else an exam is sat. Visible to the exam's own audience: an addressed instance's enrolled students, its teachers (or its şube's homeroom teacher), and managers/admins — except drafts, which stay a `404` to everyone but an addressed instance's managers. |
| POST   | `/exams/{id}/audience`                                           | teacher | Announce an exam to another instance — the **ortak sınav** write: one exam addressed to a sibling şube, so it and its marks stand in that section's exam list, marks report and karne. Requires teacher+ and management rights over the exam's **owner** instance (an assigned teacher, its şube's homeroom teacher, or a manager/admin) — the target instance's teachers have no say. The target must teach the exam's own catalog course and sit under the same academic year (`400` otherwise), the owner itself is refused (`400`), and an archived target year is a `409`. Announcing a pair that already stands is a no-op answering `200` with the audience unchanged. |
| DELETE | `/exams/{id}/audience/{instance}`                                | teacher | Withdraw an exam from one instance's audience. Requires teacher+ and management rights over the exam's **owner** instance, like the announce itself. The owner's own pair is not withdrawable (`400`) — that is the instance the exam belongs to, and deleting the exam is what ends it; an archived year refuses the withdrawal with the same `409` its other writes answer. A pair that holds no audience row is a `404`. |
| GET    | `/exams/{id}/live`                                               | teacher | A one-shot live snapshot of the exam: who's in, who's still writing (and on which sitting), who walked out of the room (`left_at`), who never showed at all (`absent`, once the window is over), time each student has left, and marks as they land — the roster of every instance the exam is addressed to, so an announced-to section's sitters are here too. Requires teacher+ and management rights over the exam's instance. Poll it to keep a monitor up to date. |
| GET    | `/exams/{id}/questions`                                          | teacher | The exam's question list, `correct` choice ids included — the answer key, paged via `?limit=&offset=` (omit `limit` for the whole list). Requires teacher+ and management rights over the exam's instance. Students read questions through `GET /exams/{id}/attempt/questions`. Returns a `{items, total, limit, offset}` envelope. |
| POST   | `/exams/{id}/questions`                                          | teacher | Add a question to an exam. Requires teacher+ and management rights over the exam's instance. `subject_id` must name one of the subjects of that instance's catalog course (`GET /courses/{id}/subjects`) — every question belongs to a subject. `choice` questions carry 2–10 `choices` plus `correct` naming one of them by id; `text` questions carry neither. Locked once attempts exist. |
| POST   | `/exams/{id}/questions/from-bank/{bid}`                          | teacher | Instantiate a bank template into this exam as a fresh question. Requires teacher+, management rights over the exam's instance, and a template the caller may see (their own, or one published to the school) — anything else is a 404. `subject_id` must name one of the subjects of the instance's catalog course — the template's own subject is origin metadata and does not carry over. The template (and its blobs) stay untouched; a full copy — text, points, spec, illustration, and option pictures — lands under a new question id. Locked once attempts exist. |
| PATCH  | `/exams/{id}/questions/{qid}`                                    | teacher | Edit a question. Requires teacher+ and management rights over the exam's instance. Omitted fields keep their value; `kind`/`choices`/`correct` are re-validated as a unit, so a kind switch must bring the matching fields along. `subject_id` re-tags within the instance's catalog course. Locked once attempts exist. An omitted `subject_id` is filled from the stored row, so any edit here — not just a re-tag — is refused with a `409` when someone else moved the question's subject after the caller read it. |
| DELETE | `/exams/{id}/questions/{qid}`                                    | teacher | Remove a question (and every answer to it). Requires teacher+ and management rights over the exam's instance. Locked once attempts exist. |
| GET    | `/exams/{id}/questions/{qid}/choices/{choice_id}/image`          | student | One option's picture bytes. Same access wall as the question-image read. |
| POST   | `/exams/{id}/questions/{qid}/choices/{choice_id}/image`          | teacher | Attach (or replace) one option's picture on a `choice` question — so the options themselves can be images (four map crops, pick the right one). Same form, limits, and rights as the question-image upload; `choice_id` is the `id` carried on that choice, as returned in the question's `choices` (not a position — an unknown id is a `400`). Replacing the question's `choices` list drops all its option pictures — re-upload against the new list. |
| DELETE | `/exams/{id}/questions/{qid}/choices/{choice_id}/image`          | teacher | Remove one option's picture. Requires teacher+ and management rights over the exam's instance; frozen once attempts exist. |
| GET    | `/exams/{id}/questions/{qid}/image`                              | student | The question's illustration bytes. Instance managers read anytime; students through the same wall as the sitting view — enrollment plus a started attempt (404 before that, like the question list itself). |
| POST   | `/exams/{id}/questions/{qid}/image`                              | teacher | Attach (or replace) a question's illustration — any question kind may carry one, e.g. the map the prompt asks about. `multipart/form-data` with the image under a `file` field; the declared content type must be `image/png`, `image/jpeg`, `image/webp`, or `image/gif` (rasters only — no SVG), the bytes at most the school's `max_file_bytes` (settings). Requires teacher+ and management rights over the exam's instance; frozen once attempts exist, like every other question edit. |
| DELETE | `/exams/{id}/questions/{qid}/image`                              | teacher | Remove a question's illustration. Requires teacher+ and management rights over the exam's instance; frozen once attempts exist. |
| POST   | `/exams/{id}/questions/{qid}/refresh-from-bank`                  | teacher | Re-copy a bank template's *current* content over the exam question that was instantiated from it — the escape hatch for the divergence a deep copy creates: fixing a typo in the template does not reach the copies, so this is how a copy is brought back in line, explicitly and per question. Requires teacher+, management rights over the exam's instance, and a template the caller may still see. |
| POST   | `/exams/{id}/questions/{qid}/to-bank`                            | teacher | Save one of this exam's questions into the school-wide bank as a reusable template. Requires teacher+ and management rights over the exam's instance. The caller becomes the template's owner; the question's subject rides along as origin metadata. A full copy — text, points, spec, illustration, and option pictures — lands under a new bank id. Provenance rides both ways: the origin exam is recorded on the template as `source_exam`, and the exam question's `banked_as` is pointed at the new template (a repeat save is allowed and repoints it at the newest one). `from_bank` is left alone — it records the other direction and a save never changes where a question came from. |
| GET    | `/exams/{id}/result`                                             | student | The current user's own result for an exam. Any authenticated user may read their own mark; `404` while ungraded (or when the exam doesn't exist). |
| GET    | `/exams/{id}/results`                                            | teacher | List an exam's results, paged via `?limit=&offset=` (omit `limit` for all of them). Requires teacher+ and management rights over an instance the exam is addressed to — students read only their own via `GET /exams/{id}/result`. Returns a `{items, total, limit, offset}` envelope. |
| POST   | `/exams/{id}/results`                                            | teacher | Record (or overwrite) a student's mark for an exam. Requires teacher+ and management rights over **an instance the exam is addressed to** (the announced-to section's teacher grades its own students on an ortak sınav); the target must be a student enrolled in one of them. Only students carry marks; students never grade — and nobody grades themselves. A draft can't be graded (`409`) — a mark would point at an exam its student can't see. |
| DELETE | `/exams/{id}/results/{user}`                                     | teacher | Remove a student's result from an exam. Requires teacher+ and management rights over an instance the exam is addressed to. |
| GET    | `/exams/{id}/review/attempts`                                    | student | The caller's own sitting numbers at an exam — every seq that carries answers or a mark, ascending. Own-scoped review view; opens once the teacher enables review and has marked the caller, and closes again (409) while the caller can still sit the exam. |
| GET    | `/exams/{id}/review/attempts/{seq}/answers`                      | student | One of the caller's own sittings, judged — the `seq`th attempt's answers, drawing refs, correctness flags, and auto-score suggestion. Own-scoped review view; 409 while the caller can still sit the exam, so a retake can't read its own correctness off an earlier seq. |
| GET    | `/exams/{id}/review/attempts/{seq}/answers/{qid}/image`          | student | The caller's own drawn-answer bytes for one of their sittings — the seq-scoped, own-scoped mirror of the grader's drawing read. Same 409 while a sitting is still available. |
| GET    | `/exams/{id}/review/questions`                                   | student | The exam's full question list, `correct` choice ids included — the answer key the caller reviews their own sheet against. Same review gate as the other self-review reads; revealing `correct` is the point (the gate already proves the caller was marked and can no longer sit the exam). Paged via `?limit=&offset=`. |
| GET    | `/exams/{id}/statistics`                                         | teacher | Summary statistics for an exam's graded results. Requires teacher+ and management rights over an instance the exam is addressed to. |
| GET    | `/exams/{id}/students/{user}/attempts`                           | teacher | The sitting numbers a student has left at an exam — every seq that carries answers or a mark, ascending. Requires teacher+ and management rights over the exam's instance. Drives the FE's attempt-by-attempt picker. |
| GET    | `/exams/{id}/students/{user}/attempts/{seq}/answers`             | teacher | One prior sitting's judged answer sheet — the `seq`th attempt's answers, drawing refs, correctness flags, and auto-score suggestion. Requires teacher+ and management rights over the exam's instance. Serves an empty sheet for a seq the student never wrote in. |
| GET    | `/exams/{id}/students/{user}/attempts/{seq}/answers/{qid}/image` | teacher | A prior sitting's drawn-answer bytes. Requires teacher+ and management rights over the exam's instance — the seq-scoped mirror of the grader's latest-sitting drawing read. |
| GET    | `/exams/{id}/students/{user}/marks`                              | teacher | A student's full mark history at an exam — every sitting's mark, oldest first (the grade-of-record is the latest). Requires teacher+ and management rights over the exam's instance. |
| GET    | `/health`                                                        | no      | Health probe: the database verdict and the AI bridge, `503` when degraded. |
| GET    | `/homework`                                                      | student | List the homework across the caller's instances — their "my homework" view — paged via `?limit=&offset=` (omit `limit` for all of it). Manager+ see every instance's homework; a teacher sees the homework of instances they run; a student sees only the homework they are assigned (whole-roster ones plus any subset that names them, each with its `assigned` narrowed to themselves). Returns a `{items, total, limit, offset}` envelope. |
| GET    | `/homework/report/{user}`                                        | teacher | A student's homework report across the instances their şube carries, paged via `?limit=&offset=` (omit `limit` for all of it): one row per homework in their audience — submitted/late/missing state plus the grade once one exists. Statuses and marks, never the submitted files (observers get the report, not the bytes). Requires teacher+, or a parent linked to the target student. Managers, admins, and parents see every instance; a teacher sees only the target's instances they manage. Returns a `{items, total, limit, offset}` envelope. |
| GET    | `/homework/{id}`                                                 | student | Fetch a single homework by id. Visible to whoever can view its instance (its enrolled students, its teachers, its şube's homeroom teacher, and managers/admins). A student the homework is *not* assigned to gets a 404 — the same no-leak an unseen exam draft gets, so a subset assignment never reveals itself to the students left out of it. To a caller without instance-management rights the `assigned` subset comes back narrowed to their own id: being named is theirs to know, the rest of the roster is not. |
| PATCH  | `/homework/{id}`                                                 | teacher | Edit a homework's title, description, due date, subject, or assigned subset. Requires teacher+ and management rights over its instance. Omitted fields keep their value; a newly set `due_at` is re-checked against now and a new `subject_id` re-checked against the instance's course. Narrowing `assigned` is refused (409) while it would orphan an existing submission or result. |
| DELETE | `/homework/{id}`                                                 | teacher | Delete a homework and everything under it — submissions, their files, and results — then unlink the file blobs from disk. Requires teacher+ and management rights over its instance. The cascade is one transaction whose homework-row lock keeps a submission from landing under the homework mid-delete; the blob names are collected inside that transaction, before the rows are wiped, and removed after, so a crash in between strands at worst an unreachable file. |
| GET    | `/homework/{id}/result`                                          | student | The caller's own grade for a homework. Any authenticated user may read their own; `404` while ungraded (or when the homework doesn't exist). This is the one read a student graded `missing` *without* ever submitting has — their submission endpoints 404 while nothing is submitted. |
| POST   | `/homework/{id}/results`                                         | teacher | Record (or overwrite) a student's grade for a homework: a status (`done`/`incomplete`/`missing`) plus an optional 0–100 mark. Requires teacher+ and management rights over the homework's instance; the target must be a live student, enrolled in the instance, and in the homework's audience. Nobody grades themselves. Grading before the due date, or before any submission exists (`missing` for work never handed in), is allowed. A stored grade freezes the student's submission until it is removed. |
| DELETE | `/homework/{id}/results/{user}`                                  | teacher | Remove a student's grade from a homework — un-grading, which unfreezes the student's submission and files for further edits. Requires teacher+ and management rights over the homework's instance. |
| GET    | `/homework/{id}/submission`                                      | student | Read the caller's own submission to a homework: their text, files, the computed late flag, and the grade if one exists. Same visibility gates as submitting (student, enrolled, assigned). `404` until they have submitted. |
| POST   | `/homework/{id}/submission`                                      | student | Submit (or re-submit) the caller's own work for a homework: optional text, files added separately. Requires the student role, enrollment in the instance, and that the homework is assigned to the caller (a subset it doesn't name 404s, never leaking the assignment). Text replaces the previous text; the first-submit stamp is pinned once and `updated_at` moves to now. `201` on the first submit, `200` on a later edit. Refused (409) once the work is graded — ask the teacher to remove the grade to reopen it. |
| DELETE | `/homework/{id}/submission`                                      | student | Withdraw the caller's own submission — its text, its file rows, and their blobs. Same visibility gates as submitting. Refused (409) once the work is graded. The file rows fall in one transaction with the submission (children first); their blob names are collected before the wipe and unlinked after. |
| POST   | `/homework/{id}/submission/files`                                | student | Attach a file to the caller's own submission. `multipart/form-data` with the bytes under a `file` field (its `filename` required); any content type, at most the school's `max_file_bytes`, up to 10 files per submission. Same visibility gates as submitting. A submission need not exist first — a photo-only homework never types text, so this auto-creates an empty submission to hang the file off (an existing one's text is preserved). Adding a file re-stamps the submission's `updated_at`. Refused (409) once graded, or once the 10-file cap is reached. |
| GET    | `/homework/{id}/submission/files/{fid}`                          | student | Download a submission file's bytes. Two callers, one handler: a student reads their own file (behind the submission gate), or a teacher who manages the homework's instance reads any file under it. A parent never reaches here — observers get the report, never the bytes. The file is scoped to the homework in the path, so a managed homework's id can't be used to pull a file from another one. |
| DELETE | `/homework/{id}/submission/files/{fid}`                          | student | Remove a file from the caller's own submission — row first, then its blob. Same visibility gates as submitting. Refused (409) once graded. Removing a file re-stamps the submission's `updated_at`. |
| GET    | `/homework/{id}/submissions`                                     | teacher | The teacher's roster for a homework, paged via `?limit=&offset=` (omit `limit` for all of it): one row per student in the audience — the assigned subset, or every student currently enrolled in the instance when the homework carries none — plus any student outside it who still owns a submission or grade (an unenrollment or an audience change leaves work behind; it stays visible here, flagged). Each row carries the submission with its files and computed late flag, the grade, a computed `missing`, and a computed `unenrolled`. Requires teacher+ and management rights over the homework's instance. Returns a `{items, total, limit, offset}` envelope. |
| GET    | `/instances/me`                                                  | student | The instances the caller may act in or see, paged via `?limit=&offset=` (omit `limit` for all of them), newest first; returns a `{items, total, limit, offset}` envelope. The route a student reads to find the courses their section is being taught, and a teacher the ones they run. |
| GET    | `/instances/{id}`                                                | student | Fetch one instance by id. Visible to its enrolled students, its assigned teachers, its şube's homeroom teacher, and managers/admins. |
| PATCH  | `/instances/{id}`                                                | teacher | Update one instance's own policy. Requires teacher+ and a right over this instance: manager+, one of its assigned teachers, or its şube's homeroom teacher. Omitted fields keep their value; both are non-clearable. |
| GET    | `/instances/{id}/enrollments`                                    | teacher | List this instance's roster, paged via `?limit=&offset=` (omit `limit` for the whole roster). Requires teacher+ and a right over the instance — students see their own instances via `GET /instances/me`. Returns a `{items, total, limit, offset}` envelope. |
| POST   | `/instances/{id}/enrollments`                                    | teacher | Enroll a student into this instance (idempotent upsert). Requires teacher+ and a right over the instance. Only students can be enrolled — enrollment is student membership, and it gates sitting exams, being graded, and the roster. The row is hand-placed (`source` `null`), so no şube sweep can take it back. |
| DELETE | `/instances/{id}/enrollments/{user}`                             | teacher | Unenroll a student from this instance. Requires teacher+ and a right over the instance. Existing exam results are kept (they disappear from the student's marks report until re-enrolled). |
| GET    | `/instances/{id}/exams`                                          | student | List one instance's exams, paged via `?limit=&offset=` (omit `limit` for all of them). Visible to the instance's enrolled students, its teachers, its şube's homeroom teacher, and managers/admins — but drafts appear only to the instance's managers. Returns a `{items, total, limit, offset}` envelope. |
| POST   | `/instances/{id}/exams`                                          | teacher | Create an exam inside one instance. Requires teacher+ and a right over the instance; the exam's marks count into the instance's average with its kind's weight (`GET /settings`) and into the dönem's karne named by `term`. Omit `mode` for an offline-graded exam nobody can sit; `sync`/`async` take a window (async also `duration_ms`), `open` is sittable anytime with an optional per-attempt `duration_ms`. `max_attempts` (default 1, `0` = unlimited) meters retakes and `allow_rejoin` (default `true`) is the exam-room door — both stay editable while the exam runs. Send `draft: true` to keep the exam private while it's still being written: only the instance's managers see it, and sitting and grading are blocked until it's published (`PATCH` `draft: false`). Only a class-delivered course (`kind` `course`) carries exams. |
| GET    | `/instances/{id}/homework`                                       | student | List one instance's homework, newest first, paged via `?limit=&offset=` (omit `limit` for all of it). Visible to the instance's enrolled students, its teachers, and managers/admins — but a student sees only the homework they are assigned (whole-roster ones plus subsets that name them, each with its `assigned` narrowed to themselves). Returns a `{items, total, limit, offset}` envelope. |
| POST   | `/instances/{id}/homework`                                       | teacher | Assign homework inside one instance. Requires teacher+ and a right over the instance. The homework is tagged with one of the catalog course's subjects and given a future `due_at`; `assigned` optionally narrows it to a subset of the enrolled students (omit or empty = the whole roster). |
| GET    | `/instances/{id}/sessions`                                       | student | List one instance's lesson sessions, most recent first, paged via `?limit=&offset=` (omit `limit` for all of them). Visible to the instance's enrolled students, its teachers, and managers/admins. Returns a `{items, total, limit, offset}` envelope. |
| POST   | `/instances/{id}/sessions`                                       | teacher | Create a lesson session inside one instance. Requires teacher+ and a right over the instance. The session's teacher defaults to the caller. |
| POST   | `/instances/{id}/teachers`                                       | manager | Assign a teacher to this instance (idempotent). Manager+ only — staffing is the office's call. The assignee must already hold the `teacher` role or higher; the assignment gives them full management of the instance (exams, sessions, homework, roster, grading) but they keep no catalog rights over the course itself. |
| DELETE | `/instances/{id}/teachers/{user}`                                | manager | Unassign a teacher from this instance. Manager+ only. The instance, its exams, sessions, and roster are untouched — the teacher just loses their management rights over it. A user who was never assigned is a 404. |
| GET    | `/limits`                                                        | no      | Every fixed validation bound the API enforces. Unauthenticated: the registration and login forms need the username and password bounds before a session exists, and none of these values are secrets — they are the same rules a 400 would spell out. |
| GET    | `/marks/karne`                                                   | student | The current user's karne for one dönem: every instance of their şubeler that counts toward the karne, the `ders_saati`-weighted average across them, and the verdict. An archived dönem serves the snapshot the school froze when it was closed; an open one computes live. |
| GET    | `/marks/karne/{user}`                                            | teacher | Any user's karne for one dönem. Requires teacher+, or a parent tied to the target student. A linked parent and manager+ read the whole karne; an exactly-teacher caller sees only the lines of the instances they run, with the dönem average recomputed over those and no verdict (see [`narrow_karne`]). |
| GET    | `/marks/me`                                                      | student | The current user's mark report: every instance they sit, with its graded exams, weighted instance averages, and the overall average. |
| GET    | `/marks/{user}`                                                  | teacher | Any user's mark report. Requires teacher+, or a parent tied to the target student. Managers, admins, and parents see every instance; a teacher sees only the target's instances they run — the rest of the report (other sections) stays out of reach. |
| GET    | `/meals/attendance/{user}`                                       | teacher | One student's meal-attendance history, newest mark first. Requires teacher+, or a parent linked to them — the caller's own id always passes, like the balance and ledger reads. Narrow to a date range with `?from=&to=` (inclusive `YYYY-MM-DD` bounds on the menu's day, each held to the same real-calendar-day rule the menu's own date is). Paged via `?limit=&offset=`. |
| GET    | `/meals/balance/me`                                              | student | What the caller owes or has on account, in minor units (negative = owes). |
| GET    | `/meals/balance/{user}`                                          | manager | One student's meal balance. Requires manager+, or a parent link to them — the caller's own id always passes. A teacher gets a `403`: canteen debt is family debt, gated exactly like `/payments`. |
| GET    | `/meals/bookings/me`                                             | student | The caller's own bookings, newest first: the seats held *for* them, plus — for a parent — the seats held for every student they currently hold a link to. The links are re-read on every call, so an unlinked parent stops seeing the child's meals at once, even the ones they booked themselves. Cancelled bookings stay in the list, flagged. Paged via `?limit=&offset=` (omit `limit` for all of them); returns a `{items, total, limit, offset}` envelope. |
| DELETE | `/meals/bookings/{bid}`                                          | student | Give the seat back. The row survives, flipped to `cancelled` — the seat is free for someone else, and the cancellation stays auditable. Only the student it is for, or their parent, **or any manager+**, may cancel it — and for the student and their parent it is refused (`409`) once the school's `meal_cancel_cutoff_minutes` has closed the meal, a cutoff a manager+ is not bound by. Idempotent: cancelling again is a `200` that replays the refund. `409` too when the menu's date is not a real calendar day, since no cutoff can be worked out from one, and when the seat was booked again while the call ran, since only the attempt it read is ever released. |
| POST   | `/meals/credits`                                                 | admin   | Record money received from a student. **Admin only** — not manager: writing down cash is the highest-trust action in the app. The target must be a student, or anyone who already carries meal-ledger lines — a debt survives its debtor's role change, and it has to stay settleable. Appends a `credit` line; nothing in the ledger is ever edited or removed, so an over-credit is corrected by a compensating line, not by a fix-up. |
| PATCH  | `/meals/dishes/{did}`                                            | manager | Edit a dish. Requires manager+. Omitted fields keep their value; `"description": null` clears it, and `tags` replaces the whole list. |
| DELETE | `/meals/dishes/{did}`                                            | manager | Remove a dish from its menu. Requires manager+. |
| GET    | `/meals/ledger/{user}`                                           | manager | One student's statement, newest line first: every charge, credit, and reversal. Nothing here is ever edited or deleted — a correction is another line. Same gate as the balance. Paged via `?limit=&offset=`. |
| GET    | `/meals/menus`                                                   | student | List published menus, newest day first, each with its dishes. Any authenticated user. Narrow to a date range with `?from=&to=` (inclusive, `YYYY-MM-DD`, each bound held to the same real-calendar-day rule a menu's own date is). Paged via `?limit=&offset=` (omit `limit` for the full list); returns a `{items, total, limit, offset}` envelope. |
| POST   | `/meals/menus`                                                   | manager | Publish a menu for one day and meal slot. Requires manager+. `date` is `YYYY-MM-DD` text and must be a real calendar day (`2026-02-29` is a `400`: a menu on a day that does not exist has no serving instant, so no booking cutoff). The slot must be one the school currently serves and may not contain `/ \ ? # %` (it becomes part of the menu's URL id). A day+slot already published is a `409` — edit that menu instead of publishing a second one. |
| GET    | `/meals/menus/{id}`                                              | student | Fetch one menu with its dishes. |
| PATCH  | `/meals/menus/{id}`                                              | manager | Change a menu's seat cap. Requires manager+. `date` and `slot` are immutable; `"capacity": null` goes back to uncapped. |
| DELETE | `/meals/menus/{id}`                                              | manager | Unpublish a menu. Requires manager+. Its dishes go with it — a dish has no meaning apart from the menu it was published on. A menu somebody still holds a seat on is a `409`: cancel the bookings first. |
| GET    | `/meals/menus/{id}/attendance`                                   | teacher | Who ate off one menu. Requires teacher+. Paged via `?limit=&offset=`. |
| POST   | `/meals/menus/{id}/attendance`                                   | teacher | Record whether someone ate off a menu. Requires teacher+ — whoever stands at the canteen door marks, students never mark themselves. One row per (menu, person), so re-marking corrects the row rather than adding another. |
| GET    | `/meals/menus/{id}/bookings`                                     | manager | Who is eating: every booking on one menu, cancelled ones included so the kitchen can see what changed. Requires manager+ — this is the whole school's list, not one family's. Paged via `?limit=&offset=`. |
| POST   | `/meals/menus/{id}/bookings`                                     | student | Take a seat on a published menu. A student books for themselves (omit `student_id`); a parent books for a linked student by naming them. Booking twice is the same seat, not a second one — and bills once, at the price the menu carried when the seat was first taken. Refused (`409`) when the menu is full, when its **day has already passed** (a seat taken then would bill a meal nobody can be served — and no role bypasses that, since only students and their parents book at all), or when the school's `meal_cancel_cutoff_minutes` has closed the meal, and (`400`) when the menu's dishes sum past the chargeable maximum, since a seat is never handed out unbilled. |
| POST   | `/meals/menus/{id}/dishes`                                       | manager | Add a dish to a menu. Requires manager+. Tags must come from the school's `dietary_tags`, and a menu holds at most 50 dishes (`409` at the cap). |
| GET    | `/meals/profiles/me`                                             | student | The caller's own dietary profile — what the school recorded about their diet. Empty tags mean nothing was ever recorded. |
| GET    | `/meals/profiles/{user}`                                         | teacher | One student's dietary profile. Requires teacher+, or a parent link to them — the caller's own id always passes. |
| PATCH  | `/meals/profiles/{user}`                                         | manager | Record what a student may not eat. **Manager+**, deliberately: an allergen list is a safety record the school keeps on the student's behalf, not a self-service preference — a student editing their own would let a mistyped (or removed) allergy reach the kitchen with the school's authority behind it. Omitted fields keep their value; `tags` replaces the whole list, and `"note": null` clears the note. First write creates the row. |
| GET    | `/messages`                                                      | student | List one of the caller's folders, newest first: `inbox` (default), `sent`, `archive`, or `trash` (`?folder=`). `?read=` narrows by the read flag; `total` counts the filtered folder, so `?folder=inbox&read=false&limit=1` is a cheap unread badge. Paged via `?limit=&offset=`. |
| POST   | `/messages`                                                      | student | Send a message to another user. Messaging is upward only below staff: a student or parent may write to a teacher, manager or admin (parents included — messaging is the one place a parent acts), never to another student or parent; staff write to anyone. Messaging yourself is refused. The send is server-stamped and lands in the recipient's inbox unread. |
| PATCH  | `/messages/{id}`                                                 | student | Update the caller's view of a message: flip the read flag (recipient only) and/or move the caller's copy between folders. Each side files independently — archiving or trashing never touches the other party's copy. Filing into `archive`/`trash` records the folder left behind as the copy's `previous_folder`, so restoring is a move back to that value (`inbox`, or `sent` for the sender's copy, when it is `null`). Omitted fields change nothing. |
| DELETE | `/messages/{id}`                                                 | student | Permanently delete the caller's copy — allowed only from the trash (`PATCH` it to `folder: "trash"` first). The other party's copy lives on; the row disappears for good once both sides have deleted theirs. |
| GET    | `/modules`                                                       | student | What the caller's school has switched on. Any logged-in user may read it — it is what the client needs to draw its own navigation, not a privileged fact. |
| GET    | `/modules/catalog`                                               | no      | Every module this deployment can sell, what each one requires, and the packages they are bundled in. Unauthenticated and deploy-constant, like `GET /limits`: fetch once, cache for the session. |
| GET    | `/notes`                                                         | student | List the notes owned by the current user, newest first. Paged via `?limit=&offset=` (omit `limit` for all of them); returns a `{items, total, limit, offset}` envelope. |
| POST   | `/notes`                                                         | student | Create a note owned by the current user. |
| GET    | `/notes/{id}`                                                    | student | Fetch a single note by id (must be owned by the current user). |
| PATCH  | `/notes/{id}`                                                    | student | Update a note's title and/or content. Omitted fields keep their value. |
| DELETE | `/notes/{id}`                                                    | student | Delete a note owned by the current user, along with its files. |
| GET    | `/notes/{id}/files`                                              | student | List a note's files (metadata only), newest first. Paged via `?limit=&offset=`. |
| POST   | `/notes/{id}/files`                                              | student | Attach a file to a note owned by the current user. `multipart/form-data` with the file under a `file` field; its `filename` is required. At most 10 files per note; each file at most the school's `max_file_bytes` (settings, default 5 MiB). |
| GET    | `/notes/{id}/files/{file_id}`                                    | student | Download a note file's bytes. `Content-Type` is the one declared on upload; `Content-Disposition` carries the original filename. |
| DELETE | `/notes/{id}/files/{file_id}`                                    | student | Delete a note file (row first, then its blob). |
| GET    | `/payments/balance/me`                                           | student | What the caller owes the school (or has on account). |
| GET    | `/payments/balance/{user}`                                       | manager | One student's fee balance. Own record always; otherwise manager+ or a parent link (a teacher gets a `403`). |
| POST   | `/payments/credits`                                              | manager | Record money received against one named charge. Allocation is recorded, not inferred: a payment always says which installment it settles. Partial payments accumulate; one that would take the charge past what it is worth is a `409`. A charge that was **reversed** takes no payment either — it is no longer owed — and that `409` says so, rather than reporting money that never arrived as "paid in full". |
| GET    | `/payments/ledger/{user}`                                        | manager | One student's raw ledger, newest line first: every charge, payment, refund, and reversal. Nothing here is ever edited or deleted — a correction is another line. Own record always; otherwise manager+ or a parent link (a teacher gets a `403`). Paged via `?limit=&offset=`. |
| GET    | `/payments/plans`                                                | manager | Every fee plan, newest first. Paged via `?limit=&offset=` (omit `limit` for all of them); returns a `{items, total, limit, offset}` envelope. |
| POST   | `/payments/plans`                                                | manager | Write a fee plan: a name and the installments it is paid in. Creating one bills nobody — assigning it does. |
| GET    | `/payments/plans/{id}`                                           | manager | One fee plan with its schedule. |
| PATCH  | `/payments/plans/{id}`                                           | manager | Edit a plan's name and/or its schedule. Refused (`409`) once the plan has been assigned to anyone: those charges are frozen copies of the installments as they stood, so editing afterwards would leave the plan and the money telling different stories. Write a new plan instead. |
| DELETE | `/payments/plans/{id}`                                           | manager | Delete a plan. Refused (`409`) once it has been assigned to anyone — the charges it raised name it, and a school's financial history keeps its references. Decided as the delete is written, the same way the edit is, so a simultaneous assign can never leave a live assignment pointing at a plan that is gone. |
| GET    | `/payments/plans/{id}/assignments`                               | manager | Who is on this plan, newest first. Paged via `?limit=&offset=`; returns a `{items, total, limit, offset}` envelope. |
| POST   | `/payments/plans/{id}/assignments`                               | manager | Place a plan on students, which is what turns it into money owed: every installment is appended as a charge line right away, each with its own due date. **Replay-safe** — a student already on the plan is reported `already_assigned` and is not billed a second time, and an assignment whose charges landed only in part heals when the call is repeated. |
| POST   | `/payments/refunds`                                              | manager | Hand money back, against one named payment — how an over-payment or a payment recorded in error is returned, and the only way a mistaken *credit* is corrected (a credit is never reversed). Capped by that credit's amount. |
| POST   | `/payments/reversals`                                            | manager | Undo a line entered by mistake, for its exact amount — the line itself stays, with an opposing one appended beside it. Only a `charge` or a `refund` may be reversed (`400` otherwise): a mistaken payment is corrected with a refund, so money leaving the school is always spelled the one way. **Idempotent** — a line has at most one reversal, however often the call is retried. |
| GET    | `/payments/statement/me`                                         | student | The caller's own statement. The per-charge rows are paged via `?limit=&offset=` (omit `limit` for all of them); `balance_minor` is the same on every page, because the fold behind it reads every line. |
| GET    | `/payments/statement/{user}`                                     | manager | One student's statement: a row per charge with what it collected, what went back out, what is still owed, and whether it is overdue. Own record always; otherwise manager+ or a parent link (a teacher gets a `403`). The rows are paged via `?limit=&offset=`; `balance_minor` is the same on every page. |
| GET    | `/podcast/audio`                                                 | student | Stream one produced audio file. `path` is the `audio_id` `podcast.result` answered, resolved under **this caller's school** output directory — the same directory the service's `PODCAST_OUTPUT_ROOT` points at — and streamed in 64 KiB chunks, so a long episode costs the backend a bounded buffer rather than its whole length in memory. |
| POST   | `/podcast/jobs`                                                  | student | Start one podcast job. Answers `202` with the service's receipt the moment the service accepts it; the pipeline then runs in the service's own worker pool, so poll `GET /jobs/{id}` for progress. |
| GET    | `/podcast/jobs/{id}`                                             | student | One job's current state. Poll this; the answer is the service's own snapshot. |
| POST   | `/podcast/jobs/{id}/cancel`                                      | student | Cancel one job. `cancelled` says whether *this call* stopped work — a job that had already finished, or was already cancelled, answers `false` and is not an error. |
| GET    | `/podcast/jobs/{id}/result`                                      | student | A finished job's artifacts — above all the `audio_id` the audio door streams. A job that has not finished yet is refused by the service with `not_ready` (409); one it has never heard of with `not_found` (404). |
| POST   | `/pomodoro/finish`                                               | student | Finish the running pomodoro session, closing it with a server-stamped instant (`409` when nothing is running); answers `counted` — whether it moved the badge counters (ran at least `pomodoro.min_counted_ms`, within that UTC day's `pomodoro.max_counted_per_day`). |
| GET    | `/pomodoro/me`                                                   | student | The caller's own pomodoro log, newest first — the running session (if any) included (`finished_at: null`) — plus `total_focus_ms`, the unpaged sum of finished-session durations. Paged via `?limit=&offset=` (omit `limit` for the whole log). |
| POST   | `/pomodoro/start`                                                | student | Start a pomodoro focus session. Students only. The instant is stamped by the server clock — clients never supply it. Always succeeds for a student: a dangling unfinished session (the browser died mid-timer) is discarded and replaced, so there is no way to lock yourself out of starting. The frontend runs the visible countdown and the break rhythm; the backend records only the focus stint. The body is optional and names the stint: a `label` rides the response and every log it lists in — a blank or absent body starts an unnamed stint, an over-long label is `400`. |
| GET    | `/pomodoro/{user}`                                               | teacher | A student's pomodoro log, newest first, with `total_focus_ms` — the same shape as `/me`. Requires teacher+ (study oversight), or a parent tied to the target student. Paged via `?limit=&offset=`. |
| GET    | `/questions`                                                     | student | The question pool, newest first. Everyone (parents excepted) sees every `approved` question; `pending` ones appear only to their asker and to teacher+ — so for a teacher, `?status=pending` is the approval queue. Paged via `?limit=&offset=`. |
| POST   | `/questions`                                                     | student | Ask a question. Students only — the pool exists for students to get help; staff answer, they don't ask. Born `pending`: invisible to the school until a teacher+ approves it, so nothing unmoderated ever reaches the pool. Attach a photo of the problem afterwards via `POST /questions/{id}/image` (only while pending). |
| GET    | `/questions/{id}`                                                | student | One question. Approved questions are school-wide; a pending one 404s for everyone but its asker and teacher+. |
| DELETE | `/questions/{id}`                                                | student | Delete a question — the asker withdrawing their own, or teacher+ moderating (this is also how a pending question is rejected). Takes the question's solutions and every image blob — its own and its solutions' — with it. |
| POST   | `/questions/{id}/approve`                                        | teacher | Approve a pending question into the school-wide pool (teacher+), stamping the approver. One-way: approved content is frozen, and there is no "rejected" state — to turn a question down, delete it. |
| GET    | `/questions/{id}/image`                                          | student | The question's photo bytes. Access follows the question itself: approved → school-wide, pending → asker and teacher+ only. |
| POST   | `/questions/{id}/image`                                          | student | Attach (or replace) the question's photo. Asker only, while the question is still `pending` — approval freezes content, image included. `multipart/form-data` with the image under a `file` field; the declared content type must be `image/png`, `image/jpeg`, `image/webp`, or `image/gif` (rasters only — no SVG), the bytes at most the school's `max_file_bytes` (settings). |
| DELETE | `/questions/{id}/image`                                          | student | Remove the question's photo. Asker only, while still `pending` — after approval the content (image included) is frozen. |
| GET    | `/questions/{id}/solutions`                                      | student | The question's solutions, oldest first (a discussion reads downward). Visibility follows the question. Paged via `?limit=&offset=`. |
| POST   | `/questions/{id}/solutions`                                      | student | Offer a solution on an approved question. Anyone in the school may — students and staff alike (parents stay read-out). Pending questions take no solutions (409 for those who can see them, 404 for everyone else). |
| PATCH  | `/questions/{id}/solutions/{sid}`                                | student | Edit a solution's body — its author reworking their own answer. Author only, teacher+ included out: moderation stays delete-only (a moderator removes a bad solution, never rewrites someone else's words under their name). No freeze either — solutions are unmoderated, so editing stays open for as long as the solution lives. |
| DELETE | `/questions/{id}/solutions/{sid}`                                | student | Delete a solution — its author withdrawing it, or teacher+ moderating. Takes the solution's image blob with it. |
| GET    | `/questions/{id}/solutions/{sid}/image`                          | student | The solution photo's bytes. Access follows the question the solution hangs on — in practice school-wide, since solutions exist only on approved questions. |
| POST   | `/questions/{id}/solutions/{sid}/image`                          | student | Attach (or replace) the solution's photo. Author only — and at any time, since solutions are never frozen. `multipart/form-data` with the image under a `file` field; the declared content type must be `image/png`, `image/jpeg`, `image/webp`, or `image/gif` (rasters only — no SVG), the bytes at most the school's `max_file_bytes` (settings). |
| DELETE | `/questions/{id}/solutions/{sid}/image`                          | student | Remove the solution's photo. Author only, anytime — solutions are never frozen. |
| GET    | `/rag/threads`                                                   | student | The caller's own threads, most recently active first. Paged via `?limit=&offset=`. Nobody — no teacher, no admin — reads anyone else's. |
| POST   | `/rag/threads`                                                   | student | Start a new RAG thread, optionally named. Every authenticated role may ask — parents included. A user may keep up to the school's `max_chatbot_threads` threads *across both AI nests*; at the cap the request is refused (409) until an old thread is deleted — the cap is storage protection, not a usage quota (that is the per-minute message limit). |
| PATCH  | `/rag/threads/{id}`                                              | student | Rename a thread, or clear its name (`title: null`). Owner only; someone else's thread is a `404`, never a `403`. The edit counts as activity, so the thread moves to the top of the list. |
| DELETE | `/rag/threads/{id}`                                              | student | Delete a thread and every turn in it, permanently. Owner only; someone else's thread is a `404`, never a `403` (its existence is not leaked). |
| GET    | `/rag/threads/{id}/messages`                                     | student | The whole thread, oldest first. Paged via `?limit=&offset=`. Owner only. |
| POST   | `/rag/threads/{id}/messages`                                     | student | Ask the corpus a question: `{content}`, at most the school's `max_chatbot_message_len`. Answers `202 {message_id, status: "pending"}` the moment both rows are written — the answer itself lands later, in the reserved assistant row. `503` when no AI service offers `rag.chat`, and nothing is written; `429` + `Retry-After` over the per-user send limit. |
| GET    | `/rag/threads/{id}/messages/{mid}`                               | student | Poll one turn. The non-SSE fallback for `/stream`, reading the same row — including the projection that presents a long-stale `pending` as `failed`, so the two can never disagree about a turn's state. |
| GET    | `/rag/threads/{id}/messages/{mid}/stream`                        | student | Watch one turn as Server-Sent Events: `delta` chunks of the answer, then a single `done` carrying the finished message, or one `error`. The stream closes after `done`/`error` — one stream per turn, not per thread. |
| GET    | `/schools`                                                       | builder | Every school this deployment serves, newest first. Paged via `?limit=&offset=` (omit `limit` for the full list). |
| POST   | `/schools`                                                       | builder | Create a school: its registry row, its database, its schema, and its first admin account — one call, or none of it. |
| GET    | `/schools/{slug}`                                                | builder | One school by slug. |
| PATCH  | `/schools/{slug}`                                                | builder | Rename a school and/or flip it between `active` and `suspended`. Omitted fields keep their value — both land in one statement, so a request that carries both is one write and never half a patch. The slug itself is immutable in this cut — it is the cookie prefix and the blob directory — though it no longer names the database (the school's uuid does); a rename API is not offered yet. |
| DELETE | `/schools/{slug}`                                                | builder | Delete a school: its database, its registry row, and its uploaded files. Irreversible — suspension is the reversible door. |
| POST   | `/schools/{slug}/admin-password`                                 | builder | Reset an admin's password inside a school — the "we are locked out" call. Every session that account held is revoked with it, so a stolen cookie does not survive the reset. Works on a suspended school. |
| POST   | `/schools/{slug}/enter`                                          | builder | Enter a school as one of its admins — support access, with the school's own session cookie (`<slug>.<token>`) and no builder power inside it. |
| GET    | `/schools/{slug}/modules`                                        | builder | What a school has bought, and what is left to sell it. Works on a suspended school — entitlements are the vendor's ledger, not one of the school's doors. |
| PATCH  | `/schools/{slug}/modules`                                        | builder | Re-sell a school's whole shelf in one call: any mix of modules and packages, in either direction. Every list is optional and an empty body is a no-op. |
| POST   | `/schools/{slug}/modules/{module}`                               | builder | Sell a school one module. Idempotent: a module it already has is a `200` with the unchanged set. Refused while what the module structurally needs is off — enable those in the same `PATCH` instead. |
| DELETE | `/schools/{slug}/modules/{module}`                               | builder | Take one module back. Idempotent, and refused while a module the school still has depends on it — the mirror of the enable direction. |
| GET    | `/sessions/{id}`                                                 | student | Fetch a single session by id. Visible to the instance's enrolled students, the session's own teacher, the instance's teachers, and managers/admins. |
| PATCH  | `/sessions/{id}`                                                 | teacher | Update a session. Requires teacher+ with instance-management rights. Omitted fields keep their value; an explicit `null` clears `ends_at`. |
| DELETE | `/sessions/{id}`                                                 | teacher | Delete a session and its roll-call rows. Requires teacher+ with instance-management rights. |
| GET    | `/sessions/{id}/attendance`                                      | teacher | List a session's roll call, paged via `?limit=&offset=` (omit `limit` for the whole roster). Same rights as taking it: the session's teacher or a manager of its instance — students see their own tallies via `GET /attendance/me`. Returns a `{items, total, limit, offset}` envelope. |
| POST   | `/sessions/{id}/attendance`                                      | teacher | Record a user's roll-call state for a session. The session's teacher or a manager of its instance marks **enrolled students** (only students attend classes); marking the **session's teacher** requires manager+ (staff presence is management's call, so a teacher can't mark themselves present). Students never self-mark a lesson. |
| DELETE | `/sessions/{id}/attendance/{user}`                               | teacher | Remove a user's roll-call row from a session. Same rights as marking: the session's teacher or a manager of its instance for students, manager+ for a staff row (any target holding teacher or higher). |
| GET    | `/settings`                                                      | student | The school's current policy. Any authenticated user — clients need it to render pickers and grades. Falls back to the built-in defaults until a manager edits it. |
| PATCH  | `/settings`                                                      | manager | Update the school's policy. Requires manager+. Omitted fields keep their value; a present field replaces its list wholesale. Existing rows are untouched — a removed exam kind or status lives on in old records; only new writes are held to the new lists. Kind weights, though, apply live: mark reports read them at request time, so editing a weight re-weights every exam of that kind. For that reason a kind whose exams already carry marks cannot be dropped from the list (409) — those marks would silently re-weight to 1; an unmarked kind leaves freely, and an exam whose kind is gone counts with weight 1 but cannot be graded (409) until the kind returns — the other end of the same rule. `max_file_bytes` likewise applies at upload time only — already-stored files keep their size, and the chatbot knobs apply to the next chat request only. `meal_slots` follows the exam-kind rule: a slot a menu was already published for cannot be dropped (409), because the menu snapshotted its name. Slot names must not contain `/ \ ? # %` (400) — a menu's id carries the name into a URL — but a name already on the school's stored list is exempt, so a list written before that rule can still be edited around it. |
| GET    | `/subjects/{id}`                                                 | student | Fetch a single subject by id. Visible to whoever can view its course: its creator, a manager/admin, or anyone the course reaches. |
| PATCH  | `/subjects/{id}`                                                 | teacher | Update a subject's name or description. Requires teacher+ and management rights over its course. Omitted fields keep their value; the course link is fixed at creation. |
| DELETE | `/subjects/{id}`                                                 | teacher | Delete a subject. Requires teacher+ and management rights over its course. Refused with a 409 while any exam question or homework still references it — re-tag or delete those first, so nothing is left pointing at a subject that no longer exists. |
| GET    | `/swagger`                                                       | no      | Interactive API docs (Swagger UI) |
| GET    | `/terms`                                                         | student | List every term, newest first. Any authenticated user — students need the calendar to make sense of their courses. Paged via `?limit=&offset=` (omit `limit` for the full list); returns a `{items, total, limit, offset}` envelope. |
| POST   | `/terms`                                                         | manager | Create a dönem inside an academic year. Requires manager+. Past dates are allowed — dönemler are calendar structure, not schedules; an *archived* year refuses the new dönem (`409`), because past years take no new structure. |
| GET    | `/terms/{id}`                                                    | student | Fetch a single term by id. |
| PATCH  | `/terms/{id}`                                                    | manager | Update a term. Requires manager+. Omitted fields keep their value; the merged range must stay ordered. |
| DELETE | `/terms/{id}`                                                    | manager | Delete a dönem. Requires manager+. Refused with a 409 while anything still hangs off it — an exam filed in it, or a karne frozen for it — so a dönem is never dropped out from under marks that name it; move or delete those first. The dönem's own academic year is untouched (that is `DELETE /academic-years/{id}`, which refuses while a dönem still links it). |
| POST   | `/terms/{id}/archive`                                            | manager | Archive a dönem. Requires manager+. Archiving **freezes the karnes**: every student with a roster row under the dönem's year gets a snapshot of their report, and from then on `GET /marks/karne` serves that record instead of recomputing — a mark corrected after the fact no longer rewrites what a family holds. An archived dönem takes no edits and no delete; exams may still be created in it while its *year* is open (the archive is a record, not a wall). Idempotent — archiving an already-archived dönem answers `200` with the stamp it already had and never re-freezes. |
| POST   | `/terms/{id}/unarchive`                                          | manager | Re-open an archived term. Requires manager+. Idempotent the same way as archiving: an already-open term answers `200`. |
| GET    | `/time`                                                          | no      | Server clock: `{now}` UTC unix-millis, for a frontend to sync against. |
| GET    | `/users`                                                         | admin   | List every user with their role, newest first. Admin only. Paged: pass `?limit=&offset=` to take a window (omit `limit` for the whole list); the response is a `{items, total, limit, offset}` envelope where `total` counts every user. |
| POST   | `/users`                                                         | admin   | Create a school account directly — the school-office path for adding a student or a staff member with no self-registration and no invite. Admin only. `{username, password}` are required and `role` is optional (omitted → `student`); the row is born with its role rather than promoted into it, so a new teacher is never briefly a student. The username is a global **person** credential exactly as at `POST /auth/register`: a name new everywhere creates the person and this school's `app_user`, while a person who already exists is attached to this school only when the password matches the stored credential — a mismatch is a `409`. A username already taken in this school is a `409`, and the reserved staff-reading names (`admin`, `root`, …) are a `400`, the same policy registration holds. |
| PATCH  | `/users/me`                                                      | student | Update the caller's own personal info: name, surname, email, phone, birth date, plus the public-profile pair `display_name` and `bio` (both readable school-wide at `GET /users/{id}/profile`, unlike the contact fields). Any authenticated role. Omitted fields stay as they are; an empty string clears a field. |
| GET    | `/users/me/avatar`                                               | student | The caller's own avatar bytes. The self alias of `GET /{id}/avatar` — the static `/me/avatar` segment wins over `/{id}/avatar` in the router, so without this a client that never learned its own id gets a bodyless `405` on the obvious route. It serves the caller's own row and nothing else, so it asks no gate: the session already proves the reach. |
| POST   | `/users/me/avatar`                                               | student | Upload (or replace) the caller's own avatar. `multipart/form-data` with the image under a `file` field; the declared content type must be `image/png`, `image/jpeg`, `image/webp`, or `image/gif` (rasters only — no SVG), the bytes at most the school's `max_file_bytes` (settings). Replacing one drops the previous picture. |
| DELETE | `/users/me/avatar`                                               | student | Remove the caller's own avatar. |
| PATCH  | `/users/me/preferences`                                          | student | Update the caller's own UI preferences: `theme` (`light`/`dark`), `language` (`tr`/`en`), and `palette_color` (accent color as `#rrggbb`). Any authenticated role. Omitted fields stay as they are; an empty string clears one back to "never chose" (the client then follows the device preference). Read them back on any user response, e.g. `GET /auth/me`. |
| GET    | `/users/me/profile`                                              | student | The caller's own public profile — what everyone else sees of them. |
| GET    | `/users/me/students`                                             | parent  | The students the calling parent observes, sorted by username. Requires the `parent` role. Each entry's reports live at `GET /marks/{user}`, `GET /attendance/{user}`, and `GET /pomodoro/{user}`. Paged via `?limit=&offset=`. |
| GET    | `/users/search`                                                  | student | Find users by username or name — backs the pickers (enroll, grade, mark attendance) and, for a student or parent, the one way to find the staff member they are allowed to message. Any authenticated user may ask; a caller below teacher only ever sees the roles they may message (teacher, manager, admin), in the items *and* in `total`. A blank or omitted `q` lists everyone the caller may see — what the pickers open with; `role` narrows to one role (e.g. `role=student` for an enroll picker), and a student or parent naming a role they may not message is refused. Paged via `?limit=&offset=` like the other lists (omit `limit` for every match); returns a `{items, total, limit, offset}` envelope carrying only id/username/display name — no contact details. |
| GET    | `/users/{id}`                                                    | admin   | Fetch one user with their role and personal info. Admin only. |
| GET    | `/users/{id}/avatar`                                             | student | The avatar bytes. Same reach as the profile itself: every authenticated account, except a parent, who is limited to their own and their linked students'. |
| DELETE | `/users/{id}/avatar`                                             | admin   | Remove any user's avatar. Admin only — the moderation path: an offensive picture is a school problem, and no route deletes the account it hangs on. |
| PATCH  | `/users/{id}/preferences`                                        | admin   | Update any user's UI preferences. Admin only — everyone else manages their own through `PATCH /users/me/preferences`, which this mirrors field for field. |
| GET    | `/users/{id}/profile`                                            | student | One user's public profile: display name, bio, avatar meta, their class and course blocks, the badges they have earned, and the motivational counters. Badges carry an id and the instant they were first earned; their labels and icons come from the `badges` catalog at `GET /limits`. Readable by every authenticated account — except a parent, who reads only their own and their linked students'. Never carries email, phone, or birth date; those stay on `GET /users/{id}` (admin) and `GET /auth/me`. The embedded blocks are capped at `max_profile_classes` / `max_profile_courses` (see `GET /limits`) — the full paged lists are `GET /classes/me` and `GET /courses/me`. The course block is also cut to what the *reader* may already see: only courses they would pass `GET /courses/{id}` on. The class block holds the same bar as `GET /classes/user/{id}` — teacher+, a parent linked to this student, or the owner themselves; every other reader gets an empty `classes` array rather than a 403. Every number in `stats` stays the owner's true total either way, including the ones whose underlying reports are gated (`/pomodoro/{id}`, `/marks/user/{id}`, `/attendance/user/{id}`): they are motivational counters, and a magnitude names no course, class, lesson or exam. |
| PATCH  | `/users/{id}/profile`                                            | admin   | Update any user's personal info. Admin only — the school-office path for maintaining records on behalf of students and staff. Same field semantics as `PATCH /users/me`. |
| PATCH  | `/users/{id}/role`                                               | admin   | Set a user's role. Admin only. An admin cannot change their own role, and the school's **last** admin cannot be demoted by anyone (`409`) — together those keep role management from locking everyone out, including when two admins demote each other at the same instant (the floor is a predicate on the role write itself, so the racing demotions serialize on row locks). A school that has already lost its admins is recovered by hand against the database, since the seed never promotes. Setting any non-`student` role also drops the user's course enrollments — only students enroll, so a promoted user leaves every roster. Demoting below `teacher` drops their course teaching assignments for the mirror reason, and withdraws their published appointment slots, cancelling the live bookings on them: nothing could reach either afterwards — a slot is listed only on its own teacher's calendar and deleted only by a teacher+, and a booking on one is decided only by a teacher+ and cancelled only by its requester, who is refused once the window opens. The requester keeps the booking as `cancelled`, naming the ex-teacher and the reason. Demoting to `parent` additionally gives back the seats they hold on still-open event signup lists: a parent can no longer free them, and nobody else may. Seats on lists that have already closed stay as they are — that roster is history. It also takes the account off every whiteboard roster and **closes every board it created** — permanently read-only, nothing deleted. A demoted creator is a `404` on their own board, the four commands that could end it are the creator's alone, and no route lists a board the caller is not on, so the room would otherwise be commandable by nobody while its participants kept drawing on it. Closed, they keep reading the board, its history and its epochs; only writes are refused. |
| GET    | `/users/{id}/students`                                           | admin   | List the students tied to a parent account, sorted by username. Admin only — parents read their own list at `GET /users/me/students`. Paged via `?limit=&offset=` like the other lists. |
| POST   | `/users/{id}/students`                                           | admin   | Tie a student to a parent account. Admin only — family ties are school-office records, like roles. `{id}` must hold the `parent` role and `user_id` the `student` role; a parent may observe any number of students. Idempotent: linking the same pair again keeps the one tie (the `linked_by` stamp moves to the latest linker, like re-enrolling). The tie grants the parent read access to the student's marks, attendance, and pomodoro reports — nothing else, and never any write. |
| DELETE | `/users/{id}/students/{student}`                                 | admin   | Untie a student from a parent account. Admin only. The student's data is untouched — only the parent's read grant goes away. |
| POST   | `/work/check-in`                                                 | teacher | Check in for work. Requires teacher+ (staff). The instant is stamped by the server clock — clients never supply it. `409` while already checked in. |
| POST   | `/work/check-out`                                                | teacher | Check out of work, closing the open stint. Requires teacher+ (staff). The instant is stamped by the server clock. `409` when not checked in. |
| PATCH  | `/work/entries/{id}`                                             | manager | Correct a closed stint's instants. Requires manager+. An open stint can't be corrected (`409`) — it has no end yet; check out first (or delete it). |
| DELETE | `/work/entries/{id}`                                             | manager | Delete a work entry (open or closed). Requires manager+. |
| GET    | `/work/me`                                                       | teacher | The caller's own work log, newest first — the open stint (if any) included (`check_out: null`). Paged via `?limit=&offset=` (omit `limit` for the whole log). Requires teacher+ (staff). Returns a `{items, total, limit, offset}` envelope. |
| GET    | `/work/{user}`                                                   | manager | A staff member's work log, newest first, paged via `?limit=&offset=` (omit `limit` for the whole log). Requires manager+. Returns a `{items, total, limit, offset}` envelope. |

<!-- END GENERATED -->

`status` must be one of the school's attendance statuses (`GET /settings`);
the core four `present | absent | late | excused` always exist, plus whatever
the school added.
`kind` must be one of the school's exam kinds (`GET /settings`; defaults:
`yazili | sozlu | uygulama`). The kind carries the
exam's weight in the instance average — an integer `1`–`100` set per **kind** in
settings (defaults all `1`), resolved when a report is read; an exam whose
kind was later removed from settings counts with weight `1`.
A course carries **no** term — the calendar hangs off the class instead. The
academic stack is three levels: an **academic year** (`/academic-years`) holds
the terms (`/terms`), and a **class section** names the year it runs in
(`year`), so every instance under it and every exam filed in one of that year's
terms shares one calendar. On `PATCH /classes/{id}`, an omitted `year` keeps the
link and an explicit `null` clears it.
`role` ∈ `parent | student | teacher | manager | admin`. Ids in responses are
hyphenated UUIDv7s. `parent` accounts are made by an admin (register as `student`, then
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
lists and hands each seat back (`PATCH /users/{id}/role`), and a registration
racing that demotion is refused rather than left behind. Unregistering is
student-or-self, plus any seat a `parent` holds — that account can free
nothing itself, so a seat stranded before this rule still has a door. Staff
free their own, and a closed list is never rewritten. Pre-existing hand-picked (`users`)
audiences convert on boot: each listed user becomes a signup row credited to
the event's creator, and the audience becomes an uncapped registration list.
Reading a child collection of a missing parent (`/events/{id}/attendance`,
`/exams/{id}/results`, `/instances/{id}/enrollments`, `/instances/{id}/exams`,
`/instances/{id}/sessions`, `/instances/{id}/homework`,
`/courses/{id}/subjects`, `/classes/{id}/members`, `/classes/{id}/instances`,
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
rejected at `/auth/register` only — the admin account a builder names when
creating a school may still take one. `display_name` (≤ 50) and `bio` (≤ 500) ride the same two patches
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
`"name surname"` join, else `null`. Those same three steps are what **every
embedded person ref** (`{id, username, display_name}`) shows, everywhere one
appears — a message's sender, a course's teachers, a roster row, an exam result:
a person who chose a display name is shown under it school-wide, which is the
point of choosing one. It does **not** replace `name`/`surname` —
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
number would be meaningless, and a magnitude names no course (see "every
number in `stats`" below). One consequence, by design: a teacher you share
nothing with has an empty `courses` block.

**The class block is cut the same way, at the same bar `GET /classes/user/{id}`
holds**: teacher+, a parent linked to that student, or the owner reading their
own profile. Every other reader — a fellow student included — gets an empty
`classes` array, not a `403`: the rest of the profile stays public to them, but
a class name and grade are exactly what the `/classes` routes withhold below
teacher+ (`GET /classes/{id}` is teacher+, `GET /classes/user/{id}` is
teacher-or-linked-parent), and a profile must not be the way around them. The
gate is all-or-nothing rather than per class, which is the one way this block
differs from the courses one: there is no "the class we share", so a reader
either sees the owner's section list or sees none of it. `stats.classes`, like
`stats.courses`, stays the owner's true total for every reader — a magnitude
names no class.

**Every number in `stats` is the owner's true total for every reader**, and
that is a decision rather than an oversight. It is not only the two counters
above: `pomodoro_focus_ms` and `pomodoro_focus_ms_total` are the same figure
`GET /pomodoro/{id}` serves as `total_focus_ms` behind its observer gate, and
`lessons_attended_total`, `homework_submitted_total`, `homework_on_time_total`,
`exam_sat_total` and `high_mark_total` are magnitudes of the mark and
attendance data that same gate holds — for a reader who is exactly a `teacher`
they are in fact *wider* than `GET /marks/user/{id}` and
`GET /attendance/user/{id}`, which narrow to the courses that teacher manages.
They stay unnarrowed because they are motivational counters: a magnitude names
no course, no class, no lesson and no exam, and a per-reader figure would make
one profile read differently to different people, which is worse than useless
on a number whose whole job is to say "this is how much you have done". The
*named* things — the class and course blocks — keep their gates, above.

**`stats` holds sixteen numbers of two different kinds, and the difference is
worth knowing.** `pomodoro_sessions`, `pomodoro_focus_ms`, `courses` and
`classes` are **computed at read**: they are recounted from the live rows on
every call, so no column can drift out of sync with what is behind it. The
twelve `*_total` keys are **stored lifetime tallies**, maintained at write time
(see below), so a tally can outlive the rows behind it — a student's
`exam_sat_total` still counts an exam a teacher has since deleted, which is the
point of a lifetime counter. Either kind reads a true `0` on a fresh account,
never `null`. Only *finished* pomodoro stints count on either side — a timer
left running is not study time — and on the stored side only the ones that
**counted** (long enough, within the day's quota; see "Pomodoro" below), so
`pomodoro_sessions` may legitimately run ahead of `pomodoro_finished_total`:
the first is the student's whole log, the second what the badges read. Exam averages, homework-done rates and
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
  own submission decrements both**, and it exists because withdrawal is
  student-callable: without it, submit/delete/submit farms one homework into
  fifty. It is the same reason the two counters below come down — the account
  credited is the account that can delete what earned it, which is the only
  shape in which a counter ever decreases here. Editing an existing submission moves
  nothing, so the on-time verdict is fixed at the first hand-in and attaching a
  file after the deadline cannot turn an on-time submission late.
- **Exams.** `+1 exam_sat_total` the **first** time a student sits an exam —
  the counter is exams sat, not sittings. A **retake moves nothing**: it is the
  same exam again, and counting it would make the whole ladder self-serve, since
  an `open` exam with `max_attempts: 0` is a start/finish loop a student runs
  alone with no teacher in it. Resuming an attempt already running is not a new
  sitting either. Deleting an exam removes the attempt rows but does **not**
  decrement anyone: a teacher tidying up does not un-sit the exam.
- **Pomodoro.** Finishing a stint that **counts** is `+1
  pomodoro_finished_total` and its duration into `pomodoro_focus_ms_total`. An
  open stint counts for neither — unfinished focus has no honest duration to
  add. A stint counts when it ran at least `pomodoro.min_counted_ms` (5 minutes,
  on `GET /limits`) and is within that **UTC day's**
  `pomodoro.max_counted_per_day` (16); every further stint that day is recorded
  and listed exactly like the others and moves nothing. Without that rule the
  ladder was self-serve in the plainest way there is: `start`/`finish` is two
  requests, no second person and no elapsed time, so 200 pairs — about ninety
  seconds inside the rate limit — bought `pomodoro_finished_10`, `_50` and
  `_200` permanently. The verdict is stamped on the stint itself and handed
  back as `counted`, so a client can say what a session was worth instead of
  guessing why a badge did not arrive, and a threshold moved in a later deploy
  never re-judges an old stint.
- **Study streak.** Finishing a stint that counts also extends
  `study_streak_total`, the **longest** run of consecutive days on which the
  student had one — a day bought with an instant round-trip is not a day
  studied, so the same rule gates this counter as the two above. A
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
  **Deleting the grade gives it back** — the grader who deletes is the grader
  who was credited, so without the refund grade → un-grade → regrade would count
  the same work twice, then fifty times. Deleting the *exam* (or homework, or
  course) does not: a cascade is not an un-grade.
- **High marks.** `+1 high_mark_total` for the **student** whenever an exam
  mark lands at or above `badges.high_mark_min` (90, published at `/limits`),
  once per sitting — a retake is another sitting and can earn another. Unlike
  the grader's counter, a **regrade does move it**, in whichever direction it
  crossed the line: it counts marks that are *stored* at or above the cut, not
  first gradings, so 40 corrected to 95 earns it and 95 corrected to 40 hands it
  straight back (a correction that stays on one side of the line moves nothing).
  It has to work that way, because the refund below reads the stored mark — the
  two decided off different values, and a mark walked across the line left a
  credit no delete could find to give back. Homework
  marks never count here: a homework mark is optional and most grades are
  status-only, so counting them would reward a teacher's habit rather than a
  student's work. The cut is compiled in rather than read from the school's
  grade bands, which are renameable display labels — `high_mark_10` has to mean
  the same thing in every school, forever. Deleting the mark gives it back, on
  the same terms as the grader's counter above.
- **Lessons held.** `+1 lessons_held_total` for the **session's teacher**, once
  per lesson, credited by the **first roll call taken at or after the lesson's
  own `starts_at`**. It means "a lesson whose roll call was taken", not "a
  lesson on the timetable" — scheduling two hundred lessons and cancelling them
  all earns nothing, and neither does marking two hundred lessons that have not
  started yet. Roll call itself is never refused early; it simply holds nothing
  until the lesson's time comes, and the sheet touched again after the bell
  credits then, once. The thirtieth student marked in that lesson credits
  nothing further (a stamp on the session row is the guard), and taking one
  student back off the roll does not un-hold the lesson.
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
**bookable** calendar — slots that have **not started yet**, earliest first.
That bound is `starts_at`, the same one booking enforces, so a slot already
underway is left out rather than offered for a request that could only answer
`409`; the 60-second publish-time skew grace does not widen it either. A
demotion below `teacher` **withdraws** that account's calendar outright, in the
role change's own transaction, and cancels the live bookings on it (see
`PATCH /users/{id}/role`): a slot nobody can list and nobody can delete would
otherwise sit there forever, holding a booking nobody can settle. The live-role
re-read stays as the belt for a slot an older build stranded — such a slot
drops out of the bookable list and booking one is refused with a `409`.
The list does not say whether a slot is already taken — booking a taken one
answers `409`.
Every slot carries its teacher's identity (id, username, display name), to a
parent as much as to a student: **deliberate**, and not to be tightened. A
parent's three direct routes to that identity are all shut (`GET
/users/{id}/profile` is a `403`, `/users` is admin, and `/users/search` names
staff but not *which* of them holds office hours), but a conference cannot be
booked off an anonymous calendar — this list
*is* the staff directory for the booking flow, narrowed to whoever published
bookable time. The same refs on a booking (`teacher`, `proposed_by`,
`decided_by`) read the same way.

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
  `teacher` decides nothing on it any more — and nothing is left for anyone
  else to decide either, since that demotion cancelled the bookings along with
  the slots. Refused
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
renders as a badge — "Etüt", "Sınav"; no threads). Writing is **upward only
below staff**: a student or parent may write to a teacher, manager or admin
(student to teacher, parent to teacher), never to another student or parent —
that is a `403`. Staff write to anyone, in any direction (teacher to student).
Messaging yourself is refused, and only new sends are gated: student↔student
rows sent before the rule stay readable and filable. `GET /users/search` is
how a student or parent finds the staff member to write to — it shows them
staff and nobody else (issue #24). A single stored message serves both parties,
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
uuid.

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

Academic structure is data too, in three levels. An **academic year**
(`/academic-years`) carries the school's year — a name, a date range, the
sınıf-geçme policy (`grade_promotions`) the next rollover applies, and, once
you ask for one, `POST /academic-years/{id}/rollover` plants the previous
year's şubeler into it. When the year itself is done,
`POST /academic-years/{id}/archive` (manager+) closes it: `archived_at` is
stamped, the year turns read-only — no new şube, no new dönem, no new exam
inside it, and no edit or delete of the year itself — and the call is
idempotent, so a second archive answers `200` with the stamp that already
stood. There is deliberately no unarchive: a closed year stays the record of
what was taught, and re-opening is what the dönem's own `archive`/`unarchive`
pair is for. A **class section** names the year it runs in (`year`),
and **terms** (`/terms`) hang off that year: semester, trimester and quarter
systems are all just rows with a name, a date range and their `year`.
A year refuses deletion with a `409` while any şube or dönem still belongs to
it, and a term refuses it while any exam is filed in it — move or delete those
first, so the calendar never disappears
under the structure. Term dates may lie in the past,
deliberately: a school adopting the app mid-year backfills its calendar —
unlike exam/lesson/event times, which reject backdating.

A finished year is closed with `POST /terms/{id}/archive` and reopened with
`POST /terms/{id}/unarchive` (manager+). Both are idempotent: archiving an
already-archived term answers `200` with the original `archived_at` stamp, and
unarchiving an open one answers `200` as well. While a term is archived it is
read-only — `PATCH`/`DELETE /terms/{id}` are refused, a new course or class may
not link to it, and every write to a course or class already on it, or to
anything hanging off them, is refused too: enrollment, teacher assignment,
class membership and homeroom, class↔course attach/detach and blueprint apply,
sessions and roll call, subjects, exams with their questions, images, attempts
and answers (the exam-room WebSocket door and its `finish` frame included),
marks and grading, homework and submissions, course notes and files. Each
refusal is a `409` with `code: "term_archived"` and the message "this term is
archived — past years are read-only". Reads stay open throughout, so a past
year is still browsable, and unarchiving restores writes. Deliberately outside
the freeze: personal notes, messages, pomodoro, meals, payments, boards,
appointments, the chatbot, the question pool/bank, events (an event only aims
an audience at a course), class blueprint templates, and the staff work log.
Also exempt are the system integrity sweeps — role demotion stripping homeroom
teachers and course teacher assignments, subject delete clearing bank-question
links — which keep the store consistent rather than edit a past year. The
guard is a pre-flight read of the term row, so an archive and a write landing
in the same instant is an accepted race; see `## Concurrency model`.

What stays fixed is deliberate too: the four roles, the `0`–`100` mark scale,
validation bounds, and the UTC time policy are invariants, not preferences
(rename role labels in the frontend if a school says "principal" instead of
"manager"). Deployment knobs (ports, rate limits, the builder seed, CORS) remain
environment variables — they belong to the deployment, not to a school. One
deployment serves many schools, each in its own database (see "Multi-school
(SaaS)"), which keeps every school's data physically isolated.

## Food program: menus, dishes, bookings, attendance & the ledger

The school publishes **one menu per calendar day and meal slot** (`POST
/meals/menus`). Two shapes there are deliberate:

- **`date` is text**, `YYYY-MM-DD`, not a timestamp. "One menu per day and
  slot" is an equality test on the school's own day, and a midnight-in-millis
  day is only unique for one timezone. Fixed-width and zero-padded, so the
  text sorts chronologically — which is what the inclusive `?from=&to=` range
  filter and the newest-day-first ordering are built on. It must be a **real
  calendar day**, leap years included: `2026-02-29` is a `400`, here and as a
  `?from=&to=` bound. A menu published on a day that does not exist was not the
  inert typo it looks — it was fully actionable (seats, charges, dishes,
  attendance marks) while having **no booking or cancel cutoff at all**, since
  the deadline is counted back from an instant that day has none of. Menus
  stored on such a day before this rule refuse booking and cancelling outright
  (`409`) whenever a cutoff is configured: a deadline that cannot be worked out
  fails closed, and the menu has to be republished on a real day. That day is
  decided by **one parser**: the text is parsed by the same calendar library
  that computes the serving instant and compared back to what came in, so the
  two can never disagree. Hand-rolled digit parsing did disagree — `"+1"`
  parses as `1` for an unsigned integer, so `2026-+1-01` passed the
  calendar-day check and minted a **second** menu id for the 1st of January,
  with its own capacity and seat counter, invisible to every `?from=&to=` read
  (`+` sorts below `0`) and unbookable besides, since the serving-instant
  parser refused the very same text. Anything that does not round-trip
  identically — a sign, a missing zero — is a `400`.
- **The day must not already be over** (`400` at `POST /meals/menus`): every
  other create path in the app refuses a past date, and this one is the
  expensive omission, because a published menu is a bookable one and **a
  booking is what charges** — a menu backdated by a manager mints a real,
  permanent ledger line for food nobody can be served. It is measured against
  the *end* of the day, so today's menu publishes at any hour; the meal's own
  deadline is `meal_cancel_cutoff_minutes`, a separate and optional thing. A
  past term is a legitimate record (`/terms` exempts itself deliberately); a
  past menu is a charge.
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
  reversal, and the student stayed billed for a seat they no longer held. The
  flip **only ever releases the attempt the call read** (`WHERE status =
  'booked' AND attempt = …`, the same fence the re-booking side carries): a
  seat taken again while the cancel was in flight is a `409`, never a silent
  cancellation of somebody else's brand-new seat. Unfenced it freed that seat,
  refunded nothing (the money is keyed to the *older* attempt, already
  reversed) and burnt the new attempt's reversal id for good.
- **A cancel decides who is asking before it looks the seat up.** An
  unauthorised caller gets the same `403` for a booking that exists and one that
  never did; only a caller who may cancel it ever sees a `404`. Booking ids are
  fully derivable (`{date}_{slot}_{student}`, and both halves are readable), so
  reading the row first made the status code answer "did this student book this
  meal?" — the whole-school list at `GET /meals/menus/{id}/bookings`, which is
  manager+ on purpose, handed out one student at a time to any teacher, and to
  any peer who knew a user id.
- **A manager+ may cancel anybody's seat.** Booking is student-and-parent
  only, and cancelling used to be the same door — which left a seat nobody on
  the API could give back the moment its student was promoted to staff or its
  parent was unlinked: the menu refused its own deletion forever and the charge
  could never be reversed, since cancelling is the only route that reverses
  one. A role change deliberately sweeps no bookings on its own; moving money
  is a decision, not a side effect.
- **The capacity check is a single conditional write, decided by the
  database.** Counting rows and then writing one would be write-skew —
  a count read in one statement does not conflict-check against a concurrent
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
  enters `540` (09:00 UTC) for a meal served at noon locally. A slot with **no
  `serving_minute` has no cutoff at all**: there is no instant to count a
  deadline back from, so nothing on that slot ever closes until the school sets
  the hour. It used to count from midnight UTC of the meal's day, and that took
  the canteen offline the moment a school set the one cutoff knob — all three
  shipped slots carry no serving hour, so every same-day menu was already past
  its deadline: `POST /meals/menus/{today}/bookings` answered `409`, and the
  seats already held could no longer be cancelled by the students and parents
  holding them, only by a manager. The fallback was chosen to avoid exactly that
  and caused it.
- **The serving time is read live, not snapshotted onto the menu.** The cutoff
  minutes are already read live, so freezing the other half of the same
  deadline would make one policy edit apply and its twin not; a kitchen that
  moves lunch an hour later wants today's menus to move with it. The menu
  still snapshots the slot *name* (that is what keeps a retired slot's history
  readable), so a menu whose slot has since left the list simply has no serving
  time, and therefore no cutoff.
- **One seat may be taken at most `meal.max_booking_attempts` times** (`GET
  /limits`, currently 10) — the first booking plus the re-bookings after a
  cancel — and past that `POST /meals/menus/{id}/bookings` is a `409` naming
  what happened. This bounds *storage*, not indecision: every cycle appends two
  permanent, undeletable ledger lines (the charge and its reversal, both keyed
  to the attempt), nothing else bounded them, and every later balance read is
  answered over whatever is there. A seat that really must move again is the
  canteen's to cancel. Rows already past the ceiling — an upgrade's leftovers —
  are untouched: cancelling consults no ceiling, so such a seat and its money
  stay reachable, and only one more re-booking is refused.
- **A menu whose day has already passed takes no booking** (`409`), whatever
  the cutoff says. Refusing it at publish time is not enough on its own: a menu
  published legitimately days ago, for a day that has *since* gone by, stayed
  bookable and therefore billable forever. This is not a deadline before the
  meal — it is the day itself being over, which no setting can make untrue, so
  it holds with `meal_cancel_cutoff_minutes` unset and on a slot with no
  `serving_minute` (the shipped default, and the case that must keep working
  for **today's** menu at every hour). Nothing bypasses it, since only students
  and their parents book at all — a manager back-entering a seat is exactly how
  a bogus charge would be minted. **Cancelling stays open** on such a menu:
  money already taken has to stay reversible, and that is the same reason
  manager+ is not held to the cutoff.
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
a `404` and writes nothing. Like a dish write, the mark moves its **menu's**
revision in the same transaction, so the menu's existence is something this
write *writes* rather than something it reads and then trusts: a delete racing
it touches the very row it bumps, and the store refuses to commit both. That
matters more here than tidiness, because a menu's id is deterministic on
day+slot — a mark that outlived its menu would come back as a mark on the next
menu published for that meal. Only the *write* paths ever check the menu:
`GET /meals/attendance/{user}` filters on the student, never on the menu still
existing, so such a mark would be read back there while no write could touch it.
Its date bounds only apply when you send them, and a link to a deleted menu has
no date to compare, so `?from=` drops such a mark while `?to=` on its own still
returns it. Nothing can create one any more, and the menu delete's own
sweep is what clears the marks a menu leaves behind.

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

Those three sums are taken **by the database, one per kind**, so a balance read
costs the same on a statement of four lines and one of forty thousand; it used
to decode and fold every line, which made a growing ledger a tax on every later
read. Only the grouping moved: the signs are still applied in Rust by the one
function the formula above is spelled in, because summing `IF kind = 'charge'
THEN -amount` in SQL would fork that rule into a second language where nothing
fails the day the two disagree. A stored running total was the other option and
is a counter that can drift.

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
- **A credit is retry-safe on request**, exactly like `POST /payments/credits`:
  send an optional **`request_key`** (`[A-Za-z0-9-]`, 1–64 characters — no `_`,
  the separator inside a ledger line's id) and the line is keyed
  `<student>_k_<key>`, so a client retry after a network timeout returns the
  line the first attempt wrote instead of crediting the money twice. Without
  one the id is a fresh uuid and a resent request is a second credit, as a desk
  taking the same amount twice really is — and since nothing here edits or
  deletes a line, that doubled credit can only be corrected by a compensating
  one. The same key with a different `amount_minor` is a `409`, never the
  stored line: that is a client bug, and answering `201` would hide it. The
  student is part of the id, so one office's "receipt-114" can never land on
  another student's account.

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
no future-date rule here. A *negative* `due_at` is a `400` — that is not an
instant, and the charge it billed would read as overdue for ever.

Writing a plan bills nobody. **Assigning it does** (`POST
/payments/plans/{id}/assignments` with `{student_ids}`, at most 200 per call):
that appends *every* installment as a `charge` line right away, each carrying
its own due date. What really bounds one request is therefore the **charges it
would raise**, not the head count: `student_ids × installments` may not exceed
**3 000** (200 students up to a 15-installment plan; 50 at a time on a
60-installment one). A bigger batch is refused whole with a `400` naming the
split — nothing is written, because a money route that billed half a batch and
gave up would leave the office guessing which families were charged. There is no scheduler, no nightly sweep, and nothing that
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
term's `exam_count` (the term's own delete is one `DELETE … WHERE
exam_count = 0 AND NOT EXISTS (SELECT 1 FROM karne_snapshot WHERE term = $1)`,
with the year's reference handed back in the same statement). It used to be a `SELECT` taken before the write, which a
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
  itself stays on the record beside it. A **reversed charge is no longer owed**,
  so it takes no payment: `POST /payments/credits` against one is a `409` naming
  the *reversal* as the reason. It has to name it, because a reversal fills
  exactly the room a payment would (it is a `+amount` child of the charge, which
  is what makes the fold read as full), and a bursar told "already paid in full"
  about a charge where not one kuruş ever arrived would go hunting for money
  that does not exist.
- **A refund frees the charge's room.** The cap on a payment is folded over the
  target's whole source subtree, so refunding a payment gives that charge its
  room back and the charge **can be paid again** — and reversing that refund
  takes the room back with it. Money that came back out is not money the school
  still holds.
- **The over-payment cap is not the database's.** The `409` past a charge's or a
  credit's worth is a cross-record fold, which no constraint can enforce on
  its own; it holds because the fold, and the append it authorizes, are
  taken under the target's ancestor-chain row locks (`FOR UPDATE`) — writers
  on one subtree take turns, different subtrees never contend. Should an over-payment ever
  be recorded anyway it is not a crisis: this is human data entry at an office
  desk, the outcome is an over-paid charge that is plainly visible in the
  statement, and it is undone by appending a refund. Both lines are true
  records of money that really arrived — refusing them would be the worse lie.
- **A line carries at most 20 applied lines.** The cap above is folded one
  query per line of the target's subtree, all of them while those row locks are
  held, so a charge settled in a thousand pieces would stall every other
  payment in the school behind its own arithmetic. Past 20 (payments under a
  charge, refunds under a payment, and the reversals among them) a further
  payment or refund against that line is a `409` — an installment settled in
  more than twenty pieces is pathological, and a plan can always raise a fresh
  charge. The ceiling binds **new writes only**: a line that already carries
  more (written before the rule) still reads, still refunds through its own
  children, and is still reversible.
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
  The mark is checked **before** the flag, so a caller who has none reads `404`
  whichever way `allow_review` is set — an outsider (an unenrolled student, a
  parent) never learns from the status code whether review is on for an exam
  `GET /exams/{id}` would refuse them outright.
  All four reads are refused (`409`) while the caller can still **write** a
  sitting at that exam — one in progress, *or* one they may still start: a mark
  on sitting 1 must not open the answer key to someone who can post sitting 2
  with it in hand. Review therefore opens once `max_attempts` is used up (for
  the default `max_attempts: 1`, as soon as the single sitting is submitted or
  expires), or the exam's `ends_at` has passed, or the exam has no `mode` at all
  (offline-graded — nothing to sit, so the mark alone opens it). Consequence,
  deliberate: an `open`-mode exam with `max_attempts: 0` (unlimited) never
  closes to its students, so its review never opens — cap the attempts or give
  the exam an end to hand the key back.

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

### Ortak sınav: one exam, several sections

An exam belongs to the instance it was created on (`exam.class_course`), and
that owner's audience row is written with it — every exam read goes through
`exam_audience`, so the owner is always in the set. A **sibling instance** can
be added to that set (`POST /exams/{id}/audience` with
`{"instance": "<class_course id>"}`): the ortak sınav — one exam, one mark
per student, standing in every addressed section's exam list, marks report
and karne, so a single mark stands on each of their lines. An announcement is
also what **admits a section**: an addressed instance's students read the exam
(`GET /exams/{id}`), start and answer it (`POST /exams/{id}/attempt`, the
answer routes, the exam room), and are graded on it exactly like the owner's —
the sitting, answering and grading gates ask "enrolled in any instance the
exam is addressed to", not only the owner's roster — and `GET
`/exams/{id}/live` monitors every addressed section's sitters, not only the
owner's, and the teacher side of the exam — grading, the results and
statistics reads, the monitor — opens to a teacher of **any** addressed
instance, so the section that sits the exam also runs it there (authoring,
`PATCH`/`DELETE` and the question/image routes, stays the owner's). Two rules
make the announcement legal: the target must teach the **same catalog course**
and sit under the **same academic year** as the owner (an exam's mark has to
land in a report that teaches the subject, in the year it is sat), and the owner itself
cannot be announced to — its row is always there, and deleting the exam is
what ends it. The target's year must still be open, so announcing into an
archived year is the usual `409`. The gate is the **owner** instance's
(`manager`+, its assigned teachers, its şube's homeroom teacher): the section
that runs the exam decides who else sits it; a teacher of the receiving
section may grade there, not re-announce. `GET /exams/{id}/audience` lists the
set (owner first, then announcement order), visible exactly as the exam is;
`DELETE /exams/{id}/audience/{instance}` withdraws one — a repeat announce is
a no-op `200`, a pair that was never announced a `404`, and the owner's own
pair a `400`.

Deleting an exam (or its course) cascades attempts, questions, answers, and
question + answer images (blobs included) along with results; unenrolling mid-exam
hides the student from the monitor roster but keeps the attempt and mark
rows, mirroring the marks report. Detaching the instance that carries an exam
sweeps it the same way, and the sweep takes every audience row that names the
detaching instance — the ones addressed **to** it (an exam a sibling owns) and
the rows of the exams it **owns** (announcements out to siblings) — so an
ortak sınav never blocks a detach on a foreign key.

> **Schema changes are migrations, not boot batches.** The sqlx migrator
> applies `migrations/control` to the control database at boot and
> `migrations/school` to the school template and every newly minted school
> database, tracked in `_sqlx_migrations` — an up-to-date database boots
> without touching a row, and there is no manual backfill. The SurrealDB-era
> `surreal sql` repair notes died with the old store.

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
under a fresh uuid, metadata in the `answer_image` table, structurally identical to
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

An **instance** hands out **homework**: `POST /instances/{id}/homework` with a title,
an optional description, a **required subject** (one of the instance's course's subjects — and
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
`404` — the same no-leak a hidden exam draft gets. The *named* learn no more
than that they are named: to anyone without course-management rights the
`assigned` field comes back holding their own id alone (`null` still meaning
the whole course), because who else was assigned is roster information and the
roster itself is teacher+-only. Narrowing the subset later
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

Events cover ad-hoc gatherings; **sessions** are an instance's lessons. A session
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
**first** mark taken for a lesson **at or after its `starts_at`** credits its
teacher's `lessons_held_total`, once — a lesson counts as held when its roll
call is taken during it, so one merely scheduled (and then cancelled) counts
for nothing, marking a lesson a week early counts for nothing *yet*, and the
marks after that first credited one count for nothing further. And a **student** marked `present` or `late`
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
A finished stint carries a `counted` verdict: whether it moved the lifetime
counters the badges read. It counts when it ran at least
`pomodoro.min_counted_ms` (5 minutes) and is within that **UTC day's**
`pomodoro.max_counted_per_day` (16) — both published on `GET /limits`, both
compiled in, so they move only with a deploy. A stint that counts for nothing
is still recorded, still listed, and still sums into `total_focus_ms`: the rule
bounds what *counts*, never what is kept. It exists because finishing is
self-service — one stint is two requests, no second person and no elapsed
time — and a badge counter moved once per round-trip is farmable, permanently,
since a badge is never revoked. Five minutes is well under a conventional
25-minute pomodoro on purpose (breaking off early is still studying) and four
orders of magnitude above a scripted pair; sixteen a day is above any honest
school day while capping the minimum-length farm at eighty minutes of real
waiting a day.

Finishing a stint that counts also feeds the **study streak** behind the
`study_streak` badges: consecutive **UTC** days on which one was finished, kept
as the longest run ever held (see "Badges").

**Attendance reports** mirror the marks report: `GET /attendance/me` for any
logged-in user, `GET /attendance/{user}` for teacher+ — narrowed to the
instances the caller manages (manager+ sees every instance; event tallies are
school-wide either way). The report tallies event attendance and lesson roll
call separately, plus a per-instance breakdown and the per-dönem devamsızlık
counts:

```json
{
  "user": "01J…",
  "events":   { "present": 4, "absent": 1, "late": 0, "excused": 1, "total": 6, "rate": 0.8 },
  "sessions": { "present": 9, "absent": 2, "late": 1, "excused": 0, "total": 12, "rate": 0.8333 },
  "courses": [ { "instance": "01J…", "course": { "id": "01J…", "title": "algebra", … }, "counts": { … } } ],
  "devamsizlik": [
    { "term": "01J…", "name": "1. Dönem", "absent_days": 3, "excused_days": 1,
      "unexcused_days": 3,
      "limits": { "max_excused_days": 10, "max_unexcused_days": 20 },
      "over_limit": false }
  ]
}
```

`rate = (present + late) / (present + absent + late)`: arriving late still
counts as attending, and an excused absence counts against no one (`rate` is
`null` when every row is excused, or there are none). Per-instance blocks appear
for every instance the user has roll-call rows in — attendance is a historical
record, so unenrolling hides marks from the marks report but never hides an
absence.

**Devamsızlık (the legal absence tally).** The same report carries a
`devamsizlik` block, one entry per **term**: `absent_days` (distinct calendar
days the student has at least one `absent` roll-call row on), `excused_days`
(idem for `excused`), `unexcused_days` (the `absent` count — the two are one
bucket in the schema), the school's `limits`
(`max_excused_absent_days`, `max_unexcused_absent_days`, each `null` when
unset), and `over_limit`. Days are bucketed in the school's **timezone**
(`GET /settings`, `timezone`, default `Europe/Istanbul`), so a 23:30 and a
00:30 lesson are two days or one depending on where the school is — never on
where the server runs. Two absences on one day count once, because veli
excuses and the yönetmelik count days, not lessons. The limits are advisory
here: nothing refuses a roll-call row that pushes a student past them.

## Class sections (şube)

A **class section** is the group a school actually teaches in — 9-A, 10-B — and
it is the **academic anchor**: a student's membership, the courses it carries,
their exams and their homework all hang off it. Its row carries a name, an
optional free-text `grade` label in the school's own vocabulary (`"9"`,
`"Lise 2"`; `""` means no grade, exactly like omitting it) and the **academic
year** it runs in (`year`, nullable while a section is being prepared). A
section does not teach a course directly: attaching one mints an **instance**
(`/instances`) — its own row, with its own teachers, `ders_saati`, karne weight,
roster, exams, sessions and homework — and two sections that attach the same
course share nothing at all. Schools that run electives or a college-style
timetable simply never create one — individual enrollment and the school-scoped
course membership are untouched, and classes are a convenience.

**The homeroom teacher.** A class may also name one — the *sınıf öğretmeni* —
with `teacher_id` on create or `PATCH`, and it rides back out as a `teacher`
person block (`null` when there is none, exactly like `grade`; both `null` and
`""` clear it). The account must exist and hold **teacher, manager or admin**
(anything else is a `400` naming `teacher_id`, the same shape a non-student
member gets). It is a **teaching right**, not a label: the homeroom teacher
manages every **instance** their section carries — its hours and karne weight,
its roster, its exams, sessions, homework and grading — under the same rule an
assigned instance teacher gets (manager+, one of the instance's assigned
teachers, or the section's homeroom teacher). It is not counted as a teacher
assignment of its own and grants nothing on the catalog: instances keep their
own `teachers` list, and editing a section is still manager+. A role change
that takes the account below `teacher`
clears the column on **every** class it held, in the same sweep that drops
their instance assignments, so no section ever lists a demoted account.

That sweep runs once, over the rows that exist when it runs — so a demotion
that lands *between* a request's role check and its write would sweep nothing
and leave the assignment standing forever. Both writers close it from the other
end: `POST /classes`, `PATCH /classes/{id}` and `POST /instances/{id}/teachers`
re-read the account's live role **after** their write and answer `409` if it
has since dropped below `teacher`, taking the assignment back (a create is
rolled back whole — no half-made class is left behind). Whichever side is
second catches it; the ordinary path costs one extra read and no extra write.

**The pump.** A class holds **members** (students) and **instances** (the
courses it teaches), and owes the product of the two: every member enrolled in
every instance. So both writes push the same way — attaching a course
(`POST /classes/{id}/instances`, body `{"course_id": …}`) enrolls the whole
roster into the new instance, adding a
member (`POST /classes/{id}/members`) enrolls them into every instance the class
already carries — and what gets written is an ordinary `enrollment` row, the
same one `POST /instances/{id}/enrollments` writes, counted against the same
`enrollment_count`. Each row a class writes is tagged with it as the row's
`source`; **no `source` means placed by hand**, and that one bit is what makes
the sweeps below safe. It rides back out on every enrollment response
(`GET /instances/{id}/enrollments`) as the class's id, or `null` for a
hand-placed row — without it no client could tell which of the roster rows it
is showing a class change is about to remove.

**Already enrolled is skipped.** An (instance, student) pair that already has
an enrollment row is
left exactly as it stands — no second row, no `source` rewritten. A
student a teacher enrolled by hand *stays* hand-placed when the class later
attaches that course, so the class never quietly adopts someone else's roster
decision.

**A link to a course that no longer exists blocks the pump.** Adding a member
walks every instance the class carries, and one of them may point at a course
row that is gone: the call is a `409` and names it
(`course:<key> no longer exists — detach it from this class first`, code
`linked_course_missing`). Reporting it as anything else would send staff off to
raise a cap that does not exist — on a class every member-add now fails on.
Detach the link and the class works again.

**Removing a member ends the stint; the row stays.** `DELETE
/classes/{id}/members/{user}` is a **soft leave**: it stamps `left_at`, gives
the class its seat back (the counter counts live members only), and sweeps the
instance enrollments the class pumped for that student. The history is not
erased — a second `POST /classes/{id}/members` inserts a **fresh** row for the
same pair (the live-pair index is partial, so both can exist), and
`class_member_count` never exceeds the live rows. Removing the wrong student
from a class mid-year is therefore something the product can undo without
losing who was there when.

**Removals take back only what the class pumped.** A leave, or detaching an
instance (`DELETE /classes/{id}/instances/{instance}`) sweeps the
enrollment rows whose `source` is *this* class and no others — hand-placed
rows survive every class operation, and every swept row's seat is handed back
to its instance's counter. There is no heir to consider: a second section
teaching the same course holds its **own** instance and therefore its own
roster row, so this class's row is released outright. Sweeps also tolerate rows
that are already gone — deleting a course or detaching an instance wipes its
enrollments wholesale
while the class memberships survive — so "this class has a member" and "that
member holds a pumped row" are independent facts.

**A manual unenroll wins, permanently.** `DELETE /instances/{id}/enrollments/{user}`
on a pumped student is allowed and sticks: nothing re-pumps them while they
remain a member, because the pump runs on writes, never on a schedule. Staff
put them back by enrolling them by hand (which makes the row hand-placed) or
by detaching and re-attaching the course.

**And a manual enroll wins too, permanently.** `POST /instances/{id}/enrollments`
landing on a row a class pumped takes the row *off* that class — its `source`
is cleared, the response comes back with `"source": null` — so no later class
sweep can undo a placement an operator made on purpose. That is the mirror of
the rule above: hand-placed beats pumped in both directions, and without it
"hand-placed" was a state only a *first* enroll could ever reach.

**Delete guards.** A class still holding live members or instances refuses
deletion with a `409` ("remove its members and detach its instances first") —
the roster it owes is never dropped out from under the courses silently. A
**term** carrying exams refuses deletion, and an **academic year** refuses while
any class or term still belongs to it: the calendar never disappears under the
structure. Deleting a **course** detaches it from every class it was on (its
instances and their enrollments go with it, and the files those rows held are
unlinked), and a role change that takes a user off `student` drops their
class memberships exactly as it drops their enrollments — in one transaction,
because a membership left behind would keep pumping them back into instances,
while an enrollment left behind would stay tagged with a class the released
counters had already made deletable, and nothing could ever sweep it again.

**A link to a deleted course detaches instead of refusing.** Should a
`class_course` row ever be left pointing at a course row that is gone,
`DELETE /classes/{id}/instances/{instance}` still answers `204`: management rights
are read *off* the course, so a stale link had no readable owner and the detach
used to `404` forever — which also left the class permanently undeletable, its
attachment counter counting a row nothing could sweep. There is no roster left
to protect, and the caller is already teacher+.

**Who may.** Creating, editing and deleting a class, and adding or removing
its members, is **manager+** — a class is school structure, not classroom
work. Attaching or detaching a course takes catalog rights on **that
course** (its creator, or manager+), since the call writes that course's roster
for this section and nothing else. Editing an instance's own policy
(`PATCH /instances/{id}`) and its roster take instance rights (manager+, an
assigned teacher, or the section's homeroom teacher). Every read
(`GET /classes`, `/classes/{id}`, its members and its instances, all paged,
newest first) is **teacher+** — and "newest" here means
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
identity `GET /users` (admin-only) withholds outright, and one `/users/search`
hands a student only as the messaging directory — never as class metadata.
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
the same attach a manager's own `POST /classes/{id}/instances` does, so what
lands is ordinary `class_course` **instances** and the ordinary `enrollment`
rows each implies, and an
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
its id — a class id is a uuid and `POST /classes` is the only place it is
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
managers chasing rows no pump will ever write. `missing` names courses that
**still exist**: deleting a course takes its id out of every template holding it
(see below). Only a template row written *before* that cascade existed — an
upgraded volume, since nothing backfills — can still report a dangling id, and
the next pump over that grade prunes it.

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
`course_deleted` (the course was deleted **while the pump ran** — one deleted
before it started is already out of the template; the pump removes that id too,
so the skip is reported once for the whole grade — the sections after the first
one are not asked again — and never again on a later pump),
`class_at_course_ceiling`, `class_roster_too_large`
(the section holds more students than one attach may enroll at once),
`linked_course_missing` (an instance the section already carries points at a
course that no longer exists), and
`blueprint_deleted` (the template itself was deleted while the pump ran —
nothing was attached, and there is nothing left to retry). That last one **ends
the run**: it says nothing about the (section, course) pair it names, so every
remaining section would only repeat it. It is reported once, the sections
already stocked stay stocked, and the call still succeeds.

**The manual attaches answer the same vocabulary**, each route with its own
set. A `409` from either is
`{"error": "<sentence>", "code": "<machine code>"}` — the prose unchanged, the
code out of:

| route | codes |
| --- | --- |
| `POST /classes/{id}/instances` | `duplicate`, `class_at_course_ceiling`, `class_roster_too_large` |
| `POST /classes/{id}/members` | `duplicate`, `class_at_roster_ceiling`, `class_course_list_too_large`, `linked_course_missing` |

The three *deleted* codes are a pump's alone: on a manual attach a deleted class
or course is a `404`, and `blueprint_deleted` needs a blueprint nobody handed
it. `duplicate` (already a member / already attached) is a refusal on either
route and no skip for a pump, which asked for a row that is already in place,
and `linked_course_missing` (another course already attached to that section no
longer exists — detach it first) only `POST /classes/{id}/members` can meet,
since a member add is the one attach that walks the section's existing course
links while a pump attaches a course it has just proved alive.

The **ceiling** pair differs by route because the ceiling does: a member add is
refused by a full roster (`class_at_roster_ceiling`, the section already holds
`max_class_members`) or by a course list longer than one add may enroll at once
(`class_course_list_too_large`); a course attach by the mirror of both
(`class_at_course_ceiling`, `class_roster_too_large`) — which is also why the
pump, which only ever attaches courses, reports that second pair. One cause
therefore reads the same whether a manager hit it by hand or a pump hit it in
bulk, which is what lets a bilingual client branch and word it once. `code` is
published on those two routes only and is simply **absent** from every other
error body — with one addition since the instance model: `linked_course_missing`,
which only a member add can meet, is published on that route.

**Removal spares what a human placed.** Every instance a blueprint mints is
tagged with it. Dropping a course from the list detaches it only where the
blueprint attached it — sweeping the enrollments it pumped, repairing to a rival
class first exactly as a manual detach does, and **unlinking the files** the
detached instances' exams, homework and submissions held (the sweep returns
their blob keys, and the route unlinks them the way a manual detach does) — and
a course a human attached to
that class by hand carries no tag and is left exactly where it is. Deleting a
blueprint applies that to its whole list, and unlinks those files too.

**Deleting the course itself takes it out of every template naming it**, in the
same transaction that detaches it from the sections. A template holds its
courses as a list on its own row, so nothing else could reach them, and the id
would otherwise be permanent rather than merely stale: the pump prunes a dead id
only while walking a section, so a grade with no sections could never drop one,
and `PATCH`ing the template back as it stands — the repair this section points
at — is a `400` for naming a course that does not exist. The sweep is a scan of
`class_blueprint`, which is deliberately unindexed: the table holds one row per
grade label the school uses, and a course delete is rare.

`DELETE /classes/blueprints/{grade}` removes the row **first** and then sweeps
by that tag, rather than by the list the call read: an edit that adds a course
and pumps it while the delete runs would otherwise leave rows tagged with a
blueprint that no longer exists, and since the grade label *is* the record id,
nothing could ever reach them again. The pump carries the other half of that —
an attach whose blueprint was deleted mid-run writes nothing and is reported as
`blueprint_deleted`. The delete returns the swept subtree's blob keys and the
route unlinks them. The attach's in-transaction claim is a `FOR KEY SHARE`
row lock on `class_blueprint` — the one strength a `DELETE` of the row
cannot take — so a sourced attach that started first holds the row and the
delete waits behind it; an attach starting after the delete finds no row
and writes nothing. A pump takes that lock one course at a time, so a
delete never waits behind a whole grade. The delete is also a
compare-and-set on the list the call read: a `409` means somebody edited the
template in between, and nothing was written — though a template deleted and
recreated at the same grade with the same list satisfies that comparison, which
is accepted, since the end state is the one the caller asked for. The remaining
cost is a **process crash** between the delete and its sweep, which no lock
survives: it leaves inert tagged attachments behind, still detachable one at a
time at `DELETE /classes/{id}/instances/{instance}`, with every counter exact —
the crash window's files outlive their rows, which is the one leak the model
cannot close from here.

A blueprint names no year — the class names its own, and the year is what the
exam calendar hangs off. The grade label is the
blueprint's id, so there is one per grade (a second is a `409`), and it must be
non-empty and contain none of `/ \ ? # %`.

## Quick tour (curl)

```sh
BASE=http://127.0.0.1:7656
JAR=/tmp/hz.cookies

# SCHOOL is the slug a builder gave this school (see "Multi-school (SaaS)").
SCHOOL=demo

curl -s $BASE/auth/register -H 'content-type: application/json' \
  -d '{"school":"'$SCHOOL'","username":"ali","password":"secret1"}'

curl -s -c $JAR $BASE/auth/login -H 'content-type: application/json' \
  -d '{"school":"'$SCHOOL'","username":"ali","password":"secret1"}'

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

# the academic calendar: a year, then a term inside it (manager+)
YR=$(curl -s -b $JAR $BASE/academic-years -H 'content-type: application/json' \
  -d '{"name":"2026-2027","starts_at":1788000000000,"ends_at":1811000000000}' \
  | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
TE=$(curl -s -b $JAR $BASE/terms -H 'content-type: application/json' \
  -d '{"name":"1. Dönem","year":"'$YR'","starts_at":1788000000000,"ends_at":1794000000000}' \
  | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)

# the class section: the academic anchor, in that year. Naming yourself as the
# homeroom teacher is what gives this teacher rights over its instances (D10) —
# a manager could assign an instance teacher afterwards instead.
ME=$(curl -s -b $JAR $BASE/users/me | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
CL=$(curl -s -b $JAR $BASE/classes -H 'content-type: application/json' \
  -d '{"name":"9-A","grade":"9","year":"'$YR'","teacher_id":"'$ME'"}' \
  | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)

# the catalog course, then the instance this section teaches it in
CO=$(curl -s -b $JAR $BASE/courses -H 'content-type: application/json' \
  -d '{"title":"Matematik"}' | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
IN=$(curl -s -b $JAR $BASE/classes/$CL/instances -H 'content-type: application/json' \
  -d '{"course_id":"'$CO'"}' | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
curl -s -b $JAR $BASE/instances/$IN              # hours, karne weight, teachers

# roster: adding a member to the section enrolls them into $IN (the pump);
# a teacher can also enroll one student by hand
# find the student to enroll (here: a registered user "veli"): fragment
# search over username/name, role-narrowed (teacher+)
SID=$(curl -s -b $JAR "$BASE/users/search?q=vel&role=student" \
  | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
curl -s -b $JAR $BASE/classes/$CL/members -H 'content-type: application/json' \
  -d "{\"user_id\":\"$SID\"}"
curl -s -b $JAR $BASE/instances/$IN/enrollments   # lists the pumped row

# exams live on the instance and are filed in a dönem; weighted marks
EX=$(curl -s -b $JAR $BASE/instances/$IN/exams -H 'content-type: application/json' \
  -d '{"title":"1. Yazılı","kind":"yazili","term":"'$TE'"}' \
  | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
curl -s -b $JAR $BASE/exams/$EX/results -H 'content-type: application/json' \
  -d "{\"mark\":90,\"user_id\":\"$SID\"}"
curl -s -b $JAR $BASE/exams/$EX/statistics

# lesson sessions + roll call (instance teacher creates; the session's teacher
# or an instance manager marks the enrolled students)
SE=$(curl -s -b $JAR $BASE/instances/$IN/sessions -H 'content-type: application/json' \
  -d '{"topic":"limits","starts_at":1900000000000}' | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)
curl -s -b $JAR $BASE/sessions/$SE/attendance -H 'content-type: application/json' \
  -d "{\"status\":\"present\",\"user_id\":\"$SID\"}"

# staff work log (instants are server-stamped)
curl -s -b $JAR -X POST $BASE/work/check-in
curl -s -b $JAR -X POST $BASE/work/check-out
curl -s -b $JAR $BASE/work/me

# ...and as the student:
curl -s -b $STUDENT_JAR $BASE/instances/me        # the sections' instances
curl -s -b $STUDENT_JAR $BASE/marks/me
curl -s -b $STUDENT_JAR "$BASE/marks/karne?term=$TE"   # the report card
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

The service connects with ALPN `hab/2`, then opens the **control stream** (the
first client-initiated bidi stream) and writes one `Hello`:

```json
{ "protocol": "hab/2", "service": "ocr", "capabilities": ["ocr.extract"],
  "token": "<AI_SHARED_TOKEN>", "max_concurrent": 8 }
```

`Hello` names **no school** — see "School scoping" below. The backend answers
one `Greeting` and leaves the stream open:

```json
{ "type": "welcome", "worker_id": "01J...", "protocol": "hab/2" }
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

### School scoping

This deployment serves many schools, one database each, and the AI fleet is
**shared** across all of them: one connected service answers for every school.
So the school cannot be bound at handshake time — a service pinned to one
school would have to be run once per customer — and every *frame* names it
instead.

`school` is the school's slug, exactly as it appears in a login (`demo`), and
it is **required** on `Request`, `ApiRequest` and `BlobRequest`. Every answer
echoes it, refusals included, so a service can tell which of its in-flight
streams a refusal belongs to. There is no default and no fallback: a frame
naming no school is `malformed`.

| `code` | Meaning |
| ------ | ------- |
| `malformed` | `school` is absent, or is not a slug at all |
| `unknown_school` | A well-formed slug this deployment does not serve — not retryable without a config change |
| `school_suspended` | The school exists and is switched off; worth retrying later |

A read is answered out of that school's own database, `on_behalf_of` is
resolved there too (so the same username in two schools is two different
people, and one school's user id names nobody in another), and blob bytes come
from that school's own directory. That is the whole point of `hab/2`: the
previous wire version carried no school, and a service still announcing it is
refused at the ALPN, before it can send a frame.

### Requests

For each request the **backend** opens a bidi stream, writes one `Request`,
finishes its send side, and reads one `Response`:

```json
{ "id": "01J...", "school": "demo", "capability": "ocr.extract",
  "deadline_ms": 30000, "payload": { "image": "<base64>" } }
```

```json
{ "status": "ok",  "id": "01J...", "school": "demo",
  "payload": { "text": "..." } }
{ "status": "err", "id": "01J...", "school": "demo",
  "code": "unsupported_image", "message": "only png and jpeg" }
```

`school` is the school the work belongs to, echoed back like `id`. `id` is a
trace id for logs on both sides — correlation is the stream, not the
id. It must still be echoed: an answer carrying a different id means the
service lost track of whose work it is, and the payload is refused. `payload`
is opaque to the transport; its shape belongs to the capability.

`deadline_ms` is when the backend gives up. A service should abandon the work
rather than answer late. A handled failure is an `err` frame; a crash is just a
dropped stream.

### The `rag.chat` capability

`rag.chat` answers a question **about a school's course-note corpus**, with
citations. Same `Request`/`Response` frames as any capability; only the payload
differs. The service retrieves the relevant passages first and then generates,
so its deadline (90s) is longer than a plain chat turn's.

The request payload:

```json
{ "message": "ikinci yasa nedir?", "asker": "01ASKER", "asker_role": "student",
  "scope": [ { "sinif": "11", "ders": "Fizik" } ],
  "history": [ { "role": "user", "content": "merhaba" } ] }
```

`asker` is the id of the person who asked, so the service can read *their* own
data back through the api reads above (`on_behalf_of`) — a question about "my
marks" is answered from their marks. `asker_role` is their school role, the
same lowercase strings the chatbot request carries. `scope` is a list of
`(sinif, ders)` **pairs**: the corpus is routed by the pair, so a grade and a
subject list sent separately would cross-product into combinations the asker
never named. At most `MAX_RAG_SCOPE_PAIRS` (200) pairs ride one request.
`history` is the same oldest-first tail as `chat.reply`, optional.

The reply payload:

```json
{ "text": "F = m·a [1]", "abstained": false, "reason": "",
  "citations": [ { "n": 1, "doc_id": "01DOC", "pages": [3],
                   "span_ids": ["s-7"], "ders": "Fizik" } ] }
```

A refusal that is part of the answer stays here rather than becoming an `err`
frame: `abstained: true` with a short `reason` (`guard_*`, `insufficient_data`,
`model_abstained`, `scope_mismatch`, `role_required`, …) is a completed turn.
Each citation's `n` is the marker `[N]` the answer text uses, so a client
resolves `[N]` by finding the citation whose `n` is `N` and opening the
document `citations[].doc_id` names. `doc_id` is the **corpus** document id —
the backend maps it to the `course_note_file` that owns that PDF.

For that mapping to exist, the `rag.index` **reply** should echo each indexed
file's ids: the service answers `{"files": [{"id": "<course_note_file id>",
"doc_id": "<corpus doc id>"}]}`, naming the corpus id it assigned to each file
it indexed. The backend pairs the echoed `doc_id` back to the file it sent, so
a later citation's `doc_id` resolves to a downloadable file.

### API reads (the other direction)

A service usually needs school data to do its work — who asked, their notes,
their marks. Rather than handing every service an HTTP session and a password,
the bridge lets it read the **same REST API** back over the connection it
already has.

After the handshake the *service* may open further bidi streams, one per read:
write one `ApiRequest`, finish the send side, read one `ApiResponse`, done.
Same framing as every other `hab/2` frame; concurrency and correlation are the
stream, exactly as for capability requests.

```json
{ "id": "01J...", "school": "demo", "path": "/marks/me",
  "query": "limit=10&offset=0", "on_behalf_of": "user:01J...",
  "method": "GET" }
```

`id` is a trace id, echoed back, and so is `school` — which names the database
the read is answered from and is required. `path` is the path alone as the REST API
spells it — no host, and **no query string**, which travels in `query` (without
the leading `?`). `query`, `on_behalf_of` and `method` are all optional; an
absent `method` means `GET`, and anything else is refused.

The answer is tagged by `outcome`:

```json
{ "outcome": "ok",  "id": "01J...", "school": "demo", "status": 200,
  "body": { } }
{ "outcome": "err", "id": "01J...", "school": "demo",
  "code": "path_not_allowed",
  "message": "`/users` is not a path AI services may read" }
```

**Any status the router produced rides as `ok`.** A `401`, a `403`, a `404` is
the API answering — the service asked and got a reply — so it arrives as `ok`
with that `status`. `err` is the *bridge* refusing, and the request then never
reached a handler at all. A client that treats `status >= 400` as a transport
failure has misread the contract.

| `code` | Meaning |
| ------ | ------- |
| `malformed` | The frame was not a readable `ApiRequest` (a missing `school` lands here), `school` is not a slug, or `path`+`query` do not form a request target |
| `method_not_allowed` | `method` was present and was not `GET` |
| `path_not_allowed` | `path` is not in the read scope below |
| `unknown_school` | `school` is a slug this deployment does not serve |
| `school_suspended` | That school is suspended — retryable once it is not |
| `unknown_user` | `on_behalf_of` names no user *of that school* (deleted since the service last saw them, or an id belonging to a different school) |
| `unavailable` | The API is not serving yet, the database socket is down, or the read outran the request timeout — retryable |
| `not_json` | The endpoint answered with a body that is not JSON |
| `too_large` | The answer does not fit one frame |

Refusals are decided in that order — method, then the allowlist, then the
school, then the principal — so an unknown user on a forbidden path reports the
path, and an unknown user in an unknown school reports the school.

#### The read scope

`GET`-only and deny-by-default. A path must match one of these patterns
exactly, segment for segment (`src/constant.rs`, `AI_API_ALLOWLIST`):

```
/auth/me                  /marks/me
/users/me/profile         /marks/{user}
/users/{id}/profile       /attendance/me
/notes                    /attendance/{user}
/notes/{id}               /pomodoro/me
/homework                 /pomodoro/{user}
/homework/{id}
/homework/{id}/result
/homework/{id}/submission
/homework/report/{user}
/course-notes
/course-notes/{id}
/course-notes/{id}/files
```

`{x}` takes exactly one non-empty segment. There is no prefix match and no
wildcard tail: `/notes` does not admit `/notes/{id}/files/{file_id}`. So a new
route family is unreachable to the services until somebody adds it on purpose —
one line in that constant, and an integration test validates every entry
against the emitted OpenAPI paths, so a renamed route breaks the build rather
than silently narrowing the scope.

#### Who the request runs as

With `on_behalf_of`, the read executes **as that user**: own-scoped endpoints
(`/auth/me`, `/notes`, `/marks/me`) return *their* data. Both spellings are
accepted — the bare key (`01J...`, as a REST path writes it) and the record
form (`user:01J...`). The account is loaded live from the database on every
request — from the named school's database, never the control one — and never
trusted from the frame, so a service holding a stale id acts
as a user who has since been deleted (`unknown_user`) or demoted (the new role,
not the old one) — never as who they used to be.

Without it the principal is the internal role **`ai`**: the lowest privilege in
the system, below `parent`. It is not a human role and is never assignable —
`"ai"` is rejected by every role input, no account can hold it, and it is never
stored. It appears in the OpenAPI *response* schema `Role` as documentation
only — request bodies take `AssignableRole`, the five human roles, so no
generated client or Swagger form offers it. So a
read that needs a role answers `403` — through the `ok` envelope, with that
status, like any other API refusal.

Responses share the frame cap (8 MiB); an answer that does not fit comes back
as `too_large` rather than a dropped stream, so a page too big is narrowed with
`query` instead of waited out.

### Blob reads (course-note file bytes)

A PDF cannot ride a JSON frame, so an indexing service pulls a course note's
attachment over a **raw byte stream** instead. Same direction and same framing
as an api read — the service opens a bidi stream and writes one frame — only
the shape differs, and the two are told apart by the field each *requires*: an
`ApiRequest` has `path`, a `BlobRequest` has `file`.

```json
{ "id": "01J...", "school": "demo", "file": "01J8XZ0K3Q8G7X2M4N5P6R7S8V",
  "on_behalf_of": "user:01J..." }
```

`school` names the school the file belongs to; the bytes are read from that
school's own blob directory, so a file id is meaningless outside it. `file` is
a `course_note_file` record key — the id `GET
/course-notes/{id}/files` publishes and the one every `rag.index` payload
carries in `files[].id`. `on_behalf_of` is optional on the wire but required in
practice: without it the principal is the `ai` role, which is enrolled in
nothing and can view no course, so every read is `forbidden`. Send the note's
`author` (the `rag.index` payload carries it) or the student who asked.

The backend answers with **one header frame**, tagged by `status` — the same
tag `Response` uses, not the api read's `outcome`, since a blob header carries
no HTTP status to collide with:

```json
{ "status": "ok",  "id": "01J...", "school": "demo", "name": "recap.pdf",
  "content_type": "application/pdf", "size": 204800 }
{ "status": "err", "id": "01J...", "school": "demo", "code": "forbidden",
  "message": "`01J...` may not view the course this file belongs to" }
```

On `ok`, **exactly `size` raw bytes follow the header frame**, then the stream
is finished. Those bytes are *not* a frame: they carry no length prefix and the
8 MiB frame cap does not apply to them, which is the whole point — read
`size` bytes and then expect EOF. `size` is measured off the stored blob
itself, so it is what will actually arrive rather than what a row remembers.
On `err` nothing follows the frame at all. A read that breaks after its header
resets the stream instead of finishing it, so a truncated file is never
mistaken for a complete one. A body that makes no progress for 30 s — a
service that opened the stream and then stopped reading — is reset the same
way rather than finished; the clock is per write, not over the transfer, so a
slow-but-reading service is never cut off.

| `code` | Meaning |
| ------ | ------- |
| `malformed` | The frame was not a readable `BlobRequest`, or `school` is absent or not a slug |
| `not_found` | No `course_note_file` with that key *in that school*, or its note or course is gone |
| `forbidden` | The principal may not view that file's course |
| `unknown_school` / `school_suspended` | As for an api read |
| `module_disabled` | That school has no `course_notes` module, or no `chatbot` module (the `ai` package: without it a school sends nothing to an AI service) — this stream bypasses the router, so it checks both entitlements itself |
| `unknown_user` | `on_behalf_of` names no user of that school |
| `unavailable` | The api is not serving yet, the database socket is down, or the row's blob is missing from disk — retryable |

**Course-note attachments only.** The key is looked up in `course_note_file`
and in no other table, so a personal note's file id (`/notes/{id}/files`) is
`not_found` here rather than a different table's row: personal notes have no
reader but their owner, and this stream does not become one.

Authorization is the very guard `GET /course-notes/{id}/files/{file_id}`
applies — course management rights or enrollment. The bridge widens *who may
ask*, never *what may be read*: the file's note, and that note's course, are
loaded and gated exactly as they are over HTTP.

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
speaks `hab/2` with a client that imports none of the crate's protocol types —
frames built as byte literals, answers parsed as untyped JSON — so it fails on
exactly the changes a service in another language would notice: a renamed
field, a re-tagged enum, a flipped length-prefix endianness, a newly-required
`Hello` field, a frame that stopped naming its school. (`tests/ai_bridge.rs`
drives real QUIC clients too, but shares
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

QUIC has no plaintext mode. With `AI_TLS_CERT`/`AI_TLS_KEY` **both** unset, a
self-signed certificate is generated at boot and its sha256 fingerprint logged
— services pin that instead of installing a CA. The certificate authenticates
*the backend*; the shared token in `Hello` authenticates *the service*.

Setting **one** of the two (a blank value counts as unset) is a startup error,
not a fallback to self-signing: a bridge quietly presenting a throwaway
`localhost` certificate to services pinning the real one is a deployment that
looks configured and answers `503` to every chatbot send.

So a service does not have to be handed a file out of band, the certificate is
published over HTTP:

```
GET /ai/certificate          # no auth; 404 when the bridge is disabled
{ "protocol": "hab/2",
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

**The deltas are sliced by the backend from the finished answer.** `hab/2` is
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
Requests then arrive as ordinary `hab/2` `Request` frames — the asker's school
is on the frame, not in the payload — whose `payload` is:

```json
{ "message": "and the second law?",
  "asker_role": "student",
  "history": [ {"role":"user","content":"what is the first law?"},
               {"role":"assistant","content":"an object at rest …"} ] }
```

`asker_role` is the **school role of the person asking** — one of `parent`,
`student`, `teacher`, `manager`, `admin` (the same lowercase strings the rest of
the API uses). Answer to it: a student must not be handed an answer scoped for a
manager. It is read live from the authenticated session on every request and can
never be set by the client — the send body carries `content` and nothing else —
so it is safe to trust. Note it is *not* the `role` inside a `history` entry:
that one is `user`/`assistant`, who *said* the turn. The backend does no
role-based filtering of its own; scoping the answer is the service's job.

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

### The `rag.index` capability (for AI-service authors)

An indexing service declares `rag.index` in its `Hello` (see "AI bridge
(QUIC)"). The backend dispatches one request whenever a course note is
created, edited, or gains or loses a file — never on a read. Requests arrive
as ordinary `hab/2` `Request` frames — the note's school is on the frame, not
in the payload — whose `payload` is:

```json
{ "course_note": "01J8XZ0K3Q8G7X2M4N5P6R7S8T",
  "course": "01J8XZ0K3Q8G7X2M4N5P6R7S8U",
  "author": "01J8XZ0K3Q8G7X2M4N5P6R7S8W",
  "title": "Chapter 3 recap",
  "content": "Covered quadratics; homework due Friday",
  "files": [ {"id":"01J8…","name":"recap.pdf",
              "content_type":"application/pdf","size":24576} ] }
```

`files` is attachment **metadata only**; the bytes ride their own QUIC stream
(see "Blob reads" above — one `BlobRequest` per `files[].id`), never a JSON
frame. It is optional (absent or `[]` = a note with no attachments). `author`
is the note's own teacher: a service reads the bytes `on_behalf_of` them,
since the `ai` principal can view no course.

The answer is a `Response::Ok` whose payload is **any JSON object**: its shape
belongs to the service, and the backend stores it verbatim against the note
(one `rag_output` row, replacing whatever was stored before). A non-object
payload is dropped with a warning. Failures reuse the existing
`Response::Err {code, message}`.

Nothing about this is on a user's request path: the dispatch is
fire-and-forget, so a course-note write answers before the service is asked
and never fails, waits, or 503s because of it. With no worker carrying
`rag.index` the trigger is a silent no-op — notes are stored exactly as
before, just unindexed. Rows are derived data: deleting the note, or a file an
output was built from, deletes the output too, service or no service.

**Unknown extra keys are ignored on both sides, on purpose** — same rule as
`chat.reply`.

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
role. A demotion sweeps the user off every board they were on — and **closes
every board they created**, in the same write. A creator demoted to `parent` is
an outsider on their own board (`404`), while clearing, locking, closing and
deleting it are theirs alone and nothing lists a board the caller is not on: the
room would be commandable by nobody at all, and its participants would keep
drawing on it until the lifetime cap closed it. So the demotion closes it —
permanently read-only, nothing deleted, every mark and every epoch still
readable to the people who drew them, and the creator's `board_count` seat still
taken, because the row it counts is still there. The first
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
teacher+ for a class (`GET /classes/{id}/members`), a manager+, one of the
instance's assigned teachers or its class's homeroom teacher for an instance
(`GET /instances/{id}/enrollments`),
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

**A closed board is never reported as `locked`.** Locking and closing are
independent — a board may be closed while locked, in either order, and nothing
refuses either call because of the other — and when both hold the answer is
`board_closed` on *every* path (a stroke, a socket `clear`, `POST
/boards/{id}/clear`). Closed outranks locked because there is no reopen: a
pause the client is told to wait out would never lift. Unlocking such a board
changes nothing about it; it stays permanently read-only.

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
"read, decide, write" is never safe on its own — under `READ COMMITTED` a
count read in one statement does not conflict-check against a concurrent
insert (write-skew), and that fires between two tasks in one process exactly as it
would between two machines. The mutex-only design this replaced was already
losing races at one process. Every invariant is therefore guarded where the
database itself decides the winner. Three tiers:

1. **Single-row conditional writes** — compare-and-set (`save_if_unchanged`),
   `UPDATE … WHERE`, stored counters (`db::cap`). A single-record write is
   atomic, so of N concurrent tasks exactly the allowed number get a non-empty
   result: the database decides the winner, the loser retries
   (`CAS_UPDATE_RETRIES`) or gets a 409.
2. **One-statement gates** — the check runs inside the same statement as
   the write it authorizes, so it is atomic with it: a guarded
   `UPDATE … WHERE` (the last-admin floor), an
   `INSERT … SELECT … WHERE EXISTS` behind a real `FOREIGN KEY` (the
   existence proofs), the CTE recipes in `db::cap`. Used where
   the rule reads the row being written (state machines, delete guards).
   "Is anything still attached?" is answered the same way, by counters on the
   row being deleted rather than a `SELECT` over the children: a course is
   deletable while its `class_course_count` **and** `course_membership_count`
   are zero (a şube attach or a club/etüt join claims that reference *before* it
   writes the link, and gives it back when the link moves or is deleted), and a
   term while its `exam_count` is zero and no `karne_snapshot` names it — the
   count is the fast path, the snapshot `NOT EXISTS` is the record a `DELETE`
   must not take with it. A fee
   plan reads the same way — editable *and* deletable while its
   `assignment_count` is zero, and frozen for good once it is not, since an
   assignment is never taken back.
   Where the child also carries a deterministic id — one enrollment per
   (course, user), one registration per (event, user), one fee-plan assignment
   per (plan, student) — the seat and the row
   are claimed in one statement (`db::cap`'s `claim_and_create` recipes), so a
   duplicate insert rolls its own seat back instead of costing a stranger their
   place. Where no single statement can decide the rule — an appointment
   approval reads other rows that may not exist yet — that one decision
   escalates to `SERIALIZABLE`, and `tx_with_retry` re-sends it when Postgres
   aborts the loser (`40001`); the re-run sees the winner committed and
   refuses with the ordinary `409`.
3. **Two accepted races**, reviewed and deliberately left open:
   - *Attempt-seq late save* — an exam-room socket writes into the sitting it
     joined with, a choice made before any lock is taken, so a save racing a
     retake can stamp an answer onto the just-terminal previous sitting.
     Damage: one history row; the grade of record (latest `seq`) is never
     touched.
   - *Archive-vs-write* — the archived-term guard is a pre-flight read on the
     term row, so an archive committing in the same instant as a write already
     in flight lets that one write through. Damage: one write on a
     just-archived term; nothing corrupts, and every later write is refused.

No process-wide domain lock remains. Exam-room presence is an in-process
map (`ExamPresence` in `src/state.rs`; the mutex never spans an await).
There is no row to conditional-write for socket counts; the paired
`left_at` stamp is an idempotent statement on the sitting. `CLAIM_LOCK`
tamed a retry loop that `tx_with_retry` now owns, and what
`APPOINTMENT_LOCK` serialized is three database guarantees — the seat
claim's one-statement write, the publish-overlap exclusion constraint, and
the serializable approval.

Boot is idempotent: the sqlx migrators apply `migrations/control` to the
control database (and `migrations/school` to the school template, and to
every school database at mint), tracked in `_sqlx_migrations`, so a second
boot touches nothing; the builder seed runs on every start but writes only
when the username is absent. A release that adds a migration simply applies
it on the next boot — there is no stop-the-world counter backfill to
schedule and no `migration_mark`: the counters the guards lean on
(`total_stroke_count` among them) are maintained by the same conditional
writes that spend them. In-flight work does not survive a restart either:
a chatbot turn left `pending` by a dead process reads back as `failed`
(a read-time projection, not a sweep).

## Layout

```
src/
  main.rs          bootstrap: config, db, router, serve
  lib.rs           build_router: routes + OpenAPI/Swagger + CORS + /health
  config.rs        env-sourced Config
  constant.rs      validation limits
  validate.rs      field validators (used by every newtype's try_new)
  error.rs         ValidationError + AppError -> HTTP responses
  database.rs      PgPool + tx_with_retry (a guarded BEGIN…COMMIT that re-runs
                   while Postgres says "contended") + the two sqlx migrator
                   sets; init() brings up the control database, a mint
                   migrates a school's from the shared template
  tenant.rs        Slug · SchoolStatus · School · Tenants: one Postgres
                   database per school (`{control}_school_{slug}`), plus the
                   control database that registers them (DEMO_SLUG is the
                   tests' school)
  db/              the only layer that executes queries: per-table SQL, one
                   file per resource · cap.rs (the count-cap CTE recipes and
                   their verdict types) · field_update.rs (one UPDATE … SET
                   from only the fields a PATCH carried) · page.rs (PagedList:
                   one paged SELECT — the window and its `total`)
  service/         workflows the handlers call: each multi-step invariant is
                   one tx_with_retry-guarded transaction
  module.rs        Module (one router nest a school may buy) · Package ·
                   ModuleSet: dependency edges, validation, stored names
  rate_limit.rs    fixed-window limiter: per-IP tiers + middleware, per-user chat tier
  state.rs         AppState { db, tenants, files_path, cookie_secure, rate_limit,
                   chatbot_limit, exam_presence, board_hub, ai, metrics }
  ai/              QUIC bridge to the out-of-process AI services
                   (see "AI bridge (QUIC)"; the HTTP half is web/ai.rs)
    protocol.rs    Hello/Greeting/Request/Response + length-prefixed JSON framing
    server.rs      AiBridge: listener, handshake, dispatch over per-request streams
    registry.rs    connected workers, capability routing, least-inflight leases
    tls.rs         listener certificate (PEM or self-signed) + fingerprint
    error.rs       AiError
    chat.rs        the `chat.reply` payload contract (ChatRequestPayload/ChatReplyPayload)
    rag.rs         the `rag.index` payload contract + spawn_index: a course note
                   changed, so its stored output is refreshed in a background task
    api.rs         AI_API_ALLOWLIST: the exact REST paths an AI service may read
                   (deny-by-default, segment-for-segment, no wildcard tail)
  domain/          validated newtypes + entities (pure data and validation;
                   persistence lives in db/)
    user.rs        UserId · Username · Password · PasswordHash · User (has role)
    role.rs        Role enum (student < teacher < manager < admin), at_least()
    monotonic_id.rs next_uuid: UUIDv7 ids that sort in write order — one
                   process-wide ContextV7, so same-millisecond rows never scramble.
                   Every id in the crate comes through here; a guard test fails on
                   a stray v4 anywhere under src/
    key.rs         sitting(): the deterministic per-sitting record key shared by
                   attempts, answers, answer images and results (seq 1 stays bare)
    text_fold.rs   case- and diacritic-insensitive folding for search, shared by
                   the Rust needle and the SQL column (Turkish İ/ı, ü, ö…)
    session.rs     SessionId · SessionToken · Session (7-day expiry)
    builder.rs     BuilderId · Builder · BuilderSession: the deployment operator
                   who creates and suspends schools, in the control database
    timestamp.rs   Timestamp (unix-millisecond instant)
    note.rs        NoteId · NoteTitle · NoteContent · Note
    note_file.rs   NoteFileId · FileName · FileContentType · NoteFile (metadata row;
                   blob on disk under FILES_PATH, named by the row's uuid)
    event.rs       EventId · EventTitle · EventDescription · Event
    attendance.rs  AttendanceId · AttendanceStatus · Attendance
    course.rs      CourseId · CourseTitle · CourseDescription · Course
    course_note.rs CourseNoteId · CourseNoteTitle · CourseNoteContent · CourseNote
    course_note_file.rs CourseNoteFileId · CourseNoteFile (metadata row; blob on
                   disk under FILES_PATH; FileName/FileContentType shared with note_file.rs)
    rag_output.rs  RagOutputId · RagOutput (what an AI service produced for a
                   course note; derived, disposable, cascaded from note and file)
    course_session.rs CourseSessionId · SessionTopic · CourseSession (an instance's lesson)
    session_attendance.rs SessionAttendanceId · SessionAttendance (roll call; one row per session+user)
    work_entry.rs  WorkEntryId · WorkEntry (staff stint; one open per user by construction)
    enrollment.rs  EnrollmentId · Enrollment (one row per instance+student; `source`
                   names the class that pumped it, absent when hand-placed)
    course_membership.rs CourseMembershipId · CourseMembership (one row per
                   catalog course+user: the club/etüt join a student makes alone)
    academic_year.rs AcademicYearId · GradePromotion · AcademicYear (the school
                   year: its terms, its sınıf geçme policy, its rollover)
    class_group.rs ClassGroupId · ClassName · ClassGrade · ClassGroup (a class
                   section; belongs to an academic year; deletable only with no
                   live members and no instances)
    class_member.rs ClassMemberId · ClassMember (one student's stint in a class;
                   `left_at` ends it — the row stays, the seat comes back, and a
                   re-add is a fresh row)
    class_course.rs ClassCourseId · DersSaati · ClassCourse (the **instance**:
                   one course as one class teaches it — hours, karne weight,
                   teachers, roster, and the key every exam/session/homework
                   under it carries)
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
                   seat claim, publish overlap is the exclusion constraint,
                   approval overlap is SERIALIZABLE)
    pomodoro.rs    PomodoroSessionId · PomodoroSession (student focus log)
    pool_question.rs PoolQuestionId · PoolQuestionTitle · PoolQuestionBody ·
                   PoolQuestion (student-asked question; teacher-approved into
                   the school-wide pool; optional photo as metadata + disk blob)
    solution.rs    SolutionId · SolutionBody · Solution (discussion thread on an
                   approved pool question; dies with the question)
    homework.rs    HomeworkId · HomeworkTitle · HomeworkDescription · Homework
                   (per-instance assignment; required subject; optional `assigned`
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
    tenant_state.rs State<AppState>: the shadow of axum's State that resolves the
                   caller's school from the `<slug>.<token>` cookie, so every
                   handler that imports it is school-scoped by construction
                   (ResolvedTenant · TenantExt · SchoolSlug · split_cookie)
    module_gate.rs one route_layer per nest: a module the school has not bought
                   answers 403 {error, module} on every route in it, while an
                   unmatched path inside it still 404s
    modules.rs     GET /modules/catalog (public: every sellable module, its
                   package, what it requires) and GET /modules (the caller's
                   school's own enabled set) — both ungated
    builder.rs     the vendor surface: /builder/login|logout|me and /schools/*
                   (create · list · rename/suspend · delete · admin-password ·
                   enter · modules: list/enable/disable/batch PATCH)
    image.rs       the halves every image upload shares: the `file` part under the
                   raster allowlist and the school's size cap, and the blob-first write
    extractor.rs   CurrentUser · RequireTeacher · RequireManager · RequireAdmin
                   · RequireBuilder (the control-database principal)
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
    courses.rs  course_notes.rs  subjects.rs  sessions.rs  homework.rs  questions.rs
    exams/         mod.rs (exam CRUD + grading) · questions.rs · attempts.rs ·
                   images.rs · review.rs (prior sittings)
    bank_questions.rs  marks.rs  work.rs  pomodoro.rs  attendance.rs
    settings.rs  terms.rs  meals.rs  payments.rs  ai.rs  chatbot.rs
    boards.rs  classes.rs  instances.rs  academic_years.rs
    limits.rs      GET /limits: every constant.rs bound served as JSON
```

Tests: `cargo test` — unit (in-source), integration (`tower::oneshot` over
per-test PostgreSQL databases), rate-limit (both tiers, proxy-header and peer-address keying, shipped
limits over every route, two limiters sharing one budget over one db), e2e
(real TCP + reqwest cookie jar), persistence
(an idempotent re-migration over a live database must leave its seeded rows
intact), ai-bridge (real QUIC on
loopback against a fake AI service), ai-protocol (the `hab/2` wire contract,
driven by a client that shares no code with the backend).

Every suite runs against a **real** PostgreSQL, because production is real
PostgreSQL. The harness (tests/common) mints each test a private pair of
databases on one server — `heztest_<16 hex>` for the control side and its
`heztest_<…>_school_<uuid hex>` school (the school's registry id, clipped to
Postgres's 63-byte identifier cap), template-cloned so a schema edit migrates
the shared template once — and drops them when the deployment's last handle
dies; a janitor sweeps what a crashed run left behind. The server comes from
`HEZARFEN_TEST_DATABASE_URL` (default: the compose stack's maintenance
database). A missing server is a hard failure, not a skip — start it with
`podman compose up -d postgres`.
