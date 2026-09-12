//! School fees: an **append-only** ledger of what a student was charged, what
//! they paid against each charge, and what was paid back out.
//!
//! Nothing here ever `UPDATE`s or `DELETE`s a row, and no such path exists. A
//! ledger line that can be edited or dropped silently rewrites a family's
//! financial history with no trace of the rewrite; a mistake is corrected by
//! appending the opposing line, which leaves both the mistake and the
//! correction visible.
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
//!   is never reversed (the key would collide with its own target's, and the
//!   kind check refuses it): a charge dropped by mistake is re-raised by
//!   assigning a fresh one-installment plan, which mints a new deterministic
//!   charge key instead of resurrecting the old one.
//! - **Every replayable line is keyed by its cause.** An installment charge's
//!   key is `{assignment-key}_c{n}` and a reversal's is `{line-key}_r`, so
//!   replaying either writes nothing at all, and an assignment that crashed
//!   half-way self-heals when it is repeated. Money must never depend on a
//!   "has this been billed yet?" scan: two concurrent requests can both read
//!   "not yet" and both append.
//!
//! The ledger row ids are **derived TEXT keys**, not minted uuids — they are
//! the identity the store's uniqueness check enforces (`id TEXT PRIMARY
//! KEY`): a duplicate insert of the same key is the "already billed" answer.
//!
//! The writes and the balance reads live in [`crate::db::payment_ledger`];
//! the credit/refund/reversal workflows in
//! [`crate::service::payment_ledger`].

use sqlx::Type;

use crate::domain::fee_plan_assignment::FeePlanAssignmentId;
// The three value types are the *same* money vocabulary the canteen ledger
// speaks, so they are imported rather than copied: one cap, one trim rule, one
// error message for both ledgers.
pub use crate::domain::meal_ledger::{LedgerAmount, LedgerMethod, LedgerNote};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::ValidationError;

/// The ledger line's id — a derived TEXT key (`id TEXT PRIMARY KEY`):
/// `{assignment-key}_c{n}` for an installment charge, `{line-key}_r` for a
/// reversal, `{target-key}_{marker}_{request_key}` for a keyed payment or
/// refund, or a fresh uuid-string key otherwise.
#[derive(Debug, Clone, PartialEq, Eq, Type)]
#[sqlx(transparent)]
pub struct PaymentLedgerId(String);

impl PaymentLedgerId {
    /// A fresh key in write order — the process-wide uuid v7 generator's
    /// string form, so the `id` tie-break of the newest-first statement below
    /// still sorts in mint order.
    pub fn generate() -> Self {
        Self(crate::domain::monotonic_id::next_uuid().to_string())
    }

    /// Parse a stored key back into an id. Ledger keys are TEXT, so any
    /// string round-trips.
    pub fn from_key(key: &str) -> Self {
        Self(key.to_string())
    }

    /// The one charge line installment `n` (1-based) of this assignment may
    /// ever have. The assignment key is already `{plan}_{student}`, so this is
    /// `{plan}_{student}_c{n}`: two concurrent assigns derive the *same* keys
    /// and so cannot bill a plan twice. Idempotence rests on identity, never
    /// on a scan a concurrent writer can slip past.
    pub fn for_installment(assignment: &FeePlanAssignmentId, n: usize) -> Self {
        Self(format!("{}_c{n}", assignment.key()))
    }

    /// The one reversal a line may ever have — a retried undo appends nothing
    /// the second time — the key decides that, not a scan.
    pub fn for_reversal(line: &PaymentLedgerId) -> Self {
        Self(format!("{}_r", line.key()))
    }

    /// The one line a `(target, request_key)` pair may ever have:
    /// `{target}_k_{key}` for a payment, `{target}_kr_{key}` for a refund.
    /// Scoping the key by the line it targets is what keeps one office's
    /// "receipt-114" from colliding with another charge's, and the two markers
    /// keep the kinds apart the same way `_c{n}` and `_r` do above.
    ///
    /// **The grammar parses uniquely because `_` joins the parts and cannot
    /// appear inside one.** Every key here is `<uuid>_<uuid>_c<n>` optionally
    /// followed by one `_k_<key>`, `_kr_<key>` or `_r` — uuid halves carry
    /// only hex digits and `-`, and a `request_key` is
    /// [`crate::validate::validate_request_key`]'s `[A-Za-z0-9-]`, so no part
    /// can spell a separator plus a marker. That is not decoration: a key of
    /// `abc_r` on a refund would derive exactly the key that refund's
    /// *reversal* must own, and the loser of that collision would be handed a
    /// line of the wrong kind and the wrong amount, with the real reversal
    /// impossible forever after. The ban on `_` in a key is what makes the
    /// collision unconstructible; `crate::db::payment_ledger::append` re-checks
    /// the kind anyway.
    pub fn for_request(target: &PaymentLedgerId, marker: &str, key: &PaymentRequestKey) -> Self {
        Self(format!("{}_{marker}_{}", target.key(), key.as_str()))
    }

    pub fn key(&self) -> &str {
        &self.0
    }
}

/// A client-chosen idempotence key for one payment or refund. Never stored as
/// a column — it lives inside the line's derived key, which is what makes a
/// retry derive the row it already wrote instead of a second one.
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

/// What a line means. Stored as the bare lowercase TEXT value the `kind`
/// column carries, in lockstep with `PAYMENT_LEDGER_KINDS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
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
/// and reversals the guard authorizes.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PaymentLedger {
    pub(crate) id: PaymentLedgerId,
    pub(crate) student: UserId,
    pub(crate) kind: PaymentLedgerKind,
    pub(crate) amount_minor: LedgerAmount,
    /// What caused the line: a charge points at its `fee_plan_assignment`
    /// row's `{plan}_{student}` key, a credit at the charge it pays, a refund
    /// at the credit it returns, a reversal at the line it undoes.
    /// Polymorphic by kind, hence a bare TEXT key — no foreign key.
    pub(crate) source: Option<String>,
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
        self.source.as_deref()
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
    use super::*;
    use crate::domain::fee_plan::FeePlanId;

    /// The `kind` column is `TEXT` with a CHECK on these exact words; the
    /// sqlx encoding must never drift from `as_str`, or a `kind = 'charge'`
    /// lookup would silently match nothing.
    #[test]
    fn sqlx_encodes_the_storage_form() {
        let mut buf = sqlx::postgres::PgArgumentBuffer::default();
        for kind in [
            PaymentLedgerKind::Charge,
            PaymentLedgerKind::Credit,
            PaymentLedgerKind::Reversal,
            PaymentLedgerKind::Refund,
        ] {
            buf.clear();
            sqlx::Encode::<sqlx::Postgres>::encode_by_ref(&kind, &mut buf);
            assert_eq!(std::str::from_utf8(&buf).unwrap(), kind.as_str());
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
    /// must always derive the same charge key, and a reversal must be the one
    /// line its target can ever have.
    #[test]
    fn a_charge_key_is_the_plan_the_student_and_the_installment() {
        const PLAN: &str = "018f1a00-0000-7000-8000-000000000001";
        const STU: &str = "018f1a00-0000-7000-8000-000000000002";
        let assignment = FeePlanAssignmentId::composite(&FeePlanId::from_key(PLAN), &UserId::from_key(STU));
        let first = PaymentLedgerId::for_installment(&assignment, 1);
        assert_eq!(first.key(), format!("{PLAN}_{STU}_c1"));
        assert_eq!(
            PaymentLedgerId::for_installment(&assignment, 1),
            first,
            "a replayed assign must derive the same key, or it bills twice"
        );
        assert_ne!(PaymentLedgerId::for_installment(&assignment, 2), first);
        assert_eq!(
            PaymentLedgerId::for_reversal(&first).key(),
            format!("{PLAN}_{STU}_c1_r")
        );
    }

    /// A `request_key` is only an idempotence key if it derives the same key
    /// every time, and only *safe* if it is scoped by the line it targets and
    /// tells a payment from a refund.
    #[test]
    fn a_request_key_is_scoped_by_its_target_and_its_kind() {
        const PLAN: &str = "018f1a00-0000-7000-8000-000000000001";
        const STU: &str = "018f1a00-0000-7000-8000-000000000002";
        let charge = PaymentLedgerId::from_key(format!("{PLAN}_{STU}_c1"));
        let other = PaymentLedgerId::from_key(format!("{PLAN}_{STU}_c2"));
        let key = PaymentRequestKey::try_new("receipt-114").unwrap();
        let credit = PaymentLedgerId::for_request(&charge, "k", &key);

        assert_eq!(credit.key(), format!("{PLAN}_{STU}_c1_k_receipt-114"));
        assert_eq!(
            PaymentLedgerId::for_request(&charge, "k", &key),
            credit,
            "a retry must derive the same key, or it pays twice"
        );
        assert_ne!(PaymentLedgerId::for_request(&other, "k", &key), credit);
        assert_ne!(PaymentLedgerId::for_request(&charge, "kr", &key), credit);
        assert_eq!(
            PaymentLedgerId::for_request(&credit, "kr", &key).key(),
            format!("{PLAN}_{STU}_c1_k_receipt-114_kr_receipt-114")
        );
    }
}
