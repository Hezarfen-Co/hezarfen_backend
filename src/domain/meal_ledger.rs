//! The food program's money: an **append-only** ledger of what a student was
//! charged and what they paid.
//!
//! Nothing here ever `UPDATE`s or `DELETE`s a row, and no such path exists. A
//! ledger line that can be edited or dropped silently rewrites a student's
//! financial history with no trace of the rewrite; a mistake is corrected by
//! appending the opposing line, which leaves both the mistake and the
//! correction visible.
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
//!   and its reversal derive their row id from the seat and the attempt
//!   number, so replaying either writes nothing at all. Money must never
//!   depend on a "has this been billed yet?" scan: two concurrent `POST`s of
//!   one seat can both read "no charge yet" and both append, which is how a
//!   double-click used to bill a seat twice.
//! - **A credit may be keyed too.** `POST /meals/credits` takes an optional
//!   client-chosen `request_key`, which lands in the line's id
//!   ([`MealLedgerId::for_request`]) and makes the call retry-safe by the same
//!   identity rule: a retry after a timeout resolves the line it already wrote.
//!   Without one, a fresh id is minted and a resent request is a second
//!   credit — which nothing here can edit or delete afterwards.
//! - **A no-show still pays.** Meal attendance has zero billing effect —
//!   nothing in this file reads or writes it. Do not add a no-show penalty
//!   here: the seat was reserved and the food was cooked.
//!
//! Money is `i64` minor units (kuruş) end to end. No float, no decimal, ever.
//!
//! The ledger row ids are **derived TEXT keys**, not minted uuids — they are
//! the identity the store's uniqueness check enforces (`id TEXT PRIMARY
//! KEY`): a duplicate insert of the same key is the "already billed" answer.
//! Only the unkeyed-credit fallback mints a fresh (uuid-string) key.
//!
//! The writes and the SUM/balance reads live in [`crate::db::meal_ledger`];
//! the charge/reversal and credit workflows in
//! [`crate::service::meal_ledger`].

use serde::{Deserialize, Serialize};
use sqlx::Type;

use crate::constant::{MAX_LEDGER_AMOUNT_MINOR, MAX_LEDGER_METHOD_LEN, MAX_LEDGER_NOTE_LEN};
// The client-chosen idempotence key both ledgers take — one grammar, one
// validator, one type, rather than a second newtype that could drift from it.
use crate::domain::monotonic_id::next_uuid;
use crate::domain::payment_ledger::PaymentRequestKey;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;
use crate::validate::validate_optional;

/// The ledger line's id — a derived TEXT key (`id TEXT PRIMARY KEY`):
/// `{booking-key}_c{attempt}` / `{booking-key}_r{attempt}` for the booking
/// lines, `{student-key}_k_{request_key}` for a keyed credit, or a fresh
/// uuid-string key for an unkeyed one.
#[derive(Debug, Clone, PartialEq, Eq, Type)]
#[sqlx(transparent)]
pub struct MealLedgerId(String);

impl MealLedgerId {
    /// A fresh key in write order — the process-wide uuid v7 generator's
    /// string form, so the `id` tie-break of the newest-first statement below
    /// still sorts in mint order.
    pub fn generate() -> Self {
        Self(next_uuid().to_string())
    }

    /// The one line a booking attempt may ever write of this `kind` — a
    /// deterministic key, exactly like the booking pair itself. Two racing
    /// `POST`s of one seat derive the *same* key and so cannot become two
    /// charges: idempotence rests on identity, never on a scan that a
    /// concurrent writer can slip past. The `attempt` counter is what keeps a
    /// re-book after a cancel a genuinely fresh charge. Booking keys are
    /// `<menu>_<student>`, so the `c`/`r` marker keeps the two kinds apart.
    pub fn for_attempt(
        booking: &crate::domain::meal_booking::MealBookingId,
        attempt: i64,
        kind: MealLedgerKind,
    ) -> Self {
        let marker = match kind {
            MealLedgerKind::Charge => 'c',
            MealLedgerKind::Reversal => 'r',
            // A credit is money arriving out of the blue, tied to no booking.
            MealLedgerKind::Credit => 'k',
        };
        Self(format!("{}_{}{attempt}", booking.key(), marker))
    }

    /// The one credit a `(student, request_key)` pair may ever have. Same
    /// identity trick as
    /// [`PaymentLedgerId::for_request`](crate::domain::payment_ledger::PaymentLedgerId::for_request),
    /// scoped by the student because a credit points at no line of its own: one
    /// office's "receipt-114" cannot land on another student's account, and a
    /// key replayed for the wrong student cannot resolve to this line at all.
    ///
    /// The grammar parses uniquely because `_` joins the parts: a student key
    /// is a UUID (hex and `-` only) and a `request_key` is
    /// [`crate::validate::validate_request_key`]'s `[A-Za-z0-9-]` — neither can
    /// spell the `_k_` separator. No key can therefore derive an id some other
    /// line owns — a collision would hand money to the wrong row.
    pub fn for_request(student: &UserId, key: &PaymentRequestKey) -> Self {
        Self(format!("{}_k_{}", student.key(), key.as_str()))
    }

    pub fn key(&self) -> &str {
        &self.0
    }
}

/// What a line means. Stored as the bare lowercase TEXT value the `kind`
/// column carries, in lockstep with `MEAL_LEDGER_KINDS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
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
    pub(crate) fn sign(self) -> i64 {
        match self {
            MealLedgerKind::Charge => -1,
            MealLedgerKind::Credit | MealLedgerKind::Reversal => 1,
        }
    }
}

/// One line's amount, always positive — the sign is the [`MealLedgerKind`]'s
/// business. Capped so a slipped keystroke cannot book a fortune.
///
/// `Serialize`/`Deserialize` ride along because the fee plan's installments
/// (a `Json<Vec<Installment>>` column) embed this same money type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[sqlx(transparent)]
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
#[derive(Debug, Clone, PartialEq, Eq, Type)]
#[sqlx(transparent)]
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
#[derive(Debug, Clone, PartialEq, Eq, Type)]
#[sqlx(transparent)]
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

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MealLedger {
    pub(crate) id: MealLedgerId,
    pub(crate) student: UserId,
    pub(crate) kind: MealLedgerKind,
    pub(crate) amount_minor: LedgerAmount,
    /// What caused the line: a charge points at its `meal_booking` row's
    /// `{menu}_{student}` key, a reversal at the `meal_ledger` charge it
    /// undoes. Polymorphic by kind, hence a bare TEXT key — no foreign key.
    pub(crate) source: Option<String>,
    pub(crate) method: Option<LedgerMethod>,
    pub(crate) note: Option<LedgerNote>,
    pub(crate) recorded_by: UserId,
    pub(crate) created_at: Timestamp,
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
        self.source.as_deref()
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

    /// The charge `booking`'s current attempt owes — the line
    /// [`claim_and_place`](crate::db::meal_booking::claim_and_place) appends inside the very transaction that
    /// takes the seat. `None` when the menu was free, which owes no line.
    ///
    /// Only the row is built here; whether it is already there is a read, and
    /// the caller that writes in one transaction has to make it there. Note the
    /// attempt has to be settled *before* the SQL runs, which is why the claim
    /// mints the number itself rather than letting the revival increment it.
    pub(crate) fn charge_for(
        booking: &crate::domain::meal_booking::MealBooking,
        recorded_by: &UserId,
    ) -> Option<MealLedger> {
        let amount = booking.get_price_minor()?;
        Some(MealLedger {
            id: MealLedgerId::for_attempt(
                &booking.id(),
                booking.get_attempt(),
                MealLedgerKind::Charge,
            ),
            student: *booking.get_student(),
            kind: MealLedgerKind::Charge,
            amount_minor: amount,
            source: Some(booking.id().key()),
            method: None,
            note: None,
            recorded_by: *recorded_by,
            created_at: Timestamp::now(),
        })
    }

    /// The refund `booking`'s current attempt owes, as `(the charge it undoes,
    /// the reversal line)` — the two keys
    /// [`release_seat`](crate::db::meal_booking::release_seat) needs to
    /// append the money back inside the transaction that frees the seat.
    /// `None` when the seat was never billed (a free menu), which owes no line.
    ///
    /// Only the keys and the row are built here: whether the charge exists is
    /// a read, and the caller that writes in one transaction has to make it
    /// there rather than a round trip earlier.
    pub(crate) fn reversal_for(
        booking: &crate::domain::meal_booking::MealBooking,
        recorded_by: &UserId,
    ) -> Option<(MealLedgerId, MealLedger)> {
        let amount = booking.get_price_minor()?;
        let charge = MealLedgerId::for_attempt(
            &booking.id(),
            booking.get_attempt(),
            MealLedgerKind::Charge,
        );
        let line = MealLedger {
            id: MealLedgerId::for_attempt(
                &booking.id(),
                booking.get_attempt(),
                MealLedgerKind::Reversal,
            ),
            student: *booking.get_student(),
            kind: MealLedgerKind::Reversal,
            amount_minor: amount,
            source: Some(charge.key().to_string()),
            method: None,
            note: None,
            recorded_by: *recorded_by,
            created_at: Timestamp::now(),
        };
        Some((charge, line))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `kind` column is `TEXT` with a CHECK on these exact words; the
    /// sqlx encoding must never drift from `as_str`, or the `kind = 'charge'`
    /// lookup would silently match nothing — which would double-charge every
    /// re-booking.
    #[test]
    fn sqlx_encodes_the_storage_form() {
        let mut buf = sqlx::postgres::PgArgumentBuffer::default();
        for kind in [
            MealLedgerKind::Charge,
            MealLedgerKind::Credit,
            MealLedgerKind::Reversal,
        ] {
            buf.clear();
            let _ = sqlx::Encode::<sqlx::Postgres>::encode_by_ref(&kind, &mut buf).unwrap();
            assert_eq!(std::str::from_utf8(&buf).unwrap(), kind.as_str());
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

    /// A keyed credit's id is scoped by the student and stable across retries;
    /// an unkeyed line mints a fresh uuid-string key.
    #[test]
    fn credit_keys_derive_deterministically() {
        let student = UserId::from_key("018f1a00-0000-7000-8000-000000000001");
        let key = PaymentRequestKey::try_new("receipt-114").unwrap();
        let first = MealLedgerId::for_request(&student, &key);
        assert_eq!(MealLedgerId::for_request(&student, &key), first);
        assert_eq!(first.key(), format!("{}_k_receipt-114", student.key()));
        // A key replayed for another student cannot resolve to the same line.
        let other = UserId::from_key("018f1a00-0000-7000-8000-000000000002");
        assert_ne!(MealLedgerId::for_request(&other, &key), first);
    }
}
