//! The `menu_dish` table: the dishes on a menu — the insert and the field
//! PATCH riding the menu's revision bump ([`bump_menu_and_write`]), the
//! reads and listings, and the delete. The dish-cap gate and the
//! [`MENU_LOCK`](crate::service::menu::MENU_LOCK) leases live in
//! [`crate::service::menu`].

use surrealdb::types::{SurrealValue, Value};

use crate::constant::MENU_VERSION_FIELD;
use crate::database::{Database, transaction_with_retry};
use crate::db::menu::bump_menu_and_write;
use crate::domain::menu::MenuId;
use crate::domain::menu_dish::{
    DishDescription, DishName, DishPrice, DishTags, MenuDish, MenuDishId,
};
use crate::domain::timestamp::Timestamp;
use crate::error::AppError;

/// Add a dish. Moves the menu's revision *first* — see [`bump_menu_and_write`].
pub async fn create(
    db: &Database,
    menu: &MenuId,
    name: DishName,
    description: Option<DishDescription>,
    price_minor: DishPrice,
    tags: DishTags,
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
    // The revision bump and the insert are one transaction, and the bump is
    // what makes the menu's existence part of it: the `UPDATE` matches
    // nothing once the menu row is deleted, and a delete racing this one
    // touches the very key this transaction writes, so the two cannot both
    // commit. Without that, a dish landing just after `DELETE /menus/{id}`
    // removed the row but before its cascade ran outlived its menu.
    // Every other dish write bumps the same version key, so a lost round is
    // ordinary here; it is re-sent rather than reported. Re-sending is sound
    // even with the `CREATE` in the batch — the abort wrote nothing, the id
    // is a ULID freshly generated above and never seen by a rival, and
    // `menu_dish` carries no UNIQUE index — so the retry cannot answer
    // "already exists" (see [`transaction_with_retry`]).
    let (mut result, mut errors) = transaction_with_retry(
        db,
        &format!(
            "BEGIN TRANSACTION;
             LET $bumped = (UPDATE $menu SET {MENU_VERSION_FIELD} = \
                 ({MENU_VERSION_FIELD} ?? 0) + 1 RETURN VALUE id);
             IF array::len($bumped) = 0 {{ THROW 'no_menu' }};
             CREATE $id CONTENT $dish;
             COMMIT TRANSACTION;"
        ),
        &[
            ("menu".into(), menu.record().into_value()),
            ("id".into(), dish.id.record().into_value()),
            ("dish".into(), dish.into_value()),
        ],
        &["no_menu"],
    )
    .await?;
    // An aborted transaction errors *every* slot, most with a generic "not
    // executed" — only the THROW's own slot names the reason.
    if errors
        .values()
        .any(|error| error.to_string().contains("no_menu"))
    {
        return Err(AppError::NotFound);
    }
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    // Slots count BEGIN, the LET and the IF: the CREATE is slot 3.
    result
        .take::<Vec<MenuDish>>(3)?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::Internal("failed to add the dish".into()))
}

pub async fn read(db: &Database, id: &MenuDishId) -> Result<Option<MenuDish>, AppError> {
    Ok(db.select(id.record()).await?)
}

/// Every dish on one menu, in the order they were added.
pub async fn list_for_menu(db: &Database, menu: &MenuId) -> Result<Vec<MenuDish>, AppError> {
    list_for_menus(db, std::slice::from_ref(menu)).await
}

/// Dishes for a whole page of menus in one round trip — the alternative is
/// a query per menu, which is the N+1 the pagination slice exists to avoid.
pub async fn list_for_menus(db: &Database, menus: &[MenuId]) -> Result<Vec<MenuDish>, AppError> {
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
pub async fn count_for_menu(db: &Database, menu: &MenuId) -> Result<usize, AppError> {
    Ok(list_for_menu(db, menu).await?.len())
}

/// Write only the fields the PATCH carried. `description` is nullable, so
/// it takes the three-way shape: absent = keep, `Some(None)` = clear.
///
/// The `SET` is built here rather than by
/// [`FieldUpdate`](crate::db::field_update::FieldUpdate) because the
/// revision bump lands on *another* row and has to share this write's
/// transaction; the field-by-field scoping — an omitted field is never
/// written, so a concurrent PATCH of another one is not reverted — is the
/// same rule, spelled out.
pub async fn update(
    db: &Database,
    dish: MenuDish,
    name: Option<DishName>,
    description: Option<Option<DishDescription>>,
    price_minor: Option<DishPrice>,
    tags: Option<DishTags>,
) -> Result<MenuDish, AppError> {
    let mut sets: Vec<&str> = Vec::new();
    let mut bindings: Vec<(String, Value)> = vec![("id".into(), dish.id.record().into_value())];
    let mut set = |field: &'static str, value: Option<Value>| {
        if let Some(value) = value {
            sets.push(field);
            bindings.push((field.into(), value));
        }
    };
    set("name", name.map(SurrealValue::into_value));
    set("description", description.map(SurrealValue::into_value));
    set("price_minor", price_minor.map(SurrealValue::into_value));
    set("tags", tags.map(SurrealValue::into_value));
    // A PATCH that carried nothing writes nothing and reads the row back,
    // exactly as `FieldUpdate` answers one — but it still moves the
    // revision, because "every dish write bumps" is the rule a caller can
    // rely on without knowing which fields were on the wire.
    let statement = if sets.is_empty() {
        "SELECT * FROM $id".to_string()
    } else {
        format!(
            "UPDATE $id SET {} RETURN AFTER",
            sets.iter()
                .map(|field| format!("{field} = ${field}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    bump_menu_and_write(&dish.menu, &statement, bindings, db)
        .await?
        .ok_or(AppError::NotFound)
}

pub async fn delete(db: &Database, dish: MenuDish) -> Result<MenuDish, AppError> {
    bump_menu_and_write(
        &dish.menu,
        "DELETE $id RETURN BEFORE",
        vec![("id".into(), dish.id.record().into_value())],
        db,
    )
    .await?
    .ok_or(AppError::NotFound)
}
