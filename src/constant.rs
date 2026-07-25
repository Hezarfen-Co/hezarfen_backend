//! Validation limits, in one place.

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

/// How long a dialling AI service has to complete its `Hello`/`Welcome`
/// exchange before the connection is dropped. Short: the handshake is one
/// frame each way, and an unauthenticated connection must not be able to
/// occupy the listener indefinitely.
pub const AI_HANDSHAKE_TIMEOUT_SECS: u64 = 10;
