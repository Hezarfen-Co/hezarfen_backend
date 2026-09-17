//! Validation limits, in one place.

/// A school slug's length bounds — the name in front of the dot in the session
/// cookie, which is also the school's database name and its blob directory
/// (see [`crate::tenant::Slug`]).
pub const MIN_SLUG_LEN: usize = 2;
pub const MAX_SLUG_LEN: usize = 32;

/// A school's display name — free text, unlike its slug.
pub const MAX_SCHOOL_NAME_LEN: usize = 120;

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

/// Profile fields: the self-chosen name a user is shown under, and the free
/// text under it.
pub const MAX_DISPLAY_NAME_LEN: usize = 50;
pub const MAX_BIO_LEN: usize = 500;

/// How many course and class references one profile read embeds. A cap on the
/// response, not on membership — the full lists stay at `/courses/me` and
/// `/classes/me`.
pub const MAX_PROFILE_COURSES: usize = 20;
pub const MAX_PROFILE_CLASSES: usize = 5;

/// RFC 5321's practical upper bound for a full address.
pub const MAX_EMAIL_LEN: usize = 254;

/// Digit-count bounds for a phone number (E.164 allows at most 15).
pub const MIN_PHONE_DIGITS: usize = 7;
pub const MAX_PHONE_DIGITS: usize = 15;

pub const MAX_NOTE_TITLE_LEN: usize = 200;
pub const MAX_NOTE_CONTENT_LEN: usize = 10_000;

/// How many files one note may carry.
pub const MAX_NOTE_FILES: usize = 10;

/// How many files one course note may carry.
pub const MAX_COURSE_NOTE_FILES: usize = 10;

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

/// A class section (şube) is named like a course, but its optional `grade` is
/// a free-text label the school picks ("9", "10-A", "anaokulu") — a line of
/// display text, so it is bounded like the other short labels, not like a
/// description.
pub const MAX_CLASS_NAME_LEN: usize = 200;
pub const MAX_CLASS_GRADE_LEN: usize = 20;

/// How big one class may get, on each of its two axes. These are not comfort
/// numbers: attaching a course to a class writes one enrollment per member and
/// adding a member writes one per attached course, both in a *single*
/// transaction, so each axis's counter is the bound on the other axis's write
/// loop — a class with no ceiling is an unbounded transaction anyone with the
/// manager role can trigger. A section is a homeroom (`MAX_HOMEWORK_ASSIGNED`
/// shares the 200) and its timetable is a school week, not a catalogue.
///
/// Which is also why each write is refused on the *other* axis: a class created
/// before these numbers existed can stand above one of them, and the axis being
/// added to having room says nothing about the loop's length — that is set by
/// the axis it multiplies against. So the pump refuses a member while the class
/// holds more than `MAX_CLASS_COURSES` courses and a course while it holds more
/// than `MAX_CLASS_MEMBERS` students, and only shrinking the overloaded side
/// clears it.
pub const MAX_CLASS_MEMBERS: i64 = 200;
pub const MAX_CLASS_COURSES: i64 = 50;

/// Weekly lesson hours of a class×course instance (D8's karne weight). Bounded
/// so a typo cannot make one instance dominate the year average.
pub const MIN_DERS_SAATI: i64 = 1;
pub const MAX_DERS_SAATI: i64 = 40;

/// The default grade-display bands: the Türkiye 5-point scale (85–100 = 5,
/// … 0–44 = 1). Schools edit the list via `PATCH /settings`; the karne verdict
/// reads the band labelled `"2"` as the pass boundary.
pub const DEFAULT_GRADE_BANDS: [(i64, &str); 5] =
    [(85, "5"), (70, "4"), (55, "3"), (45, "2"), (0, "1")];

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
/// is opt-in policy, not baked-in opinion. The defaults are the Turkish K12
/// classroom's own: yazılı, sözlü, uygulama.
pub const DEFAULT_EXAM_KINDS: [(&str, i64); 3] = [("yazili", 1), ("sozlu", 1), ("uygulama", 1)];

/// Bounds for the school-editable lists in settings (exam kinds, attendance
/// statuses): entry count and per-entry character length.
pub const MAX_SETTINGS_LIST_LEN: usize = 20;
pub const MAX_SETTINGS_ITEM_LEN: usize = 50;

/// Bounds for the grade-display bands in settings.
pub const MAX_GRADE_BANDS: usize = 20;
pub const MAX_GRADE_LABEL_LEN: usize = 20;

/// How many times a read-then-compare-and-set path (`PATCH /settings`, `PATCH
/// /bank/{bid}`, every appointment decision) re-reads and retries when a
/// concurrent edit lands between its snapshot and its save. A miss normally
/// re-validates into a `409`, so this only covers a write racing a legal
/// transition.
pub const CAS_UPDATE_RETRIES: usize = 3;

pub const MAX_TERM_NAME_LEN: usize = 100;

/// An academic year is named like a term ("2026-2027").
pub const MAX_ACADEMIC_YEAR_NAME_LEN: usize = 100;

/// The per-dönem devamsızlık limits a school may configure; a bound so a typo
/// cannot disable the rule or make it absurd.
pub const MAX_ABSENCE_DAYS: i64 = 365;

/// The timezone a school's day-bucketing falls back to when none is set, and
/// the one the default deployment runs in.
pub const DEFAULT_TIMEZONE: &str = "Europe/Istanbul";

/// The timezones a school may configure. A short allow-list on purpose: the
/// backend has no timezone database of its own, and day-bucketing needs the
/// offset to be trustworthy.
pub const TIMEZONES: [&str; 2] = ["Europe/Istanbul", "UTC"];

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

/// How many times one seat may be taken on one menu: the first booking plus
/// nine re-bookings after a cancel.
///
/// A cap on *storage*, not on indecision. Every cycle appends two permanent,
/// undeletable ledger lines — the charge and its reversal, both keyed to the
/// attempt — and nothing else bounded them: at the API's own request ceiling
/// one account could grow its statement by hundreds of thousands of rows a
/// day, which every later balance read then answers for. Ten is far past what
/// a family changing its mind about lunch needs, and a menu whose seat really
/// must move again is the canteen's to cancel.
pub const MAX_MEAL_BOOKING_ATTEMPTS: i64 = 10;

/// Ceiling on the settings knob that closes booking (and cancelling) ahead of
/// a meal — one week. The knob itself is optional: absent means no cutoff.
pub const MAX_MEAL_CANCEL_CUTOFF_MINUTES: i64 = 7 * 24 * 60;

/// Inclusive ceiling for a meal slot's `serving_minute` — minutes past
/// midnight on the menu's date, so `0` = 00:00 and `1439` = 23:59. The cutoff
/// above counts back from that instant.
///
/// **The clock is UTC.** This backend deliberately stores no school timezone
/// (rejected feature), so staff enter the serving time in UTC: a UTC+3 school
/// types `540` (09:00) to mean noon locally. Optional per slot — and a slot
/// without one has **no cutoff at all**, since there is no instant to count a
/// deadline back from; the cutoff knob starts binding that slot the day the
/// school sets its hour.
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

/// The only accepted school-payment ledger kinds. `charge`: an installment the
/// student owes, appended when a fee plan is assigned. `credit`: money in,
/// against one named charge. `refund`: money paid back out, against one named
/// credit. `reversal`: a mistaken `charge` or `refund` undone — the ledger is
/// append-only, so the opposing line is appended rather than the line edited.
/// A mistaken *credit* is corrected by a `refund`, which is why `reversal`
/// never points at one.
pub const PAYMENT_LEDGER_KINDS: [&str; 4] = ["charge", "credit", "reversal", "refund"];

/// Bounds on a fee plan: its name, and how many installments it may carry.
/// Sixty covers a monthly plan over five years — well past any school year,
/// and low enough that one assignment cannot append a thousand ledger lines.
pub const MAX_FEE_PLAN_NAME_LEN: usize = 120;
pub const MAX_FEE_PLAN_INSTALLMENTS: usize = 60;

/// How many students one `POST /payments/plans/{id}/assignments` may name.
/// Bulk placement is a whole class at a time, not the whole school.
pub const MAX_FEE_PLAN_ASSIGN_STUDENTS: usize = 200;

/// How many charge lines one assignment request may append: the students it
/// names times the plan's installments. The student cap alone cannot see the
/// schedule — 200 students on a 60-installment plan is 12 000 sequential
/// writes in one HTTP request, and nothing in the stack times a request out.
/// Three thousand leaves the ordinary shapes whole (200 students up to a
/// 15-installment plan, 50 students on the largest plan there is) and the
/// refusal tells the caller to split the batch.
pub const MAX_FEE_PLAN_ASSIGN_WRITES: usize = 3_000;

/// How many lines may be applied to one ledger line — the payments under a
/// charge, the refunds under a payment, and every reversal among them. The
/// over-payment cap folds that whole subtree one query per line while holding
/// the process-global payment lock, so an unbounded subtree is an unbounded
/// stall for every other payment in the school: 2 000 one-kuruş payments make
/// the next one issue 2 001 queries with the lock held. Twenty pieces is
/// already a pathological way to settle a single installment.
pub const MAX_LEDGER_APPLIED_LINES: usize = 20;

/// Ceiling on the client-chosen `request_key` that makes a credit or a refund
/// retry-safe: it becomes part of the ledger line's record id, so it is bounded
/// and drawn from `[A-Za-z0-9-]` — `_` joins the parts of a ledger id, so a key
/// carrying one could spell another line's id. Sixty-four characters take a
/// UUID or a receipt number comfortably.
pub const MAX_PAYMENT_REQUEST_KEY_LEN: usize = 64;

/// Bounds on a whiteboard: its title, and how many people the creator may name
/// onto it. Two hundred is a big club or a whole grade, not the school; every
/// participant may draw, so this is also what bounds one board's writer count.
///
/// It was fifty while a roster could only be typed one id at a time. Bulk
/// invite (`POST /boards/{id}/invite`) resolves a class section, a course or an
/// event's roster in one call, and fifty refused an ordinary club — so the
/// ceiling moved to the size of the largest group a school actually puts on one
/// canvas. Raising it can only widen what an existing row is allowed to hold.
pub const MAX_BOARD_TITLE_LEN: usize = 200;
pub const MAX_BOARD_PARTICIPANTS: usize = 200;

/// Ceiling on one stroke's serialized payload. A stroke is a short path — a
/// handful of points, a colour, a width — and it is stored verbatim and fanned
/// out to every socket in the room, so it is bounded at the wire rather than at
/// the canvas: 4 KiB takes a long freehand curve and still keeps the worst-case
/// board (`MAX_BOARD_STROKES` of them) inside a couple hundred megabytes.
pub const MAX_STROKE_PAYLOAD_LEN: usize = 4_096;

/// The two growth caps, both counted on the board row (see the counter fields
/// below). They are different kinds of full on purpose:
///
/// `MAX_EPOCH_STROKES` caps the *live* canvas — the strokes since the last
/// clear. Hitting it is recoverable: the creator clears, the epoch counter
/// resets to zero and drawing resumes. Nothing is deleted by that clear.
///
/// `MAX_BOARD_STROKES` caps the board's total *storage*, and never resets,
/// because a clear keeps its history for playback. Hitting it stamps
/// `closed_at`: the board turns permanently read-only, but stays fully
/// readable and replayable — a closed board loses no stroke it ever carried.
pub const MAX_EPOCH_STROKES: i64 = 5_000;
pub const MAX_BOARD_STROKES: i64 = 50_000;

/// How many boards one creator may hold. Without it the two caps above cost an
/// attacker nothing — a full board is answered by opening the next one — so
/// this is the counter that actually bounds a single account's storage. Kept on
/// the user row, the same shape as `CHATBOT_THREAD_COUNT_FIELD`.
pub const MAX_BOARDS_PER_CREATOR: i64 = 200;

/// The only accepted stroke kinds. `stroke`: a drawn path, carrying its
/// payload. `clear`: the marker row that ends an epoch — it deletes nothing,
/// it is the epoch index, so replaying across it reconstructs the whole
/// session.
pub const BOARD_STROKE_KINDS: [&str; 2] = ["stroke", "clear"];

/// Ceiling on a `board_id` arriving on the board WebSocket, for the same reason
/// as [`MAX_QUESTION_ID_LEN`]: the field is a record key — a 26-char ULID in
/// every real payload — and the error frame *echoes* it back, so an unbounded
/// id lets a client make its own room reflect a 64 MiB frame at it. Checked
/// before any database work.
pub const MAX_BOARD_ID_LEN: usize = 64;

/// Inclusive bounds for an exam's per-attempt duration, milliseconds
/// (1 minute to 24 hours). Required for `async`, optional for `open`.
pub const MIN_EXAM_DURATION_MS: i64 = 60 * 1000;
pub const MAX_EXAM_DURATION_MS: i64 = 24 * 60 * 60 * 1000;

/// Upper bound for an exam's attempt limit; `UNLIMITED_EXAM_ATTEMPTS` (zero)
/// is the wire-and-storage spelling of "no limit". A limit of 1 — the
/// default — is the classic single sitting.
pub const MAX_EXAM_ATTEMPTS: i64 = 100;
pub const UNLIMITED_EXAM_ATTEMPTS: i64 = 0;
/// Ceiling on a single HTTP request. Backstop for requests that reached the
/// database in the window between the socket dying and the keepalive noticing:
/// without it they park until the database returns, which can be hours.
/// Generous enough not to clip a legitimate slow upload at `max_file_bytes`.
pub const REQUEST_TIMEOUT_SECS: u64 = 30;

/// The longest caller-supplied `x-request-id` header the server will carry.
///
/// The id lands on `http.request.id` of every span, so anything a client puts
/// there it can export: free text and unbounded cardinality both. A header
/// longer than this (or one carrying anything outside `[A-Za-z0-9._-]`) is
/// dropped and a UUID minted in its place — the request still succeeds, it
/// just stops being correlatable by the caller's own id, which is exactly why
/// a client needs to know the bound.
pub const MAX_REQUEST_ID_LEN: usize = 64;

/// Backoff ceiling for the boot connection retry. The database is usually a
/// sibling container coming up in parallel, so the first attempts fail; the
/// process retries forever rather than exiting, because exiting turns a
/// few-second wait into a container restart loop that outruns its budget and
/// stays down.
pub const DB_CONNECT_BACKOFF_MAX_SECS: u64 = 5;

/// Cadence of the `state` ticks on the student exam-room WebSocket
/// (`GET /exams/{id}/attempt/ws`).
pub const EXAM_WS_TICK_SECS: u64 = 2;

/// Cadence of the keepalive ticks on the whiteboard-room WebSocket. Slower than
/// the exam room's, because a board room carries no countdown a client renders
/// against — the tick only keeps an idle socket from being reaped.
pub const BOARD_WS_TICK_SECS: u64 = 15;

/// How many strokes one replay message carries when a socket joins. The join
/// replays the current epoch, which is `MAX_EPOCH_STROKES` at worst, so it is
/// sent in batches rather than as one frame that could reach the WebSocket
/// frame limit on a busy board.
pub const BOARD_REPLAY_CHUNK: usize = 200;

/// Depth of one board room's fan-out channel: how far a slow socket may lag the
/// strokes being drawn before it is dropped and has to rejoin (which replays
/// the epoch from storage anyway, so nothing is lost by the drop).
pub const BOARD_HUB_CAPACITY: usize = 256;

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
pub const AI_PROTOCOL: &str = "hab/2";
pub const AI_ALPN: &[u8] = b"hab/2";

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

/// How long a blob body may make *no* progress before its stream is reset.
/// A STALL bound, not a transfer cap: the clock is per write, so a slow peer
/// that keeps reading streams a whole `max_file_bytes` file for as long as it
/// takes, while a service that opens a blob stream and never reads it stops
/// pinning a task and an open file descriptor once the QUIC stream window
/// fills. A wall-clock deadline on the transfer could not tell the two apart.
pub const AI_BLOB_WRITE_STALL_SECS: u64 = 30;

/// The capability an AI service declares to answer chatbot turns. The bridge
/// routes a chat request to any worker carrying it; with none registered the
/// chat endpoints report the service as unavailable.
pub const AI_CHAT_CAPABILITY: &str = "chat.reply";

/// The capability an AI service declares to index a course note for retrieval.
/// The backend dispatches on it after a note (or one of its files) changes;
/// with no worker carrying it the trigger is a silent no-op — indexing is a
/// bonus on top of the note, never a condition of storing one.
pub const AI_RAG_INDEX_CAPABILITY: &str = "rag.index";

/// Deadline on one `rag.index` round trip. Longer than a chat turn
/// ([`AI_DEFAULT_REQUEST_TIMEOUT_SECS`]): chunking and embedding a note with
/// ten attachments is batch work, and nobody is waiting on it — the dispatch
/// is fire-and-forget, off the HTTP request path.
pub const AI_RAG_INDEX_TIMEOUT_SECS: u64 = 120;

/// The capability an AI service declares to answer a RAG question over a
/// school's course-note corpus. Separate from [`AI_CHAT_CAPABILITY`] on
/// purpose: retrieval-then-answer is its own service with its own corpus, and
/// a deployment may run one without the other. With no worker carrying it the
/// RAG endpoints report the service as unavailable.
pub const AI_RAG_CHAT_CAPABILITY: &str = "rag.chat";

/// Deadline on one `rag.chat` round trip. Much longer than a plain chat turn
/// ([`AI_DEFAULT_REQUEST_TIMEOUT_SECS`]): the service first retrieves the
/// relevant passages and only then generates an answer, so a question that
/// touches several course notes is retrieval plus inference, not inference
/// alone.
pub const AI_RAG_CHAT_TIMEOUT_SECS: u64 = 90;

/// The capability an AI service declares to analyse one student (ZEKA,
/// `hezarfen_zeka`). It is the on-demand half of ZEKA — the service also
/// recomputes on its own schedule — and it is what a teacher's "hesapla" is
/// routed to. The service reads the student's own data back through the
/// bridge's api reads and has the backend write its `zeka_*` rows — no AI
/// service holds a school database credential here (see the storage-surface
/// constants below).
pub const AI_INSIGHT_STUDENT_CAPABILITY: &str = "insight.student";

/// Deadline on one `insight.student` round trip. Computing one student reads
/// several of the school's sources (marks, attendance, submissions, study
/// stints) and writes the derived rows before answering, so it is
/// batch-shaped work — the same ceiling [`AI_RAG_INDEX_TIMEOUT_SECS`] gives a
/// note with ten attachments. Nobody waits on it: the dispatch runs detached
/// and a reader polls the stored rows.
pub const AI_INSIGHT_STUDENT_TIMEOUT_SECS: u64 = 120;

/// The capability an AI service declares to analyse one class×course (a
/// şube's own instance of a course). Declared here so the name exists in one
/// place, but **no door dispatches it yet**: the request would name a course
/// the service has to enumerate a roster for, and the bridge's read allowlist
/// carries no member listing — that half of the contract is still open
/// (`hezarfen_zeka/service/docs/BACKEND-GEREKSINIMLERI.md`, item 3).
pub const AI_INSIGHT_CLASS_CAPABILITY: &str = "insight.class";

/// The capability an AI service declares to recompute a whole school's
/// insights. A batch job: the service works through its configured student
/// list under its own time budget and has the backend write its run ledger. Like ZEKA's own
/// schedule, this is optional on top of the service's own cadence — it is
/// what lets the office trigger a sweep from the UI.
pub const AI_INSIGHT_REFRESH_CAPABILITY: &str = "insight.refresh";

/// Deadline on one `insight.refresh` round trip — the longest AI deadline in
/// this file. The service answers when its batch is over: the ceiling is
/// sized well past ZEKA's default 60s per-school budget (a raised
/// `ZEKA_BUDGET_MS` included) and well inside its hourly default cadence. The
/// run's progress does not depend on it — ZEKA writes its own `zeka_run`
/// ledger — this only bounds how long the detached task waits for the
/// summary.
pub const AI_INSIGHT_REFRESH_TIMEOUT_SECS: u64 = 300;

// ---- zeka's storage surface (capabilities the *backend* serves) ------------
//
// The three constants above are capabilities an AI service *declares*: the
// backend dispatches `insight.student`/`insight.refresh` to ZEKA and ZEKA
// answers. The nine below run the other way and are the reason this section
// exists: no AI service may touch a school database directly, so every read
// and write ZEKA needs against its own `zeka_*` tables is a named operation
// the backend performs on its behalf, on the same bridge, on a
// client-initiated stream (`src/ai/server.rs::serve_client_stream`).
//
// They are deliberately *not* declared here as worker capabilities: a worker
// never announces them, and `AiBridge::dispatch` never routes them. They are
// the backend's own surface, dispatched by capability name against
// `crate::ai::insight`'s table — an operation, never a query: no capability
// takes a database, a schema, a table or a SQL fragment, and the school is
// the frame's own field, resolved to that school's database before any
// statement runs.

/// ZEKA stores one student's computed summary (and that student's attention
/// items, replaced wholesale) for one school.
pub const AI_INSIGHT_SUMMARY_UPSERT_CAPABILITY: &str = "insight.summary.upsert";

/// ZEKA stores the recommendation cards a run produced. The backend mints the
/// rows' ids — an id is the writer's, and the writer is now the backend.
pub const AI_INSIGHT_RECOMMENDATION_UPSERT_CAPABILITY: &str = "insight.recommendation.upsert";

/// ZEKA stores its question-segment labels (and each question's dimension
/// split, replaced wholesale).
pub const AI_INSIGHT_SEGMENT_UPSERT_CAPABILITY: &str = "insight.segment.upsert";

/// ZEKA stores a student's per-dimension segment profile.
pub const AI_INSIGHT_PROFILE_UPSERT_CAPABILITY: &str = "insight.profile.upsert";

/// ZEKA stores one compute run's ledger row (and the run's pending students
/// and failed modules, both replaced wholesale).
pub const AI_INSIGHT_RUN_UPSERT_CAPABILITY: &str = "insight.run.upsert";

/// ZEKA reads back the students the freshest run left pending — where the
/// next run starts.
pub const AI_INSIGHT_PENDING_LIST_CAPABILITY: &str = "insight.pending.list";

/// ZEKA asks the backend to sweep every `zeka_*` table of rows whose
/// retention window has closed.
pub const AI_INSIGHT_RETENTION_SWEEP_CAPABILITY: &str = "insight.retention.sweep";

/// ZEKA asks the backend to delete the derived rows of students who are no
/// longer enrolled.
pub const AI_INSIGHT_DEPARTED_PURGE_CAPABILITY: &str = "insight.departed.purge";

/// The one deployment-scoped operation: the active schools a shared AI fleet
/// schedules over. The frame carries no school because its answer is the
/// directory itself — the same reason [`crate::ai::protocol::Hello`] carries
/// none.
pub const AI_INSIGHT_SCHOOLS_LIST_CAPABILITY: &str = "insight.schools.list";

/// The most rows one of the four bulk writes may carry in a single call. The
/// service's own batch size (`store.BATCH_SIZE` in `hezarfen_zeka`) — it
/// chunks its nightly output at exactly this, and a call one row past it is
/// refused (`too_many_rows`) rather than truncated, because a silently
/// shortened batch would read as a written student.
pub const MAX_INSIGHT_BATCH_ROWS: usize = 500;

/// The most students `insight.pending.list` will answer with. The list is
/// handed over whole — the service resumes its next run from it, so a
/// clipped list would silently drop students — and a school past this bound
/// is refused (`too_many_rows`) instead.
pub const MAX_INSIGHT_PENDING_STUDENTS: usize = 20_000;

/// The most students one `insight.departed.purge` call may name. Same shape
/// as [`MAX_INSIGHT_PENDING_STUDENTS`]: over it the call is refused, and the
/// empty list is refused too — an empty roster is a fetch that failed, and
/// deleting every derived row on that reading would be the worst available
/// mistake.
pub const MAX_INSIGHT_PURGE_STUDENTS: usize = 20_000;

/// How long a `rag.chat` turn may sit `pending` before a reader projects it as
/// failed. The RAG nest mirrors the chatbot's asynchronous send
/// ([`CHATBOT_PENDING_STALE_SECS`]): the round trip outlives the request that
/// started it, and a restart mid-inference strands the row rather than letting
/// anyone answer it, so the same staleness window applies.
pub const RAG_PENDING_STALE_SECS: i64 = CHATBOT_PENDING_STALE_SECS;

// ---- podcast ---------------------------------------------------------------
//
// The podcast service turns one stored source into an audio episode, which is
// a *job*. The backend owns the job's record: `POST /podcast/jobs` mints the
// id, writes the `podcast_job` row, and dispatches `podcast.submit`; the
// service reports every transition back through the client-initiated
// `podcast.report` capability and uploads the finished mp3 as raw bytes. The
// read doors (`GET /podcast/jobs/{id}`, `.../result`, `.../audio`) answer from
// the row, so a service restart or a wiped service volume costs liveness at
// most. Only submit and cancel are dispatched to a worker, and both answer
// immediately — the pipeline runs in the service's own pool — so neither needs
// a per-capability deadline: the bridge's ordinary request timeout covers a
// job-store write.

/// The capability an AI service declares to accept a podcast job. The backend
/// dispatches on it from `POST /podcast/jobs`, and the payload carries the
/// **backend-minted** job id — the id the row, the service's own record, and
/// the audio blob are all keyed by.
pub const AI_PODCAST_SUBMIT_CAPABILITY: &str = "podcast.submit";

/// The capability the **backend** serves for one podcast job's state reports.
/// The service calls it — on a client-initiated stream, the same shape as
/// ZEKA's storage surface — once per transition, echoing the job's identity
/// (`job_id`, `source_id`, `user_id`) and the state it moved to. The backend
/// validates the echo, the transition and the retention window, and writes its
/// own row; there is no state the service holds that the backend does not.
pub const AI_PODCAST_REPORT_CAPABILITY: &str = "podcast.report";

/// The capability an AI service declares to cancel one podcast job. Its own
/// capability rather than a flag on submit: the cancel is a write on the
/// service's queue, and the capability is what decides whether a deployment
/// can offer the door at all.
pub const AI_PODCAST_CANCEL_CAPABILITY: &str = "podcast.cancel";

/// The largest episode the backend will ingest from a service
/// (`BlobUploadRequest`). A ceiling, not a quota: a two-hour episode at a
/// generous bitrate is well under it, and anything above is refused before a
/// byte reaches disk — a service bug must not be able to fill the host.
pub const PODCAST_AUDIO_MAX_BYTES: usize = 256 * 1024 * 1024;

/// How long a finished podcast job's record and audio stay readable. Past it
/// the doors answer `410` — the record is history, and the blob is no longer
/// served. Sized like the RAG outputs' retention window: long enough for a
/// listener to come back for yesterday's episode, short enough that a shared
/// host does not accumulate every episode ever made.
pub const PODCAST_JOB_RETENTION_SECS: i64 = 7 * 24 * 60 * 60;

/// The floor of the read-side staleness window: a `queued`/`running` job
/// nobody has updated for at least this long is presented as
/// `failed`/`interrupted`. The window is
/// `max(PODCAST_JOB_STALE_FLOOR_SECS, eta_secs × PODCAST_JOB_STALE_ETA_FACTOR)`
/// so a queue wait the service itself estimated as long is never declared
/// dead mid-flight.
pub const PODCAST_JOB_STALE_FLOOR_SECS: i64 = 600;

/// How many times a job's own ETA the service may exceed before the read side
/// stops believing it is still running. Three: past three times its own
/// estimate, the honest reading is that the process that owned it is gone.
pub const PODCAST_JOB_STALE_ETA_FACTOR: i64 = 3;

/// The `error_code` a job carries when *this backend* concluded it died: a
/// dispatch that never reached a worker, a cancel of a job the service no
/// longer has, or a read-side projection of a stale row. Deliberately the
/// service's own word for the same fact (its restart sweep writes it too), so
/// a client sees one vocabulary however the job died.
pub const PODCAST_INTERRUPTED_CODE: &str = "interrupted";

/// The most `(sınıf, ders)` scope pairs one `rag.chat` request may carry. The
/// corpus is routed by the **pair** — a grade and a subject list sent
/// separately would cross-product into combinations the asker never named — so
/// this is the number of pairs handed to the service in one call, bounding
/// both the frame size and the service's routing work. A request carrying more
/// is refused rather than silently narrowed: dropping pairs would answer a
/// question the asker did not ask, scoped to corpora they did not name.
pub const MAX_RAG_SCOPE_PAIRS: usize = 200;

/// Default per-minute message limit for the RAG nest, when the school sets
/// none — a lower ceiling than the chatbot's, since every message here spends
/// a retrieval over the whole course-note corpus before it generates anything.
pub const DEFAULT_RAG_RATE_LIMIT: u32 = 6;

/// The most citations one `rag.chat` answer may carry into its stored reply.
/// The service is a trust boundary — everything it sends is written verbatim
/// into a school's database — so an answer citing more is refused whole rather
/// than trimmed: dropping entries would leave `[N]` markers in the answer text
/// pointing at nothing, and a reader cannot tell a clipped citation list from
/// a complete one.
pub const MAX_RAG_CITATIONS: usize = 50;

/// The most pages one citation may name, the second axis of the same bound.
/// One reply's citations are therefore at most
/// `MAX_RAG_CITATIONS × MAX_RAG_CITATION_PAGES` page numbers, so however a
/// service distributes them it cannot turn a single stored answer into an
/// unbounded payload — the same storage-protection shape as
/// [`MAX_NOTE_FILES`], but for a body this backend does not compose.
pub const MAX_RAG_CITATION_PAGES: usize = 50;

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

/// The largest per-request AI deadline a deployment may configure
/// (`AI_REQUEST_TIMEOUT_SECS`). Anything above it is clamped down to it at
/// startup, loudly — see [`crate::config`].
///
/// A sanity ceiling, well under [`CHATBOT_PENDING_STALE_SECS`]: a reply that
/// arrives after the staleness window is discarded by the read-time
/// projection, so a deadline anywhere near it could never produce an answer a
/// user sees — it would only hold a task and a worker slot open for nothing.
/// A school that genuinely needs longer raises this and the staleness window
/// together.
pub const AI_MAX_REQUEST_TIMEOUT_SECS: u64 = 60;

/// How long a dialling AI service has to complete its `Hello`/`Welcome`
/// exchange before the connection is dropped. Short: the handshake is one
/// frame each way, and an unauthenticated connection must not be able to
/// occupy the listener indefinitely.
pub const AI_HANDSHAKE_TIMEOUT_SECS: u64 = 10;

/// The only REST paths an AI service may read over the bridge — deny-by-default,
/// matched segment-wise by [`crate::ai::api`] with `{x}` standing for exactly
/// one segment. This list *is* the services' access scope, so it is a service
/// contract documented in `README.md`, not a tunable published at `/limits`
/// (string arrays are exempt from that completeness gate).
///
/// JSON-only, and only what a study-companion needs: who the user is, their
/// notes, their course's notes, their homework, and their own progress reports.
/// Byte-serving routes
/// (note files, submission files, avatars, question images) are left out — the
/// frames carry JSON, and a blob has no business crossing this seam yet.
pub const AI_API_ALLOWLIST: &[&str] = &[
    "/auth/me",
    "/users/me/profile",
    "/users/{id}/profile",
    "/notes",
    "/notes/{id}",
    "/course-notes",
    "/course-notes/{id}",
    "/course-notes/{id}/files",
    "/homework",
    "/homework/{id}",
    "/homework/{id}/result",
    "/homework/{id}/submission",
    "/homework/report/{user}",
    "/marks/me",
    "/marks/{user}",
    "/attendance/me",
    "/attendance/{user}",
    "/pomodoro/me",
    "/pomodoro/{user}",
];

// --- rate limiting -----------------------------------------------------

/// Default requests-per-minute-per-IP for `/auth/login` + `/auth/register`.
pub const DEFAULT_AUTH_RATE_LIMIT: u32 = 10;
/// Default requests-per-minute-per-IP across the whole API.
pub const DEFAULT_API_RATE_LIMIT: u32 = 300;
/// Default chatbot messages per minute *per user*. Sits far above a human
/// typing while still blunting a scripted fan-out at the AI service behind the
/// bridge — which is the cost this tier rations, not this process.
pub const DEFAULT_CHATBOT_RATE_LIMIT: u32 = 20;

/// Once the bucket map holds this many distinct clients, a new key sweeps out
/// the expired entries and, if that frees nothing, evicts the least-spent live
/// buckets — **never an exhausted one** — down to three quarters of this.
/// Evicting a bucket that had reached its limit would hand back the very
/// refusal it was enforcing, so a flood cannot make the limiter fail open; the
/// concession is that while the map is saturated with exhausted buckets a
/// never-before-seen client gets no bucket of its own and is metered against
/// the one shared [`RATE_LIMIT_OVERFLOW_MAX`] budget instead — so newcomers
/// during a flood can be refused, while every client already in the map keeps
/// its own counter untouched. Keeps memory bounded without a reaper task.
pub const PURGE_AT: usize = 10_000;

/// Requests per window every client that finds the bucket map saturated shares
/// between them — one aggregate counter, no key, no map entry.
///
/// It is the bound on the fail-open path: without it a new key arriving at a
/// full map is admitted unmetered, so an attacker who first fills the map with
/// exhausted buckets buys unlimited throughput from fresh keys. With it that
/// bypass is capped at this many requests a minute, fleet-wide, and a genuine
/// newcomer gets in on whatever of it is left — an attacker who burns the lot
/// does `429` the newcomers, which refusing them outright would have done for
/// free. Clients already in the map are unaffected throughout. Sized for the
/// genuine side: this is a *request* budget, not a client one (a keyless client
/// spends it on every request, not just its first), so roughly 10 requests a
/// second for all newcomers together — far under any single client's API tier,
/// and only ever in force while 10k buckets sit exhausted at once, which is an
/// attack and not a school day.
pub const RATE_LIMIT_OVERFLOW_MAX: u32 = 600;

/// The shared counter the limiter folds its local admits into, so a window's
/// budget survives a restart (see [`crate::rate_limit`]).
pub const RATE_LIMIT_TABLE: &str = "rate_limit";

/// How often a limiter pushes its admits and reads the window's total back.
/// The knob that decides the worst case: within one interval a just-started
/// process can admit up to its full local budget before the total tightens it, so
/// short enough to matter and long enough to stay one query per tier per tick.
pub const RATE_SYNC_INTERVAL_SECS: u64 = 2;

/// How long one sync round may wait on the database. The SDK *parks* a query
/// while the socket is down instead of failing it, so the liveness flag alone
/// cannot stop a round wedging the task forever — this is the backstop that
/// keeps the limiter purely local through an outage instead of stalled.
pub const RATE_SYNC_TIMEOUT_SECS: u64 = 5;

/// How many client buckets one round syncs. A round is a single query holding
/// one statement per bucket, so this bounds both its size and the write volume
/// a burst of distinct clients can put on the database.
pub const RATE_SYNC_MAX_KEYS: usize = 512;

// --- time spans --------------------------------------------------------

pub const MILLIS_PER_DAY: i64 = 24 * 60 * 60 * 1000;

/// One week, the only recurrence step this backend expands.
pub const MILLIS_PER_WEEK: i64 = 7 * MILLIS_PER_DAY;

/// The record key `User::ai_principal` carries. A literal, not a ULID, so it
/// can never collide with a minted user row — and the row is never written.
pub const AI_PRINCIPAL_KEY: &str = "ai_service";

/// A `palette_color` preference is `#` plus exactly six hex digits — the length
/// of the whole string, `#` included.
pub const PALETTE_COLOR_LEN: usize = 7;

/// The shape a `palette_color` must match, as a regular expression. This
/// describes what the server **accepts**, so it is case-insensitive — the value
/// is normalized to lowercase on store, and a client that sent `#FEFAE0` reads
/// `#fefae0` back. Publishing the lowercase-only form would have a client
/// reject its own valid input.
///
/// Deliberately an open value set: any valid hex accent color, not a closed
/// list of the frontend's palette, so a new palette entry needs no backend
/// change.
pub const PALETTE_COLOR_PATTERN: &str = "^#[0-9a-fA-F]{6}$";

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

// --- database tables ---------------------------------------------------

pub const NOTE_TABLE: &str = "note";
pub const MESSAGE_TABLE: &str = "message";
pub const NOTE_FILE_TABLE: &str = "note_file";
pub const COURSE_NOTE_TABLE: &str = "course_note";
pub const COURSE_NOTE_FILE_TABLE: &str = "course_note_file";
pub const RAG_OUTPUT_TABLE: &str = "rag_output";
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
pub const CLASS_GROUP_TABLE: &str = "class_group";
pub const CLASS_MEMBER_TABLE: &str = "class_member";
pub const CLASS_COURSE_TABLE: &str = "class_course";
pub const CLASS_BLUEPRINT_TABLE: &str = "class_blueprint";
pub const PARENT_LINK_TABLE: &str = "parent_link";
pub const COURSE_SESSION_TABLE: &str = "course_session";
pub const SESSION_ATTENDANCE_TABLE: &str = "session_attendance";
pub const WORK_ENTRY_TABLE: &str = "work_entry";
pub const POMODORO_SESSION_TABLE: &str = "pomodoro_session";
pub const SETTINGS_TABLE: &str = "settings";
pub const TERM_TABLE: &str = "term";
pub const SUBJECT_TABLE: &str = "subject";
pub const POOL_QUESTION_TABLE: &str = "pool_question";
pub const POOL_QUESTION_IMAGE_TABLE: &str = "pool_question_image";
pub const SOLUTION_TABLE: &str = "solution";
pub const SOLUTION_IMAGE_TABLE: &str = "solution_image";
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
pub const FEE_PLAN_TABLE: &str = "fee_plan";
pub const FEE_PLAN_ASSIGNMENT_TABLE: &str = "fee_plan_assignment";
pub const PAYMENT_LEDGER_TABLE: &str = "payment_ledger";
pub const BOARD_TABLE: &str = "board";
/// One row per badge a user has earned, keyed by the pair. A table of its own
/// rather than an array column on the user row: an `array<…>` there breaks
/// every `PATCH` of that row, and awards are append-only facts with their own
/// timestamp (see [`crate::domain::badge`]).
pub const BADGE_AWARD_TABLE: &str = "badge_award";
/// One row per *name* the school's settings offer, keyed by the name itself:
/// how many rows still reference it, and whether it has been retired out of the
/// list (see the reference counters in [`crate::db::cap`]).
pub const KIND_REF_TABLE: &str = "kind_ref";
pub const SLOT_REF_TABLE: &str = "slot_ref";

// --- stored cap counters -------------------------------------------------

/// The counter columns behind the count caps (see [`crate::db::cap`]).
/// Each one lives on the *parent* row, because a single-record conditional
/// `UPDATE` is the only guard a concurrent writer cannot outrun (a lock is
/// released around the round trip). Spelled here rather than at the call site
/// so the field a claim
/// increments and the field a delete decrements cannot drift apart; the
/// definitions themselves are in `MIGRATION`, and a typo there is caught by
/// SCHEMAFULL refusing the write.
pub const ENROLLMENT_COUNT_FIELD: &str = "enrollment_count";
pub const REGISTRATION_COUNT_FIELD: &str = "registration_count";
/// The two refcounts on a class row: how many students it holds and how many
/// courses it is attached to. A class may only be dropped at zero on both.
pub const CLASS_MEMBER_COUNT_FIELD: &str = "class_member_count";
pub const CLASS_COURSE_COUNT_FIELD: &str = "class_course_count";
/// Classes belonging to an academic year, the second half of the year's delete
/// guard. Its own column: a class's share is never confused with another
/// roster's count.
pub const ACADEMIC_YEAR_CLASS_COUNT_FIELD: &str = "class_count";
/// A refcount too, and the whole of the fee-plan edit *and* delete guard: how
/// many students are on this plan. A plan may only be edited or deleted at
/// zero, and the count is claimed in the same transaction as the assignment
/// row, so a manager's edit and a concurrent assign contend on the plan row
/// rather than on a `SELECT` the edit had already outrun. Nothing ever
/// releases it — an assignment is never unassigned, because the charges it
/// raised are history.
pub const FEE_PLAN_ASSIGNMENT_COUNT_FIELD: &str = "assignment_count";
/// The condition itself, spelled once: a plan is editable and deletable
/// exactly while nobody is on it. The `??` is parenthesized on purpose —
/// `count ?? 0 = 0` parses as `count ?? (0 = 0)`, which is truthy for *every*
/// row and would license editing a plan a family is already being billed for.
pub const FEE_PLAN_UNASSIGNED_GUARD: &str = "(assignment_count ?? 0) = 0";
pub const SUBMISSION_FILE_COUNT_FIELD: &str = "file_count";
pub const CHATBOT_THREAD_COUNT_FIELD: &str = "chatbot_thread_count";
/// The column a *grant* claim moves and puts back, so its transaction writes
/// the holder's own `user` record — the one key
/// [`crate::service::user::set_role`] writes (see
/// [`crate::db::cap::role_claim`]). Any `option<int>` on the row would do:
/// the claim nets zero and never reads it, so this is an alias rather than a
/// column of its own — a new one would mean a migration on a SCHEMAFULL table
/// for a value nothing ever observes.
pub const USER_ROLE_CLAIM_FIELD: &str = CHATBOT_THREAD_COUNT_FIELD;
/// The lifetime totals behind the badges, all on the user row. Not caps: each
/// one counts something the person *did*. A counter comes down in exactly one
/// shape, and the two cases that qualify share a reason: the account credited
/// is the account that can delete what earned it, so a strictly monotonic
/// counter is a farm. A student withdrawing their own submission gives the two
/// homework counters back (else one homework becomes fifty by
/// submit/delete/submit), and a teacher un-grading gives back
/// `MARKS_GIVEN` — with `HIGH_MARK` on the exam side, credited to the student
/// by the same act (else one exam becomes fifty by grade/ungrade/regrade).
/// The exam and pomodoro totals never decrease at all, and each earns that by
/// counting something a repeat cannot re-earn rather than by anyone's restraint:
/// `EXAM_SAT_TOTAL` counts *exams sat*, moving only on a student's first sitting
/// of an exam (`seq == 1`), because retakes are a loop the student drives alone —
/// an open exam with unlimited attempts is start/finish/start, no teacher in it —
/// and a stint is a span of time that has to be lived through to be finished.
/// Nothing else decrements: the cascades (an exam, a homework or
/// a course delete, which do take the attempt and result rows with them) leave
/// them alone, since history a teacher erased is still history the student
/// lived. A badge already earned is never taken back either, whatever a counter
/// does afterwards — [`crate::domain::badge`] only ever adds award rows. Floored
/// at zero, and absent means zero: an account older than the columns reads
/// exactly like a fresh one.
//
// corner-cut: that cascade ruling is also the farm's long way round — delete the
// *exam* instead of the mark and the credit stands, so the loop still climbs,
// at a couple of requests a point rather than two. Closing it means
// `Exam::delete`, `Homework::delete` and `Course::delete` refunding per swept
// row the way `ExamResult::remove` now does, which is a policy call (a course
// delete would then have to walk every mark it drops) rather than an oversight.
pub const HOMEWORK_SUBMITTED_TOTAL_FIELD: &str = "homework_submitted_total";
pub const HOMEWORK_ON_TIME_TOTAL_FIELD: &str = "homework_on_time_total";
pub const EXAM_SAT_TOTAL_FIELD: &str = "exam_sat_total";
pub const POMODORO_FINISHED_TOTAL_FIELD: &str = "pomodoro_finished_total";
pub const POMODORO_FOCUS_MS_TOTAL_FIELD: &str = "pomodoro_focus_ms_total";
/// The bookkeeping behind the counted-stint rule below: which UTC day the
/// student last had a stint counted on, and how many counted that day. Internal
/// state like the two streak columns — no badge reads either, `/limits` names
/// neither, no profile key serves them. The day rolls the tally back to zero.
pub const POMODORO_COUNTED_DAY_FIELD: &str = "pomodoro_counted_day";
pub const POMODORO_COUNTED_TODAY_FIELD: &str = "pomodoro_counted_today";
/// What makes a finished stint *count* towards `pomodoro_finished_total` and
/// `pomodoro_focus_ms_total` (and towards the study streak): it must have run at
/// least this long, and it must be within the day's quota.
///
/// Without them the pair is the farm [`crate::db::pool_question::approve`]
/// reasons about from the other end: `finish` is self-service, a stint costs two
/// requests and no second person, so a counter moved once per round-trip is
/// farmable — 200 pairs in a minute and a half bought `pomodoro_finished_200`,
/// permanently, since a badge is never revoked.
///
/// Five minutes is well under a conventional 25-minute pomodoro on purpose: a
/// student who breaks off early still focused, and the counter should not
/// punish that. It is four orders of magnitude above a scripted round-trip,
/// which is the whole distance that matters — the cheat now costs the wall
/// clock it claims. Sixteen a day is likewise above any honest school day
/// (sixteen full pomodoros is over six hours of pure focus) while capping the
/// minimum-length farm at eighty minutes of real waiting per day, so
/// `pomodoro_finished_200` takes at least thirteen calendar days to buy instead
/// of ninety seconds. Both are read `>=`/`<`, i.e. a stint of exactly the
/// minimum counts and the sixteenth of the day counts.
pub const MIN_COUNTED_POMODORO_MS: i64 = 300_000;
pub const MAX_COUNTED_POMODORO_PER_DAY: i64 = 16;
/// A stint's optional student-given label — the student's own name for what
/// the stint is for ("math", "TYT denemesi"), free text rather than a subject
/// link, bounded like the other short labels.
pub const MAX_POMODORO_LABEL_LEN: usize = 100;
/// The staff-side and second student-side totals, same shape and same rules:
/// lifetime, floored at zero, absent reads as zero. `MARKS_GIVEN` and
/// `LESSONS_HELD` are what a teacher accumulates; `POOL_APPROVED` counts the
/// approvals a teacher hands out and `POOL_PUBLISHED` the questions whose
/// author got approved — credited at approval, which is why the name says
/// published rather than asked.
pub const MARKS_GIVEN_TOTAL_FIELD: &str = "marks_given_total";
pub const LESSONS_HELD_TOTAL_FIELD: &str = "lessons_held_total";
pub const POOL_APPROVED_TOTAL_FIELD: &str = "pool_approved_total";
pub const POOL_PUBLISHED_TOTAL_FIELD: &str = "pool_published_total";
pub const LESSONS_ATTENDED_TOTAL_FIELD: &str = "lessons_attended_total";
pub const HIGH_MARK_TOTAL_FIELD: &str = "high_mark_total";
/// The longest run of consecutive days the student has studied, and the two
/// bookkeeping columns the write site needs to compute it: the run in progress
/// and the last day counted (both plain day numbers, midnight UTC — the same
/// boundary every other day calculation here uses).
///
/// Only the longest is a badge counter: it never comes down, which is what
/// keeps it on the ordinary `>=` rule with no second catalog shape. The other
/// two are internal state — no badge reads them, `/limits` never names them,
/// and no profile key serves them.
pub const STUDY_STREAK_LONGEST_FIELD: &str = "study_streak_longest";
pub const STUDY_STREAK_CURRENT_FIELD: &str = "study_streak_current";
pub const STUDY_STREAK_LAST_DAY_FIELD: &str = "study_streak_last_day";
/// When a lesson was first counted towards its teacher's `lessons_held_total`,
/// stamped on the `course_session` row by the first roll call taken for it.
/// Bookkeeping like the two streak columns above: no badge reads it, `/limits`
/// never names it, no profile key serves it. Its only job is the once-per-
/// session guard — a lesson is credited when it is *taken*, and the thirtieth
/// student marked must credit nothing further.
pub const LESSON_COUNTED_AT_FIELD: &str = "held_counted_at";
/// What counts as a high mark, out of `MAX_MARK`. Hardcoded next to the badge
/// thresholds rather than read from the school's grade bands: those are
/// renameable display labels, and a badge id must never depend on a value a
/// school can change at runtime — `high_mark_10` has to mean the same thing in
/// every deployment, forever.
pub const HIGH_MARK_MIN: i64 = 90;

/// Cap 1, not N: an appointment slot holds at most one live booking, so this
/// counter is really an "is it taken" flag kept in the shape every other cap
/// uses (`claim`/`release`), which is what makes rejecting or cancelling a
/// booking give the slot back.
pub const SLOT_OCCUPIED_FIELD: &str = "occupied";
/// Not caps either, and uncapped by design: how many exam questions and how
/// many homework still point at a subject. A subject may be deleted exactly
/// while both read zero, so the delete's own `WHERE` decides it — the
/// cross-table "does anything reference this?" count it replaces was already
/// stale when the delete landed.
pub const SUBJECT_QUESTION_COUNT_FIELD: &str = "exam_question_count";
pub const SUBJECT_HOMEWORK_COUNT_FIELD: &str = "homework_count";
/// How many marks an exam carries. Uncapped too, and the only counter its
/// entity carries as a field: the exam's save is a whole-row `CONTENT` write,
/// which would wipe a column the struct did not know about. That save pins it,
/// which is what makes "this exam has no marks" — the gate a kind change is
/// refused on — decided at write time rather than at read time.
pub const EXAM_RESULT_COUNT_FIELD: &str = "result_count";
/// The two columns of a reference-counter row (`kind_ref`, `slot_ref`): how
/// many rows still point at the name, and whether it has left the school's
/// list. Both `option<…>`, absent meaning zero references and in service.
pub const REF_COUNT_FIELD: &str = "count";
pub const REF_RETIRED_FIELD: &str = "retired";
/// Seats held on one menu — the meal capacity cap.
pub const MENU_SEAT_COUNT_FIELD: &str = "seats_booked";
/// Not a cap: the menu's revision, bumped by every write that can change what
/// a seat costs (a dish added, re-priced or removed, the capacity moved). A
/// booking claims its seat only at the revision it read the price at, so the
/// price frozen onto the row is one the menu really carried at that instant.
pub const MENU_VERSION_FIELD: &str = "version";

/// Not a cap either: the grade that froze a homework submission, absent while
/// the submission is still open. Every student-side write to a submission (its
/// text, its files) carries `graded_by_result = NONE` as a condition, so the
/// "not graded yet" decision and the write it licenses are one conditional
/// single-record write rather than a cross-table read a concurrent grade can
/// outrun. Set by grading, cleared by un-grading, never by the student.
pub const SUBMISSION_GRADED_FIELD: &str = "graded_by_result";
/// The condition itself, spelled once: a submission is writable exactly while
/// its grade stamp is absent.
pub const SUBMISSION_OPEN_GUARD: &str = "graded_by_result = NONE";
/// A signup list has **frozen**: the event takes registrations at all *and* it
/// started, or — for an ends_at-only event (a pure signup deadline) — that end
/// passed. A timeless event never freezes. Matched against `$now` in millis, on
/// an `event` row.
///
/// This is `Event::registration_capacity`'s `Conflict` arm re-spelled in
/// SurrealQL, because the role cascade (`service::user::set_role`) frees a demoted
/// parent's seats *inside* its transaction and cannot call Rust from there. The
/// audience conjunct is what keeps it that arm and only that arm: an event that
/// takes no registrations is refused earlier, by the `Validation` arm, so a
/// stray row on one is deleted rather than frozen — which is what the pre-fold
/// sweep did, and dropping the conjunct would silently change it. Two spellings
/// of one rule is a drift risk, so it is spelled once here and
/// `event::tests::the_sql_freeze_guard_matches_the_rust_one` fails the suite the
/// day the two disagree.
pub const REGISTRATION_FROZEN_GUARD: &str = "audience.kind = 'registration' \
     AND (starts_at ?? ends_at) != NONE AND (starts_at ?? ends_at) <= $now";

/// How hard a counter write tries before giving up, and the first backoff step
/// it sleeps between attempts (doubling, plus jitter). Contention on one record
/// is the guard working as designed, and SurrealDB answers it by aborting the
/// loser's transaction — so these two numbers are what turn "retry the
/// transaction" into a queue. Sized for one HTTP burst of racers on a single
/// parent: seven doublings from 2ms is a quarter of a second of patience.
pub const CAP_WRITE_TRIES: usize = 8;
pub const CAP_WRITE_BACKOFF_MS: u64 = 2;
