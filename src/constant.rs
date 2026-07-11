//! Validation limits, in one place.

pub const MIN_USERNAME_LEN: usize = 3;
pub const MAX_USERNAME_LEN: usize = 32;

pub const MIN_PASSWORD_LEN: usize = 6;
pub const MAX_PASSWORD_LEN: usize = 128;

pub const MAX_NOTE_TITLE_LEN: usize = 200;
pub const MAX_NOTE_CONTENT_LEN: usize = 10_000;

pub const MAX_EVENT_TITLE_LEN: usize = 200;
pub const MAX_EVENT_DESCRIPTION_LEN: usize = 2_000;

pub const MAX_EXAM_TITLE_LEN: usize = 200;
pub const MAX_EXAM_DESCRIPTION_LEN: usize = 2_000;

/// The only accepted attendance states.
pub const ATTENDANCE_STATUSES: [&str; 4] = ["present", "absent", "late", "excused"];

/// The only accepted exam kinds — an exam is a homework or a quiz, nothing else.
pub const EXAM_KINDS: [&str; 2] = ["homework", "quiz"];

/// Inclusive bounds for an exam mark.
pub const MIN_MARK: i64 = 0;
pub const MAX_MARK: i64 = 100;

pub const SESSION_DURATION_DAYS: i64 = 7;
