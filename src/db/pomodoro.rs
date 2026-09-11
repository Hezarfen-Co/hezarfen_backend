//! The `pomodoro_session` table: the open-slot start/finish pair and the
//! newest-first log. The counting verdict, the lifetime badge counters, and
//! the study streak are all decided inside [`finish`]'s one transaction —
//! the whole rule is the batch itself.

use surrealdb::types::SurrealValue;

use crate::constant::{
    MAX_COUNTED_POMODORO_PER_DAY, MIN_COUNTED_POMODORO_MS, POMODORO_COUNTED_DAY_FIELD,
    POMODORO_COUNTED_TODAY_FIELD, POMODORO_FINISHED_TOTAL_FIELD, POMODORO_FOCUS_MS_TOTAL_FIELD,
    STUDY_STREAK_CURRENT_FIELD, STUDY_STREAK_LAST_DAY_FIELD, STUDY_STREAK_LONGEST_FIELD,
};
use crate::database::{Database, transaction_with_retry};
use crate::domain::badge;
use crate::domain::pomodoro::{PomodoroSession, PomodoroSessionId};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Start a session for `user`, stamped with the server clock. `UPSERT` on
/// the deterministic open id makes this atomic and always succeed: a
/// dangling unfinished session (the browser died mid-timer) is replaced —
/// it never counted, and blocking the next start behind it would only
/// punish the student for a crash.
pub async fn start(db: &Database, user: &UserId) -> Result<PomodoroSession, AppError> {
    let mut result = db
        .query("UPSERT $open CONTENT { user: $usr, started_at: $at }")
        .bind(("open", PomodoroSessionId::open_for(user).record()))
        .bind(("usr", user.record()))
        .bind(("at", Timestamp::now()))
        .await?
        .check()?;
    result
        .take::<Vec<PomodoroSession>>(0)?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::Internal("failed to start pomodoro session".into()))
}

/// Close `user`'s running session: atomically take the open row and
/// re-file it under a ULID id, freeing the open slot for the next start.
/// Take and re-file share one transaction — a failed re-file rolls the
/// take back, so a session can never vanish half-closed. Of two racing
/// finishes exactly one receives the row (the other gets the conflict).
///
/// A lost round is re-sent rather than reported (`transaction_with_retry`):
/// the abort wrote nothing, so the whole cascade is safe to repeat, and the
/// `CREATE` cannot answer "already exists" on the way back — the table
/// carries no `UNIQUE` index and `$closed` is one freshly minted ULID.
/// Only the guard's own `THROW` is a decision, and it stays a 409.
///
/// `finished_at` is floored at the row's own `started_at`. Both stamps come
/// from [`Timestamp::now`], i.e. the wall clock, which an NTP step can move
/// *backwards* mid-stint — so a close can legitimately read earlier than its
/// own start with nothing wrong on the client's side. Refusing it would lose
/// a real study session and leave the open slot wedged for a fault the
/// student did not cause; recording a zero-length stint keeps the session
/// count honest and its duration merely understated. Nothing downstream may
/// then read a negative duration (`ProfileStats::load` sums these).
///
/// The lifetime badge counters on the user row move in this same
/// transaction, so they can never count a stint the log does not hold (nor
/// miss one it does), and a retried round re-applies nothing — the abort
/// rolled the increment back with the rest. They are written field-scoped:
/// the row also carries admin-owned data (`role`), which a whole-row save
/// would silently revert. The clamped duration is the *same* expression the
/// `CREATE` stores (both read `$ms`), so the counter can never take a
/// negative summand and nothing downstream needs a floor. Only *finished*
/// stints reach here — an open row is a `pomodoro_session` row and nothing
/// else — and there is no student-facing delete for a stint, so neither
/// counter ever decrements.
///
/// They move only for a stint that **counts**, which is
/// [`MIN_COUNTED_POMODORO_MS`] of real focus and at most
/// [`MAX_COUNTED_POMODORO_PER_DAY`] of them per UTC day. Without that this
/// is the farm [`crate::domain::pool_question::PoolQuestion::approve`]
/// argues about from the other end: finishing is self-service, one stint is
/// two requests and no second person, so a counter moved once per
/// round-trip is farmable — two hundred pairs inside the rate limit bought
/// `pomodoro_finished_200` in ninety seconds, and a badge is never revoked.
/// Both stamps are the server's own clock, so the duration needs no
/// distrusting, only a floor.
///
/// The verdict is *stamped* on the closed row (`counted`), not re-derived
/// later: the thresholds are compiled in and will move, and a reader that
/// re-judged an old stint against today's numbers would answer something
/// other than what was credited. Nothing debits these counters today; the
/// stamp is what lets one be added without that asymmetry. A stint below
/// the bar is still recorded, still listed, and still sums into the log's
/// `total_focus_ms` — the rule bounds what *counts*, never what is kept.
///
/// The day quota is bucketed like the streak below (midnight UTC,
/// [`Timestamp::day_number`]) and rolls in its own statement ahead of the
/// verdict: that `UPDATE` reads `pomodoro_counted_day` while also setting
/// it, which resolves against the row as it was *before* the statement —
/// the same rule that forces the streak pair apart, used here on purpose. A
/// row that predates the two columns reads as no day at all (`?? -1`), so
/// the first finish after this deploy opens a fresh bucket and no backfill
/// is owed. The bucket write happens whether the stint counts or not; only
/// the `+ 1` sits behind the verdict.
///
/// The **study streak** rides the same transaction, and the same verdict: a
/// study day is a UTC calendar day on which a stint *counted*, so a day
/// bought with one instant round-trip is not a day studied (midnight UTC, like every
/// other day calculation here — no timezone is stored anywhere): the same
/// day again changes nothing, the next day extends the run, any other gap
/// starts a new one at 1. It is written as **two** statements, not one:
/// every field reference on the right-hand side of a `SET` resolves
/// against the row as it was *before* that statement, so a single
/// `SET current = …, longest = math::max([longest, current])` would take
/// the *old* `current` and lag one write behind forever (probed on a real
/// 3.2.3 server: day two left `current = 2, longest = 1`). Across
/// statements *inside a transaction* the read does see the prior write, so
/// the pair is correct and still atomic. Absent columns are read through
/// `?? 0` (and `?? -1` for the day — a sentinel no real day number
/// reaches), because `math::max` errors outright on a `NONE` argument and
/// every account older than these columns carries none of them.
///
/// The read-back is `$streak[0].…`, the same shape `$before[0]` uses two
/// lines up: a plain `UPDATE` hands back an array, so a user row that is
/// missing (a `record<user>` column checks only the table of the id) simply
/// yields nothing to index and the second statement matches no row either.
/// `UPDATE ONLY` would work too — it answers `null`, not an error, on zero
/// rows; it errors on *many* — but a `null` is a shape the read then has to
/// special-case, so the file's existing idiom wins.
///
/// No `cap::counter_lock` is taken, deliberately. The pair cannot tear:
/// both statements sit inside one `BEGIN…COMMIT`, and a real server aborts
/// a rival that touched the row in between — `transaction_with_retry` then
/// re-sends the whole round. And `longest` is written as
/// `max(longest, current)`, which cannot come down under *any*
/// interleaving, so even a torn pair could only under-count `current`,
/// never lower the high-water mark the badges read. The lock would only
/// serialize this process's writers (its other job, keeping the in-memory
/// test engine deterministic, is moot here — these tests are sequential).
/// Probed on a real 3.2.3 server, 2026-08-04: 60 rounds of 4 concurrent
/// finishes for one user left `current = 60, longest = 60` with no
/// anomaly; a rival write forced *between* the two statements (a `sleep`
/// wedged into the batch) aborted the whole cascade with a write conflict,
/// wrote nothing, and left the rival's value standing.
///
/// Streaks begin at this deploy: nothing reconstructs history from the
/// stint log, so every account starts with no streak columns and its first
/// finish sets a run of 1.
pub async fn finish(db: &Database, user: &UserId) -> Result<PomodoroSession, AppError> {
    let done = Timestamp::now();
    let (mut result, mut errors) = transaction_with_retry(
        db,
        &format!(
            "BEGIN TRANSACTION;
                 LET $before = (DELETE $open RETURN BEFORE);
                 IF array::len($before) = 0 {{ THROW 'no_pomodoro_running' }};
                 LET $end = math::max([$before[0].started_at, $done]);
                 LET $ms = $end - $before[0].started_at;
                 LET $bucket = (UPDATE $usr SET
                     {POMODORO_COUNTED_TODAY_FIELD} =
                         IF ({POMODORO_COUNTED_DAY_FIELD} ?? -1) == $day
                             THEN ({POMODORO_COUNTED_TODAY_FIELD} ?? 0) ELSE 0 END,
                     {POMODORO_COUNTED_DAY_FIELD} = $day
                     RETURN AFTER);
                 LET $counted = ($ms >= {MIN_COUNTED_POMODORO_MS})
                     AND (($bucket[0].{POMODORO_COUNTED_TODAY_FIELD} ?? 0)
                          < {MAX_COUNTED_POMODORO_PER_DAY});
                 IF $counted {{
                     UPDATE $usr SET
                         {POMODORO_FINISHED_TOTAL_FIELD} =
                             ({POMODORO_FINISHED_TOTAL_FIELD} ?? 0) + 1,
                         {POMODORO_FOCUS_MS_TOTAL_FIELD} =
                             ({POMODORO_FOCUS_MS_TOTAL_FIELD} ?? 0) + $ms,
                         {POMODORO_COUNTED_TODAY_FIELD} =
                             ({POMODORO_COUNTED_TODAY_FIELD} ?? 0) + 1;
                     LET $streak = (UPDATE $usr SET
                         {STUDY_STREAK_CURRENT_FIELD} =
                             IF ({STUDY_STREAK_LAST_DAY_FIELD} ?? -1) == $day
                                 THEN ({STUDY_STREAK_CURRENT_FIELD} ?? 0)
                             ELSE IF ({STUDY_STREAK_LAST_DAY_FIELD} ?? -1) == ($day - 1)
                                 THEN (({STUDY_STREAK_CURRENT_FIELD} ?? 0) + 1)
                             ELSE 1 END,
                         {STUDY_STREAK_LAST_DAY_FIELD} = $day
                         RETURN AFTER);
                     UPDATE $usr SET {STUDY_STREAK_LONGEST_FIELD} = math::max([
                         ({STUDY_STREAK_LONGEST_FIELD} ?? 0),
                         ($streak[0].{STUDY_STREAK_CURRENT_FIELD} ?? 0)])
                 }};
                 CREATE $closed CONTENT {{
                     user: $before[0].user,
                     started_at: $before[0].started_at,
                     finished_at: $end,
                     counted: $counted,
                 }};
                 COMMIT TRANSACTION;"
        ),
        &[
            (
                "open".into(),
                PomodoroSessionId::open_for(user).record().into_value(),
            ),
            ("usr".into(), user.record().into_value()),
            (
                "closed".into(),
                PomodoroSessionId::generate().record().into_value(),
            ),
            ("done".into(), done.into_value()),
            ("day".into(), done.day_number().into_value()),
        ],
        &["no_pomodoro_running"],
    )
    .await?;
    // An aborted transaction errors *every* slot, most with a generic
    // "not executed" — only the THROW's own slot names the reason, so scan
    // them all for the marker instead of trusting the first.
    if errors
        .values()
        .any(|error| error.to_string().contains("no_pomodoro_running"))
    {
        return Err(AppError::Conflict("no pomodoro session running"));
    }
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    // The `CREATE` is deliberately kept the last statement before `COMMIT`
    // (the counter `UPDATE` sits ahead of it — same transaction, so the
    // order is free), which is what lets its slot follow the statement
    // count instead of a hand-kept number: `num_statements` counts `BEGIN`
    // and `COMMIT` too, hence -2. See `ExamResult::record` for the bug a
    // hand-kept slot caused.
    let slot = result.num_statements().saturating_sub(2);
    let saved: Option<PomodoroSession> = result
        .take::<Vec<PomodoroSession>>(slot)?
        .into_iter()
        .next();
    let saved =
        saved.ok_or_else(|| AppError::Internal("failed to close pomodoro session".into()))?;
    // A badge is a decoration on top of the stint: losing one to a
    // transient database error must never fail the finish, and the next
    // counter move re-runs this and heals it.
    if let Err(err) = badge::sync(user, db).await {
        tracing::warn!("failed to sync badges for {}: {err}", user.key());
    }
    Ok(saved)
}

/// Every session of `user`, newest first — the running one (if any)
/// included. Ordered by `started_at`, never by id alone: the open entry's
/// `open_` key doesn't sort with the ULIDs, so id order would misplace it.
/// The `id` tie-break behind it only ever separates two *finished* stints
/// sharing a `started_at` — there is one running row per user, so it can
/// never tie with itself.
pub async fn list_for_user(db: &Database, user: &UserId) -> Result<Vec<PomodoroSession>, AppError> {
    let mut result = db
        .query("SELECT * FROM pomodoro_session WHERE user = $usr ORDER BY started_at DESC, id DESC")
        .bind(("usr", user.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<PomodoroSession>>(0)?)
}

#[cfg(test)]
mod tests {
    use ulid::Ulid;

    use super::*;
    use crate::database;

    /// A real `user` row: the counters land with `UPDATE`, which only ever
    /// touches a record that exists, so a fabricated id would silently store
    /// nothing. The tests above need no row — they read only the stint log.
    async fn a_user(db: &Database) -> UserId {
        let user = UserId::from_key(&Ulid::new().to_string());
        db.query("CREATE $usr SET username = $name, password_hash = 'x'")
            .bind(("usr", user.record()))
            .bind(("name", user.key().to_string()))
            .await
            .unwrap()
            .check()
            .unwrap();
        user
    }

    /// `(finished_total, focus_ms_total)` re-read from the store — never off
    /// what `finish` returned, which proves nothing about what was written.
    async fn counters(user: &UserId, db: &Database) -> (i64, i64) {
        let mut result = db
            .query(format!(
                "SELECT VALUE [({POMODORO_FINISHED_TOTAL_FIELD} ?? 0),
                               ({POMODORO_FOCUS_MS_TOTAL_FIELD} ?? 0)] FROM $usr"
            ))
            .bind(("usr", user.record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let rows: Vec<Vec<i64>> = result.take(0).unwrap();
        let row = rows.into_iter().next().expect("the user row");
        (row[0], row[1])
    }

    /// `(current, longest, last_day)` re-read from the store.
    async fn streak(user: &UserId, db: &Database) -> (i64, i64, i64) {
        let mut result = db
            .query(format!(
                "SELECT VALUE [({STUDY_STREAK_CURRENT_FIELD} ?? 0),
                               ({STUDY_STREAK_LONGEST_FIELD} ?? 0),
                               ({STUDY_STREAK_LAST_DAY_FIELD} ?? -1)] FROM $usr"
            ))
            .bind(("usr", user.record()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let rows: Vec<Vec<i64>> = result.take(0).unwrap();
        let row = rows.into_iter().next().expect("the user row");
        (row[0], row[1], row[2])
    }

    /// Age *both* day buckets by `days`, so the next finish lands that many
    /// days later on the calendar — the streak's and the day quota's, since
    /// they are the same calendar and a test that moved one would be asserting
    /// against a day the other has never heard of. The day is injected into the
    /// stored state rather than into the clock, which keeps `Timestamp::now`
    /// the sole clock read and the test free of sleeping.
    ///
    /// Known flake, left as-is: every caller assumes the UTC day does not turn
    /// over mid-test — a crossing between two stints shifts the real day under
    /// the injected one and reddens the run. That is one ~ms-wide window per
    /// day (~5e-6 per run), and the alternative is a clock seam in production
    /// code to serve a test. Re-run before believing a failure that only ever
    /// happens near midnight UTC.
    async fn age_by_days(user: &UserId, db: &Database, days: i64) {
        db.query(format!(
            "UPDATE $usr SET
                 {STUDY_STREAK_LAST_DAY_FIELD} = {STUDY_STREAK_LAST_DAY_FIELD} - $days,
                 {POMODORO_COUNTED_DAY_FIELD} = ({POMODORO_COUNTED_DAY_FIELD} ?? -1) - $days"
        ))
        .bind(("usr", user.record()))
        .bind(("days", days))
        .await
        .unwrap()
        .check()
        .unwrap();
    }

    /// Open a stint that has already been running for `ms` — the stamp the
    /// close measures against, injected instead of slept through.
    ///
    /// One `UPSERT`, not `start` followed by a backdate: the concurrent test
    /// races four of these onto the same open row, and a separate backdate can
    /// be wiped by a rival's fresh `start`, handing whoever wins the `DELETE` a
    /// zero-length stint and reddening the run for no defect. Same statement
    /// `start` runs, one clock read earlier.
    async fn start_aged(user: &UserId, db: &Database, ms: i64) -> Result<(), AppError> {
        db.query("UPSERT $open CONTENT { user: $usr, started_at: $at }")
            .bind(("open", PomodoroSessionId::open_for(user).record()))
            .bind(("usr", user.record()))
            .bind(("at", Timestamp::now().as_millis() - ms))
            .await?
            .check()?;
        Ok(())
    }

    /// Start and close one stint that counts: exactly the minimum on the clock,
    /// so every streak assertion below is about the day and not the duration.
    async fn one_stint(user: &UserId, db: &Database) {
        start_aged(user, db, MIN_COUNTED_POMODORO_MS).await.unwrap();
        finish(db, user).await.unwrap();
    }

    #[tokio::test]
    async fn consecutive_days_extend_the_streak_and_a_repeat_day_does_not() {
        let db = database::init_mem().await.unwrap();
        let user = a_user(&db).await;
        // The stale-data case: a row that predates all three columns reads as
        // no streak at all, and its first finish opens a run of one.
        assert_eq!(streak(&user, &db).await, (0, 0, -1));

        one_stint(&user, &db).await;
        let today = Timestamp::now().day_number();
        assert_eq!(streak(&user, &db).await, (1, 1, today));

        // A second stint the same day is not a second day.
        one_stint(&user, &db).await;
        assert_eq!(streak(&user, &db).await, (1, 1, today));

        // Days N+1 and N+2.
        for day in 2..=3 {
            age_by_days(&user, &db, 1).await;
            one_stint(&user, &db).await;
            assert_eq!(streak(&user, &db).await, (day, day, today));
        }
    }

    #[tokio::test]
    async fn a_gap_resets_the_run_but_never_the_high_water_mark() {
        let db = database::init_mem().await.unwrap();
        let user = a_user(&db).await;

        one_stint(&user, &db).await;
        age_by_days(&user, &db, 1).await;
        one_stint(&user, &db).await;
        assert_eq!(streak(&user, &db).await.0, 2);

        // Skip a day: the run restarts at one, the longest stays where it got.
        age_by_days(&user, &db, 2).await;
        one_stint(&user, &db).await;
        let (current, longest, _) = streak(&user, &db).await;
        assert_eq!((current, longest), (1, 2));

        // A backwards day (a clock stepped back over a boundary) is a gap too,
        // and still cannot lower the mark.
        age_by_days(&user, &db, -5).await;
        one_stint(&user, &db).await;
        assert_eq!(
            streak(&user, &db).await,
            (1, 2, Timestamp::now().day_number())
        );
    }

    /// The transaction boundary itself, on a real server: the streak pair and
    /// the counters must commit with the stint or not at all, under real
    /// contention on one user row.
    ///
    /// Multi-threaded and off the embedded engine for the reason
    /// [`crate::db::cap`] spells out: the in-memory engine does not
    /// conflict-check two concurrent writes to one record, so it would answer
    /// `Ok` to a write it dropped and *forge* the integrity failure this test
    /// exists to catch.
    ///
    /// It bites — proved by mutation, not assumed: deleting the `BEGIN`/`COMMIT`
    /// pair from the batch turns all 20 rounds red. A `THROW` then errors only
    /// its own statement, so the losers of the open-row race run the rest of
    /// the cascade anyway — they bump the counters for a stint they never
    /// closed and `CREATE` a session row out of an empty `$before`, which comes
    /// back as `500 … Expected record, got none` (the `errors` assertion below
    /// fires first; `finished_total` running ahead of the `Ok` finishes is the
    /// same wound one statement later). What it does *not* pin is the
    /// streak arithmetic under concurrency: same-day finishes all compute the
    /// same value, so that stays green either way — it is
    /// `max(longest, current)` that makes that robust, not the isolation.
    ///
    /// The shape of the contention, measured rather than assumed: four racers
    /// per round is four `start`s collapsing onto *one* open row (the id is
    /// deterministic per user), so typically exactly one `finish` commits and
    /// three lose the `DELETE` and abort their whole cascade — 20 finishes and
    /// 60 conflicts over 20 rounds, measured. That is the contention: the
    /// losers' cascades are live against the same user row, which is precisely
    /// what must leave nothing behind. Two commits in one round are possible
    /// (a racer scheduled entirely after another's finish frees the slot) and
    /// the assertions allow it — the day count is per round, not per stint.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs a real SurrealDB server: podman start hezarfen-surrealdb && cargo test -- --ignored"]
    async fn concurrent_finishes_commit_the_streak_with_the_stint_or_not_at_all() {
        let (db, _serialized) = crate::database::init_test_server("pomodoro_streak_race").await;
        let user = a_user(&db).await;
        let (mut finished, mut conflicts, mut errors) = (0, 0, 0);
        let mut last_error = String::new();
        for round in 1..=20 {
            let racers: Vec<_> = (0..4)
                .map(|_| {
                    let (user, db) = (user.clone(), db.clone());
                    tokio::spawn(async move {
                        // Aged past the minimum, so whichever racer wins the
                        // open row closes a stint that *counts* — a fresh
                        // `start` here would hand the winner a zero-length
                        // stint, which now moves nothing and would leave this
                        // test asserting against an untouched counter.
                        start_aged(&user, &db, MIN_COUNTED_POMODORO_MS).await?;
                        finish(&db, &user).await
                    })
                })
                .collect();
            let mut won = 0;
            for racer in racers {
                match racer.await.unwrap() {
                    Ok(_) => won += 1,
                    // Losing the open row is the correct answer for a racer
                    // whose stint someone else closed; only `Db` is a defect.
                    Err(AppError::Conflict(_)) => conflicts += 1,
                    Err(err) => {
                        errors += 1;
                        last_error = format!("{err:?}");
                    }
                }
            }
            finished += won;
            // One day per round, however many stints landed in it, and the
            // mark never lags the run in progress.
            let (current, longest, _) = streak(&user, &db).await;
            assert_eq!(
                (current, longest),
                (round, round),
                "round {round} counted {won} finishes as {current} days"
            );
            age_by_days(&user, &db, 1).await;
        }
        // The atomicity claim: the counter counts exactly the stints that were
        // closed, never a loser's aborted round.
        let (counted, _) = counters(&user, &db).await;
        eprintln!(
            "PomodoroSession::finish raced: {finished} finishes over 20 days, \
             {conflicts} rivals refused, {errors} 500s"
        );
        assert_eq!(
            errors, 0,
            "a raced finish must conflict, not 500: {last_error}"
        );
        assert_eq!(
            counted, finished,
            "the counter counted {counted} stints for {finished} finishes"
        );
        assert!(
            conflicts > 0,
            "every racer won its own round, so no cascade was ever aborted mid-flight \
             and this run proves nothing about the transaction boundary"
        );
    }

    #[tokio::test]
    async fn finishing_bumps_the_stored_counters_and_an_open_stint_bumps_neither() {
        let db = database::init_mem().await.unwrap();
        let user = a_user(&db).await;
        assert_eq!(counters(&user, &db).await, (0, 0));

        // An open stint is not a finished one: neither counter moves until it
        // closes.
        start_aged(&user, &db, MIN_COUNTED_POMODORO_MS)
            .await
            .unwrap();
        assert_eq!(counters(&user, &db).await, (0, 0));

        let mut focus_ms = 0;
        for _ in 0..2 {
            let closed = finish(&db, &user).await.unwrap();
            focus_ms +=
                closed.get_finished_at().unwrap().as_millis() - closed.get_started_at().as_millis();
            start_aged(&user, &db, MIN_COUNTED_POMODORO_MS)
                .await
                .unwrap();
        }
        // Three starts, two finishes — the third is still running.
        assert_eq!(counters(&user, &db).await, (2, focus_ms));
    }

    #[tokio::test]
    async fn a_backwards_clock_adds_zero_ms_to_the_counter_never_a_negative() {
        let db = database::init_mem().await.unwrap();
        let user = a_user(&db).await;

        start(&db, &user).await.unwrap();
        db.query("UPDATE $open SET started_at = $future")
            .bind(("open", PomodoroSessionId::open_for(&user).record()))
            .bind(("future", Timestamp::now().as_millis() + 3_600_000))
            .await
            .unwrap()
            .check()
            .unwrap();

        // The clamp holds: zero milliseconds, never a negative — and a stint
        // measuring zero is below the minimum, so it moves neither counter.
        let closed = finish(&db, &user).await.unwrap();
        assert_eq!(closed.get_counted(), Some(false));
        assert_eq!(counters(&user, &db).await, (0, 0));
    }

    /// The minimum, from both sides. A round-trip is not a study session, and
    /// the boundary is inclusive (`>=`) — a stint of exactly the minimum
    /// counts, which is the side these assertions pin.
    ///
    /// The near miss is a whole minute short, not one millisecond: `finish`
    /// reads its own clock *after* the stint is opened, so a stint aged to
    /// exactly one millisecond under the bar crosses it by the time the close
    /// measures it (measured — that assertion failed on the first run). A
    /// millisecond-exact probe would need a clock seam in production code, and
    /// what matters here is the four orders of magnitude between a scripted
    /// pair and a study session, not the last millisecond.
    #[tokio::test]
    async fn a_stint_under_the_minimum_is_logged_and_listed_but_counts_for_nothing() {
        let db = database::init_mem().await.unwrap();
        let user = a_user(&db).await;

        start_aged(&user, &db, MIN_COUNTED_POMODORO_MS - 60_000)
            .await
            .unwrap();
        let short = finish(&db, &user).await.unwrap();
        assert_eq!(short.get_counted(), Some(false));
        assert_eq!(counters(&user, &db).await, (0, 0));
        // Not a study day either — and no streak column was even opened.
        assert_eq!(streak(&user, &db).await, (0, 0, -1));
        // But it happened, and the log says so.
        let log = list_for_user(&db, &user).await.unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].get_finished_at(), short.get_finished_at());

        start_aged(&user, &db, MIN_COUNTED_POMODORO_MS)
            .await
            .unwrap();
        let counted = finish(&db, &user).await.unwrap();
        assert_eq!(counted.get_counted(), Some(true));
        let (finished, focus_ms) = counters(&user, &db).await;
        // The counter took the *same* duration the row stores, not a second
        // reading of the clock.
        assert_eq!(
            (finished, focus_ms),
            (
                1,
                counted.get_finished_at().unwrap().as_millis()
                    - counted.get_started_at().as_millis()
            )
        );
        assert_eq!(streak(&user, &db).await.0, 1);
        assert_eq!(list_for_user(&db, &user).await.unwrap().len(), 2);
    }

    /// The day quota: it stops the counter, it never stops the log, and it
    /// rolls at midnight UTC.
    #[tokio::test]
    async fn the_day_quota_caps_the_counter_and_rolls_at_midnight_utc() {
        let db = database::init_mem().await.unwrap();
        let user = a_user(&db).await;

        for _ in 0..MAX_COUNTED_POMODORO_PER_DAY {
            one_stint(&user, &db).await;
        }
        let (finished, focus_ms) = counters(&user, &db).await;
        assert_eq!(finished, MAX_COUNTED_POMODORO_PER_DAY);

        // Past the quota: recorded, listed, worth nothing — neither stint count
        // nor focus milliseconds.
        for _ in 0..3 {
            start_aged(&user, &db, MIN_COUNTED_POMODORO_MS)
                .await
                .unwrap();
            let over = finish(&db, &user).await.unwrap();
            assert_eq!(over.get_counted(), Some(false));
        }
        assert_eq!(counters(&user, &db).await, (finished, focus_ms));
        assert_eq!(
            list_for_user(&db, &user).await.unwrap().len() as i64,
            MAX_COUNTED_POMODORO_PER_DAY + 3
        );
        // The quota is a day's, not a lifetime's.
        age_by_days(&user, &db, 1).await;
        one_stint(&user, &db).await;
        assert_eq!(
            counters(&user, &db).await.0,
            MAX_COUNTED_POMODORO_PER_DAY + 1
        );
    }

    /// The farm, run: `start`/`finish` in a loop is two requests a round with
    /// no second person in it, which used to move `pomodoro_finished_total` one
    /// per round — 200 of them minted `pomodoro_finished_10`, `_50` and `_200`,
    /// and a badge is never taken back.
    #[tokio::test]
    async fn the_start_finish_farm_moves_no_counter_and_mints_no_badge() {
        let db = database::init_mem().await.unwrap();
        let user = a_user(&db).await;

        for _ in 0..200 {
            start(&db, &user).await.unwrap();
            finish(&db, &user).await.unwrap();
        }
        assert_eq!(counters(&user, &db).await, (0, 0));
        assert_eq!(streak(&user, &db).await, (0, 0, -1));
        // Every one of them is still the student's own history.
        assert_eq!(list_for_user(&db, &user).await.unwrap().len(), 200);
        assert!(
            badge::BadgeAward::list_for(&user, &db)
                .await
                .unwrap()
                .is_empty(),
            "the farm minted a permanent badge"
        );

        // And paying the minimum for each one only buys the day's quota.
        for _ in 0..(MAX_COUNTED_POMODORO_PER_DAY * 2) {
            one_stint(&user, &db).await;
        }
        assert_eq!(counters(&user, &db).await.0, MAX_COUNTED_POMODORO_PER_DAY);
    }

    #[tokio::test]
    async fn restart_replaces_the_open_session_and_finish_closes_it() {
        let db = database::init_mem().await.unwrap();
        // A `record<user>` column checks the table of the id, not row
        // existence — a fabricated id keeps this test free of user ceremony.
        let user = UserId::from_key(&Ulid::new().to_string());

        // Nothing running yet — finishing conflicts.
        assert!(matches!(
            finish(&db, &user).await,
            Err(AppError::Conflict(_))
        ));

        let first = start(&db, &user).await.unwrap();
        assert!(first.get_finished_at().is_none());
        assert_eq!(first.get_id().key(), format!("open_{}", user.key()));

        // A restart replaces the dangling session: still one row, fresh clock.
        let second = start(&db, &user).await.unwrap();
        assert!(second.get_started_at() >= first.get_started_at());
        let sessions = list_for_user(&db, &user).await.unwrap();
        assert_eq!(sessions.len(), 1);

        let closed = finish(&db, &user).await.unwrap();
        assert!(closed.get_finished_at().unwrap() >= closed.get_started_at());

        // The open slot is free again; a stray second finish conflicts.
        assert!(matches!(
            finish(&db, &user).await,
            Err(AppError::Conflict(_))
        ));
        start(&db, &user).await.unwrap();
        let sessions = list_for_user(&db, &user).await.unwrap();
        assert_eq!(sessions.len(), 2);
    }

    #[tokio::test]
    async fn a_backwards_clock_records_a_zero_stint_not_a_negative_one() {
        let db = database::init_mem().await.unwrap();
        let user = UserId::from_key(&Ulid::new().to_string());

        start(&db, &user).await.unwrap();
        // Stand in for the NTP step: push the running stint's start an hour
        // ahead, so the server clock `finish` reads is *behind* it.
        db.query("UPDATE $open SET started_at = $future")
            .bind(("open", PomodoroSessionId::open_for(&user).record()))
            .bind(("future", Timestamp::now().as_millis() + 3_600_000))
            .await
            .unwrap()
            .check()
            .unwrap();

        let closed = finish(&db, &user).await.unwrap();
        let started = closed.get_started_at().as_millis();
        assert_eq!(closed.get_finished_at().unwrap().as_millis(), started);

        // And the stat that sums these stays non-negative.
        let stats = crate::domain::profile::ProfileStats::load(&user, 0, 0, &db)
            .await
            .unwrap();
        assert_eq!(stats.get_pomodoro_sessions(), 1);
        assert_eq!(stats.get_pomodoro_focus_ms(), 0);
    }
}
