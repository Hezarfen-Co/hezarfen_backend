//! Field validators. Each newtype's `try_new` calls one of these before wrapping
//! the value, so an existing newtype is always valid (parse, don't validate).

use crate::constant::{
    COURSE_KINDS, EXAM_MODES, HOMEWORK_STATUSES, MAX_EMAIL_LEN, MAX_EXAM_ATTEMPTS,
    MAX_EXAM_DURATION_MS, MAX_MARK, MAX_PASSWORD_LEN, MAX_PHONE_DIGITS, MAX_QUESTION_POINTS,
    MAX_USERNAME_LEN, MIN_EXAM_DURATION_MS, MIN_MARK, MIN_PASSWORD_LEN, MIN_PHONE_DIGITS,
    MIN_QUESTION_POINTS, MIN_USERNAME_LEN, QUESTION_KINDS, UNLIMITED_EXAM_ATTEMPTS,
    USERNAME_SEPARATORS,
};
use crate::error::ValidationError;

pub fn validate_username(value: &str) -> Result<(), ValidationError> {
    // Length is measured on the trimmed value so padding can't defeat the
    // minimum (e.g. "a  " is a one-character name, not a three-character one).
    let value = value.trim();
    if value.is_empty() {
        return Err(ValidationError::Empty("username"));
    }
    if !value.is_ascii() {
        return Err(ValidationError::NotAscii("username"));
    }
    let len = value.chars().count();
    if len < MIN_USERNAME_LEN {
        return Err(ValidationError::TooShort {
            field: "username",
            min: MIN_USERNAME_LEN,
            got: len,
        });
    }
    if len > MAX_USERNAME_LEN {
        return Err(ValidationError::TooLong {
            field: "username",
            max: MAX_USERNAME_LEN,
            got: len,
        });
    }
    if value.chars().any(|c| c.is_ascii_uppercase()) {
        return Err(ValidationError::Invalid {
            field: "username",
            reason: "must be all lowercase",
        });
    }
    // Allowlist, not "any ASCII": control characters, spaces, quotes, and HTML
    // metacharacters inside a username enable log injection, impersonation
    // ("admin support"), and stored XSS in sloppy frontends.
    if !value
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || USERNAME_SEPARATORS.contains(&c))
    {
        return Err(ValidationError::Invalid {
            field: "username",
            reason: "may contain only lowercase letters, digits, '.', '_' and '-'",
        });
    }
    if !value.starts_with(|c: char| c.is_ascii_alphanumeric())
        || !value.ends_with(|c: char| c.is_ascii_alphanumeric())
    {
        return Err(ValidationError::Invalid {
            field: "username",
            reason: "must start and end with a letter or digit",
        });
    }
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if USERNAME_SEPARATORS.contains(&c)
            && chars
                .peek()
                .is_some_and(|n| USERNAME_SEPARATORS.contains(n))
        {
            return Err(ValidationError::Invalid {
                field: "username",
                reason: "must not contain consecutive '.', '_' or '-'",
            });
        }
    }
    Ok(())
}

pub fn validate_password(value: &str) -> Result<(), ValidationError> {
    // A password may contain anything, including whitespace, so it is not
    // trimmed — but length is still measured in characters, not UTF-8 bytes.
    let len = value.chars().count();
    if len < MIN_PASSWORD_LEN {
        return Err(ValidationError::TooShort {
            field: "password",
            min: MIN_PASSWORD_LEN,
            got: len,
        });
    }
    if len > MAX_PASSWORD_LEN {
        return Err(ValidationError::TooLong {
            field: "password",
            max: MAX_PASSWORD_LEN,
            got: len,
        });
    }
    Ok(())
}

/// A required free-text field: non-blank and within `max`.
pub fn validate_required(
    field: &'static str,
    value: &str,
    max: usize,
) -> Result<(), ValidationError> {
    if value.trim().is_empty() {
        return Err(ValidationError::Empty(field));
    }
    if value.chars().count() > max {
        return Err(ValidationError::TooLong {
            field,
            max,
            got: value.chars().count(),
        });
    }
    Ok(())
}

/// An optional free-text field: may be empty, but within `max`.
pub fn validate_optional(
    field: &'static str,
    value: &str,
    max: usize,
) -> Result<(), ValidationError> {
    if value.chars().count() > max {
        return Err(ValidationError::TooLong {
            field,
            max,
            got: value.chars().count(),
        });
    }
    Ok(())
}

/// Pragmatic email shape check: one `@` with non-empty sides, a dot inside the
/// domain, ASCII, no whitespace. Deliverability is not provable here — this only
/// rejects values that cannot be an address.
pub fn validate_email(value: &str) -> Result<(), ValidationError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(ValidationError::Empty("email"));
    }
    if !value.is_ascii() {
        return Err(ValidationError::NotAscii("email"));
    }
    if value.chars().count() > MAX_EMAIL_LEN {
        return Err(ValidationError::TooLong {
            field: "email",
            max: MAX_EMAIL_LEN,
            got: value.chars().count(),
        });
    }
    let invalid = ValidationError::Invalid {
        field: "email",
        reason: "must look like name@example.com",
    };
    if value.contains(char::is_whitespace) || value.matches('@').count() != 1 {
        return Err(invalid);
    }
    match value.split_once('@') {
        Some((local, domain))
            if !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.') =>
        {
            Ok(())
        }
        _ => Err(invalid),
    }
}

/// Phone numbers: an optional leading `+`, then digits with cosmetic spaces,
/// dashes, or parentheses; 7–15 digits total (E.164's ceiling).
pub fn validate_phone(value: &str) -> Result<(), ValidationError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(ValidationError::Empty("phone"));
    }
    let rest = value.strip_prefix('+').unwrap_or(value);
    if rest
        .chars()
        .any(|c| !c.is_ascii_digit() && !matches!(c, ' ' | '-' | '(' | ')'))
    {
        return Err(ValidationError::Invalid {
            field: "phone",
            reason: "may contain digits, spaces, dashes, parentheses, and a leading +",
        });
    }
    let digits = rest.chars().filter(char::is_ascii_digit).count();
    if !(MIN_PHONE_DIGITS..=MAX_PHONE_DIGITS).contains(&digits) {
        return Err(ValidationError::Invalid {
            field: "phone",
            reason: "must contain 7 to 15 digits",
        });
    }
    Ok(())
}

pub fn validate_course_kind(value: &str) -> Result<(), ValidationError> {
    if COURSE_KINDS.contains(&value) {
        Ok(())
    } else {
        Err(ValidationError::Invalid {
            field: "kind",
            reason: "must be one of: course, study, club",
        })
    }
}

pub fn validate_exam_mode(value: &str) -> Result<(), ValidationError> {
    if EXAM_MODES.contains(&value) {
        Ok(())
    } else {
        Err(ValidationError::Invalid {
            field: "mode",
            reason: "must be one of: sync, async, open",
        })
    }
}

pub fn validate_homework_status(value: &str) -> Result<(), ValidationError> {
    if HOMEWORK_STATUSES.contains(&value) {
        Ok(())
    } else {
        Err(ValidationError::Invalid {
            field: "status",
            reason: "must be one of: done, incomplete, missing",
        })
    }
}

pub fn validate_attempt_limit(value: i64) -> Result<(), ValidationError> {
    if value == UNLIMITED_EXAM_ATTEMPTS || (1..=MAX_EXAM_ATTEMPTS).contains(&value) {
        Ok(())
    } else {
        Err(ValidationError::Invalid {
            field: "max_attempts",
            reason: "must be between 1 and 100, or 0 for unlimited",
        })
    }
}

pub fn validate_exam_duration(value: i64) -> Result<(), ValidationError> {
    if (MIN_EXAM_DURATION_MS..=MAX_EXAM_DURATION_MS).contains(&value) {
        Ok(())
    } else {
        Err(ValidationError::Invalid {
            field: "duration_ms",
            reason: "must be between 60000 (1 minute) and 86400000 (24 hours) milliseconds",
        })
    }
}

pub fn validate_question_kind(value: &str) -> Result<(), ValidationError> {
    if QUESTION_KINDS.contains(&value) {
        Ok(())
    } else {
        Err(ValidationError::Invalid {
            field: "kind",
            reason: "must be one of: choice, text",
        })
    }
}

pub fn validate_question_points(value: i64) -> Result<(), ValidationError> {
    if (MIN_QUESTION_POINTS..=MAX_QUESTION_POINTS).contains(&value) {
        Ok(())
    } else {
        Err(ValidationError::Invalid {
            field: "points",
            reason: "must be between 1 and 100",
        })
    }
}

pub fn validate_mark(value: i64) -> Result<(), ValidationError> {
    if (MIN_MARK..=MAX_MARK).contains(&value) {
        Ok(())
    } else {
        Err(ValidationError::Invalid {
            field: "mark",
            reason: "must be between 0 and 100",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn username_rules() {
        assert!(validate_username("ali").is_ok());
        assert!(validate_username("ab").is_err()); // too short
        assert!(validate_username("   ").is_err()); // blank
        assert!(validate_username(&"x".repeat(33)).is_err()); // too long
        assert!(validate_username("naïve").is_err()); // non-ascii
        assert!(validate_username("a  ").is_err()); // padding can't defeat the minimum
        assert!(validate_username("Ali").is_err()); // uppercase
        assert!(validate_username("aLi").is_err()); // uppercase inside
        assert!(validate_username("ali9").is_ok()); // digits fine, no case
        assert!(validate_username("a-li").is_ok()); // separator inside is fine
        assert!(validate_username("a.li").is_ok());
        assert!(validate_username("a_li").is_ok());
        assert!(validate_username("a.b-c_d").is_ok()); // mixed single separators
        assert!(validate_username("-ali").is_err()); // must start alphanumeric
        assert!(validate_username("ali-").is_err()); // must end alphanumeric
        assert!(validate_username("_ali_").is_err());
        assert!(validate_username("9ali").is_ok()); // digit edges count as alphanumeric
        assert!(validate_username(" ali ").is_ok()); // trimmed before edge check
    }

    #[tokio::test]
    async fn username_interior_is_an_allowlist_not_any_ascii() {
        // ASCII control characters and metacharacters between valid edges must
        // die here: they enable log injection, spoofing, and stored XSS.
        assert!(validate_username("a b").is_err()); // interior space
        assert!(validate_username("a\nb").is_err()); // newline survives trim mid-string
        assert!(validate_username("a\x1bb").is_err()); // terminal escape
        assert!(validate_username("a\x00b").is_err()); // null byte
        assert!(validate_username("a<b>c").is_err()); // html metacharacters
        assert!(validate_username("a\"b").is_err()); // quote
        assert!(validate_username("a@b.c").is_err()); // looks like an email
        assert!(validate_username("a/b").is_err());
    }

    #[tokio::test]
    async fn username_separators_must_not_repeat() {
        assert!(validate_username("a--b").is_err());
        assert!(validate_username("a..b").is_err());
        assert!(validate_username("a__b").is_err());
        assert!(validate_username("a.-b").is_err()); // mixed pairs count too
        assert!(validate_username("a_-b").is_err());
        assert!(validate_username("a-b-c").is_ok()); // separated singles are fine
    }

    #[tokio::test]
    async fn length_is_measured_in_characters_not_bytes() {
        // "é" is two UTF-8 bytes; a 4-char password of them is under the 6-char
        // minimum and must be rejected as too short, not accepted on byte count.
        assert!(validate_password("éééé").is_err());
        // Exactly `max` multi-byte characters is allowed (bytes would overflow).
        assert!(validate_required("title", &"é".repeat(5), 5).is_ok());
        assert!(validate_required("title", &"é".repeat(6), 5).is_err());
        assert!(validate_optional("content", &"é".repeat(5), 5).is_ok());
        assert!(validate_optional("content", &"é".repeat(6), 5).is_err());
    }

    #[tokio::test]
    async fn password_rules() {
        assert!(validate_password("secret1").is_ok());
        assert!(validate_password("12345").is_err());
        assert!(validate_password(&"x".repeat(129)).is_err());
    }

    #[tokio::test]
    async fn required_rules() {
        assert!(validate_required("title", "hi", 10).is_ok());
        assert!(validate_required("title", "  ", 10).is_err());
        assert!(validate_required("title", "toolong", 3).is_err());
    }

    #[tokio::test]
    async fn optional_rules() {
        assert!(validate_optional("content", "", 10).is_ok());
        assert!(validate_optional("content", "toolong", 3).is_err());
    }

    #[tokio::test]
    async fn email_rules() {
        assert!(validate_email("ada@example.com").is_ok());
        assert!(validate_email("  ada@example.com  ").is_ok()); // trimmed before checking
        assert!(validate_email("").is_err());
        assert!(validate_email("ada").is_err()); // no @
        assert!(validate_email("@example.com").is_err()); // empty local part
        assert!(validate_email("ada@").is_err()); // empty domain
        assert!(validate_email("ada@example").is_err()); // no dot in domain
        assert!(validate_email("ada@.com").is_err()); // dot at domain edge
        assert!(validate_email("ada@example.com.").is_err());
        assert!(validate_email("a da@example.com").is_err()); // whitespace inside
        assert!(validate_email("ada@ex@ample.com").is_err()); // two @
        assert!(validate_email("adä@example.com").is_err()); // non-ascii
        assert!(validate_email(&format!("{}@example.com", "x".repeat(250))).is_err());
    }

    #[tokio::test]
    async fn phone_rules() {
        assert!(validate_phone("+90 555 123 45 67").is_ok());
        assert!(validate_phone("05551234567").is_ok());
        assert!(validate_phone("(555) 123-4567").is_ok());
        assert!(validate_phone("").is_err());
        assert!(validate_phone("123456").is_err()); // 6 digits, too few
        assert!(validate_phone("1234567890123456").is_err()); // 16 digits, too many
        assert!(validate_phone("call-me-maybe").is_err()); // letters
        assert!(validate_phone("55+5123456").is_err()); // + only allowed in front
    }

    #[tokio::test]
    async fn course_kind_rules() {
        for kind in ["course", "study", "club"] {
            assert!(validate_course_kind(kind).is_ok());
        }
        assert!(validate_course_kind("etut").is_err());
        assert!(validate_course_kind("kulup").is_err());
        assert!(validate_course_kind("").is_err());
    }

    #[tokio::test]
    async fn exam_mode_rules() {
        for mode in ["sync", "async", "open"] {
            assert!(validate_exam_mode(mode).is_ok());
        }
        assert!(validate_exam_mode("live").is_err());
        assert!(validate_exam_mode("").is_err());
    }

    #[tokio::test]
    async fn attempt_limit_rules() {
        assert!(validate_attempt_limit(UNLIMITED_EXAM_ATTEMPTS).is_ok());
        for limit in [1, 2, 50, MAX_EXAM_ATTEMPTS] {
            assert!(validate_attempt_limit(limit).is_ok());
        }
        assert!(validate_attempt_limit(-1).is_err());
        assert!(validate_attempt_limit(MAX_EXAM_ATTEMPTS + 1).is_err());
    }

    #[tokio::test]
    async fn exam_duration_rules() {
        assert!(validate_exam_duration(MIN_EXAM_DURATION_MS).is_ok());
        assert!(validate_exam_duration(90 * 60 * 1000).is_ok());
        assert!(validate_exam_duration(MAX_EXAM_DURATION_MS).is_ok());
        assert!(validate_exam_duration(MIN_EXAM_DURATION_MS - 1).is_err());
        assert!(validate_exam_duration(MAX_EXAM_DURATION_MS + 1).is_err());
        assert!(validate_exam_duration(0).is_err());
        assert!(validate_exam_duration(-1).is_err());
    }

    #[tokio::test]
    async fn question_kind_rules() {
        for kind in ["choice", "text"] {
            assert!(validate_question_kind(kind).is_ok());
        }
        assert!(validate_question_kind("essay").is_err());
        assert!(validate_question_kind("").is_err());
    }

    #[tokio::test]
    async fn question_points_rules() {
        for points in [1, 50, 100] {
            assert!(validate_question_points(points).is_ok());
        }
        assert!(validate_question_points(0).is_err());
        assert!(validate_question_points(-1).is_err());
        assert!(validate_question_points(101).is_err());
    }

    #[tokio::test]
    async fn mark_rules() {
        for mark in [0, 1, 50, 99, 100] {
            assert!(validate_mark(mark).is_ok());
        }
        assert!(validate_mark(-1).is_err());
        assert!(validate_mark(101).is_err());
    }
}
