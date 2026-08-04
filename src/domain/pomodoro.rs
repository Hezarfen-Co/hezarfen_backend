use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{
    POMODORO_FINISHED_TOTAL_FIELD, POMODORO_FOCUS_MS_TOTAL_FIELD, POMODORO_SESSION_TABLE,
};
use crate::database::{Database, transaction_with_retry};
use crate::domain::badge;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct PomodoroSessionId(RecordId);

impl PomodoroSessionId {
    /// Minted from the process-wide monotonic generator, not `Ulid::new()`:
    /// the log sorts `started_at DESC, id DESC` and the id breaks the tie between
    /// two sessions started at the same instant. The `open_` key below never ties
    /// with itself (one running session per user), so it needs no ordering.
    pub fn generate() -> Self {
        Self(RecordId::new(
            POMODORO_SESSION_TABLE,
            next_ulid().to_string(),
        ))
    }

    /// The deterministic id of `user`'s *running* session. At most one runs
    /// per user by construction: starting is a single `UPSERT` on this id
    /// (atomic — a restart replaces the row in place), and finishing
    /// atomically takes the row and re-files it under a ULID id. `open_`
    /// cannot collide with a ULID key (ULIDs are bare alphanumerics).
    pub fn open_for(user: &UserId) -> Self {
        Self(RecordId::new(
            POMODORO_SESSION_TABLE,
            format!("open_{}", user.key()),
        ))
    }

    pub fn record(&self) -> RecordId {
        self.0.clone()
    }

    pub fn key(&self) -> &str {
        match &self.0.key {
            RecordIdKey::String(key) => key,
            _ => "",
        }
    }
}

/// One pomodoro focus session of a student: server-stamped `started_at`, and
/// `finished_at` once closed. The wall clock is read server-side only — a
/// client can never supply its own instants, so the recorded focus time is
/// honest. Breaks are not stored; the frontend owns the work/break rhythm and
/// the backend records only the focus stint.
#[derive(Debug, Clone, SurrealValue)]
pub struct PomodoroSession {
    id: PomodoroSessionId,
    user: UserId,
    started_at: Timestamp,
    finished_at: Option<Timestamp>,
}

impl PomodoroSession {
    pub fn get_id(&self) -> &PomodoroSessionId {
        &self.id
    }

    pub fn get_user(&self) -> &UserId {
        &self.user
    }

    pub fn get_started_at(&self) -> Timestamp {
        self.started_at
    }

    pub fn get_finished_at(&self) -> Option<Timestamp> {
        self.finished_at
    }

    /// Start a session for `user`, stamped with the server clock. `UPSERT` on
    /// the deterministic open id makes this atomic and always succeed: a
    /// dangling unfinished session (the browser died mid-timer) is replaced —
    /// it never counted, and blocking the next start behind it would only
    /// punish the student for a crash.
    pub async fn start(user: &UserId, db: &Database) -> Result<PomodoroSession, AppError> {
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
    /// `CREATE` stores, so the counter can never take a negative summand and
    /// nothing downstream needs a floor. Only *finished* stints reach here — an
    /// open row is a `pomodoro_session` row and nothing else — and there is no
    /// student-facing delete for a stint, so neither counter ever decrements.
    pub async fn finish(user: &UserId, db: &Database) -> Result<PomodoroSession, AppError> {
        let (mut result, mut errors) = transaction_with_retry(
            db,
            &format!(
                "BEGIN TRANSACTION;
                 LET $before = (DELETE $open RETURN BEFORE);
                 IF array::len($before) = 0 {{ THROW 'no_pomodoro_running' }};
                 UPDATE $usr SET
                     {POMODORO_FINISHED_TOTAL_FIELD} = ({POMODORO_FINISHED_TOTAL_FIELD} ?? 0) + 1,
                     {POMODORO_FOCUS_MS_TOTAL_FIELD} = ({POMODORO_FOCUS_MS_TOTAL_FIELD} ?? 0)
                         + (math::max([$before[0].started_at, $done]) - $before[0].started_at);
                 CREATE $closed CONTENT {{
                     user: $before[0].user,
                     started_at: $before[0].started_at,
                     finished_at: math::max([$before[0].started_at, $done]),
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
                ("done".into(), Timestamp::now().into_value()),
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
    pub async fn list_for_user(
        user: &UserId,
        db: &Database,
    ) -> Result<Vec<PomodoroSession>, AppError> {
        let mut result = db
            .query("SELECT * FROM pomodoro_session WHERE user = $usr ORDER BY started_at DESC, id DESC")
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<PomodoroSession>>(0)?)
    }
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

    #[tokio::test]
    async fn finishing_bumps_the_stored_counters_and_an_open_stint_bumps_neither() {
        let db = database::init_mem().await.unwrap();
        let user = a_user(&db).await;
        assert_eq!(counters(&user, &db).await, (0, 0));

        // An open stint is not a finished one: neither counter moves until it
        // closes.
        PomodoroSession::start(&user, &db).await.unwrap();
        assert_eq!(counters(&user, &db).await, (0, 0));

        let mut focus_ms = 0;
        for _ in 0..2 {
            let closed = PomodoroSession::finish(&user, &db).await.unwrap();
            focus_ms +=
                closed.get_finished_at().unwrap().as_millis() - closed.get_started_at().as_millis();
            PomodoroSession::start(&user, &db).await.unwrap();
        }
        // Three starts, two finishes — the third is still running.
        assert_eq!(counters(&user, &db).await, (2, focus_ms));
    }

    #[tokio::test]
    async fn a_backwards_clock_adds_zero_ms_to_the_counter_never_a_negative() {
        let db = database::init_mem().await.unwrap();
        let user = a_user(&db).await;

        PomodoroSession::start(&user, &db).await.unwrap();
        db.query("UPDATE $open SET started_at = $future")
            .bind(("open", PomodoroSessionId::open_for(&user).record()))
            .bind(("future", Timestamp::now().as_millis() + 3_600_000))
            .await
            .unwrap()
            .check()
            .unwrap();

        PomodoroSession::finish(&user, &db).await.unwrap();
        assert_eq!(counters(&user, &db).await, (1, 0));
    }

    #[tokio::test]
    async fn restart_replaces_the_open_session_and_finish_closes_it() {
        let db = database::init_mem().await.unwrap();
        // A `record<user>` column checks the table of the id, not row
        // existence — a fabricated id keeps this test free of user ceremony.
        let user = UserId::from_key(&Ulid::new().to_string());

        // Nothing running yet — finishing conflicts.
        assert!(matches!(
            PomodoroSession::finish(&user, &db).await,
            Err(AppError::Conflict(_))
        ));

        let first = PomodoroSession::start(&user, &db).await.unwrap();
        assert!(first.get_finished_at().is_none());
        assert_eq!(first.get_id().key(), format!("open_{}", user.key()));

        // A restart replaces the dangling session: still one row, fresh clock.
        let second = PomodoroSession::start(&user, &db).await.unwrap();
        assert!(second.get_started_at() >= first.get_started_at());
        let sessions = PomodoroSession::list_for_user(&user, &db).await.unwrap();
        assert_eq!(sessions.len(), 1);

        let closed = PomodoroSession::finish(&user, &db).await.unwrap();
        assert!(closed.get_finished_at().unwrap() >= closed.get_started_at());

        // The open slot is free again; a stray second finish conflicts.
        assert!(matches!(
            PomodoroSession::finish(&user, &db).await,
            Err(AppError::Conflict(_))
        ));
        PomodoroSession::start(&user, &db).await.unwrap();
        let sessions = PomodoroSession::list_for_user(&user, &db).await.unwrap();
        assert_eq!(sessions.len(), 2);
    }

    #[tokio::test]
    async fn a_backwards_clock_records_a_zero_stint_not_a_negative_one() {
        let db = database::init_mem().await.unwrap();
        let user = UserId::from_key(&Ulid::new().to_string());

        PomodoroSession::start(&user, &db).await.unwrap();
        // Stand in for the NTP step: push the running stint's start an hour
        // ahead, so the server clock `finish` reads is *behind* it.
        db.query("UPDATE $open SET started_at = $future")
            .bind(("open", PomodoroSessionId::open_for(&user).record()))
            .bind(("future", Timestamp::now().as_millis() + 3_600_000))
            .await
            .unwrap()
            .check()
            .unwrap();

        let closed = PomodoroSession::finish(&user, &db).await.unwrap();
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
