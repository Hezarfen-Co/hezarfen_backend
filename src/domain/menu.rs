//! A published menu: what the kitchen serves on one calendar day in one meal
//! slot. Dishes hang off it ([`MenuDish`](crate::domain::menu_dish::MenuDish)),
//! and bookings hang off its `capacity`.
//!
//! Two things are deliberate:
//!
//! - **`date` is text** (`YYYY-MM-DD`), not a timestamp. The uniqueness rule is
//!   "one menu per day and slot", an equality test — and a midnight-in-millis
//!   day is only one school's day.
//! - **`slot` is a snapshot**, not a link into settings. Retiring a slot must
//!   never rewrite a menu already published under it.
//!
//! The queries live in [`crate::db::menu`]; the kitchen workflows (and the
//! dish-cap lock) in [`crate::service::menu`].

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{MAX_MENU_CAPACITY, MENU_TABLE, SLOT_REF_TABLE};
use crate::domain::settings::MealSlotDef;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;

/// The reference counter for one meal slot — how many menus are published under
/// that name, and whether the school has retired it (see
/// [`crate::db::cap`]). The mirror of
/// [`kind_ref`](crate::domain::exam_result::kind_ref) for exam kinds: the slot
/// is snapshotted text on the menu, so this row is the only place the two
/// tables' relationship is a single record concurrent writes can contend on.
pub(crate) fn slot_ref(slot: &str) -> RecordId {
    RecordId::new(SLOT_REF_TABLE, slot)
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct MenuId(RecordId);

impl MenuId {
    /// The one id a menu for this day and slot can have. Deterministic on
    /// purpose (the `EnrollmentId` trick): two requests publishing the same
    /// meal race on a single record instead of writing two rows, so the loser
    /// is told "already exists" by the store and answered the same 409 the
    /// pre-check gives. `date` is fixed-width `YYYY-MM-DD`, so the `_` joiner
    /// cannot be read two ways however the school spells its slots.
    pub fn for_slot(date: &MenuDate, slot: &MenuSlot) -> Self {
        Self(RecordId::new(
            MENU_TABLE,
            format!("{}_{}", date.as_str(), slot.as_str()),
        ))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(MENU_TABLE, key))
    }

    pub fn record(&self) -> RecordId {
        self.0.clone()
    }

    pub fn key(&self) -> &str {
        match &self.0.key {
            RecordIdKey::String(key) => key,
            _ => "",
        }
    }
}

/// A calendar day as `YYYY-MM-DD`. Zero-padded and fixed-width on purpose:
/// that makes the text sort chronologically, so the `?from=&to=` range filter
/// and the newest-first ordering are plain string comparisons.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, SurrealValue)]
pub struct MenuDate(String);

impl MenuDate {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        let invalid = ValidationError::Invalid {
            field: "date",
            reason: "must be a calendar day as YYYY-MM-DD",
        };
        let value = value.trim();
        let bytes = value.as_bytes();
        if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
            return Err(invalid);
        }
        // Parsed by **chrono** and compared back to the text it came from, so
        // this parser and the one that computes the serving instant
        // ([`served_at`](crate::domain::meal_booking)) agree by construction
        // rather than by two independent checks that can drift apart. They did:
        // a hand-rolled `"+1".parse::<u32>()` accepted a signed component (std
        // does), `from_ymd_opt` was then handed the normalized number and said
        // yes, and `2026-+1-01` became a *second* record id for the 1st of
        // January — its own capacity and seat counter, invisible to every
        // `date >= $from` range read ('+' sorts below '0'), and unbookable
        // besides, since chrono refuses to parse it back.
        //
        // The round trip also keeps the day a **real** one, leap years and all
        // (a 31st of February is fully actionable — seats, charges, dishes,
        // marks — with no instant any cutoff can count back from), and pins the
        // zero-padding the text ordering rests on.
        let Ok(day) = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d") else {
            return Err(invalid);
        };
        if day.format("%Y-%m-%d").to_string() != value {
            return Err(invalid);
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The instant this calendar day is over: midnight UTC opening the next
    /// one. UTC because the backend stores no school timezone, the same choice
    /// [`served_at`](crate::domain::meal_booking) makes for the serving instant.
    ///
    /// This is the whole day's deadline, not the meal's: a menu for *today* is
    /// still publishable and still bookable at any hour, whatever the slot's
    /// serving time says — that is [`MealCutoff`](crate::domain::meal_booking::MealCutoff)'s
    /// separate business. `None` for a day that cannot be parsed (a row written
    /// before `try_new` rejected impossible days).
    pub fn day_end(&self) -> Option<Timestamp> {
        let day = chrono::NaiveDate::parse_from_str(&self.0, "%Y-%m-%d").ok()?;
        Some(Timestamp::from_millis(
            day.succ_opt()?
                .and_hms_opt(0, 0, 0)?
                .and_utc()
                .timestamp_millis(),
        ))
    }
}

/// The meal slot a menu was published for, snapshotted as text. Validated
/// against the school's *current* list at write time — that list is the only
/// place slot names are defined, and this is the sole funnel into the column.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct MenuSlot(String);

impl MenuSlot {
    pub fn try_new(value: &str, allowed: &[MealSlotDef]) -> Result<Self, ValidationError> {
        let value = value.trim();
        if !allowed.iter().any(|slot| slot.get_name() == value) {
            return Err(ValidationError::Invalid {
                field: "slot",
                reason: "not one of the school's meal slots (see GET /settings)",
            });
        }
        // The slot goes verbatim into the menu's record id ([`MenuId::for_slot`]),
        // and that id is a URL path segment: a slot named `a/b` publishes a menu
        // at `/meals/menus/2026-09-14_a/b`, which no route can ever address
        // again — the menu could not be read, edited or deleted.
        // `MealSlotDef::try_new` refuses the same characters, so no school can
        // define such a slot; this is the second gate, and it is what a
        // settings row written before that rule runs into.
        if value
            .chars()
            .any(|c| matches!(c, '/' | '\\' | '?' | '#' | '%'))
        {
            return Err(ValidationError::Invalid {
                field: "slot",
                reason: "meal slot names used for menus must not contain / \\ ? # or %",
            });
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// How many students the menu may seat. `None` = uncapped, the shape a
/// course's `capacity` already uses.
pub fn validate_capacity(value: Option<i64>) -> Result<(), ValidationError> {
    match value {
        Some(capacity) if !(0..=MAX_MENU_CAPACITY).contains(&capacity) => {
            Err(ValidationError::Invalid {
                field: "capacity",
                reason: "must be between 0 and 10000",
            })
        }
        _ => Ok(()),
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct Menu {
    pub(crate) id: MenuId,
    pub(crate) date: MenuDate,
    pub(crate) slot: MenuSlot,
    pub(crate) capacity: Option<i64>,
    /// The menu's revision (see [`MENU_VERSION_FIELD`](crate::constant::MENU_VERSION_FIELD)).
    /// Absent on rows written before the column existed, which reads as
    /// revision zero — the same thing `(version ?? 0)` says in the claim's
    /// `WHERE`. The seat counter is deliberately *not* here: it is the
    /// database's to own, and a whole-row save must never carry a stale copy
    /// of it.
    pub(crate) version: Option<i64>,
    pub(crate) created_by: UserId,
    pub(crate) created_at: Timestamp,
}

impl Menu {
    pub fn get_id(&self) -> &MenuId {
        &self.id
    }

    pub fn get_date(&self) -> &MenuDate {
        &self.date
    }

    pub fn get_slot(&self) -> &MenuSlot {
        &self.slot
    }

    pub fn get_capacity(&self) -> Option<i64> {
        self.capacity
    }

    /// The revision a booking must still find on the row when it claims its
    /// seat. Absent (a pre-column row) is revision zero, exactly as the `WHERE`
    /// reads it.
    pub fn get_version(&self) -> i64 {
        self.version.unwrap_or(0)
    }

    pub fn get_created_by(&self) -> &UserId {
        &self.created_by
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use surrealdb::types::Value;

    #[test]
    fn date_accepts_only_padded_calendar_days() {
        assert!(MenuDate::try_new("2026-07-26").is_ok());
        assert!(MenuDate::try_new("2026-7-26").is_err());
        assert!(MenuDate::try_new("2026-13-01").is_err());
        assert!(MenuDate::try_new("2026-00-01").is_err());
        assert!(MenuDate::try_new("2026-01-32").is_err());
        assert!(MenuDate::try_new("26-01-02").is_err());
        assert!(MenuDate::try_new("").is_err());
        // A day that no calendar has. Not a harmless typo: the serving instant
        // a menu's cutoff counts back from cannot be computed for it, so such a
        // menu used to be bookable and cancellable with no deadline at all.
        for impossible in ["2026-02-29", "2026-04-31", "2026-06-31", "2026-11-31"] {
            assert!(
                MenuDate::try_new(impossible).is_err(),
                "{impossible} is not a real day"
            );
        }
        // …and the leap day that does exist is still a menu day.
        assert!(MenuDate::try_new("2024-02-29").is_ok());
        // Sorting the text must sort the calendar — the range filter relies on it.
        assert!(
            MenuDate::try_new("2026-01-09").unwrap() < MenuDate::try_new("2026-01-10").unwrap()
        );
        assert!(
            MenuDate::try_new("2026-09-01").unwrap() < MenuDate::try_new("2026-10-01").unwrap()
        );
    }

    /// A signed component is not a calendar day, however willingly
    /// `"+1".parse::<u32>()` reads one out of it. Accepting one minted a
    /// *second* record id for the same day — its own capacity and seat counter
    /// — that sorted below every `?from=` bound (`+` is 0x2B, `0` is 0x30) and
    /// that the serving-instant parser could not read at all.
    #[test]
    fn date_refuses_a_signed_component() {
        for signed in ["2026-+1-01", "2026-01-+1", "+026-01-01", "2026-01-+2"] {
            assert!(
                MenuDate::try_new(signed).is_err(),
                "{signed} is not a calendar day"
            );
        }
    }

    /// The drift guard: whatever this accepts must be exactly what the calendar
    /// library writes back for that day. Two parsers that merely *agree today*
    /// are what let the signed form through — one normalized the sign away
    /// before the other ever saw it.
    #[test]
    fn every_accepted_date_round_trips_through_the_calendar() {
        for candidate in [
            "2026-07-26",
            "2024-02-29",
            "2026-+1-01",
            "2026-1-1",
            "0001-01-01",
            "2026-02-29",
        ] {
            let stored = MenuDate::try_new(candidate).ok();
            let round_tripped = chrono::NaiveDate::parse_from_str(candidate, "%Y-%m-%d")
                .ok()
                .map(|day| day.format("%Y-%m-%d").to_string())
                .filter(|text| text == candidate);
            assert_eq!(
                stored.map(|date| date.as_str().to_string()),
                round_tripped,
                "{candidate} must be stored exactly as the calendar writes it, or not at all"
            );
        }
    }

    /// The day is over at midnight UTC opening the next one — not at the meal's
    /// serving hour, which is the cutoff's separate business.
    #[test]
    fn a_day_ends_at_the_next_midnight_utc() {
        let day = MenuDate::try_new("1970-01-01").unwrap();
        assert_eq!(day.day_end().unwrap().as_millis(), 86_400_000);
        // A row written before `try_new` refused impossible days has no end.
        let impossible = MenuDate::from_value(Value::String("2026-02-29".into())).unwrap();
        assert!(impossible.day_end().is_none());
    }

    #[test]
    fn slot_must_be_one_the_school_serves() {
        let allowed = vec![MealSlotDef::try_new("lunch", None).unwrap()];
        assert!(MenuSlot::try_new("lunch", &allowed).is_ok());
        assert!(MenuSlot::try_new("brunch", &allowed).is_err());
        assert!(MenuSlot::try_new("lunch", &[]).is_err());
    }

    /// The slot becomes the menu's record id, and the id becomes a URL path
    /// segment: a name carrying a separator publishes a menu at an address no
    /// route can match again. `MealSlotDef::try_new` refuses the name too, so
    /// no school can define such a slot any more — this gate is what a settings
    /// row written *before* that rule still runs into. Ordinary names — spaces
    /// and Turkish letters included — are untouched; they percent-encode into
    /// one segment as they always have.
    #[test]
    fn a_slot_name_that_would_break_the_menu_url_is_refused() {
        // The only shape that can still carry such a name: a stored slot,
        // decoded rather than constructed.
        use surrealdb::types::Value;
        let stale = |name: &str| {
            let Value::Object(mut object) =
                MealSlotDef::try_new("lunch", None).unwrap().into_value()
            else {
                panic!("a slot must encode as an object");
            };
            object.insert("name".to_string(), Value::String(name.into()));
            vec![MealSlotDef::from_value(Value::Object(object)).unwrap()]
        };
        let named = |name: &str| {
            MenuSlot::try_new(name, &stale(name))
                .map(|slot| MenuId::for_slot(&MenuDate::try_new("2026-09-14").unwrap(), &slot))
        };
        for broken in ["a/b", "a\\b", "a?b", "a#b", "a%b"] {
            assert!(named(broken).is_err(), "{broken} must not become an id");
        }
        assert_eq!(
            named("öğle yemeği").unwrap().key(),
            "2026-09-14_öğle yemeği"
        );
    }

    #[test]
    fn capacity_is_bounded_but_optional() {
        assert!(validate_capacity(None).is_ok());
        assert!(validate_capacity(Some(0)).is_ok());
        assert!(validate_capacity(Some(MAX_MENU_CAPACITY)).is_ok());
        assert!(validate_capacity(Some(-1)).is_err());
        assert!(validate_capacity(Some(MAX_MENU_CAPACITY + 1)).is_err());
    }
}
