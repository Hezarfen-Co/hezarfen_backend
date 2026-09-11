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
//!   `source` pointing at the charge it undoes, *in the same transaction as the
//!   flip that frees the seat*. The charge row stays. Booking again after a
//!   cancel is a *fresh* charge at the then-current price — and since every
//!   line is keyed by the attempt, a reversal that landed a transaction later
//!   than its flip could be overtaken by that re-book and then never be
//!   writable at all.
//! - **Every booking line is keyed by `(booking, attempt)`.** Both the charge
//!   and its reversal derive their record id from the seat and the attempt
//!   number, so replaying either writes nothing at all. Money must never
//!   depend on a "has this been billed yet?" scan: two concurrent `POST`s of
//!   one seat can both read "no charge yet" and both append, which is how a
//!   double-click used to bill a seat twice.
//! - **A credit may be keyed too.** `POST /meals/credits` takes an optional
//!   client-chosen `request_key`, which lands in the line's id
//!   ([`MealLedgerId::for_request`]) and makes the call retry-safe by the same
//!   identity rule: a retry after a timeout resolves the line it already wrote.
//!   Without one, a fresh ulid is minted and a resent request is a second
//!   credit — which nothing here can edit or delete afterwards.
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
use crate::db::page::PagedList;
// The client-chosen idempotence key both ledgers take — one grammar, one
// validator, one type, rather than a second newtype that could drift from it.
use crate::domain::payment_ledger::PaymentRequestKey;
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

    /// The one credit a `(student, request_key)` pair may ever have. Same trick
    /// as [`PaymentLedgerId::for_request`](crate::domain::payment_ledger::PaymentLedgerId::for_request),
    /// scoped by the student because a credit points at no line of its own: one
    /// office's "receipt-114" cannot land on another student's account, and a
    /// key replayed for the wrong student cannot resolve to this line at all.
    ///
    /// The grammar parses uniquely because `_` joins the parts. A booking line
    /// is `<date>_<slot>_<student>_c<n>`, whose first part carries the `-` of a
    /// `YYYY-MM-DD` day, and an unkeyed line is a bare ULID; a credit's first
    /// part is a student ULID (`[0-9A-Z]`) and a `request_key` is
    /// [`crate::validate::validate_request_key`]'s `[A-Za-z0-9-]`, which cannot
    /// spell a separator plus a marker. No key can therefore derive an id some
    /// other line owns — a collision would hand money to the wrong row.
    pub fn for_request(student: &UserId, key: &PaymentRequestKey) -> Self {
        Self(RecordId::new(
            MEAL_LEDGER_TABLE,
            format!("{}_k_{}", student.key(), key.as_str()),
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
        let Some(line) = Self::charge_for(booking, recorded_by) else {
            return Ok(());
        };
        Self::append(line, db).await?;
        Ok(())
    }

    /// The charge `booking`'s current attempt owes — the line
    /// [`MealBooking::claim_and_place`] appends inside the very transaction that
    /// takes the seat. `None` when the menu was free, which owes no line.
    ///
    /// Only the row is built here; whether it is already there is a read, and
    /// the caller that writes in one transaction has to make it there. Note the
    /// attempt has to be settled *before* the SQL runs, which is why the claim
    /// mints the number itself rather than letting the revival increment it.
    pub(crate) fn charge_for(booking: &MealBooking, recorded_by: &UserId) -> Option<MealLedger> {
        let amount = booking.get_price_minor()?;
        Some(MealLedger {
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
        })
    }

    /// Give the money back for a cancelled `booking`: a new `reversal` line for
    /// the charge's exact amount, pointing at it. The charge is never touched.
    /// A booking that was never billed (a free menu) reverses nothing, and
    /// neither does one whose charge never landed — the reversal is keyed to
    /// the very charge it undoes, so it can only exist alongside it.
    ///
    /// Keyed by `(booking, attempt)` like the charge, so a retried cancel
    /// refunds once. The line normally lands *inside* the flip's own
    /// transaction ([`MealBooking::release_seat`]); this path is what heals a
    /// seat flipped before that was true, and it is why
    /// [`MealBooking::cancel`] replays it on an already-cancelled row instead
    /// of refusing it. Nothing else on the API can append the missing line.
    pub async fn reverse_booking(
        booking: &MealBooking,
        recorded_by: &UserId,
        db: &Database,
    ) -> Result<(), AppError> {
        let Some((charge, line)) = Self::reversal_for(booking, recorded_by) else {
            return Ok(());
        };
        if Self::read(&charge, db).await?.is_none() {
            return Ok(());
        }
        Self::append(line, db).await?;
        Ok(())
    }

    /// The refund `booking`'s current attempt owes, as `(the charge it undoes,
    /// the reversal line)` — the two ids [`MealBooking::release_seat`] needs to
    /// append the money back inside the transaction that frees the seat.
    /// `None` when the seat was never billed (a free menu), which owes no line.
    ///
    /// Only the ids and the row are built here: whether the charge exists is a
    /// read, and the caller that writes in one transaction has to make it there
    /// rather than a round trip earlier.
    pub(crate) fn reversal_for(
        booking: &MealBooking,
        recorded_by: &UserId,
    ) -> Option<(MealLedgerId, MealLedger)> {
        let amount = booking.get_price_minor()?;
        let charge = MealLedgerId::for_attempt(
            booking.get_id(),
            booking.get_attempt(),
            MealLedgerKind::Charge,
        );
        let line = MealLedger {
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
        };
        Some((charge, line))
    }

    /// Money in: a payment received, or an opening balance.
    ///
    /// With a `request_key` the line is keyed by it (see
    /// [`MealLedgerId::for_request`]) and the call is **retry-safe**: a client
    /// resending the identical body after a timeout gets back the line the
    /// first attempt wrote, not a second credit — nothing on this API can edit
    /// or delete one, so a doubled credit is corrected only by a compensating
    /// line. Without one the id is a fresh ulid and two identical calls are two
    /// credits, which is what a desk taking the same amount twice really means.
    pub async fn credit(
        student: &UserId,
        amount_minor: LedgerAmount,
        method: Option<LedgerMethod>,
        note: Option<LedgerNote>,
        request_key: Option<&PaymentRequestKey>,
        recorded_by: &UserId,
        db: &Database,
    ) -> Result<MealLedger, AppError> {
        let line = Self::append(
            MealLedger {
                id: match request_key {
                    Some(key) => MealLedgerId::for_request(student, key),
                    None => MealLedgerId::generate(),
                },
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
        .await?;
        // A replay is answered from the stored line — but only if it is the
        // same money. The same key for a different amount is a client bug, and
        // handing back the old line would hide it behind a `201`. Checked on
        // what `append` gave back rather than on a read before it: two retries
        // arriving together both find no row, and only the id decides which
        // one's amount is stored, so a check *before* the write would tell the
        // loser its own amount landed.
        if request_key.is_some() && line.amount_minor != amount_minor {
            return Err(AppError::Conflict(
                "this request_key was already used for a different amount",
            ));
        }
        Ok(line)
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
    ///
    /// **The sum is taken per kind by the database** and only the three totals
    /// come back, so a balance read costs the same whether the statement holds
    /// four lines or forty thousand. It used to decode and fold every line, and
    /// nothing bounds a statement's length but the cycle ceiling
    /// ([`MAX_MEAL_BOOKING_ATTEMPTS`](crate::constant::MAX_MEAL_BOOKING_ATTEMPTS))
    /// added alongside this: every book/cancel
    /// pair appends two permanent rows, and each one was paid for again by
    /// every later `GET /meals/balance/*`.
    ///
    /// **The signs stay here**, applied by the very [`MealLedgerKind::sign`]
    /// the documented formula is spelled in. Summing `IF kind = 'charge' THEN
    /// -amount …` in SQL would have folded the whole balance in one statement
    /// and forked the one rule that decides what money means into a second
    /// language, where nothing would fail the day the two disagreed. Grouping
    /// instead keeps the aggregate ignorant of signs: it counts kinds, and
    /// Rust still says what a kind does. A stored running total was the third
    /// option and is a counter that can drift — a bug class this repo closes,
    /// not one it opens.
    pub async fn balance_of(student: &UserId, db: &Database) -> Result<i64, AppError> {
        let totals: Vec<KindTotal> = db
            .query(format!(
                "SELECT kind, math::sum(amount_minor) AS total \
                 FROM {MEAL_LEDGER_TABLE} WHERE student = $student GROUP BY kind"
            ))
            .bind(("student", student.record()))
            .await?
            .check()?
            .take(0)?;
        Ok(totals
            .iter()
            .fold(0i64, |sum, row| sum + row.kind.sign() * row.total))
    }
}

/// One kind's whole sum, as the `GROUP BY` in [`MealLedger::balance_of`] hands
/// it back — at most three rows, never the lines behind them.
#[derive(Debug, SurrealValue)]
struct KindTotal {
    kind: MealLedgerKind,
    total: i64,
}

#[cfg(test)]
mod tests {
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

    /// The balance aggregate **against a real server**, and `#[ignore]`d for
    /// it: `init_mem`'s embedded engine is not the store this runs on, and an
    /// aggregate is exactly where the two are known to differ — a `count()`
    /// over an indexed field comes back `{count: N}` from the server and a
    /// bare int from memory, and `student` here *is* indexed. This asserts the
    /// `GROUP BY` really decodes into [`KindTotal`] and folds to the
    /// documented figure, per kind, scoped to one student.
    #[tokio::test]
    #[ignore = "needs a real SurrealDB server: podman start hezarfen-surrealdb && cargo test -- --ignored"]
    async fn the_balance_aggregate_decodes_on_a_real_server() {
        let (db, _serialized) = crate::database::init_test_server("meal_balance_sum").await;
        db.query(
            "CREATE meal_ledger:a SET student = user:ali, kind = 'credit', \
                 amount_minor = 10000, recorded_by = user:adm, created_at = 1;
             CREATE meal_ledger:b SET student = user:ali, kind = 'charge', \
                 amount_minor = 4500, recorded_by = user:adm, created_at = 2;
             CREATE meal_ledger:c SET student = user:ali, kind = 'charge', \
                 amount_minor = 1500, recorded_by = user:adm, created_at = 3;
             CREATE meal_ledger:d SET student = user:ali, kind = 'reversal', \
                 amount_minor = 4500, recorded_by = user:adm, created_at = 4;
             CREATE meal_ledger:e SET student = user:veli, kind = 'credit', \
                 amount_minor = 777, recorded_by = user:adm, created_at = 5;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
        let ali = UserId::from_key("ali");
        assert_eq!(
            MealLedger::balance_of(&ali, &db).await.unwrap(),
            10_000 - 4_500 - 1_500 + 4_500
        );
        // Somebody with no lines at all: no groups come back, not an error.
        assert_eq!(
            MealLedger::balance_of(&UserId::from_key("nobody"), &db)
                .await
                .unwrap(),
            0
        );
    }

    #[test]
    fn an_amount_is_positive_and_capped() {
        assert!(LedgerAmount::try_new(0).is_err());
        assert!(LedgerAmount::try_new(-1).is_err());
        assert!(LedgerAmount::try_new(MAX_LEDGER_AMOUNT_MINOR + 1).is_err());
        assert_eq!(LedgerAmount::try_new(1).unwrap().as_minor(), 1);
    }
}
