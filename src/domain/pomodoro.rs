use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::POMODORO_SESSION_TABLE;
use crate::database::{Database, transaction_with_retry};
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
    pub async fn finish(user: &UserId, db: &Database) -> Result<PomodoroSession, AppError> {
        let (mut result, mut errors) = transaction_with_retry(
            db,
            "BEGIN TRANSACTION;
                 LET $before = (DELETE $open RETURN BEFORE);
                 IF array::len($before) = 0 { THROW 'no_pomodoro_running' };
                 CREATE $closed CONTENT {
                     user: $before[0].user,
                     started_at: $before[0].started_at,
                     finished_at: $done,
                 };
                 COMMIT TRANSACTION;",
            &[
                (
                    "open".into(),
                    PomodoroSessionId::open_for(user).record().into_value(),
                ),
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
        // Statement slots count BEGIN, the LET, and the IF: the CREATE is slot 3.
        let saved: Option<PomodoroSession> =
            result.take::<Vec<PomodoroSession>>(3)?.into_iter().next();
        saved.ok_or_else(|| AppError::Internal("failed to close pomodoro session".into()))
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
}
