//! Validation limits, in one place.

pub const MIN_USERNAME_LEN: usize = 3;
pub const MAX_USERNAME_LEN: usize = 32;

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

pub const MAX_EVENT_TITLE_LEN: usize = 200;
pub const MAX_EVENT_DESCRIPTION_LEN: usize = 2_000;

pub const MAX_EXAM_TITLE_LEN: usize = 200;
pub const MAX_EXAM_DESCRIPTION_LEN: usize = 2_000;

pub const MAX_COURSE_TITLE_LEN: usize = 200;
pub const MAX_COURSE_DESCRIPTION_LEN: usize = 2_000;

/// The only accepted attendance states.
pub const ATTENDANCE_STATUSES: [&str; 4] = ["present", "absent", "late", "excused"];

pub const MAX_SESSION_TOPIC_LEN: usize = 200;

/// The only accepted exam kinds. Informational metadata only — `weight` drives
/// the course average.
pub const EXAM_KINDS: [&str; 6] = ["homework", "quiz", "midterm", "final", "project", "oral"];

/// The only accepted exam modes. `sync`: everyone sits the exam inside one
/// fixed window. `async`: each student starts inside the window and gets their
/// own `duration_ms` slice of it.
pub const EXAM_MODES: [&str; 2] = ["sync", "async"];

/// Inclusive bounds for an async exam's per-student duration, milliseconds
/// (1 minute to 24 hours).
pub const MIN_EXAM_DURATION_MS: i64 = 60 * 1000;
pub const MAX_EXAM_DURATION_MS: i64 = 24 * 60 * 60 * 1000;

/// Cadence of the live exam-monitor SSE stream (`GET /exams/{id}/live/stream`).
pub const EXAM_LIVE_STREAM_INTERVAL_SECS: u64 = 2;

/// Cadence of the `state` ticks on the student exam-room WebSocket
/// (`GET /exams/{id}/attempt/ws`).
pub const EXAM_WS_TICK_SECS: u64 = 2;

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

/// Inclusive bounds for an exam's weight in the course average. The minimum of
/// 1 keeps every graded exam counted and the average's denominator non-zero.
pub const MIN_EXAM_WEIGHT: i64 = 1;
pub const MAX_EXAM_WEIGHT: i64 = 100;

pub const SESSION_DURATION_DAYS: i64 = 7;
