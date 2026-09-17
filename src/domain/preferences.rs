//! Per-user UI preference newtypes: color theme, interface language, and accent
//! color. All are optional on the account — `None` means the user never chose,
//! and the frontend falls back to the device preference (`prefers-color-scheme`,
//! browser language) or its own default accent.

use sqlx::Type;

use crate::constant::PALETTE_COLOR_LEN;
use crate::error::ValidationError;

/// The frontend color scheme.
///
/// Each variant stores as a bare lowercase TEXT value (`"light"` /
/// `"dark"`) — same encoding contract as [`crate::domain::role::Role`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
pub enum Theme {
    Light,
    Dark,
}

impl Theme {
    /// The wire/storage form. Must stay in lockstep with `rename_all = "lowercase"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }

    /// Parse a wire string into a theme — the inverse of [`Theme::as_str`].
    pub fn try_from_str(value: &str) -> Result<Self, ValidationError> {
        THEMES
            .into_iter()
            .find(|theme| theme.as_str() == value)
            .ok_or(ValidationError::Invalid {
                field: "theme",
                reason: "must be one of: light, dark",
            })
    }
}

pub const THEMES: [Theme; 2] = [Theme::Light, Theme::Dark];

/// The frontend interface language, as an ISO 639-1 code — the same values the
/// frontend's locale switch uses (`"tr"` Turkish, `"en"` English).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
pub enum Language {
    Tr,
    En,
}

impl Language {
    /// The wire/storage form. Must stay in lockstep with `rename_all = "lowercase"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Tr => "tr",
            Language::En => "en",
        }
    }

    /// Parse a wire string into a language — the inverse of [`Language::as_str`].
    pub fn try_from_str(value: &str) -> Result<Self, ValidationError> {
        LANGUAGES
            .into_iter()
            .find(|language| language.as_str() == value)
            .ok_or(ValidationError::Invalid {
                field: "language",
                reason: "must be one of: tr, en",
            })
    }
}

pub const LANGUAGES: [Language; 2] = [Language::Tr, Language::En];

/// The frontend accent color, a 6-digit hex with a leading `#`, stored
/// lowercase (`"#fefae0"`).
///
/// Unlike [`Theme`] and [`Language`] this is an **open** value set: any valid
/// hex passes, so the frontend can grow its palette without a backend change.
/// A plain newtype over `String`, which stores as a bare string — the same
/// `TEXT NULL` column contract the two enums encode into.
#[derive(Debug, Clone, PartialEq, Eq, Type)]
#[sqlx(transparent)]
pub struct PaletteColor(String);

impl PaletteColor {
    /// The wire/storage form: always lowercase.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse a wire string into an accent color. Mixed case is accepted (the
    /// frontend's own validator is case-insensitive) and normalized down; no
    /// trimming, so a stray space is a rejection rather than a silent fix.
    pub fn try_from_str(value: &str) -> Result<Self, ValidationError> {
        let invalid = ValidationError::Invalid {
            field: "palette_color",
            reason: "must be a hex color: # followed by exactly 6 hex digits, e.g. #fefae0",
        };
        if value.len() != PALETTE_COLOR_LEN
            || !value.starts_with('#')
            || !value[1..].chars().all(|c| c.is_ascii_hexdigit())
        {
            return Err(invalid);
        }
        Ok(Self(value.to_ascii_lowercase()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::PALETTE_COLOR_PATTERN;

    #[tokio::test]
    async fn theme_str_round_trips() {
        for theme in THEMES {
            assert_eq!(Theme::try_from_str(theme.as_str()).unwrap(), theme);
        }
        assert!(Theme::try_from_str("solarized").is_err());
        assert!(Theme::try_from_str("Light").is_err());
        assert!(Theme::try_from_str("").is_err());
    }

    #[tokio::test]
    async fn language_str_round_trips() {
        for language in LANGUAGES {
            assert_eq!(Language::try_from_str(language.as_str()).unwrap(), language);
        }
        assert!(Language::try_from_str("turkish").is_err());
        assert!(Language::try_from_str("de").is_err());
        assert!(Language::try_from_str("").is_err());
    }

    #[tokio::test]
    async fn palette_color_normalizes_and_rejects_non_hex() {
        assert_eq!(
            PaletteColor::try_from_str("#fefae0").unwrap().as_str(),
            "#fefae0"
        );
        // Mixed case is accepted (the frontend validator is case-insensitive)
        // and stored lowercase.
        assert_eq!(
            PaletteColor::try_from_str("#FEFAE0").unwrap().as_str(),
            "#fefae0"
        );
        assert!(PaletteColor::try_from_str("fefae0").is_err()); // no #
        assert!(PaletteColor::try_from_str("#fff").is_err()); // short form
        assert!(PaletteColor::try_from_str("#gggggg").is_err()); // not hex
        assert!(PaletteColor::try_from_str("#fefae0 ").is_err()); // untrimmed
        assert!(PaletteColor::try_from_str("").is_err());

        // The pattern `GET /limits` publishes must describe the rule above.
        assert_eq!(
            PALETTE_COLOR_PATTERN,
            format!("^#[0-9a-fA-F]{{{}}}$", PALETTE_COLOR_LEN - 1)
        );
    }

    /// The published pattern is a promise a client validates its own input
    /// against, so it must describe the **accept** set exactly — not the
    /// storage set. A lowercase-only pattern would have a client refuse
    /// `#FEFAE0`, which this server takes and normalizes. Length agreement
    /// (above) cannot catch that; only running both sides over the same
    /// examples can.
    #[tokio::test]
    async fn published_pattern_agrees_with_the_accept_set() {
        let pattern = regex::Regex::new(PALETTE_COLOR_PATTERN).expect("pattern compiles");
        for accepted in [
            "#fefae0", "#FEFAE0", "#FeFaE0", "#000000", "#ffffff", "#283618",
        ] {
            assert!(
                PaletteColor::try_from_str(accepted).is_ok(),
                "validator rejects {accepted}"
            );
            assert!(
                pattern.is_match(accepted),
                "published pattern rejects accepted input {accepted}"
            );
        }
        for rejected in [
            "",
            "fefae0",
            "#fff",
            "#gggggg",
            "#fefae0 ",
            " #fefae0",
            "#fefae01",
            "#fefae",
            "#fefae0\n",
            "rebeccapurple",
        ] {
            assert!(
                PaletteColor::try_from_str(rejected).is_err(),
                "validator accepts {rejected:?}"
            );
            assert!(
                !pattern.is_match(rejected),
                "published pattern accepts rejected input {rejected:?}"
            );
        }
    }

    #[test]
    fn sqlx_encodes_the_storage_form() {
        // `rename_all` keeps the stored value the bare lowercase string the
        // TEXT columns carry; the palette color stores as the string it
        // validated to. Guard that the sqlx encoding never drifts.
        let mut buf = sqlx::postgres::PgArgumentBuffer::default();
        for theme in THEMES {
            buf.clear();
            let _ = sqlx::Encode::<sqlx::Postgres>::encode_by_ref(&theme, &mut buf).unwrap();
            assert_eq!(std::str::from_utf8(&buf).unwrap(), theme.as_str());
        }
        for language in LANGUAGES {
            buf.clear();
            let _ = sqlx::Encode::<sqlx::Postgres>::encode_by_ref(&language, &mut buf).unwrap();
            assert_eq!(std::str::from_utf8(&buf).unwrap(), language.as_str());
        }
        let color = PaletteColor::try_from_str("#fefae0").unwrap();
        buf.clear();
        let _ = sqlx::Encode::<sqlx::Postgres>::encode_by_ref(&color, &mut buf).unwrap();
        assert_eq!(std::str::from_utf8(&buf).unwrap(), "#fefae0");
    }
}
