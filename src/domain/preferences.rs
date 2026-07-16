//! Per-user UI preference newtypes: color theme and interface language. Both
//! are optional on the account — `None` means the user never chose, and the
//! frontend falls back to the device preference (`prefers-color-scheme`,
//! browser language).

use surrealdb::types::SurrealValue;

use crate::error::ValidationError;

/// The frontend color scheme.
///
/// `#[surreal(untagged, rename_all = "lowercase")]` stores each variant as a
/// bare lowercase string (`"light"` / `"dark"`) — same encoding contract as
/// [`crate::domain::role::Role`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, SurrealValue)]
#[surreal(untagged, rename_all = "lowercase")]
pub enum Theme {
    Light,
    Dark,
}

impl Theme {
    pub const ALL: [Theme; 2] = [Theme::Light, Theme::Dark];

    /// The wire/storage form. Must stay in lockstep with `rename_all = "lowercase"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }

    /// Parse a wire string into a theme — the inverse of [`Theme::as_str`].
    pub fn try_from_str(value: &str) -> Result<Self, ValidationError> {
        Self::ALL
            .into_iter()
            .find(|theme| theme.as_str() == value)
            .ok_or(ValidationError::Invalid {
                field: "theme",
                reason: "must be one of: light, dark",
            })
    }
}

/// The frontend interface language, as an ISO 639-1 code — the same values the
/// frontend's locale switch uses (`"tr"` Turkish, `"en"` English).
#[derive(Debug, Clone, Copy, PartialEq, Eq, SurrealValue)]
#[surreal(untagged, rename_all = "lowercase")]
pub enum Language {
    Tr,
    En,
}

impl Language {
    pub const ALL: [Language; 2] = [Language::Tr, Language::En];

    /// The wire/storage form. Must stay in lockstep with `rename_all = "lowercase"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Tr => "tr",
            Language::En => "en",
        }
    }

    /// Parse a wire string into a language — the inverse of [`Language::as_str`].
    pub fn try_from_str(value: &str) -> Result<Self, ValidationError> {
        Self::ALL
            .into_iter()
            .find(|language| language.as_str() == value)
            .ok_or(ValidationError::Invalid {
                field: "language",
                reason: "must be one of: tr, en",
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use surrealdb::types::Value;

    #[tokio::test]
    async fn theme_str_round_trips() {
        for theme in Theme::ALL {
            assert_eq!(Theme::try_from_str(theme.as_str()).unwrap(), theme);
        }
        assert!(Theme::try_from_str("solarized").is_err());
        assert!(Theme::try_from_str("Light").is_err());
        assert!(Theme::try_from_str("").is_err());
    }

    #[tokio::test]
    async fn language_str_round_trips() {
        for language in Language::ALL {
            assert_eq!(Language::try_from_str(language.as_str()).unwrap(), language);
        }
        assert!(Language::try_from_str("turkish").is_err());
        assert!(Language::try_from_str("de").is_err());
        assert!(Language::try_from_str("").is_err());
    }

    #[tokio::test]
    async fn surreal_values_are_plain_strings() {
        // `untagged` keeps the stored value a bare string so the `option<string>`
        // columns accept it. Guard that the encoding never regresses.
        for theme in Theme::ALL {
            let value = theme.into_value();
            assert_eq!(value, Value::String(theme.as_str().to_string()));
            assert_eq!(Theme::from_value(value).unwrap(), theme);
        }
        for language in Language::ALL {
            let value = language.into_value();
            assert_eq!(value, Value::String(language.as_str().to_string()));
            assert_eq!(Language::from_value(value).unwrap(), language);
        }
    }
}
