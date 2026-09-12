//! The `menu` table: publishing (the slot reference claimed inside the
//! write's own transaction), row reads and listings, the compare-and-set
//! every cap PATCH writes through, and the cascading delete. The kitchen
//! workflows — the guarded dish-cap insert — live in
//! [`crate::service::menu`].

use crate::database::{Database, tx_with_retry, unique_violation};
use crate::db::page::{PagedList, Param};
use crate::domain::menu::{Menu, MenuDate, MenuId, MenuSlot, slot_ref};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Publish a menu. Refused (409) when the day+slot already carries one.
///
/// The day and slot *are* the row's primary key ([`MenuId::for_slot`]), so
/// the refusal is decided by the store rather than by a check a concurrent
/// publish can outrun: two publishes of the same meal write one row, and
/// the loser's unique violation on `menu_pkey` becomes the same 409. The
/// in-transaction pre-check stays for the ordinary case.
///
/// The menu takes a reference on its slot, which is what stops the slot
/// being dropped from the settings while this menu (whose slot is only
/// snapshotted text) still points at it. Claimed *inside* the write's own
/// transaction, with the duplicate gate ahead of the retired check so
/// "you already published this" still outranks "the slot is retired" (the
/// [`crate::db::cap`] reference-claim recipe): the counted row and the
/// count commit together or not at all, so no crash window is small enough
/// to leave a slot counted by a menu that does not exist — a slot nobody
/// could ever retire.
pub async fn create(
    db: &Database,
    date: MenuDate,
    slot: MenuSlot,
    capacity: Option<i64>,
    created_by: &UserId,
) -> Result<Menu, AppError> {
    let id_key = MenuId::for_slot(&date, &slot).key().to_string();
    let counter = slot_ref(slot.as_str());
    let date_s = date.as_str().to_string();
    let slot_s = slot.as_str().to_string();
    let created_by = *created_by;
    let created_at = Timestamp::now();
    tx_with_retry(db, false, async move |tx| {
        // Duplicate gate ahead of the retired check: "already published"
        // outranks "slot retired", on the very path a rival publish races.
        let exists = sqlx::query!("SELECT 1 AS taken FROM menu WHERE id = $1", id_key,)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_some() {
            return Err(AppError::Conflict(
                "a menu is already published for that date and slot",
            ));
        }
        let inserted = match sqlx::query_as!(
            Menu,
            "WITH ref AS (
                 INSERT INTO slot_ref (name, count) VALUES ($2, 1)
                 ON CONFLICT (name) DO UPDATE SET count = slot_ref.count + 1
                 WHERE slot_ref.retired = false
                 RETURNING 1 AS ref
             )
             INSERT INTO menu (id, date, slot, capacity, version, created_by, created_at)
             SELECT $1, $3, $4, $5, $6, $7, $8 WHERE EXISTS (SELECT 1 FROM ref)
             RETURNING id AS \"id: MenuId\", date AS \"date: MenuDate\", slot AS \"slot: MenuSlot\", capacity, version, created_by AS \"created_by: UserId\", created_at AS \"created_at: Timestamp\"",
            id_key,
            counter,
            date_s,
            slot_s,
            capacity,
            Some(0i64),
            created_by.uuid(),
            created_at.as_millis(),
        )
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(row) => row,
            Err(err) => {
                // A rival published the same meal after the pre-check: its
                // row owns the key — or, for a legacy id a derived key can
                // never collide with, its (date, slot) pair does — and the
                // rival's publish is the answer either way.
                if matches!(unique_violation(&err),
                            Some("menu_pkey") | Some("menu_date_slot"))
                {
                    return Err(AppError::Conflict(
                        "a menu is already published for that date and slot",
                    ));
                }
                return Err(err.into());
            }
        };
        match inserted {
            Some(menu) => Ok(menu),
            // The slot's ref row is retired — nothing was written.
            None => Err(AppError::ConflictOwned(format!(
                "the '{}' meal slot has been removed from the school's settings",
                slot_s,
            ))),
        }
    })
    .await
}

pub async fn read(db: &Database, id: &MenuId) -> Result<Option<Menu>, AppError> {
    let row = sqlx::query_as!(
        Menu,
        "SELECT id AS \"id: MenuId\", date AS \"date: MenuDate\", slot AS \"slot: MenuSlot\", capacity, version, created_by AS \"created_by: UserId\", created_at AS \"created_at: Timestamp\"
         FROM menu WHERE id = $1",
        id.key(),
    )
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// The menu for one day and slot, if any — the uniqueness check.
pub async fn find(
    db: &Database,
    date: &MenuDate,
    slot: &MenuSlot,
) -> Result<Option<Menu>, AppError> {
    let row = sqlx::query_as!(
        Menu,
        "SELECT id AS \"id: MenuId\", date AS \"date: MenuDate\", slot AS \"slot: MenuSlot\", capacity, version, created_by AS \"created_by: UserId\", created_at AS \"created_at: Timestamp\"
         FROM menu WHERE date = $1 AND slot = $2 LIMIT 1",
        date.as_str(),
        slot.as_str(),
    )
    .fetch_optional(db)
    .await?;
    Ok(row)
}

/// Menus, newest day first. `from`/`to` are inclusive `YYYY-MM-DD` bounds;
/// either may be omitted (`NULL` bound). The comparison is lexical, which
/// is chronological for this format.
pub async fn list(
    db: &Database,
    from: Option<&MenuDate>,
    to: Option<&MenuDate>,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Menu>, i64), AppError> {
    PagedList::new(
        "menu WHERE ($1::text IS NULL OR date >= $1::text) \
         AND ($2::text IS NULL OR date <= $2::text)",
        "ORDER BY date DESC, slot ASC, id DESC",
    )
    .bind(Param::OptText(from.map(|date| date.as_str().to_string())))
    .bind(Param::OptText(to.map(|date| date.as_str().to_string())))
    .run(limit, offset, db)
    .await
}

/// Only `capacity` is writable: `date` and `slot` are fixed at publish,
/// because moving a published menu to another day is a different menu.
/// `None` keeps the stored cap, `Some(None)` clears it back to uncapped.
///
/// The revision moves **in the same statement** as the cap, indivisibly: a
/// booking claims its seat against the revision it read the cap at, so a
/// shrink that bumped in a query of its own left a window where the row
/// carried the *new* revision and the *old* cap — and a booking arriving
/// there passes a CAS meant to refuse it, over-admitting by exactly the
/// seats in flight.
pub async fn update(
    db: &Database,
    menu: Menu,
    capacity: Option<Option<i64>>,
) -> Result<Menu, AppError> {
    let Some(capacity) = capacity else {
        // A PATCH carrying nothing writes nothing and bumps nothing: no
        // booking's price or cap has moved, so none owes a re-read.
        return read(db, &menu.id).await?.ok_or(AppError::NotFound);
    };
    let row = sqlx::query_as!(
        Menu,
        "UPDATE menu SET capacity = $2, version = COALESCE(version, 0) + 1
         WHERE id = $1
         RETURNING id AS \"id: MenuId\", date AS \"date: MenuDate\", slot AS \"slot: MenuSlot\", capacity, version, created_by AS \"created_by: UserId\", created_at AS \"created_at: Timestamp\"",
        menu.id.key(),
        capacity,
    )
    .fetch_optional(db)
    .await?;
    row.ok_or(AppError::NotFound)
}

/// Delete the menu and the dishes and marks on it — a dish or a mark has
/// no meaning without its menu, and the menu key cannot be re-homed.
///
/// Refused (409) while a seat is still held, and the *row itself* decides
/// that: the guard that unlocks the sweep reads the seat counter in its own
/// `WHERE` under `FOR UPDATE`, so a booking landing at that instant either
/// takes its seat before the delete (which then finds a non-zero counter
/// and refuses) or after it (and finds no menu). A read-then-delete pair
/// had a window where both happened — a paid seat on a menu that no longer
/// exists. The lock is also what makes the sweep's order safe: the dishes
/// and marks go first (both FKs are `ON DELETE NO ACTION`, so the parent
/// row cannot leave while they stand) and the menu row goes last, with no
/// window in between in which the children of a surviving menu could be
/// destroyed.
///
/// The slot gets its reference back in that same transaction — a slot no
/// menu is published for any more may leave the settings again. Released
/// afterwards in a statement of its own, a crash between the two left the
/// slot counted by a menu that no longer exists: a slot nobody can retire.
///
/// The marks go in that same sweep, for a sharper reason than tidiness:
/// [`MenuId::for_slot`] is deterministic, so republishing the same day and
/// slot mints the *same* row key. A mark left behind would come back as a
/// mark on the new menu — the kitchen reading "served" for a student who
/// never came — and until then the student's own report cites a menu that
/// is gone. Attendance carries no money and no counter, so it is safe to
/// drop; cancelled *bookings* deliberately stay (no foreign key ties them
/// to the menu), because their `attempt` counter is what keeps the
/// ledger's `(booking, attempt)` keys unique across the republish. Cut
/// those and a re-book on a republished menu would reuse a charge id the
/// ledger already holds, and the append (idempotent by design) would bill
/// the seat nothing.
///
/// The dishes ride in the same sweep: run after the commit, a crash
/// between the two would orphan them on the row key a republish mints
/// again — a new menu serving (and pricing) the old one's food.
pub async fn delete(db: &Database, menu: Menu) -> Result<Menu, AppError> {
    let key = menu.id.key().to_string();
    let counter = slot_ref(menu.slot.as_str());
    let deleted = tx_with_retry(db, true, async move |tx| {
        // The row itself decides the guard, under its own lock: a booking
        // racing this delete either committed first (the guard reads its
        // seat and refuses before any child is touched) or queues on the
        // lock and lands after the commit — and finds no menu.
        let guard = sqlx::query!(
            "SELECT 1 AS deletable FROM menu WHERE id = $1 AND seats_booked = 0 FOR UPDATE",
            key,
        )
        .fetch_optional(&mut *tx)
        .await?;
        if guard.is_none() {
            return Ok(None);
        }
        // The children go first: both `menu_dish.menu` and
        // `meal_attendance.menu` are `ON DELETE NO ACTION`, so the parent
        // row cannot leave while they stand.
        sqlx::query!("DELETE FROM meal_attendance WHERE menu = $1", key)
            .execute(&mut *tx)
            .await?;
        sqlx::query!("DELETE FROM menu_dish WHERE menu = $1", key)
            .execute(&mut *tx)
            .await?;
        let row = sqlx::query_as!(
            Menu,
            "DELETE FROM menu WHERE id = $1
             RETURNING id AS \"id: MenuId\", date AS \"date: MenuDate\", slot AS \"slot: MenuSlot\", capacity, version, created_by AS \"created_by: UserId\", created_at AS \"created_at: Timestamp\"",
            key,
        )
        .fetch_optional(&mut *tx)
        .await?;
        // Unreachable while the guard holds the row's lock, but the match
        // keeps the shape honest: `row` exists iff the guard passed.
        let Some(row) = row else {
            return Ok(None);
        };
        sqlx::query!(
            "UPDATE slot_ref SET count = GREATEST(count - 1, 0) WHERE name = $1",
            counter,
        )
        .execute(&mut *tx)
        .await?;
        Ok(Some(row))
    })
    .await?;
    match deleted {
        Some(menu) => Ok(menu),
        // Nothing back: either seats are held, or the menu is already gone.
        None => match read(db, &menu.id).await? {
            Some(_) => Err(AppError::Conflict("the menu still has live bookings")),
            None => Err(AppError::NotFound),
        },
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::init_test_db;
    use crate::db::meal_attendance;
    use crate::db::menu_dish;
    use crate::domain::meal_attendance::MealAttendanceStatus;
    use crate::domain::menu_dish::{DishName, DishPrice, DishTags};
    use crate::domain::settings::MealSlotDef;

    /// The one fixture person, by a fixed valid id every publish can name.
    const TEACHER: &str = "019732e3-7b00-7000-8000-00000000acdc";

    async fn school() -> (Database, crate::database::TestDatabases) {
        let (db, leases) = init_test_db().await;
        sqlx::query(
            "INSERT INTO app_user (id, username, password_hash, role) \
             VALUES ($1, 'teacher', 'x', 'teacher')",
        )
        .bind(UserId::from_key(TEACHER).uuid())
        .execute(&db)
        .await
        .unwrap();
        (db, leases)
    }

    fn lunch() -> MenuSlot {
        MenuSlot::try_new("lunch", &[MealSlotDef::try_new("lunch", None).unwrap()]).unwrap()
    }

    /// The reference count, re-read out of the store — never off a return
    /// value, which the in-memory engine forges wins on.
    async fn refs(db: &Database) -> i64 {
        use sqlx::Row as _;

        sqlx::query("SELECT count FROM slot_ref WHERE name = 'lunch'")
            .fetch_optional(db)
            .await
            .unwrap()
            .map(|row| row.try_get::<i64, _>(0).unwrap())
            .unwrap_or(0)
    }

    /// The settings guard's retire switch, driven straight: `true` only when
    /// this call flipped the bit.
    async fn retire_lunch(db: &Database) -> bool {
        sqlx::query(
            "INSERT INTO slot_ref (name, count, retired) VALUES ('lunch', 0, TRUE)
             ON CONFLICT (name) DO UPDATE SET retired = TRUE
             WHERE slot_ref.count = 0 AND slot_ref.retired IS DISTINCT FROM TRUE
             RETURNING 1",
        )
        .fetch_optional(db)
        .await
        .unwrap()
        .is_some()
    }

    async fn publish(date: &str, db: &Database) -> Result<Menu, AppError> {
        create(
            db,
            MenuDate::try_new(date).unwrap(),
            lunch(),
            None,
            &UserId::from_key(TEACHER),
        )
        .await
    }

    /// Plant a menu row the pre-check will (or will not) find, without going
    /// through the claim — a row published before the counter existed.
    async fn plant(id: &MenuId, date: &str, db: &Database) {
        sqlx::query(
            "INSERT INTO menu (id, date, slot, created_by, created_at) \
             VALUES ($1, $2, 'lunch', $3, 1)",
        )
        .bind(id.key())
        .bind(date)
        .bind(UserId::from_key(TEACHER).uuid())
        .execute(db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn publishing_lands_the_menu_and_its_reference_together() {
        let (db, _leases) = school().await;
        let menu = publish("2026-08-02", &db).await.unwrap();
        assert!(read(&db, menu.get_id()).await.unwrap().is_some());
        assert_eq!(refs(&db).await, 1);

        // The day+slot pre-check answers the second publish — and a refusal
        // may not count the slot, or the menu would outlive its own reference.
        let again = publish("2026-08-02", &db)
            .await
            .expect_err("that day and slot are taken");
        assert!(matches!(again, AppError::Conflict(_)), "got {again:?}");
        assert_eq!(refs(&db).await, 1);
    }

    #[tokio::test]
    async fn a_retired_slot_refuses_and_writes_nothing() {
        let (db, _leases) = school().await;
        assert!(retire_lunch(&db).await);

        let refused = publish("2026-08-02", &db)
            .await
            .expect_err("the slot left the settings");
        assert!(
            matches!(refused, AppError::ConflictOwned(_)),
            "got {refused:?}"
        );
        assert_eq!(refs(&db).await, 0);
        assert!(
            find(&db, &MenuDate::try_new("2026-08-02").unwrap(), &lunch(),)
                .await
                .unwrap()
                .is_none()
        );
    }

    /// The race the fold exists for: a rival places the very row this publish
    /// is placing, between the pre-check and the write. Only the claim's own
    /// gate can answer it, and its answer must cost no reference.
    #[tokio::test]
    async fn a_rival_on_the_id_answers_taken_and_counts_nothing() {
        let (db, _leases) = school().await;
        let id = MenuId::for_slot(&MenuDate::try_new("2026-08-02").unwrap(), &lunch());
        // Planted under another day, so `find` misses it exactly as it would
        // in the instant before the rival's own row was visible.
        plant(&id, "1999-01-01", &db).await;

        let taken = publish("2026-08-02", &db)
            .await
            .expect_err("the id is taken");
        assert!(matches!(taken, AppError::Conflict(_)), "got {taken:?}");
        assert_eq!(refs(&db).await, 0);
    }

    /// Menus published before ids were derived keep a ULID key a new publish
    /// cannot collide with, so the pre-check is the only thing that can refuse.
    #[tokio::test]
    async fn a_legacy_ulid_menu_still_answers_taken() {
        let (db, _leases) = school().await;
        plant(&MenuId::from_key("01JLEGACYMENU"), "2026-08-02", &db).await;

        let taken = publish("2026-08-02", &db)
            .await
            .expect_err("that day and slot are taken");
        assert!(matches!(taken, AppError::Conflict(_)), "got {taken:?}");
        assert_eq!(refs(&db).await, 0);
    }

    /// The cap and the revision are one write. Nothing single-process can
    /// observe the torn state the two-query version left (a bumped revision
    /// over a cap that had not moved yet), so what is pinned here is the pair:
    /// the revision steps exactly once per cap move, and never for a PATCH that
    /// carried no cap at all — a booking re-reads only when something it priced
    /// itself against actually changed.
    #[tokio::test]
    async fn the_cap_and_the_revision_move_together_or_not_at_all() {
        let (db, _leases) = school().await;
        let menu = publish("2026-08-05", &db).await.unwrap();
        let id = menu.get_id().clone();
        assert_eq!(menu.get_version(), 0);

        let capped = update(&db, menu, Some(Some(5))).await.unwrap();
        assert_eq!(capped.get_capacity(), Some(5));
        assert_eq!(capped.get_version(), 1);
        // Re-read out of the store, never off the return value.
        let stored = read(&db, &id).await.unwrap().unwrap();
        assert_eq!((stored.get_capacity(), stored.get_version()), (Some(5), 1));

        // An empty PATCH moves neither, and reads the row back unchanged.
        let same = update(&db, stored, None).await.unwrap();
        assert_eq!((same.get_capacity(), same.get_version()), (Some(5), 1));

        // Clearing the cap is a move like any other.
        let uncapped = update(&db, same, Some(None)).await.unwrap();
        assert_eq!((uncapped.get_capacity(), uncapped.get_version()), (None, 2));
    }

    #[tokio::test]
    async fn deleting_hands_the_reference_back_in_the_same_step() {
        let (db, _leases) = school().await;
        let monday = publish("2026-08-03", &db).await.unwrap();
        publish("2026-08-04", &db).await.unwrap();
        assert_eq!(refs(&db).await, 2);

        let id = monday.get_id().clone();
        let ghost = monday.clone();
        delete(&db, monday).await.unwrap();
        assert!(read(&db, &id).await.unwrap().is_none());
        assert_eq!(refs(&db).await, 1, "the row and its reference go together");

        // Deleting what is already gone hands nothing back: a second release
        // would leave a slot one menu still uses free to be retired.
        let gone = delete(&db, ghost).await.expect_err("already deleted");
        assert!(matches!(gone, AppError::NotFound), "got {gone:?}");
        assert_eq!(refs(&db).await, 1);
        assert!(
            !retire_lunch(&db).await,
            "a slot a menu is still published for may not be retired"
        );

        // …and once the last menu goes, it may.
        let last = find(&db, &MenuDate::try_new("2026-08-04").unwrap(), &lunch())
            .await
            .unwrap()
            .unwrap();
        delete(&db, last).await.unwrap();
        assert_eq!(refs(&db).await, 0);
    }

    /// Nothing hanging off a menu may survive the delete that swept it — not a
    /// dish, and not an attendance mark. Menu ids are deterministic on
    /// (date, slot), so a survivor is not litter: republishing that meal mints
    /// the *same* id and the orphan comes back attached to the new menu — the
    /// kitchen reading "served" for a student who never came, or pricing the
    /// old menu's food.
    ///
    /// The window is between [`delete`]'s `DELETE` and its cascade: a
    /// child write committing in there is a phantom insert into a range the
    /// delete already swept on an older snapshot, so the sweep removes nothing
    /// and *both* commit. It is closed by making every child write **write** the
    /// menu row too ([`bump_menu_and_write`]) instead of reading it: the two
    /// transactions then touch one key and the store refuses to commit both.
    ///
    /// The window the schema event used to force open is the menu row's own
    /// lock now: every child write also writes the menu row, so a child racing
    /// the delete serializes on that row instead of committing into a range
    /// the sweep has already passed. A barrier start lets both orders happen;
    /// the invariant must hold in each.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_child_written_inside_a_delete_never_outlives_the_menu() {
        use crate::domain::menu_dish::DishDescription;

        let (db, _leases) = school().await;

        let (mut dishes, mut marks, mut swept) = (0, 0, 0);
        for round in 0..4 {
            let menu = publish(&format!("2026-09-{:02}", round + 1), &db)
                .await
                .unwrap();
            let id = menu.get_id().clone();

            let gate = std::sync::Arc::new(tokio::sync::Barrier::new(2));
            let drop_it = {
                let (db, gate, menu) = (db.clone(), gate.clone(), menu);
                tokio::spawn(async move {
                    gate.wait().await;
                    delete(&db, menu).await
                })
            };
            // One child per round, never both: whichever wrote first forces the
            // delete to re-send, and its second pass sweeps the other's row —
            // which would mask exactly the defect this test exists to catch.
            let child = {
                let (id, db, dish, gate) = (id.clone(), db.clone(), round % 2 == 0, gate);
                tokio::spawn(async move {
                    gate.wait().await;
                    if dish {
                        menu_dish::create(
                            &db,
                            &id,
                            DishName::try_new("Pilav").unwrap(),
                            None::<DishDescription>,
                            DishPrice::try_new(1).unwrap(),
                            DishTags::try_new(&[], &[]).unwrap(),
                            1_000,
                        )
                        .await
                        .map(|_| ())
                    } else {
                        meal_attendance::mark(
                            &db,
                            &id,
                            &UserId::from_key(TEACHER),
                            MealAttendanceStatus::try_new("served").unwrap(),
                            &UserId::from_key(TEACHER),
                        )
                        .await
                        .map(|_| ())
                    }
                })
            };
            let (drop_it, child) = (drop_it.await.unwrap(), child.await.unwrap());
            // A 404 for the child, or a 409/NotFound for the delete, is a
            // correct answer — the only defect is stored state.
            assert!(
                !matches!(child, Err(AppError::Db(_))),
                "round {round}: a raced child write must be answered, not 500: {child:?}"
            );

            // Stored state is the whole verdict; a return value is not evidence.
            if read(&db, &id).await.unwrap().is_none() {
                swept += 1;
                dishes += menu_dish::list_for_menu(&db, &id).await.unwrap().len();
                marks += meal_attendance::list_for_menu(&db, &id, None, 0)
                    .await
                    .unwrap()
                    .1;
            } else if drop_it.is_ok() {
                panic!("round {round}: the delete reported success but the menu is still there");
            }
        }
        eprintln!("Menu::delete raced by its children: {swept}/4 rounds deleted the menu");
        assert!(
            swept > 0,
            "no round ever deleted the menu, so the window was never reached"
        );
        assert_eq!(dishes, 0, "a dish outlived its menu");
        assert_eq!(marks, 0, "a mark outlived its menu");
    }
}
