//! `GET /limits` — every compile-time validation bound, served as data.
//!
//! The frontend has to enforce the same rules the newtypes do (a `maxlength`
//! on an input, a client-side check before a doomed request). Hard-coding them
//! there means two copies that silently drift apart the day one moves. This
//! endpoint is the single source: every field below reads a `crate::constant`
//! item directly, so a changed constant changes the response in the same
//! commit.
//!
//! School-adjustable policy (exam kinds, attendance statuses, grade bands, the
//! live `max_file_bytes`) lives in `GET /settings` — this route carries only
//! the fixed bounds those settings must themselves stay inside.

use axum::Json;
use axum::extract::State;
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::constant::*;
use crate::domain::badge::BADGES;
use crate::domain::preferences::{LANGUAGES, THEMES};
use crate::domain::role::ROLES;
use crate::state::AppState;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(get_limits))
}

/// Account fields: credentials, profile, and the closed role set.
#[derive(Serialize, ToSchema)]
struct UserLimits {
    /// The school slug the login and registration forms ask for, in front of
    /// the dot in the session cookie. Lowercase `a-z`, `0-9` and `-`, starting
    /// with a letter or digit.
    min_slug_len: usize,
    max_slug_len: usize,
    /// Slugs no school may take — they name the builder cookie prefix and the
    /// control database.
    reserved_slugs: Vec<&'static str>,
    /// A school's display name, set by the vendor when the school is created.
    max_school_name_len: usize,
    min_username_len: usize,
    max_username_len: usize,
    /// Separators allowed inside a username — never at the edges, never doubled.
    #[schema(example = json!([".", "_", "-"]))]
    username_separators: Vec<String>,
    /// Names `POST /auth/register` refuses (they read as staff).
    reserved_usernames: Vec<&'static str>,
    min_password_len: usize,
    max_password_len: usize,
    max_name_len: usize,
    /// The self-chosen name a user's profile is shown under.
    max_display_name_len: usize,
    /// Free text under a profile's name.
    max_bio_len: usize,
    /// How many course references a profile read embeds — a cap on the
    /// response, not on membership. The full list stays at `/courses/me`.
    max_profile_courses: usize,
    /// Same, for classes; the full list stays at `/classes/me`.
    max_profile_classes: usize,
    max_email_len: usize,
    /// Digit count, ignoring spaces and punctuation.
    min_phone_digits: usize,
    max_phone_digits: usize,
    /// Every role, lowest privilege first.
    #[schema(example = json!(["parent", "student", "teacher", "manager", "admin"]))]
    roles: Vec<&'static str>,
    /// Accepted `theme` preference values (`null` clears it).
    #[schema(example = json!(["light", "dark"]))]
    themes: Vec<&'static str>,
    /// Accepted `language` preference values (`null` clears it).
    #[schema(example = json!(["tr", "en"]))]
    languages: Vec<&'static str>,
    /// Shape a `palette_color` preference must match, as a regular expression
    /// (`null` clears it). Open on purpose — any hex accent color, not a fixed
    /// palette — so this is a pattern, not a value list like `themes`. Matches
    /// either case: the server normalizes the value to lowercase on store, so a
    /// response may differ in case from what was sent.
    #[schema(example = "^#[0-9a-fA-F]{6}$")]
    palette_color_pattern: &'static str,
    /// Length of a `palette_color`, `#` included.
    palette_color_len: usize,
    /// How long a login session stays valid, days.
    session_duration_days: i64,
}

/// The auto-earned badge catalog. A group like every other key here, so a
/// client can walk this response uniformly; the catalog itself is the one
/// field inside.
#[derive(Serialize, ToSchema)]
struct BadgeLimits {
    /// Every badge the system can auto-award, in catalog order. Hardcoded, so
    /// like everything else here it moves only with a deploy.
    catalog: Vec<BadgeLimit>,
    /// The exam mark that counts as a high one, out of `mark.max_mark`, behind
    /// the `high_mark` ladder. Published because the id alone does not say it:
    /// deliberately not the school's grade bands, which are renameable labels.
    #[schema(example = 90)]
    high_mark_min: i64,
}

/// One badge in the auto-earned catalog. The id is the whole contract: a
/// profile hands back ids and nothing else, and the label and icon behind one
/// live in the client, exactly as they do for a role or a course kind.
#[derive(Serialize, ToSchema)]
struct BadgeLimit {
    /// Stable id, never reused for another meaning. A retired badge simply
    /// stops appearing here.
    #[schema(example = "homework_submitted_10")]
    id: &'static str,
    /// The lifetime counter it reads — badges sharing one form a ladder. An
    /// API-side name, not the column it is stored in, so storage can be
    /// renamed without moving this contract.
    #[schema(example = "homework_submitted")]
    stat: &'static str,
    /// Counter value that earns it. Awards are permanent: a counter that later
    /// falls back below this does not take the badge away.
    #[schema(example = 10)]
    threshold: i64,
}

/// Note bodies and their attachments.
#[derive(Serialize, ToSchema)]
struct NoteLimits {
    max_title_len: usize,
    max_content_len: usize,
    /// How many files one note may carry.
    max_files: usize,
    /// How many files one course note may carry.
    max_course_note_files: usize,
}

/// Uploaded-file metadata, shared by every upload route.
#[derive(Serialize, ToSchema)]
struct FileLimits {
    max_name_len: usize,
    max_content_type_len: usize,
    /// Inclusive range a manager may set `max_file_bytes` to in `/settings`;
    /// the live value is on `GET /settings`, this is the hard ceiling above it.
    min_max_file_bytes: i64,
    max_max_file_bytes: i64,
    default_max_file_bytes: i64,
    /// Content types an inline image (question illustrations, option pictures,
    /// pool photos, answer drawings) may declare. Raster only — no SVG.
    #[schema(example = json!(["image/png", "image/jpeg", "image/webp", "image/gif"]))]
    image_content_types: Vec<&'static str>,
}

/// One-to-one messages.
#[derive(Serialize, ToSchema)]
struct MessageLimits {
    max_subject_len: usize,
    max_body_len: usize,
    /// The sender's optional free-text badge.
    max_label_len: usize,
}

/// Events and their registration lists.
#[derive(Serialize, ToSchema)]
struct EventLimits {
    max_title_len: usize,
    max_description_len: usize,
}

/// Courses, their curriculum subjects, lesson sessions, and the classes that
/// bulk-enroll into them.
#[derive(Serialize, ToSchema)]
struct CourseLimits {
    max_title_len: usize,
    max_description_len: usize,
    /// The only accepted `kind` values. Only `course` is taught through class
    /// instances (exams, sessions, homework); `study` (supervised study) and
    /// `club` are joined school-wide.
    #[schema(example = json!(["course", "study", "club"]))]
    kinds: Vec<&'static str>,
    max_subject_name_len: usize,
    max_subject_description_len: usize,
    max_session_topic_len: usize,
    max_term_name_len: usize,
    max_class_name_len: usize,
    /// A class's optional free-text grade label ("9", "10-A").
    max_class_grade_len: usize,
    /// Students one class may hold. Removing one frees a place.
    max_class_members: i64,
    /// Instances one class may carry. Detaching one frees a place.
    max_class_courses: i64,
    /// Inclusive bounds for an instance's weekly lesson hours (`ders_saati`).
    min_ders_saati: i64,
    max_ders_saati: i64,
    /// An academic year's name (`POST /academic-years`).
    max_academic_year_name_len: usize,
}

/// Exams, their questions, answers, and marks.
#[derive(Serialize, ToSchema)]
struct ExamLimits {
    max_title_len: usize,
    max_description_len: usize,
    /// The only accepted `mode` values; an exam with no mode is offline-graded.
    #[schema(example = json!(["sync", "async", "open"]))]
    modes: Vec<&'static str>,
    /// Inclusive bounds for `duration_ms` (1 minute – 24 hours).
    min_duration_ms: i64,
    max_duration_ms: i64,
    /// Upper bound for `max_attempts`; `unlimited_attempts` (0) means no limit.
    max_attempts: i64,
    unlimited_attempts: i64,
    /// The only accepted question `kind` values.
    #[schema(example = json!(["choice", "text"]))]
    question_kinds: Vec<&'static str>,
    max_question_text_len: usize,
    min_question_points: i64,
    max_question_points: i64,
    min_question_choices: usize,
    max_question_choices: usize,
    max_choice_text_len: usize,
    max_answer_text_len: usize,
    min_mark: i64,
    max_mark: i64,
    /// Cadence of the exam-room WebSocket's `state` frames, seconds.
    ws_tick_secs: u64,
    /// Ceiling on a `question_id` sent over the exam-room WebSocket.
    ws_max_question_id_len: usize,
}

/// Homework assignments and submissions.
#[derive(Serialize, ToSchema)]
struct HomeworkLimits {
    max_title_len: usize,
    max_description_len: usize,
    /// A submission's optional free-text note.
    max_text_len: usize,
    max_files_per_submission: usize,
    /// How many students an explicit `assigned` subset may name.
    max_assigned: usize,
    /// The only accepted teacher-set `status` values.
    #[schema(example = json!(["done", "incomplete", "missing"]))]
    statuses: Vec<&'static str>,
}

/// The school question pool and its solutions.
#[derive(Serialize, ToSchema)]
struct QuestionPoolLimits {
    max_title_len: usize,
    max_body_len: usize,
    max_solution_body_len: usize,
}

/// Teacher availability slots and the bookings on them.
#[derive(Serialize, ToSchema)]
struct AppointmentLimits {
    /// A published slot's optional note.
    max_note_len: usize,
    /// The requester's required reason.
    max_reason_len: usize,
    /// How many slots one weekly-repeating publish may expand into.
    max_slot_occurrences: usize,
}

/// The food program: menus and their dishes, dietary profiles, bookings and
/// the meal ledger. The school's own lists (`meal_slots`, `dietary_tags`) and
/// its booking cutoff are on `GET /settings`; what stands here is the fixed
/// range those must stay inside.
#[derive(Serialize, ToSchema)]
struct MealLimits {
    max_dish_name_len: usize,
    max_dish_description_len: usize,
    /// How many dishes one menu may list.
    max_dishes_per_menu: usize,
    /// Dietary tags one dish may carry.
    max_dish_tags: usize,
    /// Upper bound for a menu's optional `capacity`; absent = uncapped.
    max_menu_capacity: i64,
    /// How many times one student's seat on one menu may be taken — the first
    /// booking plus the re-bookings after a cancel. Past it, `POST
    /// /meals/menus/{id}/bookings` is a `409`: every cycle writes two
    /// permanent ledger lines, so this bounds the statement, not the mind
    /// changing. Only the canteen can free such a seat afterwards.
    max_booking_attempts: i64,
    /// Dietary tags one student's profile may carry, and its kitchen note.
    max_dietary_tags: usize,
    max_dietary_note_len: usize,
    /// Money is **minor units** as an integer everywhere — never a
    /// decimal, never a float. These cap one dish and one ledger line.
    max_dish_price_minor: i64,
    max_ledger_amount_minor: i64,
    /// How the money moved, and why.
    max_ledger_method_len: usize,
    max_ledger_note_len: usize,
    /// Ceiling on the `/settings` knob that closes booking (and cancelling)
    /// ahead of a meal, minutes; the live value is on `GET /settings`.
    max_cancel_cutoff_minutes: i64,
    /// Ceiling on a meal slot's `serving_minute` — minutes past midnight
    /// **UTC** on the menu's date, which is what the cutoff counts back from.
    /// `1439` = 23:59 UTC. There is no school timezone: staff enter UTC.
    max_serving_minute: i64,
    /// The only accepted booking `status` values.
    #[schema(example = json!(["booked", "cancelled"]))]
    booking_statuses: Vec<&'static str>,
    /// The only accepted meal-attendance `status` values — deliberately not
    /// the school's lesson attendance statuses.
    #[schema(example = json!(["served", "missed"]))]
    attendance_statuses: Vec<&'static str>,
    /// The only accepted ledger `kind` values.
    #[schema(example = json!(["charge", "credit", "reversal"]))]
    ledger_kinds: Vec<&'static str>,
}

/// School payments: fee plans, their assignment, and the payment ledger. Money
/// is **minor units** as an integer, and one ledger line is capped by
/// the same `max_ledger_amount_minor` the meal ledger publishes.
#[derive(Serialize, ToSchema)]
struct PaymentLimits {
    max_plan_name_len: usize,
    /// How many installments one fee plan may schedule.
    max_plan_installments: usize,
    /// How many students one bulk assignment may name.
    max_assign_students: usize,
    /// How many charge lines one bulk assignment may append — the students it
    /// names times the plan's installments. A request past this is a `400`
    /// telling the caller to split the batch; nothing is written.
    max_assign_writes: usize,
    /// How many lines may be applied to one ledger line: the payments under a
    /// charge, the refunds under a payment, and the reversals among them. Past
    /// it, a further payment or refund against that line is a `409`.
    max_applied_lines: usize,
    /// Ceiling on the optional `request_key` that makes a credit or a refund
    /// retry-safe; charset `[A-Za-z0-9-]` (no `_`), at least one character.
    max_request_key_len: usize,
    /// The only accepted payment-ledger `kind` values.
    #[schema(example = json!(["charge", "credit", "reversal", "refund"]))]
    ledger_kinds: Vec<&'static str>,
}

/// The AI chatbot. Each range bounds the matching `/settings` knob; the live
/// values are on `GET /settings`.
#[derive(Serialize, ToSchema)]
struct ChatbotLimits {
    /// Hard ceiling on a message, above which no school setting can reach.
    max_message_len: usize,
    /// A thread's display name. Threads are named by hand or left untitled —
    /// nothing auto-titles one.
    max_thread_title_len: usize,
    min_max_message_len: i64,
    max_max_message_len: i64,
    default_max_message_len: i64,
    min_history_turns: i64,
    max_history_turns: i64,
    default_history_turns: i64,
    min_max_threads: i64,
    max_max_threads: i64,
    default_max_threads: i64,
}

/// The RAG nest: a tier of its own — every message spends a retrieval over the
/// whole course-note corpus before it generates anything — plus the school
/// knobs it reuses from the chatbot. One `rag.chat` message is a chat message
/// in every way the newtypes check, so message length, thread titles, history
/// depth and the thread cap come from the same `/settings` fields; their fixed
/// ranges are published here a second time so a RAG client need not read the
/// chatbot group to bound its input.
#[derive(Serialize, ToSchema)]
struct RagLimits {
    /// Hard ceiling on the `(class, course)` scope pairs one ask may carry. The
    /// corpus is routed by the pair, so this bounds what one question can ask
    /// a retrieval to sweep.
    max_scope_pairs: usize,
    /// Hard ceiling on the practice questions one `POST /rag/questions` call
    /// may ask for. The caller waits on the generation, so this bounds one
    /// model round trip rather than a corpus sweep.
    max_questions: u32,
    /// Hard ceiling on the citations one answer may store in its message row.
    max_citations: usize,
    /// Pages one citation may span. A citation names a page range, and an
    /// answer that could name every page of every document would be a copy of
    /// the corpus, not a citation.
    max_citation_pages: usize,
    /// The chatbot's message bound, reused.
    max_message_len: usize,
    /// The chatbot's thread-title bound, reused.
    max_thread_title_len: usize,
    /// The school-owned message-length setting's fixed range, reused.
    min_max_message_len: i64,
    max_max_message_len: i64,
    default_max_message_len: i64,
    /// The school-owned history-depth setting's fixed range, reused.
    min_history_turns: i64,
    max_history_turns: i64,
    default_history_turns: i64,
    /// The school-owned thread-cap setting's fixed range, reused.
    min_max_threads: i64,
    max_max_threads: i64,
    default_max_threads: i64,
}

/// What a finished pomodoro stint must be to *count* — to move the lifetime
/// counters the badges and the study streak read. Not a validation bound:
/// `POST /pomodoro/finish` never refuses a stint over these, it records it and
/// answers `counted: false`. Published so a client can say what a stint is
/// worth (and what is left of today's quota) instead of guessing why a badge
/// did not arrive.
#[derive(Serialize, ToSchema)]
struct PomodoroLimits {
    /// Shortest stint that counts, milliseconds — a stint of exactly this long
    /// counts. Well under a conventional 25-minute pomodoro: breaking off early
    /// is still studying. Its job is only to price a scripted round-trip out.
    #[schema(example = 300000)]
    min_counted_ms: i64,
    /// How many stints count per UTC day. Every further one that day is
    /// recorded and listed as usual, and counts nothing.
    #[schema(example = 16)]
    max_counted_per_day: i64,
    /// How long a stint's optional student-given label may be — the student's
    /// own name for what the stint is for, given at `POST /pomodoro/start`.
    max_label_len: usize,
}

/// A collaborative whiteboard: its title, its roster, and the two growth caps
/// that decide when a canvas must be cleared and when a board is finished.
#[derive(Serialize, ToSchema)]
struct BoardLimits {
    max_title_len: usize,
    /// People the creator may name onto one board. Every participant draws, so
    /// this also bounds a room's writer count.
    max_participants: usize,
    /// Ceiling on one stroke's serialized payload, bytes.
    max_stroke_payload_len: usize,
    /// The *live* canvas cap: strokes since the last clear. Hitting it is
    /// recoverable — the creator clears, the canvas empties, the history is
    /// kept, and drawing resumes.
    max_epoch_strokes: i64,
    /// The *lifetime* storage cap, which never resets because a clear keeps its
    /// history. Hitting it makes the board permanently read-only: it stays
    /// fully readable and replayable, and a new board must be opened.
    max_board_strokes: i64,
    /// Boards one creating user may hold. Deleting a board frees a seat.
    max_boards_per_creator: i64,
    /// The accepted stroke kinds. `clear` is a marker that ends an epoch, not a
    /// deletion.
    #[schema(example = json!(["stroke", "clear"]))]
    stroke_kinds: Vec<&'static str>,
    /// Cadence of the board-room WebSocket's keepalive ticks, seconds.
    ws_tick_secs: u64,
    /// Ceiling on a `board_id` sent over the board-room WebSocket.
    ws_max_board_id_len: usize,
}

/// Bounds on the school-editable lists in `PATCH /settings`.
#[derive(Serialize, ToSchema)]
struct SettingsLimits {
    /// Entry count for the exam-kind and attendance-status lists.
    max_list_len: usize,
    /// Per-entry characters in those lists.
    max_item_len: usize,
    min_exam_kind_weight: i64,
    max_exam_kind_weight: i64,
    max_grade_bands: usize,
    max_grade_label_len: usize,
    /// Attendance statuses no school may remove.
    #[schema(example = json!(["present", "absent", "late", "excused"]))]
    required_attendance_statuses: Vec<&'static str>,
}

/// The request budgets a caller is held to before it starts getting `429`s
/// with a `Retry-After`. Unlike everything else here these are **deployment**
/// configuration, not compile-time constants — they come from environment
/// variables, so the numbers are this server's live values rather than fixed
/// ones. `0` means the tier is switched off.
#[derive(Serialize, ToSchema)]
struct RateLimits {
    /// Fixed window every tier is counted over, seconds.
    window_secs: u64,
    /// Per-IP budget for `/auth/login` and `/auth/register`.
    #[schema(example = 10)]
    auth_per_minute: u32,
    /// Per-IP budget across the whole API, this endpoint included.
    #[schema(example = 300)]
    api_per_minute: u32,
    /// Per-*user* budget for sending chatbot messages (keyed by account, not
    /// address, so it survives a changing IP).
    #[schema(example = 20)]
    chatbot_per_minute: u32,
    /// Per-*user* budget for sending RAG messages, keyed the same way.
    #[schema(example = 6)]
    rag_per_minute: u32,
}

/// Rules that hold across every endpoint.
#[derive(Serialize, ToSchema)]
struct RequestLimits {
    /// Upper bound on a list endpoint's `?limit=`. Omitting it returns
    /// everything remaining.
    max_page_limit: i64,
    /// How far in the past a submitted schedule instant may lie before it is
    /// refused as backdated — the grace that absorbs clock skew.
    schedule_past_grace_ms: i64,
    /// Ceiling on how long one request may take before it is aborted.
    request_timeout_secs: u64,
    /// Longest `x-request-id` header the server keeps. A longer one — or one
    /// carrying anything outside `[A-Za-z0-9._-]` — is replaced by a
    /// server-minted UUID, so the caller loses correlation on its own id.
    max_request_id_len: usize,
}

/// Every fixed limit the API enforces, grouped by the resource it applies to.
#[derive(Serialize, ToSchema)]
struct LimitsResponse {
    user: UserLimits,
    badges: BadgeLimits,
    note: NoteLimits,
    file: FileLimits,
    message: MessageLimits,
    event: EventLimits,
    course: CourseLimits,
    exam: ExamLimits,
    homework: HomeworkLimits,
    question_pool: QuestionPoolLimits,
    appointment: AppointmentLimits,
    meal: MealLimits,
    payment: PaymentLimits,
    chatbot: ChatbotLimits,
    rag: RagLimits,
    pomodoro: PomodoroLimits,
    board: BoardLimits,
    settings: SettingsLimits,
    request: RequestLimits,
    rate: RateLimits,
}

impl LimitsResponse {
    fn new(st: &AppState) -> Self {
        Self {
            user: UserLimits {
                min_slug_len: MIN_SLUG_LEN,
                max_slug_len: MAX_SLUG_LEN,
                reserved_slugs: crate::tenant::RESERVED_SLUGS.to_vec(),
                max_school_name_len: MAX_SCHOOL_NAME_LEN,
                min_username_len: MIN_USERNAME_LEN,
                max_username_len: MAX_USERNAME_LEN,
                username_separators: USERNAME_SEPARATORS.iter().map(char::to_string).collect(),
                reserved_usernames: RESERVED_USERNAMES.to_vec(),
                min_password_len: MIN_PASSWORD_LEN,
                max_password_len: MAX_PASSWORD_LEN,
                max_name_len: MAX_NAME_LEN,
                max_display_name_len: MAX_DISPLAY_NAME_LEN,
                max_bio_len: MAX_BIO_LEN,
                max_profile_courses: MAX_PROFILE_COURSES,
                max_profile_classes: MAX_PROFILE_CLASSES,
                max_email_len: MAX_EMAIL_LEN,
                min_phone_digits: MIN_PHONE_DIGITS,
                max_phone_digits: MAX_PHONE_DIGITS,
                roles: ROLES.iter().map(|role| role.as_str()).collect(),
                themes: THEMES.iter().map(|theme| theme.as_str()).collect(),
                languages: LANGUAGES.iter().map(|language| language.as_str()).collect(),
                palette_color_pattern: PALETTE_COLOR_PATTERN,
                palette_color_len: PALETTE_COLOR_LEN,
                session_duration_days: SESSION_DURATION_DAYS,
            },
            badges: BadgeLimits {
                catalog: BADGES
                    .iter()
                    .map(|(id, stat, threshold)| BadgeLimit {
                        id,
                        stat: stat.as_str(),
                        threshold: *threshold,
                    })
                    .collect(),
                high_mark_min: HIGH_MARK_MIN,
            },
            note: NoteLimits {
                max_title_len: MAX_NOTE_TITLE_LEN,
                max_content_len: MAX_NOTE_CONTENT_LEN,
                max_files: MAX_NOTE_FILES,
                max_course_note_files: MAX_COURSE_NOTE_FILES,
            },
            file: FileLimits {
                max_name_len: MAX_FILE_NAME_LEN,
                max_content_type_len: MAX_FILE_CONTENT_TYPE_LEN,
                min_max_file_bytes: MIN_MAX_FILE_BYTES,
                max_max_file_bytes: MAX_MAX_FILE_BYTES,
                default_max_file_bytes: DEFAULT_MAX_FILE_BYTES,
                image_content_types: QUESTION_IMAGE_CONTENT_TYPES.to_vec(),
            },
            message: MessageLimits {
                max_subject_len: MAX_MESSAGE_SUBJECT_LEN,
                max_body_len: MAX_MESSAGE_BODY_LEN,
                max_label_len: MAX_MESSAGE_LABEL_LEN,
            },
            event: EventLimits {
                max_title_len: MAX_EVENT_TITLE_LEN,
                max_description_len: MAX_EVENT_DESCRIPTION_LEN,
            },
            course: CourseLimits {
                max_title_len: MAX_COURSE_TITLE_LEN,
                max_description_len: MAX_COURSE_DESCRIPTION_LEN,
                kinds: COURSE_KINDS.to_vec(),
                max_subject_name_len: MAX_SUBJECT_NAME_LEN,
                max_subject_description_len: MAX_SUBJECT_DESCRIPTION_LEN,
                max_session_topic_len: MAX_SESSION_TOPIC_LEN,
                max_term_name_len: MAX_TERM_NAME_LEN,
                max_class_name_len: MAX_CLASS_NAME_LEN,
                max_class_grade_len: MAX_CLASS_GRADE_LEN,
                max_class_members: MAX_CLASS_MEMBERS,
                max_class_courses: MAX_CLASS_COURSES,
                min_ders_saati: MIN_DERS_SAATI,
                max_ders_saati: MAX_DERS_SAATI,
                max_academic_year_name_len: MAX_ACADEMIC_YEAR_NAME_LEN,
            },
            exam: ExamLimits {
                max_title_len: MAX_EXAM_TITLE_LEN,
                max_description_len: MAX_EXAM_DESCRIPTION_LEN,
                modes: EXAM_MODES.to_vec(),
                min_duration_ms: MIN_EXAM_DURATION_MS,
                max_duration_ms: MAX_EXAM_DURATION_MS,
                max_attempts: MAX_EXAM_ATTEMPTS,
                unlimited_attempts: UNLIMITED_EXAM_ATTEMPTS,
                question_kinds: QUESTION_KINDS.to_vec(),
                max_question_text_len: MAX_QUESTION_TEXT_LEN,
                min_question_points: MIN_QUESTION_POINTS,
                max_question_points: MAX_QUESTION_POINTS,
                min_question_choices: MIN_QUESTION_CHOICES,
                max_question_choices: MAX_QUESTION_CHOICES,
                max_choice_text_len: MAX_CHOICE_TEXT_LEN,
                max_answer_text_len: MAX_ANSWER_TEXT_LEN,
                min_mark: MIN_MARK,
                max_mark: MAX_MARK,
                ws_tick_secs: EXAM_WS_TICK_SECS,
                ws_max_question_id_len: MAX_QUESTION_ID_LEN,
            },
            homework: HomeworkLimits {
                max_title_len: MAX_HOMEWORK_TITLE_LEN,
                max_description_len: MAX_HOMEWORK_DESCRIPTION_LEN,
                max_text_len: MAX_HOMEWORK_TEXT_LEN,
                max_files_per_submission: MAX_HOMEWORK_FILES_PER_SUBMISSION,
                max_assigned: MAX_HOMEWORK_ASSIGNED,
                statuses: HOMEWORK_STATUSES.to_vec(),
            },
            question_pool: QuestionPoolLimits {
                max_title_len: MAX_POOL_QUESTION_TITLE_LEN,
                max_body_len: MAX_POOL_QUESTION_BODY_LEN,
                max_solution_body_len: MAX_SOLUTION_BODY_LEN,
            },
            appointment: AppointmentLimits {
                max_note_len: MAX_APPOINTMENT_NOTE_LEN,
                max_reason_len: MAX_APPOINTMENT_REASON_LEN,
                max_slot_occurrences: MAX_SLOT_OCCURRENCES,
            },
            meal: MealLimits {
                max_dish_name_len: MAX_DISH_NAME_LEN,
                max_dish_description_len: MAX_DISH_DESCRIPTION_LEN,
                max_dishes_per_menu: MAX_DISHES_PER_MENU,
                max_dish_tags: MAX_DISH_TAGS,
                max_menu_capacity: MAX_MENU_CAPACITY,
                max_booking_attempts: MAX_MEAL_BOOKING_ATTEMPTS,
                max_dietary_tags: MAX_DIETARY_TAGS,
                max_dietary_note_len: MAX_DIETARY_NOTE_LEN,
                max_dish_price_minor: MAX_DISH_PRICE_MINOR,
                max_ledger_amount_minor: MAX_LEDGER_AMOUNT_MINOR,
                max_ledger_method_len: MAX_LEDGER_METHOD_LEN,
                max_ledger_note_len: MAX_LEDGER_NOTE_LEN,
                max_cancel_cutoff_minutes: MAX_MEAL_CANCEL_CUTOFF_MINUTES,
                max_serving_minute: MAX_MEAL_SERVING_MINUTE,
                booking_statuses: MEAL_BOOKING_STATUSES.to_vec(),
                attendance_statuses: MEAL_ATTENDANCE_STATUSES.to_vec(),
                ledger_kinds: MEAL_LEDGER_KINDS.to_vec(),
            },
            payment: PaymentLimits {
                max_plan_name_len: MAX_FEE_PLAN_NAME_LEN,
                max_plan_installments: MAX_FEE_PLAN_INSTALLMENTS,
                max_assign_students: MAX_FEE_PLAN_ASSIGN_STUDENTS,
                max_assign_writes: MAX_FEE_PLAN_ASSIGN_WRITES,
                max_applied_lines: MAX_LEDGER_APPLIED_LINES,
                max_request_key_len: MAX_PAYMENT_REQUEST_KEY_LEN,
                ledger_kinds: PAYMENT_LEDGER_KINDS.to_vec(),
            },
            chatbot: ChatbotLimits {
                max_message_len: MAX_CHATBOT_MESSAGE_LEN,
                max_thread_title_len: MAX_CHATBOT_THREAD_TITLE_LEN,
                min_max_message_len: MIN_MAX_CHATBOT_MESSAGE_LEN,
                max_max_message_len: MAX_MAX_CHATBOT_MESSAGE_LEN,
                default_max_message_len: DEFAULT_MAX_CHATBOT_MESSAGE_LEN,
                min_history_turns: MIN_CHATBOT_HISTORY_TURNS,
                max_history_turns: MAX_CHATBOT_HISTORY_TURNS,
                default_history_turns: DEFAULT_CHATBOT_HISTORY_TURNS,
                min_max_threads: MIN_MAX_CHATBOT_THREADS,
                max_max_threads: MAX_MAX_CHATBOT_THREADS,
                default_max_threads: DEFAULT_MAX_CHATBOT_THREADS,
            },
            rag: RagLimits {
                max_scope_pairs: MAX_RAG_SCOPE_PAIRS,
                max_questions: MAX_RAG_QUESTIONS,
                max_citations: MAX_RAG_CITATIONS,
                max_citation_pages: MAX_RAG_CITATION_PAGES,
                max_message_len: MAX_CHATBOT_MESSAGE_LEN,
                max_thread_title_len: MAX_CHATBOT_THREAD_TITLE_LEN,
                min_max_message_len: MIN_MAX_CHATBOT_MESSAGE_LEN,
                max_max_message_len: MAX_MAX_CHATBOT_MESSAGE_LEN,
                default_max_message_len: DEFAULT_MAX_CHATBOT_MESSAGE_LEN,
                min_history_turns: MIN_CHATBOT_HISTORY_TURNS,
                max_history_turns: MAX_CHATBOT_HISTORY_TURNS,
                default_history_turns: DEFAULT_CHATBOT_HISTORY_TURNS,
                min_max_threads: MIN_MAX_CHATBOT_THREADS,
                max_max_threads: MAX_MAX_CHATBOT_THREADS,
                default_max_threads: DEFAULT_MAX_CHATBOT_THREADS,
            },
            pomodoro: PomodoroLimits {
                min_counted_ms: MIN_COUNTED_POMODORO_MS,
                max_counted_per_day: MAX_COUNTED_POMODORO_PER_DAY,
                max_label_len: MAX_POMODORO_LABEL_LEN,
            },
            board: BoardLimits {
                max_title_len: MAX_BOARD_TITLE_LEN,
                max_participants: MAX_BOARD_PARTICIPANTS,
                max_stroke_payload_len: MAX_STROKE_PAYLOAD_LEN,
                max_epoch_strokes: MAX_EPOCH_STROKES,
                max_board_strokes: MAX_BOARD_STROKES,
                max_boards_per_creator: MAX_BOARDS_PER_CREATOR,
                stroke_kinds: BOARD_STROKE_KINDS.to_vec(),
                ws_tick_secs: BOARD_WS_TICK_SECS,
                ws_max_board_id_len: MAX_BOARD_ID_LEN,
            },
            settings: SettingsLimits {
                max_list_len: MAX_SETTINGS_LIST_LEN,
                max_item_len: MAX_SETTINGS_ITEM_LEN,
                min_exam_kind_weight: MIN_EXAM_KIND_WEIGHT,
                max_exam_kind_weight: MAX_EXAM_KIND_WEIGHT,
                max_grade_bands: MAX_GRADE_BANDS,
                max_grade_label_len: MAX_GRADE_LABEL_LEN,
                required_attendance_statuses: DEFAULT_ATTENDANCE_STATUSES.to_vec(),
            },
            rate: RateLimits {
                window_secs: 60,
                auth_per_minute: st.rate_limit.auth_per_minute,
                api_per_minute: st.rate_limit.api_per_minute,
                chatbot_per_minute: st.chatbot_limit.max_per_window(),
                rag_per_minute: st.rag_limit.max_per_window(),
            },
            request: RequestLimits {
                max_page_limit: MAX_PAGE_LIMIT,
                schedule_past_grace_ms: SCHEDULE_PAST_GRACE_MS,
                request_timeout_secs: REQUEST_TIMEOUT_SECS,
                max_request_id_len: MAX_REQUEST_ID_LEN,
            },
        }
    }
}

/// Every fixed validation bound the API enforces. Unauthenticated: the
/// registration and login forms need the username and password bounds before a
/// session exists, and none of these values are secrets — they are the same
/// rules a 400 would spell out.
///
/// The values are compile-time constants, so the response only ever changes
/// with a deploy — cache it for the session rather than re-fetching per form.
/// School-adjustable policy (exam kinds, attendance statuses, grade bands, the
/// live `max_file_bytes` and chatbot knobs) is on `GET /settings` instead; what
/// appears here for those is the fixed range a manager may set them within.
#[utoipa::path(
    get,
    path = "/limits",
    tag = "meta",
    responses((status = 200, description = "Every fixed validation bound", body = LimitsResponse)),
)]
async fn get_limits(State(st): State<AppState>) -> Json<LimitsResponse> {
    Json(LimitsResponse::new(&st))
}
