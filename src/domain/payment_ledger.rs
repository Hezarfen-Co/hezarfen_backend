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
//! - **Assignment is the charge trigger.** Assigning a [`FeePlan`] appends
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
//! - **A payment or a refund is replayable when the client names it.** An
//!   optional `request_key` keys the line `<target>_k_<key>` (or `_kr_` for a
//!   refund), so a retry after a timeout derives the row the first attempt
//!   wrote — same identity rule, no scan. The key is scoped by the line it
//!   targets, so it never has to be globally unique. Without a key the id is a
//!   fresh ulid and two identical calls are two payments, which is what a desk
//!   taking the same amount twice really means. The **replay read happens
//!   before the cap check** and inside [`PAYMENT_LOCK`]: the first attempt's
//!   line is already inside what the cap folds, so checking the cap first would
//!   refuse the very payment that landed. A key replayed with a *different*
//!   amount or against a *different* line is a `409`, never the stored line —
//!   returning it would hide a client bug behind a `201`.
//!
//! **The over-payment cap is the lock's, not the database's.** [`PaymentLedger::credit`]
//! and [`PaymentLedger::refund`] refuse to take more than the line they target
//! is worth, by folding that line's whole subtree under [`PAYMENT_LOCK`]. The
//! fold, rather than a sum of the direct children, because money handed back
//! frees the room it took: a charge paid in full and then refunded is owed
//! again, and must be payable again. Every write that *moves* that arithmetic
//! takes the same lock — [`PaymentLedger::reversal`] included, since a reversal
//! landing between a payment's fold and its append would admit the payment
//! against room it no longer has.
//!
//! The lock is what makes that check mean anything, and it is load-bearing:
//! the fold is a count-then-write across rows, and SurrealDB does not
//! conflict-check a cross-record read against a concurrent insert (the
//! write-skew this repo has hit before), so a fold left unserialized would let
//! two payments both see room and both take it. Running as one process buys
//! nothing on its own — two request tasks interleave across the fold's `await`
//! exactly as two machines would. The cap therefore holds only for as long as
//! *every* write that moves this arithmetic is taken under [`PAYMENT_LOCK`];
//! a future append that skips it re-opens the hole silently, which is why the
//! rule is stated here rather than left to be noticed. If an over-payment ever
//! does land it is not a crisis — an over-paid charge is plainly visible in the
//! statement and undone by appending a refund, and both entries are true
//! records of money that really arrived. A CAS counter row was rejected —
//! refunding would have to decrement it, which is a stored derived balance by
//! another name. What the fold costs is also what bounds it: at most
//! [`MAX_LEDGER_APPLIED_LINES`] lines may be applied to any one line, since
//! every one of them is another query taken with the lock held.
//!
//! The `request_key` mismatch `409` rides on the same lock: it is a
//! read-then-compare, and it is read *inside* the lock, so two first-time posts
//! of one key with different amounts cannot both walk past it. One line, one
//! amount, no double charge, and the "you reused a key" diagnostic is exact.

use surrealdb::types::{AlreadyExistsError, RecordId, RecordIdKey, SurrealValue};
use tokio::sync::Mutex;

use crate::constant::{CAS_UPDATE_RETRIES, MAX_LEDGER_APPLIED_LINES, PAYMENT_LEDGER_TABLE};
use crate::database::{Database, lost_the_race};
use crate::domain::fee_plan::Installment;
use crate::domain::fee_plan_assignment::{FeePlanAssignment, FeePlanAssignmentId};
// The three value types are the *same* money vocabulary the canteen ledger
// speaks, so they are imported rather than copied: one cap, one trim rule, one
// error message for both ledgers.
pub use crate::domain::meal_ledger::{LedgerAmount, LedgerMethod, LedgerNote};
use crate::domain::monotonic_id::next_ulid;
use crate::domain::page::PagedList;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// Serializes the over-payment check, the append it authorizes, and every
/// other append that changes the arithmetic that check does — nothing else.
/// It is the whole guarantee behind that cap, not a convenience: the fold it
/// protects is a cross-record read the database will not conflict-check
/// against a concurrent insert. Held across no other lock, and no other lock is
/// taken while it is held.
pub(crate) static PAYMENT_LOCK: Mutex<()> = Mutex::const_new(());

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct PaymentLedgerId(RecordId);

impl PaymentLedgerId {
    /// A fresh id in write order — `Ulid::new()`'s random low bits sort
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
    /// unconstructible; [`PaymentLedger::append`] re-checks the kind anyway.
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
    fn sign(self) -> i64 {
        match self {
            PaymentLedgerKind::Charge | PaymentLedgerKind::Refund => -1,
            PaymentLedgerKind::Credit | PaymentLedgerKind::Reversal => 1,
        }
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct PaymentLedger {
    id: PaymentLedgerId,
    student: UserId,
    kind: PaymentLedgerKind,
    amount_minor: LedgerAmount,
    /// What caused the line: a charge points at its `fee_plan_assignment`, a
    /// credit at the charge it pays, a refund at the credit it returns, a
    /// reversal at the line it undoes. Untyped, hence a bare `RecordId`.
    source: Option<RecordId>,
    /// When this installment falls due. Charges only — nothing else has one.
    due_at: Option<Timestamp>,
    method: Option<LedgerMethod>,
    note: Option<LedgerNote>,
    recorded_by: UserId,
    created_at: Timestamp,
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

    /// The only writer: one `CREATE`, no update path anywhere in this module.
    ///
    /// A line whose id already exists is left exactly as it is — the row wins,
    /// the write is dropped. That is what makes a deterministic id (see
    /// [`PaymentLedgerId::for_installment`]) an idempotence key: replaying the
    /// same append is a no-op, never a second line and never an edit of the
    /// first. The point read is on the id itself, so unlike a scan it cannot
    /// miss a row a concurrent writer just made — but it is only a fast path.
    /// The guarantee is `CREATE`'s own: on an existing id it *errors* and
    /// leaves the row untouched, so the writer that lost the race reads back
    /// the winner's line instead of failing the request with a 500.
    ///
    /// A *write conflict* is the same race decided one layer down — two appends
    /// of one id arriving together are no longer serialized by a process-wide
    /// lock, so the store aborts one as retryable instead of answering it
    /// "already exists". Both are read back the same way, and a conflict that
    /// turns out to have written nothing is simply tried again; no path here
    /// can write a second line, since the id is the key.
    ///
    /// Both read-backs check the stored line is the *kind* that was being
    /// appended. Handing a caller someone else's row is only safe while every
    /// id shape is unambiguous (see [`PaymentLedgerId::for_request`]); if a
    /// future marker or a widened key charset ever let two shapes meet, this is
    /// the check that turns silently-wrong money into a 500 instead. It should
    /// be unreachable, and it is cheap enough to keep it that way.
    async fn append(row: PaymentLedger, db: &Database) -> Result<PaymentLedger, AppError> {
        let same_kind = |existing: PaymentLedger| {
            if existing.kind == row.kind {
                Ok(existing)
            } else {
                Err(AppError::Internal(
                    "a ledger id resolved to a line of another kind".into(),
                ))
            }
        };
        if let Some(existing) = Self::read(&row.id, db).await? {
            return same_kind(existing);
        }
        let id = row.id.clone();
        for _ in 0..CAS_UPDATE_RETRIES {
            match db.create(id.record()).content(row.clone()).await {
                Ok(Some(created)) => return Ok(created),
                Ok(None) => break,
                Err(e) if is_duplicate_record(&e) || lost_the_race(&e) => {
                    if let Some(existing) = Self::read(&id, db).await? {
                        return same_kind(existing);
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(AppError::Internal("failed to write the ledger line".into()))
    }

    pub async fn read(
        id: &PaymentLedgerId,
        db: &Database,
    ) -> Result<Option<PaymentLedger>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// Bill installment `n` (1-based) of `assignment`, at the amount and due
    /// date the plan carried when it was assigned — the charge is a frozen
    /// copy, so editing the plan afterwards moves no existing line.
    ///
    /// Replaying it is free: the id is `(plan, student, n)`, so a duplicate
    /// assign writes nothing and an assignment whose charges landed only in
    /// part self-heals on the next assign of the same pair.
    pub async fn charge_for_installment(
        assignment: &FeePlanAssignment,
        n: usize,
        installment: &Installment,
        recorded_by: &UserId,
        db: &Database,
    ) -> Result<PaymentLedger, AppError> {
        Self::append(
            PaymentLedger {
                id: PaymentLedgerId::for_installment(assignment.get_id(), n),
                student: assignment.get_student().clone(),
                kind: PaymentLedgerKind::Charge,
                amount_minor: installment.get_amount_minor(),
                source: Some(assignment.get_id().record()),
                due_at: Some(installment.get_due_at()),
                method: None,
                note: None,
                recorded_by: recorded_by.clone(),
                created_at: Timestamp::now(),
            },
            db,
        )
        .await
    }

    /// Money in, against one named `charge`. Partial payments are the norm, so
    /// a charge may collect several credits; together they may not exceed it
    /// (advisory — see the module doc's accepted race). A *reversed* charge
    /// takes no payment either, and is refused saying so rather than claiming
    /// it was paid.
    ///
    /// With a `request_key` the line is keyed by it (see
    /// [`PaymentLedgerId::for_request`]) and the call is retry-safe; without
    /// one the id is a fresh ulid and two identical calls are two payments,
    /// which is what a cash desk taking the same amount twice really means.
    pub async fn credit(
        charge: &PaymentLedger,
        amount_minor: LedgerAmount,
        method: Option<LedgerMethod>,
        note: Option<LedgerNote>,
        request_key: Option<&PaymentRequestKey>,
        recorded_by: &UserId,
        db: &Database,
    ) -> Result<PaymentLedger, AppError> {
        Self::against(
            charge,
            PaymentLedgerKind::Charge,
            PaymentLedgerKind::Credit,
            "a payment must be recorded against a charge",
            "the charge is already paid in full",
            // A charge *is* reversible, and its reversal fills the same room a
            // payment would, so the fold refuses the two cases identically.
            Some("the charge was reversed, so it is no longer owed"),
            amount_minor,
            method,
            note,
            request_key.map(|key| PaymentLedgerId::for_request(charge.get_id(), "k", key)),
            recorded_by,
            db,
        )
        .await
    }

    /// Money back out, against one named `credit` — how an over-payment or a
    /// payment recorded in error is returned. Partials allowed, and no more
    /// than the credit was worth (advisory, same race). `request_key` makes it
    /// retry-safe exactly as it does for [`PaymentLedger::credit`].
    pub async fn refund(
        credit: &PaymentLedger,
        amount_minor: LedgerAmount,
        method: Option<LedgerMethod>,
        note: Option<LedgerNote>,
        request_key: Option<&PaymentRequestKey>,
        recorded_by: &UserId,
        db: &Database,
    ) -> Result<PaymentLedger, AppError> {
        Self::against(
            credit,
            PaymentLedgerKind::Credit,
            PaymentLedgerKind::Refund,
            "a refund must be recorded against a payment",
            "the payment is already refunded in full",
            // A credit is never reversed ([`PaymentLedger::reversal`] refuses
            // it), so a full fold here can only mean the refunds, and there is
            // no second reason to tell apart — nor a read to spend looking.
            None,
            amount_minor,
            method,
            note,
            request_key.map(|key| PaymentLedgerId::for_request(credit.get_id(), "kr", key)),
            recorded_by,
            db,
        )
        .await
    }

    /// The shared body of `credit` and `refund`: check the target is the kind
    /// this line may point at, then append under [`PAYMENT_LOCK`] while the
    /// target's existing children still sum below its amount.
    ///
    /// The replay read comes **before** the cap, and inside the lock: on a
    /// retry the first attempt's line is already part of the subtree the cap
    /// folds, so consulting the cap first would answer a payment that landed
    /// with "already paid in full" — refusing precisely the request that
    /// succeeded. Reading the id under the lock also keeps two simultaneous
    /// retries from both walking into the cap check.
    #[allow(clippy::too_many_arguments)]
    async fn against(
        target: &PaymentLedger,
        expected: PaymentLedgerKind,
        kind: PaymentLedgerKind,
        wrong_target: &'static str,
        over: &'static str,
        reversed: Option<&'static str>,
        amount_minor: LedgerAmount,
        method: Option<LedgerMethod>,
        note: Option<LedgerNote>,
        keyed: Option<PaymentLedgerId>,
        recorded_by: &UserId,
        db: &Database,
    ) -> Result<PaymentLedger, AppError> {
        if target.kind != expected {
            return Err(ValidationError::Invalid {
                field: "source",
                reason: wrong_target,
            }
            .into());
        }
        let _guard = PAYMENT_LOCK.lock().await;
        if let Some(id) = &keyed
            && let Some(existing) = Self::read(id, db).await?
        {
            // A replay is answered from the stored line — but only if it is the
            // same money. The same key for a different amount or a different
            // target is a client bug, and handing back the old line would hide
            // it behind a `201`.
            if existing.amount_minor.as_minor() != amount_minor.as_minor()
                || existing.get_source_key() != Some(target.id.key())
            {
                return Err(AppError::Conflict(
                    "this request_key was already used for a different amount or target",
                ));
            }
            return Ok(existing);
        }
        let taken = Self::applied_to(target, db).await?;
        if taken.saturating_add(amount_minor.as_minor()) > target.amount_minor.as_minor() {
            // The fold cannot say *why* the room is gone: a reversal is a child
            // of the line it undoes and folds in at `+amount` exactly as a
            // payment does, so a reversed charge reads as full to the kuruş.
            // The stored answer is the same either way — nothing may be
            // recorded against it — but "already paid in full" told a bursar
            // money had arrived when none ever did. The reversal's id is
            // derived from its target's, so telling the two apart is one read,
            // taken only on the refusal path.
            if let Some(reversed) = reversed
                && Self::read(&PaymentLedgerId::for_reversal(&target.id), db)
                    .await?
                    .is_some()
            {
                return Err(AppError::Conflict(reversed));
            }
            return Err(AppError::Conflict(over));
        }
        Self::append(
            PaymentLedger {
                id: keyed.unwrap_or_else(PaymentLedgerId::generate),
                student: target.student.clone(),
                kind,
                amount_minor,
                source: Some(target.id.record()),
                due_at: None,
                method,
                note,
                recorded_by: recorded_by.clone(),
                created_at: Timestamp::now(),
            },
            db,
        )
        .await
    }

    /// How much of `target` is already taken up — the balance fold restricted
    /// to everything that points at it, however deep, and read the way `target`
    /// is measured.
    ///
    /// The whole subtree, not just the direct children, because money given
    /// back frees the room it took: a charge paid in full and then *refunded*
    /// is owed again, so it must be payable again (the fold nets to zero). The
    /// same walk answers both sides, because the signs already say what each
    /// line does — a credit adds under a charge, its refund takes that back,
    /// and a reversal of that refund puts it back once more. `-target.sign()`
    /// is what flips the reading for a credit, whose room is measured in the
    /// refunds against it.
    ///
    /// **The subtree is bounded, and that is what makes the walk affordable.**
    /// One query per line, all of them while [`PAYMENT_LOCK`] is held, so an
    /// unbounded subtree stalls every other payment in the school: a charge
    /// settled 2 000 kuruş at a time would make the next payment issue 2 001
    /// sequential queries under the lock. Past
    /// [`MAX_LEDGER_APPLIED_LINES`] the walk stops where it is and the write is
    /// refused — the count is what is refused, never the money already
    /// recorded, so a line that is *already* over the ceiling (written before
    /// it existed) still reads, still refunds through its own children, and is
    /// still reversible. Only a fresh line applied to *it* is turned away.
    // ponytail: one query per line, ceiling MAX_LEDGER_APPLIED_LINES; fold the
    // walk into one recursive statement if that ceiling ever has to rise.
    async fn applied_to(target: &PaymentLedger, db: &Database) -> Result<i64, AppError> {
        let mut total = 0i64;
        let mut seen = 0usize;
        // The graph is a DAG by construction — a line can only point at one
        // that already existed — so the walk terminates.
        let mut pending = vec![target.id.clone()];
        while let Some(id) = pending.pop() {
            for child in Self::list_for_source(&id, db).await? {
                seen += 1;
                if seen >= MAX_LEDGER_APPLIED_LINES {
                    return Err(AppError::Conflict(
                        "this line already carries the most lines that may be applied to it; \
                         record the rest against another",
                    ));
                }
                total = total.saturating_add(child.kind.sign() * child.amount_minor.as_minor());
                pending.push(child.id);
            }
        }
        Ok(total * -target.kind.sign())
    }

    /// Undo a line entered by mistake, for its exact amount, with `source`
    /// pointing at it. The line itself is never touched.
    ///
    /// Only a *negative-fold* line may be reversed: a charge that should never
    /// have been raised, or a refund that should never have been paid out. A
    /// mistaken credit is corrected by a [`PaymentLedger::refund`] against it,
    /// which is the same money leaving the school and is spelled that one way.
    /// Keyed `<line>_r`, so a retried undo reverses once.
    ///
    /// Appends under [`PAYMENT_LOCK`] — not for its own sake (the id makes it
    /// idempotent by itself) but for the cap's: a reversal *changes* the
    /// subtree [`PaymentLedger::applied_to`] folds, so one landing between a
    /// concurrent payment's fold and its append would let that payment be
    /// admitted against room it no longer has. The lock is what makes the cap
    /// exact, which is the promise this module's doc makes.
    pub async fn reversal(
        line: &PaymentLedger,
        note: Option<LedgerNote>,
        recorded_by: &UserId,
        db: &Database,
    ) -> Result<PaymentLedger, AppError> {
        if !matches!(
            line.kind,
            PaymentLedgerKind::Charge | PaymentLedgerKind::Refund
        ) {
            return Err(ValidationError::Invalid {
                field: "source",
                reason: "only a charge or a refund can be reversed; refund a payment instead",
            }
            .into());
        }
        let _guard = PAYMENT_LOCK.lock().await;
        Self::append(
            PaymentLedger {
                id: PaymentLedgerId::for_reversal(line.get_id()),
                student: line.student.clone(),
                kind: PaymentLedgerKind::Reversal,
                amount_minor: line.amount_minor,
                source: Some(line.id.record()),
                due_at: None,
                method: None,
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
    ) -> Result<(Vec<PaymentLedger>, i64), AppError> {
        PagedList::new(
            "payment_ledger WHERE student = $student",
            "ORDER BY created_at DESC, id DESC",
        )
        .bind("student", student.record())
        .run(limit, offset, db)
        .await
    }

    /// Every line pointing at `source`: a charge's payments (and its reversal),
    /// a credit's refunds. Feeds the over-payment cap and, later, the statement
    /// rollup. Oldest first — this is a history, not a page.
    pub async fn list_for_source(
        source: &PaymentLedgerId,
        db: &Database,
    ) -> Result<Vec<PaymentLedger>, AppError> {
        let mut result = db
            .query("SELECT * FROM payment_ledger WHERE source = $source ORDER BY created_at, id")
            .bind(("source", source.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<PaymentLedger>>(0)?)
    }

    /// The derived balance: `credits + reversals - charges - refunds`, in minor
    /// units. Negative means the family owes the school. Never stored anywhere.
    ///
    /// **The sum is taken per kind by the database** and only the four totals
    /// come back, so a balance read costs the same whether the ledger holds
    /// four lines or forty thousand. It used to decode and fold every row, and
    /// a fee ledger grows by design rather than only under abuse: one
    /// assignment appends a charge *per installment* per student (up to
    /// [`MAX_FEE_PLAN_ASSIGN_WRITES`](crate::constant::MAX_FEE_PLAN_ASSIGN_WRITES)
    /// in a single request), and every one of those rows was decoded again by
    /// every later `GET /payments/balance/*`.
    ///
    /// **The signs stay here**, applied by the same [`PaymentLedgerKind::sign`]
    /// the documented formula is spelled in — [`PaymentLedger::fold_balance`]
    /// folds the grouped totals and the raw lines alike. Summing
    /// `IF kind = 'charge' THEN -amount …` in SQL would have folded the whole
    /// balance in one statement and forked the one rule that decides what money
    /// means into a second language, where nothing fails the day the two
    /// disagree. Grouping keeps the aggregate ignorant of signs. A stored
    /// running total was the third option and is a counter that can drift — a
    /// bug class this repo closes, not one it opens.
    pub async fn balance_of(student: &UserId, db: &Database) -> Result<i64, AppError> {
        let totals: Vec<KindTotal> = db
            .query(format!(
                "SELECT kind, math::sum(amount_minor) AS total \
                 FROM {PAYMENT_LEDGER_TABLE} WHERE student = $student GROUP BY kind"
            ))
            .bind(("student", student.record()))
            .await?
            .check()?
            .take(0)?;
        Ok(Self::fold_balance(
            totals.into_iter().map(|row| (row.kind, row.total)),
        ))
    }

    /// The one fold, over whatever already carries a kind and an amount: the
    /// four grouped totals above, or the raw lines a caller has in hand. A
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

/// One kind's whole sum, as the `GROUP BY` in [`PaymentLedger::balance_of`]
/// hands it back — at most four rows, never the lines behind them.
#[derive(Debug, SurrealValue)]
struct KindTotal {
    kind: PaymentLedgerKind,
    total: i64,
}

#[cfg(test)]
mod tests {
    use surrealdb::types::SurrealValue as _;
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

    /// The mem engine is not the store this runs against, and an aggregate is
    /// exactly where the two have diverged before: `count()` over an *indexed*
    /// field compared to a plan-time value comes back `{count: N}` rather than
    /// a plain int, and `student` is indexed on this table
    /// (`payment_ledger_student`). So the `GROUP BY` is proved on a real
    /// server: that it decodes into [`KindTotal`], that it folds to the
    /// documented figure per kind, that it is scoped to one student, and that
    /// someone with no lines at all comes back `0` rather than an error.
    #[tokio::test]
    #[ignore = "needs a real SurrealDB server: podman start hezarfen-surrealdb && cargo test -- --ignored"]
    async fn the_balance_aggregate_decodes_on_a_real_server() {
        let (db, _serialized) = crate::database::init_test_server("payment_balance_sum").await;
        db.query(
            "CREATE payment_ledger:a SET student = user:ali, kind = 'charge', \
                 amount_minor = 10000, recorded_by = user:adm, created_at = 1;
             CREATE payment_ledger:b SET student = user:ali, kind = 'credit', \
                 amount_minor = 6000, recorded_by = user:adm, created_at = 2;
             CREATE payment_ledger:c SET student = user:ali, kind = 'refund', \
                 amount_minor = 1000, recorded_by = user:adm, created_at = 3;
             CREATE payment_ledger:d SET student = user:ali, kind = 'reversal', \
                 amount_minor = 4500, recorded_by = user:adm, created_at = 4;
             CREATE payment_ledger:e SET student = user:veli, kind = 'credit', \
                 amount_minor = 777, recorded_by = user:adm, created_at = 5;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
        let ali = UserId::from_key("ali");
        assert_eq!(
            PaymentLedger::balance_of(&ali, &db).await.unwrap(),
            6_000 + 4_500 - 10_000 - 1_000,
            "credits + reversals - charges - refunds, and veli's line is not ali's"
        );
        // Somebody with no lines at all: no groups come back, not an error.
        assert_eq!(
            PaymentLedger::balance_of(&UserId::from_key("nobody"), &db)
                .await
                .unwrap(),
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

    /// Raise one charge of `amount` on a fresh student, and hand it back.
    #[cfg(test)]
    async fn one_charge(amount: i64, db: &Database) -> (PaymentLedger, UserId, UserId) {
        use crate::domain::fee_plan::{FeePlan, FeePlanName, Installment};
        use crate::domain::fee_plan_assignment::FeePlanAssignment;

        let manager = UserId::from_key("mgr1");
        let student = UserId::from_key("stu1");
        let plan = FeePlan::create(
            FeePlanName::try_new("Yearly").unwrap(),
            vec![Installment::new(
                LedgerAmount::try_new(amount).unwrap(),
                Timestamp::from_millis(1_000),
            )],
            &manager,
            db,
        )
        .await
        .unwrap();
        FeePlanAssignment::assign(&plan, &student, &manager, db)
            .await
            .unwrap();
        let (lines, _) = PaymentLedger::list_for_student(&student, None, 0, db)
            .await
            .unwrap();
        (lines.into_iter().next().unwrap(), student, manager)
    }

    /// The collision the id grammar must not admit: with `_` legal in a key, a
    /// refund keyed `abc_r` derived exactly `<refund>_r` — the id that refund's
    /// own reversal must own — and whichever came second was handed the other's
    /// line, of the wrong kind and the wrong amount, with the real reversal
    /// impossible ever after. `_` is now illegal in a key, so the collision
    /// cannot be constructed; the nearest legal key still works, and the
    /// reversal still gets its own line.
    #[tokio::test]
    async fn a_keyed_refund_cannot_take_the_id_of_its_own_reversal() {
        assert!(
            PaymentRequestKey::try_new("abc_r").is_err(),
            "the key that spelled a reversal's id must not parse"
        );
        let db = crate::database::init_mem().await.unwrap();
        let (charge, _, manager) = one_charge(100, &db).await;
        let key = PaymentRequestKey::try_new("abc-r").unwrap();
        let credit = PaymentLedger::credit(
            &charge,
            LedgerAmount::try_new(100).unwrap(),
            None,
            None,
            None,
            &manager,
            &db,
        )
        .await
        .unwrap();
        let refund = PaymentLedger::refund(
            &credit,
            LedgerAmount::try_new(100).unwrap(),
            None,
            None,
            Some(&key),
            &manager,
            &db,
        )
        .await
        .unwrap();
        let reversal = PaymentLedger::reversal(&refund, None, &manager, &db)
            .await
            .unwrap();
        assert_ne!(reversal.get_id(), refund.get_id());
        assert_eq!(reversal.get_kind(), PaymentLedgerKind::Reversal);
        assert_eq!(reversal.get_amount_minor().as_minor(), 100);

        // And the last-ditch guard bites: an id that resolves to another kind
        // is a 500, never that other line handed back as this one.
        let intruder = PaymentLedger {
            id: charge.get_id().clone(),
            student: charge.student.clone(),
            kind: PaymentLedgerKind::Credit,
            amount_minor: LedgerAmount::try_new(1).unwrap(),
            source: Some(charge.id.record()),
            due_at: None,
            method: None,
            note: None,
            recorded_by: manager.clone(),
            created_at: Timestamp::now(),
        };
        assert!(
            matches!(
                PaymentLedger::append(intruder, &db).await,
                Err(AppError::Internal(_))
            ),
            "a read-back of another kind must not pass for the appended line"
        );
    }

    /// The defect this fold replaced a plain sum for: a payment that was handed
    /// back frees the room it took, so the charge it paid can be paid again. It
    /// is the same charge, still owed — refusing the second payment would leave
    /// a family unable to settle a bill the school itself refunded.
    #[tokio::test]
    async fn a_refund_frees_the_room_it_took_under_the_charge() {
        let db = crate::database::init_mem().await.unwrap();
        let (charge, student, manager) = one_charge(100, &db).await;
        let pay = |amount| {
            PaymentLedger::credit(
                &charge,
                LedgerAmount::try_new(amount).unwrap(),
                None,
                None,
                None,
                &manager,
                &db,
            )
        };

        let credit = pay(100).await.unwrap();
        // Still fully paid: a second payment has no room.
        assert!(
            matches!(pay(100).await, Err(AppError::Conflict(_))),
            "a charge paid in full may not be paid twice"
        );

        PaymentLedger::refund(
            &credit,
            LedgerAmount::try_new(100).unwrap(),
            None,
            None,
            None,
            &manager,
            &db,
        )
        .await
        .unwrap();
        assert_eq!(
            PaymentLedger::balance_of(&student, &db).await.unwrap(),
            -100,
            "the refund puts the charge back on the family"
        );
        let repaid = pay(100)
            .await
            .expect("the refunded charge is payable again");
        assert_eq!(repaid.get_amount_minor().as_minor(), 100);
        assert_eq!(PaymentLedger::balance_of(&student, &db).await.unwrap(), 0);

        // And the room is gone again, so the cap still bites after all that.
        assert!(matches!(pay(1).await, Err(AppError::Conflict(_))));
    }

    /// The other half of the fold: reversing a refund un-does the money going
    /// out, so the room that refund freed is taken back.
    #[tokio::test]
    async fn a_reversed_refund_takes_its_room_back() {
        let db = crate::database::init_mem().await.unwrap();
        let (charge, _, manager) = one_charge(100, &db).await;
        let credit = PaymentLedger::credit(
            &charge,
            LedgerAmount::try_new(100).unwrap(),
            None,
            None,
            None,
            &manager,
            &db,
        )
        .await
        .unwrap();
        let refund = PaymentLedger::refund(
            &credit,
            LedgerAmount::try_new(100).unwrap(),
            None,
            None,
            None,
            &manager,
            &db,
        )
        .await
        .unwrap();
        PaymentLedger::reversal(&refund, None, &manager, &db)
            .await
            .unwrap();

        assert!(
            matches!(
                PaymentLedger::credit(
                    &charge,
                    LedgerAmount::try_new(100).unwrap(),
                    None,
                    None,
                    None,
                    &manager,
                    &db
                )
                .await,
                Err(AppError::Conflict(_))
            ),
            "the refund was undone, so the charge is paid in full again"
        );
    }

    /// The money branches, end to end against the engine: partial payments
    /// accumulate, the advisory cap refuses the one that would overshoot the
    /// charge, a refund is capped by *its* credit, and neither a credit nor a
    /// reversal may point at a line of the wrong kind.
    #[tokio::test]
    async fn payments_are_capped_by_the_line_they_target() {
        use crate::domain::fee_plan::{FeePlan, FeePlanName, Installment};
        use crate::domain::fee_plan_assignment::FeePlanAssignment;

        let db = crate::database::init_mem().await.unwrap();
        let manager = UserId::from_key("mgr1");
        let student = UserId::from_key("stu1");
        let plan = FeePlan::create(
            FeePlanName::try_new("Yearly").unwrap(),
            vec![Installment::new(
                LedgerAmount::try_new(10_000).unwrap(),
                Timestamp::from_millis(1_000),
            )],
            &manager,
            &db,
        )
        .await
        .unwrap();
        FeePlanAssignment::assign(&plan, &student, &manager, &db)
            .await
            .unwrap();
        let (charge, _) = PaymentLedger::list_for_student(&student, None, 0, &db)
            .await
            .unwrap();
        let charge = charge.into_iter().next().unwrap();
        assert_eq!(charge.get_due_at(), Some(Timestamp::from_millis(1_000)));

        let paid = |amount| {
            PaymentLedger::credit(
                &charge,
                LedgerAmount::try_new(amount).unwrap(),
                None,
                None,
                None,
                &manager,
                &db,
            )
        };
        let first = paid(6_000).await.unwrap();
        assert!(
            matches!(paid(5_000).await, Err(AppError::Conflict(_))),
            "6 000 + 5 000 overshoots a 10 000 charge"
        );
        paid(4_000).await.expect("the exact remainder is allowed");
        assert_eq!(PaymentLedger::balance_of(&student, &db).await.unwrap(), 0);

        // A refund is bounded by the credit it returns, not by the charge.
        let refund = |amount| {
            PaymentLedger::refund(
                &first,
                LedgerAmount::try_new(amount).unwrap(),
                None,
                None,
                None,
                &manager,
                &db,
            )
        };
        assert!(matches!(refund(6_001).await, Err(AppError::Conflict(_))));
        refund(6_000).await.unwrap();
        assert_eq!(
            PaymentLedger::balance_of(&student, &db).await.unwrap(),
            -6_000,
            "money handed back puts the debt back on the family"
        );

        // Wrong-kind targets are refused, and a credit is never reversed.
        assert!(
            PaymentLedger::credit(
                &first,
                LedgerAmount::try_new(1).unwrap(),
                None,
                None,
                None,
                &manager,
                &db
            )
            .await
            .is_err()
        );
        assert!(
            PaymentLedger::reversal(&first, None, &manager, &db)
                .await
                .is_err()
        );
        // A charge, though, reverses — once, however often it is retried.
        let reversed = PaymentLedger::reversal(&charge, None, &manager, &db)
            .await
            .unwrap();
        let again = PaymentLedger::reversal(&charge, None, &manager, &db)
            .await
            .unwrap();
        assert_eq!(reversed.get_id(), again.get_id());
        let (lines, _) = PaymentLedger::list_for_student(&student, None, 0, &db)
            .await
            .unwrap();
        assert_eq!(lines.len(), 5, "charge, two credits, refund, one reversal");
    }
}
