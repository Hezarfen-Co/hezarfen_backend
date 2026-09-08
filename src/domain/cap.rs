//! Count caps the database enforces, not the process.
//!
//! Every "at most N children per parent" rule used to be a count-then-write
//! under a process-wide `Mutex`, because a `BEGIN…COMMIT` cannot enforce it:
//! SurrealDB does not conflict-check a cross-record `count()` against a
//! concurrent insert (write-skew), so two racing writers both saw a free slot.
//! A mutex serializes the writers, but only around whatever it wraps: every
//! one of those caps held a lock across a database round trip, and the count it
//! was protecting was already a guess by the time the insert landed.
//!
//! The guard that does survive is a single-record conditional write: an
//! `UPDATE … SET n += 1 WHERE n < cap` on the *parent* row is atomic, so of N
//! concurrent claimers exactly `cap` get a non-empty result and the rest are
//! refused, with no lock and no window. The counter is therefore the
//! authority on how many slots are taken, and the child rows follow it: the
//! claim and the child's insert land in one transaction ([`claim_and_create`]
//! and its variants), and a decrement rides the *same transaction* as every
//! delete of a child row.
//!
//! The counters are `option<int>` columns on the parent table, absent meaning
//! zero, so no Rust struct carries them and no whole-row save can clobber one
//! (every parent's `.content()` write is a create of a fresh row).
//
// corner-cut: a counter can only drift if a future delete path forgets its
// decrement, and the boot backfill seeds most counters once rather than
// recomputing them. `enrollment_count` is the one that already drifted (the boot
// sweep of promoted users' rows kept their seats), so it is recomputed on every
// boot instead — see the repair in `migration_sql::BACKFILL`. That is the shape
// to copy for any other counter drift observed: a counter whose meaning is
// exactly "the live child rows" converges on a recompute, a lifetime tally does
// not and must stay seeded once.

use surrealdb::types::{RecordId, SurrealValue};
use tokio::sync::Mutex;

use crate::constant::{CAP_WRITE_TRIES, REF_COUNT_FIELD, REF_RETIRED_FIELD, USER_ROLE_CLAIM_FIELD};
use crate::database::{Database, backoff, lost_the_race, transaction_with_retry};
use crate::error::AppError;

/// One counter writer at a time, over every counter.
///
/// Not the cap — the `WHERE` clause is the cap, and it holds with or without
/// this. What the lock buys is that the process sends its counter writes one at
/// a time, so the database never sees a parent row contended by every in-flight
/// request at once. That keeps the optimistic retry loop below to a beat or two
/// rather than a storm.
///
/// It is also what keeps the test suite deterministic. The embedded in-memory
/// engine the tests run on does *not* have the real server's conflict
/// detection: under two concurrent writes to one record it can drop one and
/// still answer `Ok` (probed — the stored counter stays correct, but ~13% of a
/// 16-racer round had one more caller told it had won than the counter agreed
/// to). The real ws server never did this in 150 rounds of the same probe.
//
// corner-cut: one lock for every cap. Per-parent locks (a keyed map) if unrelated
// caps ever contend measurably — the counters are already independent, this is
// only about how many writes a process has in flight.
static CLAIM_LOCK: Mutex<()> = Mutex::const_new(());

/// [`CLAIM_LOCK`], for a counter write that rides someone else's transaction
/// instead of [`write`]'s — [`crate::domain::field_update::FieldUpdate`]'s term
/// move claims and releases inside the very `UPDATE`'s transaction, and still
/// owes the process one counter write at a time.
///
/// **Is it load-bearing?** For the caps themselves, no — every one of them is a
/// single conditional write the store decides, and it decides it the same way
/// with a dozen writers in flight. So a counter write that skips this lock is
/// not a hole in a cap, and "every other site takes it" is not a correctness
/// argument. What the lock *is* load-bearing for is (a) the test suite, whose
/// in-memory engine forges wins under concurrency, and (b) any guard built out
/// of **two** statements that must not interleave with a rival's pair —
/// [`retire_name`] is exactly that, and it is why a skipped lock there would be
/// a real bug while a skipped lock around a lone `UPDATE … WHERE` is only
/// contention.
pub(crate) async fn counter_lock() -> tokio::sync::MutexGuard<'static, ()> {
    CLAIM_LOCK.lock().await
}

/// "No cap" as a number, so the guard stays one comparison instead of a
/// nullable branch: `NONE`/`NULL` bind ambiguity is a worse trade than a
/// sentinel no roster will ever reach.
pub(crate) const UNLIMITED: i64 = i64::MAX;

/// Take one slot on `parent`'s `field` counter, and nothing else. `false` means
/// the cap is full (or the parent row is gone) and the caller must refuse —
/// nothing was written. `field` is always an in-crate constant, never user
/// input.
///
/// **Test-only.** Every production claim now lands its child row in the same
/// transaction ([`claim_and_create`] and its variants); a bare claim followed by
/// a separate insert is the leak those exist to close. What survives is the
/// probe: the appointment tests put the double-book question to this `WHERE`
/// directly (a `join!` would ask the in-memory engine instead, which forges
/// wins), and staging a taken seat with *no* child row — exactly what a booking
/// mid-flight looks like — is a state no `*_and_create` can produce.
#[cfg(test)]
pub(crate) async fn claim(
    parent: &RecordId,
    field: &str,
    cap: i64,
    db: &Database,
) -> Result<bool, AppError> {
    let sql = format!(
        "UPDATE $parent SET {field} = ({field} ?? 0) + 1 \
         WHERE ({field} ?? 0) < $num RETURN VALUE id"
    );
    Ok(!write(&sql, parent, cap, db).await?.is_empty())
}

/// The `THROW` markers the in-transaction gates below abort with: no room, and
/// this pair already holds its row.
const FULL_MARK: &str = "cap_full";
const HELD_MARK: &str = "cap_held";

/// [`claim_ref_and_create`]'s third answer: the *name* is retired, which is not
/// a count being full — nothing empties it, and only putting the name back in
/// service lets a claim through.
const RETIRED_MARK: &str = "cap_retired";

/// The two markers [`claim_two_when_and_create`] aborts with, one per counter,
/// deliberately *not* shared: its two caps mean opposite things to the person
/// who hit one. The soft cap is a resettable working set (a whiteboard's live
/// canvas, emptied by a clear and drawn on again); the hard cap is the parent's
/// lifetime budget, and hitting it is terminal. One marker for both would tell
/// a caller a recoverable state is a permanent one.
const SOFT_FULL_MARK: &str = "cap_full_soft";
const HARD_FULL_MARK: &str = "cap_full_hard";

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
/// A bare claim followed by a separate insert cannot promise this. Where the child
/// carries a deterministic id (one row per pair), two writers placing the *same*
/// pair both pass the claim — neither row exists yet — so on a tight cap the
/// second is told "full" for a seat it was never going to need, and a release
/// afterwards is too late to unsay it. A process-wide mutex could not hide that
/// either: it is released around the very round trip the two writes race in.
/// Here the duplicate `CREATE` aborts the transaction, which takes its own
/// increment with it, and the caller reads the winner's row back — so the
/// counter never counts a row that does not exist.
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
    claim_at_and_create(parent, field, "$num", cap, "", 1, None, id, content, db).await
}

/// [`claim_and_create`] for a counter only *some* of a parent's children move:
/// the row lands either way, and `counts` decides whether the tally follows it.
///
/// The one live user is `exam_sat_total`, which counts exams sat rather than
/// sittings — a retake writes a row and moves nothing. Splitting that into
/// "count and create" versus a bare create outside this module would put the
/// uncounted write on a different statement to the counted one, and a
/// deterministic child id needs the duplicate abort either way: whichever
/// branch runs, exactly one writer's `CREATE` commits, and only that one's
/// increment survives with it. `counts` false still writes the seat statement
/// (`+ 0`), so a missing parent row is refused as [`Claimed::Full`] on both
/// paths rather than silently writing a child of nobody.
pub(crate) async fn create_counting<T: SurrealValue + Clone>(
    parent: &RecordId,
    field: &str,
    counts: bool,
    id: &RecordId,
    content: &T,
    db: &Database,
) -> Result<Claimed<T>, AppError> {
    let bump = i64::from(counts);
    claim_at_and_create(
        parent, field, "$num", UNLIMITED, "", bump, None, id, content, db,
    )
    .await
}

/// [`claim_and_create`] against a cap the *statement* reads rather than one the
/// caller snapshotted: `cap_expr` is SQL evaluated on the parent row at write
/// time (`audience.capacity ?? $num`, a subquery onto the settings singleton),
/// and `fallback` is the `$num` it coalesces to when the column is unset.
///
/// A snapshotted cap is only ever a claim about the past. Bound as an integer,
/// a `PATCH` that *lowers* the cap while N requests are in flight admits all N
/// anyway — each one is measured against the number its own read saw, and the
/// single-record guard that makes concurrent claimers safe against each other
/// says nothing about the writer of the cap itself. Read inside the same
/// conditional write, the lowered cap refuses the very next claimer, because
/// the write that lowered it and the write that reads it contend on one record.
///
/// `cap_expr` is always an in-crate SQL literal, never text from a client.
///
/// `holder` is the child's *other* parent when that parent is a user whose live
/// role decides whether the row may exist at all: `(the user record, the role
/// it may not carry)`, claimed by [`role_claim`] in this same transaction. A
/// refusal from it arrives as [`Claimed::Full`], which already means "full, or
/// the guard failed, or the parent is gone" — the caller re-reads to pick the
/// message, and only on that path.
pub(crate) async fn claim_live_and_create<T: SurrealValue + Clone>(
    parent: &RecordId,
    field: &str,
    cap_expr: &str,
    fallback: i64,
    holder: Option<(&RecordId, &str)>,
    new: (&RecordId, &T),
    db: &Database,
) -> Result<Claimed<T>, AppError> {
    let (id, content) = new;
    claim_at_and_create(
        parent, field, cap_expr, fallback, "", 1, holder, id, content, db,
    )
    .await
}

/// [`claim_and_create`], but only while `guard` — an extra predicate on that
/// same parent row — also holds: one conditional write decides the cap *and*
/// the caller's second precondition (a homework submission's file add is
/// refused once the work is graded), so no read a concurrent write can outrun
/// sits between them.
///
/// The contract that costs the caller something: [`Claimed::Full`] means "full
/// **or** the guard failed **or** the parent row is gone" — one marker for all
/// three, because the seat `UPDATE` matching nothing cannot say which clause
/// refused it. A caller that needs to tell them apart re-reads the parent, and
/// only to pick the message: none of the three is retryable into a success.
///
/// `guard` is always an in-crate SQL literal or built in-crate out of numbers
/// this crate read back itself — never text from a client.
pub(crate) async fn claim_when_and_create<T: SurrealValue + Clone>(
    parent: &RecordId,
    field: &str,
    cap: i64,
    guard: &str,
    id: &RecordId,
    content: &T,
    db: &Database,
) -> Result<Claimed<T>, AppError> {
    claim_at_and_create(parent, field, "$num", cap, guard, 1, None, id, content, db).await
}

#[allow(clippy::too_many_arguments)]
async fn claim_at_and_create<T: SurrealValue + Clone>(
    parent: &RecordId,
    field: &str,
    cap_expr: &str,
    cap: i64,
    extra: &str,
    bump: i64,
    holder: Option<(&RecordId, &str)>,
    id: &RecordId,
    content: &T,
    db: &Database,
) -> Result<Claimed<T>, AppError> {
    let extra = if extra.is_empty() {
        String::new()
    } else {
        format!(" AND ({extra})")
    };
    // The holder's claim rides between the duplicate gate and the seat, so a
    // caller whose row a rival placed is still told "you already have it", and
    // a demotion outranks a full cap: `Full` on a list with room left sends
    // nobody looking for a seat that is not the problem.
    let held_by = holder.map_or_else(Vec::new, |(_, unfit)| {
        role_claim("holder", unfit, FULL_MARK)
    });
    let hold = if held_by.is_empty() {
        String::new()
    } else {
        format!("{};\n", held_by.join(";\n"))
    };
    let sql = format!(
        "BEGIN TRANSACTION;
         LET $held = (SELECT VALUE id FROM $id);
         IF array::len($held) > 0 {{ THROW '{HELD_MARK}' }};
         {hold}LET $seat = (UPDATE $parent SET {field} = ({field} ?? 0) + $bump \
             WHERE ({field} ?? 0) < ({cap_expr}){extra} RETURN VALUE id);
         IF array::len($seat) = 0 {{ THROW '{FULL_MARK}' }};
         CREATE $id CONTENT $row;
         COMMIT TRANSACTION;"
    );
    let _guard = CLAIM_LOCK.lock().await;
    let mut last = None;
    for attempt in 0..CAP_WRITE_TRIES {
        backoff(attempt).await;
        let mut query = db
            .query(sql.as_str())
            .bind(("parent", parent.clone()))
            .bind(("num", cap))
            .bind(("bump", bump))
            .bind(("id", id.clone()))
            .bind(("row", content.clone()));
        if let Some((record, _)) = holder {
            query = query.bind(("holder", record.clone()));
        }
        let attempted = query.await;
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
        // A conflict in *any* slot is the whole batch's verdict, and it is read
        // before the arbitrary pick below: the siblings say only "not executed",
        // which is the *consequence* of this abort and never a reason of its
        // own. Picking one of those out of a `HashMap` — unordered, so it wins
        // most rounds — retired a retryable round as a 500.
        if errors.values().any(lost_the_race) {
            last = errors.drain().map(|(_, error)| error).find(lost_the_race);
            continue;
        }
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        // Slots count BEGIN, two LETs and two IFs: the CREATE is slot 5, plus
        // the holder claim's own statements when one is carried — counted, not
        // tallied by hand, so a statement added there cannot read back the
        // wrong result.
        let slot = 5 + held_by.len();
        return result
            .take::<Vec<T>>(slot)?
            .into_iter()
            .next()
            .map(Claimed::Made)
            .ok_or_else(|| AppError::Internal("cap claim wrote no row".into()));
    }
    Err(last
        .map(AppError::from)
        .unwrap_or_else(|| AppError::Internal("cap counter write never ran".into())))
}

/// The marker [`touch_and_create`] aborts with: the parent row is gone.
const GONE_MARK: &str = "cap_gone";

/// Write a child of `parent` while *colliding* with anything that removes the
/// parent — no seat spent, only the proof. `Ok(None)` = the parent is gone and
/// nothing was written.
///
/// Reading the parent first and then inserting cannot promise this, and neither
/// can a `SELECT` inside the transaction: SurrealDB 3.2.3 conflict-checks write
/// sets, not read sets, so a create landing inside a parent delete's window
/// reads a row that is still there (removed, uncommitted) while the delete's
/// `DELETE <child> WHERE parent = $parent` sweep already ran on a snapshot
/// without this row — both commit, and the child outlives its parent
/// unreachably, because every route to it goes through the parent.
///
/// So the proof is a *write* on the parent's own record, which is the key the
/// delete writes. `field` is moved — bumped and put back by captured value,
/// `NONE` included, so the row is byte-identical afterwards — because an
/// `UPDATE` that leaves the document unchanged is elided and never enters the
/// write set: `SET x = x` sits on no key at all and collides with nothing
/// (measured on 3.2.3, and the reason
/// [`crate::domain::class_pump::Axis::pivot_claim`] and
/// [`crate::domain::exam_answer::ExamAnswer::save`] have the same shape). Both
/// statements are inside the transaction, so an abort between them cannot leave
/// the counter up.
///
/// It lives here because it is [`claim_and_create`] with the cap removed: the
/// same single-record conditional write on the parent, asked "are you still
/// there?" instead of "is there room?". No [`CLAIM_LOCK`] either — the counter
/// nets zero, so no cap depends on the order these arrive in.
///
/// Admissible for [`crate::database::transaction_with_retry`]: `UPDATE`s and a
/// `CREATE` whose id is a freshly minted ULID no rival can aim at, so no
/// statement can legitimately answer "already exists" and a lost round wrote
/// nothing.
pub(crate) async fn touch_and_create<T: SurrealValue + Clone>(
    parent: &RecordId,
    field: &str,
    id: &RecordId,
    content: &T,
    db: &Database,
) -> Result<Option<T>, AppError> {
    let sql = format!(
        "BEGIN TRANSACTION;
         LET $was = (SELECT VALUE {field} FROM ONLY $parent);
         LET $alive = (UPDATE $parent SET {field} = ({field} ?? 0) + 1 RETURN VALUE id);
         IF array::len($alive) = 0 {{ THROW '{GONE_MARK}' }};
         UPDATE $parent SET {field} = $was;
         LET $made = (CREATE $id CONTENT $row RETURN AFTER);
         RETURN $made[0];
         COMMIT TRANSACTION;"
    );
    let (mut result, mut errors) = transaction_with_retry(
        db,
        &sql,
        &[
            ("parent".into(), parent.clone().into_value()),
            ("id".into(), id.clone().into_value()),
            ("row".into(), content.clone().into_value()),
        ],
        &[GONE_MARK],
    )
    .await?;
    // An aborted transaction errors *every* slot, most with a generic "not
    // executed" — only the THROW's own slot names the marker.
    if errors
        .values()
        .any(|error| error.to_string().contains(GONE_MARK))
    {
        return Ok(None);
    }
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    // The trailing `RETURN` is the last statement before `COMMIT`, so its slot
    // follows the statement count rather than a hand-kept number;
    // `num_statements` counts BEGIN and COMMIT.
    let slot = result.num_statements().saturating_sub(2);
    result
        .take::<Vec<T>>(slot)?
        .into_iter()
        .next()
        .map(Some)
        .ok_or_else(|| AppError::Internal("the parent touch wrote no row".into()))
}

/// The statements that put a transaction's write on a **user's own record**
/// while the role that row still carries may hold the grant being written —
/// [`touch_and_create`]'s trick pointed at the one key
/// [`crate::domain::user::User::set_role`] writes.
///
/// WHY: a demotion sheds every grant the old role implied, and it does that in
/// the role write's own transaction — but its sweeps are *snapshots*
/// (`DELETE <child> WHERE user = $usr`), and SurrealDB 3.2.3 conflict-checks
/// write sets, not read sets. A seat, an enrollment or a class membership
/// created after that snapshot therefore survives the sweep, and nothing ever
/// re-sweeps: the grant stands forever under a role that may not hold it. Every
/// one of those creates writes its *other* parent (the event, the course, the
/// class), so it shares no key with the demotion and the store has nothing to
/// settle.
///
/// So the counter named by [`USER_ROLE_CLAIM_FIELD`] is **moved** — bumped and
/// put back by captured value, `NONE` included, so the row is byte-identical
/// afterwards — because re-stating a value claims nothing: an `UPDATE` that
/// leaves the document unchanged is elided and enters no write set. The same
/// `UPDATE` hands back the role it found, so one statement is both the write
/// that collides and the read that decides. Either the sweep sees this row, or
/// this statement sees the demotion; and the caller that *loses* the conflict
/// re-sends into a fresh transaction that reads the new role, which is what
/// keeps the retry from turning a refusal into a success.
///
/// A user row that is not there claims nothing and refuses nothing: the
/// `UPDATE` matches no key, and every caller has already answered a missing
/// target its own way. `holder` names the binding the user record is bound to,
/// and `unfit` is an in-crate comparison against the role that may *not* hold
/// this grant — never text from a client.
pub(crate) fn role_claim(holder: &str, unfit: &str, mark: &str) -> Vec<String> {
    vec![
        format!("LET $role_was = (SELECT VALUE {USER_ROLE_CLAIM_FIELD} FROM ONLY ${holder})"),
        format!(
            "LET $role_now = (UPDATE ${holder} SET {USER_ROLE_CLAIM_FIELD} = \
             ({USER_ROLE_CLAIM_FIELD} ?? 0) + 1 RETURN VALUE role)"
        ),
        format!("IF array::len($role_now) > 0 AND $role_now[0] {unfit} {{ THROW '{mark}' }}"),
        format!("UPDATE ${holder} SET {USER_ROLE_CLAIM_FIELD} = $role_was"),
    ]
}

/// What [`claim_two_when_and_create`] settled.
pub(crate) enum ClaimedTwo<T> {
    /// Both seats and the row committed together.
    Made(T),
    /// The *resettable* counter (`fields[0]`) is full — nothing was written,
    /// and whatever empties it lets the caller through again.
    FullSoft,
    /// The *lifetime* counter (`fields[1]`) is full, the guard failed, or the
    /// parent row is gone — nothing was written. The caller re-reads the parent
    /// to tell those apart, and only to pick the message: both are refusals the
    /// caller cannot retry into a success.
    FullHard,
}

/// Take a slot on *two* of `parent`'s counters, while `guard` holds, and write
/// the child that fills them — one transaction, one verdict.
///
/// Two separate guarded claims cannot promise this. A parent whose children are
/// counted twice — once against a resettable working set, once against a
/// lifetime budget — has the two counters describing the same rows, so a writer
/// that lands one increment and loses the other leaves them disagreeing
/// forever, and the lifetime cap stops bounding anything. Here either both
/// increments and the row commit, or the transaction aborts having written
/// nothing.
///
/// The hard cap is claimed *first*, carrying the guard, so a terminal refusal
/// outranks a recoverable one: a parent that has spent its lifetime budget is
/// not helped by being told its resettable counter is also full.
///
/// There is no `$held` gate — [`claim_and_create`]'s exists because its child
/// carries a deterministic id two writers can both aim at. This one's caller
/// mints a fresh monotonic ULID on a table with no `UNIQUE` index, so no rival
/// can target that id, and its absence is also what keeps the batch retryable:
/// no statement in it can legitimately answer "already exists" (see
/// [`crate::database::transaction_with_retry`]).
///
/// The two counters are named one by one rather than passed as a pair of
/// arrays: which slot is the resettable one and which is terminal decides
/// whether a refusal is phrased "clear it and carry on" or "this is over", and
/// two `[i64; 2]` positions swapped by mistake compile, pass every test — both
/// slots are the same type — and misreport that forever. The names are the
/// guard against it.
///
/// The fields are always in-crate constants, and `guard` is either one or
/// built in-crate out of numbers this crate read back itself
/// (`board_stroke::append` appends an `epoch` it took off the board row) —
/// never text from a client.
pub(crate) async fn claim_two_when_and_create<T: SurrealValue + Clone>(
    parent: &RecordId,
    soft: (&str, i64),
    hard: (&str, i64),
    guard: &str,
    id: &RecordId,
    content: &T,
    db: &Database,
) -> Result<ClaimedTwo<T>, AppError> {
    let (soft_field, soft_cap) = soft;
    let (hard_field, hard_cap) = hard;
    // Parenthesized `??` throughout: `n ?? 0 < $cap` parses as `n ?? (0 < $cap)`,
    // which is truthy for every row and would claim past the cap.
    let sql = format!(
        "BEGIN TRANSACTION;
         LET $lifetime = (UPDATE $parent SET {hard_field} = ({hard_field} ?? 0) + 1 \
             WHERE ({hard_field} ?? 0) < $hard AND ({guard}) RETURN VALUE id);
         IF array::len($lifetime) = 0 {{ THROW '{HARD_FULL_MARK}' }};
         LET $seat = (UPDATE $parent SET {soft_field} = ({soft_field} ?? 0) + 1 \
             WHERE ({soft_field} ?? 0) < $soft RETURN VALUE id);
         IF array::len($seat) = 0 {{ THROW '{SOFT_FULL_MARK}' }};
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
            .bind(("soft", soft_cap))
            .bind(("hard", hard_cap))
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
        // executed" — only the failing slot says why. The two markers are read
        // before the conflict check, because a `THROW` is a decision.
        let mut errors = result.take_errors();
        if errors
            .values()
            .any(|error| error.to_string().contains(HARD_FULL_MARK))
        {
            return Ok(ClaimedTwo::FullHard);
        }
        if errors
            .values()
            .any(|error| error.to_string().contains(SOFT_FULL_MARK))
        {
            return Ok(ClaimedTwo::FullSoft);
        }
        if errors.values().any(lost_the_race) {
            last = errors.drain().map(|(_, error)| error).find(lost_the_race);
            continue;
        }
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        // Slots count BEGIN, two LETs and two IFs: the CREATE is slot 5.
        return result
            .take::<Vec<T>>(5)?
            .into_iter()
            .next()
            .map(ClaimedTwo::Made)
            .ok_or_else(|| AppError::Internal("cap claim wrote no row".into()));
    }
    Err(last
        .map(AppError::from)
        .unwrap_or_else(|| AppError::Internal("cap counter write never ran".into())))
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

/// What [`claim_ref_and_create`] settled.
pub(crate) enum ClaimedRef<T> {
    /// The reference and the row committed together.
    Made(T),
    /// The name is retired — nothing was written, and no retry helps until the
    /// name is put back in service.
    Retired,
    /// Another writer placed this very row first, so this caller never owed a
    /// reference: nothing was written, and the winner's row is the answer.
    Duplicate,
}
//
// A separate enum rather than a `Retired` variant on [`Claimed`]: that enum is
// matched exhaustively by a dozen callers in files this change does not touch,
// and a cap has no retired state to answer for anyway.

/// Take one reference on `counter` *and* write the row that holds it, in one
/// transaction — [`claim_and_create`] pointed at a name instead of a cap.
///
/// A bare reference claim followed by a separate insert cannot promise this: it
/// lands, the insert then fails or the process dies, and the name is counted as
/// used by a row that does not exist — which is a name nobody can ever retire.
/// Here the abort takes the increment with it.
///
/// The `$held` gate comes first for [`claim_and_create`]'s reason: where the
/// child carries a deterministic id, a rival can place the very row this caller
/// is placing, and "you already have it" is the honest answer — one that must
/// not cost a reference. Which is also why this carries its own retry loop
/// rather than [`crate::database::transaction_with_retry`]'s: the `CREATE` can
/// legitimately answer "already exists", and that loop cannot tell that answer
/// from a lost round. The `UPSERT` cannot answer it — `kind_ref`/`slot_ref`
/// carry no `UNIQUE` index, so a write keyed by id resolves onto the row the id
/// names.
pub(crate) async fn claim_ref_and_create<T: SurrealValue + Clone>(
    counter: &RecordId,
    n: i64,
    id: &RecordId,
    content: &T,
    db: &Database,
) -> Result<ClaimedRef<T>, AppError> {
    let sql = format!(
        "BEGIN TRANSACTION;
         LET $held = (SELECT VALUE id FROM $id);
         IF array::len($held) > 0 {{ THROW '{HELD_MARK}' }};
         LET $ref = (UPSERT $parent SET {REF_COUNT_FIELD} = ({REF_COUNT_FIELD} ?? 0) + $num \
             WHERE {REF_RETIRED_FIELD} != true RETURN VALUE id);
         IF array::len($ref) = 0 {{ THROW '{RETIRED_MARK}' }};
         CREATE $id CONTENT $row;
         COMMIT TRANSACTION;"
    );
    let _guard = CLAIM_LOCK.lock().await;
    let mut last = None;
    for attempt in 0..CAP_WRITE_TRIES {
        backoff(attempt).await;
        let attempted = db
            .query(sql.as_str())
            .bind(("parent", counter.clone()))
            .bind(("num", n))
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
        // executed" — only the failing slot says why. The markers are read
        // before the conflict check, because a `THROW` is a decision.
        let mut errors = result.take_errors();
        if errors
            .values()
            .any(|error| error.to_string().contains(HELD_MARK) || error.is_already_exists())
        {
            return Ok(ClaimedRef::Duplicate);
        }
        if errors
            .values()
            .any(|error| error.to_string().contains(RETIRED_MARK))
        {
            return Ok(ClaimedRef::Retired);
        }
        if errors.values().any(lost_the_race) {
            last = errors.drain().map(|(_, error)| error).find(lost_the_race);
            continue;
        }
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        // Slots count BEGIN, two LETs and two IFs: the CREATE is slot 5.
        return result
            .take::<Vec<T>>(5)?
            .into_iter()
            .next()
            .map(ClaimedRef::Made)
            .ok_or_else(|| AppError::Internal("cap claim wrote no row".into()));
    }
    Err(last
        .map(AppError::from)
        .unwrap_or_else(|| AppError::Internal("cap counter write never ran".into())))
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
    /// Retirement only: something still references the name and the caller must
    /// refuse. Nothing was written.
    InUse,
}

/// Retire the name behind `id`: no further claim succeeds. Idempotent — a
/// retirement of an already-retired unused name lands again, and says so
/// ([`Switched::Unchanged`]) so the caller does not roll back a bit it never
/// set.
pub(crate) async fn retire_name(id: &RecordId, db: &Database) -> Result<Switched, AppError> {
    // Parenthesized `??`: `count ?? 0 = 0` parses as `count ?? (0 = 0)`, which
    // is truthy for *every* row and would retire a name still in use.
    //
    // The bit as it stood is read in the *same round trip* as the write, so the
    // pair is one hold of `CLAIM_LOCK` (see [`switch`]) and no rival in this
    // process can retire the name between them and be undone by this caller.
    let sql = format!(
        "SELECT VALUE ({REF_RETIRED_FIELD} ?? false) FROM $parent;
         UPSERT $parent SET {REF_RETIRED_FIELD} = true \
             WHERE ({REF_COUNT_FIELD} ?? 0) = 0 RETURN VALUE id"
    );
    let (was, landed) = switch(&sql, id, db).await?;
    Ok(match (landed, was) {
        (false, _) => Switched::InUse,
        (true, true) => Switched::Unchanged,
        (true, false) => Switched::Flipped,
    })
}

/// Put a name back in service — it re-entered the list it was retired from, or
/// the edit that retired it never landed. [`Switched::Flipped`] only when the
/// name really was retired, for [`retire_name`]'s reason pointed the other way.
pub(crate) async fn unretire_name(id: &RecordId, db: &Database) -> Result<Switched, AppError> {
    let sql = format!(
        "SELECT VALUE ({REF_RETIRED_FIELD} ?? false) FROM $parent;
         UPSERT $parent SET {REF_RETIRED_FIELD} = false RETURN VALUE id"
    );
    let (was, _) = switch(&sql, id, db).await?;
    Ok(if was {
        Switched::Flipped
    } else {
        Switched::Unchanged
    })
}

/// [`retire_name`] as the yes/no the domain probes ask — did the name end up
/// retired? **Test-only**: production callers roll their edit back and need to
/// know which of the two "yes" answers they got.
#[cfg(test)]
pub(crate) async fn retire(id: &RecordId, db: &Database) -> Result<bool, AppError> {
    Ok(!matches!(retire_name(id, db).await?, Switched::InUse))
}

/// The read of the `retired` bit and the write that moves it, one round trip
/// under one [`CLAIM_LOCK`] hold: `(bit as it stood, did the write match)`.
async fn switch(sql: &str, parent: &RecordId, db: &Database) -> Result<(bool, bool), AppError> {
    let _guard = CLAIM_LOCK.lock().await;
    let mut last = None;
    for attempt in 0..CAP_WRITE_TRIES {
        backoff(attempt).await;
        let attempted = async {
            let mut result = db
                .query(sql)
                .bind(("parent", parent.clone()))
                .await?
                .check()?;
            let was = result
                .take::<Vec<bool>>(0)?
                .first()
                .copied()
                .unwrap_or(false);
            let landed = !result.take::<Vec<RecordId>>(1)?.is_empty();
            Ok::<_, surrealdb::Error>((was, landed))
        }
        .await;
        match attempted {
            Ok(answer) => return Ok(answer),
            Err(err) if lost_the_race(&err) => last = Some(err),
            Err(err) => return Err(err.into()),
        }
    }
    Err(last
        .map(AppError::from)
        .unwrap_or_else(|| AppError::Internal("cap counter write never ran".into())))
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
/// `num` is the statement's one number, bound as `$num` — the cap.
///
/// **Test-only**, like its one caller [`claim`]: every production counter write
/// now rides its child row's transaction and carries its own loop.
#[cfg(test)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::{
        BOARD_EPOCH_STROKE_COUNT_FIELD, BOARD_OPEN_GUARD, BOARD_TOTAL_STROKE_COUNT_FIELD,
    };

    #[derive(Debug, Clone, SurrealValue)]
    struct Stroke {
        board: RecordId,
        author: RecordId,
        kind: String,
        epoch: i64,
        created_at: i64,
    }

    /// The pair of counters, re-read out of the store — never off a return
    /// value, which the in-memory engine forges wins on (see `CLAIM_LOCK`).
    async fn stored(db: &Database) -> (i64, i64) {
        let mut result = db
            .query("SELECT VALUE [epoch_stroke_count ?? 0, total_stroke_count ?? 0] FROM board:b")
            .await
            .unwrap()
            .check()
            .unwrap();
        let rows: Vec<Vec<i64>> = result.take(0).unwrap();
        (rows[0][0], rows[0][1])
    }

    async fn strokes(db: &Database) -> usize {
        let mut result = db
            .query("SELECT VALUE id FROM board_stroke")
            .await
            .unwrap()
            .check()
            .unwrap();
        result.take::<Vec<RecordId>>(0).unwrap().len()
    }

    async fn a_board(epoch: i64, total: i64, locked: bool) -> Database {
        let db = crate::database::init_mem().await.unwrap();
        db.query(
            "CREATE user:u SET username = 'u', password_hash = 'x';
             CREATE board:b SET creator = user:u, title = 't', participants = [user:u],
                 locked = $locked, epoch = 0, epoch_stroke_count = $epoch,
                 total_stroke_count = $total, created_at = 1;",
        )
        .bind(("locked", locked))
        .bind(("epoch", epoch))
        .bind(("total", total))
        .await
        .unwrap()
        .check()
        .unwrap();
        db
    }

    fn a_stroke() -> Stroke {
        Stroke {
            board: RecordId::new("board", "b"),
            author: RecordId::new("user", "u"),
            kind: "stroke".into(),
            epoch: 0,
            created_at: 2,
        }
    }

    async fn claim(db: &Database, caps: [i64; 2], key: &str) -> ClaimedTwo<Stroke> {
        claim_two_when_and_create(
            &RecordId::new("board", "b"),
            (BOARD_EPOCH_STROKE_COUNT_FIELD, caps[0]),
            (BOARD_TOTAL_STROKE_COUNT_FIELD, caps[1]),
            BOARD_OPEN_GUARD,
            &RecordId::new("board_stroke", key),
            &a_stroke(),
            db,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn both_counters_move_together_or_not_at_all() {
        let db = a_board(0, 0, false).await;
        assert!(matches!(
            claim(&db, [2, 2], "s1").await,
            ClaimedTwo::Made(_)
        ));
        assert_eq!(stored(&db).await, (1, 1));
        assert_eq!(strokes(&db).await, 1);
    }

    /// One cap refuses; the *other* counter must not have advanced, asserted by
    /// re-reading the row. A drift here would let the lifetime cap be passed.
    #[tokio::test]
    async fn a_refused_claim_advances_neither_counter() {
        // Soft (epoch) full, hard (lifetime) with room to spare.
        let db = a_board(5, 5, false).await;
        assert!(matches!(
            claim(&db, [5, 99], "s1").await,
            ClaimedTwo::FullSoft
        ));
        assert_eq!(stored(&db).await, (5, 5));
        assert_eq!(strokes(&db).await, 0);

        // Hard full, soft with room to spare.
        let db = a_board(5, 5, false).await;
        assert!(matches!(
            claim(&db, [99, 5], "s1").await,
            ClaimedTwo::FullHard
        ));
        assert_eq!(stored(&db).await, (5, 5));
        assert_eq!(strokes(&db).await, 0);
    }

    /// The two `THROW` markers must reach the caller as *different* answers:
    /// one board is recoverable by a clear, the other is read-only for good.
    #[tokio::test]
    async fn the_two_caps_refuse_distinguishably() {
        let db = a_board(5, 5, false).await;
        let soft = claim(&db, [5, 99], "s1").await;
        let hard = claim(&db, [99, 5], "s2").await;
        assert!(matches!(soft, ClaimedTwo::FullSoft));
        assert!(matches!(hard, ClaimedTwo::FullHard));
        assert_eq!(stored(&db).await, (5, 5));
    }

    #[tokio::test]
    async fn the_guard_refuses_a_locked_board() {
        let db = a_board(0, 0, true).await;
        assert!(matches!(
            claim(&db, [9, 9], "s1").await,
            ClaimedTwo::FullHard
        ));
        assert_eq!(stored(&db).await, (0, 0));
        assert_eq!(strokes(&db).await, 0);
    }

    // --- claim_when_and_create ------------------------------------------

    async fn claim_one(db: &Database, cap: i64, key: &str) -> Claimed<Stroke> {
        claim_when_and_create(
            &RecordId::new("board", "b"),
            BOARD_EPOCH_STROKE_COUNT_FIELD,
            cap,
            BOARD_OPEN_GUARD,
            &RecordId::new("board_stroke", key),
            &a_stroke(),
            db,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_guarded_claim_advances_its_counter_exactly_once() {
        let db = a_board(0, 0, false).await;
        assert!(matches!(claim_one(&db, 9, "s1").await, Claimed::Made(_)));
        assert_eq!(stored(&db).await, (1, 0));
        assert_eq!(strokes(&db).await, 1);
    }

    /// A failed guard is refused as `Full` (the one marker covers full, guard
    /// and missing parent), and must leave the counter and the table alone.
    #[tokio::test]
    async fn a_guard_refusal_advances_neither_counter_nor_row() {
        let db = a_board(0, 0, true).await;
        assert!(matches!(claim_one(&db, 9, "s1").await, Claimed::Full));
        assert_eq!(stored(&db).await, (0, 0));
        assert_eq!(strokes(&db).await, 0);
    }

    /// The second writer at the same id owes no seat: the row is already there,
    /// so the counter must not move a second time for it.
    #[tokio::test]
    async fn a_duplicate_row_claims_no_seat() {
        let db = a_board(0, 0, false).await;
        assert!(matches!(claim_one(&db, 9, "s1").await, Claimed::Made(_)));
        assert!(matches!(claim_one(&db, 9, "s1").await, Claimed::Duplicate));
        assert_eq!(stored(&db).await, (1, 0));
        assert_eq!(strokes(&db).await, 1);
    }

    // --- claim_ref_and_create -------------------------------------------

    #[derive(Debug, Clone, SurrealValue)]
    struct Menu {
        date: String,
        slot: String,
        created_by: RecordId,
        created_at: i64,
    }

    async fn a_slot(count: i64, retired: bool) -> Database {
        let db = crate::database::init_mem().await.unwrap();
        db.query(
            "CREATE user:u SET username = 'u', password_hash = 'x';
             UPSERT slot_ref:lunch SET count = $count, retired = $retired;",
        )
        .bind(("count", count))
        .bind(("retired", retired))
        .await
        .unwrap()
        .check()
        .unwrap();
        db
    }

    /// The reference count, re-read out of the store — never off a return
    /// value, which the in-memory engine forges wins on (see `CLAIM_LOCK`).
    async fn refs(db: &Database) -> i64 {
        let mut result = db
            .query("SELECT VALUE (count ?? 0) FROM slot_ref:lunch")
            .await
            .unwrap()
            .check()
            .unwrap();
        result.take::<Vec<i64>>(0).unwrap()[0]
    }

    async fn menus(db: &Database) -> usize {
        let mut result = db
            .query("SELECT VALUE id FROM menu")
            .await
            .unwrap()
            .check()
            .unwrap();
        result.take::<Vec<RecordId>>(0).unwrap().len()
    }

    async fn claim_slot(db: &Database, key: &str) -> ClaimedRef<Menu> {
        claim_ref_and_create(
            &RecordId::new(crate::constant::SLOT_REF_TABLE, "lunch"),
            1,
            &RecordId::new("menu", key),
            &Menu {
                date: "2026-08-02".into(),
                slot: "lunch".into(),
                created_by: RecordId::new("user", "u"),
                created_at: 1,
            },
            db,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_reference_and_its_row_land_together() {
        let db = a_slot(0, false).await;
        assert!(matches!(claim_slot(&db, "m1").await, ClaimedRef::Made(_)));
        assert_eq!(refs(&db).await, 1);
        assert_eq!(menus(&db).await, 1);
    }

    /// A retired name refuses, and the refusal must cost nothing: no row, no
    /// count — a counted row on a retired name is a name nobody can retire.
    #[tokio::test]
    async fn a_retired_name_refuses_with_no_row_and_no_count() {
        let db = a_slot(0, true).await;
        assert!(matches!(claim_slot(&db, "m1").await, ClaimedRef::Retired));
        assert_eq!(refs(&db).await, 0);
        assert_eq!(menus(&db).await, 0);
    }

    /// The `retired` bit as the store holds it, `None` when the row is absent.
    async fn bit(db: &Database) -> Option<bool> {
        let mut result = db
            .query("SELECT VALUE retired FROM slot_ref:lunch")
            .await
            .unwrap()
            .check()
            .unwrap();
        result.take::<Vec<bool>>(0).unwrap().first().copied()
    }

    fn lunch() -> RecordId {
        RecordId::new(crate::constant::SLOT_REF_TABLE, "lunch")
    }

    /// Retirement is idempotent, so "it is retired now" is *not* "I retired
    /// it" — and a caller that rolls back on the second answer un-retires a
    /// name somebody else legitimately took out of service. That is the whole
    /// settings-CAS wedge: the list drops the name, the counter says in
    /// service, the name grades again and can then never be removed.
    #[tokio::test]
    async fn a_second_retirement_says_it_changed_nothing() {
        let db = a_slot(0, false).await;
        assert!(matches!(
            retire_name(&lunch(), &db).await.unwrap(),
            Switched::Flipped
        ));
        assert!(matches!(
            retire_name(&lunch(), &db).await.unwrap(),
            Switched::Unchanged
        ));
        assert_eq!(bit(&db).await, Some(true), "the winner's bit still stands");
    }

    /// The mirror: only the un-retire that really put a name back in service
    /// owes a re-retirement, so a re-add that never lands cannot be undone
    /// twice.
    #[tokio::test]
    async fn a_second_un_retirement_says_it_changed_nothing() {
        let db = a_slot(0, true).await;
        assert!(matches!(
            unretire_name(&lunch(), &db).await.unwrap(),
            Switched::Flipped
        ));
        assert!(matches!(
            unretire_name(&lunch(), &db).await.unwrap(),
            Switched::Unchanged
        ));
        assert_eq!(bit(&db).await, Some(false));
    }

    /// A referenced name is refused, and the refusal writes nothing — the
    /// caller must not record an undo for it either.
    #[tokio::test]
    async fn a_referenced_name_refuses_and_leaves_the_bit_alone() {
        let db = a_slot(1, false).await;
        assert!(matches!(
            retire_name(&lunch(), &db).await.unwrap(),
            Switched::InUse
        ));
        assert_eq!(bit(&db).await, Some(false));
    }

    // --- claim_live_and_create ------------------------------------------

    /// The cap the statement reads and the cap the caller remembers are
    /// different numbers the moment anyone edits the cap. `epoch` stands in for
    /// a capacity column here — any int on the parent row does.
    #[tokio::test]
    async fn a_live_cap_refuses_where_the_caller_s_snapshot_admits() {
        let db = a_board(1, 0, false).await;
        db.query("UPDATE board:b SET epoch = 1")
            .await
            .unwrap()
            .check()
            .unwrap();

        // Live: one seat, one taken — full, and nothing written.
        let live = claim_live_and_create(
            &RecordId::new("board", "b"),
            BOARD_EPOCH_STROKE_COUNT_FIELD,
            "epoch ?? $num",
            UNLIMITED,
            None,
            (&RecordId::new("board_stroke", "s1"), &a_stroke()),
            &db,
        )
        .await
        .unwrap();
        assert!(matches!(live, Claimed::Full));
        assert_eq!(stored(&db).await, (1, 0));
        assert_eq!(strokes(&db).await, 0);

        // The same row, the same instant, against a cap of 5 the caller read
        // before it was lowered: admitted. That gap is the over-admission.
        assert!(matches!(claim_one(&db, 5, "s2").await, Claimed::Made(_)));
        assert_eq!(stored(&db).await, (2, 0));
    }

    /// An unset capacity column means "no cap", which is the bound `$num`
    /// fallback — not a cap of zero that refuses everything.
    #[tokio::test]
    async fn an_absent_live_cap_falls_back_to_the_bound_number() {
        let db = a_board(0, 0, false).await;
        let made = claim_live_and_create(
            &RecordId::new("board", "b"),
            BOARD_EPOCH_STROKE_COUNT_FIELD,
            "capacity ?? $num",
            UNLIMITED,
            None,
            (&RecordId::new("board_stroke", "s1"), &a_stroke()),
            &db,
        )
        .await
        .unwrap();
        assert!(matches!(made, Claimed::Made(_)));
        assert_eq!(stored(&db).await, (1, 0));
    }

    #[tokio::test]
    async fn a_duplicate_row_claims_no_reference() {
        let db = a_slot(0, false).await;
        assert!(matches!(claim_slot(&db, "m1").await, ClaimedRef::Made(_)));
        assert!(matches!(claim_slot(&db, "m1").await, ClaimedRef::Duplicate));
        assert_eq!(refs(&db).await, 1);
        assert_eq!(menus(&db).await, 1);
    }
}
