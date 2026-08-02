//! A published menu: what the kitchen serves on one calendar day in one meal
//! slot. Dishes hang off it ([`MenuDish`](crate::domain::menu_dish::MenuDish)),
//! and bookings will hang off its `capacity`.
//!
//! Two things are deliberate:
//!
//! - **`date` is text** (`YYYY-MM-DD`), not a timestamp. The uniqueness rule is
//!   "one menu per day and slot", an equality test — and a midnight-in-millis
//!   day is only one school's day.
//! - **`slot` is a snapshot**, not a link into settings. Retiring a slot must
//!   never rewrite a menu already published under it.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use tokio::sync::Mutex;

use crate::constant::{
    MAX_MENU_CAPACITY, MEAL_ATTENDANCE_TABLE, MENU_DISH_TABLE, MENU_SEAT_COUNT_FIELD, MENU_TABLE,
    MENU_VERSION_FIELD, REF_COUNT_FIELD, SLOT_REF_TABLE,
};
use crate::database::{Database, transaction_with_retry, write_with_retry};
use crate::domain::cap;
use crate::domain::page::PagedList;
use crate::domain::settings::MealSlotDef;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// Serializes what is left that counts rows against one menu: the dish cap
/// (`MAX_DISHES_PER_MENU`, a count-then-write SurrealDB does not
/// conflict-check). Publishing no longer needs it — the day+slot *is* the
/// record id — and neither does a booking, a menu delete, or the settings
/// slot-removal guard: those went to conditional single-record writes
/// ([`crate::domain::cap`]), which the store decides as this lock cannot.
//
// ponytail: the dish cap therefore rests on this lock alone — a dish write
// added without taking it reopens the count-then-write hole silently. Closing
// it properly is another `cap` counter (`dish_count` on the menu row) plus its
// backfill; the ceiling is 51 dishes on a menu, not money or a seat, so it was
// not worth the column here.
///
/// **A leaf**: every dish write held under it moves the menu's revision inside
/// its *own* transaction ([`MenuDish::bump_menu_and_write`](crate::domain::menu_dish)),
/// so no path under this lock reaches `cap`'s counter lock any more — it did
/// while the bump was a `cap` call of its own. Should one ever need both, the
/// order is `MENU_LOCK` → `CLAIM_LOCK` and nothing may take this lock while
/// holding a counter lock: that is the invariant a new caller must keep, and it
/// is what would keep the pair deadlock-free.
pub(crate) static MENU_LOCK: Mutex<()> = Mutex::const_new(());

/// The reference counter for one meal slot — how many menus are published under
/// that name, and whether the school has retired it (see
/// [`crate::domain::cap`]). The mirror of
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
        let digits =
            |from: usize, to: usize| -> Option<u32> { value.get(from..to)?.parse::<u32>().ok() };
        let (Some(_year), Some(month), Some(day)) = (digits(0, 4), digits(5, 7), digits(8, 10))
        else {
            return Err(invalid);
        };
        // Day count per month is not checked: the calendar rules (leap years)
        // would be the only thing this file knows about time, and a 31st of
        // February menu is a typo nobody can act on, not a data hazard.
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return Err(invalid);
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
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
    id: MenuId,
    date: MenuDate,
    slot: MenuSlot,
    capacity: Option<i64>,
    /// The menu's revision (see [`MENU_VERSION_FIELD`]). Absent on rows written
    /// before the column existed, which reads as revision zero — the same thing
    /// `(version ?? 0)` says in the claim's `WHERE`. The seat counter is
    /// deliberately *not* here: it is the database's to own, and a whole-row
    /// save must never carry a stale copy of it.
    version: Option<i64>,
    created_by: UserId,
    created_at: Timestamp,
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

    /// Publish a menu. Refused (409) when the day+slot already carries one.
    ///
    /// The day and slot *are* the record id ([`MenuId::for_slot`]), so the
    /// refusal is decided by the store rather than by a check a concurrent
    /// publish can outrun: two publishes of the same meal write one id, and the loser's
    /// "already exists" becomes the same 409. The pre-check stays for the
    /// ordinary case — and for menus published before ids were derived, whose
    /// ULID key no new publish can collide with.
    pub async fn create(
        date: MenuDate,
        slot: MenuSlot,
        capacity: Option<i64>,
        created_by: &UserId,
        db: &Database,
    ) -> Result<Menu, AppError> {
        let taken = AppError::Conflict("a menu is already published for that date and slot");
        if Self::find(&date, &slot, db).await?.is_some() {
            return Err(taken);
        }
        // The menu takes a reference on its slot, which is what stops the slot
        // being dropped from the settings while this menu (whose slot is only
        // snapshotted text) still points at it. Claimed *inside* the write's own
        // transaction: a claim of its own could land and the row then fail to,
        // leaving the slot counted by a menu that does not exist — a slot
        // nobody can ever retire, and no crash window is small enough for that.
        let counter = slot_ref(slot.as_str());
        let menu = Menu {
            id: MenuId::for_slot(&date, &slot),
            date,
            slot,
            capacity,
            version: Some(0),
            created_by: created_by.clone(),
            created_at: Timestamp::now(),
        };
        let id = menu.id.record();
        match cap::claim_ref_and_create(&counter, 1, &id, &menu, db).await? {
            cap::ClaimedRef::Made(created) => Ok(created),
            cap::ClaimedRef::Duplicate => Err(taken),
            cap::ClaimedRef::Retired => Err(AppError::ConflictOwned(format!(
                "the '{}' meal slot has been removed from the school's settings",
                menu.slot.as_str()
            ))),
        }
    }

    pub async fn read(id: &MenuId, db: &Database) -> Result<Option<Menu>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// The menu for one day and slot, if any — the uniqueness check.
    pub async fn find(
        date: &MenuDate,
        slot: &MenuSlot,
        db: &Database,
    ) -> Result<Option<Menu>, AppError> {
        let mut result = db
            .query("SELECT * FROM menu WHERE date = $date AND slot = $slot LIMIT 1")
            .bind(("date", date.as_str().to_string()))
            .bind(("slot", slot.as_str().to_string()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Menu>>(0)?.into_iter().next())
    }

    /// Menus, newest day first. `from`/`to` are inclusive `YYYY-MM-DD` bounds;
    /// either may be omitted. The comparison is lexical, which is chronological
    /// for this format.
    pub async fn list(
        from: Option<&MenuDate>,
        to: Option<&MenuDate>,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<Menu>, i64), AppError> {
        PagedList::new(
            "menu WHERE ($from = NONE OR date >= $from) AND ($to = NONE OR date <= $to)",
            "ORDER BY date DESC, slot ASC, id DESC",
        )
        .bind("from", from.map(|date| date.as_str().to_string()))
        .bind("to", to.map(|date| date.as_str().to_string()))
        .run(limit, offset, db)
        .await
    }

    /// Only `capacity` is writable: `date` and `slot` are `READONLY` columns,
    /// because moving a published menu to another day is a different menu.
    /// `None` keeps the stored cap, `Some(None)` clears it back to uncapped.
    ///
    /// The revision moves **in the same statement** as the cap, indivisibly: a
    /// booking claims its seat against the revision
    /// it read the cap at, so a shrink that bumped in a query of its own left a
    /// window where the row carried the *new* revision and the *old* cap — and
    /// a booking arriving there passes a CAS meant to refuse it, over-admitting
    /// by exactly the seats in flight.
    pub async fn update(
        self,
        capacity: Option<Option<i64>>,
        db: &Database,
    ) -> Result<Menu, AppError> {
        let Some(capacity) = capacity else {
            // A PATCH carrying nothing writes nothing and bumps nothing: no
            // booking's price or cap has moved, so none owes a re-read.
            return Self::read(&self.id, db).await?.ok_or(AppError::NotFound);
        };
        let sql = format!(
            "UPDATE $id SET capacity = $capacity, \
             {MENU_VERSION_FIELD} = ({MENU_VERSION_FIELD} ?? 0) + 1 RETURN AFTER"
        );
        // One counter write in flight at a time, like every other one.
        let _guard = cap::counter_lock().await;
        write_with_retry::<Menu>(
            db,
            &sql,
            &[
                ("id".into(), self.id.record().into_value()),
                ("capacity".into(), capacity.into_value()),
            ],
        )
        .await?
        .into_iter()
        .next()
        .ok_or(AppError::NotFound)
    }

    /// Delete the menu and the dishes on it — a dish has no meaning without
    /// its menu, and the `menu` link is `READONLY`, so it cannot be re-homed.
    ///
    /// Refused (409) while a seat is still held, and the *row itself* decides
    /// that: the delete carries the seat counter in its `WHERE`, so a booking
    /// landing at that instant either takes its seat before
    /// the delete (which then finds a non-zero counter and refuses) or after it
    /// (and finds no menu). A read-then-delete pair had a window where both
    /// happened — a paid seat on a menu that no longer exists.
    ///
    /// The slot gets its reference back in that same transaction — a slot no
    /// menu is published for any more may leave the settings again, and the
    /// `FOR` runs only over a row the guard actually took. Released afterwards
    /// in a query of its own, a crash between the two left the slot counted by
    /// a menu that no longer exists: a slot nobody can retire.
    ///
    /// The marks go in that same `FOR`, for a sharper reason than tidiness:
    /// [`MenuId::for_slot`] is deterministic, so republishing the same day and
    /// slot mints the *same* record id. A mark left behind would come back as a
    /// mark on the new menu — the kitchen reading "served" for a student who
    /// never came — and until then the student's own report cites a menu that
    /// is gone. Attendance carries no money and no counter, so it is safe to
    /// drop; cancelled *bookings* deliberately stay, because their `attempt`
    /// counter is what keeps the ledger's `(booking, attempt)` keys unique. Cut
    /// those and a re-book on a republished menu would reuse a charge id the
    /// ledger already holds, and the append (idempotent by design) would bill
    /// the seat nothing.
    ///
    /// The dishes ride in that `FOR` too, and for the same reason as the marks:
    /// run after the transaction committed, a crash between the two orphaned
    /// them on a record id republishing the day and slot mints again — a new
    /// menu serving (and pricing) the old one's food.
    pub async fn delete(self, db: &Database) -> Result<Menu, AppError> {
        let sql = format!(
            "BEGIN TRANSACTION;
             LET $gone = (DELETE $id WHERE ({MENU_SEAT_COUNT_FIELD} ?? 0) = 0 RETURN BEFORE);
             FOR $row IN ($gone ?? []) {{
                 UPSERT $counter SET {REF_COUNT_FIELD} = \
                     math::max([({REF_COUNT_FIELD} ?? 0) - 1, 0]);
                 DELETE {MEAL_ATTENDANCE_TABLE} WHERE menu = $id;
                 DELETE {MENU_DISH_TABLE} WHERE menu = $id;
             }};
             RETURN $gone;
             COMMIT TRANSACTION;"
        );
        // `slot` is a `READONLY` column, so this counter is the one the deleted
        // row carries. Nothing here can answer "already exists" — the `UPSERT`
        // is keyed by a slot name on a table with no `UNIQUE` index, so it
        // resolves onto the row it names ([`transaction_with_retry`]).
        let (mut result, mut errors) = transaction_with_retry(
            db,
            &sql,
            &[
                ("id".into(), self.id.record().into_value()),
                ("counter".into(), slot_ref(self.slot.as_str()).into_value()),
            ],
            &[],
        )
        .await?;
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        // BEGIN, the LET and the FOR take a slot each.
        let Some(deleted) = result.take::<Vec<Menu>>(3)?.into_iter().next() else {
            // Nothing back: either seats are held, or the menu is already gone.
            return Err(match Self::read(&self.id, db).await? {
                Some(_) => AppError::Conflict("the menu still has live bookings"),
                None => AppError::NotFound,
            });
        };
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_accepts_only_padded_calendar_days() {
        assert!(MenuDate::try_new("2026-07-26").is_ok());
        assert!(MenuDate::try_new("2026-7-26").is_err());
        assert!(MenuDate::try_new("2026-13-01").is_err());
        assert!(MenuDate::try_new("2026-00-01").is_err());
        assert!(MenuDate::try_new("2026-01-32").is_err());
        assert!(MenuDate::try_new("26-01-02").is_err());
        assert!(MenuDate::try_new("").is_err());
        // Sorting the text must sort the calendar — the range filter relies on it.
        assert!(
            MenuDate::try_new("2026-01-09").unwrap() < MenuDate::try_new("2026-01-10").unwrap()
        );
        assert!(
            MenuDate::try_new("2026-09-01").unwrap() < MenuDate::try_new("2026-10-01").unwrap()
        );
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

    // --- the slot reference and the menu row move together ---------------

    async fn school() -> Database {
        let db = crate::database::init_mem().await.unwrap();
        db.query("CREATE user:teacher SET username = 'teacher', password_hash = 'x';")
            .await
            .unwrap()
            .check()
            .unwrap();
        db
    }

    fn lunch() -> MenuSlot {
        MenuSlot::try_new("lunch", &[MealSlotDef::try_new("lunch", None).unwrap()]).unwrap()
    }

    /// The reference count, re-read out of the store — never off a return
    /// value, which the in-memory engine forges wins on.
    async fn refs(db: &Database) -> i64 {
        let mut result = db
            .query("SELECT VALUE (count ?? 0) FROM $id")
            .bind(("id", slot_ref("lunch")))
            .await
            .unwrap()
            .check()
            .unwrap();
        result
            .take::<Vec<i64>>(0)
            .unwrap()
            .into_iter()
            .next()
            .unwrap_or(0)
    }

    async fn publish(date: &str, db: &Database) -> Result<Menu, AppError> {
        Menu::create(
            MenuDate::try_new(date).unwrap(),
            lunch(),
            None,
            &UserId::from_key("teacher"),
            db,
        )
        .await
    }

    /// Plant a menu row the pre-check will (or will not) find, without going
    /// through the claim — a row published before the counter existed.
    async fn plant(id: &MenuId, date: &str, db: &Database) {
        db.query(
            "CREATE $id SET date = $date, slot = 'lunch', \
             created_by = user:teacher, created_at = 1",
        )
        .bind(("id", id.record()))
        .bind(("date", date.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
    }

    #[tokio::test]
    async fn publishing_lands_the_menu_and_its_reference_together() {
        let db = school().await;
        let menu = publish("2026-08-02", &db).await.unwrap();
        assert!(Menu::read(menu.get_id(), &db).await.unwrap().is_some());
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
        let db = school().await;
        assert!(cap::retire(&slot_ref("lunch"), &db).await.unwrap());

        let refused = publish("2026-08-02", &db)
            .await
            .expect_err("the slot left the settings");
        assert!(
            matches!(refused, AppError::ConflictOwned(_)),
            "got {refused:?}"
        );
        assert_eq!(refs(&db).await, 0);
        assert!(
            Menu::find(&MenuDate::try_new("2026-08-02").unwrap(), &lunch(), &db)
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
        let db = school().await;
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
        let db = school().await;
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
        let db = school().await;
        let menu = publish("2026-08-05", &db).await.unwrap();
        let id = menu.get_id().clone();
        assert_eq!(menu.get_version(), 0);

        let capped = menu.update(Some(Some(5)), &db).await.unwrap();
        assert_eq!(capped.get_capacity(), Some(5));
        assert_eq!(capped.get_version(), 1);
        // Re-read out of the store, never off the return value.
        let stored = Menu::read(&id, &db).await.unwrap().unwrap();
        assert_eq!((stored.get_capacity(), stored.get_version()), (Some(5), 1));

        // An empty PATCH moves neither, and reads the row back unchanged.
        let same = stored.update(None, &db).await.unwrap();
        assert_eq!((same.get_capacity(), same.get_version()), (Some(5), 1));

        // Clearing the cap is a move like any other.
        let uncapped = same.update(Some(None), &db).await.unwrap();
        assert_eq!((uncapped.get_capacity(), uncapped.get_version()), (None, 2));
    }

    #[tokio::test]
    async fn deleting_hands_the_reference_back_in_the_same_step() {
        let db = school().await;
        let monday = publish("2026-08-03", &db).await.unwrap();
        publish("2026-08-04", &db).await.unwrap();
        assert_eq!(refs(&db).await, 2);

        let id = monday.get_id().clone();
        let ghost = monday.clone();
        monday.delete(&db).await.unwrap();
        assert!(Menu::read(&id, &db).await.unwrap().is_none());
        assert_eq!(refs(&db).await, 1, "the row and its reference go together");

        // Deleting what is already gone hands nothing back: a second release
        // would leave a slot one menu still uses free to be retired.
        let gone = ghost.delete(&db).await.expect_err("already deleted");
        assert!(matches!(gone, AppError::NotFound), "got {gone:?}");
        assert_eq!(refs(&db).await, 1);
        assert!(
            !cap::retire(&slot_ref("lunch"), &db).await.unwrap(),
            "a slot a menu is still published for may not be retired"
        );

        // …and once the last menu goes, it may.
        let last = Menu::find(&MenuDate::try_new("2026-08-04").unwrap(), &lunch(), &db)
            .await
            .unwrap()
            .unwrap();
        last.delete(&db).await.unwrap();
        assert_eq!(refs(&db).await, 0);
    }
}
