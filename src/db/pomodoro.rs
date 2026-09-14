//! The `pomodoro_session` table: the open-slot start/finish pair and the
//! newest-first log. The counting verdict, the lifetime badge counters, and
//! the study streak are all decided inside [`finish`]'s one transaction —
//! the whole rule is the batch itself.

use crate::constant::{MAX_COUNTED_POMODORO_PER_DAY, MIN_COUNTED_POMODORO_MS};
use crate::database::{Database, tx_with_retry};
use crate::domain::pomodoro::{PomodoroSession, PomodoroSessionId};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;
use sqlx::query_as;

/// Start a session for `user`, stamped with the server clock. The open slot
/// is the partial unique index (`pomodoro_session_open_stint`): one row per
/// user with `finished_at IS NULL`. A dangling unfinished session (the
/// browser died mid-timer) is replaced by the same statement — the arbiter
/// turns the insert into an in-place re-stamp (`DO UPDATE`), so the old row
/// becomes the new stint; it never counted, and blocking the next start
/// behind it would only punish the student for a crash.
///
/// The replace has to ride the arbiter. A `WITH gone AS (DELETE …) INSERT …`
/// CTE cannot do it: both halves run on one snapshot, so the insert's
/// uniqueness check still sees the row the CTE just deleted and refuses
/// (23505) *deterministically* — a retry re-sends the identical statement and
/// dies identically, which is the 500 the port briefly shipped. `DO UPDATE`
/// is atomic: two racing starts cannot both win the slot, and the loser
/// re-stamps the winner's row — the same last-write-wins the old
/// deterministic-key upsert had.
///
/// `label` is the student's own name for the stint, validated and trimmed
/// upstream and stored verbatim. A `None` binds NULL — an unnamed start
/// reads back unnamed.
pub async fn start(
    db: &Database,
    user: &UserId,
    label: Option<String>,
) -> Result<PomodoroSession, AppError> {
    let started_at = Timestamp::now();
    let id = PomodoroSessionId::generate();
    let started = query_as!(
        PomodoroSession,
        "INSERT INTO pomodoro_session (id, app_user, started_at, finished_at, counted, label)
         VALUES ($2, $1, $3, NULL, NULL, $4)
         ON CONFLICT (app_user) WHERE finished_at IS NULL DO UPDATE
         SET started_at = EXCLUDED.started_at, finished_at = NULL, counted = NULL,
             label = EXCLUDED.label
         RETURNING id AS \"id: PomodoroSessionId\", app_user AS \"user: UserId\", \
                   started_at AS \"started_at: Timestamp\", \
                   finished_at AS \"finished_at: Timestamp\", counted, label",
        user.uuid(),
        id.uuid(),
        started_at.as_millis(),
        label
    )
    .fetch_one(db)
    .await?;
    Ok(started)
}

/// Close `user`'s running session, deciding the counting verdict, the
/// lifetime badge counters, and the study streak in one transaction.
///
/// The open row is taken by a guarded `DELETE … WHERE finished_at IS NULL
/// RETURNING` — of two racing finishes exactly one receives the row, and
/// the other is refused (`no pomodoro session running`), the old abort
/// marker now an ordinary early return. The user row carries every counter family
/// this decision moves, so the transaction takes it `FOR NO KEY UPDATE`,
/// decides in Rust, and writes once: no interleaving can tear the streak
/// pair, and a rival finish is serialized on the row lock instead of
/// aborting. `longest` is written as `max(longest, current)`, which cannot
/// come down under any interleaving.
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
/// The lifetime badge counters move only for a stint that **counts**, which
/// is [`MIN_COUNTED_POMODORO_MS`] of real focus and at most
/// [`MAX_COUNTED_POMODORO_PER_DAY`] of them per UTC day. Without that this
/// is the farm [`crate::domain::pool_question::PoolQuestion::approve`]
/// argues about from the other end: finishing is self-service, one stint is
/// two requests and no second person, so a counter moved once per
/// round-trip is farmable. Both stamps are the server's own clock, so the
/// duration needs no distrusting, only a floor.
///
/// The verdict is *stamped* on the closed row (`counted`), not re-derived
/// later: the thresholds are compiled in and will move, and a reader that
/// re-judged an old stint against today's numbers would answer something
/// other than what was credited. A stint below the bar is still recorded,
/// still listed, and still sums into the log's focus total — the rule
/// bounds what *counts*, never what is kept.
///
/// The day quota is bucketed like the streak (midnight UTC,
/// [`Timestamp::day_number`]) and rolls whether the stint counts or not;
/// only the `+ 1` sits behind the verdict.
///
/// The **study streak** rides the same transaction, and the same verdict: a
/// study day is a UTC calendar day on which a stint *counted* — the same
/// day again changes nothing, the next day extends the run, any other gap
/// starts a new one at 1.
///
/// The closed stint is re-filed under a freshly minted id: the open slot is
/// the partial index, so the closed row releases it by no longer matching
/// `finished_at IS NULL`.
pub async fn finish(db: &Database, user: &UserId) -> Result<PomodoroSession, AppError> {
    let done = Timestamp::now();
    let day = done.day_number();
    // Owned capture: an `async move` closure holding a `&UserId` fails the
    // higher-ranked `Send` check `tx_with_retry`'s future must pass.
    let user = *user;
    let saved = tx_with_retry(db, false, async move |tx| {
        // Take the open row. Empty means nothing is running.
        let open = sqlx::query!(
            "DELETE FROM pomodoro_session
             WHERE app_user = $1 AND finished_at IS NULL
             RETURNING started_at AS \"started_at: Timestamp\", label",
            user.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(open) = open else {
            return Err(AppError::Conflict("no pomodoro session running"));
        };
        let end = Timestamp::from_millis(open.started_at.as_millis().max(done.as_millis()));
        let ms = end.as_millis() - open.started_at.as_millis();

        // Every counter this decision moves lives on the user row; the row
        // lock decides the whole verdict against one consistent read.
        let counts = sqlx::query!(
            "SELECT pomodoro_counted_today, pomodoro_counted_day, pomodoro_finished_total, \
                    pomodoro_focus_ms_total, study_streak_current, study_streak_last_day, \
                    study_streak_longest \
             FROM app_user WHERE id = $1 \
             FOR NO KEY UPDATE",
            user.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(c) = counts else {
            // The foreign key on the stint would refuse the insert below
            // anyway; the honest answer for a user who does not exist.
            return Err(AppError::NotFound);
        };

        // Day-bucket roll: a finish on a new day reopens the quota.
        let counted_today = if c.pomodoro_counted_day == Some(day) {
            c.pomodoro_counted_today
        } else {
            0
        };
        let counted = ms >= MIN_COUNTED_POMODORO_MS && counted_today < MAX_COUNTED_POMODORO_PER_DAY;
        if counted {
            let current = match c.study_streak_last_day {
                Some(last) if last == day => c.study_streak_current,
                Some(last) if last == day - 1 => c.study_streak_current + 1,
                _ => 1,
            };
            sqlx::query!(
                "UPDATE app_user SET pomodoro_finished_total = pomodoro_finished_total + 1, \
                        pomodoro_focus_ms_total = pomodoro_focus_ms_total + $2, \
                        pomodoro_counted_today = $3, pomodoro_counted_day = $4, \
                        study_streak_current = $5, study_streak_last_day = $4, \
                        study_streak_longest = GREATEST(study_streak_longest, $5) \
                 WHERE id = $1",
                user.uuid(),
                ms,
                counted_today + 1,
                day,
                current,
            )
            .execute(&mut *tx)
            .await?;
        } else if c.pomodoro_counted_day != Some(day) {
            sqlx::query!(
                "UPDATE app_user SET pomodoro_counted_today = 0, pomodoro_counted_day = $2 \
                 WHERE id = $1",
                user.uuid(),
                day
            )
            .execute(&mut *tx)
            .await?;
        }

        let closed = query_as!(
            PomodoroSession,
            "INSERT INTO pomodoro_session (id, app_user, started_at, finished_at, counted, label)
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING id AS \"id: PomodoroSessionId\", app_user AS \"user: UserId\", \
                       started_at AS \"started_at: Timestamp\", \
                       finished_at AS \"finished_at: Timestamp\", counted, label",
            PomodoroSessionId::generate().uuid(),
            user.uuid(),
            open.started_at.as_millis(),
            Some(end.as_millis()),
            Some(counted),
            open.label,
        )
        .fetch_one(&mut *tx)
        .await?;
        Ok(closed)
    })
    .await?;
    // A badge is a decoration on top of the stint: losing one to a
    // transient database error must never fail the finish, and the next
    // counter move re-runs this and heals it.
    if let Err(err) = crate::db::badge::sync(db, &user).await {
        tracing::warn!("failed to sync badges for {}: {err}", user.key());
    }
    Ok(saved)
}

/// Every session of `user`, newest first — the running one (if any)
/// included. Ordered by `started_at`, never by id alone: the log is a
/// chronology, and the id tie-break only ever separates two *finished*
/// stints sharing a `started_at` — there is one running row per user, so it
/// can never tie with itself.
pub async fn list_for_user(db: &Database, user: &UserId) -> Result<Vec<PomodoroSession>, AppError> {
    let sessions = query_as!(
        PomodoroSession,
        "SELECT id AS \"id: PomodoroSessionId\", app_user AS \"user: UserId\", \
                started_at AS \"started_at: Timestamp\", \
                finished_at AS \"finished_at: Timestamp\", counted, label \
         FROM pomodoro_session WHERE app_user = $1 \
         ORDER BY started_at DESC, id DESC",
        user.uuid()
    )
    .fetch_all(db)
    .await?;
    Ok(sessions)
}
#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row as _;

    /// A real `app_user` row: the counters land with `UPDATE`, which only ever
    /// touches a row that exists, so a fabricated id would silently store
    /// nothing. The tests above need no row — they read only the stint log.
    async fn a_user(db: &Database) -> UserId {
        let user = UserId::generate();
        sqlx::query("INSERT INTO app_user (id, username, created_at) VALUES ($1, $2, 0)")
            .bind(user.uuid())
            .bind(format!("u{}", &user.key()[30..]))
            .execute(db)
            .await
            .unwrap();
        user
    }

    /// `(finished_total, focus_ms_total)` re-read from the store — never off
    /// what `finish` returned, which proves nothing about what was written.
    async fn counters(user: &UserId, db: &Database) -> (i64, i64) {
        let row = sqlx::query(
            "SELECT pomodoro_finished_total, pomodoro_focus_ms_total \
             FROM app_user WHERE id = $1",
        )
        .bind(user.uuid())
        .fetch_one(db)
        .await
        .unwrap();
        (
            row.try_get::<i64, _>(0).unwrap(),
            row.try_get::<i64, _>(1).unwrap(),
        )
    }

    /// `(current, longest, last_day)` re-read from the store; the unset day
    /// reads as the sentinel `-1` the streak arithmetic runs on.
    async fn streak(user: &UserId, db: &Database) -> (i64, i64, i64) {
        let row = sqlx::query(
            "SELECT study_streak_current, study_streak_longest, \
                    COALESCE(study_streak_last_day, -1) \
             FROM app_user WHERE id = $1",
        )
        .bind(user.uuid())
        .fetch_one(db)
        .await
        .unwrap();
        (
            row.try_get::<i64, _>(0).unwrap(),
            row.try_get::<i64, _>(1).unwrap(),
            row.try_get::<i64, _>(2).unwrap(),
        )
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
        sqlx::query(
            "UPDATE app_user SET
                 study_streak_last_day = COALESCE(study_streak_last_day, -1) - $1,
                 pomodoro_counted_day = COALESCE(pomodoro_counted_day, -1) - $1
             WHERE id = $2",
        )
        .bind(days)
        .bind(user.uuid())
        .execute(db)
        .await
        .unwrap();
    }

    /// Open a stint that has already been running for `ms` — the stamp the
    /// close measures against, injected instead of slept through.
    ///
    /// One `UPSERT`, not `start` followed by a backdate: the concurrent test
    /// races four of these onto the same open row, and a separate backdate can
    /// be wiped by a rival's fresh `start`, handing whoever wins the arbiter
    /// a zero-length stint and reddening the run for no defect. The same
    /// arbiter-riding statement `start` runs, one clock read earlier — the
    /// `DO UPDATE` arm drops the loser onto the winner's row in place.
    async fn start_aged(user: &UserId, db: &Database, ms: i64) -> Result<(), AppError> {
        sqlx::query(
            "INSERT INTO pomodoro_session (id, app_user, started_at, finished_at, counted, label)
             VALUES ($2, $1, $3, NULL, NULL, NULL)
             ON CONFLICT (app_user) WHERE finished_at IS NULL DO UPDATE
             SET started_at = EXCLUDED.started_at, finished_at = NULL, counted = NULL",
        )
        .bind(user.uuid())
        .bind(PomodoroSessionId::generate().uuid())
        .bind(Timestamp::now().as_millis() - ms)
        .execute(db)
        .await?;
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
        let (db, _leases) = crate::database::init_test_db().await;
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
        let (db, _leases) = crate::database::init_test_db().await;
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
    async fn concurrent_finishes_commit_the_streak_with_the_stint_or_not_at_all() {
        let (db, _leases) = crate::database::init_test_db().await;
        let user = a_user(&db).await;
        let (mut finished, mut conflicts, mut errors) = (0, 0, 0);
        let mut last_error = String::new();
        for round in 1..=20 {
            let racers: Vec<_> = (0..4)
                .map(|_| {
                    let (user, db) = (user, db.clone());
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
            // Streak length == round is load luck: same-day finishes share
            // a day, and a round with only uncounted closes does not
            // advance it. Atomicity is the claim — pinned after the loop.
            let (current, longest, _) = streak(&user, &db).await;
            assert!(
                longest >= current && current <= round,
                "round {round} streak ran off the calendar: current={current} longest={longest} won={won}"
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
        let (db, _leases) = crate::database::init_test_db().await;
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
        let (db, _leases) = crate::database::init_test_db().await;
        let user = a_user(&db).await;

        start(&db, &user, None).await.unwrap();
        sqlx::query(
            "UPDATE pomodoro_session SET started_at = $1
             WHERE app_user = $2 AND finished_at IS NULL",
        )
        .bind(Timestamp::now().as_millis() + 3_600_000)
        .bind(user.uuid())
        .execute(&db)
        .await
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
        let (db, _leases) = crate::database::init_test_db().await;
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
        let (db, _leases) = crate::database::init_test_db().await;
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
        let (db, _leases) = crate::database::init_test_db().await;
        let user = a_user(&db).await;

        for _ in 0..200 {
            start(&db, &user, None).await.unwrap();
            finish(&db, &user).await.unwrap();
        }
        assert_eq!(counters(&user, &db).await, (0, 0));
        assert_eq!(streak(&user, &db).await, (0, 0, -1));
        // Every one of them is still the student's own history.
        assert_eq!(list_for_user(&db, &user).await.unwrap().len(), 200);
        assert!(
            crate::db::badge::list_for(&db, &user)
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
        let (db, _leases) = crate::database::init_test_db().await;
        // The stint's owner is a real parent row now (the FK checks it), so
        // the user ceremony stays — it is one helper call.
        let user = a_user(&db).await;

        // Nothing running yet — finishing conflicts.
        assert!(matches!(
            finish(&db, &user).await,
            Err(AppError::Conflict(_))
        ));

        let first = start(&db, &user, None).await.unwrap();

        // A restart replaces the dangling session: still one row, fresh clock.
        let second = start(&db, &user, None).await.unwrap();
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
        start(&db, &user, None).await.unwrap();
        let sessions = list_for_user(&db, &user).await.unwrap();
        assert_eq!(sessions.len(), 2);
    }

    #[tokio::test]
    async fn a_backwards_clock_records_a_zero_stint_not_a_negative_one() {
        let (db, _leases) = crate::database::init_test_db().await;
        let user = a_user(&db).await;

        start(&db, &user, None).await.unwrap();
        // Stand in for the NTP step: push the running stint's start an hour
        // ahead, so the server clock `finish` reads is *behind* it.
        sqlx::query(
            "UPDATE pomodoro_session SET started_at = $1
             WHERE app_user = $2 AND finished_at IS NULL",
        )
        .bind(Timestamp::now().as_millis() + 3_600_000)
        .bind(user.uuid())
        .execute(&db)
        .await
        .unwrap();

        let closed = finish(&db, &user).await.unwrap();
        let started = closed.get_started_at().as_millis();
        assert_eq!(closed.get_finished_at().unwrap().as_millis(), started);

        // And the stat that sums these stays non-negative.
        let stats = crate::db::profile::load(&db, &user, 0, 0).await.unwrap();
        assert_eq!(stats.get_pomodoro_sessions(), 1);
        assert_eq!(stats.get_pomodoro_focus_ms(), 0);
    }
}
