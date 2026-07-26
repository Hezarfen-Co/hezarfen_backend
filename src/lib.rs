pub mod ai;
pub mod config;
pub mod constant;
pub mod database;
pub mod domain;
pub mod error;
pub mod rate_limit;
pub mod state;
pub mod validate;
pub mod web;

use axum::extract::Request;
use axum::http::{HeaderValue, Method, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;
use utoipa::openapi::security::{ApiKey, ApiKeyValue, SecurityScheme};
use utoipa::{Modify, OpenApi};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use utoipa_swagger_ui::SwaggerUi;

use crate::error::AppError;
use crate::rate_limit::RateLimiter;
use crate::state::AppState;

/// Top-level OpenAPI document. Per-path operations and schemas are collected
/// automatically from the `#[utoipa::path]`-annotated handlers via `utoipa-axum`.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Hezarfen Backend API",
        version = env!("CARGO_PKG_VERSION"),
        description = "Notes, events, attendance, courses, and weighted exam marks behind cookie-session auth. Notes carry file attachments (PDFs, documents — stored on disk, capped per file by the school's settings). Any two users can message each other one-to-one, mail-style: each side files its own copy through folders (recipient: inbox/archive/trash with a read flag the sender sees as a receipt; sender: sent/archive/trash), and a message is truly gone only when both sides have deleted it from their trash. Exams run sync (one window), async (window + per-attempt duration), or open (sit anytime), with per-exam attempt limits (retakes) and a teacher-controlled rejoin door on the live exam room; questions can carry images — an illustration on any question, and per-option pictures on choice questions; students can attach an answer drawing to any question in their attempt. Enrollment, exam-sitting, roll call, and marks are student-only — staff run them, they don't take part. Every event carries an audience — the whole school, one role, a course's enrollment, or a registration list — that defines its expected-attendee roster (visibility is unaffected: everyone sees every event). Registration-audience events hold a signup list built one seat at a time: teachers register students (students never register themselves), staff register only themselves, the list optionally caps at a capacity, and it closes the moment the event starts. Event attendance is taken by teachers+ against that audience (students never self-mark), and a per-event roster report joins the expected list with the marks to show who missed. Courses come in three behaviorally identical kinds — `course` (a regular class), `study` (a supervised study session — etüt), and `club` (a student club — kulüp) — may cap their roster with an optional enroll-time `capacity` (a full course refuses new members), and carry lesson sessions with teacher-taken roll call. A course is run by whoever created it plus any teachers a manager assigns to it (`POST /courses/{id}/teachers`) — an assigned teacher manages everything inside the course (exams, sessions, subjects, roster, grading) but cannot delete the course or change who else teaches it, and a demotion below `teacher` drops their assignments. Each course also owns its curriculum as a list of subjects, and every exam question and every homework must be tagged with one of its course's subjects (a subject still referenced by questions or homework cannot be deleted); staff clock in/out on a server-stamped work log; students run a pomodoro study timer whose focus sessions land in a server-stamped log (starting discards a dangling unfinished session; teachers can read any student's log and its total focus time); attendance reports tally events and per-course roll call with rates. Courses also hand out homework: teacher-assigned per course to the whole class or a named subset the unnamed never even see (404s, no existence leak), subject-tagged and due by a required future `due_at`; students submit optional text plus up to 10 files of any content type (each capped by `max_file_bytes`, always served back as forced attachment downloads), editable until a teacher grades a status (`done`/`incomplete`/`missing`) with an optional 0–100 mark — the grade freezes the submission until removed, work never handed in is gradable `missing`, lateness is computed from an immutable first-submit stamp and a moving last-touch stamp (never stored), the grading roster computes `missing` and `unenrolled` flags, and homework marks stay out of the weighted `/marks` averages. School-varying policy (exam kinds with their course-average weights, attendance statuses, grade-display bands, the note-file size limit) lives in an editable settings singleton, and courses may link to academic terms. Each account also carries its own UI preferences — theme (`light`/`dark`) and language (`tr`/`en`) — self-managed, admin-editable for anyone, and `null` until chosen (the client then follows the device preference). A `parent` role observes without touching: admins tie any number of students to a parent account (`POST /users/{id}/students`), the parent lists them at `GET /users/me/students` and reads each one's mark, attendance, pomodoro, and homework reports — and nothing else; parents hold no staff power and never act as students (no enrolling, sitting exams, or roll call). A school-wide **question pool** lets students ask for help: a student posts a question (optionally with a photo of the problem — same raster rules and size cap as question images), a teacher+ approves it into the pool (or deletes it — rejection is deletion, there is no rejected state), and every approved question is readable by the whole school (parents excepted), with anyone free to offer solutions under it — text plus an optional photo of the worked steps, both editable by the solution's author at any time, since solutions are unmoderated and never freeze — and every question row carries its `solution_count`; pending questions are visible only to their asker and to teacher+ (whose `?status=pending` list is the approval queue), and approval freezes the question's content so nothing unmoderated can slip into the pool afterwards. Teachers keep an **appointment calendar**: a teacher+ publishes availability slots (one-off, or repeating weekly up to an `until` date as a series that deletes as one), and students and parents book a slot with a reason — the booking lands `pending` until the teacher approves it, rejects it, or counter-proposes another time (which sends it back to `pending` for the requester to accept or decline); the requester may cancel until the meeting starts — a teacher ends a booking by rejecting it, or by counter-proposing and then rejecting an approved one — one live booking holds a slot, a teacher's published windows may not overlap each other (`409`, half-open, so back-to-back slots are fine), and no approved meeting may overlap another for the teacher or the requester. While the database connection is reconnecting (a restart, a dropped socket) requests briefly answer `503` with `Retry-After: 1` — transient, retry shortly. Every list endpoint accepts `?limit=&offset=` and returns a `{items, total, limit, offset}` page envelope — paging is opt-in, so omitting `limit` returns the full list and `total` always carries the unpaged count. Every fixed validation bound — field lengths, numeric ranges, and the closed value sets (roles, course kinds, exam and question kinds, homework statuses, uploadable image types) — is published at `GET /limits` (no auth, cache for the session), so a client validates against the server's own constants instead of a hard-coded copy that drifts. It also reports this server's live rate-limit tiers, so a client learns its request budget instead of discovering it by collecting a `429`, and it keeps answering while the database is down — a client booting against a degraded backend still gets the contract.",
    ),
    modifiers(&SecurityAddon),
    tags(
        (name = "meta", description = "Liveness, the authoritative clock, and the validation contract. `GET /limits` publishes every fixed bound the API enforces — field lengths, numeric ranges, closed value sets (roles, course kinds, exam and question kinds, homework statuses, image content types) — so a client validates against the server's own constants instead of a hard-coded copy that drifts. Unauthenticated, because the registration form needs the username and password bounds before a session exists, and the values change only with a deploy: fetch once, cache for the session. School-adjustable policy lives on `GET /settings`; what `/limits` carries for those knobs is the fixed range a manager may set them within. Its `rate` group is the exception to \"fixed\": those tiers are environment-tunable, so the endpoint serves the running server's live values (`0` = tier off)"),
        (name = "ai", description = "AI-bridge discovery. The AI features run as separate services that dial IN to this backend over QUIC (protocol/ALPN `hab/1`, address `AI_QUIC_ADDR`) and register the capabilities they serve; each request then rides its own QUIC bidirectional stream on that one connection. `GET /ai/certificate` publishes the bridge listener's certificate (PEM + SHA-256 fingerprint) so a service can pin it before dialling — public, because a server certificate is presented to every peer during the TLS handshake anyway, while the shared token that actually authenticates a service (`AI_SHARED_TOKEN`) is configured out of band. With no certificate configured the bridge self-signs afresh at each boot, so a service must re-fetch this on every reconnect, not only at startup. `404` when the bridge is disabled (`AI_QUIC_ADDR` unset). See the README's \"AI bridge (QUIC)\" section for the frame-level protocol"),
        (name = "auth", description = "Registration, login, session lifecycle. `POST /auth/register` answers `201` whether or not the username was free, and the two bodies are byte-identical: it returns only `{username, role}` — the echoed name and the `student` role every fresh account gets — and deliberately **no `id`**, since on the taken path there is no row to name (log in to learn your id). A deliberate anti-enumeration measure, since the route is unauthenticated and a `409` would let anyone harvest the school's user list. The accepted cost is that a caller who collides gets no distinct error and simply cannot log in with that password; do not \"fix\" this back to a `409`"),
        (name = "chatbot", description = "The AI chatbot, relayed through the QUIC bridge to an out-of-process AI service. Every authenticated role may chat (parents included) and every thread is private to its owner — nobody, not even an admin, reads someone else's. A user keeps up to the school's `max_chatbot_threads` threads (409 at the cap, delete one first) and sends at most `RATE_LIMIT_CHATBOT_PER_MINUTE` messages a minute (default 20, `0` disables the tier; 429 with `Retry-After`). A thread can be renamed — or its name cleared — with `PATCH /chatbot/threads/{id}` (`{title}`, `null` to clear); nothing names a thread automatically. Sending is asynchronous: `POST /chatbot/threads/{id}/messages` writes the turn plus an empty `pending` assistant row and answers `202 {message_id, status}` immediately, because an inference outlives a normal request. The pending row is a claimed job rather than an in-process task: whichever backend process holds a `chat.reply` worker picks it up (within ~200ms), dispatches it and stamps the result, so a restart no longer strands an accepted turn and a claim its owner never finishes is retried after 90 seconds. The answer is then read either by polling `GET .../messages/{mid}` or over the Server-Sent-Events stream at `.../messages/{mid}/stream` (`delta` chunks, then one `done` or `error`) — both read the same row, so they never disagree, and a stream opened after the answer landed still replays it. An answer the service made longer than `max_chatbot_message_len` is clipped rather than discarded, and the stored turn is flagged `truncated: true` — the polling read and the SSE `done` payload always agree on it. A turn always settles: `complete` with the text, or `failed` with a short code (`unavailable`, `busy`, `timed_out`, `transport`, `protocol`, `bad_reply`, `empty_reply`, `interrupted`, or the service's own). With no `chat.reply` service connected anywhere in the deployment, sending answers `503` and writes nothing"),
        (name = "users", description = "User info: self-service profile and UI preferences (theme `light`/`dark`, language `tr`/`en` — returned on every user response, `null` until chosen), name search for the pickers (teacher+, optionally role-filtered), plus listing, lookup, and role/profile/preferences administration (admin only). Also the parent↔student ties: admins link students to a `parent` account under `/users/{id}/students` (link, list, unlink — a role change on either side drops its ties), and a parent lists their own students at `/users/me/students`; the tie grants the parent read access to those students' mark, attendance, pomodoro, and homework reports and nothing else"),
        (name = "notes", description = "Per-user notes CRUD, plus file attachments per note (multipart upload, download, delete — at most 10 per note, each at most the school's settings-configured `max_file_bytes`)"),
        (name = "messages", description = "One-to-one mail-style messages between any two users (parents included — messaging is the one place a parent writes), each optionally tagged with a free-text `label` badge. Each party owns their copy independently: the recipient's moves through `inbox`/`archive`/`trash` with a read flag (visible to the sender as a read receipt), the sender's through `sent`/`archive`/`trash`. Filing a copy away records the folder it left as its `previous_folder`, so restoring it puts it back where it came from (a trashed archive copy returns to the archive, not the inbox). `GET /messages?folder=` lists one folder (`&read=false` narrows to unread — with `limit=1`, `total` is the unread badge); permanent deletion (`DELETE`) works only from the trash and removes the row once both sides have deleted"),
        (name = "appointments", description = "Teacher availability slots and the meetings booked on them. A teacher+ publishes windows (`POST /appointments/slots`), optionally repeating weekly up to an `until` date (at most 52 occurrences, all sharing a `series` id that deletes as one). A published window may not overlap another of the *same teacher's* (`409`): the check is half-open, so 10:00–10:30 and 10:30–11:00 are two slots and not a collision, and a weekly publish is all-or-nothing — one colliding occurrence refuses the whole series. Students and parents book one with a required reason — a parent books for themselves, this is the parent-teacher conference — and the request lands `pending`: publishing availability is not consent to a person and a topic. The teacher approves, rejects, or counter-proposes another time; a counter-proposal sends the booking back to `pending` and clears `decided_by`, and the requester either accepts it (approval at the new time) or declines, which cancels the booking. A window that has already opened is closed on every side (409): such a slot cannot be booked, an underway time cannot be approved, accepted, or counter-proposed, and the requester may cancel — declining included, since a decline *is* a cancel — only until the meeting's effective window starts. Cancelling is the **requester's** route alone (`403` for anyone else, the slot's teacher and a manager/admin included, since the guard compares ids and not roles): a teacher ends a booking by rejecting it while it is `pending`, and an approved one by counter-proposing another time (back to `pending`) and then rejecting it, or simply by rescheduling. Both the reject and the cancel take an optional `reason`, and the row records who settled it (`decided_by`, `cancelled_by`) with that text (`reject_reason`, `cancel_reason`) — visible only to the requester and the slot's teacher, since that is who a booking is ever rendered to. One live booking holds a slot (rejecting or cancelling frees it), no approved meeting may overlap another for the teacher *or* the requester (409), and a slot with a live booking refuses deletion (409). A teacher demoted after publishing leaves inert slots: they drop out of the bookable list and booking one is refused"),
        (name = "events", description = "Events with an audience (school-wide, one role, a course's enrollment, or a registration signup list — the expected-attendee roster; every event stays visible to all) and their attendance: teacher+ marks people in the audience (students never mark, not even themselves), and the roster report joins the expected list with the marks to show who missed. Registration lists are filled seat by seat (teachers place students, staff place only themselves), optionally capped by `capacity`, and close once the event starts. `GET /events` takes an optional `?starts_after=&ends_after=` schedule window (unix-millis) that keeps only upcoming/unfinished events and lists them soonest-first; without it the list stays newest-first"),
        (name = "courses", description = "Courses — kind `course` (regular class), `study` (etüt, a supervised study session), or `club` (kulüp, a student club; same behavior, different label) — optional enroll-time seat `capacity`, enrollment (students only), course exams, and course sessions. A course is run by its creator plus any teachers a manager assigned to it (`POST /courses/{id}/teachers`): assigned teachers manage everything inside the course but cannot delete it or change the assignment list. A course with anyone still enrolled refuses deletion (409) — empty the roster first"),
        (name = "sessions", description = "Lesson sessions and their roll call (session teacher or course manager marks enrolled students; manager+ marks the teacher)"),
        (name = "work", description = "Staff work log: check-in/check-out stamped by the server clock, manager corrections"),
        (name = "pomodoro", description = "Student pomodoro focus log: start/finish stamped by the server clock (starting discards any dangling unfinished session, so a crashed timer never blocks the next one), own history with the unpaged `total_focus_ms` sum, and teacher+ (or linked-parent) reads of any student's log. Breaks and the work/break rhythm stay in the frontend — the backend records only focus stints"),
        (name = "questions", description = "The school question pool. Students ask (`POST /questions`, optionally attaching one photo of the problem via `POST /questions/{id}/image` — raster types only, ≤ the school's `max_file_bytes`, only while pending); teacher+ approve (`POST /questions/{id}/approve`) into the school-wide pool, or reject by deleting. Everyone student-and-up sees every approved question and may offer solutions under it (oldest first); parents stay out. Pending questions are visible only to their asker and to teacher+ (`GET /questions?status=pending` = the approval queue), and approval freezes title, body, and image — moderated content cannot be edited afterwards, only deleted (asker or teacher+; solutions and every image blob go with it). Solutions are the unmoderated half, so they never freeze: the author may edit the body (`PATCH /questions/{id}/solutions/{sid}` — author only, teacher+ included out; moderation stays delete-only) and attach/replace/remove one photo of the worked steps anytime (`/questions/{id}/solutions/{sid}/image`, same raster rules and size cap as the question's). Question rows carry a `solution_count`; solutions are deletable by their author or teacher+ (photo blob included)"),
        (name = "bank", description = "The question bank: reusable question templates, **private by default**. A template is visible to its owner (and admins) alone until its owner explicitly publishes it by `PATCH`ing `visibility` to `school`, at which point every teacher+ sees it — saving a question to the bank must never broadcast its `correct` answer key as a side effect. A template the caller may not see is a `404` on every route, never a `403` (a 403 would confirm it exists). A teacher+ reads what they can see (`GET /bank-questions`: `?subject=` origin filter, `?owner=` (a user id or `me`), `?q=` text search, `?visibility=private|school` — the last narrows the caller's own view and never widens it, so `private` means \"my drafts\", never another teacher's) and instantiates any visible template into their own exam (`POST /exams/{id}/questions/from-bank/{bid}`, which copies the template — text, points, `choice`/`text` spec, illustration, and option pictures — into a fresh exam question under the target subject, source untouched); a teacher also saves an existing exam question back into the bank (`POST /exams/{id}/questions/{qid}/to-bank`). A template's `subject` is origin metadata only — the bank spans courses, so the same-course rule is checked at instantiate time against the target exam's course, never on the bank row. Editing, deleting, or publishing a template needs ownership (its owner, or an admin — 403 otherwise); as a consequence of that admin bypass, an admin may publish or unpublish another teacher's private template, which is accepted by design. Templates never freeze, since they carry no exam tie. Images mirror exam question images (one illustration, one picture per option on `choice` templates; raster types only, ≤ the school's `max_file_bytes`), owner-gated to write and readable by whoever may see the template"),
        (name = "attendance", description = "Attendance summary reports: event tallies + per-course lesson roll call with rates. Own report at `/me`; another user's needs teacher+ (teachers narrowed to their courses) or a parent link to that student"),
        (name = "exams", description = "Exams (per course, weighted by their kind — see `settings`): results, sync/async/open scheduling, attempts (students only — staff never sit) with retakes (`max_attempts`, 0 = unlimited) and a live rejoin door (`allow_rejoin`), questions/answers, and live monitoring (no-shows flagged `absent` once the window closes). Questions may carry images: one illustration per question (any kind — the map the prompt asks about) and one picture per option on `choice` questions, uploaded per slot (multipart, raster types only, ≤ the school's `max_file_bytes`) and frozen with the rest of the question once attempts exist; bytes are served to course managers and to enrolled students once they hold an attempt. Students mirror this on the answer side — one drawing per question inside their attempt (`POST /exams/{id}/attempt/answers/{qid}/image`, same raster types and size cap), saved like any answer through the writable-attempt gate and read back by the student (own) or a course manager (grading). Each sitting keeps its own answers, drawings, and mark keyed by the attempt `seq` — a retake starts from a blank sheet without erasing the prior one; the grading views show the latest sitting, a course manager reads any prior one via `GET /exams/{id}/students/{user}/attempts[/{seq}/answers]` (and the mark history at `.../marks`), and the latest seq is the grade-of-record. When the teacher turns on `allow_review`, a marked student reads back the answer key (`GET /exams/{id}/review/questions`, `correct` included) and their own sittings through `GET /exams/{id}/review/attempts[/{seq}/answers[/{qid}/image]]` (own-scoped — never another student's, and only once an `ExamResult` proves they were marked). A modeless exam is offline-graded — attempts on it are a 409. An exam still being written can be saved as a **draft** (`draft: true` on create, published later by `PATCH`ing `draft: false`): drafts are visible only to the course's managers (students get 404s), cannot be sat or graded, and once attempts or results exist an exam cannot be re-drafted. Not in this spec (WebSocket): the student exam room at `GET /exams/{id}/attempt/ws` — JSON frames; entering clears the attempt's `left_at`, leaving mid-attempt stamps it. See the README's \"Taking an exam\" section for the protocol. `GET /exams` takes an optional `?starts_after=&ends_after=` schedule window (unix-millis), applied after visibility and draft filtering, that keeps only upcoming/unfinished exams and lists them soonest-first (window-less exams — no `mode`, or `open` — drop out); without it the list stays newest-first"),
        (name = "meals", description = "The school's food program. A manager+ publishes one menu per calendar day and meal slot (`POST /meals/menus` — `date` is text, `YYYY-MM-DD`, and `slot` must be one of the school's `meal_slots` from `GET /settings`), optionally capped by a `capacity`; the day+slot pair is unique, so a second publish for the same meal is a `409` — edit the first one instead. The slot is snapshotted as text, never a link, so retiring a slot in settings leaves every published menu intact (and a slot a menu already used cannot be retired at all, `409`). Only `capacity` is patchable: `date` and `slot` are immutable, since a menu on another day is another menu. Dishes hang off a menu (`POST /meals/menus/{id}/dishes`, at most 50, edited and removed at `/meals/dishes/{did}`), each with an optional description, dietary tags drawn from the school's `dietary_tags`, and a `price_minor` in **minor units** (kuruş) as an integer — this API never speaks decimals or floats about money. Deleting a menu takes its dishes with it. A student carries a **dietary profile** of tags from that same `dietary_tags` list plus a free-text note for the kitchen (`GET /meals/profiles/me`, `GET /meals/profiles/{user}` — own always, otherwise teacher+ or a parent link — and `PATCH /meals/profiles/{user}`, **manager+**: an allergen list is a safety record the school keeps, not a self-service preference, so a student editing their own is a `403`). Because both sides are tagged from one vocabulary, every dish a menu read returns carries a `conflicts` list — the intersection of the dish's tags with the *calling user's* profile tags, empty when they do not overlap and empty for every reader without a profile (a manager sees `conflicts: []`, which is correct, not a bug). Reads are open to every authenticated user (a student needs to see what is being served); every write is manager+. Students take seats on a menu (`POST /meals/menus/{id}/bookings`): a student books for themselves, a parent for a student they hold a link to (the one write a `parent_link` authorises) — staff never book for a child (`403`). One row per (menu, student), so booking twice is the same seat; a full menu is a `409`, and that cap is serialized in-process, since a cross-record count is not conflict-checked against a concurrent insert. Cancelling (`DELETE /meals/bookings/{bid}`, answering `200` with the flipped row) is a **status transition, not a delete**: the row stays as `cancelled` with a `cancelled_at` stamp so the freed seat remains auditable, only `booked` rows count against the capacity, and re-booking flips the same row back. The call is **idempotent** — cancelling an already-cancelled seat is a `200` that replays the refund, not a `409`, so a cancel cut short between the flip and the reversal is recovered by simply repeating it. The school's `meal_cancel_cutoff_minutes` closes both ends at once — inside that window neither booking nor cancelling lands (`409`) — measured back from the instant the meal is served: the menu's `date` plus the slot's `serving_minute` from `GET /settings` (minutes past midnight **UTC** — this API stores no school timezone, so staff enter UTC). A slot with no `serving_minute` set falls back to midnight UTC of that date, the pre-serving-time behaviour. The slot list is read live at booking and cancelling time, so correcting a serving time moves the deadline of menus already published for it. A menu with a live booking refuses deletion (`409`). `GET /meals/bookings/me` returns the seats held for the caller plus, for a parent, the seats held for every student they *currently* hold a link to — the links are re-read on every call rather than trusted from the booking's `booked_by` stamp, so an unlinked parent stops seeing the child's seats at once, including ones they booked themselves; `GET /meals/menus/{id}/bookings` is the kitchen's per-menu list (manager+), cancelled rows included. The money is an **append-only ledger** — no route edits or deletes a line, and no balance is stored anywhere: it is always the fold `credits + reversals - charges` in minor units, so a negative balance means the student owes the school. Booking is what charges, at a **price snapshot** (the menu's dishes summed the moment the seat is taken and frozen onto the booking row, under the same in-process lock every dish write takes, so no dish can land between the sum and the seat), so a later price edit never moves an existing charge and re-booking after a cancel bills the then-current price; a menu whose dishes sum past the chargeable maximum refuses the booking outright (`400`) rather than seating anyone unbilled. Booking the same seat twice bills once — the charge and its reversal are keyed by (booking, attempt) and written under the same in-process lock as the seat, so eight simultaneous `POST`s hold one seat and write one charge line, a retried cancel refunds once, and a half-written attempt heals when the request is repeated. A seat taken while the menu was free records *that it was free*, so pricing the menu afterwards never bills it retroactively. Cancelling appends a `reversal` for that exact amount pointing at the charge — the charge row itself stays. Who actually ate is recorded separately by teacher+ (`POST /meals/menus/{id}/attendance` with `{student_id, status}` — `served` or `missed`, the canteen's own fixed pair rather than the school's editable roll-call statuses; one row per (menu, person), so re-marking flips that row; the target need only exist, since a canteen also feeds staff and a mark records what happened rather than who was entitled), read back per menu (`GET /meals/menus/{id}/attendance`) or per student (`GET /meals/attendance/{user}`, `?from=&to=` inclusive `YYYY-MM-DD` bounds on the menu's day, gated like the other per-student reports: teacher+ or a parent link). Meal attendance has **zero billing effect**: a no-show still pays, since the seat was reserved and the food was cooked — no penalty, no refund-on-missed, no auto-reversal; and a walk-in marked `served` without a booking is recorded but never charged, since charging is booking's job alone. `POST /meals/credits` records money received (`{student_id, amount_minor, method?, note?}`) and is **admin-only**, not manager — the backend speaks to no payment gateway and stores no card data; an over-credit is corrected with a compensating line, never a fix-up. `GET /meals/balance/me`, `/meals/balance/{user}`, and `/meals/ledger/{user}` (paged) read it: your own always, another student's only as teacher+ or as a parent linked to them"),
        (name = "marks", description = "Weighted mark reports per course and overall, labeled by the school's grade bands when configured. Each mark counts its exam kind's settings-configured weight times (weight 1 when the kind was since removed from settings). Own report at `/me`; another user's needs teacher+ (teachers narrowed to their courses) or a parent link to that student"),
        (name = "settings", description = "School policy, one singleton: exam kinds with their course-average weights, attendance statuses, grade-display bands, and the per-file upload size limit (`max_file_bytes` — note files and question images alike). An exam kind whose exams already carry marks cannot be removed from the list (409) — those marks would silently re-weight. Also the food program: the meal slots menus are published for — each with an optional `serving_minute`, the minutes past midnight **UTC** at which it is served (there is deliberately no school-timezone setting, so a UTC+3 school enters `540` for a noon meal; `null` = unset, and the cutoff then counts from midnight UTC) — the dietary tags a dish or a student's profile may carry, and `meal_cancel_cutoff_minutes` — one knob closing both booking and cancelling ahead of a slot's serving time (`null` = no cutoff). A slot a menu was already published for cannot be removed (409), same reason. Read: any authenticated user; edit: manager+"),
        (name = "subjects", description = "A course's curriculum topics. Created and listed under `/courses/{id}/subjects`; lookup/edit/delete at `/subjects/{id}`. Every exam question links to one of its course's subjects, so a subject in use cannot be deleted (409) — re-tag or delete the questions first. View follows the course (enrolled users, creator, assigned teachers, manager+); edit follows course management rights"),
        (name = "homework", description = "Course homework: a teacher assigns it per course (`POST /courses/{id}/homework`), tagged with one of the course's subjects (re-taggable; a subject with homework refuses deletion until it is re-tagged or removed) and given a future `due_at`, to the whole enrolled course or an optional `assigned` subset (empty means the whole course — whoever is enrolled when they submit; a subset cannot later be narrowed so as to strand work that already exists). Students submit optional text plus files — any content type, up to 10 per submission, each capped by the school's `max_file_bytes`, always served back as forced attachment downloads (never inline); a submission touched after `due_at` is flagged late (computed from the moving last-touch stamp, never stored — the first-submit stamp is immutable) and stays editable until it is graded. A teacher grades a status (`done`/`incomplete`/`missing`) with an optional 0–100 mark — grading freezes the submission until the grade is removed (un-grading reopens it), never targets the grader themselves, and covers work never handed in (that is how `missing` is set; the student reads the verdict at `GET /homework/{id}/result`). Beyond a teacher-set `missing` the roster and reports also compute a `missing` for anyone unsubmitted past due, and the roster keeps a straggler's stale work visible (flagged `unenrolled`) after an unenrollment or promotion. Homework marks stay out of the weighted `/marks` averages. Students see and fetch only the homework they are assigned (others 404, so a subset assignment never leaks); a linked parent reads a student's homework report — statuses, late flags, marks — but never the submitted files"),
        (name = "terms", description = "Academic terms (semester/trimester/quarter — whatever the school runs); courses may link to one. A term still linked by any course cannot be deleted (409) — unlink those courses first. Read: any authenticated user; edit: manager+"),
    ),
)]
struct ApiDoc;

/// Registers the `session` cookie as an API-key security scheme so protected
/// operations render an auth requirement in the docs.
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "session_cookie",
                SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::new("session"))),
            );
        }
    }
}

/// Assemble the full application router. Shared by `main` and the test suites.
///
/// Also serves interactive docs: Swagger UI at `/swagger`, raw spec at
/// `/api-docs/openapi.json`. Root `/` mirrors the health probe.
pub fn build_router(state: AppState) -> Router {
    // The AI bridge's two database-backed halves, wired here because this is
    // the only place holding the bridge and the database at once: the
    // `ai_worker` gate (so peers can see the workers this process holds) and
    // this replica's chat claim loop. `attach_presence` returning `true` means
    // this call was the first, which is what keeps it to one loop per process.
    if let Some(bridge) = state.ai.clone()
        && bridge.attach_presence(state.db.clone())
    {
        web::chatbot::spawn_claim_loop(bridge, state.db.clone());
    }

    let (router, api) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .route("/", get(health))
        .routes(routes!(health))
        .routes(routes!(server_time))
        .merge(web::limits::routes())
        .nest("/ai", web::ai::routes())
        .nest("/auth", web::auth::routes(&state.rate_limit))
        .nest("/chatbot", web::chatbot::routes())
        .nest("/users", web::users::routes())
        .nest("/notes", web::notes::routes())
        .nest("/messages", web::messages::routes())
        .nest("/events", web::events::routes())
        .nest("/appointments", web::appointments::routes())
        .nest("/courses", web::courses::routes())
        .nest("/sessions", web::sessions::routes())
        .nest("/exams", web::exams::routes())
        .nest("/marks", web::marks::routes())
        .nest("/meals", web::meals::routes())
        .nest("/work", web::work::routes())
        .nest("/pomodoro", web::pomodoro::routes())
        .nest("/questions", web::questions::routes())
        .nest("/bank-questions", web::bank_questions::routes())
        .nest("/attendance", web::attendance::routes())
        .nest("/settings", web::settings::routes())
        .nest("/subjects", web::subjects::routes())
        .nest("/homework", web::homework::routes())
        .nest("/terms", web::terms::routes())
        .split_for_parts();

    // Catch-all per-IP limit over every route (Swagger included). Kept inside
    // the CORS layer so a 429 still carries the CORS headers a browser needs
    // to surface the error to frontend code.
    let api_limiter = RateLimiter::per_minute(
        state.rate_limit.api_per_minute,
        state.rate_limit.trust_proxy,
    );

    let db_up = state.db_up.clone();

    let cors_allowlist = cors_allowlist_from_env();
    if state.cookie_secure && cors_allowlist.is_empty() {
        tracing::warn!(
            "COOKIE_SECURE is on but CORS_ALLOWED_ORIGINS is unset: production should list its frontend origins explicitly; mirror mode runs uncredentialed, so browser frontends cannot send the session cookie"
        );
    }

    router
        .merge(SwaggerUi::new("/swagger").url("/api-docs/openapi.json", api))
        .with_state(state)
        // Conditional-GET: revalidatable `ETag` on 200 JSON GETs, `304` on a
        // matching `If-None-Match`. Innermost, so it sees the handler's own
        // response (mutations and errors pass straight through untouched).
        .layer(middleware::from_fn(web::etag::etag))
        .layer(middleware::from_fn(move |req: Request, next: Next| {
            let health = db_up.clone();
            async move { db_guard(health, req, next).await }
        }))
        .layer(middleware::from_fn(move |req: Request, next: Next| {
            let limiter = api_limiter.clone();
            async move { limiter.enforce(req, next).await }
        }))
        .layer(cors_layer(cors_allowlist))
        .layer(TraceLayer::new_for_http())
}

/// Refuse work the database cannot currently do, and cap how long any request
/// may wait on it.
///
/// Both halves exist because a query issued while the database socket is down
/// never fails — the SDK parks it until the connection returns, so without a
/// guard a handler waits out the entire outage holding a connection open.
///
/// Order matters. The liveness check comes first and is the honest path: it
/// answers before the request touches the database, so nothing is queued and
/// the caller's retry cannot double-apply a write. The timeout only catches
/// requests that slipped through in the window between the socket dying and
/// the keepalive noticing — those are already queued, hence the weaker
/// [`AppError::DbTimeout`] verdict.
///
/// Long-lived responses are unaffected: a WebSocket upgrade and an SSE stream
/// both return their response immediately and do the work afterwards, so
/// neither is measured against the timeout.
async fn db_guard(health: state::DbHealth, req: Request, next: Next) -> Response {
    // `/limits` never touches the database — it serializes constants and this
    // process's own configuration. Refusing it during an outage would be a
    // pure own-goal: a frontend booting into a degraded backend is exactly
    // when it needs the validation contract, and answering `503` would push it
    // back to the hard-coded copy this endpoint exists to delete.
    if req.uri().path() == "/limits" {
        return next.run(req).await;
    }
    if !health.is_up() {
        return AppError::DbUnavailable.into_response();
    }
    let timeout = std::time::Duration::from_secs(constant::REQUEST_TIMEOUT_SECS);
    match tokio::time::timeout(timeout, next.run(req)).await {
        Ok(response) => response,
        Err(_) => AppError::DbTimeout.into_response(),
    }
}

/// Parse the `CORS_ALLOWED_ORIGINS` (comma-separated) allowlist; empty when unset.
fn cors_allowlist_from_env() -> Vec<HeaderValue> {
    std::env::var("CORS_ALLOWED_ORIGINS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|origin| !origin.is_empty())
        .filter_map(|origin| origin.parse().ok())
        .collect()
}

/// CORS for a cookie-authenticated API. `CorsLayer::permissive()` would send
/// `Access-Control-Allow-Origin: *`, which browsers refuse to combine with
/// credentialed (cookie) requests — so origins from the `CORS_ALLOWED_ORIGINS`
/// allowlist are echoed back with `Access-Control-Allow-Credentials: true`.
///
/// With no allowlist (dev) the caller's origin is mirrored, but WITHOUT
/// credentials: mirror + credentials would let any website ride a visitor's
/// session cookie, so the two must NEVER be recombined (the cookie's
/// `SameSite=Lax` in `web/auth.rs` is the only other guard on that door). A
/// cross-origin dev frontend can't send the Lax cookie anyway, so credentials
/// bought nothing in mirror mode; a credentialed browser frontend requires
/// listing its origin in `CORS_ALLOWED_ORIGINS`.
///
/// Takes the allowlist as a parameter (env read once in `build_router`) so
/// tests can exercise both modes without racing on process-global env vars.
pub fn cors_layer(allowlist: Vec<HeaderValue>) -> CorsLayer {
    let layer = CorsLayer::new()
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([header::CONTENT_TYPE])
        // Contract headers cross-origin JS must be able to read: `Retry-After`
        // on 429s and `Content-Disposition` (original filename) on downloads.
        .expose_headers([header::RETRY_AFTER, header::CONTENT_DISPOSITION]);

    if allowlist.is_empty() {
        layer
            .allow_origin(AllowOrigin::mirror_request())
            .allow_credentials(false)
    } else {
        layer
            .allow_origin(AllowOrigin::list(allowlist))
            .allow_credentials(true)
    }
}

/// Liveness probe.
#[utoipa::path(
    get,
    path = "/health",
    tag = "meta",
    responses((status = 200, description = "Service is up")),
)]
async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

/// The server's current time. All API timestamps are UTC unix-milliseconds
/// judged by this clock (session expiry included), so a frontend that renders
/// countdowns or "is this in the past?" logic should not trust the device
/// clock — fetch this once, keep `offset = now - Date.now()`, and add the
/// offset to `Date.now()` whenever it needs the authoritative time.
#[derive(serde::Serialize, utoipa::ToSchema)]
struct TimeResponse {
    /// Current server time, UTC unix-milliseconds.
    #[schema(example = 1_752_275_000_000_i64)]
    now: i64,
}

#[utoipa::path(
    get,
    path = "/time",
    tag = "meta",
    responses((status = 200, description = "Current server time (UTC unix-milliseconds)", body = TimeResponse)),
)]
async fn server_time() -> Json<TimeResponse> {
    Json(TimeResponse {
        now: domain::timestamp::Timestamp::now().as_millis(),
    })
}
