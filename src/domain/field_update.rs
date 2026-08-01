//! One `UPDATE $id SET ... RETURN AFTER` built from *only* the fields a PATCH
//! actually carried.
//!
//! WHY: a handler reads the row, then writes. Filling a field the request
//! omitted from that snapshot re-sends a value the client never gave, so a
//! concurrent PATCH of the *other* field that landed in between is silently
//! reverted — the same lost update a whole-row `.content(self)` save causes,
//! just spelled field by field. Scoping the `SET` is not enough; the *values* must come
//! from the request, so an absent field is never written at all.
//!
//! Callers therefore take `Option<T>` per field (`None` = absent, keep) and
//! hand them straight here:
//!
//! ```ignore
//! FieldUpdate::new(self.id.record())
//!     .set("title", title)
//!     .set("content", content)
//!     .run::<Note>(db)
//!     .await
//! ```
//!
//! Clearing a nullable column is `Some(None)` on a `Option<Option<T>>` field
//! — pass it as `Some(value_or_none)` so `NONE` is written explicitly.

use surrealdb::types::{RecordId, SurrealValue, Value};

use crate::database::{Database, transaction_with_retry, write_with_retry};
use crate::domain::cap;
use crate::error::AppError;

/// The `THROW` markers [`FieldUpdate::refcount`]'s transaction aborts with: the
/// row the reference was to be claimed on is gone, the row being patched is
/// gone (or its guard bit), and the link this move started from is no longer
/// the one the handler read.
const CLAIM_MARK: &str = "ref_claim_gone";
const ROW_MARK: &str = "ref_row_gone";
const STALE_MARK: &str = "ref_stale_move";

/// `(counter field, link column, expected link, claim, release, refusal)`.
type Refcount = (
    &'static str,
    &'static str,
    Option<RecordId>,
    Option<RecordId>,
    Option<RecordId>,
    AppError,
);

pub struct FieldUpdate {
    id: RecordId,
    /// `field = $field` fragments, in the order they were set.
    assignments: Vec<String>,
    bindings: Vec<(String, Value)>,
    /// `(low, high, refusal)` for [`FieldUpdate::ordered`].
    ordered: Option<(&'static str, &'static str, AppError)>,
    /// `(condition, refusal)` for [`FieldUpdate::guard`].
    guard: Option<(&'static str, AppError)>,
    /// `(counter field, link column, expected link, claim, release, refusal)`
    /// for [`FieldUpdate::refcount`].
    refcount: Option<Refcount>,
}

impl FieldUpdate {
    pub fn new(id: RecordId) -> Self {
        Self {
            id,
            assignments: Vec::new(),
            bindings: Vec::new(),
            ordered: None,
            guard: None,
            refcount: None,
        }
    }

    /// Write `field` only when the request carried it. The bind variable is
    /// named after the field, so `id` — and `ref_claim`/`ref_release`/
    /// `ref_expected`, which [`FieldUpdate::refcount`] binds — are the names a
    /// caller must not use.
    #[must_use]
    pub fn set<T: SurrealValue>(mut self, field: &'static str, value: Option<T>) -> Self {
        if let Some(value) = value {
            self.assignments.push(format!("{field} = ${field}"));
            self.bindings.push((field.to_string(), value.into_value()));
        }
        self
    }

    /// Refuse the write unless `low <= high` still holds on the *merged* row —
    /// the new value for an end this PATCH carries, the stored column for one it
    /// omits. Emitted as a `WHERE` on the same `UPDATE`, so the comparison the
    /// handler made against its snapshot is re-made by the database at write
    /// time, against the row as it is *then*: two PATCHes each moving one end,
    /// each fine on its own, cannot commit an inverted range between them. That
    /// is what an in-process `Mutex` used to buy, without a window between the
    /// snapshot and the write for a concurrent request to slip into.
    ///
    /// Call it after the `.set()`s of both fields. A PATCH that moves neither
    /// end emits no guard at all (nothing to race), matching the lock window it
    /// replaces — so a pre-existing inverted row stays editable field by field.
    /// `NONE` on either side passes, exactly like
    /// [`crate::web::check_time_range`] skipping an absent end.
    #[must_use]
    pub fn ordered(mut self, low: &'static str, high: &'static str, refused: AppError) -> Self {
        self.ordered = Some((low, high, refused));
        self
    }

    /// Refuse the write unless `condition` — a predicate on this same row —
    /// still holds at write time. The [`FieldUpdate::ordered`] guard for a
    /// precondition that is not about a range: a handler that read "nothing
    /// references this yet" re-asks the database at the instant it writes, so a
    /// reference landing in between refuses the edit instead of being edited
    /// out from under. `condition` is always an in-crate SQL literal, never
    /// user input.
    ///
    /// A request that carries no field at all emits no `UPDATE`, so it is not
    /// refused: it writes nothing, and reading the row back is a truthful
    /// answer to a PATCH that asked for no change.
    #[must_use]
    pub fn guard(mut self, condition: &'static str, refused: AppError) -> Self {
        self.guard = Some((condition, refused));
        self
    }

    /// Move a reference counter *with* this write: `claim` takes one on the row
    /// the link moves to, `release` gives one back on the row it moves off, and
    /// both ride the same `BEGIN…COMMIT` as the `UPDATE` that moves the link.
    ///
    /// That is the whole point. A claim sent as its own query leaves a window
    /// where a crash strands a count on a row nothing links any more — and a
    /// count is what a delete guard reads, so the stranded one makes its parent
    /// undeletable forever. `refused` is the answer when the claimed row is gone
    /// (the conditional write doubles as the existence check, as in
    /// [`crate::domain::cap`]); the transaction then aborts, so the release and
    /// the row write never happened either.
    ///
    /// Only ever called alongside a `.set()` of the very column that carries the
    /// link, so the empty-request short-circuit below cannot swallow a move.
    ///
    /// `link` is that column (never `field`: the counter lives on the *other*
    /// row, `course_count` on the term against a `term` link) and `expected` is
    /// what the handler's snapshot said it held — `None` for a link that was
    /// absent. The pair becomes a CAS on the row write, because claim and
    /// release are computed from that snapshot: two PATCHes moving the same link
    /// A→B both read A, and both would claim B, leaving B counted twice for one
    /// link — a count with no link is exactly what makes a term undeletable
    /// forever. The loser's row write matches nothing and the whole transaction
    /// aborts, so it claims nothing and answers 409.
    ///
    /// **Carrying the link and shifting a counter are separate questions**, and
    /// the CAS keys off the first one — `run` arms it for any request that
    /// `.set()` this column, `claim`/`release` both `None` included. A PATCH
    /// *re-stating* the link its snapshot showed shifts no counter, but it is
    /// the same stale read: run it against a row someone else has since moved
    /// and the unguarded write silently drags the link back, stranding the
    /// winner's claim on a term nothing points at (undeletable forever) and
    /// leaving the reverted-to term linked at zero (deletable while linked).
    /// Gating the CAS on the counters is what let that through.
    #[must_use]
    pub fn refcount(
        mut self,
        field: &'static str,
        link: &'static str,
        expected: Option<RecordId>,
        claim: Option<RecordId>,
        release: Option<RecordId>,
        refused: AppError,
    ) -> Self {
        self.refcount = Some((field, link, expected, claim, release, refused));
        self
    }

    /// Run the update and return the stored row. An empty request writes
    /// nothing at all and reads the row back unchanged.
    pub async fn run<T: SurrealValue>(mut self, db: &Database) -> Result<T, AppError> {
        // Armed by the *request*, not by the counters: a PATCH that carries the
        // link column gets the CAS even when it re-states the value it read and
        // so shifts nothing. Decided here rather than in `refcount` so the
        // builder's call order cannot silently disarm it — `run` is always last.
        let moved = self.refcount.take();
        let moved = moved.filter(|(_, link, ..)| self.is_set(link));
        if self.assignments.is_empty() {
            let row: Option<T> = db.select(self.id).await?;
            return row.ok_or(AppError::NotFound);
        }
        let (ordered, mut refused) = match self.ordered.take() {
            // Both ends stored: name the bound variable for an end this request
            // set, the column for one it left alone.
            Some((low, high, err)) if self.is_set(low) || self.is_set(high) => {
                let side = |field: &str, set| {
                    if set {
                        format!("${field}")
                    } else {
                        field.into()
                    }
                };
                let low = side(low, self.is_set(low));
                let high = side(high, self.is_set(high));
                (
                    Some(format!(
                        "({low} = NONE OR {high} = NONE OR {low} <= {high})"
                    )),
                    Some(err),
                )
            }
            _ => (None, None),
        };
        // Both guards land on the one `UPDATE`, ANDed. No caller sets both, so
        // the refusal is whichever one is there.
        let (extra, extra_refused) = match self.guard.take() {
            Some((condition, err)) => (Some(condition.to_string()), Some(err)),
            None => (None, None),
        };
        refused = refused.or(extra_refused);
        // A PATCH that does not carry the link at all writes the same unguarded
        // `UPDATE` it always did — it re-states nothing and races nobody. Probed
        // on the mem engine:
        // a bound `None` comes through as `NONE` and `link = NONE` matches a row
        // whose option column was never set, while failing one that holds a
        // record, so the absent-link shape needs no `??`.
        let cas = moved
            .as_ref()
            .map(|(_, link, ..)| format!("{link} = $ref_expected"));
        let conditions: Vec<String> = ordered.into_iter().chain(extra).chain(cas).collect();
        let guard = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };
        let sets = self.assignments.join(", ");
        let sql = format!("UPDATE $id SET {sets}{guard} RETURN AFTER");
        self.bindings.push(("id".into(), self.id.into_value()));
        // Retried on a write conflict: a guarded PATCH contends on the very row
        // its guard reads, and the loser wrote nothing, so re-sending it is the
        // recovery — see [`write_with_retry`].
        let rows: Vec<T> = match moved {
            Some(moved) => run_with_refcount(db, &sql, self.bindings, moved).await?,
            None => write_with_retry(db, &sql, &self.bindings).await?,
        };
        // No row back means the guard bit (or, in the window after the handler's
        // read, the row was deleted — the guard cannot tell the two apart, and
        // reports the refusal it was given).
        rows.into_iter()
            .next()
            .ok_or_else(|| refused.take().unwrap_or(AppError::NotFound))
    }

    fn is_set(&self, field: &str) -> bool {
        self.bindings.iter().any(|(name, _)| name == field)
    }
}

/// Send `update` with its counter move, as one transaction. An empty `Vec` back
/// means the row write matched nothing, exactly as [`write_with_retry`] would
/// have reported it, so the caller's refusal is unchanged.
///
/// A re-statement shifts no counter, so `claim` and `release` are both `None`
/// and the transaction is the CAS'd row write and its split alone — still a
/// transaction, because the split's probe must see the same snapshot the write
/// was refused against.
///
/// Admissible for [`transaction_with_retry`]: every statement is an `UPDATE`,
/// an `IF`/`THROW` or a `RETURN`, and none of those can answer "already
/// exists" — a lost round writes nothing and re-sending it is the recovery.
async fn run_with_refcount<T: SurrealValue>(
    db: &Database,
    update: &str,
    mut bindings: Vec<(String, Value)>,
    (field, link, expected, claim, release, refused): Refcount,
) -> Result<Vec<T>, AppError> {
    // Parenthesized `??` throughout: `n ?? 0 + 1` parses as `n ?? (0 + 1)`.
    // Both counter writes commit with the row write or neither does, so their
    // order is free — the release goes first because that is what makes the
    // rollback observable: a move to a term that is gone decrements before the
    // claim throws, and the old count still being there proves the abort undid
    // it (`a_term_move_to_a_dead_term_leaves_everything_untouched`).
    let mut statements: Vec<String> = Vec::new();
    if let Some(release) = release {
        statements.push(format!(
            "UPDATE $ref_release SET {field} = math::max([({field} ?? 0) - 1, 0])"
        ));
        bindings.push(("ref_release".into(), release.into_value()));
    }
    if let Some(claim) = claim {
        statements.push(format!(
            "LET $seat = (UPDATE $ref_claim SET {field} = ({field} ?? 0) + 1 RETURN VALUE id)"
        ));
        statements.push(format!(
            "IF array::len($seat) = 0 {{ THROW '{CLAIM_MARK}' }}"
        ));
        bindings.push(("ref_claim".into(), claim.into_value()));
    }
    bindings.push(("ref_expected".into(), expected.into_value()));
    statements.push(format!("LET $row = ({update})"));
    // Without this the claim would outlive a row write that matched nothing —
    // the very leak the transaction exists to close. Nothing was written yet
    // either way, so the probe that tells the two empty cases apart is free to
    // be another `UPDATE`: it matches only a row that is *there* and whose link
    // has moved off what the handler read, which is the CAS having bitten. A
    // deleted row and a plain guard refusal both miss it and keep the answer
    // they have always got.
    statements.push(format!(
        "IF array::len($row) = 0 {{ \
         LET $live = (UPDATE $id WHERE {link} != $ref_expected RETURN VALUE id); \
         IF array::len($live) = 0 {{ THROW '{ROW_MARK}' }} ELSE {{ THROW '{STALE_MARK}' }} }}"
    ));
    statements.push("RETURN $row".into());
    let sql = format!(
        "BEGIN TRANSACTION; {}; COMMIT TRANSACTION;",
        statements.join("; ")
    );
    // BEGIN is slot 0, so the trailing RETURN sits at `statements.len()`.
    let slot = statements.len();

    // One counter write in flight at a time, like every other counter write.
    let _guard = cap::counter_lock().await;
    let (mut result, mut errors) =
        transaction_with_retry(db, &sql, &bindings, &[CLAIM_MARK, ROW_MARK, STALE_MARK]).await?;
    if errors
        .values()
        .any(|error| error.to_string().contains(CLAIM_MARK))
    {
        return Err(refused);
    }
    if errors
        .values()
        .any(|error| error.to_string().contains(STALE_MARK))
    {
        return Err(AppError::Conflict(
            "the link this update moves changed since it was read; re-read and retry",
        ));
    }
    if errors
        .values()
        .any(|error| error.to_string().contains(ROW_MARK))
    {
        return Ok(Vec::new());
    }
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    // Sound only because the error map came back empty: `take_errors` swap-
    // removes errored slots, which would renumber the rest.
    Ok(result.take::<Vec<T>>(slot)?)
}
