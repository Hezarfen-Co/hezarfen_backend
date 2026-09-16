//! Count caps the database enforces, not the process.
//!
//! Every "at most N children per parent" rule used to be a count-then-write
//! under a process-wide `Mutex`, because the old store did not conflict-check
//! a cross-record `count()` against a concurrent insert (write-skew): two
//! racing writers both saw a free slot. A mutex serialized the writers, but
//! only around whatever it wrapped — every one of those caps held a lock
//! across a database round trip, and the count it was protecting was already
//! a guess by the time the insert landed.
//!
//! The guard that survives is a single-record conditional write: an
//! `UPDATE … SET n = n + 1 WHERE n < cap` on the *parent* row is atomic, so
//! of N concurrent claimers exactly `cap` get a non-empty result and the rest
//! are refused, with no lock and no window. The counter is therefore the
//! authority on how many slots are taken, and the child rows follow it: the
//! claim and the child's insert land in one statement (the CTE recipes
//! below), and a decrement rides the *same transaction* as every delete of a
//! child row.
//!
//! Postgres takes over the rest of what the old builders hand-rolled:
//!
//! * **Existence proof** — a real `FOREIGN KEY` refuses a child whose parent
//!   is gone (SQLSTATE 23503), so the old bump-and-restore "touch" trick is
//!   deleted. A gated variant ("parent must still be open") is
//!   `INSERT … SELECT … WHERE EXISTS (SELECT 1 FROM parent WHERE id = $n AND <gate>)`.
//! * **Duplicate detection** — the child's primary key answers "another
//!   writer placed this very row first" as SQLSTATE 23505
//!   (`crate::database::unique_violation`).
//! * **Retry** — `crate::database::tx_with_retry` owns the retry loop; a
//!   refusal is an `Err` from the closure and is never retried, because a
//!   refusal is a decision.
//!
//! ## The per-call-site recipes
//!
//! Table, column and cap names are crate constants, so every call site writes
//! its own static, macro-checkable statement (`sqlx::query!`). This module
//! keeps only the result types the call sites answer with, plus the one
//! genuinely shared mapping ([`full_kind`]). The shapes:
//!
//! ### Claim a seat and create the child (`claim_and_create` and its variants)
//!
//! ```sql
//! WITH seat AS (
//!     UPDATE <parent>
//!        SET <cnt> = <cnt> + <bump>            -- bump 0 for the uncounted create
//!      WHERE id = $1
//!        AND <cnt> < $2                        -- or: < (SELECT <cap_expr> …)  (live cap)
//!        -- AND (<guard>)                      -- the guarded variant: one predicate decides cap and precondition
//!      RETURNING 1)
//! INSERT INTO <child> (id, <cols>)
//! SELECT $3, $4, … WHERE EXISTS (SELECT 1 FROM seat)
//! RETURNING <cols>;
//! ```
//!
//! One statement is atomic: a refused insert takes its own seat bump back.
//! The verdict maps: one row → [`Claimed::Made`]; 23505 on the child key →
//! [`Claimed::Duplicate`] (read before the row count); zero rows →
//! [`Claimed::Full`] — deliberately still one marker for "full, guard failed,
//! parent gone", as before: the caller re-reads the parent to pick the
//! message, and only on that path. A live cap reads its capacity as a
//! subselect inside the same `WHERE`, so the write that lowers the cap and
//! the write that reads it still contend on one record; a lowered cap refuses
//! the very next claimer instead of admitting everyone whose snapshot predates
//! the edit. The uncounted create (`bump = 0`) still writes the seat
//! statement, so a missing parent row is refused as [`Claimed::Full`] on both
//! paths rather than silently writing a child of nobody.
//!
//! ### Two counters, one verdict (`claim_two_when_and_create`)
//!
//! ```sql
//! WITH bump AS (
//!     UPDATE board
//!        SET total_stroke_count = total_stroke_count + 1,   -- hard (lifetime) cap first
//!            epoch_stroke_count  = epoch_stroke_count + 1   -- soft (resettable) counter with it
//!      WHERE id = $1
//!        AND total_stroke_count < $2                         -- the hard cap carries the guard
//!        AND epoch = $3
//!        AND epoch_stroke_count < $4
//!      RETURNING 1)
//! INSERT INTO board_stroke (id, <cols>)
//! SELECT $5, … WHERE EXISTS (SELECT 1 FROM bump)
//! RETURNING <cols>;
//! ```
//!
//! One `UPDATE` moves both counters, so the row is written once and the
//! two-statement interleave the old process lock existed for cannot happen.
//! Zero rows means "hard full, guard failed, board gone, or soft full": the
//! caller re-reads both counters and maps them with [`full_kind`] — the
//! terminal cap outranks the recoverable one, as [`ClaimedTwo`] documents.
//! The child id is freshly minted, so no rival can aim at it and no 23505
//! verdict exists on this path.
//!
//! ### Claim a reference on a name (`claim_ref_and_create`)
//!
//! In the caller's transaction, the duplicate gate stays *ahead* of the
//! retired check, so "you already have it" still outranks "the name is
//! retired":
//!
//! ```sql
//! SELECT 1 FROM <child> WHERE <its primary key>;   -- found → ClaimedRef::Duplicate, stop
//! WITH ref AS (
//!     INSERT INTO <ref_table> (name, count)
//!     VALUES ($1, $2)
//!     ON CONFLICT (name) DO UPDATE
//!        SET count = <ref_table>.count + EXCLUDED.count
//!      WHERE <ref_table>.retired = false           -- zero rows → ClaimedRef::Retired
//!     RETURNING 1)
//! INSERT INTO <child> (…)
//! SELECT … WHERE EXISTS (SELECT 1 FROM ref)
//! RETURNING …;                                     -- 23505 on the child key → ClaimedRef::Duplicate
//! ```
//!
//! The 23505 backstop covers the rival that passed the same gate a moment
//! ago (referential integrity keeps "row exists ∧ name retired" unreachable:
//! retiring requires count = 0, and a live row holds a count). The counted
//! row and the count still commit together or not at all — a name counted for
//! a row that does not exist is a name nobody can ever retire.
//!
//! ### Retire / un-retire a name (produces a [`Switched`])
//!
//! Two statements in the caller's transaction; the row lock makes the read
//! and the write one switch, which is why the old process lock is gone:
//!
//! ```sql
//! SELECT retired FROM <ref_table> WHERE name = $1 FOR UPDATE;   -- absent → treated as false
//! INSERT INTO <ref_table> (name, count, retired)
//!     VALUES ($1, 0, $2)
//!     ON CONFLICT (name) DO UPDATE
//!        SET retired = $2
//!      WHERE <ref_table>.count = 0                               -- retire only
//!        AND <ref_table>.retired IS DISTINCT FROM $2             -- idempotence
//!     RETURNING 1;                        -- empty result → the write did not land
//! ```
//!
//! `(landed, was.unwrap_or(false))` maps exactly as [`Switched`] documents.
//! Retiring a never-claimed name still creates its ref row retired (the
//! `INSERT` arm), as before.
//!
//! ### Role-gated grants (the old `role_claim` handshake)
//!
//! The grant-survives-demotion handshake is now two statements in the
//! caller's transaction — the old counter bump-and-restore dance is dead:
//!
//! ```sql
//! SELECT role FROM app_user WHERE id = $1 FOR NO KEY UPDATE;
//! -- Rust checks the role it found; unfit → the call-site refusal (no retry).
//! INSERT INTO <grant> …;
//! ```
//!
//! The demotion cascade's transaction `UPDATE`s that same user row before its
//! sweeps, so grant and demotion serialize on the row lock in both
//! directions: either the sweep sees the grant row, or the grant saw the new
//! role.

#![allow(dead_code)] // the types below are the wave-2 lanes' verdict vocabulary

/// "No cap" as a number, so the guard stays one comparison instead of a
/// nullable branch: `NULL` bind ambiguity is a worse trade than a sentinel no
/// roster will ever reach.
pub(crate) const UNLIMITED: i64 = i64::MAX;

/// What the claim-and-create recipe (module docs) settled.
pub(crate) enum Claimed<T> {
    /// The seat and the row committed together.
    Made(T),
    /// The cap is full or the parent row is gone — nothing was written.
    Full,
    /// Another writer placed this very row first, so this caller never owed a
    /// seat: nothing was written, and the winner's row is the answer.
    Duplicate,
}

/// What the two-counter recipe (module docs) settled.
pub(crate) enum ClaimedTwo<T> {
    /// Both seats and the row committed together.
    Made(T),
    /// The *resettable* counter (the soft, epoch-scoped one) is full —
    /// nothing was written, and whatever empties it lets the caller through
    /// again.
    FullSoft,
    /// The *lifetime* counter is full, the guard failed, or the parent row is
    /// gone — nothing was written. The caller re-reads the parent to tell
    /// those apart, and only to pick the message: both are refusals the
    /// caller cannot retry into a success.
    FullHard,
}

/// What the reference-counter recipe (module docs) settled.
pub(crate) enum ClaimedRef<T> {
    /// The reference and the row committed together.
    Made(T),
    /// The name is retired — nothing was written, and no retry helps until
    /// the name is put back in service.
    Retired,
    /// Another writer placed this very row first, so this caller never owed a
    /// reference: nothing was written, and the winner's row is the answer.
    Duplicate,
}

/// What a retire / un-retire write did to the name's bit.
///
/// The distinction a rollback lives on. Both writes are idempotent, so "it
/// ended up retired" says nothing about *who* retired it: a caller that undoes
/// a no-op undoes whoever really did flip the bit — two settings PATCHes each
/// dropping the same exam kind, one of them losing the row's compare-and-set,
/// left the kind gone from the list with its counter reading "in service",
/// which is a kind that grades again and can then never be removed.
pub(crate) enum Switched {
    /// This call flipped the bit; a rollback owes the opposite write.
    Flipped,
    /// The bit already stood that way — this call wrote nothing, so there is
    /// nothing to take back, and taking it back would undo somebody else.
    Unchanged,
    /// Retirement only: something still references the name and the caller
    /// must refuse. Nothing was written.
    InUse,
}

/// Which of [`ClaimedTwo`]'s caps refused a zero-row dual claim.
pub(crate) enum FullKind {
    /// The resettable counter is full — recoverable by whatever empties it.
    Soft,
    /// The lifetime counter is full, the guard failed, or the parent row is
    /// gone — terminal.
    Hard,
}

/// Map a zero-row dual-counter claim, re-read off the parent row, onto
/// [`ClaimedTwo`]'s two full states.
///
/// The recipe bumps both counters in one `UPDATE` with the hard cap's
/// conditions first, so zero rows means "hard full, guard failed, parent
/// gone, or soft full". The terminal cap outranks the recoverable one
/// everywhere: [`FullKind::Soft`] only when the soft counter is full *while*
/// the hard one still has room — every other answer, including the ambiguous
/// guard/gone case, folds into [`FullKind::Hard`], whose contract already
/// covers it. This is the old claim order (hard claim, guard attached, before
/// the soft one) expressed as a re-read.
pub(crate) fn full_kind(
    soft_count: i64,
    soft_cap: i64,
    hard_count: i64,
    hard_cap: i64,
) -> FullKind {
    if soft_count >= soft_cap && hard_count < hard_cap {
        FullKind::Soft
    } else {
        FullKind::Hard
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The terminal cap outranks the recoverable one: only a soft-full-with-
    /// hard-room claim is recoverable, everything else answers Hard — the old
    /// claim order (hard first, guard attached), preserved as a re-read.
    #[test]
    fn the_terminal_cap_outranks_the_recoverable_one() {
        // Soft full, hard with room: the one recoverable answer.
        assert!(matches!(full_kind(5, 5, 9, 10), FullKind::Soft));
        // Both full: terminal.
        assert!(matches!(full_kind(5, 5, 9, 9), FullKind::Hard));
        // Hard full alone.
        assert!(matches!(full_kind(4, 5, 9, 9), FullKind::Hard));
        // Neither full — the guard failed or the parent is gone, and both fold
        // into the terminal answer, whose contract already covers them.
        assert!(matches!(full_kind(4, 5, 8, 9), FullKind::Hard));
        // Boundaries: exactly-at-cap is full.
        assert!(matches!(full_kind(5, 5, 10, 10), FullKind::Hard));
    }
}
