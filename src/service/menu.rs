//! The kitchen's workflows: publishing and unpublishing menus, and every
//! dish write. The queries live in [`crate::db::menu`] and
//! [`crate::db::menu_dish`]; the row shapes and the date/slot/capacity
//! rules in [`crate::domain::menu`] and [`crate::domain::menu_dish`].
//!
//! **No process-wide lock guards this domain.** The dish cap
//! (`MAX_DISHES_PER_MENU`) used to be a count-then-write pair only a
//! process mutex could make safe; it is now one guarded statement — the
//! menu row is taken `FOR NO KEY UPDATE` inside the dish write's own
//! transaction and the `INSERT … WHERE count(*) < cap` runs behind that
//! lock, so two racing writers serialize on the row the database owns.
//! Every dish write moves the menu's revision in that same transaction, and
//! a booking claims its seat at the revision it priced itself against.

use crate::constant::MAX_DISHES_PER_MENU;
use crate::database::Database;
use crate::db::{menu, menu_dish};
use crate::domain::menu::{Menu, MenuDate, MenuId, MenuSlot};
use crate::domain::menu_dish::{
    DishDescription, DishName, DishPrice, DishTags, MenuDish, MenuDishId,
};
use crate::domain::user::UserId;
use crate::error::AppError;

/// Publish a menu for one day and meal slot. The caller has validated the
/// date (a real calendar day, not already over), the slot (one the school
/// serves) and the capacity; the day+slot duplicate refusal is decided by
/// the store ([`menu::create`]).
pub async fn create(
    db: &Database,
    date: MenuDate,
    slot: MenuSlot,
    capacity: Option<i64>,
    created_by: &UserId,
) -> Result<Menu, AppError> {
    menu::create(db, date, slot, capacity, created_by).await
}

/// The menu, for callers that only inspect it — the web layer's handlers
/// read through here.
pub async fn read(db: &Database, id: &MenuId) -> Result<Option<Menu>, AppError> {
    menu::read(db, id).await
}

/// Menus, newest day first, within inclusive `YYYY-MM-DD` bounds.
pub async fn list(
    db: &Database,
    from: Option<&MenuDate>,
    to: Option<&MenuDate>,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Menu>, i64), AppError> {
    menu::list(db, from, to, limit, offset).await
}

/// Change a menu's seat cap; `None` keeps it, `Some(None)` clears it.
pub async fn update(
    db: &Database,
    menu_row: Menu,
    capacity: Option<Option<i64>>,
) -> Result<Menu, AppError> {
    menu::update(db, menu_row, capacity).await
}

/// Unpublish a menu, sweeping its dishes and attendance marks in the same
/// transaction; refused while a seat is still held.
pub async fn delete(db: &Database, menu_row: Menu) -> Result<Menu, AppError> {
    menu::delete(db, menu_row).await
}

/// Add a dish to a menu, under the dish cap.
///
/// The cap is the database's to enforce, not this process's: the write
/// locks the menu row `FOR NO KEY UPDATE` and its `INSERT` carries a
/// `count(*) < cap` guard, so a writer that queued behind a concurrent one
/// re-counts after the lock and is refused (`409`) instead of overfilling
/// — the count and the insert are one decision, with no window a second
/// writer can slip through.
///
/// The *price* needs no lock — a dish write moves the menu's revision, and
/// a booking claims its seat at the revision it priced itself against.
pub async fn add_dish(
    db: &Database,
    menu_id: &MenuId,
    name: DishName,
    description: Option<DishDescription>,
    price_minor: DishPrice,
    tags: DishTags,
) -> Result<MenuDish, AppError> {
    menu_dish::create(
        db,
        menu_id,
        name,
        description,
        price_minor,
        tags,
        MAX_DISHES_PER_MENU as i64,
    )
    .await
}

/// One dish, for callers that only inspect it.
pub async fn read_dish(db: &Database, id: &MenuDishId) -> Result<Option<MenuDish>, AppError> {
    menu_dish::read(db, id).await
}

/// Every dish on one menu, in the order they were added.
pub async fn list_dishes(db: &Database, menu_id: &MenuId) -> Result<Vec<MenuDish>, AppError> {
    menu_dish::list_for_menu(db, menu_id).await
}

/// Dishes for a whole page of menus in one round trip.
pub async fn list_dishes_for_menus(
    db: &Database,
    menus: &[MenuId],
) -> Result<Vec<MenuDish>, AppError> {
    menu_dish::list_for_menus(db, menus).await
}

/// Edit a dish. Only what the request carried is written.
pub async fn update_dish(
    db: &Database,
    dish: MenuDish,
    name: Option<DishName>,
    description: Option<Option<DishDescription>>,
    price_minor: Option<DishPrice>,
    tags: Option<DishTags>,
) -> Result<MenuDish, AppError> {
    menu_dish::update(db, dish, name, description, price_minor, tags).await
}

/// Remove a dish from its menu.
pub async fn delete_dish(db: &Database, dish: MenuDish) -> Result<MenuDish, AppError> {
    menu_dish::delete(db, dish).await
}
