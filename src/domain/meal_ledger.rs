//! The food program's money: an **append-only** ledger of what a student was
//! charged and what they paid.
//!
//! Nothing here ever `UPDATE`s or `DELETE`s a row, and no such path exists —
//! every field is `READONLY` in the schema as well. A ledger line that can be
//! edited or dropped silently rewrites a student's financial history with no
//! trace of the rewrite; a mistake is corrected by appending the opposing
//! line, which leaves both the mistake and the correction visible.
//!
//! **The balance is never stored.** It is always the fold
//!
//! ```text
//! balance = SUM(credit) + SUM(reversal) - SUM(charge)
//! ```
//!
//! so a *negative* balance means the student owes the school, and a positive
//! one is money on account. Every amount on a row is stored positive; the sign
//! lives in the `kind`, so a line's meaning never depends on how it was read.
//!
//! Three more rules the code depends on:
//!
//! - **Booking is the charge trigger, at a price snapshot.** The menu's dishes
//!   are summed when the seat is taken and that number is frozen onto the
//!   *booking row*. Editing a dish's price afterwards moves no existing charge
//!   — what a student owes is what the menu cost the day they booked — and a
//!   seat taken while the menu was free stays free, because "free" is recorded
//!   on the row rather than inferred from the absence of a charge.
//! - **A cancel appends a `reversal`**, for the charge's exact amount, with
//!   `source` pointing at the charge it undoes. The charge row stays. Booking
//!   again after a cancel is a *fresh* charge at the then-current price.
//! - **Every booking line is keyed by `(booking, attempt)`.** Both the charge
//!   and its reversal derive their record id from the seat and the attempt
//!   number, so replaying either writes nothing at all. Money must never
//!   depend on a "has this been billed yet?" scan: two concurrent `POST`s of
//!   one seat can both read "no charge yet" and both append, which is how a
//!   double-click used to bill a seat twice.
//! - **A no-show still pays.** Meal attendance has zero billing effect —
//!   nothing in this file reads or writes it. Do not add a no-show penalty
//!   here: the seat was reserved and the food was cooked.
//!
//! Money is `i64` minor units (kuruş) end to end. No float, no decimal, ever.

use std::sync::LazyLock;

use surrealdb::types::{AlreadyExistsError, RecordId, RecordIdKey, SurrealValue};
use ulid::Generator;

use crate::constant::{
    CAS_UPDATE_RETRIES, MAX_LEDGER_AMOUNT_MINOR, MAX_LEDGER_METHOD_LEN, MAX_LEDGER_NOTE_LEN,
    MEAL_LEDGER_TABLE,
};
use crate::database::{Database, lost_the_race};
use crate::domain::meal_booking::{MealBooking, MealBookingId};
use crate::domain::menu::MenuId;
use crate::domain::menu_dish::MenuDish;
use crate::domain::page::PagedList;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};
use crate::validate::validate_optional;

/// Mints ledger ids in write order — `Ulid::new()`'s random low bits sort
/// arbitrarily within one millisecond, which would scramble the `id` tie-break
/// of the newest-first statement below.
static IDS: LazyLock<std::sync::Mutex<Generator>> =
    LazyLock::new(|| std::sync::Mutex::new(Generator::new()));

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct MealLedgerId(RecordId);

impl MealLedgerId {
    pub fn generate() -> Self {
        let mut ids = IDS.lock().expect("meal ledger id generator poisoned");
        // The only error is exhausting the random bits within one millisecond
        // (2^80 ids deep); it clears itself as the clock ticks, so retry.
        let ulid = loop {
            if let Ok(ulid) = ids.generate() {
                break ulid;
            }
        };
        Self(RecordId::new(MEAL_LEDGER_TABLE, ulid.to_string()))
    }

    /// The one line a booking attempt may ever write of this `kind` — a
    /// deterministic id, exactly like [`MealBookingId::composite`]. Two racing
    /// `POST`s of one seat derive the *same* id and so cannot become two
    /// charges: idempotence rests on identity, never on a scan that a
    /// concurrent writer can slip past. The `attempt` counter is what keeps a
    /// re-book after a cancel a genuinely fresh charge. Booking keys are
    /// `<ulid>_<ulid>`, so the `c`/`r` marker keeps the two kinds apart.
    pub fn for_attempt(booking: &MealBookingId, attempt: i64, kind: MealLedgerKind) -> Self {
        let marker = match kind {
            MealLedgerKind::Charge => 'c',
            MealLedgerKind::Reversal => 'r',
            // A credit is money arriving out of the blue, tied to no booking.
            MealLedgerKind::Credit => 'k',
        };
        Self(RecordId::new(
            MEAL_LEDGER_TABLE,
            format!("{}_{marker}{attempt}", booking.key()),
        ))
    }

    pub fn record(&self) -> RecordId {
        self.0.clone()
    }

    pub fn key(&self) -> &str {
        key_of(&self.0)
    }
}

/// Did this `CREATE` fail *only* because the row is already there? Matched on
/// the SDK's typed `AlreadyExists`/`Record` detail — never on the message text
/// and never on "any database error", because swallowing a real fault in money
/// code would be far worse than the 500 it saves.
fn is_duplicate_record(error: &surrealdb::Error) -> bool {
    matches!(
        error.already_exists_details(),
        Some(AlreadyExistsError::Record { .. })
    )
}

/// The bare key of a record id — how every id leaves this API.
fn key_of(record: &RecordId) -> &str {
    match &record.key {
        RecordIdKey::String(key) => key,
        _ => "",
    }
}

/// What a line means. `untagged` + `rename_all` store it as the bare lowercase
/// string the `kind` column types as, in lockstep with `MEAL_LEDGER_KINDS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, SurrealValue)]
#[surreal(untagged, rename_all = "lowercase")]
pub enum MealLedgerKind {
    Charge,
    Credit,
    Reversal,
}

impl MealLedgerKind {
    /// The wire/storage form. Must stay in lockstep with `rename_all`.
    pub fn as_str(self) -> &'static str {
        match self {
            MealLedgerKind::Charge => "charge",
            MealLedgerKind::Credit => "credit",
            MealLedgerKind::Reversal => "reversal",
        }
    }

    /// How the line folds into the balance: a charge is the only thing that
    /// takes money away. This is the *single* place the sign convention lives.
    fn sign(self) -> i64 {
        match self {
            MealLedgerKind::Charge => -1,
            MealLedgerKind::Credit | MealLedgerKind::Reversal => 1,
        }
    }
}

/// One line's amount, always positive — the sign is the [`MealLedgerKind`]'s
/// business. Capped so a slipped keystroke cannot book a fortune.
#[derive(Debug, Clone, Copy, PartialEq, Eq, SurrealValue)]
pub struct LedgerAmount(i64);

impl LedgerAmount {
    pub fn try_new(value: i64) -> Result<Self, ValidationError> {
        if !(1..=MAX_LEDGER_AMOUNT_MINOR).contains(&value) {
            return Err(ValidationError::Invalid {
                field: "amount_minor",
                reason: "must be between 1 and 10000000 minor units",
            });
        }
        Ok(Self(value))
    }

    pub fn as_minor(self) -> i64 {
        self.0
    }
}

/// How the money arrived ("cash", "havale", …). Free text: the backend never
/// speaks to a payment gateway and stores no card data, ever.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct LedgerMethod(String);

impl LedgerMethod {
    pub fn try_new(value: &str) -> Result<Option<Self>, ValidationError> {
        let value = value.trim();
        validate_optional("method", value, MAX_LEDGER_METHOD_LEN)?;
        Ok((!value.is_empty()).then(|| Self(value.to_string())))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The bookkeeper's own words about a line — a receipt number, whose envelope
/// it came in.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct LedgerNote(String);

impl LedgerNote {
    pub fn try_new(value: &str) -> Result<Option<Self>, ValidationError> {
        let value = value.trim();
        validate_optional("note", value, MAX_LEDGER_NOTE_LEN)?;
        Ok((!value.is_empty()).then(|| Self(value.to_string())))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct MealLedger {
    id: MealLedgerId,
    student: UserId,
    kind: MealLedgerKind,
    amount_minor: LedgerAmount,
    /// What caused the line: a charge points at its `meal_booking`, a reversal
    /// at the `meal_ledger` charge it undoes. Untyped, hence a bare `RecordId`.
    source: Option<RecordId>,
    method: Option<LedgerMethod>,
    note: Option<LedgerNote>,
    recorded_by: UserId,
    created_at: Timestamp,
}

impl MealLedger {
    pub fn get_id(&self) -> &MealLedgerId {
        &self.id
    }

    pub fn get_student(&self) -> &UserId {
        &self.student
    }

    pub fn get_kind(&self) -> MealLedgerKind {
        self.kind
    }

    pub fn get_amount_minor(&self) -> LedgerAmount {
        self.amount_minor
    }

    /// The cause's bare key; which table it lives in follows from the kind.
    pub fn get_source_key(&self) -> Option<&str> {
        self.source.as_ref().map(key_of)
    }

    pub fn get_method(&self) -> Option<&LedgerMethod> {
        self.method.as_ref()
    }

    pub fn get_note(&self) -> Option<&LedgerNote> {
        self.note.as_ref()
    }

    pub fn get_recorded_by(&self) -> &UserId {
        &self.recorded_by
    }

    pub fn get_created_at(&self) -> Timestamp {
        self.created_at
    }

    /// The only writer: one `CREATE`, no update path anywhere in this module.
    ///
    /// A line whose id already exists is left exactly as it is — the row wins,
    /// the write is dropped. That is what makes a deterministic id (see
    /// [`MealLedgerId::for_attempt`]) an idempotence key: replaying the same
    /// append is a no-op, never a second line and never an edit of the first.
    /// The point read is on the id itself, so unlike a scan it cannot miss a
    /// row a concurrent writer just made — but it is only a fast path. The
    /// guarantee is `CREATE`'s own: on an existing id it *errors* and leaves
    /// the row untouched, so the writer that lost the race reads back the
    /// winner's line instead of failing the request with a 500.
    ///
    /// A *write conflict* is the same race decided one layer down — two
    /// appends of one id arriving together are no longer serialized by a
    /// process-wide lock, so the store aborts one as retryable instead of
    /// answering it "already exists". Both are read back the same way, and a
    /// conflict that turns out to have written nothing is simply tried again;
    /// no path here can write a second line, since the id is the key.
    async fn append(row: MealLedger, db: &Database) -> Result<MealLedger, AppError> {
        if let Some(existing) = Self::read(&row.id, db).await? {
            return Ok(existing);
        }
        let id = row.id.clone();
        for _ in 0..CAS_UPDATE_RETRIES {
            match db.create(id.record()).content(row.clone()).await {
                Ok(Some(created)) => return Ok(created),
                Ok(None) => break,
                Err(e) if is_duplicate_record(&e) || lost_the_race(&e) => {
                    if let Some(existing) = Self::read(&id, db).await? {
                        return Ok(existing);
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(AppError::Internal("failed to write the ledger line".into()))
    }

    pub async fn read(id: &MealLedgerId, db: &Database) -> Result<Option<MealLedger>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// What a seat on `menu` costs *right now*: the sum of its dishes. `None`
    /// when the menu is free (or empty) — a zero line is noise, not history.
    ///
    /// Called before the seat is taken so an unchargeable menu (one summing
    /// past `MAX_LEDGER_AMOUNT_MINOR`) refuses the booking outright instead of
    /// leaving a booked-but-unbilled row behind.
    pub async fn price_snapshot(
        menu: &MenuId,
        db: &Database,
    ) -> Result<Option<LedgerAmount>, AppError> {
        let total = MenuDish::list_for_menu(menu, db)
            .await?
            .iter()
            .fold(0i64, |sum, dish| {
                sum.saturating_add(dish.get_price_minor().as_minor())
            });
        if total == 0 {
            return Ok(None);
        }
        Ok(Some(LedgerAmount::try_new(total)?))
    }

    /// Bill `booking`'s current attempt at the price frozen onto it when that
    /// attempt took the seat.
    ///
    /// The price comes off the *booking row*, never off the menu as it stands
    /// now: a seat taken while the menu was free carries `None` forever, so
    /// "was free" is a recorded fact and a later dish never bills a seat
    /// retroactively. Called from [`MealBooking::book`] alone, right after the seat
    /// was claimed, so the seat and its money move together.
    ///
    /// Replaying it is free: the id is `(booking, attempt)`, so a duplicate
    /// `POST` writes nothing, and an attempt whose charge failed the first time
    /// self-heals on the next `POST` of the same seat.
    pub async fn charge_booking(
        booking: &MealBooking,
        recorded_by: &UserId,
        db: &Database,
    ) -> Result<(), AppError> {
        let Some(amount) = booking.get_price_minor() else {
            return Ok(());
        };
        Self::append(
            MealLedger {
                id: MealLedgerId::for_attempt(
                    booking.get_id(),
                    booking.get_attempt(),
                    MealLedgerKind::Charge,
                ),
                student: booking.get_student().clone(),
                kind: MealLedgerKind::Charge,
                amount_minor: amount,
                source: Some(booking.get_id().record()),
                method: None,
                note: None,
                recorded_by: recorded_by.clone(),
                created_at: Timestamp::now(),
            },
            db,
        )
        .await?;
        Ok(())
    }

    /// Give the money back for a cancelled `booking`: a new `reversal` line for
    /// the charge's exact amount, pointing at it. The charge is never touched.
    /// A booking that was never billed (a free menu) reverses nothing, and
    /// neither does one whose charge never landed — the reversal is keyed to
    /// the very charge it undoes, so it can only exist alongside it.
    ///
    /// Keyed by `(booking, attempt)` like the charge, so a retried cancel
    /// refunds once. A cancel cut short between the status flip and this
    /// reversal is recovered by repeating the cancel — which is precisely why
    /// [`MealBooking::cancel`] replays this on an already-cancelled row instead
    /// of refusing it. Nothing else on the API can append the missing line.
    pub async fn reverse_booking(
        booking: &MealBooking,
        recorded_by: &UserId,
        db: &Database,
    ) -> Result<(), AppError> {
        let Some(amount) = booking.get_price_minor() else {
            return Ok(());
        };
        let charge = MealLedgerId::for_attempt(
            booking.get_id(),
            booking.get_attempt(),
            MealLedgerKind::Charge,
        );
        if Self::read(&charge, db).await?.is_none() {
            return Ok(());
        }
        Self::append(
            MealLedger {
                id: MealLedgerId::for_attempt(
                    booking.get_id(),
                    booking.get_attempt(),
                    MealLedgerKind::Reversal,
                ),
                student: booking.get_student().clone(),
                kind: MealLedgerKind::Reversal,
                amount_minor: amount,
                source: Some(charge.record()),
                method: None,
                note: None,
                recorded_by: recorded_by.clone(),
                created_at: Timestamp::now(),
            },
            db,
        )
        .await?;
        Ok(())
    }

    /// Money in: a payment received, or an opening balance.
    pub async fn credit(
        student: &UserId,
        amount_minor: LedgerAmount,
        method: Option<LedgerMethod>,
        note: Option<LedgerNote>,
        recorded_by: &UserId,
        db: &Database,
    ) -> Result<MealLedger, AppError> {
        Self::append(
            MealLedger {
                id: MealLedgerId::generate(),
                student: student.clone(),
                kind: MealLedgerKind::Credit,
                amount_minor,
                source: None,
                method,
                note,
                recorded_by: recorded_by.clone(),
                created_at: Timestamp::now(),
            },
            db,
        )
        .await
    }

    /// A student's whole statement, newest first.
    pub async fn list_for_student(
        student: &UserId,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<MealLedger>, i64), AppError> {
        PagedList::new(
            "meal_ledger WHERE student = $student",
            "ORDER BY created_at DESC, id DESC",
        )
        .bind("student", student.record())
        .run(limit, offset, db)
        .await
    }

    /// The derived balance: `credits + reversals - charges`, in minor units.
    /// Negative means the student owes the school. Never stored anywhere.
    // ponytail: folds the student's lines in-process (a few hundred a year);
    // push it into a `math::sum` aggregate if a statement ever gets long.
    pub async fn balance_of(student: &UserId, db: &Database) -> Result<i64, AppError> {
        Ok(Self::list_for_student(student, None, 0, db)
            .await?
            .0
            .iter()
            .fold(0i64, |sum, line| {
                sum + line.kind.sign() * line.amount_minor.as_minor()
            }))
    }
}

#[cfg(test)]
mod tests {
    use surrealdb::types::SurrealValue as _;
    use surrealdb::types::Value;

    use super::*;

    /// The `kind` column is `TYPE string`: an object-wrapped enum would be
    /// rejected on write, and the `kind = 'charge'` lookup would silently
    /// match nothing — which would double-charge every re-booking.
    #[test]
    fn kind_stores_as_a_bare_string() {
        for kind in [
            MealLedgerKind::Charge,
            MealLedgerKind::Credit,
            MealLedgerKind::Reversal,
        ] {
            let value = kind.into_value();
            assert_eq!(value, Value::String(kind.as_str().to_string()));
            assert_eq!(MealLedgerKind::from_value(value).unwrap(), kind);
        }
    }

    /// The one fold: a charge subtracts, a credit and a reversal add. A booked
    /// meal that was cancelled nets to exactly zero, never to a rounding scrap.
    #[test]
    fn the_balance_fold_is_credit_plus_reversal_minus_charge() {
        assert_eq!(MealLedgerKind::Charge.sign(), -1);
        assert_eq!(MealLedgerKind::Credit.sign(), 1);
        assert_eq!(MealLedgerKind::Reversal.sign(), 1);
        let lines = [
            (MealLedgerKind::Credit, 10_000),
            (MealLedgerKind::Charge, 4_500),
            (MealLedgerKind::Reversal, 4_500),
        ];
        let balance: i64 = lines
            .iter()
            .map(|(kind, amount)| kind.sign() * amount)
            .sum();
        assert_eq!(balance, 10_000);
    }

    #[test]
    fn an_amount_is_positive_and_capped() {
        assert!(LedgerAmount::try_new(0).is_err());
        assert!(LedgerAmount::try_new(-1).is_err());
        assert!(LedgerAmount::try_new(MAX_LEDGER_AMOUNT_MINOR + 1).is_err());
        assert_eq!(LedgerAmount::try_new(1).unwrap().as_minor(), 1);
    }
}
