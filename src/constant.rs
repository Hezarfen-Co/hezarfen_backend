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

/// The only accepted attendance states.
pub const ATTENDANCE_STATUSES: [&str; 4] = ["present", "absent", "late", "excused"];

/// The only accepted exam kinds — an exam is a homework or a quiz, nothing else.
pub const EXAM_KINDS: [&str; 2] = ["homework", "quiz"];

/// Inclusive bounds for an exam mark.
pub const MIN_MARK: i64 = 0;
pub const MAX_MARK: i64 = 100;

pub const SESSION_DURATION_DAYS: i64 = 7;
