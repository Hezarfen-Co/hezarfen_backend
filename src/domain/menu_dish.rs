//! One dish on a published [`Menu`](crate::domain::menu::Menu): what it is,
//! what it costs, and which dietary tags it carries.
//!
//! Money is **minor units** (kuruş) as `i64` — never a float, never a decimal,
//! at any layer. `tags` are validated against the school's `dietary_tags`
//! list, the same contract a menu's `slot` has with `meal_slots`.

use std::sync::LazyLock;

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Generator;

use crate::constant::{
    MAX_DISH_DESCRIPTION_LEN, MAX_DISH_NAME_LEN, MAX_DISH_PRICE_MINOR, MAX_DISH_TAGS,
    MENU_DISH_TABLE,
};
use crate::database::Database;
use crate::domain::field_update::FieldUpdate;
use crate::domain::menu::MenuId;
use crate::domain::timestamp::Timestamp;
use crate::error::{AppError, ValidationError};
use crate::validate::{validate_optional, validate_required};

/// Mints dish ids in write order — see [`MenuId::generate`]; the listings below
/// order by `id` within a menu, which random low bits would scramble.
static IDS: LazyLock<std::sync::Mutex<Generator>> =
    LazyLock::new(|| std::sync::Mutex::new(Generator::new()));

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct MenuDishId(RecordId);

impl MenuDishId {
    pub fn generate() -> Self {
        let mut ids = IDS.lock().expect("menu dish id generator poisoned");
        // The only error is exhausting the random bits within one millisecond
        // (2^80 ids deep); it clears itself as the clock ticks, so retry.
        let ulid = loop {
            if let Ok(ulid) = ids.generate() {
                break ulid;
            }
        };
        Self(RecordId::new(MENU_DISH_TABLE, ulid.to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(MENU_DISH_TABLE, key))
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

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
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

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct DishDescription(String);

impl DishDescription {
    /// Blank (or whitespace-only) means "no description" — `None`, not an
    /// empty string, so the column is absent rather than falsely present.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, SurrealValue)]
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
/// is matched against, so a repeat carries no extra meaning.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
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

#[derive(Debug, Clone, SurrealValue)]
pub struct MenuDish {
    id: MenuDishId,
    menu: MenuId,
    name: DishName,
    description: Option<DishDescription>,
    price_minor: DishPrice,
    tags: DishTags,
    created_at: Timestamp,
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

    pub async fn create(
        menu: &MenuId,
        name: DishName,
        description: Option<DishDescription>,
        price_minor: DishPrice,
        tags: DishTags,
        db: &Database,
    ) -> Result<MenuDish, AppError> {
        let dish = MenuDish {
            id: MenuDishId::generate(),
            menu: menu.clone(),
            name,
            description,
            price_minor,
            tags,
            created_at: Timestamp::now(),
        };
        let created: Option<MenuDish> = db.create(dish.id.record()).content(dish).await?;
        created.ok_or_else(|| AppError::Internal("failed to add the dish".into()))
    }

    pub async fn read(id: &MenuDishId, db: &Database) -> Result<Option<MenuDish>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// Every dish on one menu, in the order they were added.
    pub async fn list_for_menu(menu: &MenuId, db: &Database) -> Result<Vec<MenuDish>, AppError> {
        Self::list_for_menus(std::slice::from_ref(menu), db).await
    }

    /// Dishes for a whole page of menus in one round trip — the alternative is
    /// a query per menu, which is the N+1 the pagination slice exists to avoid.
    pub async fn list_for_menus(
        menus: &[MenuId],
        db: &Database,
    ) -> Result<Vec<MenuDish>, AppError> {
        if menus.is_empty() {
            return Ok(Vec::new());
        }
        let mut result = db
            .query("SELECT * FROM menu_dish WHERE menu IN $menus ORDER BY id ASC")
            .bind((
                "menus",
                menus.iter().map(MenuId::record).collect::<Vec<_>>(),
            ))
            .await?
            .check()?;
        Ok(result.take::<Vec<MenuDish>>(0)?)
    }

    /// How many dishes the menu already carries — the `MAX_DISHES_PER_MENU` gate.
    pub async fn count_for_menu(menu: &MenuId, db: &Database) -> Result<usize, AppError> {
        Ok(Self::list_for_menu(menu, db).await?.len())
    }

    /// Write only the fields the PATCH carried. `description` is nullable, so
    /// it takes the three-way shape: absent = keep, `Some(None)` = clear.
    pub async fn update(
        self,
        name: Option<DishName>,
        description: Option<Option<DishDescription>>,
        price_minor: Option<DishPrice>,
        tags: Option<DishTags>,
        db: &Database,
    ) -> Result<MenuDish, AppError> {
        FieldUpdate::new(self.id.record())
            .set("name", name)
            .set("description", description)
            .set("price_minor", price_minor)
            .set("tags", tags)
            .run::<MenuDish>(db)
            .await
    }

    pub async fn delete(self, db: &Database) -> Result<MenuDish, AppError> {
        let deleted: Option<MenuDish> = db.delete(self.id.record()).await?;
        deleted.ok_or(AppError::NotFound)
    }

    /// Drop every dish on a menu — the menu delete's cascade.
    pub async fn delete_for_menu(menu: &MenuId, db: &Database) -> Result<(), AppError> {
        db.query("DELETE menu_dish WHERE menu = $menu")
            .bind(("menu", menu.record()))
            .await?
            .check()?;
        Ok(())
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
