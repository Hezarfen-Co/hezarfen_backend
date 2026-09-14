//! The `menu_dish` table: the dishes on a menu — the guarded insert (the
//! dish cap, enforced by the menu row's own lock), the field PATCH riding
//! the menu's revision bump, the reads and listings, and the delete. The
//! kitchen workflows live in [`crate::service::menu`].

use crate::database::{Database, tx_with_retry};
use crate::domain::menu::MenuId;
use crate::domain::menu_dish::{
    DishDescription, DishName, DishPrice, DishTags, MenuDish, MenuDishId,
};
use crate::domain::timestamp::Timestamp;
use crate::error::AppError;

/// Add a dish. The dish cap is one guarded decision, not a count a caller
/// took a moment earlier: the menu row is locked `FOR NO KEY UPDATE` in
/// this write's own transaction and the `INSERT` runs with a
/// `count(*) < $cap` guard — a writer that queued on the lock behind a
/// concurrent one re-counts after it and is refused (`409`) instead of
/// overfilling. The revision bump rides the same transaction, and the
/// lock's `SELECT` is what makes the menu's **existence** part of the
/// write: it matches nothing once the menu row is deleted, so `NotFound`
/// means the dish was not written.
pub async fn create(
    db: &Database,
    menu: &MenuId,
    name: DishName,
    description: Option<DishDescription>,
    price_minor: DishPrice,
    tags: DishTags,
    cap: i64,
) -> Result<MenuDish, AppError> {
    let menu_key = menu.key().to_string();
    let id = MenuDishId::generate();
    let created_at = Timestamp::now();
    tx_with_retry(db, false, async move |tx| {
        // The lock first: every later statement in this transaction then
        // sees the committed state of whoever queued ahead of it.
        let locked = sqlx::query!(
            "SELECT 1 AS locked FROM menu WHERE id = $1 FOR NO KEY UPDATE",
            menu_key,
        )
        .fetch_optional(&mut *tx)
        .await?;
        if locked.is_none() {
            return Err(AppError::NotFound);
        }
        sqlx::query!(
            "UPDATE menu SET version = COALESCE(version, 0) + 1 WHERE id = $1",
            menu_key,
        )
        .execute(&mut *tx)
        .await?;
        let placed = sqlx::query!(
            "INSERT INTO menu_dish (id, menu, name, description, price_minor, created_at)
             SELECT $1, $2, $3, $4, $5, $6
             WHERE (SELECT count(*) FROM menu_dish WHERE menu = $2) < $7",
            id.uuid(),
            menu_key,
            name.as_str(),
            description.as_ref().map(|d| d.as_str()),
            price_minor.as_minor(),
            created_at.as_millis(),
            cap,
        )
        .execute(&mut *tx)
        .await?;
        if placed.rows_affected() == 0 {
            // The menu exists (locked above), so an empty result is the cap.
            return Err(AppError::Conflict(
                "the menu already carries the maximum number of dishes",
            ));
        }
        // The tags are child rows: one per tag, `ord` carrying the order
        // the caller gave the list in (already deduplicated by DishTags).
        sqlx::query!(
            "INSERT INTO menu_dish_tag (dish, tag, ord)
             SELECT $1, tag, ord FROM unnest($2::text[]) WITH ORDINALITY AS t(tag, ord)",
            id.uuid(),
            tags.as_slice(),
        )
        .execute(&mut *tx)
        .await?;
        let row = sqlx::query_as!(
            MenuDish,
            "SELECT id AS \"id: MenuDishId\", menu AS \"menu: MenuId\", name AS \"name: DishName\", description AS \"description: DishDescription\", price_minor AS \"price_minor: DishPrice\",
                 COALESCE((SELECT array_agg(t.tag ORDER BY t.ord) FROM menu_dish_tag t WHERE t.dish = menu_dish.id), '{}'::text[]) AS \"tags!: DishTags\",
                 created_at AS \"created_at: Timestamp\"
             FROM menu_dish WHERE id = $1",
            id.uuid(),
        )
        .fetch_one(&mut *tx)
        .await?;
        Ok(row)
    })
    .await
}

pub async fn read(db: &Database, id: &MenuDishId) -> Result<Option<MenuDish>, AppError> {
    let row = sqlx::query_as!(
        MenuDish,
        "SELECT id AS \"id: MenuDishId\", menu AS \"menu: MenuId\", name AS \"name: DishName\", description AS \"description: DishDescription\", price_minor AS \"price_minor: DishPrice\",
             COALESCE((SELECT array_agg(t.tag ORDER BY t.ord) FROM menu_dish_tag t WHERE t.dish = menu_dish.id), '{}'::text[]) AS \"tags!: DishTags\",
             created_at AS \"created_at: Timestamp\"
         FROM menu_dish WHERE id = $1",
        id.uuid(),
    )
    .fetch_optional(db)
    .await?;
    Ok(row)
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
    let keys: Vec<String> = menus.iter().map(|m| m.key().to_string()).collect();
    let rows = sqlx::query_as!(
        MenuDish,
        "SELECT id AS \"id: MenuDishId\", menu AS \"menu: MenuId\", name AS \"name: DishName\", description AS \"description: DishDescription\", price_minor AS \"price_minor: DishPrice\",
             COALESCE((SELECT array_agg(t.tag ORDER BY t.ord) FROM menu_dish_tag t WHERE t.dish = menu_dish.id), '{}'::text[]) AS \"tags!: DishTags\",
             created_at AS \"created_at: Timestamp\"
         FROM menu_dish WHERE menu = ANY($1) ORDER BY id ASC",
        &keys,
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

/// Write only the fields the PATCH carried. `description` is nullable, so
/// it takes the three-way shape: absent = keep, `Some(None)` = clear.
/// Each field is spelled with a "was it on the request?" flag and its
/// nullable value, so an omitted field is never written and a concurrent
/// PATCH of another one is not reverted.
///
/// The revision bump lands in the same transaction — "every dish write
/// bumps" is the rule a caller can rely on without knowing which fields
/// were on the wire. A PATCH that carried nothing still bumps and reads
/// the row back.
pub async fn update(
    db: &Database,
    dish: MenuDish,
    name: Option<DishName>,
    description: Option<Option<DishDescription>>,
    price_minor: Option<DishPrice>,
    tags: Option<DishTags>,
) -> Result<MenuDish, AppError> {
    let dish_key = dish.id.uuid();
    let menu_key = dish.menu.key().to_string();
    tx_with_retry(db, false, async move |tx| {
        let bumped = sqlx::query!(
            "UPDATE menu SET version = COALESCE(version, 0) + 1 WHERE id = $1
             RETURNING 1 AS bumped",
            menu_key,
        )
        .fetch_optional(&mut *tx)
        .await?;
        if bumped.is_none() {
            return Err(AppError::NotFound);
        }
        let updated = sqlx::query!(
            "UPDATE menu_dish SET
                 name = COALESCE($2, name),
                 description = CASE WHEN $3 THEN $4 ELSE description END,
                 price_minor = COALESCE($5, price_minor)
             WHERE id = $1",
            dish_key,
            name.as_ref().map(|n| n.as_str()),
            description.is_some(),
            description.as_ref().and_then(|d| d.as_ref()).map(|d| d.as_str()),
            price_minor.map(DishPrice::as_minor),
        )
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 0 {
            return Err(AppError::NotFound);
        }
        // The tags are child rows now: an absent field keeps the stored
        // ones, a carried field replaces the whole set (an empty list
        // clears it) — under the same transaction as the bump above.
        if let Some(tags) = &tags {
            sqlx::query!("DELETE FROM menu_dish_tag WHERE dish = $1", dish_key)
                .execute(&mut *tx)
                .await?;
            sqlx::query!(
                "INSERT INTO menu_dish_tag (dish, tag, ord)
                 SELECT $1, tag, ord FROM unnest($2::text[]) WITH ORDINALITY AS t(tag, ord)",
                dish_key,
                tags.as_slice(),
            )
            .execute(&mut *tx)
            .await?;
        }
        let row = sqlx::query_as!(
            MenuDish,
            "SELECT id AS \"id: MenuDishId\", menu AS \"menu: MenuId\", name AS \"name: DishName\", description AS \"description: DishDescription\", price_minor AS \"price_minor: DishPrice\",
                 COALESCE((SELECT array_agg(t.tag ORDER BY t.ord) FROM menu_dish_tag t WHERE t.dish = menu_dish.id), '{}'::text[]) AS \"tags!: DishTags\",
                 created_at AS \"created_at: Timestamp\"
             FROM menu_dish WHERE id = $1",
            dish_key,
        )
        .fetch_one(&mut *tx)
        .await?;
        Ok(row)
    })
    .await
}

pub async fn delete(db: &Database, dish: MenuDish) -> Result<MenuDish, AppError> {
    let dish_key = dish.id.uuid();
    let menu_key = dish.menu.key().to_string();
    tx_with_retry(db, false, async move |tx| {
        let bumped = sqlx::query!(
            "UPDATE menu SET version = COALESCE(version, 0) + 1 WHERE id = $1
             RETURNING 1 AS bumped",
            menu_key,
        )
        .fetch_optional(&mut *tx)
        .await?;
        if bumped.is_none() {
            return Err(AppError::NotFound);
        }
        let row = sqlx::query_as!(
            MenuDish,
            "SELECT id AS \"id: MenuDishId\", menu AS \"menu: MenuId\", name AS \"name: DishName\", description AS \"description: DishDescription\", price_minor AS \"price_minor: DishPrice\",
                 COALESCE((SELECT array_agg(t.tag ORDER BY t.ord) FROM menu_dish_tag t WHERE t.dish = menu_dish.id), '{}'::text[]) AS \"tags!: DishTags\",
                 created_at AS \"created_at: Timestamp\"
             FROM menu_dish WHERE id = $1",
            dish_key,
        )
        .fetch_optional(&mut *tx)
        .await?;
        let dish = row.ok_or(AppError::NotFound)?;
        // The tag rows go first: `menu_dish_tag.dish` is NO ACTION too.
        sqlx::query!("DELETE FROM menu_dish_tag WHERE dish = $1", dish_key)
            .execute(&mut *tx)
            .await?;
        sqlx::query!("DELETE FROM menu_dish WHERE id = $1", dish_key)
            .execute(&mut *tx)
            .await?;
        Ok(dish)
    })
    .await
}
