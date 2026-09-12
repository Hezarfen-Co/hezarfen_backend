//! School fees: an **append-only** ledger of what a student was charged, what
//! they paid against each charge, and what was paid back out.
//!
//! Nothing here ever `UPDATE`s or `DELETE`s a row, and no such path exists —
//! every field is `READONLY` in the schema as well. A ledger line that can be
//! edited or dropped silently rewrites a family's financial history with no
//! trace of the rewrite; a mistake is corrected by appending the opposing line,
//! which leaves both the mistake and the correction visible.
//!
//! **The balance is never stored.** It is always the fold
//!
//! ```text
//! balance = SUM(credit) + SUM(reversal) - SUM(charge) - SUM(refund)
//! ```
//!
//! so a *negative* balance means the family owes the school. Every amount on a
//! row is stored positive; the sign lives in the `kind`, so a line's meaning
//! never depends on how it was read. Money is `i64` minor units (kuruş) end to
//! end. No float, no decimal, ever.
//!
//! The rules the rest of the code depends on:
//!
//! - **Assignment is the charge trigger.** Assigning a
//!   [`FeePlan`](crate::domain::fee_plan::FeePlan) appends
//!   *every* installment as a charge at once, each carrying its own `due_at`.
//!   There is no scheduler and no sweep: "overdue" is a derived reading of an
//!   unpaid line whose `due_at` has passed. The amount is a frozen copy — later
//!   edits to the plan move no existing charge.
//! - **A credit names the charge it pays**, a refund names the credit it
//!   returns. Allocation is recorded, never inferred from a balance.
//! - **A reversal only undoes a negative-fold line** (a charge, or a refund).
//!   A mistaken *credit* is corrected by a refund pointing at it, so that the
//!   money leaving the school is always spelled the same way. A reversal itself
//!   is never reversed (the id would collide with its own target's, and the
//!   kind check refuses it): a charge dropped by mistake is re-raised by
//!   assigning a fresh one-installment plan, which mints a new deterministic
//!   charge id instead of resurrecting the old one.
//! - **Every replayable line is keyed by its cause.** An installment charge's
//!   id is `(plan, student, n)` and a reversal's is `<line>_r`, so replaying
//!   either writes nothing at all, and an assignment that crashed half-way
//!   self-heals when it is repeated. Money must never depend on a "has this
//!   been billed yet?" scan: two concurrent requests can both read "not yet"
//!   and both append.

use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::PAYMENT_LEDGER_TABLE;
use crate::domain::fee_plan_assignment::FeePlanAssignmentId;
// The three value types are the *same* money vocabulary the canteen ledger
// speaks, so they are imported rather than copied: one cap, one trim rule, one
// error message for both ledgers.
pub use crate::domain::meal_ledger::{LedgerAmount, LedgerMethod, LedgerNote};
use crate::domain::monotonic_id::next_ulid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct PaymentLedgerId(RecordId);

impl PaymentLedgerId {
    /// A fresh id in write order — `Ulid::generate()`'s random low bits sort
    /// arbitrarily within one millisecond, which would scramble the `id`
    /// tie-break of the newest-first statement below.
    pub fn generate() -> Self {
        Self(RecordId::new(PAYMENT_LEDGER_TABLE, next_ulid().to_string()))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(PAYMENT_LEDGER_TABLE, key))
    }

    /// The one charge line installment `n` (1-based) of this assignment may
    /// ever have. The assignment key is already `<plan>_<student>`, so this is
    /// `<plan>_<student>_c<n>`: two concurrent assigns derive the *same* ids
    /// and so cannot bill a plan twice. Idempotence rests on identity, never on
    /// a scan a concurrent writer can slip past.
    pub fn for_installment(assignment: &FeePlanAssignmentId, n: usize) -> Self {
        Self(RecordId::new(
            PAYMENT_LEDGER_TABLE,
            format!("{}_c{n}", assignment.key()),
        ))
    }

    /// The one reversal a line may ever have — a retried undo appends nothing
    /// the second time — the id decides that, not a scan.
    pub fn for_reversal(line: &PaymentLedgerId) -> Self {
        Self(RecordId::new(
            PAYMENT_LEDGER_TABLE,
            format!("{}_r", line.key()),
        ))
    }

    /// The one line a `(target, request_key)` pair may ever have: `<target>_k_`
    /// for a payment, `<target>_kr_` for a refund. Scoping the key by the line
    /// it targets is what keeps one office's "receipt-114" from colliding with
    /// another charge's, and the two markers keep the kinds apart the same way
    /// `_c<n>` and `_r` do above.
    ///
    /// **The grammar parses uniquely because `_` joins the parts and cannot
    /// appear inside one.** Every id here is `<ulid>_<ulid>_c<n>` optionally
    /// followed by one `_k_<key>`, `_kr_<key>` or `_r` — plan and student keys
    /// are ULIDs (`[0-9A-Z]` only) and a `request_key` is
    /// [`crate::validate::validate_request_key`]'s `[A-Za-z0-9-]`, so no part
    /// can spell a separator plus a marker. That is not decoration: a key of
    /// `abc_r` on a refund would derive exactly the id that refund's *reversal*
    /// must own, and the loser of that collision would be handed a line of the
    /// wrong kind and the wrong amount, with the real reversal impossible
    /// forever after. The ban on `_` in a key is what makes the collision
    /// unconstructible; `crate::db::payment_ledger::append` re-checks the
    /// kind anyway.
    pub fn for_request(target: &PaymentLedgerId, marker: &str, key: &PaymentRequestKey) -> Self {
        Self(RecordId::new(
            PAYMENT_LEDGER_TABLE,
            format!("{}_{marker}_{}", target.key(), key.as_str()),
        ))
    }

    pub fn record(&self) -> RecordId {
        self.0.clone()
    }

    pub fn key(&self) -> &str {
        key_of(&self.0)
    }
}

/// A client-chosen idempotence key for one payment or refund. Never stored as
/// a column — it lives inside the line's record id, which is what makes a retry
/// derive the row it already wrote instead of a second one.
#[derive(Debug, Clone)]
pub struct PaymentRequestKey(String);

impl PaymentRequestKey {
    pub fn try_new(value: &str) -> Result<Self, ValidationError> {
        crate::validate::validate_request_key(value)?;
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The bare key of a record id — how every id leaves this API.
fn key_of(record: &RecordId) -> &str {
    match &record.key {
        RecordIdKey::String(key) => key,
        _ => "",
    }
}

/// What a line means. `untagged` + `rename_all` store it as the bare lowercase
/// string the `kind` column types as, in lockstep with `PAYMENT_LEDGER_KINDS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, SurrealValue)]
#[surreal(untagged, rename_all = "lowercase")]
pub enum PaymentLedgerKind {
    Charge,
    Credit,
    Reversal,
    Refund,
}

impl PaymentLedgerKind {
    /// The wire/storage form. Must stay in lockstep with `rename_all`.
    pub fn as_str(self) -> &'static str {
        match self {
            PaymentLedgerKind::Charge => "charge",
            PaymentLedgerKind::Credit => "credit",
            PaymentLedgerKind::Reversal => "reversal",
            PaymentLedgerKind::Refund => "refund",
        }
    }

    /// How the line folds into the balance: a charge bills the family and a
    /// refund hands money back, so both take away. This is the *single* place
    /// the sign convention lives.
    pub(crate) fn sign(self) -> i64 {
        match self {
            PaymentLedgerKind::Charge | PaymentLedgerKind::Refund => -1,
            PaymentLedgerKind::Credit | PaymentLedgerKind::Reversal => 1,
        }
    }
}

/// Fields are crate-visible: [`crate::db::payment_ledger`] mints the charge
/// rows, and [`crate::service::payment_ledger`] appends the credits, refunds,
/// and reversals the cap lock authorizes.
#[derive(Debug, Clone, SurrealValue)]
pub struct PaymentLedger {
    pub(crate) id: PaymentLedgerId,
    pub(crate) student: UserId,
    pub(crate) kind: PaymentLedgerKind,
    pub(crate) amount_minor: LedgerAmount,
    /// What caused the line: a charge points at its `fee_plan_assignment`, a
    /// credit at the charge it pays, a refund at the credit it returns, a
    /// reversal at the line it undoes. Untyped, hence a bare `RecordId`.
    pub(crate) source: Option<RecordId>,
    /// When this installment falls due. Charges only — nothing else has one.
    pub(crate) due_at: Option<Timestamp>,
    pub(crate) method: Option<LedgerMethod>,
    pub(crate) note: Option<LedgerNote>,
    pub(crate) recorded_by: UserId,
    pub(crate) created_at: Timestamp,
}

impl PaymentLedger {
    pub fn get_id(&self) -> &PaymentLedgerId {
        &self.id
    }

    pub fn get_student(&self) -> &UserId {
        &self.student
    }

    pub fn get_kind(&self) -> PaymentLedgerKind {
        self.kind
    }

    pub fn get_amount_minor(&self) -> LedgerAmount {
        self.amount_minor
    }

    /// The cause's bare key; which table it lives in follows from the kind.
    pub fn get_source_key(&self) -> Option<&str> {
        self.source.as_ref().map(key_of)
    }

    pub fn get_due_at(&self) -> Option<Timestamp> {
        self.due_at
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

    /// The one fold, over whatever already carries a kind and an amount: the
    /// four grouped totals
    /// [`crate::db::payment_ledger::balance_of`] hands back, or the raw lines
    /// a caller has in hand. A
    /// document that reports both a per-charge rollup and a balance must fold
    /// both from one read — two reads straddle a payment landing between them,
    /// and the halves then disagree about the same money in the same response.
    pub fn fold_balance(lines: impl IntoIterator<Item = (PaymentLedgerKind, i64)>) -> i64 {
        lines
            .into_iter()
            .fold(0i64, |sum, (kind, amount)| sum + kind.sign() * amount)
    }

    /// This line's contribution to that fold, as [`PaymentLedger::fold_balance`]
    /// takes it.
    pub fn folded(&self) -> (PaymentLedgerKind, i64) {
        (self.kind, self.amount_minor.as_minor())
    }
}

#[cfg(test)]
mod tests {
    use surrealdb::types::Value;

    use super::*;
    use crate::domain::fee_plan::FeePlanId;
    use crate::domain::user::UserId;

    /// The `kind` column is `TYPE string`: an object-wrapped enum would be
    /// rejected on write, and a `kind = 'charge'` lookup would silently match
    /// nothing.
    #[test]
    fn kind_stores_as_a_bare_string() {
        for kind in [
            PaymentLedgerKind::Charge,
            PaymentLedgerKind::Credit,
            PaymentLedgerKind::Reversal,
            PaymentLedgerKind::Refund,
        ] {
            let value = kind.into_value();
            assert_eq!(value, Value::String(kind.as_str().to_string()));
            assert_eq!(PaymentLedgerKind::from_value(value).unwrap(), kind);
        }
    }

    /// The one fold: a charge and a refund subtract, a credit and a reversal
    /// add. A charge raised in error and reversed nets to exactly zero, and a
    /// payment handed back leaves the family owing again.
    #[test]
    fn the_balance_fold_is_credit_plus_reversal_minus_charge_and_refund() {
        assert_eq!(PaymentLedgerKind::Charge.sign(), -1);
        assert_eq!(PaymentLedgerKind::Refund.sign(), -1);
        assert_eq!(PaymentLedgerKind::Credit.sign(), 1);
        assert_eq!(PaymentLedgerKind::Reversal.sign(), 1);

        let fold = |lines: &[(PaymentLedgerKind, i64)]| -> i64 {
            lines
                .iter()
                .map(|(kind, amount)| kind.sign() * amount)
                .sum()
        };
        // Billed 10 000, paid 4 000: still 6 000 owed.
        assert_eq!(
            fold(&[
                (PaymentLedgerKind::Charge, 10_000),
                (PaymentLedgerKind::Credit, 4_000),
            ]),
            -6_000
        );
        // That payment handed back puts the whole charge back on the family.
        assert_eq!(
            fold(&[
                (PaymentLedgerKind::Charge, 10_000),
                (PaymentLedgerKind::Credit, 4_000),
                (PaymentLedgerKind::Refund, 4_000),
            ]),
            -10_000
        );
        // A charge raised in error and reversed leaves nothing behind.
        assert_eq!(
            fold(&[
                (PaymentLedgerKind::Charge, 10_000),
                (PaymentLedgerKind::Reversal, 10_000),
            ]),
            0
        );
    }

    /// Idempotence rests on identity: the same (plan, student, installment)
    /// must always derive the same charge id, and a reversal must be the one
    /// line its target can ever have.
    #[test]
    fn a_charge_id_is_the_plan_the_student_and_the_installment() {
        let assignment = FeePlanAssignmentId::composite(
            &FeePlanId::from_key("plan1"),
            &UserId::from_key("stu1"),
        );
        let first = PaymentLedgerId::for_installment(&assignment, 1);
        assert_eq!(first.key(), "plan1_stu1_c1");
        assert_eq!(
            PaymentLedgerId::for_installment(&assignment, 1),
            first,
            "a replayed assign must derive the same id, or it bills twice"
        );
        assert_ne!(PaymentLedgerId::for_installment(&assignment, 2), first);
        assert_eq!(
            PaymentLedgerId::for_reversal(&first).key(),
            "plan1_stu1_c1_r"
        );
    }

    /// A `request_key` is only an idempotence key if it derives the same id
    /// every time, and only *safe* if it is scoped by the line it targets and
    /// tells a payment from a refund.
    #[test]
    fn a_request_key_is_scoped_by_its_target_and_its_kind() {
        let charge = PaymentLedgerId::from_key("plan1_stu1_c1");
        let other = PaymentLedgerId::from_key("plan1_stu1_c2");
        let key = PaymentRequestKey::try_new("receipt-114").unwrap();
        let credit = PaymentLedgerId::for_request(&charge, "k", &key);

        assert_eq!(credit.key(), "plan1_stu1_c1_k_receipt-114");
        assert_eq!(
            PaymentLedgerId::for_request(&charge, "k", &key),
            credit,
            "a retry must derive the same id, or it pays twice"
        );
        assert_ne!(PaymentLedgerId::for_request(&other, "k", &key), credit);
        assert_ne!(PaymentLedgerId::for_request(&charge, "kr", &key), credit);
        assert_eq!(
            PaymentLedgerId::for_request(&credit, "kr", &key).key(),
            "plan1_stu1_c1_k_receipt-114_kr_receipt-114"
        );
    }
}
