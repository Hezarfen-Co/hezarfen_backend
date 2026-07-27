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

use surrealdb::types::{RecordId, SurrealValue};
use tokio::sync::Mutex;

use crate::constant::{CAP_WRITE_BACKOFF_MS, CAP_WRITE_TRIES, REF_COUNT_FIELD, REF_RETIRED_FIELD};
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
    claim_at(parent, field, cap, "", db).await
}

/// [`claim`], but only while `guard` — an extra predicate on that same parent
/// row — also holds. A caller whose insert has a second precondition (a homework
/// submission's file add is refused once the work is graded) gets both decided
/// by the one conditional write, instead of by a read a peer replica can outrun.
/// A miss is either "full" *or* "the guard failed"; the caller re-reads to tell
/// them apart, and only to pick the message. `guard` is always an in-crate SQL
/// literal, never user input.
pub(crate) async fn claim_when(
    parent: &RecordId,
    field: &str,
    cap: i64,
    guard: &str,
    db: &Database,
) -> Result<bool, AppError> {
    claim_at(parent, field, cap, guard, db).await
}

async fn claim_at(
    parent: &RecordId,
    field: &str,
    cap: i64,
    extra: &str,
    db: &Database,
) -> Result<bool, AppError> {
    let extra = if extra.is_empty() {
        String::new()
    } else {
        format!(" AND ({extra})")
    };
    let sql = format!(
        "UPDATE $parent SET {field} = ({field} ?? 0) + 1 \
         WHERE ({field} ?? 0) < $num{extra} RETURN VALUE id"
    );
    Ok(!write(&sql, parent, cap, db).await?.is_empty())
}

/// The `THROW` markers the in-transaction gates below abort with: no room, and
/// this pair already holds its row.
const FULL_MARK: &str = "cap_full";
const HELD_MARK: &str = "cap_held";

/// What [`claim_and_create`] settled.
pub(crate) enum Claimed<T> {
    /// The seat and the row committed together.
    Made(T),
    /// The cap is full or the parent row is gone — nothing was written.
    Full,
    /// Another writer placed this very row first, so this caller never owed a
    /// seat: nothing was written, and the winner's row is the answer.
    Duplicate,
}

/// Take a slot on `parent`'s `field` counter *and* write the child that fills
/// it, in one transaction.
///
/// [`claim`] followed by a separate insert cannot promise this. Where the child
/// carries a deterministic id (one row per pair), two writers placing the *same*
/// pair both pass the claim — neither row exists yet — so on a tight cap the
/// second is told "full" for a seat it was never going to need, and a release
/// afterwards is too late to unsay it. A process-wide mutex hid that inside one
/// process and hid nothing between two. Here the duplicate `CREATE` aborts the
/// transaction, which takes its own increment with it, and the caller reads the
/// winner's row back — so the counter never counts a row that does not exist,
/// in any replica.
///
/// The row is looked for *before* the seat is claimed, in that same
/// transaction, because on a tight cap the claim is the first thing to fail: a
/// member whose row a rival placed a moment ago would be told "full" about a
/// seat they already hold. The order makes "you are already in" outrank "there
/// is no room", which is what every caller's early return promises anyway.
pub(crate) async fn claim_and_create<T: SurrealValue + Clone>(
    parent: &RecordId,
    field: &str,
    cap: i64,
    id: &RecordId,
    content: &T,
    db: &Database,
) -> Result<Claimed<T>, AppError> {
    let sql = format!(
        "BEGIN TRANSACTION;
         LET $held = (SELECT VALUE id FROM $id);
         IF array::len($held) > 0 {{ THROW '{HELD_MARK}' }};
         LET $seat = (UPDATE $parent SET {field} = ({field} ?? 0) + 1 \
             WHERE ({field} ?? 0) < $num RETURN VALUE id);
         IF array::len($seat) = 0 {{ THROW '{FULL_MARK}' }};
         CREATE $id CONTENT $row;
         COMMIT TRANSACTION;"
    );
    let _guard = CLAIM_LOCK.lock().await;
    let mut last = None;
    for attempt in 0..CAP_WRITE_TRIES {
        backoff(attempt).await;
        let attempted = db
            .query(sql.as_str())
            .bind(("parent", parent.clone()))
            .bind(("num", cap))
            .bind(("id", id.clone()))
            .bind(("row", content.clone()))
            .await;
        let mut result = match attempted {
            Ok(result) => result,
            Err(err) if lost_the_race(&err) => {
                last = Some(err);
                continue;
            }
            Err(err) => return Err(err.into()),
        };
        // An aborted transaction errors *every* slot, most with a generic "not
        // executed" — only the failing slot says why.
        let mut errors = result.take_errors();
        // "Already a member" is read first: it outranks a full cap, and the two
        // never both fire (the gate aborts before the seat is touched).
        if errors
            .values()
            .any(|error| error.to_string().contains(HELD_MARK) || error.is_already_exists())
        {
            return Ok(Claimed::Duplicate);
        }
        if errors
            .values()
            .any(|error| error.to_string().contains(FULL_MARK))
        {
            return Ok(Claimed::Full);
        }
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            if !lost_the_race(&error) {
                return Err(error.into());
            }
            last = Some(error);
            continue;
        }
        // Slots count BEGIN, two LETs and two IFs: the CREATE is slot 5.
        return result
            .take::<Vec<T>>(5)?
            .into_iter()
            .next()
            .map(Claimed::Made)
            .ok_or_else(|| AppError::Internal("cap claim wrote no row".into()));
    }
    Err(last
        .map(AppError::from)
        .unwrap_or_else(|| AppError::Internal("cap counter write never ran".into())))
}

/// Wait out one lost round. Exponential with jitter, because racers arrive in
/// lockstep (one HTTP burst) and a fixed delay would just re-synchronize them.
/// Attempt zero waits not at all.
async fn backoff(attempt: usize) {
    if attempt == 0 {
        return;
    }
    let step = CAP_WRITE_BACKOFF_MS << (attempt - 1);
    let jitter = Timestamp::now().as_millis().unsigned_abs() % step.max(1);
    tokio::time::sleep(std::time::Duration::from_millis(step + jitter)).await;
}

/// Move `parent` to its next revision. A revision column is not a cap: it is
/// what a reader pins a value it took off the parent to, so that a write which
/// invalidates that value (a menu's price) can refuse the claim carrying the old
/// one. Every such mutation bumps *before* it writes, so a stale claim is
/// refused whether or not the mutation itself then lands.
pub(crate) async fn bump(parent: &RecordId, field: &str, db: &Database) -> Result<(), AppError> {
    let sql = format!("UPDATE $parent SET {field} = ({field} ?? 0) + 1 RETURN VALUE id");
    write(&sql, parent, UNLIMITED, db).await?;
    Ok(())
}

/// Give a claimed slot back, for the insert that never landed. Clamped at zero
/// so a double release can never push a counter negative and open the cap.
pub(crate) async fn release(parent: &RecordId, field: &str, db: &Database) -> Result<(), AppError> {
    let sql =
        format!("UPDATE $parent SET {field} = math::max([({field} ?? 0) - 1, 0]) RETURN VALUE id");
    write(&sql, parent, UNLIMITED, db).await?;
    Ok(())
}

// --- reference counters --------------------------------------------------
//
// The same single-record guard, pointed the other way. A cap asks "is there
// room for one more child?"; a reference counter asks "may this *name* still be
// used, and does anything still use it?" — the shape behind "an exam kind
// nothing is graded under may leave the school's settings". Both questions are
// answered by one row per name (`kind_ref:<name>`, `slot_ref:<name>`), created
// on first use, so the removal and the last claim contend on a record rather
// than on a cross-table count no transaction serializes.
//
// The two writes are exact mirrors: a claim lands only while the name is not
// retired, a retirement lands only while the count is zero. Whichever reaches
// the record first, the other is refused — in any process.

/// Take `n` references on `id`. `false` = the name is retired and the caller
/// must refuse; nothing was written.
pub(crate) async fn claim_ref(id: &RecordId, n: i64, db: &Database) -> Result<bool, AppError> {
    let sql = format!(
        "UPSERT $parent SET {REF_COUNT_FIELD} = ({REF_COUNT_FIELD} ?? 0) + $num \
         WHERE {REF_RETIRED_FIELD} != true RETURN VALUE id"
    );
    Ok(!write(&sql, id, n, db).await?.is_empty())
}

/// Give `n` references back — the referencing rows are gone (or never landed).
/// Clamped at zero like [`release`], so a double release cannot push a counter
/// below the rows it counts and let a used name be retired.
pub(crate) async fn release_ref(id: &RecordId, n: i64, db: &Database) -> Result<(), AppError> {
    let sql = format!(
        "UPSERT $parent SET {REF_COUNT_FIELD} = \
         math::max([({REF_COUNT_FIELD} ?? 0) - $num, 0]) RETURN VALUE id"
    );
    write(&sql, id, n, db).await?;
    Ok(())
}

/// Retire the name behind `id`: no further claim succeeds. `false` = something
/// still references it and the caller must refuse; nothing was written.
/// Idempotent — retiring an already-retired unused name lands again.
pub(crate) async fn retire(id: &RecordId, db: &Database) -> Result<bool, AppError> {
    // Parenthesized `??`: `count ?? 0 = 0` parses as `count ?? (0 = 0)`, which
    // is truthy for *every* row and would retire a name still in use.
    let sql = format!(
        "UPSERT $parent SET {REF_RETIRED_FIELD} = true \
         WHERE ({REF_COUNT_FIELD} ?? 0) = 0 RETURN VALUE id"
    );
    Ok(!write(&sql, id, UNLIMITED, db).await?.is_empty())
}

/// Put a name back in service — it re-entered the list it was retired from, or
/// the edit that retired it never landed.
pub(crate) async fn unretire(id: &RecordId, db: &Database) -> Result<(), AppError> {
    let sql = format!("UPSERT $parent SET {REF_RETIRED_FIELD} = false RETURN VALUE id");
    write(&sql, id, UNLIMITED, db).await?;
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
/// `num` is the statement's one number, bound as `$num`: a cap for the claims,
/// a step for the reference counters below.
async fn write(
    sql: &str,
    parent: &RecordId,
    num: i64,
    db: &Database,
) -> Result<Vec<RecordId>, AppError> {
    let _guard = CLAIM_LOCK.lock().await;
    let mut last = None;
    for attempt in 0..CAP_WRITE_TRIES {
        backoff(attempt).await;
        let attempted = async {
            let mut result = db
                .query(sql)
                .bind(("parent", parent.clone()))
                .bind(("num", num))
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
