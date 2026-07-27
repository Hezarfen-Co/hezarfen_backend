//! Count caps that hold across replicas.
//!
//! Every "at most N children per parent" rule used to be a count-then-write
//! under a process-wide `Mutex`, because a `BEGIN…COMMIT` cannot enforce it:
//! SurrealDB does not conflict-check a cross-record `count()` against a
//! concurrent insert (write-skew), so two racing writers both saw a free slot.
//! A mutex only serializes the writers *inside one process*, and the backend
//! now runs as two replicas against one database — so every one of those caps
//! was open again.
//!
//! The guard that does survive is a single-record conditional write: an
//! `UPDATE … SET n += 1 WHERE n < cap` on the *parent* row is atomic, so of N
//! concurrent claimers exactly `cap` get a non-empty result and the rest are
//! refused, whatever process they run in. The counter is therefore the
//! authority on how many slots are taken, and the child rows follow it:
//! [`claim`] before the insert, [`release`] if the insert then fails, and a
//! decrement in the *same transaction* as every delete of a child row.
//!
//! The counters are `option<int>` columns on the parent table, absent meaning
//! zero, so no Rust struct carries them and no whole-row save can clobber one
//! (every parent's `.content()` write is a create of a fresh row).
//
// ponytail: a counter can only drift if a future delete path forgets its
// decrement, and the boot backfill seeds counters once rather than recomputing
// them (recomputing on every boot would clobber a live peer's increments). If
// drift is ever observed, the repair is the same GROUP BY count the backfill
// runs, issued by hand with the field's `= NONE` guard dropped.

use surrealdb::types::RecordId;
use tokio::sync::Mutex;

use crate::constant::{CAP_WRITE_BACKOFF_MS, CAP_WRITE_TRIES};
use crate::database::{Database, lost_the_race};
use crate::domain::timestamp::Timestamp;
use crate::error::AppError;

/// One writer per process at a time, over every counter.
///
/// Not the cap — the `WHERE` clause is the cap, and it holds with or without
/// this. What the lock buys is that a replica sends its counter writes one at a
/// time, so the only contention the database ever sees on a parent row is
/// between replicas: at most as many concurrent writers as there are processes,
/// instead of as many as there are in-flight requests. That keeps the optimistic
/// retry loop below to a beat or two rather than a storm.
///
/// It is also what keeps the test suite deterministic. The embedded in-memory
/// engine the tests run on does *not* have the real server's conflict
/// detection: under two concurrent writes to one record it can drop one and
/// still answer `Ok` (probed — the stored counter stays correct, but ~13% of a
/// 16-racer round had one more caller told it had won than the counter agreed
/// to). The real ws server never did this in 150 rounds of the same probe.
//
// ponytail: one lock for every cap. Per-parent locks (a keyed map) if unrelated
// caps ever contend measurably — the counters are already independent, this is
// only about how many writes a process has in flight.
static CLAIM_LOCK: Mutex<()> = Mutex::const_new(());

/// "No cap" as a number, so the guard stays one comparison instead of a
/// nullable branch: `NONE`/`NULL` bind ambiguity is a worse trade than a
/// sentinel no roster will ever reach.
pub(crate) const UNLIMITED: i64 = i64::MAX;

/// Take one slot on `parent`'s `field` counter. `false` means the cap is full
/// (or the parent row is gone) and the caller must refuse — nothing was
/// written. `field` is always an in-crate constant, never user input.
pub(crate) async fn claim(
    parent: &RecordId,
    field: &str,
    cap: i64,
    db: &Database,
) -> Result<bool, AppError> {
    let sql = format!(
        "UPDATE $parent SET {field} = ({field} ?? 0) + 1 \
         WHERE ({field} ?? 0) < $cap RETURN VALUE id"
    );
    Ok(!write(&sql, parent, cap, db).await?.is_empty())
}

/// Give a claimed slot back, for the insert that never landed. Clamped at zero
/// so a double release can never push a counter negative and open the cap.
pub(crate) async fn release(parent: &RecordId, field: &str, db: &Database) -> Result<(), AppError> {
    let sql =
        format!("UPDATE $parent SET {field} = math::max([({field} ?? 0) - 1, 0]) RETURN VALUE id");
    write(&sql, parent, UNLIMITED, db).await?;
    Ok(())
}

/// Run one conditional counter write, retrying while the store reports a write
/// conflict.
///
/// Claimers contend on a single record by design — that contention *is* the
/// guard — and SurrealDB resolves it optimistically: the loser's transaction is
/// aborted with "can be retried" rather than queued, which the old mutex hid by
/// never letting two writers reach the database at once. So the queueing has to
/// happen here. Backoff is exponential with jitter, because racers arrive in
/// lockstep (one HTTP burst) and a fixed delay would just re-synchronize them.
async fn write(
    sql: &str,
    parent: &RecordId,
    cap: i64,
    db: &Database,
) -> Result<Vec<RecordId>, AppError> {
    let _guard = CLAIM_LOCK.lock().await;
    let mut last = None;
    for attempt in 0..CAP_WRITE_TRIES {
        if attempt > 0 {
            let step = CAP_WRITE_BACKOFF_MS << (attempt - 1);
            let jitter = Timestamp::now().as_millis().unsigned_abs() % step.max(1);
            tokio::time::sleep(std::time::Duration::from_millis(step + jitter)).await;
        }
        let attempted = async {
            let mut result = db
                .query(sql)
                .bind(("parent", parent.clone()))
                .bind(("cap", cap))
                .await?
                .check()?;
            result.take::<Vec<RecordId>>(0)
        }
        .await;
        match attempted {
            Ok(hit) => return Ok(hit),
            // `lost_the_race` also matches "already exists", which an UPDATE
            // cannot produce — what is being reused here is its transaction
            // conflict detection, typed details first and wording second.
            Err(err) if lost_the_race(&err) => last = Some(err),
            Err(err) => return Err(err.into()),
        }
    }
    Err(last
        .map(AppError::from)
        .unwrap_or_else(|| AppError::Internal("cap counter write never ran".into())))
}
