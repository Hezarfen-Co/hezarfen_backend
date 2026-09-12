//! One dish on a published [`Menu`](crate::domain::menu::Menu): what it is,
//! what it costs, and which dietary tags it carries.
//!
//! Money is **minor units** (kuruş) as `i64` — never a float, never a decimal,
//! at any layer. `tags` are validated against the school's `dietary_tags`
//! list, the same contract a menu's `slot` has with `meal_slots`.
//!
//! The queries live in [`crate::db::menu_dish`]; the dish-cap gate and the
//! write lock in [`crate::service::menu`].

use sqlx::Type;
use uuid::Uuid;

use crate::constant::{
    MAX_DISH_DESCRIPTION_LEN, MAX_DISH_NAME_LEN, MAX_DISH_PRICE_MINOR, MAX_DISH_TAGS,
};
use crate::domain::menu::MenuId;
use crate::domain::monotonic_id::next_uuid;
use crate::domain::timestamp::Timestamp;
use crate::error::ValidationError;
use crate::validate::{validate_optional, validate_required};

/// Typed menu-dish row id. A UUIDv7 minted by the process-wide monotonic
/// generator, so `id` order is mint order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Type)]
#[sqlx(transparent)]
pub struct MenuDishId(Uuid);

impl MenuDishId {
    /// Minted from the process-wide monotonic generator, not a random v4 —
    /// the listings order by `id` within a menu, which random low bits would
    /// scramble.
    pub fn generate() -> Self {
        Self(next_uuid())
    }

    /// Parse a wire key. A key that parses as no UUID — a malformed path
    /// segment — reads as the nil id, which matches no row: exactly the 404 a
    /// dangling record key produced under the old store, without turning a
    /// typo into a panic.
    pub fn from_key(key: &str) -> Self {
        Self(Uuid::parse_str(key).unwrap_or(Uuid::nil()))
    }

    /// The hyphenated wire form.
    pub fn key(&self) -> String {
        self.0.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Type)]
#[sqlx(transparent)]
pub struct DishName(String);

impl DishName {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        validate_required("name", value, MAX_DISH_NAME_LEN)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Type)]
#[sqlx(transparent)]
pub struct DishDescription(String);

impl DishDescription {
    /// Blank (or whitespace-only) means "no description" — `None`, not an
    /// empty string, so the column is NULL rather than falsely present.
    pub fn try_new(value: &str) -> Result<Option<Self>, ValidationError> {
        validate_optional("description", value, MAX_DISH_DESCRIPTION_LEN)?;
        let value = value.trim();
        Ok((!value.is_empty()).then(|| Self(value.to_string())))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What one serving costs, in **minor units** (kuruş). Never negative — a
/// giveaway dish is `0`, and money that flows the other way is a ledger line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Type)]
#[sqlx(transparent)]
pub struct DishPrice(i64);

impl DishPrice {
    pub fn try_new(value: i64) -> Result<Self, ValidationError> {
        if !(0..=MAX_DISH_PRICE_MINOR).contains(&value) {
            return Err(ValidationError::Invalid {
                field: "price_minor",
                reason: "must be between 0 and 1000000 minor units",
            });
        }
        Ok(Self(value))
    }

    pub fn as_minor(self) -> i64 {
        self.0
    }
}

/// The dietary tags a dish carries, drawn from the school's `dietary_tags`
/// list. Deduplicated, order preserved — the list is what a student's profile
/// is matched against, so a repeat carries no extra meaning. Stored as a
/// `TEXT[]` column.
#[derive(Debug, Clone, PartialEq, Eq, Type)]
#[sqlx(transparent, no_pg_array)]
pub struct DishTags(Vec<String>);

impl DishTags {
    pub fn try_new(values: &[String], allowed: &[String]) -> Result<Self, ValidationError> {
        let mut tags: Vec<String> = Vec::new();
        for value in values {
            let value = value.trim();
            if !allowed.iter().any(|tag| tag == value) {
                return Err(ValidationError::Invalid {
                    field: "tags",
                    reason: "not one of the school's dietary tags (see GET /settings)",
                });
            }
            if !tags.iter().any(|tag| tag == value) {
                tags.push(value.to_string());
            }
        }
        if tags.len() > MAX_DISH_TAGS {
            return Err(ValidationError::Invalid {
                field: "tags",
                reason: "at most 10 tags per dish",
            });
        }
        Ok(Self(tags))
    }

    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MenuDish {
    pub(crate) id: MenuDishId,
    pub(crate) menu: MenuId,
    pub(crate) name: DishName,
    pub(crate) description: Option<DishDescription>,
    pub(crate) price_minor: DishPrice,
    pub(crate) tags: DishTags,
    pub(crate) created_at: Timestamp,
}

impl MenuDish {
    pub fn get_id(&self) -> &MenuDishId {
        &self.id
    }

    pub fn get_menu(&self) -> &MenuId {
        &self.menu
    }

    pub fn get_name(&self) -> &DishName {
        &self.name
    }

    pub fn get_description(&self) -> Option<&DishDescription> {
        self.description.as_ref()
    }

    pub fn get_price_minor(&self) -> DishPrice {
        self.price_minor
    }

    pub fn get_tags(&self) -> &DishTags {
        &self.tags
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_is_bounded_non_negative_minor_units() {
        assert_eq!(DishPrice::try_new(0).unwrap().as_minor(), 0);
        assert_eq!(DishPrice::try_new(4550).unwrap().as_minor(), 4550);
        assert!(DishPrice::try_new(-1).is_err());
        assert!(DishPrice::try_new(MAX_DISH_PRICE_MINOR).is_ok());
        assert!(DishPrice::try_new(MAX_DISH_PRICE_MINOR + 1).is_err());
    }

    #[test]
    fn tags_must_come_from_the_school_list_and_dedupe() {
        let allowed = vec!["vegan".to_string(), "nut_allergy".to_string()];
        let tags =
            DishTags::try_new(&["vegan".to_string(), "vegan".to_string()], &allowed).unwrap();
        assert_eq!(tags.as_slice(), ["vegan"]);
        assert!(DishTags::try_new(&["halal".to_string()], &allowed).is_err());
        assert!(
            DishTags::try_new(&[], &allowed)
                .unwrap()
                .as_slice()
                .is_empty()
        );
    }
}
