pub mod ai;
pub mod config;
pub mod constant;
pub mod database;
pub mod db;
pub mod domain;
pub mod error;
pub mod module;
pub mod rate_limit;
pub mod service;
pub mod state;
pub mod telemetry;
pub mod tenant;
pub mod validate;
pub mod web;

use axum::extract::{MatchedPath, Request};
use axum::http::{HeaderName, HeaderValue, Method, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::request_id::{
    MakeRequestId, PropagateRequestIdLayer, RequestId, SetRequestIdLayer,
};
use tower_http::trace::TraceLayer;
use utoipa::openapi::security::{ApiKey, ApiKeyValue, SecurityScheme};
use utoipa::{Modify, OpenApi};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use utoipa_swagger_ui::SwaggerUi;

use crate::constant::MAX_REQUEST_ID_LEN;
use crate::error::AppError;
use crate::module::Module;
use crate::rate_limit::RateLimiter;
use crate::state::AppState;
use crate::telemetry::SchoolSlot;
use crate::web::module_gate::gate;

/// Top-level OpenAPI document. Per-path operations and schemas are collected
/// automatically from the `#[utoipa::path]`-annotated handlers via `utoipa-axum`.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Hezarfen Backend API",
        version = env!("CARGO_PKG_VERSION"),
        description = "Notes, events, attendance, courses, and weighted exam marks behind cookie-session auth. Notes carry file attachments (PDFs, documents — stored on disk, capped per file by the school's settings). Any two users can message each other one-to-one, mail-style: each side files its own copy through folders (recipient: inbox/archive/trash with a read flag the sender sees as a receipt; sender: sent/archive/trash), and a message is truly gone only when both sides have deleted it from their trash. Exams run sync (one window), async (window + per-attempt duration), or open (sit anytime), with per-exam attempt limits (retakes) and a teacher-controlled rejoin door on the live exam room; questions can carry images — an illustration on any question, and per-option pictures on choice questions; students can attach an answer drawing to any question in their attempt. Enrollment, exam-sitting, roll call, and marks are student-only — staff run them, they don't take part. Every event carries an audience — the whole school, one role, a course's enrollment, or a registration list — that defines its expected-attendee roster (visibility is unaffected: everyone sees every event). Registration-audience events hold a signup list built one seat at a time: teachers register students (students never register themselves), staff register only themselves, the list optionally caps at a capacity, and it closes the moment the event starts. Event attendance is taken by teachers+ against that audience (students never self-mark), and a per-event roster report joins the expected list with the marks to show who missed. Courses come in three behaviorally identical kinds — `course` (a regular taught course), `study` (a supervised study session — etüt), and `club` (a student club — kulüp) — may cap their roster with an optional enroll-time `capacity` (a full course refuses new members), and carry lesson sessions with teacher-taken roll call. A course is run by whoever created it plus any teachers a manager assigns to it (`POST /courses/{id}/teachers`) — an assigned teacher manages everything inside the course (exams, sessions, subjects, roster, grading) but cannot delete the course or change who else teaches it, and a demotion below `teacher` drops their assignments. Students can also be grouped into class sections (şube — a `9-A`), which are bulk enrollment rather than a second kind of membership: attaching a course to a class enrolls its whole roster, adding a member enrolls them into every course the class already carries, and what gets written are ordinary enrollment rows tagged with the class that pumped them (that tag rides out as the enrollment response's `source`; `null` = placed by hand, and a hand-placed row is never adopted, never swept — enrolling a pumped student by hand clears the tag, the mirror of a manual unenroll winning permanently). An attach or a member-add that any target course has no seat for is refused whole (409 naming that course), as is one whose class already holds `max_class_members` students or `max_class_courses` courses (200 and 50: each add writes one enrollment per row on the other axis in a single transaction, so each ceiling is the bound on the other's write loop) or one whose class carries a link to a course that no longer exists (409 naming it — detach that link, no capacity will help); removing a member or detaching a course takes back only the rows that class pumped — re-tagging a row a second class still claims instead of dropping it — and a class still holding members or courses refuses deletion. Each course also owns its curriculum as a list of subjects, and every exam question and every homework must be tagged with one of its course's subjects (a subject still referenced by questions or homework cannot be deleted); staff clock in/out on a server-stamped work log; students run a pomodoro study timer whose focus sessions land in a server-stamped log (starting discards a dangling unfinished session; teachers can read any student's log and its total focus time; every stint is logged, but only one of at least `pomodoro.min_counted_ms` and within the UTC day's `pomodoro.max_counted_per_day` *counts* towards the badges, which is what keeps a start/finish loop from buying them); attendance reports tally events and per-course roll call with rates. Courses also hand out homework: teacher-assigned per course to the whole course or a named subset the unnamed never even see (404s, no existence leak) and the named see only themselves in, subject-tagged and due by a required future `due_at`; students submit optional text plus up to 10 files of any content type (each capped by `max_file_bytes`, always served back as forced attachment downloads), editable until a teacher grades a status (`done`/`incomplete`/`missing`) with an optional 0–100 mark — the grade freezes the submission until removed, work never handed in is gradable `missing`, lateness is computed from an immutable first-submit stamp and a moving last-touch stamp (never stored), the grading roster computes `missing` and `unenrolled` flags, and homework marks stay out of the weighted `/marks` averages. School-varying policy (exam kinds with their course-average weights, attendance statuses, grade-display bands, the note-file size limit) lives in an editable settings singleton, and courses may link to academic terms. Each account also carries its own UI preferences — theme (`light`/`dark`), language (`tr`/`en`), and accent color (`palette_color`, a 6-digit hex like `#fefae0` — any hex, not a fixed palette) — self-managed, admin-editable for anyone, and `null` until chosen (the client then follows the device preference). A `parent` role observes without touching: admins tie any number of students to a parent account (`POST /users/{id}/students`), the parent lists them at `GET /users/me/students` and reads each one's mark, attendance, pomodoro, and homework reports — and nothing else; parents hold no staff power and never act as students (no enrolling, sitting exams, or roll call). A school-wide **question pool** lets students ask for help: a student posts a question (optionally with a photo of the problem — same raster rules and size cap as question images), a teacher+ approves it into the pool (or deletes it — rejection is deletion, there is no rejected state), and every approved question is readable by the whole school (parents excepted), with anyone free to offer solutions under it — text plus an optional photo of the worked steps, both editable by the solution's author at any time, since solutions are unmoderated and never freeze — and every question row carries its `solution_count`; pending questions are visible only to their asker and to teacher+ (whose `?status=pending` list is the approval queue), and approval freezes the question's content so nothing unmoderated can slip into the pool afterwards. Teachers keep an **appointment calendar**: a teacher+ publishes availability slots (one-off, or repeating weekly up to an `until` date as a series that deletes as one), and students and parents book a slot with a reason — the booking lands `pending` until the teacher approves it, rejects it, or counter-proposes another time (which sends it back to `pending` for the requester to accept or decline); the requester may cancel until the meeting starts — a teacher ends a booking by rejecting it, or by counter-proposing and then rejecting an approved one — one live booking holds a slot, a teacher's published windows may not overlap each other (`409`, half-open, so back-to-back slots are fine), and no approved meeting may overlap another for the teacher or the requester. While the database connection is reconnecting (a restart, a dropped socket) requests briefly answer `503` with `Retry-After: 1` — transient, retry shortly. Every list endpoint accepts `?limit=&offset=` and returns a `{items, total, limit, offset}` page envelope — paging is opt-in, so omitting `limit` returns the full list and `total` always carries the unpaged count. Every fixed validation bound — field lengths, numeric ranges, and the closed value sets (roles, course kinds, exam and question kinds, homework statuses, uploadable image types) — is published at `GET /limits` (no auth, cache for the session), so a client validates against the server's own constants instead of a hard-coded copy that drifts. It also reports this server's live rate-limit tiers, so a client learns its request budget instead of discovering it by collecting a `429`, and it keeps answering while the database is down — a client booting against a degraded backend still gets the contract.",
    ),
    modifiers(&SecurityAddon),
    tags(
        (name = "meta", description = "Liveness, the authoritative clock, and the validation contract. `GET /health` (and its `/` mirror) reports what the process can currently do — `{status, db, ai}` — answering `503` with a body that names the failing dependency rather than a bare refusal, and it keeps answering while the database is down. `GET /limits` publishes every fixed bound the API enforces — field lengths, numeric ranges, closed value sets (roles, course kinds, exam and question kinds, homework statuses, image content types) — so a client validates against the server's own constants instead of a hard-coded copy that drifts. Unauthenticated, because the registration form needs the username and password bounds before a session exists, and the values change only with a deploy: fetch once, cache for the session. Two refusals split that contract: every JSON-bodied operation answers `422` when the request never became the type the handler asked for (a wrong-typed field, a missing required one), and that one is a plain-text deserializer diagnostic rather than the `{error}` envelope, so it is read by a human and not parsed by a client; `400` carries the usual `{error}` and means either non-JSON bytes or a well-typed value that broke a rule published here. Multipart upload routes have no JSON body and so answer `400`, never `422`. School-adjustable policy lives on `GET /settings`; what `/limits` carries for those knobs is the fixed range a manager may set them within. Its `rate` group is the exception to \"fixed\": those tiers are environment-tunable, so the endpoint serves the running server's live values (`0` = tier off). `GET /modules/catalog` is the same kind of contract for the product's shape: every sellable module, the package it is sold in, and the modules it structurally requires — unauthenticated and deploy-constant, since it is identical for every school. What one school actually bought is `GET /modules` (any logged-in user), the cheap way to hide a nest the school does not have instead of discovering it as a `403`. Those tiers are per client; a server whose client table is saturated by a flood meters otherwise-unknown callers against one shared budget instead, so an unfamiliar client can briefly see a `429` it did not itself earn — always with a `Retry-After`, never longer than the window"),
        (name = "ai", description = "AI-bridge discovery. The AI features run as separate services that dial IN to this backend over QUIC (protocol/ALPN `hab/2`, address `AI_QUIC_ADDR`) and register the capabilities they serve; each request then rides its own QUIC bidirectional stream on that one connection. `GET /ai/certificate` publishes the bridge listener's certificate (PEM + SHA-256 fingerprint) so a service can pin it before dialling — public, because a server certificate is presented to every peer during the TLS handshake anyway, while the shared token that actually authenticates a service (`AI_SHARED_TOKEN`) is configured out of band. With **both** `AI_TLS_CERT` and `AI_TLS_KEY` unset the bridge self-signs afresh at each boot, so a service must re-fetch this on every reconnect, not only at startup; setting just one of the two (a blank value counts as unset) fails the boot instead of quietly self-signing, since a bridge presenting a throwaway `localhost` leaf to services pinning the real one looks configured and answers `503` to every send. Every frame in either direction names the school it belongs to (`school`, the slug), so one fleet of services serves every school on the deployment and a read is answered out of that school's own database — an unknown slug is `unknown_school`, a suspended one `school_suspended`. Traffic also runs the other way on that same connection: a connected service opens streams of its own to **read this API back** (one `ApiRequest`/`ApiResponse` frame pair per stream, no HTTP session and no password of its own) — `GET` only, against a deny-by-default allowlist of read endpoints (identity, profiles, notes, course notes, homework, marks, attendance, pomodoro), optionally `on_behalf_of` a named user, whose account is re-read live per request so a stale id acts as the demoted or deleted user and never as who they were. Without that field the principal is the internal role `ai`, the lowest privilege here and not assignable to anyone — it appears in the `Role` schema as documentation only, no account can hold it, and a read that needs a role simply answers `403`. A connected service also pulls **file bytes** the same way, on its own stream: one `BlobRequest` naming a `course_note_file` id, one header frame, then exactly `size` raw bytes and FIN — no frame cap, because a PDF fits in no frame. That stream reaches course-note attachments and nothing else, behind the very guard `GET /course-notes/{id}/files/{file_id}` applies, and `on_behalf_of` is required in practice there since the `ai` role can view no course. Nothing on this HTTP surface changes: the allowlisted endpoints are the ones documented here, reached with the same guards. `404` when the bridge is disabled (`AI_QUIC_ADDR` unset). `GET /ai/capabilities` (authenticated) lists what connected services are serving right now — one entry per capability with its worker count and in-flight requests — and answers `200 {enabled: false, capabilities: []}` when the bridge is off: a discovery read showing *that* there is no AI fleet, not a dependency probe. See the README's \"AI bridge (QUIC)\" section for the frame-level protocol"),
        (name = "builder", description = "The vendor surface: one deployment, many schools. A **builder** is the operator who owns the deployment — not a user of any school, and holding no role in one. It logs in at `POST /builder/login` and gets a `session` cookie prefixed `builder.` instead of a school slug; that cookie works here and is `401` everywhere else, exactly as a school cookie is `401` here. `POST /schools` creates a school's database, schema and first admin in one call; the registry row reads `provisioning` until that schema is in place, so a boot cut short mid-create is finished by the next one instead of leaving a school that answers `500`; `PATCH /schools/{slug}` renames it or flips it between `active` and `suspended` (a suspension is immediate and total for that school's users — every request including login answers `403`, live session or not); `DELETE /schools/{slug}` destroys its data and its uploaded files for good. `POST /schools/{slug}/admin-password` is the lockout fix: it re-keys a named admin of that school and revokes every session that account held. `POST /schools/{slug}/enter` mints an ordinary school session for one of its admins (support access) — the only route here refused on a suspended school, so a suspension really does close every door into it. It is also where a school's **entitlements** are sold: `GET /schools/{slug}/modules` lists what it has and what is left, `POST`/`DELETE /schools/{slug}/modules/{module}` flips one (idempotently), and `PATCH /schools/{slug}/modules` re-sells the whole shelf in one atomic write — modules and packages, both directions, expanded into a single resulting set that is checked once, so a `409` names every broken dependency at once and nothing is written unless the request is accepted whole. A change lands on the school's very next request, live cookies included: a disabled module answers `403 {error, module}` on every route in its nest. The catalog itself (`GET /modules/catalog`) and a school user's own set (`GET /modules`) are on the `meta` tag."),
        (name = "auth", description = "Registration, login, session lifecycle, and school selection. The account is a **person**: one global username + password in the control plane, able to hold memberships in many schools — the school is picked after login, never named by it. `POST /auth/register` is still aimed at a school (`{school, username, password}`): it creates the person plus the school's `app_user` (a `student`), or attaches an existing person to one more school when the password matches. It answers `201` with the byte-identical `{username, role}` body in every outcome — created, name already taken, or taken under a different password — and deliberately **no `id`**, since a distinguishable reply would let anyone unauthenticated enumerate accounts (an anti-enumeration measure; log in to learn who you are). `POST /auth/login` takes `{username, password}`: with exactly one active membership it enters that school directly — a `UserResponse` and a `session` cookie valued `<slug>.<token>` (see the `builder` tag for cookie prefixing); with two or more it answers `{username, schools: [{slug, name}]}` and a `person.<token>` cookie, which `POST /auth/school` (`{school}`) exchanges for the chosen school's cookie and a `UserResponse` — the school must be one of the caller's memberships (`401` otherwise) and not suspended (`403`). Suspended schools never appear in the choice list, and a person whose every membership is suspended is `403` like the school's own doors. Login failures — unknown username, wrong password, no membership left to enter — are all the same `401`, and the unknown-username branch burns the same argon2 work a wrong-password check costs, so timing confirms nothing. `POST /auth/logout` revokes whichever session the cookie names — school, person, or builder — and stays idempotent `204` without one"),
        (name = "chatbot", description = "The AI chatbot, relayed through the QUIC bridge to an out-of-process AI service. Every authenticated role may chat (parents included) and every thread is private to its owner — nobody, not even an admin, reads someone else's. A user keeps up to the school's `max_chatbot_threads` threads (409 at the cap, delete one first) and sends at most `RATE_LIMIT_CHATBOT_PER_MINUTE` messages a minute (default 20, `0` disables the tier; 429 with `Retry-After`). A thread can be renamed — or its name cleared — with `PATCH /chatbot/threads/{id}` (`{title}`, `null` to clear); nothing names a thread automatically. Sending is asynchronous: `POST /chatbot/threads/{id}/messages` writes the turn plus an empty `pending` assistant row and answers `202 {message_id, status}` immediately, because an inference outlives a normal request. The pending row is what a reload comes back to, not a queued job: the bridge round trip runs in an in-process task the `202` leaves behind, so a restart mid-inference strands that turn — it settles `failed`/`interrupted` (a reader projects a `pending` row older than 300 seconds as failed, and the boot sweep stamps that verdict durably) rather than being answered by anyone. Accepted, because the backend is a single process with stop-the-world deploys: there is no peer that could have taken the turn over. Sending is also serialized against the thread's own deletion — both rows ride a transaction that writes the thread row, so a turn accepted while `DELETE /chatbot/threads/{id}` is committing is refused with a `404` instead of surviving as a message under a thread that is gone. The answer is then read either by polling `GET .../messages/{mid}` or over the Server-Sent-Events stream at `.../messages/{mid}/stream` (`delta` chunks, then one `done` or `error`) — both read the same row, so they never disagree, and a stream opened after the answer landed still replays it. An answer the service made longer than `max_chatbot_message_len` is clipped rather than discarded, and the stored turn is flagged `truncated: true` — the polling read and the SSE `done` payload always agree on it. A turn always settles: `complete` with the text, or `failed` with a short code (`unavailable`, `busy`, `timed_out`, `transport`, `protocol`, `bad_reply`, `empty_reply`, `interrupted`, or the service's own). With no `chat.reply` service connected anywhere in the deployment, sending answers `503` and writes nothing"),
        (name = "rag", description = "The course-note RAG: its own nest, threads and tables, relayed through the same QUIC bridge as the chatbot — RAG is not folded into the chatbot's storage. Every authenticated role may ask (parents included) and every thread is private to its owner. Every question is scoped server-side to the asker's own `(sınıf, ders)` pairs, derived on each request from their live memberships — their şubeler' instances, a linked parent's children, and the school-wide club/etüt memberships they hold (which scope the subject across grades, `sinif: null`) — never from the body, so a caller cannot widen their own retrieval; a scope past `MAX_RAG_SCOPE_PAIRS` pairs is refused (400) rather than narrowed. A RAG thread draws on the school's existing AI thread seats (`max_chatbot_threads`, the same seat count the chatbot nest uses, one pool per user), its prompts and answers are bounded by `max_chatbot_message_len`, and its per-minute tier is its own: `RATE_LIMIT_RAG_PER_MINUTE` (default 6, `0` disables the tier; 429 with `Retry-After`). Sending is asynchronous exactly as the chatbot's is: `POST /rag/threads/{id}/messages` writes the turn plus a `pending` assistant row and answers `202`, then poll `GET /rag/threads/{id}/messages/{mid}` or open its `/stream` (SSE `delta`s then `done`/`error`). `503` when no AI service offers `rag.chat`, with nothing written. An answer may abstain — `abstained: true` with a `reason` (`guard_*`, `insufficient_data`, `model_abstained`) is a complete, successful turn, not an error — and carries its `citations`, up to `MAX_RAG_CITATIONS` per answer and at most `MAX_RAG_CITATION_PAGES` pages each (a service reply past either cap fails the turn whole). Each citation's corpus `doc_id` is resolved to the course-note file that owns it (`file`, `null` when no file the asker may view claims that document) beside the pages and retrieval span ids behind it."),
        (name = "insights", description = "ZEKA's computed student insights — the `hezarfen_zeka` AI service dials in over the same QUIC bridge as the chatbot and writes its own `zeka_*` rows into each school's database, so this nest reads what it wrote and asks it to compute on demand; the backend never composes an insight row itself. `GET /insights/me` is the caller's own view: a student's summary, their own recommendation cards and their segment profile — and **never** the attention list about themselves, which is a teacher's tool and is not returned to a student or to a linked parent. `GET /insights/students/{id}` is the observer's view of one student — teacher+ narrowed to the students they actually reach through a shared class×course instance, or a parent holding a live link — adding the attention list and the caller's own cards about that student; anyone else reads `404`, never `403`. `POST /insights/students/{id}` and `POST /insights/refresh` (manager+) queue a recompute through the bridge and answer `202` — the compute lands in the service's own rows, so a client polls the read routes; `GET /insights/runs` (manager+) is the run ledger, newest first. Both dispatch doors answer `503` when no service offers the capability, and nothing is queued. Expired and dismissed cards are filtered out of every card read, segment rows below 30 answers (`confidence: none`) are filtered out of every profile read, and a card's `about` is never returned to a student caller."),
        (name = "podcast", description = "Podcast generation, relayed through the same QUIC bridge as the chatbot and the RAG nest — the backend owns authentication and the HTTP surface, the podcast service owns the job. `POST /podcast/jobs` (`{source_id, format?}`) starts one and answers `202` with the service's `{job_id, state, eta_secs}` receipt; `GET /podcast/jobs/{id}` polls its `{state, stage, progress, error_code}`, `GET /podcast/jobs/{id}/result` collects a finished job's `{audio_id, duration_secs, script_id, audio_ids, script_ids, format}` (`409` while it is still running), and `POST /podcast/jobs/{id}/cancel` asks the service to stop it (`cancelled` is \"this call stopped work\", so a finished job answers `false`). Each door refuses `503` when no connected service declares its capability, and each relays the service's own refusal code (`not_found`, `not_ready`, `busy`, `llm_unavailable`, …) with a matching status. The produced audio never crosses the bridge: `audio_id` is a path relative to the school's own output directory under `FILES_PATH` (the service's `PODCAST_OUTPUT_ROOT` points there), and `GET /podcast/audio?path=…` streams those bytes — refusing absolute paths, `.`/`..` segments, backslashes and symlinks that resolve outside the school's directory with `400`, so one school can never read another's episode even knowing its path. **None of these doors writes a row**: the job record and its files live in the service. Gated by the school's `chatbot` module (the AI package's only module), like `/chatbot` and `/rag`; any authenticated member of the school may call them, and the school wall is the bridge's (`hab/2` stamps the caller's school onto every request; the bridge does **not** verify the echo on replies — answers are correlated by frame id alone). What the backend cannot judge is *which* sources a caller may narrate — `source_id` is the service's own record id, and no backend query can say what it names."),
        (name = "users", description = "User info: self-service profile and UI preferences (theme `light`/`dark`, language `tr`/`en`, accent color `palette_color` as `#rrggbb` — returned on every user response, `null` until chosen), name search for the pickers (any authenticated user, optionally role-filtered; a student or parent sees only the staff they may message), plus account creation (admin only: `POST /users` mints a school account outright, born with the role the caller names — `student` by default), listing, lookup, and role/profile/preferences administration (admin only). An admin never changes their own role, and the school's **last** admin cannot be demoted at all (`409`) — role management can no longer lock everyone out, not even when two admins…, accent color `palette_color` as `#rrggbb` — returned on every user response, `null` until chosen), name search for the pickers (any authenticated user, optionally role-filtered; a student or parent sees only the staff they may message), plus account creation (admin only: `POST /users` mints a school account outright, born with the role the caller names — `student` by default), listing, lookup, and role/profile/preferences administration (admin only). An admin never changes their own role, and the school's **last** admin cannot be demoted at all (`409`) — role management can no longer lock everyone out, not even when two admins demote each other at the same instant. Also the parent↔student ties: admins link students to a `parent` account under `/users/{id}/students` (link, list, unlink — a role change on either side drops its ties in the *same transaction* as the role write, so a failure mid-way leaves the old role with all its ties intact rather than a half-applied change), and a parent lists their own students at `/users/me/students`; every one of those lists is filtered to accounts that hold the `student` role **right now**, so a tie whose student was promoted out names nobody, exactly as it grants nothing; the tie grants the parent read access to those students' mark, attendance, pomodoro, and homework reports and nothing else. Every account also has a **public profile** (`GET /users/me/profile`, `GET /users/{id}/profile`): username, `display_name` (the self-chosen one, else the `name surname` join, else `null` — the very same three steps every embedded person ref resolves, so a chosen name shows school-wide; it never replaces `name`/`surname`, which stay the school-office record, and it deliberately stays out of the user search fold), role, `bio`, avatar metadata, a narrowed class/course block (id/name/grade and id/title/kind only — never a class's creator or homeroom teacher — capped at `max_profile_classes` / `max_profile_courses`, since `/classes/me` and `/courses/me` are the full paged lists), a sixteen-key `stats` block, and the badges those stats have earned. Four stats are computed at read (finished pomodoro stints and their focused milliseconds, plus the full course and class counts) and twelve are stored lifetime tallies maintained at write time, so a tally can outlive the rows behind it — a deleted exam does not un-sit it; all sixteen read `0`, never `null`. Five are the student's own work (`homework_submitted_total`, `homework_on_time_total`, `exam_sat_total` — **exams sat**, not sittings: a retake of an exam already counted moves nothing, since an `open` exam with unlimited attempts is a start/finish loop a student runs with no teacher in it —, `pomodoro_finished_total`, `pomodoro_focus_ms_total` — the last two count only stints that **counted**: at least `pomodoro.min_counted_ms` long and within that UTC day's `pomodoro.max_counted_per_day`, since a start/finish pair is two requests with nobody else in it, so `pomodoro_sessions` may legitimately run ahead of them), and seven more cover both sides of the school: `marks_given_total` (grades a teacher recorded, exam sittings and homework alike, once per pair — a regrade moves nothing, but deleting the grade gives it back, since the grader who deletes is the grader who was credited and grade → un-grade → regrade would otherwise count the same work twice), `lessons_held_total` (a session teacher's lessons, credited by the **first roll call taken at or after the lesson's own `starts_at`**, so neither a lesson scheduled and cancelled nor two hundred marked before they begin count for anything), `pool_approved_total` and `pool_published_total` (the approver's and the asker's halves of one pending→approved transition — a teacher approving their own question publishes it but earns neither, since a counter moves only when one person's work was judged by somebody else), `lessons_attended_total` (a **student** marked `present` or `late` at lesson roll call, moved back down by a correction to any other status and untouched by daily/event attendance), `high_mark_total` (the student's exam marks at or above `badges.high_mark_min`, once per sitting, given back when the mark is deleted on the same terms as the grader's — but unlike the grader's, a **regrade does move it** whenever the correction crosses the cut in either direction, since it counts marks *stored* above the line rather than first gradings, which is the only way the credit and the refund read the same value; homework marks are optional and often status-only, so they never count), and `study_streak_total` — the **longest** run of consecutive days the student finished a pomodoro stint that counted, not a sum, spelled `_total` like every other key so a client joins it to the catalog's `stat` name mechanically. Days there end at **midnight UTC**, like every other day calculation here, and the run is read as a high-water mark, so breaking it never takes a badge back. **Badges are auto-earned only** — no route awards or revokes one; a badge appears when a stored counter crosses a hardcoded threshold and is **permanent**, surviving that counter falling back (withdrawing a submission decrements the counter, never the badge), with `earned_at` pinned at the first crossing and the list ordered oldest first. Each entry is `{id, earned_at}`: labels and icons belong to the client, keyed by id, exactly as for `roles` and course `kinds`. The catalog behind the ids is `badges.catalog[]` at `GET /limits` (`{id, stat, threshold}`, 34 badges over twelve counters, `stat` being the API's name for the counter rather than the column it is stored in) alongside `badges.high_mark_min`, the exam mark the `high_mark` ladder counts from (the id alone does not say it), both compiled in, so a threshold moves only with a deploy. Contact details (`email`, `phone`, `birth_date`) are **not** part of it and keep exactly the gate they have today. Any authenticated account reads any profile — except a `parent`, who reads their own and their linked students' only, with the link *and* the target's live role re-read per call, so an unlinked or promoted-out student stops being readable at once. The courses block follows the owner's **live** role: teacher+ lists what they teach, everyone else what they are enrolled in, so a demoted ex-teacher lists enrollments and not what they created — and it is then cut to the courses the *reader* may already read at `GET /courses/{id}` (a title a reader is `403` on there must not arrive here instead), so a stranger sees an empty block while the owner and manager+ see it whole; the truncation runs after that filter. The class block is cut the same way, against the bar `GET /classes/user/{id}` holds: teacher+, a parent linked to that student, or the owner reading their own profile — every other reader (a fellow student included) gets an empty `classes` array rather than a `403`, since a class name and grade are exactly what the `/classes` routes withhold below teacher+. **Every** number in `stats` stays the owner's true total either way, not just those two — `pomodoro_focus_ms`/`pomodoro_focus_ms_total` are the same figure `GET /pomodoro/{id}` serves behind its observer gate, and `lessons_attended_total`, `homework_submitted_total`, `homework_on_time_total`, `exam_sat_total` and `high_mark_total` are magnitudes of the mark and attendance data behind that same gate (for a reader who is exactly a `teacher`, wider than the `can_manage_course`-narrowed `/marks/user/{id}` and `/attendance/user/{id}` reports). That is deliberate: they are motivational counters, a magnitude names no course, class, lesson or exam, and a per-reader figure would make one profile read differently to different people. `display_name` (≤ 50) and `bio` (≤ 500) ride the same `PATCH /users/me` / `PATCH /users/{id}/profile` pair as the rest of the personal info, `\"\"` clearing either. One avatar per account: `POST /users/me/avatar` (multipart, raster types only — no SVG, whose script can run when the bytes render inline school-wide — at most the school's `max_file_bytes` rather than a knob of its own; replacing one drops the previous picture), the bytes at `GET /users/{id}/avatar` — or `GET /users/me/avatar` for the caller's own, so a client never needs to know its own id — (same reach as the profile, served `nosniff` and `private, no-store`), removed by the owner at `DELETE /users/me/avatar` or, as moderation, by an admin at `DELETE /users/{id}/avatar`"),
        (name = "notes", description = "Per-user notes CRUD, plus file attachments per note (multipart upload, download, delete — at most 10 per note, each at most the school's settings-configured `max_file_bytes`). Every authenticated role keeps notes, `parent` included: a note is private own-scoped data with no other reader, and the routes carry **no role bar at all** — ownership is the whole authorization, so nobody but the owner ever reaches a note or its files (a stranger's id is a `404`, never a `403`). That is deliberate and it is the fix for a real defect: while these routes required `student`, a demotion to `parent` locked the owner out of their own notes for good, and since nothing else in the crate reads a note and no route cascades one, the rows and their on-disk blobs became unreachable and undeletable by everybody. Own-scoped personal data is not confiscated by a role change"),
        (name = "messages", description = "One-to-one mail-style messages, upward only below staff: a student or parent writes to a teacher, manager or admin (parents included — messaging is the one place a parent writes) and never to another student or parent (`403`), while staff write to anyone, each optionally tagged with a free-text `label` badge. Each party owns their copy independently: the recipient's moves through `inbox`/`archive`/`trash` with a read flag (visible to the sender as a read receipt), the sender's through `sent`/`archive`/`trash`. Filing a copy away records the folder it left as its `previous_folder`, so restoring it puts it back where it came from (a trashed archive copy returns to the archive, not the inbox). `GET /messages?folder=` lists one folder (`&read=false` narrows to unread — with `limit=1`, `total` is the unread badge); permanent deletion (`DELETE`) works only from the trash and removes the row once both sides have deleted"),
        (name = "appointments", description = "Teacher availability slots and the meetings booked on them. A teacher+ publishes windows (`POST /appointments/slots`), optionally repeating weekly up to an `until` date (at most 52 occurrences, all sharing a `series` id that deletes as one). A published window may not overlap another of the *same teacher's* (`409`): the check is half-open, so 10:00–10:30 and 10:30–11:00 are two slots and not a collision, and a weekly publish is all-or-nothing — one colliding occurrence refuses the whole series. Students and parents book one with a required reason — a parent books for themselves, this is the parent-teacher conference — and the request lands `pending`: publishing availability is not consent to a person and a topic. The teacher approves, rejects, or counter-proposes another time; a counter-proposal sends the booking back to `pending` and clears `decided_by`, and the requester either accepts it (approval at the new time) or declines, which cancels the booking. A window that has already opened is closed on every side (409): such a slot cannot be booked — and the bookable list is bounded on that same `starts_at`, so it never offers one — an underway time cannot be approved, accepted, or counter-proposed, and the requester may cancel — declining included, since a decline *is* a cancel — only until the meeting's effective window starts. Cancelling is the **requester's** route alone (`403` for anyone else, the slot's teacher and a manager/admin included, since the guard compares ids and not roles): a teacher ends a booking by rejecting it while it is `pending`, and an approved one by counter-proposing another time (back to `pending`) and then rejecting it, or simply by rescheduling. Both the reject and the cancel take an optional `reason`, and the row records who settled it (`decided_by`, `cancelled_by`) with that text (`reject_reason`, `cancel_reason`) — visible only to the requester and the slot's teacher, since that is who a booking is ever rendered to. One live booking holds a slot (rejecting or cancelling frees it), no approved meeting may overlap another for the teacher *or* the requester (409), and a slot with a live booking refuses deletion (409). A demotion below `teacher` withdraws that account's whole calendar in the role change's own transaction and cancels the live bookings on it (nothing else could reach them afterwards — the requester keeps a `cancelled` row naming who dropped it and why); a slot an older build stranded is inert instead: it drops out of the bookable list and booking one is refused. The bookable list carries each slot's teacher identity (id, username, display name) to every caller, parents included — deliberate, since a conference cannot be booked from an anonymous calendar; it is the staff directory for booking, narrowed to whoever published bookable time, and the direct people routes stay closed to a parent"),
        (name = "events", description = "Calendar events and attendance. An event's `audience` is its expected-attendee roster, not a visibility wall: everyone sees every event. Aim it at the whole school, one role, a course's enrollment, a class section's (şube) roster, or a hand-built registration list. Rosters resolve live, so role changes, (un)enrollments and class-roster changes move people in and out by themselves. Attendance: teacher+ marks people in the audience (students never mark, not even themselves), and the roster report joins the expected list with the marks to show who missed. Registration lists are filled seat by seat (teachers place students, staff place only themselves), optionally capped by `capacity`, and close once the event starts. `GET /events` takes an optional `?starts_after=&ends_after=` schedule window (unix-millis) that keeps only upcoming/unfinished events and lists them soonest-first; without it the list stays newest-first"),
        (name = "courses", description = "The course **catalog**: the school's reusable rows — kind `course` (a regular ders), `study` (etüt, a supervised study session) or `club` (kulüp) — with their curriculum subjects (`/courses/{id}/subjects`). A catalog row teaches nobody by itself: a ders is taught by attaching it to a şube, which mints an instance (`POST /classes/{id}/instances`), and everything a class actually runs — roster, teachers, exams, lessons, homework — belongs to that instance (`GET /instances/{id}`). An etüt or kulüp has no şube at all: it is joined school-wide (`POST /courses/{id}/members`, students only), and a `course`-kind ders refuses that door with a `400` — its students come through its instances. Catalog rights are the office's and the row's creator: edit, delete, its subjects and its member list all need the creator or manager+, and a row still taught anywhere (any instance, any member) cannot be deleted (`409`). `GET /courses/me` is what a caller is reached by, both tiers together; the instances themselves are `GET /instances/me`."),
        (name = "instances", description = "One catalog course **as one şube teaches it** — the instance, and the anchor everything academic hangs off. Attaching a course to a class (`POST /classes/{id}/instances`) mints one; two şubeler teaching the same course are two instances with their own roster, teachers, exams, lessons and homework. The row carries the instance's own policy: `ders_saati` (weekly lesson hours — the weight the instance takes in the year's karne average) and `counts_toward_karne`. Acting on an instance (`PATCH`, its rosters, its exams/sessions/homework, its roll-call) is open to manager+, to a teacher assigned to that instance (`POST /instances/{id}/teachers`, manager+) and to the şube's homeroom teacher — one rule, applied by every route here. `GET /instances/me` is a student's own list: the instances of the şubeler they are a live member of."),
        (name = "course-notes", description = "Notes a teacher attaches to a **catalog course** — announcements, recaps, anything worth pinning to the course rather than to one student — with file attachments, mirroring the personal `notes` stack. Writing (create, edit, delete, upload, delete a file) requires teacher+ and catalog rights over the course (its creator, or a manager/admin); reading (`GET /course-notes/{id}`, listing, downloading) is open to anyone the course reaches — a student enrolled in one of its instances, a member of it, its creator, or a manager/admin. `GET /course-notes` requires `?course=` and lists that course's notes only, newest first. At most 10 files per note, each at most the school's `max_file_bytes`. `GET /course-notes/{id}/rag` serves what the backend indexed for the AI features, and is derived data: deleting it suppresses nothing, and the next edit to the note or its files may regenerate it."),
        (name = "classes", description = "Class sections (şube): a named set of students the school moves as one, sitting in an academic year (`year`; a şube with no year takes no exam and is never rolled over). A class is bulk enrollment, not a second kind of membership — adding a student enrolls them into every instance the class carries, and attaching a course (`POST /classes/{id}/instances`) mints the class×course **instance** and enrolls the whole roster into it, the rows written being ordinary enrollments tagged with the şube that pumped them (a row with no such tag was placed by hand, and no sweep takes it back). A student already enrolled by hand keeps their own row. Removing a member is a **soft** leave: the stint is stamped `left_at` and the live-member counter comes down, but the row stays as the section's history — re-adding the same student is a fresh stint, so a pair may hold two rows, one live. `DELETE /classes/{id}/instances/{instance}` takes the whole instance with it — exams, homework, sessions, the roster it pumped, its teacher links — and unlinks the uploaded files those rows named. A homeroom teacher (`teacher_id`, sınıf öğretmeni) may act on every instance of their section; the class itself is the office's (manager+). Refusals carry a machine `code` beside the prose where two routes share one vocabulary: a member add is refused by a full roster (`class_at_roster_ceiling`) or by an instance list longer than one add may enroll at once (`class_course_list_too_large`), a course attach by the mirror of both (`class_at_course_ceiling`, `class_roster_too_large`); `duplicate` is a refusal on either manual route; `linked_course_missing` (another course on that section no longer exists — detach it first) only the member add can meet. Grade blueprints (`/classes/blueprints`) are the template side: a manager+ writes the courses every section of a grade takes, and the pump that stocks a section from it is idempotent and best-effort — the courses that did not fit come back in `skipped` with those codes, and `blueprint_deleted`/`class_deleted`/`course_deleted` report a row that vanished mid-pump."),
        (name = "sessions", description = "Lesson sessions and their roll call (session teacher or course manager marks enrolled students; manager+ marks the teacher). Roll call is also what feeds two badge counters (see `users`): the first mark taken for a lesson **at or after its own `starts_at`** credits its teacher's `lessons_held_total` once — a sheet opened early is never refused, it simply holds nothing until the bell — and a student marked `present` or `late` gains a `lessons_attended_total` a later correction gives back"),
        (name = "work", description = "Staff work log: check-in/check-out stamped by the server clock, manager corrections"),
        (name = "pomodoro", description = "Student pomodoro focus log: start/finish stamped by the server clock (starting discards any dangling unfinished session, so a crashed timer never blocks the next one), own history with the unpaged `total_focus_ms` sum, and teacher+ (or linked-parent) reads of any student's log. Breaks and the work/break rhythm stay in the frontend — the backend records only focus stints. A finished stint answers `counted`: whether it moved the lifetime counters the badges and the study streak read, which it does when it ran at least `pomodoro.min_counted_ms` and is within that UTC day's `pomodoro.max_counted_per_day` (both on `GET /limits`, both compiled in). A stint that counts for nothing is still recorded, still listed and still sums into `total_focus_ms` — the rule bounds what counts, never what is kept, and it exists because finishing is self-service: two requests, no second person and no elapsed time, so a counter moved once per round-trip is farmable and a badge is never revoked"),
        (name = "questions", description = "The school question pool. Students ask (`POST /questions`, optionally attaching one photo of the problem via `POST /questions/{id}/image` — raster types only, ≤ the school's `max_file_bytes`, only while pending); teacher+ approve (`POST /questions/{id}/approve`) into the school-wide pool, or reject by deleting. Everyone student-and-up sees every approved question and may offer solutions under it (oldest first); parents stay out. Pending questions are visible only to their asker and to teacher+ (`GET /questions?status=pending` = the approval queue), and approval freezes title, body, and image — moderated content cannot be edited afterwards, only deleted (asker or teacher+; solutions and every image blob go with it). Solutions are the unmoderated half, so they never freeze: the author may edit the body (`PATCH /questions/{id}/solutions/{sid}` — author only, teacher+ included out; moderation stays delete-only) and attach/replace/remove one photo of the worked steps anytime (`/questions/{id}/solutions/{sid}/image`, same raster rules and size cap as the question's). Question rows carry a `solution_count`; solutions are deletable by their author or teacher+ (photo blob included)"),
        (name = "bank", description = "The question bank: reusable question templates, **private by default**. A template is visible to its owner (and admins) alone until its owner explicitly publishes it by `PATCH`ing `visibility` to `school`, at which point every teacher+ sees it — saving a question to the bank must never broadcast its `correct` answer key as a side effect. A template the caller may not see is a `404` on every route, never a `403` (a 403 would confirm it exists). A teacher+ reads what they can see (`GET /bank-questions`: `?subject=` origin filter, `?owner=` (a user id or `me`), `?q=` text search, `?visibility=private|school` — the last narrows the caller's own view and never widens it, so `private` means \"my drafts\", never another teacher's) and instantiates any visible template into their own exam (`POST /exams/{id}/questions/from-bank/{bid}`, which copies the template — text, points, `choice`/`text` spec, illustration, and option pictures — into a fresh exam question under the target subject, source untouched); a teacher also saves an existing exam question back into the bank (`POST /exams/{id}/questions/{qid}/to-bank`). A template's `subject` is origin metadata only — the bank spans courses, so the same-course rule is checked at instantiate time against the target exam's course, never on the bank row. Editing, deleting, or publishing a template needs ownership (its owner, or an admin — 403 otherwise), and every one of those rights is re-checked against the caller's live role: `owner` is a historical column no demotion sweeps, so an account below `teacher` reads and edits nothing here, its own templates included; as a consequence of that admin bypass, an admin may publish or unpublish another teacher's private template, which is accepted by design. Templates never freeze, since they carry no exam tie. Images mirror exam question images (one illustration, one picture per option on `choice` templates; raster types only, ≤ the school's `max_file_bytes`), owner-gated to write and readable by whoever may see the template"),
        (name = "attendance", description = "Attendance summary reports: event tallies, lesson roll-call tallies per instance with rates, and the **devamsızlık** block a Turkish school reads — per dönem, how many distinct school-days the student was away, split `absent` (unexcused) and `excused`, against the school's configured day limits (`max_excused_absent_days` / `max_unexcused_absent_days` on `/settings`) with an `over_limit` flag. Days are counted in the school's timezone (`settings.timezone`) and deduplicated: two missed lessons in one day are one absent day, and the day is the *lesson's* (`course_session.starts_at`), never when the teacher marked it. Own report at `/me`; another user's needs teacher+ (teachers narrowed to the instances they run — blocks, tallies and absence days alike) or a parent link to that student"),
        (name = "exams", description = "Exams (per course, weighted by their kind — see `settings`): results, sync/async/open scheduling, attempts (students only — staff never sit) with retakes (`max_attempts`, 0 = unlimited) and a live rejoin door (`allow_rejoin`), questions/answers, and live monitoring (no-shows flagged `absent` once the window closes). An exam is owned by the instance it was created on, and it can be announced to **sibling instances** — the ortak sınav: `POST /exams/{id}/audience` adds an instance that teaches the same catalog course in the same academic year, `GET /exams/{id}/audience` lists the set (owner first), and `DELETE /exams/{id}/audience/{instance}` withdraws one — the owner's own pair never, that is the exam's own instance and deleting the exam is what ends it. An announcement is what *attributes* and what *admits*: every addressed instance's students read the exam (`GET /exams/{id}`), sit it (attempts and answers), and are graded on it exactly like the owner's — the sitting/answering/grading enrollment gates ask \"enrolled in any addressed instance\", and the live monitor's roster spans the audience — while every addressed instance's exam list, marks report and karne carry the exam and its marks (one sitting, one mark, standing in each addressed instance's report). The announce/withdraw gate is the owner instance's (`manager`+, its assigned teachers, its şube's homeroom teacher) — the section that runs the exam decides who else sits it — while the **teacher side of the exam itself** (grading, the results and statistics reads, the live monitor) opens to a teacher of *any* addressed instance, so the section that sits it also runs it there; exam authoring (PATCH/DELETE, questions, images) stays the owner's. Questions may carry images: one illustration per question (any kind — the map the prompt asks about) and one picture per option on `choice` questions, uploaded per slot (multipart, raster types only, ≤ the school's `max_file_bytes`) and frozen with the rest of the question once attempts exist; bytes are served to course managers and to enrolled students once they hold an attempt. Students mirror this on the answer side — one drawing per question inside their attempt (`POST /exams/{id}/attempt/answers/{qid}/image`, same raster types and size cap), saved like any answer through the writable-attempt gate and read back by the student (own) or a course manager (grading). Each sitting keeps its own answers, drawings, and mark keyed by the attempt `seq` — a retake starts from a blank sheet without erasing the prior one; the grading views show the latest sitting, a course manager reads any prior one via `GET /exams/{id}/students/{user}/attempts[/{seq}/answers]` (and the mark history at `.../marks`), and the latest seq is the grade-of-record. When the teacher turns on `allow_review`, a marked student reads back the answer key (`GET /exams/{id}/review/questions`, `correct` included) and their own sittings through `GET /exams/{id}/review/attempts[/{seq}/answers[/{qid}/image]]` (own-scoped — never another student's, and only once an `ExamResult` proves they were marked; a caller with no mark reads `404` whether review is on or off, so the status code never discloses `allow_review` to someone `GET /exams/{id}` would refuse outright). Those reads are refused (`409`) while the caller can still *write* a sitting at that exam — one in progress, or one still startable under `max_attempts` before the window closes — since a mark on an earlier sitting must never hand out the key to a retake, and a question sharing a question-bank template with a question in *another* exam the caller has a sitting open on comes back with `correct: null` — and with no correctness flag and no `auto_score` contribution on the answer sheet — since the bank copies `correct` into every instantiation. A question links to a template whichever way it got there, and both directions are matched: the template it was instantiated from (`from_bank`) and the one minted by saving it into the bank (`banked_as`), so a hand-written question that was saved to the bank and then instantiated elsewhere is hidden too. That redaction is per question, keyed on the template: the rest of the reviewed exam reads normally, a question with no bank link in either direction is never hidden, and the key comes back once the other sitting is submitted or expires. Known limit, accepted: deleting a template clears both columns on every question that pointed at it, so a pair whose template was since deleted keeps the identical `correct` with nothing left to join them by. A modeless exam is offline-graded — attempts on it are a 409. An exam still being written can be saved as a **draft** (`draft: true` on create, published later by `PATCH`ing `draft: false`): drafts are visible only to the course's managers (students get 404s), cannot be sat or graded, and once attempts or results exist an exam cannot be re-drafted. Not in this spec (WebSocket): the student exam room at `GET /exams/{id}/attempt/ws` — JSON frames; entering clears the attempt's `left_at`, leaving mid-attempt stamps it. See the README's \"Taking an exam\" section for the protocol. `GET /exams` takes an optional `?starts_after=&ends_after=` schedule window (unix-millis), applied after visibility and draft filtering, that keeps only upcoming/unfinished exams and lists them soonest-first (window-less exams — no `mode`, or `open` — drop out); without it the list stays newest-first. An exam on an archived term is frozen with the rest of that year: the exam-room WebSocket upgrade at `GET /exams/{id}/attempt/ws` is refused with `409` and `code: \"term_archived\"`, and so is a `finish` frame on a socket already open"),
        (name = "meals", description = "The school's food program. A manager+ publishes one menu per calendar day and meal slot (`POST /meals/menus` — `date` is text, `YYYY-MM-DD`, and must be a **real calendar day**, leap years included: a menu on a day that does not exist (`2026-02-29`) has no serving instant, so it carried no booking or cancel cutoff at all while being fully actionable otherwise, and a menu stored on such a day before that rule now refuses both (`409`) whenever a cutoff is configured — the deadline fails closed rather than never closing. `slot` must be one of the school's `meal_slots` from `GET /settings`, and may not contain `/ \\ ? # %` — the slot name is copied into the menu's record id, which is a URL path segment, so such a menu could never be addressed again; the same characters are refused at `PATCH /settings`), optionally capped by a `capacity`. The date is parsed by the same calendar library that computes the serving instant and compared back to the text it came from, so a signed component (`2026-+1-01`, which `\"+1\".parse()` once accepted) is a `400` rather than a **second** menu id for the 1st of January — one with its own capacity and seat counter, invisible to every `?from=&to=` read because `+` sorts below `0`. The day must also not already be over (`400`, measured against the end of the day, so today publishes all day long): a published menu is a bookable one and a booking is what charges, so a menu behind the calendar mints real debt for food nobody can be served. The day+slot pair is unique, so a second publish for the same meal is a `409` — edit the first one instead. The slot is snapshotted as text, never a link, so retiring a slot in settings leaves every published menu intact (and a slot a menu already used cannot be retired at all, `409`). Only `capacity` is patchable: `date` and `slot` are immutable, since a menu on another day is another menu. Dishes hang off a menu (`POST /meals/menus/{id}/dishes`, at most 50, edited and removed at `/meals/dishes/{did}`, both `404` once the dish's menu is gone — every dish write moves its menu's revision in the same transaction, which makes the menu's existence part of the write), each with an optional description, dietary tags drawn from the school's `dietary_tags`, and a `price_minor` in **minor units** (kuruş) as an integer — this API never speaks decimals or floats about money. Deleting a menu takes its dishes **and its attendance marks** with it, in the same transaction as the row: a menu's id is deterministic on day+slot, so anything left behind would resurface attached to the next menu published for that meal. Cancelled bookings deliberately stay — their `attempt` counter is what keeps the ledger's `(booking, attempt)` ids unique. A student carries a **dietary profile** of tags from that same `dietary_tags` list plus a free-text note for the kitchen (`GET /meals/profiles/me`, `GET /meals/profiles/{user}` — own always, otherwise teacher+ or a parent link — and `PATCH /meals/profiles/{user}`, **manager+**: an allergen list is a safety record the school keeps, not a self-service preference, so a student editing their own is a `403`). Because both sides are tagged from one vocabulary, every dish a menu read returns carries a `conflicts` list — the intersection of the dish's tags with the *calling user's* profile tags, empty when they do not overlap and empty for every reader without a profile (a manager sees `conflicts: []`, which is correct, not a bug). Reads are open to every authenticated user (a student needs to see what is being served) except the two money reads below; every write is manager+. Students take seats on a menu (`POST /meals/menus/{id}/bookings`): a student books for themselves, a parent for a student they hold a link to (the one write a `parent_link` authorises) — staff never book for a child (`403`). One row per (menu, student), so booking twice is the same seat, and at most `meal.max_booking_attempts` (`GET /limits`) seats may be taken on it in all — the first booking plus the re-bookings after a cancel, past which it is a `409`, because every cycle appends a charge and its reversal permanently and nothing else bounded that growth; a menu whose **day has already passed** takes no booking at all (`409`), whatever the cutoff is set to and with no role able to bypass it — the booking is the charge, so a seat taken on a day already served bills a meal that cannot be eaten; *cancelling* a seat on such a menu stays open, since money already taken has to stay reversible. A full menu is a `409`, and that cap is a single conditional write on a counter kept on the menu row — taken and spent in the same transaction that places the booking, since counting rows and then writing one is write-skew. Cancelling (`DELETE /meals/bookings/{bid}`, answering `200` with the flipped row — its student, their parent, or **any manager+**, so a seat and its charge stay reachable once the student it was booked for is no longer a student) is a **status transition, not a delete**: the row stays as `cancelled` with a `cancelled_at` stamp so the freed seat remains auditable, only `booked` rows count against the capacity, and re-booking flips the same row back. The call is **idempotent** — cancelling an already-cancelled seat is a `200` that replays the refund, not a `409`, so a cancel cut short between the flip and the reversal is recovered by simply repeating it. **Who is asking is decided before the seat is looked up**: an unauthorised caller gets the same `403` for a booking that exists and one that never did, and only a caller who may cancel it ever sees a `404` — booking ids are derivable (`{date}_{slot}_{student}`), so reading the row first made the status code hand out the manager-only per-menu list one student at a time. The flip releases **only the attempt the call read** (the same `attempt` fence the re-booking side carries), so a seat taken again while the cancel was in flight is a `409` rather than a silent cancellation of that brand-new seat — unfenced it freed a seat the API had just answered `201` for, refunded nothing (the money is keyed to the older attempt, already reversed) and burnt the new attempt's reversal id for good. The school's `meal_cancel_cutoff_minutes` closes both ends at once — inside that window neither booking nor cancelling lands (`409`) — but it binds **students and parents only**: a manager+ is not held to it, since a deadline meant to stop students gaming the kitchen's headcount is no reason to leave staff holding a seat they cannot free, on a menu that then refuses its own deletion forever and a charge that can never be reversed. It is measured back from the instant the meal is served: the menu's `date` plus the slot's `serving_minute` from `GET /settings` (minutes past midnight **UTC** — this API stores no school timezone, so staff enter UTC). A slot with **no `serving_minute` closes nothing at all** — there is no instant to count the deadline back from, and counting from midnight UTC (as it once did) put every same-day menu past its deadline the moment a school set the cutoff knob, since all three shipped slots ship without an hour: today's lunch could not be booked and the seats already held could not be cancelled by the families holding them. Setting the hour is what starts the deadline binding that slot, on menus already published too. The slot list is read live at booking and cancelling time, so correcting a serving time moves the deadline of menus already published for it. A menu with a live booking refuses deletion (`409`). `GET /meals/bookings/me` returns the seats held for the caller plus, for a parent, the seats held for every student they *currently* hold a link to — the links are re-read on every call rather than trusted from the booking's `booked_by` stamp, so an unlinked parent stops seeing the child's seats at once, including ones they booked themselves; `GET /meals/menus/{id}/bookings` is the kitchen's per-menu list (manager+), cancelled rows included. The money is an **append-only ledger** — no route edits or deletes a line, and no balance is stored anywhere: it is always the fold `credits + reversals - charges` in minor units, so a negative balance means the student owes the school. Booking is what charges, at a **price snapshot** (the menu's dishes summed the moment the seat is taken and frozen onto the booking row; the claim that takes the seat asserts the menu is still at the revision the price was read at, and every dish write bumps that revision, so a dish landing between the sum and the seat refuses the booking, which re-reads and prices itself again), so a later price edit never moves an existing charge and re-booking after a cancel bills the then-current price; a menu whose dishes sum past the chargeable maximum refuses the booking outright (`400`) rather than seating anyone unbilled. Booking the same seat twice bills once — the charge and its reversal are keyed by (booking, attempt), and the seat and its flip move in one transaction, so eight simultaneous `POST`s hold one seat and write one charge line, a retried cancel refunds once, and a half-written attempt heals when the request is repeated. A seat taken while the menu was free records *that it was free*, so pricing the menu afterwards never bills it retroactively. Cancelling appends a `reversal` for that exact amount pointing at the charge, written by the **same transaction that flips the seat** rather than after it — as two writes a crash in between gave the seat back and left the student billed, with no later cancel able to repay it, since the money is keyed to the attempt the flip had already consumed; the reversal lands only when the charge it undoes is really there, checked inside that transaction, because a refund with no charge behind it invents money. The charge row itself stays. Who actually ate is recorded separately by teacher+ (`POST /meals/menus/{id}/attendance` with `{student_id, status}` — `served` or `missed`, the canteen's own fixed pair rather than the school's editable roll-call statuses; one row per (menu, person), so re-marking flips that row; the target need only exist, since a canteen also feeds staff and a mark records what happened rather than who was entitled; a `404` when the menu is unpublished mid-request, because the menu row is the write's own target and a mark left on a deleted menu would resurface on the next one published for that day and slot), read back per menu (`GET /meals/menus/{id}/attendance`) or per student (`GET /meals/attendance/{user}`, `?from=&to=` inclusive `YYYY-MM-DD` bounds on the menu's day, gated like the other per-student reports: teacher+ or a parent link). Meal attendance has **zero billing effect**: a no-show still pays, since the seat was reserved and the food was cooked — no penalty, no refund-on-missed, no auto-reversal; and a walk-in marked `served` without a booking is recorded but never charged, since charging is booking's job alone. `POST /meals/credits` records money received (`{student_id, amount_minor, method?, note?, request_key?}`, whose target must be a student or anyone already carrying meal-ledger lines — a debt outlives its debtor's role change and has to stay settleable, while a mistyped staff id carries no lines and is still a `400`) and is **admin-only**, not manager — the backend speaks to no payment gateway and stores no card data; an over-credit is corrected with a compensating line, never a fix-up. It takes the same optional **`request_key`** (`[A-Za-z0-9-]`, 1–64 characters, no `_`) that `/payments/credits` does: the line is keyed `<student>_k_<key>`, so a client retry after a network timeout returns the line the first attempt wrote rather than crediting the money a second time — which nothing on this API could then edit or delete. The same key with a different `amount_minor` is a `409`; omit it and two identical posts are two credits. `GET /meals/balance/me`, `/meals/balance/{user}`, and `/meals/ledger/{user}` (paged) read it: your own always, another student's only as **manager+** or as a parent linked to them — **a teacher gets a `403`**, the same rule every `/payments` route holds, because canteen debt is family debt and what a family owes is not classroom information"),
        (name = "payments", description = "School fees: the plans, who is on them, and the money. A manager+ writes a **fee plan** (`POST /payments/plans` — a name and 1 to 60 installments, each `{amount_minor, due_at}` in **minor units** (kuruş) as an integer and unix milliseconds; `due_at` **may be in the past**, since a school adopting the app mid-year assigns plans whose first installments were already due). Writing a plan bills nobody: **assigning it does** (`POST /payments/plans/{id}/assignments` with `{student_ids}`, at most 200 per call), and that appends *every* installment as a `charge` line at once, each carrying its own due date. One request is bounded by the charges it would raise rather than by the head count alone — `student_ids × installments` may not exceed 3 000 (200 students up to a 15-installment plan, 50 at a time on a 60-installment one) — and a bigger batch is a `400` naming the split, refused whole before anything is written, since a batch billed in part would leave the office guessing which families were charged. There is no scheduler and no sweep. Assignment is **replay-safe by identity** — an assignment is keyed (plan, student) and every charge it raises (plan, student, installment) — so re-assigning bills nothing a second time (reported per student as `already_assigned`), and an assignment cut short after three of twelve charges landed completes itself when the call is repeated. Only students carry a fee record, so any other target comes back `rejected` without losing the rest of the batch. A plan that has been assigned to anyone can no longer be edited or deleted (`409`): its charges are frozen copies of the installments as they stood, so an edit would only make the plan and the money disagree — write a new plan instead. That freeze is a **counter on the plan row**, incremented in the same transaction as the assignment it counts, and the edit and the delete are conditional writes against it — so a manager editing a plan at the very instant it is being assigned is decided one way or the other by the database, never left half-applied, and the installments a first assignment bills are read back from the stored plan at the moment it freezes. The money is an **append-only ledger**: no route edits or deletes a line, every stored field is `READONLY`, and a mistake is corrected by appending the opposing line so that both the mistake and the correction stay visible. **No balance is stored anywhere** — it is always the fold `credits + reversals - charges - refunds` in minor units, so a negative balance means the family owes the school, and every amount is stored positive with the sign living in the `kind`. `POST /payments/credits` records money received against **one named charge** (`{charge_id, amount_minor, method?, note?}`) — allocation is recorded, never inferred from a balance, and partial payments are the norm; `POST /payments/refunds` hands money back against **one named payment** (`{credit_id, …}`), which is also the only way a mistaken credit is corrected, since a credit is never reversed; both take an optional **`request_key`** (`[A-Za-z0-9-]`, 1–64 characters — no `_`, which is the separator inside a ledger line's id) that makes the call **retry-safe by identity**: the line is keyed `<target>_k_<key>` (`_kr_` for a refund), so a client retry after a network timeout returns the line the first attempt wrote instead of recording the money twice — even when that payment filled the charge exactly, since the replay is answered before the over-payment cap is consulted. The same key sent with a different `amount_minor` or target is a `409`, never the stored line: that is a client bug, and answering `201` would hide it. Omit the key and two identical posts are two payments, as a desk taking the same amount twice really is. `POST /payments/reversals` (`{line_id, note?}`) undoes a `charge` or a `refund` for its exact amount and nothing else (`400` otherwise), is keyed `<line>_r` and so reverses once however often it is retried. A reversed charge is no longer owed, so it takes no payment: `POST /payments/credits` against one is a `409` that says it was **reversed**, not that it was paid in full — a reversal fills the same room a payment does, and a bursar told \"paid in full\" about money that never arrived would go looking for it. Payments and refunds are capped by the line they target (`409` past it) — that cap is a cross-record sum, which the database cannot enforce on its own — it holds because the backend runs as one process and the sum, and the append it authorizes, are taken under one lock. That sum costs one query per line of the target's subtree, all of them under that lock, so a line carries **at most 20 applied lines** (the payments under a charge, the refunds under a payment, and the reversals among them): past that a further payment or refund against it is a `409`, an installment settled in more than twenty pieces being pathological. The ceiling binds new writes only — a line that already carries more still reads, still refunds through its own children, and is still reversible. Should an over-payment ever be recorded anyway, it is visible in the statement and undone by a refund, and both lines stay true records of money that really arrived. The backend speaks to no payment gateway and stores no card data; `method` is free text (\"cash\", \"havale\", …). Every write here is **manager+** — teachers never touch fees. Reads are narrower than the other per-student reports: `GET /payments/ledger/{user}` (paged raw lines), `/payments/statement/me`, `/payments/statement/{user}` (the per-charge rows are paged too, in an `entries: {items, total, limit, offset}` envelope — the window is taken **after** the fold, so `balance_minor` and every `overdue` reading are the same on every page), `/payments/balance/me` and `/payments/balance/{user}` are readable by the student themselves, by a parent holding a live link to them, and by manager+ — **a teacher gets a `403`**, because what a family owes the school is not classroom information. The statement is a per-charge rollup — plan, installment amount and due date, what it collected, what went back out, what is still outstanding, whether the charge was reversed, and whether it is `overdue` (still owed and its `due_at` has passed) — all folded from the raw lines on every request and stored nowhere"),
        (name = "marks", description = "Weighted mark reports per class×course instance and overall, labeled by the school's grade bands when configured. Each mark counts its exam kind's settings-configured weight times (weight 1 when the kind was since removed from settings). Own report at `/me`; another user's needs teacher+ (teachers narrowed to the instances they run) or a parent link to that student. `GET /marks/karne?term=` is the dönem's karne — every karne-counting instance of the student's şubeler with its average and band, the `ders_saati`-weighted dönem average, and the pass/fail verdict read off the bands (never hardcoded); `GET /marks/karne/{user}` is the same for another student, narrowed for an exactly-teacher caller to their own instances (the dönem average recomputed over those, no verdict — a partially-seen karne states none). An archived dönem serves the snapshot the school froze when it closed; an open one computes live"),
        (name = "settings", description = "School policy, one singleton: exam kinds with their course-average weights, attendance statuses, grade-display bands, and the per-file upload size limit (`max_file_bytes` — note files and question images alike). An exam kind whose exams already carry marks cannot be removed from the list (409) — those marks would silently re-weight. Also the food program: the meal slots menus are published for — each with an optional `serving_minute`, the minutes past midnight **UTC** at which it is served (there is deliberately no school-timezone setting, so a UTC+3 school enters `540` for a noon meal; `null` = unset, and a slot with no serving minute carries **no cutoff at all** rather than one counted from midnight) — the dietary tags a dish or a student's profile may carry, and `meal_cancel_cutoff_minutes` — one knob closing both booking and cancelling ahead of a slot's serving time (`null` = no cutoff). A slot a menu was already published for cannot be removed (409), same reason. A slot name may not contain `/ \\ ? # %` (400) — the name is copied verbatim into a menu's record id, which is a URL path segment — but a name the school's stored list already carries is exempt from that rule, since re-validating it would leave a list written before the rule existed unable to be edited at all: re-sending the name is a 400 and dropping it is a 409 once a menu used it. A grandfathered name still cannot carry a new menu, and dropping it is still the only way to remove it. Read: any authenticated user; edit: manager+"),
        (name = "subjects", description = "A **catalog** course's curriculum topics (Matematik's \"Üslü sayılar\", not one şube's lesson plan). Created and listed under `/courses/{id}/subjects`; lookup/edit/delete at `/subjects/{id}`. Every exam question of every instance teaching that course links to one of them, so a subject in use cannot be deleted (409) — re-tag or delete the questions first. View follows the course (anyone it reaches, its creator, manager+); edit follows catalog rights (its creator or a manager/admin)."),
        (name = "homework", description = "Instance homework: a teacher assigns it per class×course instance (`POST /instances/{id}/homework`), tagged with one of the course's subjects (re-taggable; a subject with homework refuses deletion until it is re-tagged or removed) and given a future `due_at`, to the whole enrolled course or an optional `assigned` subset (empty means the whole course — whoever is enrolled when they submit; a subset cannot later be narrowed so as to strand work that already exists). Students submit optional text plus files — any content type, up to 10 per submission, each capped by the school's `max_file_bytes`, always served back as forced attachment downloads (never inline); a submission touched after `due_at` is flagged late (computed from the moving last-touch stamp, never stored — the first-submit stamp is immutable) and stays editable until it is graded. A teacher grades a status (`done`/`incomplete`/`missing`) with an optional 0–100 mark — grading freezes the submission until the grade is removed (un-grading reopens it), never targets the grader themselves, and covers work never handed in (that is how `missing` is set; the student reads the verdict at `GET /homework/{id}/result`). Beyond a teacher-set `missing` the roster and reports also compute a `missing` for anyone unsubmitted past due, and the roster keeps a straggler's stale work visible (flagged `unenrolled`) after an unenrollment or promotion. Homework marks stay out of the weighted `/marks` averages. Students see and fetch only the homework they are assigned (others 404, so a subset assignment never leaks), and the ones it does name get its `assigned` list narrowed to their own id — who else was assigned is roster information, and the roster is teacher+-only; a linked parent reads a student's homework report — statuses, late flags, marks — but never the submitted files"),
        (name = "boards", description = "Collaborative whiteboards. Anyone **from `student` upwards** opens a board (`POST /boards`) with a title and an ad-hoc invite list of user ids — no course, session or appointment is involved — and every participant draws on it; the creator alone clears, locks, closes or deletes it. `GET /boards` takes `?open=true|false` to narrow the list by the closing stamp (`total` counts the filtered list) — a closed board is never deleted, so this is how a creator holding many boards trims the retired ones out of sight. It filters `closed_at` and nothing else: a **locked** board, and one that has spent its lifetime stroke budget but was never drawn on again, are both still open, because the stamp is written only by `/close` or by the append the lifetime cap refuses. The `parent` role has no whiteboard access whatsoever: it is refused `POST /boards` and `GET /boards` with a `403`, and cannot be named in an invite list at all (`400`) — so a parent is never on a roster, which makes every id-scoped route and the room's door answer the outsider's `404` rather than a `403` that would confirm the board exists. Every frame the room delivers is authorized against the database first, so a caller who loses the right to be there — taken off the roster, or demoted below `student` — is served no further content: they get one `error{code:\"forbidden\"}` naming which of the two happened, and the room closes. That is separate from a refused *command*: `clear` and `lock` by a non-creator answer `error{code:\"forbidden\"}` and deliberately keep the socket open, since a participant who clicked the wrong button has made a mistake, not left the room. A demotion below `student` also sweeps that account off every roster it was on, and the rosters an older binary left behind were repaired at boot: a board written before that sweep can name a parent, or a user since deleted, and every such id is stripped the first time this binary starts. A board whose *creator* was demoted is **closed** by that same write: barred to them, still fully readable to everyone else, and refusing every write with the terminal `board_closed` — because clearing, locking, closing and deleting are the creator's alone and nothing lists a board the caller is not on, so a room whose creator was demoted could be ended by nobody at all while its participants kept drawing on it. Nothing is deleted (the marks are the participants' work too) and the seat it holds on `max_boards_per_creator` stays taken, exactly as a manually closed board's does. A caller who is not on a board gets a `404` for it on every route, existence included, while a participant who is not the creator gets a `403` on those four commands (they are already rendering the board, so hiding it from them would be a lie their client cannot act on). Strokes are **append-only and a clear deletes nothing**: it bumps the board's `epoch`, so the live canvas empties while every mark ever drawn stays stored — `GET /boards/{id}/strokes` is the current epoch — drawn marks only, never a `clear` marker, exactly as the room's socket replays it, so the REST catch-up and the live canvas can never disagree, `GET /boards/{id}/history?epoch=` is the whole session or one named epoch, and `GET /boards/{id}/epochs` is the index of `clear` markers (a clear needs at least one mark to close: an **already-blank** canvas answers `409` alongside the closed board and the **locked** one — the pause the creator set holds against their own clear too, so a locked board sitting at `max_epoch_strokes` is recovered by unlock, clear, relock — since the marker is a stored row charged to the lifetime cap), each carrying the epoch it closed, that epoch's final stroke count, who cleared and when (the markers *are* the index, so the open epoch is deliberately absent from it). Two growth caps, both published at `GET /limits`: `max_epoch_strokes` bounds the *live* canvas and is recoverable (clear it and drawing resumes, losing nothing), while `max_board_strokes` bounds the board's *lifetime* storage — rows on the board, so a `clear` marker costs one unit like any mark, and a board holds at most `max_board_strokes + 1` rows because the marker closing the final stroke is never the one refused — and stamps `closed_at` on the way past — a closed board is permanently read-only and fully readable, never deleted, and there is no reopen. `POST /boards/{id}/close` is the manual form of that, and idempotent. Closing and locking are independent, and a board holding both flags answers **closed** everywhere — a stroke, a socket `clear` and `POST /boards/{id}/clear` all report the terminal state, never the pause, because the pause can never lift. `DELETE /boards/{id}` is the one operation here that really destroys marks — it takes the whole stroke log with it and frees one of the creator's `max_boards_per_creator` seats. `PATCH /boards/{id}` re-titles (any participant) or changes the roster and the lock (creator only). `POST /boards/{id}/invite` fills that roster from a group that already exists rather than one id at a time — `{kind:\"class\", class}` for a class section (şube), `{kind:\"course\", course}` for a course's enrolled students (clubs and study groups *are* courses, so this is how a whole club is invited), `{kind:\"event\", event}` for an event's expected-attendee roster exactly as `GET /events/{id}/roster` resolves it. It is the creator's alone and strictly **additive** — nothing is ever removed by it, removal stays `PATCH` — and it resolves **once**, at the call: a board holds a flat list of ids and no memory of where they came from, so a student who joins that class tomorrow is not on today's board, and re-inviting the same source is how a roster is topped up (idempotent when nothing changed). Ineligible members are dropped silently rather than failing the call for everyone else — ids that no longer resolve to a user, and anyone below `student`, which is the same cut that keeps `parent` off a whiteboard — but the participant cap is **all-or-nothing**: an invite that would carry the board past `max_participants` is refused with a `409` naming both numbers, and nobody is added. Since a board's roster is visible to every participant, inviting a group *discloses* that group's membership, so each source carries the gate its own listing route already carries: teacher+ for a class, the course's creator/assigned teacher/manager+ for a course, teacher+ for an event. A student may still build a board one id at a time; they cannot pour a class roster into one. Not in this spec (WebSocket): the live board room at `GET /boards/{id}/ws`, where the drawing itself happens — JSON frames, cookie-authed, and a **404** at the door for anyone not on the board. A `join` replays the current epoch only (chunked `strokes`, then `synced` with the cursor to resume from); a `stroke` is persisted before it is fanned out, so every canvas in the room is showing rows that committed; `clear` and `lock` are the creator's alone and refuse anyone else with `error{code:\"forbidden\"}` *without* closing their socket. The REST mutations above fan out into the same room, and a roster change drops the socket of anyone it removed. See `src/web/board_ws.rs` for the frame-by-frame protocol"),
        (name = "academic-years", description = "The calendar above the dönemler. A year (`POST /academic-years`, manager+) holds the şubeler and the sınıf-geçme policy (`grade_promotions`) that `POST /academic-years/{id}/rollover` applies: each şube of the source year whose grade the target promotes is planted afresh there — same name, mapped grade, copies of its instances (same courses, `ders_saati`, karne policy), their assigned teachers and every live member, who are also enrolled into the new instances. A grade the policy does not name is not carried over: that is how graduation is expressed, and `graduated` reports it. The target year must still be empty (a second rollover into it is `409`, which is what makes the command idempotent), open, and different from the source. `POST /academic-years/{id}/archive` (manager+) closes a finished year — it stamps `archived_at`, and a second archive keeps the stamp that already stood — after which the year is read-only: no new şube, no new dönem, no new exam inside it, and no edit or delete of the year itself. A year still linked by any şube or dönem cannot be deleted (`409`). `GET` is teacher+, every write manager+."),
        (name = "terms", description = "Academic terms (dönem: semester/trimester/quarter — whatever the school runs), each a grading slice **inside an academic year** (`year`, required; `GET /academic-years`). Exams name one — that is the dönem whose karne their marks count into — and `?term=` on `/marks/karne` picks one. A dönem still holding an exam or a frozen karne cannot be deleted (`409`). `POST /terms/{id}/archive` (manager+) **freezes the karnes**: every student with a roster row under the dönem's year gets a snapshot of their report, and from then on the karne route serves that record instead of recomputing, so a mark corrected later never rewrites what a family holds; `POST /terms/{id}/unarchive` re-opens it. Both are idempotent — a second archive answers 200 with the original stamp and never re-freezes. An archived dönem is read-only (`PATCH`/`DELETE` refused, no new exam); creating an exam inside it stays allowed while its *year* is open, because the archive is a record and not a wall. An archived **year** is what refuses new structure: şubeler, dönemler, and every write to either."),
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

/// `/courses` and the foreign-module route pair mounted inside it. The child
/// carries its own module's gate *and*, through the outer one applied here, the
/// course gate — so either module being off refuses it.
///
/// The other three pairs that used to live here (exams, sessions, homework)
/// moved to `/instances/{id}/...`, because they hang off the class×course
/// instance rather than off the catalog row; see [`instances_router`].
fn courses_router(state: &AppState) -> OpenApiRouter<AppState> {
    let children = gate(web::courses::subject_routes(), state, Module::Subjects);
    gate(
        web::courses::routes().merge(children),
        state,
        Module::Courses,
    )
}

/// `/instances` and the three foreign-module route pairs mounted inside it —
/// the same split as `/courses`, one anchor down: the instance is what a şube
/// teaches, and what exams, sessions and homework hang off. Each child carries
/// its own module's gate *and*, through the outer one applied here, the course
/// gate (an instance is a course taught somewhere), so either module being off
/// refuses it.
fn instances_router(state: &AppState) -> OpenApiRouter<AppState> {
    let children = gate(web::instances::exam_routes(), state, Module::Exams)
        .merge(gate(
            web::instances::session_routes(),
            state,
            Module::Sessions,
        ))
        .merge(gate(
            web::instances::homework_routes(),
            state,
            Module::Homework,
        ));
    gate(
        web::instances::routes().merge(children),
        state,
        Module::Courses,
    )
}

/// Assemble the full application router. Shared by `main` and the test suites.
///
/// Also serves interactive docs: Swagger UI at `/swagger`, raw spec at
/// `/api-docs/openapi.json`. Root `/` mirrors the health probe.
pub fn build_router(state: AppState) -> Router {
    let (router, api) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .route("/", get(health))
        .routes(routes!(health))
        .routes(routes!(server_time))
        .merge(web::limits::routes())
        .merge(web::modules::routes())
        .nest("/ai", web::ai::routes())
        .nest("/auth", web::auth::routes(&state))
        .merge(web::builder::routes(&state))
        .nest(
            "/chatbot",
            gate(web::chatbot::routes(), &state, Module::Chatbot),
        )
        .nest("/rag", gate(web::rag::routes(), &state, Module::Chatbot))
        .nest(
            "/insights",
            gate(web::insights::routes(), &state, Module::Chatbot),
        )
        .nest(
            "/podcast",
            gate(web::podcast::routes(), &state, Module::Chatbot),
        )
        .nest("/users", web::users::routes())
        .nest("/notes", gate(web::notes::routes(), &state, Module::Notes))
        .nest(
            "/messages",
            gate(web::messages::routes(), &state, Module::Messages),
        )
        .nest(
            "/events",
            gate(web::events::routes(), &state, Module::Events),
        )
        .nest(
            "/appointments",
            gate(web::appointments::routes(), &state, Module::Appointments),
        )
        .nest("/courses", courses_router(&state))
        .nest("/instances", instances_router(&state))
        .nest("/academic-years", web::academic_years::routes())
        .nest(
            "/course-notes",
            gate(web::course_notes::routes(), &state, Module::CourseNotes),
        )
        .nest(
            "/classes",
            gate(web::classes::routes(), &state, Module::Classes),
        )
        .nest(
            "/sessions",
            gate(web::sessions::routes(), &state, Module::Sessions),
        )
        .nest("/exams", gate(web::exams::routes(), &state, Module::Exams))
        .nest("/marks", gate(web::marks::routes(), &state, Module::Marks))
        .nest("/meals", gate(web::meals::routes(), &state, Module::Meals))
        .nest(
            "/payments",
            gate(web::payments::routes(), &state, Module::Payments),
        )
        .nest("/work", gate(web::work::routes(), &state, Module::Work))
        .nest(
            "/pomodoro",
            gate(web::pomodoro::routes(), &state, Module::Pomodoro),
        )
        .nest(
            "/questions",
            gate(web::questions::routes(), &state, Module::Questions),
        )
        .nest(
            "/bank-questions",
            gate(web::bank_questions::routes(), &state, Module::BankQuestions),
        )
        .nest(
            "/attendance",
            gate(web::attendance::routes(), &state, Module::Attendance),
        )
        .nest("/settings", web::settings::routes())
        .nest(
            "/subjects",
            gate(web::subjects::routes(), &state, Module::Subjects),
        )
        .nest(
            "/homework",
            gate(web::homework::routes(), &state, Module::Homework),
        )
        .nest("/terms", web::terms::routes())
        .nest(
            "/boards",
            gate(web::boards::routes(), &state, Module::Boards),
        )
        .split_for_parts();

    // Catch-all per-IP limit over every route (Swagger included). Kept inside
    // the CORS layer so a 429 still carries the CORS headers a browser needs
    // to surface the error to frontend code.
    let api_limiter = RateLimiter::per_minute(
        state.rate_limit.api_per_minute,
        state.rate_limit.trust_proxy,
    );
    // Every tier's budget outlives the process it was spent in (see
    // [`rate_limit::RateLimiter::share`]). The chatbot tier is shared here too
    // rather than in `main`, so a router built anywhere gets the same behaviour.
    api_limiter.share("api", state.db.clone());
    state.chatbot_limit.share("chatbot", state.db.clone());
    state.rag_limit.share("rag", state.db.clone());
    let metrics = state.metrics.clone();

    let cors_allowlist = cors_allowlist_from_env();
    if state.cookie_secure && cors_allowlist.is_empty() {
        tracing::warn!(
            "COOKIE_SECURE is on but CORS_ALLOWED_ORIGINS is unset: production should list its frontend origins explicitly; mirror mode runs uncredentialed, so browser frontends cannot send the session cookie"
        );
    }

    let service = router
        .merge(SwaggerUi::new("/swagger").url("/api-docs/openapi.json", api))
        .with_state(state.clone());

    // Hand the AI bridge the router *before* the outer layers, because a QUIC
    // request from a service is not a browser request from an IP: it has no
    // client address to bill the per-IP limiter (it would drain the shared
    // unknown-client budget), and `ETag`/CORS are browser concerns. Skipping the
    // layers means skipping the request deadline too, so the bridge applies the
    // same deadline to its own dispatch (see `ai::server`).
    if let Some(ai) = &state.ai {
        ai.arm_api(
            service.clone(),
            state.tenants.clone(),
            state.files_path.clone(),
            metrics.clone(),
        );
    }

    service
        // Conditional-GET: revalidatable `ETag` on 200 JSON GETs, `304` on a
        // matching `If-None-Match`. Innermost, so it sees the handler's own
        // response (mutations and errors pass straight through untouched).
        .layer(middleware::from_fn(web::etag::etag))
        .layer(middleware::from_fn(request_timeout))
        .layer(middleware::from_fn(move |req: Request, next: Next| {
            let limiter = api_limiter.clone();
            async move { limiter.enforce(req, next).await }
        }))
        .layer(cors_layer(cors_allowlist))
        // Request duration and in-flight count. Sits inside the trace layer so
        // it measures the same request the span describes, and outside CORS so
        // a preflight is counted like anything else.
        .layer(middleware::from_fn(move |req: Request, next: Next| {
            let metrics = metrics.clone();
            async move { measure(metrics, req, next).await }
        }))
        // Inside the trace layer, so the span it stamps `error.type = panic` on
        // is the request's own; outside everything else, so a panic anywhere in
        // a handler still answers the standard 500 body instead of dropping the
        // connection. The panic itself is logged and counted by the process
        // hook (see [`telemetry::install_panic_hook`]).
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(
            panic_response,
        ))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(request_span)
                .on_request(())
                .on_response(record_response)
                .on_failure(tower_http::trace::DefaultOnFailure::new().level(tracing::Level::WARN)),
        )
        // Copy the id onto the response, so a caller reporting a problem can
        // name the exact request without us needing their IP or their URL.
        .layer(PropagateRequestIdLayer::new(HeaderName::from_static(
            REQUEST_ID_HEADER,
        )))
        // Outermost: every request has an id from here inward, its own or ours.
        .layer(SetRequestIdLayer::new(
            HeaderName::from_static(REQUEST_ID_HEADER),
            MakeRequestIdV7,
        ))
        // Above `SetRequestIdLayer`, which keeps a caller-supplied id verbatim:
        // an unusable one is dropped here so the layer below mints a UUID.
        .layer(axum::middleware::from_fn(drop_unusable_request_id))
}

/// The request id this crate's spans carry: minted by its own v7 generator
/// ([`crate::domain::monotonic_id::next_uuid`]) instead of tower-http's
/// `MakeRequestUuid`, so the process mints one kind of uuid rather than two.
/// Same wire shape either way — `x-request-id` holds a uuid — and the same
/// job: letting a caller name the one request they are reporting without us
/// needing their address or their URL.
#[derive(Clone, Copy)]
struct MakeRequestIdV7;

impl MakeRequestId for MakeRequestIdV7 {
    fn make_request_id<B>(&mut self, _request: &Request<B>) -> Option<RequestId> {
        // A hyphenated uuid is always a valid header value, so this never
        // takes the trait's "could not mint one" arm.
        HeaderValue::from_str(&crate::domain::monotonic_id::next_uuid().to_string())
            .ok()
            .map(RequestId::new)
    }
}

/// Refuse a caller-supplied `x-request-id` that is not a short, boring token.
///
/// The id lands on `http.request.id` of every span, so anything a client can
/// put there it can export: free text (personal data) and unbounded cardinality
/// both. Accepting only `[A-Za-z0-9._-]{1,64}` keeps the useful case — a
/// gateway correlating its own request id with ours — and mints a UUID for
/// everything else.
async fn drop_unusable_request_id(mut req: Request, next: Next) -> Response {
    let usable = req.headers().get(REQUEST_ID_HEADER).is_some_and(|value| {
        value.to_str().is_ok_and(|id| {
            !id.is_empty()
                && id.len() <= MAX_REQUEST_ID_LEN
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
    });
    if !usable {
        req.headers_mut().remove(REQUEST_ID_HEADER);
    }
    next.run(req).await
}

/// The header carrying the per-request id, in and out.
const REQUEST_ID_HEADER: &str = "x-request-id";

/// The span every request gets.
///
/// Deliberately *not* on it: the URL path (it carries record ids), the client
/// address, the user agent, any header or cookie, and anything about who is
/// calling. The route template and the method are what a builder needs to see
/// which endpoint is slow; see [`crate::telemetry`] for the rule.
pub fn request_span(req: &Request<axum::body::Body>) -> tracing::Span {
    let method = req.method().as_str();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map_or("unmatched", MatchedPath::as_str);
    let request_id = req
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    tracing::info_span!(
        "http.request",
        // Consumed by the OpenTelemetry layer, not exported as attributes:
        // this is how a span gets a name computed at runtime.
        otel.name = %format_args!("{method} {route}"),
        otel.kind = "server",
        otel.status_code = tracing::field::Empty,
        http.request.method = %method,
        http.route = %route,
        http.request.id = %request_id,
        // Filled in by `web::tenant_state::resolve_tenant` once the cookie has
        // named a school; a builder request has none and leaves it empty.
        school = tracing::field::Empty,
        http.response.status_code = tracing::field::Empty,
        error.type = tracing::field::Empty,
    )
}

/// The response a panicking handler gets, byte-identical to
/// [`AppError::Internal`]'s: a caller learns nothing about our stack trace, and
/// a client parsing `{"error": ...}` is not surprised by the one 500 that used
/// to be a dropped connection. Runs inside the trace layer, so the class lands
/// on the request's span.
pub fn panic_response(_panic: Box<dyn std::any::Any + Send + 'static>) -> Response {
    tracing::Span::current().record("error.type", "panic");
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "internal server error" })),
    )
        .into_response()
}

/// Stamp the outcome onto the request span. A `5xx` is our fault, so it also
/// marks the span itself as failed; a `4xx` is the caller's and does not.
fn record_response<B>(res: &Response<B>, _latency: std::time::Duration, span: &tracing::Span) {
    let status = res.status();
    span.record("http.response.status_code", status.as_u16());
    if status.is_server_error() {
        span.record("otel.status_code", "ERROR");
        // `error.type` is NOT set here: whatever produced the 5xx —
        // [`AppError::into_response`] or [`panic_response`] — has already
        // recorded its class, and the status code would overwrite it with a
        // number that is already on the span.
    }
}

/// Count the request in and out and record its duration. The school slug is
/// the only caller-derived attribute, and it is only known after the handler's
/// extractor filled the slot (see [`SchoolSlot`]).
async fn measure(metrics: crate::telemetry::Metrics, mut req: Request, next: Next) -> Response {
    let method = req.method().as_str().to_owned();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map_or("unmatched", MatchedPath::as_str)
        .to_owned();
    let school = SchoolSlot::default();
    req.extensions_mut().insert(school.clone());
    let mut in_flight = InFlight::started(&metrics, method, route, school);
    let res = next.run(req).await;
    in_flight.status = res.status().as_u16();
    res
}

/// The in-flight accounting for one request, closed by `Drop`.
///
/// A dropped request future — the client disconnected, an outer timeout fired —
/// never returns from `next.run`, so a decrement written after the `.await`
/// leaks the gauge upwards for the process's whole life. Same shape as
/// [`web::room::Connected`]: `Drop` is the only placement every exit passes
/// through.
struct InFlight<'a> {
    metrics: &'a crate::telemetry::Metrics,
    method: String,
    route: String,
    school: SchoolSlot,
    started: std::time::Instant,
    /// The answer's status, or `499` ("client closed request") while there is
    /// no answer — which is what a cancelled request is recorded as.
    status: u16,
}

impl<'a> InFlight<'a> {
    fn started(
        metrics: &'a crate::telemetry::Metrics,
        method: String,
        route: String,
        school: SchoolSlot,
    ) -> Self {
        metrics.request_started(&method);
        Self {
            metrics,
            method,
            route,
            school,
            started: std::time::Instant::now(),
            status: 499,
        }
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        // `request_finished` owns both halves — the decrement and the duration
        // sample — so a cancelled request lands here too, under `499`.
        self.metrics.request_finished(
            &self.method,
            &self.route,
            self.status,
            self.school.get(),
            self.started.elapsed(),
        );
    }
}

/// Cap how long any request may wait on the database. The deadline covers both
/// the pool wait for a connection and the query run on it.
///
/// The health half this guard used to carry is gone with the engine that
/// needed it: a sqlx query issued on a dead pool fails with an error instead
/// of being parked until the socket returns, so there is no known-outage
/// verdict to publish and no queued work to refuse up front. What stays is
/// the honest [`AppError::DbTimeout`] semantics: a request that outlives the
/// deadline may or may not have applied its write, so a caller must not
/// blind-retry it.
///
/// Long-lived responses are unaffected: a WebSocket upgrade and an SSE stream
/// both return their response immediately and do the work afterwards, so
/// neither is measured against the timeout.
async fn request_timeout(req: Request, next: Next) -> Response {
    // `/limits` never touches the database — it serializes constants and this
    // process's own configuration. Holding it to the deadline would be a pure
    // own-goal: a frontend booting into a degraded backend is exactly when it
    // needs the validation contract. `/health` and its `/` mirror are exempt
    // for the same reason and one more: their own probe is bounded at 2s, far
    // inside this deadline, and the generic timeout body would replace the
    // probe's answer ("degraded, the database is down") with one that names
    // nothing.
    if matches!(req.uri().path(), "/limits" | "/health" | "/") {
        return next.run(req).await;
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
/// session cookie, so the two must never be recombined by default (the
/// cookie's `SameSite=Lax` in `web/auth.rs` is the other guard on that door).
/// A cross-origin dev frontend can't send the Lax cookie anyway, so credentials
/// bought nothing in mirror mode; a credentialed browser frontend requires
/// listing its origin in `CORS_ALLOWED_ORIGINS`.
///
/// One deliberate exception — `CORS_ALLOWED_ORIGINS=*` ("allow CORS to
/// anywhere for now", ordered 2026-09-13): a `*` entry mirrors every origin
/// WITH credentials, for a deployment whose frontend origins are not settled
/// yet. `SameSite=Lax` is what still holds the door: a Lax cookie never rides
/// a cross-site fetch, so what this opens is same-site origins (the
/// deployment's own subdomains and ports), not third-party websites. Replace
/// the `*` with the real origins once they are known.
///
/// Takes the allowlist as a parameter (env read once in `build_router`) so
/// tests can exercise the modes without racing on process-global env vars.
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

    // A `*` entry anywhere in the list asks for anywhere-mode: every origin
    // mirrored AND credentialed (the exception paragraph above).
    if allowlist.iter().any(|origin| origin == "*") {
        return layer
            .allow_origin(AllowOrigin::mirror_request())
            .allow_credentials(true);
    }
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

/// What the process can currently do, for a load balancer and a status page.
#[derive(serde::Serialize, utoipa::ToSchema)]
struct HealthResponse {
    /// `ok` when every dependency the API needs is answering, `degraded` when
    /// one is not (today: the database).
    #[schema(example = "ok")]
    status: &'static str,
    /// Did the bounded database probe (`SELECT 1`) answer?
    #[schema(example = "up")]
    db: &'static str,
    /// The optional AI bridge (see the `ai` tag).
    ai: AiHealth,
}

/// The AI bridge's side of [`HealthResponse`]. `enabled: false` is a
/// deployment without `AI_QUIC_ADDR`, not a fault — the core API has never
/// needed the bridge — so it never degrades the status.
#[derive(serde::Serialize, utoipa::ToSchema)]
struct AiHealth {
    enabled: bool,
    /// AI services currently registered on the bridge; `0` when it is off.
    workers: usize,
}

/// Health probe: the database verdict and the AI bridge, `503` when degraded.
///
/// Probes the database with a `SELECT 1` bounded to 2s — far inside the
/// request deadline — so a dead or wedged database answers `503` with a body
/// that says what is down, instead of the poll hanging on a full request
/// timeout.
#[utoipa::path(
    get,
    path = "/health",
    tag = "meta",
    responses(
        (status = 200, description = "Every dependency is answering", body = HealthResponse),
        (status = 503, description = "The database is down; the body says so", body = HealthResponse),
    ),
)]
async fn health(axum::extract::State(state): axum::extract::State<AppState>) -> Response {
    let up = matches!(
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            sqlx::query("SELECT 1").execute(&state.db),
        )
        .await,
        Ok(Ok(_))
    );
    let status = if up {
        axum::http::StatusCode::OK
    } else {
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(HealthResponse {
            status: if up { "ok" } else { "degraded" },
            db: if up { "up" } else { "down" },
            ai: AiHealth {
                enabled: state.ai.is_some(),
                workers: state.ai.as_ref().map_or(0, |ai| ai.workers().len()),
            },
        }),
    )
        .into_response()
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

/// Server clock: `{now}` UTC unix-millis, for a frontend to sync against.
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
