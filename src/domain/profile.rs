//! Personal-info newtypes shared by every account role — student, teacher,
//! manager, and admin records all carry the same optional contact fields.

use surrealdb::types::SurrealValue;

use crate::constant::MAX_NAME_LEN;
use crate::error::ValidationError;
use crate::validate::{validate_email, validate_phone, validate_required};

/// A person's given or family name. One type serves both fields — the `field`
/// tag only steers the error message ("name …" vs "surname …"). Stored trimmed;
/// unicode is welcome (names are not ASCII).
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct PersonName(String);

impl PersonName {
    pub fn try_new(field: &'static str, value: &str) -> Result<Self, ValidationError> {
        validate_required(field, value, MAX_NAME_LEN)?;
        Ok(Self(value.trim().to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A plausibly-shaped email address (see [`validate_email`]). Stored trimmed.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct Email(String);

impl Email {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_email(value)?;
        Ok(Self(value.trim().to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A phone number: optional leading `+`, 7–15 digits, cosmetic separators
/// allowed (see [`validate_phone`]). Stored trimmed, separators kept as given.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct Phone(String);

impl Phone {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_phone(value)?;
        Ok(Self(value.trim().to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A birth date, canonicalized to `YYYY-MM-DD`. Construction parses the input
/// as a real calendar date (no 2026-02-30) and refuses future dates.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct BirthDate(String);

impl BirthDate {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        let date = chrono::NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d").map_err(|_| {
            ValidationError::Invalid {
                field: "birth_date",
                reason: "must be a calendar date in YYYY-MM-DD form",
            }
        })?;
        if date > chrono::Utc::now().date_naive() {
            return Err(ValidationError::Invalid {
                field: "birth_date",
                reason: "must not be in the future",
            });
        }
        Ok(Self(date.format("%Y-%m-%d").to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn person_name_trims_and_validates() {
        assert_eq!(
            PersonName::try_new("name", "  Ada  ").unwrap().as_str(),
            "Ada"
        );
        // Unicode names must pass — this is not the ASCII-only username rule.
        assert_eq!(
            PersonName::try_new("surname", "Gümüş").unwrap().as_str(),
            "Gümüş"
        );
        assert!(PersonName::try_new("name", "   ").is_err());
        assert!(PersonName::try_new("name", &"x".repeat(101)).is_err());
    }

    #[tokio::test]
    async fn name_errors_carry_the_field_tag() {
        // One newtype serves two fields; the message must still say which one.
        let err = PersonName::try_new("surname", "").unwrap_err();
        assert!(err.to_string().contains("surname"));
    }

    #[tokio::test]
    async fn email_and_phone_store_trimmed() {
        assert_eq!(
            Email::try_new(" ada@example.com ").unwrap().as_str(),
            "ada@example.com"
        );
        assert_eq!(
            Phone::try_new(" +90 555 123 45 67 ").unwrap().as_str(),
            "+90 555 123 45 67"
        );
    }

    #[tokio::test]
    async fn birth_date_canonicalizes() {
        assert_eq!(
            BirthDate::try_new("1990-1-2").unwrap().as_str(),
            "1990-01-02"
        );
        assert!(BirthDate::try_new("1990-02-30").is_err()); // not a real date
        assert!(BirthDate::try_new("02/01/1990").is_err()); // wrong format
        assert!(BirthDate::try_new("9999-01-01").is_err()); // future
        assert!(BirthDate::try_new("").is_err());
    }
}
