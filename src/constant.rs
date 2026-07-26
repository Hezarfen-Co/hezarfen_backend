//! Validation limits, in one place.

use crate::domain::message::Folder;
use crate::domain::preferences::{Language, Theme};
use crate::domain::role::Role;

pub const MIN_USERNAME_LEN: usize = 3;
pub const MAX_USERNAME_LEN: usize = 32;

/// Separators allowed inside a username (never at the edges, never doubled).
pub const USERNAME_SEPARATORS: [char; 3] = ['.', '_', '-'];

/// Names nobody may claim through `/auth/register`: they read as staff and
/// invite impersonation. Enforced at the registration endpoint, not in
/// `Username` itself, so the `ADMIN_USERNAME` bootstrap can still seed
/// accounts like `admin`.
pub const RESERVED_USERNAMES: [&str; 7] = [
    "admin",
    "administrator",
    "root",
    "support",
    "system",
    "moderator",
    "staff",
];

pub const MIN_PASSWORD_LEN: usize = 6;
pub const MAX_PASSWORD_LEN: usize = 128;

pub const MAX_NAME_LEN: usize = 100;

/// RFC 5321's practical upper bound for a full address.
pub const MAX_EMAIL_LEN: usize = 254;

/// Digit-count bounds for a phone number (E.164 allows at most 15).
pub const MIN_PHONE_DIGITS: usize = 7;
pub const MAX_PHONE_DIGITS: usize = 15;

pub const MAX_NOTE_TITLE_LEN: usize = 200;
pub const MAX_NOTE_CONTENT_LEN: usize = 10_000;

/// How many files one note may carry.
pub const MAX_NOTE_FILES: usize = 10;

pub const MAX_MESSAGE_SUBJECT_LEN: usize = 200;
pub const MAX_MESSAGE_BODY_LEN: usize = 10_000;
/// A message's optional sender-chosen tag ("Etüt", "Sınav", …) — free text,
/// rendered as a badge by the UI.
pub const MAX_MESSAGE_LABEL_LEN: usize = 50;

/// Bounds for a note file's original filename and its MIME content type.
pub const MAX_FILE_NAME_LEN: usize = 255;
pub const MAX_FILE_CONTENT_TYPE_LEN: usize = 100;

/// The school-adjustable per-file upload size limit (`max_file_bytes` in
/// settings): its default and the inclusive range a manager may set. The
/// ceiling is a server-protection hard cap — uploads buffer in memory and land
/// in single disk files, so it must stay modest no matter the school's wish.
pub const DEFAULT_MAX_FILE_BYTES: i64 = 5 * 1024 * 1024;
pub const MIN_MAX_FILE_BYTES: i64 = 1024;
pub const MAX_MAX_FILE_BYTES: i64 = 25 * 1024 * 1024;

/// Headroom on top of `MAX_MAX_FILE_BYTES` for the upload route's HTTP body
/// cap: multipart boundaries, part headers, and the filename ride alongside
/// the file bytes themselves.
pub const UPLOAD_BODY_OVERHEAD_BYTES: usize = 64 * 1024;

/// The content types an uploaded inline image (exam question images, pool
/// question photos) may declare — raster formats only. SVG is deliberately
/// out: it can carry scripts, and these bytes are served for inline display
/// to whole classes.
pub const QUESTION_IMAGE_CONTENT_TYPES: [&str; 4] =
    ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// Bounds for a pool question (the student-asked Q&A pool) and its solutions.
pub const MAX_POOL_QUESTION_TITLE_LEN: usize = 200;
pub const MAX_POOL_QUESTION_BODY_LEN: usize = 10_000;
pub const MAX_SOLUTION_BODY_LEN: usize = 10_000;

pub const MAX_EVENT_TITLE_LEN: usize = 200;
pub const MAX_EVENT_DESCRIPTION_LEN: usize = 2_000;

pub const MAX_EXAM_TITLE_LEN: usize = 200;
pub const MAX_EXAM_DESCRIPTION_LEN: usize = 2_000;

pub const MAX_COURSE_TITLE_LEN: usize = 200;
pub const MAX_COURSE_DESCRIPTION_LEN: usize = 2_000;

pub const MAX_SUBJECT_NAME_LEN: usize = 200;
pub const MAX_SUBJECT_DESCRIPTION_LEN: usize = 2_000;

pub const MAX_HOMEWORK_TITLE_LEN: usize = 200;
pub const MAX_HOMEWORK_DESCRIPTION_LEN: usize = 2_000;

/// A homework submission's optional free-text note, sent alongside its files.
pub const MAX_HOMEWORK_TEXT_LEN: usize = 5_000;

/// How many files one homework submission may carry.
pub const MAX_HOMEWORK_FILES_PER_SUBMISSION: usize = 10;

/// How many students a homework may be narrowed to. The whole-course default
/// carries no assigned list at all, so this caps only an explicit subset.
pub const MAX_HOMEWORK_ASSIGNED: usize = 200;

/// The default attendance states, and also the mandatory core: a school may
/// add its own statuses via `PATCH /settings`, but these four can never be
/// removed — the attendance rate's semantics are defined over them.
pub const DEFAULT_ATTENDANCE_STATUSES: [&str; 4] = ["present", "absent", "late", "excused"];

pub const MAX_SESSION_TOPIC_LEN: usize = 200;

/// The default exam kinds with their weights; schools replace the list via
/// `PATCH /settings`. An exam's kind decides how heavily it counts into the
/// course average — the defaults all weigh 1 (a plain average) so weighting
/// is opt-in policy, not baked-in opinion.
pub const DEFAULT_EXAM_KINDS: [(&str, i64); 6] = [
    ("homework", 1),
    ("quiz", 1),
    ("midterm", 1),
    ("final", 1),
    ("project", 1),
    ("oral", 1),
];

/// Bounds for the school-editable lists in settings (exam kinds, attendance
/// statuses): entry count and per-entry character length.
pub const MAX_SETTINGS_LIST_LEN: usize = 20;
pub const MAX_SETTINGS_ITEM_LEN: usize = 50;

/// Bounds for the grade-display bands in settings.
pub const MAX_GRADE_BANDS: usize = 20;
pub const MAX_GRADE_LABEL_LEN: usize = 20;

/// How many times `PATCH /settings` re-merges and retries when a concurrent
/// edit lands between its snapshot and its compare-and-set save.
pub const SETTINGS_UPDATE_RETRIES: usize = 3;

pub const MAX_TERM_NAME_LEN: usize = 100;

/// A published availability slot's optional note ("bring your report card").
pub const MAX_APPOINTMENT_NOTE_LEN: usize = 500;

/// Why a requester wants the meeting — required, and read by a human, so it
/// stays short.
pub const MAX_APPOINTMENT_REASON_LEN: usize = 1_000;

/// How many concrete slot rows one recurring publish may expand into. Weekly
/// occurrences over a full school year fit inside this; a runaway `until`
/// (a decade out) is refused instead of writing thousands of rows.
pub const MAX_SLOT_OCCURRENCES: usize = 52;

/// The default meal slots; schools replace the list via `PATCH /settings`, and
/// a published menu snapshots the slot it was written for, so retiring a slot
/// never rewrites history.
pub const DEFAULT_MEAL_SLOTS: [&str; 3] = ["breakfast", "lunch", "snack"];

/// The default dietary tags. A student's profile and a dish both carry tags
/// from this school-editable list, which is how "this dish is safe for them"
/// is answered without the backend knowing any nutrition.
pub const DEFAULT_DIETARY_TAGS: [&str; 5] = [
    "vegetarian",
    "vegan",
    "gluten_free",
    "lactose_free",
    "nut_allergy",
];

/// Bounds on one menu and the dishes hanging off it.
pub const MAX_DISH_NAME_LEN: usize = 100;
pub const MAX_DISH_DESCRIPTION_LEN: usize = 500;
pub const MAX_DISHES_PER_MENU: usize = 50;
pub const MAX_DISH_TAGS: usize = 10;

/// How many students one menu may seat. Absent capacity means uncapped, the
/// same shape a course's `capacity` uses.
pub const MAX_MENU_CAPACITY: i64 = 10_000;

/// Bounds on a student's dietary profile: one row per student, so the tag list
/// is the whole payload plus a free-text note for the kitchen.
pub const MAX_DIETARY_TAGS: usize = 10;
pub const MAX_DIETARY_NOTE_LEN: usize = 500;

/// Money is **minor units** (kuruş) as `i64` everywhere — never a decimal and
/// never a float. `MAX_DISH_PRICE_MINOR` caps one dish (10 000 ₺);
/// `MAX_LEDGER_AMOUNT_MINOR` caps one ledger line (100 000 ₺), which is wide
/// enough for a term's prepayment.
pub const MAX_DISH_PRICE_MINOR: i64 = 1_000_000;
pub const MAX_LEDGER_AMOUNT_MINOR: i64 = 10_000_000;

/// Bounds on the free-text a ledger line carries: how the money moved, and why.
pub const MAX_LEDGER_METHOD_LEN: usize = 50;
pub const MAX_LEDGER_NOTE_LEN: usize = 500;

/// Ceiling on the settings knob that closes booking (and cancelling) ahead of
/// a meal — one week. The knob itself is optional: absent means no cutoff.
pub const MAX_MEAL_CANCEL_CUTOFF_MINUTES: i64 = 7 * 24 * 60;

/// Inclusive ceiling for a meal slot's `serving_minute` — minutes past
/// midnight on the menu's date, so `0` = 00:00 and `1439` = 23:59. The cutoff
/// above counts back from that instant.
///
/// **The clock is UTC.** This backend deliberately stores no school timezone
/// (rejected feature), so staff enter the serving time in UTC: a UTC+3 school
/// types `540` (09:00) to mean noon locally. Optional per slot — a slot
/// without one keeps the old behaviour, midnight UTC of the menu's date.
pub const MAX_MEAL_SERVING_MINUTE: i64 = 24 * 60 - 1;

/// The only accepted course kinds. `course`: a regular class (ders). `study`:
/// a supervised study session (etüt). Behaviorally identical — the kind is a
/// label for the UI, everything else (enrollment, exams, sessions, marks)
/// works the same.
pub const COURSE_KINDS: [&str; 3] = ["course", "study", "club"];

/// The only accepted exam modes. `sync`: everyone sits the exam inside one
/// fixed window. `async`: each student starts inside the window and gets their
/// own `duration_ms` slice of it. `open`: no window — students sit anytime,
/// with an optional per-attempt `duration_ms` (absent = unlimited time). An
/// exam with no mode at all is an offline-graded draft and cannot be sat.
pub const EXAM_MODES: [&str; 3] = ["sync", "async", "open"];

/// The only accepted homework grades. `done`: submitted and complete;
/// `incomplete`: submitted but lacking; `missing`: not done. A teacher-set
/// `missing` is a deliberate verdict, distinct from the roster's *computed*
/// missing (unsubmitted past due). The `HomeworkStatus` newtype enforces the
/// set; the DDL carries no ASSERT (repo convention), exactly like `EXAM_MODES`.
pub const HOMEWORK_STATUSES: [&str; 3] = ["done", "incomplete", "missing"];

/// The only accepted meal-booking states. A cancel flips the status and stamps
/// `cancelled_at` instead of deleting the row, so a seat freed after the cutoff
/// is still auditable against the ledger line it charged.
pub const MEAL_BOOKING_STATUSES: [&str; 2] = ["booked", "cancelled"];

/// The only accepted meal-attendance states — did the student actually eat.
/// Deliberately *not* the school's `attendance_statuses`: a canteen line has no
/// "late" or "excused", and mixing the two would let a school edit one meaning
/// while changing the other.
pub const MEAL_ATTENDANCE_STATUSES: [&str; 2] = ["served", "missed"];

/// The only accepted ledger kinds. `charge`: a booking billed the student.
/// `credit`: money in (a payment, or an opening balance). `reversal`: a charge
/// undone — the ledger is append-only, so a cancelled booking writes a new
/// opposing line rather than editing or deleting the charge.
pub const MEAL_LEDGER_KINDS: [&str; 3] = ["charge", "credit", "reversal"];

/// Inclusive bounds for an exam's per-attempt duration, milliseconds
/// (1 minute to 24 hours). Required for `async`, optional for `open`.
pub const MIN_EXAM_DURATION_MS: i64 = 60 * 1000;
pub const MAX_EXAM_DURATION_MS: i64 = 24 * 60 * 60 * 1000;

/// Upper bound for an exam's attempt limit; `UNLIMITED_EXAM_ATTEMPTS` (zero)
/// is the wire-and-storage spelling of "no limit". A limit of 1 — the
/// default — is the classic single sitting.
pub const MAX_EXAM_ATTEMPTS: i64 = 100;
pub const UNLIMITED_EXAM_ATTEMPTS: i64 = 0;

/// Cadence of the background keepalive query on the database WebSocket. The
/// traffic keeps the connection from being dropped as idle; when it does drop,
/// the ping also makes the SDK notice and reconnect long before the next real
/// request would. Doubles as the liveness probe behind
/// [`crate::state::DbHealth`], so this is also the widest window in which a
/// request can reach a socket already known-dead — keep it short.
pub const DB_KEEPALIVE_INTERVAL_SECS: u64 = 5;

/// How long a keepalive ping may hang before the socket counts as down. The
/// SDK parks queries indefinitely while it reconnects (its retry loop stops
/// draining the request channel), so the ping needs its own deadline or the
/// probe hangs with everything else and never reports.
pub const DB_PING_TIMEOUT_SECS: u64 = 2;

/// Ceiling on a single HTTP request. Backstop for requests that reached the
/// database in the window between the socket dying and the keepalive noticing:
/// without it they park until the database returns, which can be hours.
/// Generous enough not to clip a legitimate slow upload at `max_file_bytes`.
pub const REQUEST_TIMEOUT_SECS: u64 = 30;

/// Backoff ceiling for the boot connection retry. The database is usually a
/// sibling container coming up in parallel, so the first attempts fail; the
/// process retries forever rather than exiting, because exiting turns a
/// few-second wait into a container restart loop that outruns its budget and
/// stays down.
pub const DB_CONNECT_BACKOFF_MAX_SECS: u64 = 5;

/// Cadence of the `state` ticks on the student exam-room WebSocket
/// (`GET /exams/{id}/attempt/ws`).
pub const EXAM_WS_TICK_SECS: u64 = 2;

/// Ceiling on a `question_id` arriving on the exam-room WebSocket. The field is
/// a record key — a 26-char ULID in every real payload — not free text, so this
/// is generous by a factor of two and change. The cap exists because the
/// per-question error frame *echoes* the id back: axum's default WebSocket
/// frame limit is 64 MiB, so without it a client can make its own room reflect
/// that whole payload. Rejected before any database work, and unattributed — an
/// unusable id names no question, so nothing of it is sent back.
pub const MAX_QUESTION_ID_LEN: usize = 64;

/// The only accepted question kinds. `choice`: pick one of the listed
/// choices, auto-scorable. `text`: free text, judged by the grader.
pub const QUESTION_KINDS: [&str; 2] = ["choice", "text"];

pub const MAX_QUESTION_TEXT_LEN: usize = 2_000;

/// Inclusive bounds for a question's points (its share of the auto-score).
pub const MIN_QUESTION_POINTS: i64 = 1;
pub const MAX_QUESTION_POINTS: i64 = 100;

/// Bounds for a choice question's option list and each option's text.
pub const MIN_QUESTION_CHOICES: usize = 2;
pub const MAX_QUESTION_CHOICES: usize = 10;
pub const MAX_CHOICE_TEXT_LEN: usize = 500;

pub const MAX_ANSWER_TEXT_LEN: usize = 10_000;

/// Inclusive bounds for an exam mark.
pub const MIN_MARK: i64 = 0;
pub const MAX_MARK: i64 = 100;

/// Inclusive bounds for an exam kind's weight in the course average. The
/// minimum of 1 keeps every graded exam counted and the average's denominator
/// non-zero.
pub const MIN_EXAM_KIND_WEIGHT: i64 = 1;
pub const MAX_EXAM_KIND_WEIGHT: i64 = 100;

pub const SESSION_DURATION_DAYS: i64 = 7;

/// How far in the past a request-supplied schedule instant (exam window,
/// lesson, event time) may lie before it is rejected as backdated. The grace
/// absorbs request latency and modest client-clock skew — a "starts now"
/// submission must survive its own round trip — while still refusing
/// genuinely past deadlines.
pub const SCHEDULE_PAST_GRACE_MS: i64 = 60 * 1000;

/// Upper bound on a list endpoint's `limit` page-size parameter. Omitting
/// `limit` returns every (remaining) row; when supplied it must be
/// `1..=MAX_PAGE_LIMIT`. The `#[param(maximum = ...)]` and the "1 and 500"
/// wording in `web::page` mirror this literal — keep them in step.
pub const MAX_PAGE_LIMIT: i64 = 500;

/// Wire-protocol identifier for the AI bridge, sent in [`crate::ai::protocol::Hello`]
/// and used as the QUIC ALPN. Bump both together on a breaking frame change:
/// ALPN mismatch rejects an old service at the TLS handshake, before it can
/// send a frame we would misparse.
pub const AI_PROTOCOL: &str = "hab/1";
pub const AI_ALPN: &[u8] = b"hab/1";

/// Hard ceiling on one AI-bridge frame. Checked against the length prefix
/// before the body buffer is allocated. Generous because a payload may carry a
/// base64 question image, but bounded so a bad length cannot exhaust memory.
pub const AI_MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Default wait for one AI request before the stream is abandoned
/// (`AI_REQUEST_TIMEOUT_SECS`). Model inference is slow, so this is far longer
/// than [`REQUEST_TIMEOUT_SECS`] — AI calls must not sit on an HTTP request
/// path that the outer timeout would kill first.
pub const AI_DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;

/// Upper bound on a worker's self-declared `max_concurrent`, and the value
/// used when it declares none. A worker that claims a huge number would let
/// the registry pile every request onto one service.
pub const AI_MAX_CONCURRENT_PER_WORKER: usize = 64;
pub const AI_DEFAULT_CONCURRENT_PER_WORKER: usize = 8;

/// QUIC idle timeout and keepalive for a service connection. The keepalive is
/// well under the idle timeout so an idle-but-healthy service is never dropped
/// for being quiet; a service that actually died is deregistered within the
/// idle window without any application-level heartbeat frame.
pub const AI_IDLE_TIMEOUT_SECS: u64 = 30;
pub const AI_KEEPALIVE_SECS: u64 = 10;

/// The capability an AI service declares to answer chatbot turns. The bridge
/// routes a chat request to any worker carrying it; with none registered the
/// chat endpoints report the service as unavailable.
pub const AI_CHAT_CAPABILITY: &str = "chat.reply";

// --- AI worker presence: the GATE, never the dispatcher -----------------
//
// Read `crate::ai::presence` before touching anything below. In one line: the
// `ai_worker` table answers "could *anything, anywhere* serve this
// capability?" so a POST on a replica holding no worker can still be accepted
// (a peer's claim loop will answer it). It is a heartbeated cache of other
// processes' sockets and it is allowed to be wrong in both directions. The
// in-process `ai::registry::Registry` is the dispatcher and the only thing
// that may decide who actually answers.

/// The heartbeated presence table. One row per connected worker, written by
/// whichever replica owns that worker's QUIC connection.
pub const AI_WORKER_TABLE: &str = "ai_worker";

/// How often a replica restamps `seen_at` for the workers it holds, and how
/// long a row is believed without a restamp. The lease is a comfortable
/// multiple of the beat (three), so a single missed heartbeat never hides a
/// live worker, and it matches [`AI_IDLE_TIMEOUT_SECS`] — the window in which
/// the transport itself notices a dead service — so a replica that dies
/// outright leaves a phantom for no longer than a service that dies outright.
pub const AI_WORKER_HEARTBEAT_SECS: u64 = 10;
pub const AI_WORKER_LEASE_SECS: i64 = 30;

/// Write (or restamp) one worker's row. `UPSERT` rather than `CREATE`: the
/// heartbeat replays the whole live set every beat, which is what heals a row
/// lost to a database blip or written before the presence table was attached.
pub const AI_WORKER_ANNOUNCE: &str = "
    UPSERT $id CONTENT { service: $service, capabilities: $capabilities,
        seen_at: time::unix(time::now()) * 1000 };
";

/// Drop a worker whose connection closed. Immediate — the gate should not
/// hold a door open for a service that has already said goodbye.
pub const AI_WORKER_WITHDRAW: &str = "DELETE $id;";

/// Forget workers nobody has restamped within the lease: the replica holding
/// them died without deregistering. Any replica's heartbeat may run it — the
/// rows are keyed by worker, not by owner.
pub const AI_WORKER_SWEEP: &str = "
    DELETE ai_worker WHERE seen_at < time::unix(time::now()) * 1000 - $lease_ms;
";

/// The gate read. Age-filtered in the query, not by the sweep, so a phantom
/// row is inert the moment it is stale rather than when someone gets round to
/// deleting it.
pub const AI_WORKER_SERVES: &str = "
    SELECT VALUE id FROM ai_worker WHERE $capability IN capabilities
        AND seen_at >= time::unix(time::now()) * 1000 - $lease_ms LIMIT 1;
";

/// Hard ceiling on one chat message's characters — the newtype bound, above
/// which no school setting can reach. Sized for a pasted question with its
/// working, well under [`AI_MAX_FRAME_BYTES`] once history rides along.
pub const MAX_CHATBOT_MESSAGE_LEN: usize = 8_000;

/// The school-adjustable per-message character cap (`max_chatbot_message_len` in
/// settings): its default and the inclusive range a manager may set. The
/// ceiling is [`MAX_CHATBOT_MESSAGE_LEN`] — a longer prompt costs the AI service
/// context it needs for the history.
pub const DEFAULT_MAX_CHATBOT_MESSAGE_LEN: i64 = 4_000;
pub const MIN_MAX_CHATBOT_MESSAGE_LEN: i64 = 100;
pub const MAX_MAX_CHATBOT_MESSAGE_LEN: i64 = MAX_CHATBOT_MESSAGE_LEN as i64;

/// Bound on a chat thread's display name. A thread is named by hand or not at
/// all — nothing auto-titles one — so this only has to fit a line a user typed.
pub const MAX_CHATBOT_THREAD_TITLE_LEN: usize = 200;

/// How many prior user/assistant turns of a thread are replayed to the
/// AI service as context (`chatbot_history_turns` in settings): its default and
/// the inclusive range a manager may set. Every turn is re-sent on every
/// request, so the ceiling bounds both the frame size and the inference cost.
pub const DEFAULT_CHATBOT_HISTORY_TURNS: i64 = 10;
pub const MIN_CHATBOT_HISTORY_TURNS: i64 = 1;
pub const MAX_CHATBOT_HISTORY_TURNS: i64 = 50;

/// How many threads one user may keep (`max_chatbot_threads` in
/// settings): its default and the inclusive range a manager may set. Reached,
/// the user deletes an old thread before starting a new one — the cap is
/// storage protection, not a usage quota.
pub const DEFAULT_MAX_CHATBOT_THREADS: i64 = 50;
pub const MIN_MAX_CHATBOT_THREADS: i64 = 1;
pub const MAX_MAX_CHATBOT_THREADS: i64 = 500;

/// How often the chat stream re-reads a `pending` assistant message while it
/// waits for the answer. The reply lands in the database from whichever task
/// owns the bridge stream, so the reader polls rather than sharing state.
pub const CHAT_STREAM_POLL_MS: u64 = 200;

/// How long an assistant message may sit `pending` before a reader gives up on
/// it and reports it failed. Comfortably past
/// [`AI_DEFAULT_REQUEST_TIMEOUT_SECS`] (which `AI_REQUEST_TIMEOUT_SECS` may
/// raise), because the answering task normally stamps the failure itself —
/// this only catches the row whose task died with the process.
pub const CHATBOT_PENDING_STALE_SECS: i64 = 300;

// --- the chat claim queue ----------------------------------------------

/// How often a replica looks for an unanswered turn, and how many it takes in
/// one sweep. The poll is what replaces the in-process `tokio::spawn` a POST
/// used to do, so it is the answer's added latency floor — matched to
/// [`CHAT_STREAM_POLL_MS`], the cadence the client already waits at, so a turn
/// is never claimed slower than its own reader refreshes. The batch is small
/// on purpose: a claim is a promise to dispatch now, and dispatching more at
/// once than a worker's `max_concurrent` would only turn the surplus into
/// `busy` failures.
pub const CHATBOT_CLAIM_POLL_MS: u64 = 200;
pub const CHATBOT_CLAIM_BATCH: i64 = 4;

/// How long a claim is believed before another replica may take the turn back.
///
/// Sized between the two horizons it sits between, and both bounds bite (see
/// `the_claim_horizon_sits_between_the_inference_and_the_stale_window`):
///
/// * **Above the AI request timeout** ([`AI_DEFAULT_REQUEST_TIMEOUT_SECS`],
///   30s), with room to spare, because a reclaim that fires while the first
///   claimer is still waiting on a legitimate inference makes two services
///   answer the same turn — twice the cost for one reply (the second is then
///   discarded by the `status = 'pending'` gate on the settle, so it is waste,
///   not corruption). Three times the default leaves margin for the settle
///   write and for a moderately raised `AI_REQUEST_TIMEOUT_SECS`.
/// * **Below [`CHATBOT_PENDING_STALE_SECS`]** (300s), because a turn is only
///   claimable inside that window; a reclaim horizon at or above it would
///   never fire before the turn stopped being worth answering, making the
///   dead-claimer retry dead code. At 90s a turn survives two lost claimers
///   and still gets answered.
pub const CHATBOT_CLAIM_RECLAIM_SECS: i64 = 90;

/// The largest per-request AI deadline a deployment may configure
/// (`AI_REQUEST_TIMEOUT_SECS`). Anything above it is clamped down to it at
/// startup, loudly — see [`crate::config`].
///
/// Not a taste limit; it is the claim queue's arithmetic, and the bound the
/// doc above only *asserted* before. A dispatch allowed to outlive
/// [`CHATBOT_CLAIM_RECLAIM_SECS`] has its turn reclaimed and re-sent while the
/// first inference is still legitimately running, so the model runs twice for
/// one reply. (The settle's `status = 'pending'` gate still admits only one
/// answer, so it is waste rather than corruption — but a worker with side
/// effects makes it worse than waste.) Nothing but the parser stood between an
/// operator and that: `AI_REQUEST_TIMEOUT_SECS=120` used to be honoured
/// verbatim.
///
/// Clamped rather than refused, because a slow model is a legitimate thing to
/// own and a boot that dies over it helps nobody — and because the ceiling is
/// real either way: a reply that arrives after
/// [`CHATBOT_PENDING_STALE_SECS`] is discarded by the read-time projection, so
/// no deadline near it was ever going to produce an answer a user sees. A
/// school that genuinely needs longer raises the horizon and the staleness
/// window together, and the ordering tests police the result.
///
/// Two thirds of the horizon, derived and never hand-kept: raising the horizon
/// raises this with it, so the two numbers cannot drift apart. The remaining
/// third is the room the reply needs to arrive, be capped and be stamped
/// before anyone may reclaim the turn.
pub const AI_MAX_REQUEST_TIMEOUT_SECS: u64 = CHATBOT_CLAIM_RECLAIM_SECS as u64 * 2 / 3;

/// Take up to [`CHATBOT_CLAIM_BATCH`] unanswered turns for this replica.
///
/// Two statements because SurrealDB has no `LIMIT` on `UPDATE`: the first
/// picks candidates, the second claims them. The pick is advisory — a peer may
/// take a row in between — so the `UPDATE` repeats the *whole* guard, and
/// per-record `UPDATE ... WHERE` is atomic, so a row claimed by a rival simply
/// does not come back in `RETURN AFTER`. Nothing here consults `ai_worker`:
/// the caller has already asked its own registry whether it can answer.
///
/// The guard, term by term: only a reserved assistant row (`pending`), only
/// one young enough that somebody is still waiting for it (`$stale_ms` — past
/// that a reader already shows it failed, so answering it would resurrect a
/// turn the user was told was lost), and only one that is unclaimed *or* whose
/// claimer has gone quiet past `$reclaim_ms`. Both disjuncts are required: a
/// row whose claimer died must be retried, not left forever.
pub const CHATBOT_CLAIM: &str = "
    LET $ids = (SELECT VALUE id FROM chatbot_message
        WHERE status = 'pending' AND role = 'assistant'
            AND created_at >= time::unix(time::now()) * 1000 - $stale_ms
            AND (claimed_by = NONE OR claimed_at < time::unix(time::now()) * 1000 - $reclaim_ms)
        ORDER BY created_at ASC, id ASC LIMIT $batch);
    UPDATE $ids SET claimed_by = $me, claimed_at = time::unix(time::now()) * 1000
        WHERE status = 'pending'
            AND (claimed_by = NONE OR claimed_at < time::unix(time::now()) * 1000 - $reclaim_ms)
        RETURN AFTER;
";

/// How long a dialling AI service has to complete its `Hello`/`Welcome`
/// exchange before the connection is dropped. Short: the handshake is one
/// frame each way, and an unauthenticated connection must not be able to
/// occupy the listener indefinitely.
pub const AI_HANDSHAKE_TIMEOUT_SECS: u64 = 10;

// --- rate limiting -----------------------------------------------------

/// Default requests-per-minute-per-IP for `/auth/login` + `/auth/register`.
pub const DEFAULT_AUTH_RATE_LIMIT: u32 = 10;
/// Default requests-per-minute-per-IP across the whole API.
pub const DEFAULT_API_RATE_LIMIT: u32 = 300;
/// Default chatbot messages per minute *per user*. Sits far above a human
/// typing while still blunting a scripted fan-out at the AI service behind the
/// bridge — which is the cost this tier rations, not this process.
pub const DEFAULT_CHATBOT_RATE_LIMIT: u32 = 20;

/// Once the bucket map holds this many distinct client IPs, expired entries are
/// swept out on the next check. Keeps memory bounded without a reaper task.
pub const PURGE_AT: usize = 10_000;

// --- time spans --------------------------------------------------------

pub const MILLIS_PER_DAY: i64 = 24 * 60 * 60 * 1000;

/// One week, the only recurrence step this backend expands.
pub const MILLIS_PER_WEEK: i64 = 7 * MILLIS_PER_DAY;

// --- enum value tables -------------------------------------------------

/// Every role, lowest privilege first.
pub const ROLES: [Role; 5] = [
    Role::Parent,
    Role::Student,
    Role::Teacher,
    Role::Manager,
    Role::Admin,
];

pub const THEMES: [Theme; 2] = [Theme::Light, Theme::Dark];

pub const LANGUAGES: [Language; 2] = [Language::Tr, Language::En];

/// Folders a sender may file their side into (`Sent` is their home).
pub const SENDER_FOLDERS: [Folder; 3] = [Folder::Sent, Folder::Archive, Folder::Trash];
/// Folders a recipient may file their side into (`Inbox` is their home).
pub const RECIPIENT_FOLDERS: [Folder; 3] = [Folder::Inbox, Folder::Archive, Folder::Trash];

/// The two lifecycle states of a pool question. `pending`: awaiting teacher+
/// approval, visible only to the asker and to teacher+. `approved`: in the
/// pool, school-wide. Rejection is not a state — a teacher+ simply deletes the
/// question.
pub const STATUS_PENDING: &str = "pending";
pub const STATUS_APPROVED: &str = "approved";
pub const POOL_QUESTION_STATUSES: [&str; 2] = [STATUS_PENDING, STATUS_APPROVED];

/// The two reaches of a bank question template: the author's own drawer, or
/// published to the whole school.
pub const BANK_VISIBILITY_PRIVATE: &str = "private";
pub const BANK_VISIBILITY_SCHOOL: &str = "school";

// --- fixed keys and literals -------------------------------------------

/// The settings singleton's fixed key: one school per deployment, one settings
/// row.
pub const SETTINGS_KEY: &str = "school";

/// A fixed password whose hash is the login decoy (see
/// [`crate::domain::user::PasswordHash::verify_decoy`]). Not a secret — it
/// never matches a real account.
pub const DECOY_PASSWORD: &str = "decoy-password-not-a-secret";

/// The `error_code` a stale `pending` chatbot row presents as. Distinct from
/// the boot sweep's `interrupted`: this one was never repaired, only projected.
pub const STALE_ERROR_CODE: &str = "timed_out";

/// Ceiling on a stored `error_code`. Codes are short slugs, but one arrives
/// from an out-of-process AI service — a trust boundary — so it is trimmed
/// rather than trusted.
pub const MAX_ERROR_CODE_LEN: usize = 64;

/// Replacements applied *after* lowercasing, in order. One table, two
/// consumers ([`crate::domain::text_fold::search_fold`] and
/// [`crate::domain::text_fold::search_fold_sql`]): the needle and the column
/// are folded by the same rules by construction, which is the whole point.
pub const TEXT_FOLD_REPLACEMENTS: &[(&str, &str)] = &[
    ("\u{307}", ""), // combining dot above, left behind by İ → i̇
    ("ı", "i"),
    ("ş", "s"),
    ("ğ", "g"),
    ("ç", "c"),
    ("ö", "o"),
    ("ü", "u"),
    ("â", "a"),
    ("î", "i"),
    ("û", "u"),
];

// --- response shaping --------------------------------------------------

/// Bodies larger than this are streamed through without buffering — JSON pages
/// sit far below it, so the cap only bounds worst-case memory. A response whose
/// `Content-Length` exceeds it (or is absent, i.e. a stream) skips the ETag
/// middleware.
pub const MAX_ETAG_BODY_BYTES: u64 = 1 << 20; // 1 MiB

/// How many `delta` events a completed chatbot answer is cut into, and the
/// shortest chunk worth emitting.
pub const REPLY_CHUNKS: usize = 8;
pub const MIN_CHUNK_CHARS: usize = 24;

// --- query text --------------------------------------------------------

/// The whole of [`crate::domain::bank_question::BankQuestion::usage_counts`]:
/// **one** statement (no `;`), so a page of templates costs one round trip no
/// matter how long it is. Named so a test can assert that, since a per-row
/// `count()` is exactly the N+1 this page was cleaned of once.
pub const USAGE_COUNTS_SQL: &str =
    "SELECT from_bank, count() AS n FROM exam_question WHERE from_bank IN $ids GROUP BY from_bank";

// --- database tables ---------------------------------------------------

pub const USER_TABLE: &str = "user";
pub const SESSION_TABLE: &str = "session";
pub const NOTE_TABLE: &str = "note";
pub const MESSAGE_TABLE: &str = "message";
pub const NOTE_FILE_TABLE: &str = "note_file";
pub const EVENT_TABLE: &str = "event";
pub const ATTENDANCE_TABLE: &str = "attendance";
pub const REGISTRATION_TABLE: &str = "registration";
pub const EXAM_TABLE: &str = "exam";
pub const EXAM_RESULT_TABLE: &str = "exam_result";
pub const EXAM_ATTEMPT_TABLE: &str = "exam_attempt";
pub const EXAM_QUESTION_TABLE: &str = "exam_question";
pub const QUESTION_IMAGE_TABLE: &str = "question_image";
pub const EXAM_ANSWER_TABLE: &str = "exam_answer";
pub const ANSWER_IMAGE_TABLE: &str = "answer_image";
pub const BANK_QUESTION_TABLE: &str = "bank_question";
pub const BANK_QUESTION_IMAGE_TABLE: &str = "bank_question_image";
pub const COURSE_TABLE: &str = "course";
pub const ENROLLMENT_TABLE: &str = "enrollment";
pub const PARENT_LINK_TABLE: &str = "parent_link";
pub const COURSE_SESSION_TABLE: &str = "course_session";
pub const SESSION_ATTENDANCE_TABLE: &str = "session_attendance";
pub const WORK_ENTRY_TABLE: &str = "work_entry";
pub const POMODORO_SESSION_TABLE: &str = "pomodoro_session";
pub const SETTINGS_TABLE: &str = "settings";
pub const TERM_TABLE: &str = "term";
pub const SUBJECT_TABLE: &str = "subject";
pub const POOL_QUESTION_TABLE: &str = "pool_question";
pub const SOLUTION_TABLE: &str = "solution";
pub const HOMEWORK_TABLE: &str = "homework";
pub const HOMEWORK_SUBMISSION_TABLE: &str = "homework_submission";
pub const HOMEWORK_FILE_TABLE: &str = "homework_file";
pub const HOMEWORK_RESULT_TABLE: &str = "homework_result";
pub const CHATBOT_THREAD_TABLE: &str = "chatbot_thread";
pub const CHAT_MESSAGE_TABLE: &str = "chatbot_message";
pub const APPOINTMENT_SLOT_TABLE: &str = "appointment_slot";
pub const APPOINTMENT_TABLE: &str = "appointment";
pub const MENU_TABLE: &str = "menu";
pub const MENU_DISH_TABLE: &str = "menu_dish";
pub const DIETARY_PROFILE_TABLE: &str = "dietary_profile";
pub const MEAL_BOOKING_TABLE: &str = "meal_booking";
pub const MEAL_ATTENDANCE_TABLE: &str = "meal_attendance";
pub const MEAL_LEDGER_TABLE: &str = "meal_ledger";

// --- boot leader election ----------------------------------------------

/// The lock row every booting process races for (see
/// [`crate::database::boot_once`]). Its own table, and the only SCHEMALESS one
/// in the database: the lock has to be claimable *before* the SCHEMAFULL batch
/// that defines every other table has run, so a lock whose own schema needed
/// migrating could not guard the migration. `IF NOT EXISTS` makes the
/// definition safe for all N processes to issue.
pub const MIGRATION_LOCK_TABLE: &str = "migration_lock";
pub const MIGRATION_LOCK_DDL: &str = "DEFINE TABLE IF NOT EXISTS migration_lock SCHEMALESS;";

/// Claim the lock. A deterministic id is the whole mechanism: SurrealDB v3
/// answers a `CREATE` on an existing id with "already exists" rather than
/// overwriting it, so of N concurrent processes exactly one gets an `Ok`.
/// Returns the holder like the takeover does, so one caller reads both.
pub const MIGRATION_LOCK_CLAIM: &str = "
    CREATE migration_lock:boot SET holder = $me, claimed_at = time::unix(time::now()) * 1000,
        applied_at = NONE, applies = 0 RETURN VALUE holder;
";

/// What a loser needs to know, decided by the *database's* clock — never the
/// process's, since two replicas' clocks disagree and the lease horizon is the
/// one thing a wrong clock could turn into a concurrent migration.
///
/// `applied` means "applied *this* binary's schema": a stamp carries the
/// fingerprint of the DDL that produced it, and a peer's stamp with any other
/// fingerprint is no evidence about the schema this process is about to write
/// (see [`crate::database::migration_fingerprint`]).
pub const MIGRATION_LOCK_STATE: &str = "
    SELECT holder, applied_at != NONE AND (fingerprint ?? '') = $fingerprint AS applied,
        claimed_at >= time::unix(time::now()) * 1000 - $lease_ms AS live
        FROM ONLY migration_lock:boot;
";

/// Take the lock off a holder that cannot be relied on: one that has gone
/// silent past the lease horizon (killed mid-migration), or one that finished
/// with a *different* schema than ours — a stamp is the last thing a leader
/// writes, so a foreign fingerprint means nobody is in the DDL right now and
/// this binary's own migration still has to run.
///
/// Per-record `UPDATE ... WHERE` is atomic, so a second taker re-reads the
/// freshly bumped `claimed_at` and is refused (empty result). Clears
/// `applied_at` and `fingerprint`: the previous generation's stamp says nothing
/// about this one's schema.
pub const MIGRATION_LOCK_TAKEOVER: &str = "
    UPDATE migration_lock:boot SET holder = $me, claimed_at = time::unix(time::now()) * 1000,
        applied_at = NONE, fingerprint = NONE
        WHERE claimed_at < time::unix(time::now()) * 1000 - $lease_ms
            OR (applied_at != NONE AND (fingerprint ?? '') != $fingerprint)
        RETURN VALUE holder;
";

/// Renew the lease while the migration runs, and stamp it done afterwards.
/// Both are `holder`-guarded and both report the holder back, so a leader that
/// was declared dead and replaced not only stops being able to write the lock —
/// it finds out (an empty result), which is the only way it can learn that a
/// peer is now applying the same DDL underneath it.
pub const MIGRATION_LOCK_HEARTBEAT: &str = "
    UPDATE migration_lock:boot SET claimed_at = time::unix(time::now()) * 1000
        WHERE holder = $me RETURN VALUE holder;
";
pub const MIGRATION_LOCK_STAMP: &str = "
    UPDATE migration_lock:boot SET applied_at = time::unix(time::now()) * 1000, applies += 1,
        fingerprint = $fingerprint
        WHERE holder = $me RETURN VALUE holder;
";

/// How long a lock may go unrenewed before a peer may take it over. Must stay
/// a comfortable multiple of `MIGRATION_LOCK_HEARTBEAT_SECS`: it is the number
/// of missed beats that separates "the leader was killed" from "the leader is
/// slow", and mistaking the second for the first runs two migrations at once.
pub const MIGRATION_LOCK_LEASE_SECS: i64 = 30;
pub const MIGRATION_LOCK_HEARTBEAT_SECS: u64 = 5;

/// How long a non-leader waits for the leader's `applied_at` before failing the
/// boot, and how often it looks. Bounded because a boot that hangs forever is
/// indistinguishable from a hung database; loud because the alternative —
/// serving on a schema nobody confirmed — is worse.
pub const MIGRATION_LOCK_WAIT_SECS: u64 = 180;
pub const MIGRATION_LOCK_POLL_MS: u64 = 200;

// --- schema migration SQL ----------------------------------------------

/// Repairs that must run *before* the DDL batch, because the DDL is what makes
/// them impossible.
///
/// `MIGRATION` retires `exam_question.source_bank` with `REMOVE FIELD`, and
/// once the column is gone SCHEMAFULL rejects every write to a row that still
/// stores it ("Found field 'source_bank', but no such field exists") — an
/// UNSET included. So the value has to go while its definition is still
/// standing. Runs as its own query for the usual reason (see `MIGRATION`).
///
/// The table-exists guard is load-bearing: on a fresh database `MIGRATION` has
/// not run yet, and an UPDATE against an undefined table is an error, not an
/// empty result. On a database that has the table but never had the column the
/// `WHERE` simply matches nothing.
pub const PRE_REPAIR: &str = "
    IF 'exam_question' IN object::keys((INFO FOR DB).tables) {
        UPDATE exam_question UNSET source_bank WHERE source_bank != NONE
    };
";

/// SCHEMAFULL schema: every column is typed, references use `record<..>`.
/// Idempotent — safe to run on every boot: `IF NOT EXISTS` guards the
/// definitions and `REMOVE ... IF EXISTS` retires schema (like the
/// single-attempt unique index) exactly once. DDL only — data backfills live
/// in `BACKFILL`, which runs as a *separate* query: statements inside one
/// batch see the schema as it stood when the batch started, so an UPDATE next
/// to a fresh `DEFINE FIELD audience.*` writes against the old field set and
/// SCHEMAFULL silently strips the very keys being backfilled.
pub const MIGRATION: &str = "
    DEFINE TABLE IF NOT EXISTS user SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS username ON user TYPE string;
    DEFINE FIELD IF NOT EXISTS password_hash ON user TYPE string;
    DEFINE FIELD IF NOT EXISTS role ON user TYPE string DEFAULT 'student';
    DEFINE FIELD IF NOT EXISTS name ON user TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS surname ON user TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS email ON user TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS phone ON user TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS birth_date ON user TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS theme ON user TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS language ON user TYPE option<string>;
    DEFINE INDEX IF NOT EXISTS user_username ON user FIELDS username UNIQUE;

    DEFINE TABLE IF NOT EXISTS session SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS user ON session TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS token ON session TYPE string;
    DEFINE FIELD IF NOT EXISTS expires_at ON session TYPE int;
    DEFINE INDEX IF NOT EXISTS session_token ON session FIELDS token UNIQUE;
    DEFINE INDEX IF NOT EXISTS session_expires ON session FIELDS expires_at;

    DEFINE TABLE IF NOT EXISTS note SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS user ON note TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS title ON note TYPE string;
    DEFINE FIELD IF NOT EXISTS content ON note TYPE string;
    DEFINE INDEX IF NOT EXISTS note_user ON note FIELDS user;

    DEFINE TABLE IF NOT EXISTS message SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS sender ON message TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS recipient ON message TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS subject ON message TYPE string;
    DEFINE FIELD IF NOT EXISTS body ON message TYPE string;
    DEFINE FIELD IF NOT EXISTS label ON message TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS sent_at ON message TYPE int;
    DEFINE FIELD IF NOT EXISTS read ON message TYPE bool DEFAULT false;
    DEFINE FIELD IF NOT EXISTS sender_folder ON message TYPE string DEFAULT 'sent';
    DEFINE FIELD IF NOT EXISTS recipient_folder ON message TYPE string DEFAULT 'inbox';
    -- Where each side's copy sat before it was filed into archive/trash, so a
    -- restore lands where it came from. NONE while the copy is in its home
    -- folder — and on rows filed before this field existed (2026-07-23), which
    -- restore to the home folder just as they always did.
    DEFINE FIELD IF NOT EXISTS sender_origin ON message TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS recipient_origin ON message TYPE option<string>;
    DEFINE INDEX IF NOT EXISTS message_sender ON message FIELDS sender;
    DEFINE INDEX IF NOT EXISTS message_recipient ON message FIELDS recipient;

    DEFINE TABLE IF NOT EXISTS note_file SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS note ON note_file TYPE record<note>;
    DEFINE FIELD IF NOT EXISTS name ON note_file TYPE string;
    DEFINE FIELD IF NOT EXISTS content_type ON note_file TYPE string;
    DEFINE FIELD IF NOT EXISTS size ON note_file TYPE int;
    DEFINE INDEX IF NOT EXISTS note_file_note ON note_file FIELDS note;

    -- Chatbot relay (2026-07-23): a `thread` groups the turns, one
    -- `chatbot_message` is one turn. `user_id` rides on the message too so an
    -- ownership check needs no join. An assistant turn is born `pending` and
    -- is completed (or failed) by the task holding the AI-bridge stream —
    -- hence the BACKFILL sweep, since that task dies with the process.
    DEFINE TABLE IF NOT EXISTS chatbot_thread SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS user_id ON chatbot_thread TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS title ON chatbot_thread TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS created_at ON chatbot_thread TYPE int READONLY;
    DEFINE FIELD IF NOT EXISTS updated_at ON chatbot_thread TYPE int;
    DEFINE INDEX IF NOT EXISTS chatbot_thread_user_updated ON chatbot_thread FIELDS user_id, updated_at;

    DEFINE TABLE IF NOT EXISTS chatbot_message SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS thread_id ON chatbot_message TYPE record<chatbot_thread> READONLY;
    DEFINE FIELD IF NOT EXISTS user_id ON chatbot_message TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS role ON chatbot_message TYPE string READONLY;
    DEFINE FIELD IF NOT EXISTS content ON chatbot_message TYPE string;
    DEFINE FIELD IF NOT EXISTS status ON chatbot_message TYPE string;
    DEFINE FIELD IF NOT EXISTS truncated ON chatbot_message TYPE bool DEFAULT false;
    DEFINE FIELD IF NOT EXISTS error_code ON chatbot_message TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS created_at ON chatbot_message TYPE int READONLY;
    DEFINE FIELD IF NOT EXISTS completed_at ON chatbot_message TYPE option<int>;
    -- The claim (2026-07-27): which replica's claim loop owes this turn an
    -- answer, and when it said so. Absent = nobody has taken it yet; a claim
    -- older than CHATBOT_CLAIM_RECLAIM_SECS is a dead claimer and the turn is
    -- taken back (see `CHATBOT_CLAIM`).
    DEFINE FIELD IF NOT EXISTS claimed_by ON chatbot_message TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS claimed_at ON chatbot_message TYPE option<int>;
    DEFINE INDEX IF NOT EXISTS chatbot_message_thread_created ON chatbot_message FIELDS thread_id, created_at;
    DEFINE INDEX IF NOT EXISTS chatbot_message_user ON chatbot_message FIELDS user_id;
    -- The claim loop's sweep: every poll asks for pending rows by age.
    DEFINE INDEX IF NOT EXISTS chatbot_message_status_created ON chatbot_message FIELDS status, created_at;

    -- Which AI workers are connected to *any* replica (2026-07-27). The GATE
    -- and nothing else: a row only says a socket existed somewhere at
    -- `seen_at`, so it may spare a user a 503 but must never pick who answers
    -- — that is the in-process registry's call. Heartbeated by the replica
    -- owning the connection; a stale row is ignored by the read and swept.
    DEFINE TABLE IF NOT EXISTS ai_worker SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS service ON ai_worker TYPE string;
    DEFINE FIELD IF NOT EXISTS capabilities ON ai_worker TYPE array<string>;
    DEFINE FIELD IF NOT EXISTS seen_at ON ai_worker TYPE int;

    DEFINE TABLE IF NOT EXISTS event SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS creator ON event TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS title ON event TYPE string;
    DEFINE FIELD IF NOT EXISTS description ON event TYPE string;
    DEFINE FIELD IF NOT EXISTS audience ON event TYPE object;
    DEFINE FIELD IF NOT EXISTS audience.kind ON event TYPE string;
    DEFINE FIELD IF NOT EXISTS audience.role ON event TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS audience.course ON event TYPE option<record<course>>;
    DEFINE FIELD IF NOT EXISTS audience.capacity ON event TYPE option<int>;
    REMOVE FIELD IF EXISTS audience.users ON TABLE event;
    DEFINE FIELD IF NOT EXISTS starts_at ON event TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS ends_at ON event TYPE option<int>;

    DEFINE TABLE IF NOT EXISTS attendance SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS event ON attendance TYPE record<event>;
    DEFINE FIELD IF NOT EXISTS user ON attendance TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS status ON attendance TYPE string;
    DEFINE FIELD IF NOT EXISTS marked_by ON attendance TYPE record<user>;
    DEFINE INDEX IF NOT EXISTS attendance_event_user ON attendance FIELDS event, user UNIQUE;
    DEFINE INDEX IF NOT EXISTS attendance_user ON attendance FIELDS user;

    DEFINE TABLE IF NOT EXISTS registration SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS event ON registration TYPE record<event>;
    DEFINE FIELD IF NOT EXISTS user ON registration TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS registered_by ON registration TYPE record<user>;
    DEFINE INDEX IF NOT EXISTS registration_event_user ON registration FIELDS event, user UNIQUE;
    DEFINE INDEX IF NOT EXISTS registration_event ON registration FIELDS event;

    DEFINE TABLE IF NOT EXISTS term SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS name ON term TYPE string;
    DEFINE FIELD IF NOT EXISTS starts_at ON term TYPE int;
    DEFINE FIELD IF NOT EXISTS ends_at ON term TYPE int;

    DEFINE TABLE IF NOT EXISTS course SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS creator ON course TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS teachers ON course TYPE array<record<user>> DEFAULT [];
    DEFINE FIELD IF NOT EXISTS teachers[*] ON course TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS title ON course TYPE string;
    DEFINE FIELD IF NOT EXISTS description ON course TYPE string;
    DEFINE FIELD IF NOT EXISTS kind ON course TYPE string DEFAULT 'course';
    DEFINE FIELD IF NOT EXISTS term ON course TYPE option<record<term>>;
    DEFINE FIELD IF NOT EXISTS capacity ON course TYPE option<int>;

    DEFINE TABLE IF NOT EXISTS subject SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS course ON subject TYPE record<course>;
    DEFINE FIELD IF NOT EXISTS name ON subject TYPE string;
    DEFINE FIELD IF NOT EXISTS description ON subject TYPE string;
    DEFINE INDEX IF NOT EXISTS subject_course ON subject FIELDS course;

    DEFINE TABLE IF NOT EXISTS enrollment SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS course ON enrollment TYPE record<course>;
    DEFINE FIELD IF NOT EXISTS user ON enrollment TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS enrolled_by ON enrollment TYPE record<user>;
    DEFINE INDEX IF NOT EXISTS enrollment_course_user ON enrollment FIELDS course, user UNIQUE;
    DEFINE INDEX IF NOT EXISTS enrollment_user ON enrollment FIELDS user;

    DEFINE TABLE IF NOT EXISTS parent_link SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS parent ON parent_link TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS student ON parent_link TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS linked_by ON parent_link TYPE record<user>;
    DEFINE INDEX IF NOT EXISTS parent_link_parent_student ON parent_link FIELDS parent, student UNIQUE;
    DEFINE INDEX IF NOT EXISTS parent_link_parent ON parent_link FIELDS parent;
    DEFINE INDEX IF NOT EXISTS parent_link_student ON parent_link FIELDS student;

    DEFINE TABLE IF NOT EXISTS course_session SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS course ON course_session TYPE record<course>;
    DEFINE FIELD IF NOT EXISTS teacher ON course_session TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS topic ON course_session TYPE string;
    DEFINE FIELD IF NOT EXISTS starts_at ON course_session TYPE int;
    DEFINE FIELD IF NOT EXISTS ends_at ON course_session TYPE option<int>;
    DEFINE INDEX IF NOT EXISTS course_session_course ON course_session FIELDS course;

    DEFINE TABLE IF NOT EXISTS session_attendance SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS session ON session_attendance TYPE record<course_session>;
    DEFINE FIELD IF NOT EXISTS course ON session_attendance TYPE record<course>;
    DEFINE FIELD IF NOT EXISTS user ON session_attendance TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS status ON session_attendance TYPE string;
    DEFINE FIELD IF NOT EXISTS marked_by ON session_attendance TYPE record<user>;
    DEFINE INDEX IF NOT EXISTS session_attendance_session_user ON session_attendance FIELDS session, user UNIQUE;
    DEFINE INDEX IF NOT EXISTS session_attendance_session ON session_attendance FIELDS session;
    DEFINE INDEX IF NOT EXISTS session_attendance_user ON session_attendance FIELDS user;
    DEFINE INDEX IF NOT EXISTS session_attendance_course ON session_attendance FIELDS course;

    DEFINE TABLE IF NOT EXISTS work_entry SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS user ON work_entry TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS check_in ON work_entry TYPE int;
    DEFINE FIELD IF NOT EXISTS check_out ON work_entry TYPE option<int>;
    DEFINE INDEX IF NOT EXISTS work_entry_user ON work_entry FIELDS user;

    DEFINE TABLE IF NOT EXISTS pomodoro_session SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS user ON pomodoro_session TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS started_at ON pomodoro_session TYPE int;
    DEFINE FIELD IF NOT EXISTS finished_at ON pomodoro_session TYPE option<int>;
    DEFINE INDEX IF NOT EXISTS pomodoro_session_user ON pomodoro_session FIELDS user;

    DEFINE TABLE IF NOT EXISTS exam SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS creator ON exam TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS course ON exam TYPE record<course>;
    DEFINE FIELD IF NOT EXISTS title ON exam TYPE string;
    DEFINE FIELD IF NOT EXISTS description ON exam TYPE string;
    DEFINE FIELD IF NOT EXISTS kind ON exam TYPE string;
    DEFINE FIELD IF NOT EXISTS mode ON exam TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS starts_at ON exam TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS ends_at ON exam TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS duration_ms ON exam TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS max_attempts ON exam TYPE int DEFAULT 1;
    DEFINE FIELD IF NOT EXISTS allow_rejoin ON exam TYPE bool DEFAULT true;
    DEFINE FIELD IF NOT EXISTS allow_review ON exam TYPE bool DEFAULT false;
    DEFINE FIELD IF NOT EXISTS draft ON exam TYPE bool DEFAULT false;
    DEFINE INDEX IF NOT EXISTS exam_course ON exam FIELDS course;

    DEFINE TABLE IF NOT EXISTS exam_attempt SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS exam ON exam_attempt TYPE record<exam>;
    DEFINE FIELD IF NOT EXISTS user ON exam_attempt TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS seq ON exam_attempt TYPE int DEFAULT 1;
    DEFINE FIELD IF NOT EXISTS started_at ON exam_attempt TYPE int;
    DEFINE FIELD IF NOT EXISTS finished_at ON exam_attempt TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS left_at ON exam_attempt TYPE option<int>;
    DEFINE INDEX IF NOT EXISTS exam_attempt_exam_user_seq ON exam_attempt FIELDS exam, user, seq UNIQUE;
    DEFINE INDEX IF NOT EXISTS exam_attempt_exam ON exam_attempt FIELDS exam;

    DEFINE TABLE IF NOT EXISTS exam_question SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS exam ON exam_question TYPE record<exam>;
    DEFINE FIELD IF NOT EXISTS text ON exam_question TYPE string;
    DEFINE FIELD IF NOT EXISTS kind ON exam_question TYPE string;
    DEFINE FIELD IF NOT EXISTS points ON exam_question TYPE int;
    -- OVERWRITE, not IF NOT EXISTS: these columns changed type when choices
    -- gained stable ids (`array<string>`/`int` -> `array<object>`/`string`),
    -- and IF NOT EXISTS would leave an existing database on the old types.
    DEFINE FIELD OVERWRITE choices ON exam_question TYPE option<array<object>>;
    -- SCHEMAFULL rejects any nested key it wasn't told about, so the choice
    -- object's own fields are declared too (same shape as `settings.exam_kinds`).
    DEFINE FIELD OVERWRITE choices.*.id ON exam_question TYPE string;
    DEFINE FIELD OVERWRITE choices.*.text ON exam_question TYPE string;
    DEFINE FIELD OVERWRITE correct ON exam_question TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS subject ON exam_question TYPE record<subject>;
    -- Provenance, one column per direction: `from_bank` is the template this
    -- question was inserted from, `banked_as` the template most recently minted
    -- by saving it into the bank. They replace the single `source_bank`, which
    -- both directions wrote — so a question inserted from the bank read as
    -- already-saved. OVERWRITE (not IF NOT EXISTS): the old column has to
    -- actually go on an existing database, or SCHEMAFULL keeps accepting it
    -- while the struct no longer writes it.
    DEFINE FIELD OVERWRITE from_bank ON exam_question TYPE option<record<bank_question>>;
    DEFINE FIELD OVERWRITE banked_as ON exam_question TYPE option<record<bank_question>>;
    REMOVE FIELD IF EXISTS source_bank ON TABLE exam_question;
    DEFINE INDEX IF NOT EXISTS exam_question_exam ON exam_question FIELDS exam;
    DEFINE INDEX IF NOT EXISTS exam_question_subject ON exam_question FIELDS subject;

    DEFINE TABLE IF NOT EXISTS question_image SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS exam ON question_image TYPE record<exam>;
    DEFINE FIELD IF NOT EXISTS question ON question_image TYPE record<exam_question>;
    -- OVERWRITE: `slot` is now the choice's id, not its position.
    DEFINE FIELD OVERWRITE slot ON question_image TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS file ON question_image TYPE string;
    DEFINE FIELD IF NOT EXISTS content_type ON question_image TYPE string;
    DEFINE FIELD IF NOT EXISTS size ON question_image TYPE int;
    DEFINE INDEX IF NOT EXISTS question_image_exam ON question_image FIELDS exam;
    DEFINE INDEX IF NOT EXISTS question_image_question ON question_image FIELDS question;

    DEFINE TABLE IF NOT EXISTS bank_question SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS owner ON bank_question TYPE record<user>;
    -- OVERWRITE, not IF NOT EXISTS: the field started life as a required
    -- `record<subject>` and has to actually change type on an existing database,
    -- otherwise the subject-delete cascade (`SET subject = NONE`) is rejected.
    DEFINE FIELD OVERWRITE subject ON bank_question TYPE option<record<subject>>;
    DEFINE FIELD IF NOT EXISTS text ON bank_question TYPE string;
    DEFINE FIELD IF NOT EXISTS kind ON bank_question TYPE string;
    DEFINE FIELD IF NOT EXISTS points ON bank_question TYPE int;
    -- OVERWRITE, not IF NOT EXISTS: these columns changed type when choices
    -- gained stable ids (`array<string>`/`int` -> `array<object>`/`string`),
    -- and IF NOT EXISTS would leave an existing database on the old types.
    DEFINE FIELD OVERWRITE choices ON bank_question TYPE option<array<object>>;
    -- SCHEMAFULL rejects any nested key it wasn't told about, so the choice
    -- object's own fields are declared too (same shape as `settings.exam_kinds`).
    DEFINE FIELD OVERWRITE choices.*.id ON bank_question TYPE string;
    DEFINE FIELD OVERWRITE choices.*.text ON bank_question TYPE string;
    DEFINE FIELD OVERWRITE correct ON bank_question TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS source_exam ON bank_question TYPE option<record<exam>>;
    -- 'private' | 'school'. DEFAULT so a row that predates the field (or any
    -- partial write) can never come out published: the answer key stays the
    -- owner's until they publish it.
    DEFINE FIELD IF NOT EXISTS visibility ON bank_question TYPE string DEFAULT 'private';
    DEFINE FIELD IF NOT EXISTS created_at ON bank_question TYPE int READONLY;
    DEFINE INDEX IF NOT EXISTS bank_question_owner ON bank_question FIELDS owner;
    DEFINE INDEX IF NOT EXISTS bank_question_subject ON bank_question FIELDS subject;

    DEFINE TABLE IF NOT EXISTS bank_question_image SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS bank_question ON bank_question_image TYPE record<bank_question>;
    -- OVERWRITE: `slot` is now the choice's id, not its position.
    DEFINE FIELD OVERWRITE slot ON bank_question_image TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS file ON bank_question_image TYPE string;
    DEFINE FIELD IF NOT EXISTS content_type ON bank_question_image TYPE string;
    DEFINE FIELD IF NOT EXISTS size ON bank_question_image TYPE int;
    DEFINE INDEX IF NOT EXISTS bank_question_image_question ON bank_question_image FIELDS bank_question;

    DEFINE TABLE IF NOT EXISTS exam_answer SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS exam ON exam_answer TYPE record<exam>;
    DEFINE FIELD IF NOT EXISTS question ON exam_answer TYPE record<exam_question>;
    DEFINE FIELD IF NOT EXISTS user ON exam_answer TYPE record<user>;
    -- OVERWRITE: `selected` now names the picked option by id.
    DEFINE FIELD OVERWRITE selected ON exam_answer TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS text ON exam_answer TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS updated_at ON exam_answer TYPE int;
    -- `seq` numbers the sitting an answer belongs to (1, 2, …), mirroring
    -- exam_attempt. It rides the record key too, so a retake's answer is a new
    -- row, not an overwrite of the prior sitting.
    DEFINE FIELD IF NOT EXISTS seq ON exam_answer TYPE int DEFAULT 1;
    -- Retire the pre-history unique index: (question, user) is no longer unique
    -- once a student re-sits; (question, user, seq) is.
    REMOVE INDEX IF EXISTS exam_answer_question_user ON exam_answer;
    DEFINE INDEX IF NOT EXISTS exam_answer_question_user_seq ON exam_answer FIELDS question, user, seq UNIQUE;
    DEFINE INDEX IF NOT EXISTS exam_answer_exam_user ON exam_answer FIELDS exam, user;
    DEFINE INDEX IF NOT EXISTS exam_answer_exam ON exam_answer FIELDS exam;

    DEFINE TABLE IF NOT EXISTS answer_image SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS exam ON answer_image TYPE record<exam>;
    DEFINE FIELD IF NOT EXISTS question ON answer_image TYPE record<exam_question>;
    DEFINE FIELD IF NOT EXISTS user ON answer_image TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS file ON answer_image TYPE string;
    DEFINE FIELD IF NOT EXISTS content_type ON answer_image TYPE string;
    DEFINE FIELD IF NOT EXISTS size ON answer_image TYPE int;
    -- `seq` numbers the sitting this drawing belongs to; it rides the record
    -- key so a retake's image never overwrites the prior sitting's.
    DEFINE FIELD IF NOT EXISTS seq ON answer_image TYPE int DEFAULT 1;
    DEFINE INDEX IF NOT EXISTS answer_image_exam ON answer_image FIELDS exam;
    DEFINE INDEX IF NOT EXISTS answer_image_user ON answer_image FIELDS user;

    DEFINE TABLE IF NOT EXISTS exam_result SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS exam ON exam_result TYPE record<exam>;
    DEFINE FIELD IF NOT EXISTS user ON exam_result TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS mark ON exam_result TYPE int;
    DEFINE FIELD IF NOT EXISTS graded_by ON exam_result TYPE record<user>;
    -- `seq` numbers the sitting a mark belongs to; a retake earns its own mark
    -- row, and the latest seq is the student's standing.
    DEFINE FIELD IF NOT EXISTS seq ON exam_result TYPE int DEFAULT 1;
    REMOVE INDEX IF EXISTS exam_result_exam_user ON exam_result;
    DEFINE INDEX IF NOT EXISTS exam_result_exam_user_seq ON exam_result FIELDS exam, user, seq UNIQUE;

    DEFINE TABLE IF NOT EXISTS pool_question SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS asker ON pool_question TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS title ON pool_question TYPE string;
    DEFINE FIELD IF NOT EXISTS body ON pool_question TYPE string;
    DEFINE FIELD IF NOT EXISTS status ON pool_question TYPE string DEFAULT 'pending';
    DEFINE FIELD IF NOT EXISTS asked_at ON pool_question TYPE int;
    DEFINE FIELD IF NOT EXISTS approved_by ON pool_question TYPE option<record<user>>;
    DEFINE FIELD IF NOT EXISTS image_file ON pool_question TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS image_content_type ON pool_question TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS image_size ON pool_question TYPE option<int>;
    DEFINE INDEX IF NOT EXISTS pool_question_status ON pool_question FIELDS status;
    DEFINE INDEX IF NOT EXISTS pool_question_asker ON pool_question FIELDS asker;

    DEFINE TABLE IF NOT EXISTS solution SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS question ON solution TYPE record<pool_question>;
    DEFINE FIELD IF NOT EXISTS author ON solution TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS body ON solution TYPE string;
    DEFINE FIELD IF NOT EXISTS offered_at ON solution TYPE int;
    DEFINE FIELD IF NOT EXISTS image_file ON solution TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS image_content_type ON solution TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS image_size ON solution TYPE option<int>;
    DEFINE INDEX IF NOT EXISTS solution_question ON solution FIELDS question;

    DEFINE TABLE IF NOT EXISTS settings SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS exam_kinds ON settings TYPE array<object>;
    DEFINE FIELD IF NOT EXISTS exam_kinds.*.name ON settings TYPE string;
    DEFINE FIELD IF NOT EXISTS exam_kinds.*.weight ON settings TYPE int;
    DEFINE FIELD IF NOT EXISTS attendance_statuses ON settings TYPE array<string>;
    DEFINE FIELD IF NOT EXISTS grade_bands ON settings TYPE array<object>;
    DEFINE FIELD IF NOT EXISTS grade_bands.*.min ON settings TYPE int;
    DEFINE FIELD IF NOT EXISTS grade_bands.*.label ON settings TYPE string;
    DEFINE FIELD IF NOT EXISTS max_file_bytes ON settings TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS chatbot_history_turns ON settings TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS max_chatbot_threads ON settings TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS max_chatbot_message_len ON settings TYPE option<int>;
    -- Food program (2026-07-26). `meal_slots` mirrors the `exam_kinds` shape —
    -- a named list a menu snapshots from — and `dietary_tags` mirrors
    -- `attendance_statuses`. `meal_cancel_cutoff_minutes` is ONE knob for both
    -- the booking and the cancel deadline; absent means no cutoff at all. All
    -- three are `option<>`, exactly like `max_file_bytes`: NOT `DEFAULT []`.
    -- `DEFAULT` fires on create only, and `PATCH /settings` writes the whole
    -- row with `UPDATE ... CONTENT`, so a required-with-default field a writer
    -- omits coerces to NONE and fails every settings write until the domain
    -- struct carries it. `option<>` also means the existing singleton reads
    -- fine, which is why the food program needs no BACKFILL anywhere.
    DEFINE FIELD IF NOT EXISTS meal_slots ON settings TYPE option<array<object>>;
    DEFINE FIELD IF NOT EXISTS meal_slots.*.name ON settings TYPE string;
    -- Minutes past midnight UTC at which the slot is served (2026-07-26).
    -- `option<int>` for the same reason the list itself is: slots written
    -- before the field existed simply have no key, and read back as NONE ->
    -- the booking cutoff falls back to midnight UTC, its old meaning. No
    -- BACKFILL, and no DEFAULT — see the note above.
    DEFINE FIELD IF NOT EXISTS meal_slots.*.serving_minute ON settings TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS dietary_tags ON settings TYPE option<array<string>>;
    DEFINE FIELD IF NOT EXISTS meal_cancel_cutoff_minutes ON settings TYPE option<int>;

    -- Homework (greenfield, 2026-07-22): a teacher assigns per course, students
    -- submit files + optional text, a teacher grades a status + optional mark.
    -- `course`/`created_by`/`created_at` are READONLY (a homework never moves
    -- course, and stamps never rewrite); `subject` is NOT — it is re-taggable
    -- via PATCH. `assigned` NONE/empty means the whole course. No BACKFILL: the
    -- tables are new, so the whole migration stays additive and idempotent.
    DEFINE TABLE IF NOT EXISTS homework SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS course ON homework TYPE record<course> READONLY;
    DEFINE FIELD IF NOT EXISTS subject ON homework TYPE record<subject>;
    DEFINE FIELD IF NOT EXISTS title ON homework TYPE string;
    DEFINE FIELD IF NOT EXISTS description ON homework TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS due_at ON homework TYPE int;
    DEFINE FIELD IF NOT EXISTS assigned ON homework TYPE option<array<record<user>>>;
    DEFINE FIELD IF NOT EXISTS assigned[*] ON homework TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS created_by ON homework TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS created_at ON homework TYPE int READONLY;
    DEFINE INDEX IF NOT EXISTS homework_course ON homework FIELDS course;

    DEFINE TABLE IF NOT EXISTS homework_submission SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS homework ON homework_submission TYPE record<homework> READONLY;
    DEFINE FIELD IF NOT EXISTS user ON homework_submission TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS text ON homework_submission TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS submitted_at ON homework_submission TYPE int READONLY;
    DEFINE FIELD IF NOT EXISTS updated_at ON homework_submission TYPE int;
    DEFINE INDEX IF NOT EXISTS homework_submission_homework ON homework_submission FIELDS homework;

    DEFINE TABLE IF NOT EXISTS homework_file SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS submission ON homework_file TYPE record<homework_submission> READONLY;
    DEFINE FIELD IF NOT EXISTS name ON homework_file TYPE string;
    DEFINE FIELD IF NOT EXISTS content_type ON homework_file TYPE string;
    DEFINE FIELD IF NOT EXISTS size ON homework_file TYPE int;
    DEFINE FIELD IF NOT EXISTS file ON homework_file TYPE string READONLY;
    DEFINE FIELD IF NOT EXISTS created_at ON homework_file TYPE int READONLY;
    DEFINE INDEX IF NOT EXISTS homework_file_submission ON homework_file FIELDS submission;

    DEFINE TABLE IF NOT EXISTS homework_result SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS homework ON homework_result TYPE record<homework> READONLY;
    DEFINE FIELD IF NOT EXISTS user ON homework_result TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS status ON homework_result TYPE string;
    DEFINE FIELD IF NOT EXISTS mark ON homework_result TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS graded_by ON homework_result TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS created_at ON homework_result TYPE int;
    DEFINE INDEX IF NOT EXISTS homework_result_homework ON homework_result FIELDS homework;

    -- Appointments (greenfield, 2026-07-23): a teacher publishes availability
    -- slots, a student or parent books one. `series` groups the occurrences a
    -- weekly repeat expanded into, so one cancel deletes one row and a series
    -- delete finds the rest. A booking's live/dead state is the `status`
    -- string; occupancy is derived from it under the appointment lock rather
    -- than stored, so a rejected or cancelled booking frees its slot again.
    -- The `proposed_*` fields carry a teacher's counter-proposal on the same
    -- row until the requester accepts. No BACKFILL: both tables are new.
    DEFINE TABLE IF NOT EXISTS appointment_slot SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS teacher ON appointment_slot TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS starts_at ON appointment_slot TYPE int;
    DEFINE FIELD IF NOT EXISTS ends_at ON appointment_slot TYPE int;
    DEFINE FIELD IF NOT EXISTS note ON appointment_slot TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS series ON appointment_slot TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS created_at ON appointment_slot TYPE int;
    DEFINE INDEX IF NOT EXISTS appointment_slot_teacher_starts ON appointment_slot FIELDS teacher, starts_at;
    DEFINE INDEX IF NOT EXISTS appointment_slot_series ON appointment_slot FIELDS series;

    DEFINE TABLE IF NOT EXISTS appointment SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS slot ON appointment TYPE record<appointment_slot>;
    DEFINE FIELD IF NOT EXISTS requester ON appointment TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS status ON appointment TYPE string DEFAULT 'pending';
    DEFINE FIELD IF NOT EXISTS reason ON appointment TYPE string;
    DEFINE FIELD IF NOT EXISTS proposed_starts_at ON appointment TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS proposed_ends_at ON appointment TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS proposed_by ON appointment TYPE option<record<user>>;
    DEFINE FIELD IF NOT EXISTS decided_by ON appointment TYPE option<record<user>>;
    DEFINE FIELD IF NOT EXISTS cancelled_by ON appointment TYPE option<record<user>>;
    DEFINE FIELD IF NOT EXISTS cancel_reason ON appointment TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS reject_reason ON appointment TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS created_at ON appointment TYPE int;
    DEFINE INDEX IF NOT EXISTS appointment_slot_ref ON appointment FIELDS slot;
    DEFINE INDEX IF NOT EXISTS appointment_requester ON appointment FIELDS requester;
    DEFINE INDEX IF NOT EXISTS appointment_status ON appointment FIELDS status;

    -- Food program (greenfield, 2026-07-26): a published menu per day+slot,
    -- dishes on it, a dietary profile per student, bookings against a menu's
    -- capacity, who actually ate, and the money that moved. `date` is a
    -- calendar day as `YYYY-MM-DD` text, not a timestamp: the unique index is
    -- an equality test and a midnight-in-millis day is only unique for one
    -- timezone. `slot` is snapshotted text, not a link, so retiring a slot in
    -- settings never rewrites a past menu. Stamps stay `int` millis like the
    -- rest of the schema. No BACKFILL: all six tables are new.
    DEFINE TABLE IF NOT EXISTS menu SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS date ON menu TYPE string READONLY;
    DEFINE FIELD IF NOT EXISTS slot ON menu TYPE string READONLY;
    DEFINE FIELD IF NOT EXISTS capacity ON menu TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS created_by ON menu TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS created_at ON menu TYPE int READONLY;
    DEFINE INDEX IF NOT EXISTS menu_date_slot ON menu FIELDS date, slot UNIQUE;

    DEFINE TABLE IF NOT EXISTS menu_dish SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS menu ON menu_dish TYPE record<menu> READONLY;
    DEFINE FIELD IF NOT EXISTS name ON menu_dish TYPE string;
    DEFINE FIELD IF NOT EXISTS description ON menu_dish TYPE option<string>;
    -- Money is minor units (kuruş) as an integer, everywhere. Never decimal.
    DEFINE FIELD IF NOT EXISTS price_minor ON menu_dish TYPE int;
    DEFINE FIELD IF NOT EXISTS tags ON menu_dish TYPE array<string> DEFAULT [];
    DEFINE FIELD IF NOT EXISTS tags[*] ON menu_dish TYPE string;
    DEFINE FIELD IF NOT EXISTS created_at ON menu_dish TYPE int READONLY;
    DEFINE INDEX IF NOT EXISTS menu_dish_menu ON menu_dish FIELDS menu;

    DEFINE TABLE IF NOT EXISTS dietary_profile SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS student ON dietary_profile TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS tags ON dietary_profile TYPE array<string> DEFAULT [];
    DEFINE FIELD IF NOT EXISTS tags[*] ON dietary_profile TYPE string;
    DEFINE FIELD IF NOT EXISTS note ON dietary_profile TYPE option<string>;
    DEFINE FIELD IF NOT EXISTS updated_by ON dietary_profile TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS updated_at ON dietary_profile TYPE int;
    DEFINE INDEX IF NOT EXISTS dietary_profile_student ON dietary_profile FIELDS student UNIQUE;

    -- A cancel flips `status` and stamps `cancelled_at`; the row stays so the
    -- freed seat is still auditable against the ledger line it charged.
    DEFINE TABLE IF NOT EXISTS meal_booking SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS menu ON meal_booking TYPE record<menu> READONLY;
    DEFINE FIELD IF NOT EXISTS student ON meal_booking TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS booked_by ON meal_booking TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS status ON meal_booking TYPE string DEFAULT 'booked';
    -- How many times this seat has been taken, and what it cost when the
    -- *current* attempt took it (NONE = the menu was free then). Together they
    -- key the attempt's ledger lines, which is what makes billing idempotent
    -- by identity rather than by scanning for an outstanding charge.
    DEFINE FIELD IF NOT EXISTS attempt ON meal_booking TYPE int DEFAULT 1;
    DEFINE FIELD IF NOT EXISTS price_minor ON meal_booking TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS cancelled_at ON meal_booking TYPE option<int>;
    DEFINE FIELD IF NOT EXISTS created_at ON meal_booking TYPE int READONLY;
    DEFINE INDEX IF NOT EXISTS meal_booking_menu ON meal_booking FIELDS menu;
    DEFINE INDEX IF NOT EXISTS meal_booking_student ON meal_booking FIELDS student;

    DEFINE TABLE IF NOT EXISTS meal_attendance SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS menu ON meal_attendance TYPE record<menu> READONLY;
    DEFINE FIELD IF NOT EXISTS student ON meal_attendance TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS status ON meal_attendance TYPE string;
    DEFINE FIELD IF NOT EXISTS marked_by ON meal_attendance TYPE record<user>;
    DEFINE FIELD IF NOT EXISTS marked_at ON meal_attendance TYPE int;
    DEFINE INDEX IF NOT EXISTS meal_attendance_menu_student ON meal_attendance FIELDS menu, student UNIQUE;

    -- APPEND-ONLY by design: a mistake is corrected with an opposing
    -- `reversal` line, never by editing or deleting one. Hence every field is
    -- READONLY. `source` is untyped on purpose — a charge points at the
    -- booking that caused it, a reversal at the line it undoes.
    DEFINE TABLE IF NOT EXISTS meal_ledger SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS student ON meal_ledger TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS kind ON meal_ledger TYPE string READONLY;
    DEFINE FIELD IF NOT EXISTS amount_minor ON meal_ledger TYPE int READONLY;
    DEFINE FIELD IF NOT EXISTS source ON meal_ledger TYPE option<record> READONLY;
    DEFINE FIELD IF NOT EXISTS method ON meal_ledger TYPE option<string> READONLY;
    DEFINE FIELD IF NOT EXISTS note ON meal_ledger TYPE option<string> READONLY;
    DEFINE FIELD IF NOT EXISTS recorded_by ON meal_ledger TYPE record<user> READONLY;
    DEFINE FIELD IF NOT EXISTS created_at ON meal_ledger TYPE int READONLY;
    DEFINE INDEX IF NOT EXISTS meal_ledger_student ON meal_ledger FIELDS student;
";

/// Data backfills for rows written by older binaries. Runs *after* (and apart
/// from) the DDL batch so every statement sees the freshly defined schema —
/// see the `MIGRATION` doc for why sharing the batch corrupts the writes.
/// Idempotent: each backfill's `WHERE` only matches unconverted rows.
pub const BACKFILL: &str = "
    UPDATE user SET role = 'student' WHERE role = NONE;

    UPDATE course SET kind = 'course' WHERE kind = NONE;

    -- Courses predate assignable teachers (2026-07-21): every existing course
    -- was run by its creator alone, so it starts with nobody else assigned.
    UPDATE course SET teachers = [] WHERE teachers = NONE;

    -- Exams predate the draft flag (2026-07-19): everything already out there
    -- was live for its course, so it stays published.
    UPDATE exam SET draft = false WHERE draft = NONE;

    -- Exams predate the review toggle (2026-07-24): default it closed (opt-in).
    UPDATE exam SET allow_review = false WHERE allow_review = NONE;

    UPDATE event SET audience = { kind: 'school' } WHERE audience = NONE OR audience = {};

    -- Questions written before subjects existed (2026-07-17) are destroyed, not
    -- backfilled: a subject is mandatory and there is nothing truthful to
    -- assign. Their answers go first so no answer row outlives its question.
    DELETE exam_answer WHERE question IN (SELECT VALUE id FROM exam_question WHERE subject = NONE);
    DELETE exam_question WHERE subject = NONE;

    -- The hand-picked `users` audience retired into `registration` (2026-07-16):
    -- each listed user becomes a signup row credited to the event's creator, and
    -- the event becomes an uncapped registration list. Runs once — converted
    -- events no longer match the `kind = 'users'` filter. (`?? []`: an empty
    -- statement-subquery evaluates to NONE, which FOR refuses to iterate.)
    FOR $ev IN ((SELECT id, creator, audience FROM event WHERE audience.kind = 'users') ?? []) {
        FOR $usr IN ($ev.audience.users ?? []) {
            UPSERT type::record('registration', string::concat(record::id($ev.id), '_', record::id($usr))) CONTENT {
                event: $ev.id,
                user: $usr,
                registered_by: $ev.creator,
            };
        };
        UPDATE $ev.id SET audience = { kind: 'registration' };
    };

    -- Turns written before the clipped-answer flag existed (2026-07-23): the
    -- clip was silent then, so nothing can be recovered — they read as whole,
    -- which is what they were presented as all along.
    UPDATE chatbot_message SET truncated = false WHERE truncated = NONE;

    -- An assistant turn is answered by a claim loop in some replica's process,
    -- so a restart can leave its row `pending` with nobody left to complete it.
    -- Only rows past the stale horizon ($stale_ms, from
    -- `CHATBOT_PENDING_STALE_SECS`) are certainly abandoned though: a young one
    -- may still be being answered on the other side of the AI bridge, and a
    -- deploy must not shoot down a turn dispatched seconds ago. Nothing is lost
    -- by waiting — a reader already presents an over-age `pending` row as
    -- failed, and the next boot sweeps for real whatever crossed the horizon in
    -- the meantime.
    --
    -- Claim-aware since 2026-07-27, because the sweep is no longer school-wide
    -- truth: under N replicas the row may belong to a *live peer's* claim loop,
    -- and failing it here would shoot down another process's in-flight turn.
    -- So a claim restamped within the window is left alone. Both disjuncts
    -- matter — `claimed_by = NONE` alone would strand every row whose claimer
    -- died, which is the one case the sweep exists for.
    --
    -- The claim age is measured against $stale_ms rather than
    -- CHATBOT_CLAIM_RECLAIM_SECS only because the boot binds one horizon (see
    -- `database::migration_binds`); erring long is the safe direction — the
    -- cost is that a row claimed just before it aged out waits one more boot
    -- for its durable stamp, while readers have shown it failed all along.
    UPDATE chatbot_message SET status = 'failed', error_code = 'interrupted',
        completed_at = time::unix(time::now()) * 1000
        WHERE status = 'pending' AND created_at < time::unix(time::now()) * 1000 - $stale_ms
            AND (claimed_by = NONE
                 OR claimed_at < time::unix(time::now()) * 1000 - $stale_ms);

    -- Promotion out of student now deletes the user's enrollments (2026-07-18);
    -- this sweeps rows promoted before that fix. A deleted user reads as
    -- `user.role = NONE`, which is also != 'student' — those rows go too.
    DELETE enrollment WHERE user.role != 'student';

    -- Choices gained stable ids (2026-07-24): `choices` held bare strings and
    -- `correct`/`selected`/`slot` held the option's *position*. The DDL above
    -- retypes those columns, which leaves every old-shaped row readable but
    -- unwritable ('Expected `none | array<object>` but found `[..]`'), so this
    -- conversion is repair, not cosmetics.
    --
    -- Ids are minted in array order, so the id at position i *is* the id for
    -- old index i — that identity is what keeps a stored answer, and an option
    -- picture, pointing at the option the student actually saw. Hence the
    -- dependent rows are remapped inside the loop that mints `$new`, against
    -- that same array: once the ids are stored the positions are still
    -- recoverable, but nothing guarantees a later pass would look them up the
    -- same way. `WHERE choices[0] != NONE AND type::is_string(choices[0])`
    -- matches only unconverted rows, so a second boot re-mints nothing; an
    -- empty or absent `choices` needs no conversion at all. A question-level
    -- picture (`slot = NONE`) is not an index and is left alone.
    --
    -- Runs *before* every other backfill that writes these rows (the `seq`
    -- stamps below): a write coerces the whole record, so touching a row for
    -- any reason fails while its choices are still positional. For the same
    -- reason the answer's remap stamps `seq` itself — the two legacy gaps sit
    -- on the same row, and a write that fixes only one of them is rejected for
    -- the other. `seq ?? 1` leaves an already-numbered sitting alone (DEFAULT
    -- fills a CREATE, not an UPDATE of a row that predates the column).
    FOR $q IN ((SELECT id, choices, correct FROM exam_question
        WHERE choices[0] != NONE AND type::is_string(choices[0])) ?? []) {
        LET $new = $q.choices.map(|$c| { id: rand::ulid(), text: $c });
        UPDATE $q.id SET choices = $new,
            correct = IF $q.correct = NONE { NONE } ELSE { $new[$q.correct].id };
        FOR $img IN ((SELECT id, slot FROM question_image
            WHERE question = $q.id AND type::is_int(slot)) ?? []) {
            UPDATE $img.id SET slot = $new[$img.slot].id;
        };
        FOR $ans IN ((SELECT id, selected FROM exam_answer
            WHERE question = $q.id AND type::is_int(selected)) ?? []) {
            UPDATE $ans.id SET selected = $new[$ans.selected].id, seq = seq ?? 1;
        };
    };

    FOR $q IN ((SELECT id, choices, correct FROM bank_question
        WHERE choices[0] != NONE AND type::is_string(choices[0])) ?? []) {
        LET $new = $q.choices.map(|$c| { id: rand::ulid(), text: $c });
        UPDATE $q.id SET choices = $new,
            correct = IF $q.correct = NONE { NONE } ELSE { $new[$q.correct].id };
        FOR $img IN ((SELECT id, slot FROM bank_question_image
            WHERE bank_question = $q.id AND type::is_int(slot)) ?? []) {
            UPDATE $img.id SET slot = $new[$img.slot].id;
        };
    };

    -- Per-attempt history (2026-07-24): answers, drawings, and marks written
    -- before retakes stopped wiping belong to the student's first sitting.
    -- They already use the bare (seq==1) record key, so only the denormalized
    -- `seq` field needs stamping.
    UPDATE exam_answer SET seq = 1 WHERE seq = NONE;
    UPDATE answer_image SET seq = 1 WHERE seq = NONE;
    UPDATE exam_result SET seq = 1 WHERE seq = NONE;
";

/// The migration batches, in the order a boot applies them — and the *only*
/// list of them. [`crate::database::migrate`] runs exactly these and
/// [`crate::database::migration_fingerprint`] hashes exactly these, so a fourth
/// batch cannot be executed without changing the fingerprint that decides
/// whether a peer's schema is ours. They stay three separate queries: statements
/// in one batch see the schema as it stood when the batch started (see
/// `MIGRATION`).
pub const MIGRATION_BATCHES: [&str; 3] = [PRE_REPAIR, MIGRATION, BACKFILL];
