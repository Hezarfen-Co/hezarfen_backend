//! The kitchen's workflows: publishing and unpublishing menus, and every
//! dish write — which all hold [`MENU_LOCK`], the lock that makes the
//! per-menu dish cap (`MAX_DISHES_PER_MENU`, a count-then-write SurrealDB
//! does not conflict-check) more than a wish. The queries live in
//! [`crate::db::menu`] and [`crate::db::menu_dish`]; the row shapes and the
//! date/slot/capacity rules in [`crate::domain::menu`] and
//! [`crate::domain::menu_dish`].
//!
//! [`MENU_LOCK`] spans a **multi-call** sequence (read the menu, count the
//! dishes, write the dish), which is why it lives here and not in the db
//! layer: a single-call atomicity lock would be the store's to own, but
//! count-then-write is write-skew only this process can serialize.
//!
//! **A leaf**: every dish write held under it moves the menu's revision
//! inside its *own* transaction ([`crate::db::menu::bump_menu_and_write`]),
//! so no path under this lock reaches `cap`'s counter lock any more. Should
//! one ever need both, the order is `MENU_LOCK` → `CLAIM_LOCK` and nothing
//! may take this lock while holding a counter lock: that is the invariant a
//! new caller must keep, and it is what would keep the pair deadlock-free.

use tokio::sync::Mutex;

use crate::constant::MAX_DISHES_PER_MENU;
use crate::database::Database;
use crate::db::{menu, menu_dish};
use crate::domain::menu::{Menu, MenuDate, MenuId, MenuSlot};
use crate::domain::menu_dish::{
    DishDescription, DishName, DishPrice, DishTags, MenuDish, MenuDishId,
};
use crate::domain::user::UserId;
use crate::error::AppError;

/// Serializes what is left that counts rows against one menu: the dish cap
/// (`MAX_DISHES_PER_MENU`, a count-then-write SurrealDB does not
/// conflict-check). Publishing no longer needs it — the day+slot *is* the
/// record id — and neither does a booking, a menu delete, or the settings
/// slot-removal guard: those went to conditional single-record writes
/// ([`crate::db::cap`]), which the store decides as this lock cannot.
//
// corner-cut: the dish cap therefore rests on this lock alone — a dish write
// added without taking it reopens the count-then-write hole silently. Closing
// it properly is another `cap` counter (`dish_count` on the menu row) plus its
// backfill; the ceiling is 51 dishes on a menu, not money or a seat, so it was
// not worth the column here.
pub(crate) static MENU_LOCK: Mutex<()> = Mutex::const_new(());

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
/// [`MENU_LOCK`] is taken for the dish cap alone: count-then-write
/// is write-skew, so the count and the insert have to be one step. The
/// *price* no longer needs it — a dish write moves the menu's revision, and
/// a booking claims its seat at the revision it priced itself against.
/// The lock is a leaf again: the revision bump rides the dish write's own
/// transaction now, so nothing held under it takes `cap`'s counter lock. It
/// stays a leaf only while that holds — see [`MENU_LOCK`] for the order a
/// caller that changes it must keep.
///
/// The menu is read *inside* the lock: read before it, a `DELETE /menus/{id}`
/// running in the gap takes its cascade with it and this dish lands on a menu
/// that no longer exists.
pub async fn add_dish(
    db: &Database,
    menu_id: &MenuId,
    name: DishName,
    description: Option<DishDescription>,
    price_minor: DishPrice,
    tags: DishTags,
) -> Result<MenuDish, AppError> {
    let _guard = MENU_LOCK.lock().await;
    let menu = menu::read(db, menu_id).await?.ok_or(AppError::NotFound)?;
    if menu_dish::count_for_menu(db, menu.get_id()).await? >= MAX_DISHES_PER_MENU {
        return Err(AppError::Conflict(
            "the menu already carries the maximum number of dishes",
        ));
    }
    menu_dish::create(db, menu.get_id(), name, description, price_minor, tags).await
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

/// Edit a dish. Under [`MENU_LOCK`] like every dish write — see
/// [`add_dish`]. Only what the request carried is written.
pub async fn update_dish(
    db: &Database,
    dish: MenuDish,
    name: Option<DishName>,
    description: Option<Option<DishDescription>>,
    price_minor: Option<DishPrice>,
    tags: Option<DishTags>,
) -> Result<MenuDish, AppError> {
    let _guard = MENU_LOCK.lock().await;
    menu_dish::update(db, dish, name, description, price_minor, tags).await
}

/// Remove a dish from its menu. Under [`MENU_LOCK`] like every dish write.
pub async fn delete_dish(db: &Database, dish: MenuDish) -> Result<MenuDish, AppError> {
    let _guard = MENU_LOCK.lock().await;
    menu_dish::delete(db, dish).await
}
