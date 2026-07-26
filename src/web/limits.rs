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
use crate::constant::{LANGUAGES, ROLES, THEMES};
use crate::state::AppState;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(get_limits))
}

/// Account fields: credentials, profile, and the closed role set.
#[derive(Serialize, ToSchema)]
struct UserLimits {
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
    /// How long a login session stays valid, days.
    session_duration_days: i64,
}

/// Note bodies and their attachments.
#[derive(Serialize, ToSchema)]
struct NoteLimits {
    max_title_len: usize,
    max_content_len: usize,
    /// How many files one note may carry.
    max_files: usize,
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

/// Courses, their curriculum subjects, and lesson sessions.
#[derive(Serialize, ToSchema)]
struct CourseLimits {
    max_title_len: usize,
    max_description_len: usize,
    /// The only accepted `kind` values — behaviorally identical labels.
    #[schema(example = json!(["course", "study", "club"]))]
    kinds: Vec<&'static str>,
    max_subject_name_len: usize,
    max_subject_description_len: usize,
    max_session_topic_len: usize,
    max_term_name_len: usize,
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
}

/// Every fixed limit the API enforces, grouped by the resource it applies to.
#[derive(Serialize, ToSchema)]
struct LimitsResponse {
    user: UserLimits,
    note: NoteLimits,
    file: FileLimits,
    message: MessageLimits,
    event: EventLimits,
    course: CourseLimits,
    exam: ExamLimits,
    homework: HomeworkLimits,
    question_pool: QuestionPoolLimits,
    appointment: AppointmentLimits,
    chatbot: ChatbotLimits,
    settings: SettingsLimits,
    request: RequestLimits,
    rate: RateLimits,
}

impl LimitsResponse {
    fn new(st: &AppState) -> Self {
        Self {
            user: UserLimits {
                min_username_len: MIN_USERNAME_LEN,
                max_username_len: MAX_USERNAME_LEN,
                username_separators: USERNAME_SEPARATORS.iter().map(char::to_string).collect(),
                reserved_usernames: RESERVED_USERNAMES.to_vec(),
                min_password_len: MIN_PASSWORD_LEN,
                max_password_len: MAX_PASSWORD_LEN,
                max_name_len: MAX_NAME_LEN,
                max_email_len: MAX_EMAIL_LEN,
                min_phone_digits: MIN_PHONE_DIGITS,
                max_phone_digits: MAX_PHONE_DIGITS,
                roles: ROLES.iter().map(|role| role.as_str()).collect(),
                themes: THEMES.iter().map(|theme| theme.as_str()).collect(),
                languages: LANGUAGES.iter().map(|language| language.as_str()).collect(),
                session_duration_days: SESSION_DURATION_DAYS,
            },
            note: NoteLimits {
                max_title_len: MAX_NOTE_TITLE_LEN,
                max_content_len: MAX_NOTE_CONTENT_LEN,
                max_files: MAX_NOTE_FILES,
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
            },
            request: RequestLimits {
                max_page_limit: MAX_PAGE_LIMIT,
                schedule_past_grace_ms: SCHEDULE_PAST_GRACE_MS,
                request_timeout_secs: REQUEST_TIMEOUT_SECS,
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
