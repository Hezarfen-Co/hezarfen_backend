//! One `UPDATE <table> … WHERE id = $n RETURNING *` built from *only* the
//! fields a PATCH actually carried.
//!
//! WHY: a handler reads the row, then writes. Filling a field the request
//! omitted from that snapshot re-sends a value the client never gave, so a
//! concurrent PATCH of the *other* field that landed in between is silently
//! reverted — the lost update a whole-row save causes, just spelled field by
//! field. Scoping the `SET` is not enough; the *values* must come from the
//! request, so an absent field is never written at all.
//!
//! Callers take `Option<T>` per field (`None` = absent, keep) and hand them
//! straight here:
//!
//! ```ignore
//! FieldUpdate::new("note", note.id.0)
//!     .set("title", title)
//!     .set("content", content)
//!     .run::<Note>(db)
//!     .await
//! ```
//!
//! Clearing a nullable column is `Some(None)` on an `Option<Option<T>>`
//! field — pass it as `Some(value_or_none)` so `NULL` is written explicitly
//! (for a link column, `Param::OptUuid(None)`).
//!
//! Runtime-checked SQL by design (this module is the named exemption from the
//! compile-time `query!` rule): the `SET` list is the request's shape. All
//! emitted statements are plain Postgres — positional `$n` placeholders, the
//! row read back with `RETURNING *` (the old store's return-updated-document
//! spelling), and an explicit clear written as a bound `NULL`; there is no
//! remove-a-key statement a row needs a counterpart for.
//!
//! The dynamic parts of every statement here are table/column names (in-crate
//! `&'static str` constants, never client text) and the `$n` numbers this
//! builder prints itself; every value binds — which is the audit the
//! [`AssertSqlSafe`] wraps stand on.

use sqlx::postgres::{PgArguments, PgRow};
use sqlx::{AssertSqlSafe, FromRow, PgConnection};
use uuid::Uuid;

use crate::database::{Database, tx_with_retry};
use crate::db::page::Param;
use crate::error::AppError;

/// The statement-shaped markers [`run_with_refcount`]'s transaction aborts
/// with, mapped to their answers right after [`tx_with_retry`] returns. They
/// never reach the wire: a refusal is a decision, never a retry, and the
/// marker `Internal` error is replaced before it can escape.
const REFUSAL_MARK: &str = "ref_claim_gone";
const STALE_MARK: &str = "ref_stale_move";
const GONE_MARK: &str = "ref_row_gone";

/// The answer a moved link has always carried.
const STALE_MOVE: &str = "the link this update moves changed since it was read; re-read and retry";

/// An abort marker packed as the one error arm no caller ever observes.
fn mark(kind: &'static str) -> AppError {
    AppError::Internal(kind.to_owned())
}

/// `(counter table, counter field, link column, expected link, claim, release,
/// refusal)`.
type Refcount = (
    &'static str,
    &'static str,
    &'static str,
    Option<Uuid>,
    Option<Uuid>,
    Option<Uuid>,
    AppError,
);

pub(crate) struct FieldUpdate {
    /// The row's own table — an in-crate constant, never user input.
    table: &'static str,
    id: Uuid,
    /// `(field, value)` fragments, in the order they were set; a value's
    /// placeholder number is its position here plus one.
    sets: Vec<(&'static str, Param)>,
    /// `(low, high, refusal)` for [`FieldUpdate::ordered`].
    ordered: Option<(&'static str, &'static str, AppError)>,
    /// `(condition, refusal)` for [`FieldUpdate::guard`].
    guard: Option<(&'static str, AppError)>,
    /// `(counter table, counter field, link column, expected link, claim,
    /// release, refusal)` for [`FieldUpdate::refcount`].
    refcount: Option<Refcount>,
}

impl FieldUpdate {
    /// `table` names the row's table; `id` is its uuid primary key.
    pub(crate) fn new(table: &'static str, id: Uuid) -> Self {
        Self {
            table,
            id,
            sets: Vec::new(),
            ordered: None,
            guard: None,
            refcount: None,
        }
    }

    /// Write `field` only when the request carried it. The value binds as the
    /// `SET` list's next positional parameter, in call order.
    #[must_use]
    pub(crate) fn set<T: Into<Param>>(mut self, field: &'static str, value: Option<T>) -> Self {
        if let Some(value) = value {
            self.sets.push((field, value.into()));
        }
        self
    }

    /// Refuse the write unless `low <= high` still holds on the *merged* row —
    /// the new value for an end this PATCH carries, the stored column for one
    /// it omits. Emitted as a `WHERE` on the same `UPDATE`, so the comparison
    /// the handler made against its snapshot is re-made by the database at
    /// write time, against the row as it is *then*: two PATCHes each moving
    /// one end, each fine on its own, cannot commit an inverted range between
    /// them. That is what an in-process `Mutex` used to buy, without a window
    /// between the snapshot and the write for a concurrent request to slip
    /// into.
    ///
    /// Call it after the `.set()`s of both fields. A PATCH that moves neither
    /// end emits no guard at all (nothing to race), so a pre-existing inverted
    /// row stays editable field by field. `NULL` on either side passes,
    /// exactly like [`crate::web::check_time_range`] skipping an absent end.
    #[must_use]
    pub(crate) fn ordered(
        mut self,
        low: &'static str,
        high: &'static str,
        refused: AppError,
    ) -> Self {
        self.ordered = Some((low, high, refused));
        self
    }

    /// Refuse the write unless `condition` — a predicate on this same row —
    /// still holds at write time. The [`FieldUpdate::ordered`] guard for a
    /// precondition that is not about a range: a handler that read "nothing
    /// references this yet" re-asks the database at the instant it writes, so
    /// a reference landing in between refuses the edit instead of being
    /// edited out from under. `condition` is always an in-crate SQL literal,
    /// never user input.
    ///
    /// A request that carries no field at all emits no `UPDATE`, so it is not
    /// refused: it writes nothing, and reading the row back is a truthful
    /// answer to a PATCH that asked for no change.
    #[must_use]
    pub(crate) fn guard(mut self, condition: &'static str, refused: AppError) -> Self {
        self.guard = Some((condition, refused));
        self
    }

    /// Move a reference counter *with* this write: `claim` takes one on the
    /// row the link moves to (in `counter_table`), `release` gives one back
    /// on the row it moves off, and both ride the same transaction as the
    /// `UPDATE` that moves the link.
    ///
    /// That is the whole point. A claim sent as its own statement leaves a
    /// window where a crash strands a count on a row nothing links any more —
    /// and a count is what a delete guard reads, so the stranded one makes
    /// its parent undeletable forever. `refused` is the answer when the
    /// claimed row is gone (the conditional claim doubles as the existence
    /// check); the transaction then aborts, so the release and the row write
    /// never happened either.
    ///
    /// Only ever called alongside a `.set()` of the very column that carries
    /// the link, so the empty-request short-circuit below cannot swallow a
    /// move.
    ///
    /// `link` is that column (never a `.set()` field on this row: the counter
    /// lives on the *other* table — `course_count` on the term against a
    /// `term` link) and `expected` is what the handler's snapshot said it
    /// held — `None` for a link that was absent. The pair becomes a CAS on
    /// the row write, because claim and release are computed from that
    /// snapshot: two PATCHes moving the same link A→B both read A, and both
    /// would claim B, leaving B counted twice for one link — a count with no
    /// link is exactly what makes a term undeletable forever. The loser's row
    /// write matches nothing and the whole transaction aborts, so it claims
    /// nothing and answers 409.
    ///
    /// **Carrying the link and shifting a counter are separate questions**,
    /// and the CAS keys off the first one — `run` arms it for any request
    /// that `.set()`s this column, `claim`/`release` both `None` included. A
    /// PATCH *re-stating* the link its snapshot showed shifts no counter, but
    /// it is the same stale read: run it against a row someone else has since
    /// moved and the unguarded write silently drags the link back, stranding
    /// the winner's claim on a term nothing points at (undeletable forever)
    /// and leaving the reverted-to term linked at zero (deletable while
    /// linked). Gating the CAS on the counters is what let that through.
    #[must_use]
    pub(crate) fn refcount(
        mut self,
        counter_table: &'static str,
        counter_field: &'static str,
        link: &'static str,
        expected: Option<Uuid>,
        claim: Option<Uuid>,
        release: Option<Uuid>,
        refused: AppError,
    ) -> Self {
        self.refcount = Some((
            counter_table,
            counter_field,
            link,
            expected,
            claim,
            release,
            refused,
        ));
        self
    }

    /// Run the update and return the stored row. An empty request writes
    /// nothing at all and reads the row back unchanged.
    pub(crate) async fn run<T>(mut self, db: &Database) -> Result<T, AppError>
    where
        T: for<'r> FromRow<'r, PgRow> + Send + Unpin,
    {
        // Armed by the *request*, not by the counters: a PATCH that carries
        // the link column gets the CAS even when it re-states the value it
        // read and so shifts nothing. Decided here rather than in `refcount`
        // so the builder's call order cannot silently disarm it — `run` is
        // always last.
        let moved = self.refcount.take().filter(|rc| self.is_set(rc.2));
        if self.sets.is_empty() {
            let row: Option<T> = sqlx::query_as(AssertSqlSafe(format!(
                "SELECT * FROM {} WHERE id = $1",
                self.table
            )))
            .bind(self.id)
            .fetch_optional(db)
            .await?;
            return row.ok_or(AppError::NotFound);
        }
        // The `SET` list, each value binding as the next positional
        // parameter.
        let mut binds: Vec<Param> = Vec::new();
        let mut set_sql = String::new();
        for (index, (field, value)) in self.sets.iter().enumerate() {
            if index > 0 {
                set_sql.push_str(", ");
            }
            set_sql.push_str(field);
            set_sql.push_str(&format!(" = ${}", binds.len() + 1));
            binds.push(value.clone());
        }
        // Both ends stored: name the placeholder for an end this request set,
        // the column for one it left alone.
        let (ordered, mut refused) = match self.ordered.take() {
            Some((low, high, err)) if self.is_set(low) || self.is_set(high) => {
                let side = |field: &str, set: bool| {
                    if set {
                        format!("${}", self.number_of(field))
                    } else {
                        field.to_owned()
                    }
                };
                (
                    Some(format!(
                        "({} IS NULL OR {} IS NULL OR {} <= {})",
                        side(low, self.is_set(low)),
                        side(high, self.is_set(high)),
                        side(low, self.is_set(low)),
                        side(high, self.is_set(high)),
                    )),
                    Some(err),
                )
            }
            _ => (None, None),
        };
        let (extra, extra_refused) = match self.guard.take() {
            Some((condition, err)) => (Some(condition.to_owned()), Some(err)),
            None => (None, None),
        };
        // No caller sets both, so the refusal is whichever one is there.
        refused = refused.or(extra_refused);
        // A PATCH that does not carry the link at all writes the same
        // unguarded `UPDATE` it always did — it re-states nothing and races
        // nobody. The CAS is `IS NOT DISTINCT FROM`, so an absent link
        // (bound `None`) still compares truthfully.
        let mut expected_at: Option<usize> = None;
        let cas = moved.as_ref().map(|rc| {
            binds.push(Param::OptUuid(rc.3));
            expected_at = Some(binds.len());
            format!("{} IS NOT DISTINCT FROM ${}", rc.2, binds.len())
        });
        // The caller's own guards, kept apart from the CAS: the probe below
        // has to ask "did *only* the link move?", and a probe that ignores
        // them answers "the link moved" to a write its `.guard()` refused
        // outright.
        let own = ordered
            .iter()
            .chain(extra.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join(" AND ");
        let conditions: Vec<String> = ordered.into_iter().chain(extra).chain(cas).collect();
        let guard_sql = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };
        // The row id binds last, after the CAS value.
        binds.push(Param::Uuid(self.id));
        let id_at = binds.len();
        let update = format!(
            "UPDATE {} SET {}{} WHERE id = ${id_at} RETURNING *",
            self.table, set_sql, guard_sql
        );
        let rows: Vec<PgRow> = match moved {
            Some(rc) => {
                let still_mine = if own.is_empty() {
                    String::new()
                } else {
                    format!("({own}) AND ")
                };
                let probe = format!(
                    "SELECT 1 FROM {} WHERE id = ${id_at} AND {still_mine}{} \
                     IS DISTINCT FROM ${} LIMIT 1",
                    self.table,
                    rc.2,
                    expected_at.expect("the CAS arms with its link"),
                );
                run_with_refcount(db, update, probe, binds, rc).await?
            }
            None => {
                let mut args = PgArguments::default();
                for bind in binds {
                    bind.add_to(&mut args);
                }
                sqlx::query_with(AssertSqlSafe(update), args)
                    .fetch_all(db)
                    .await?
            }
        };
        let rows = rows
            .iter()
            .map(T::from_row)
            .collect::<Result<Vec<T>, sqlx::Error>>()?;
        // No row back means the guard bit failed (or, in the window after the
        // handler's read, the row was deleted — the guard cannot tell the two
        // apart, and reports the refusal it was given).
        rows.into_iter()
            .next()
            .ok_or_else(|| refused.take().unwrap_or(AppError::NotFound))
    }

    /// The `$n` the field's value bound as (set order is bind order).
    fn number_of(&self, field: &str) -> usize {
        self.sets
            .iter()
            .position(|(name, _)| *name == field)
            .expect("ordered() sides are .set() fields")
            + 1
    }

    fn is_set(&self, field: &str) -> bool {
        self.sets.iter().any(|(name, _)| *name == field)
    }
}

/// Send the update with its counter move, as one transaction.
///
/// A re-statement shifts no counter, so `claim` and `release` are both `None`
/// and the transaction is the CAS'd row write and its probe alone — still a
/// transaction, because the probe must see the same snapshot the write was
/// refused against.
///
/// Statement order is the old one, for the old reason: the release goes
/// first, so a move to a counter row that is gone decrements before the claim
/// refuses — and the rolled-back transaction demonstrably undid it. Every
/// abort takes the whole move back with it: nothing is committed unless the
/// row write landed.
async fn run_with_refcount(
    db: &Database,
    update: String,
    probe: String,
    binds: Vec<Param>,
    refcount: Refcount,
) -> Result<Vec<PgRow>, AppError> {
    // Destructured in the body, not the signature: a `&'static str` binding
    // lifted from an async fn's *pattern parameter* poisons the higher-ranked
    // `AsyncFnMut`/`Send` evaluation of any closure that captures it
    // ("`Send` would have to be implemented for `&'0 str`, for any lifetime").
    let (counter_table, counter_field, _link, _expected, claim, release, refused) = refcount;
    // Owned into the closure: a captured `&str` — even `'static` — drags the
    // async closure's arg lifetime off the higher-ranked one `tx_with_retry`
    // needs ("AsyncFnMut is not general enough" at the route registration).
    let counter_table = counter_table.to_owned();
    let counter_field = counter_field.to_owned();

    // The body lives in a named `async fn` over raw rows, the closure a
    // thin delegate (the house `decision_cas`/`save_in` shape): an inline
    // body of raw sqlx chains — or a delegate generic over the row type —
    // fails the higher-ranked `Send` check axum's handler registration runs
    // on `tx_with_retry`'s future. The `FromRow` decode happens on the
    // caller's side of that check.
    let outcome = tx_with_retry(db, false, async move |tx| {
        run_refcount_tx(
            &mut *tx,
            update.clone(),
            probe.clone(),
            binds.clone(),
            &counter_table,
            &counter_field,
            claim,
            release,
        )
        .await
    })
    .await;
    match outcome {
        // The claim's refusal rides out of the transaction as the bare mark
        // (a refusal is never retryable, so the loop stopped at it) and is
        // answered with the caller's real error here.
        Err(AppError::Internal(m)) if m == REFUSAL_MARK => Err(refused),
        Err(AppError::Internal(m)) if m == STALE_MARK => Err(AppError::Conflict(STALE_MOVE)),
        // The row is gone: exactly the "wrote nothing" answer the plain path
        // reports, so the caller's refusal is unchanged.
        Err(AppError::Internal(m)) if m == GONE_MARK => Ok(Vec::new()),
        other => other,
    }
}

/// One attempt of [`run_with_refcount`]'s transaction: the release, the
/// claim, the guarded row write, and — on an empty write — the probe that
/// tells a stale link apart from a row that is simply gone.
#[allow(clippy::too_many_arguments)]
async fn run_refcount_tx(
    tx: &mut PgConnection,
    update: String,
    probe: String,
    binds: Vec<Param>,
    counter_table: &str,
    counter_field: &str,
    claim: Option<Uuid>,
    release: Option<Uuid>,
) -> Result<Vec<PgRow>, AppError> {
    if let Some(release) = release {
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE {counter_table} \
                 SET {counter_field} = GREATEST({counter_field} - 1, 0) WHERE id = $1"
        )))
        .bind(release)
        .execute(&mut *tx)
        .await?;
    }
    // The conditional claim doubles as the existence check: zero rows
    // is the claimed row being gone, the caller's own refusal.
    if let Some(claim) = claim {
        let seat: Option<i32> = sqlx::query_scalar(AssertSqlSafe(format!(
            "UPDATE {counter_table} SET {counter_field} = {counter_field} + 1 \
                 WHERE id = $1 RETURNING 1"
        )))
        .bind(claim)
        .fetch_optional(&mut *tx)
        .await?;
        if seat.is_none() {
            return Err(mark(REFUSAL_MARK));
        }
    }
    let mut args = PgArguments::default();
    for bind in binds.iter().cloned() {
        bind.add_to(&mut args);
    }
    let rows: Vec<PgRow> = sqlx::query_with(AssertSqlSafe(update), args)
        .fetch_all(&mut *tx)
        .await?;
    if !rows.is_empty() {
        return Ok(rows);
    }
    // Zero rows: the caller's own guard bit, the row deleted in the
    // window after the handler's read, or — the CAS only — the link
    // moved since the handler read it. Nothing was written yet either
    // way, so the probe that tells the cases apart is free: it matches
    // only a row that is *there*, whose *own* guards still hold, and
    // whose link has moved off what the handler read — which is the
    // CAS, and nothing else, having bitten. Carrying the caller's
    // guards is what keeps that true when a write is refused for
    // *both* reasons at once: a `.guard()`/`.ordered()` refusal is the
    // caller's own answer to give, and reporting it as "the link
    // moved" would send the client to re-read a link that was never
    // its problem.
    let mut args = PgArguments::default();
    for bind in binds.iter().cloned() {
        bind.add_to(&mut args);
    }
    let live: Option<i32> = sqlx::query_scalar_with(AssertSqlSafe(probe), args)
        .fetch_optional(&mut *tx)
        .await?;
    match live {
        Some(_) => Err(mark(STALE_MARK)),
        None => Err(mark(GONE_MARK)),
    }
}
