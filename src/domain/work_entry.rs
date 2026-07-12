use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};
use ulid::Ulid;

use crate::database::{Database, WORK_ENTRY_TABLE};
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct WorkEntryId(RecordId);

impl WorkEntryId {
    pub fn generate() -> Self {
        Self(RecordId::new(WORK_ENTRY_TABLE, Ulid::new().to_string()))
    }

    /// The deterministic id of `user`'s *open* entry. At most one open stint
    /// per user holds by construction: checking in is a single `INSERT IGNORE`
    /// on this id (atomic — a second check-in changes nothing), and checking
    /// out atomically takes the row and re-files it under a ULID id.
    /// `open_` cannot collide with a ULID key (ULIDs are bare alphanumerics).
    pub fn open_for(user: &UserId) -> Self {
        Self(RecordId::new(
            WORK_ENTRY_TABLE,
            format!("open_{}", user.key()),
        ))
    }

    pub fn from_key(key: &str) -> Self {
        Self(RecordId::new(WORK_ENTRY_TABLE, key))
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

/// One work stint of a staff member: server-stamped `check_in`, and
/// `check_out` once closed. The wall clock is read server-side only — a
/// client can never supply its own instants (managers correct closed entries
/// through an explicit endpoint instead).
#[derive(Debug, Clone, SurrealValue)]
pub struct WorkEntry {
    id: WorkEntryId,
    user: UserId,
    check_in: Timestamp,
    check_out: Option<Timestamp>,
}

impl WorkEntry {
    pub fn get_id(&self) -> &WorkEntryId {
        &self.id
    }

    pub fn get_user(&self) -> &UserId {
        &self.user
    }

    pub fn get_check_in(&self) -> Timestamp {
        self.check_in
    }

    pub fn get_check_out(&self) -> Option<Timestamp> {
        self.check_out
    }

    pub fn is_open(&self) -> bool {
        self.check_out.is_none()
    }

    /// Open a stint for `user`, stamped with the server clock. `INSERT IGNORE`
    /// on the deterministic open id makes this atomic: if an open entry
    /// already exists the insert is skipped (empty result) and the caller gets
    /// a conflict — two racing check-ins can never create two open rows or
    /// reset a running clock.
    pub async fn check_in(user: &UserId, db: &Database) -> Result<WorkEntry, AppError> {
        let entry = WorkEntry {
            id: WorkEntryId::open_for(user),
            user: user.clone(),
            check_in: Timestamp::now(),
            check_out: None,
        };
        let mut result = db
            .query("INSERT IGNORE INTO work_entry $entry")
            .bind(("entry", entry))
            .await?
            .check()?;
        result
            .take::<Vec<WorkEntry>>(0)?
            .into_iter()
            .next()
            .ok_or(AppError::Conflict("already checked in — check out first"))
    }

    /// Close `user`'s open stint. The `DELETE .. RETURN BEFORE` is an atomic
    /// take: of two racing check-outs exactly one receives the row (the other
    /// gets the conflict). The closed stint is then re-filed under a ULID id,
    /// freeing the open slot for the next check-in.
    pub async fn check_out(user: &UserId, db: &Database) -> Result<WorkEntry, AppError> {
        let mut result = db
            .query("DELETE $open RETURN BEFORE")
            .bind(("open", WorkEntryId::open_for(user).record()))
            .await?
            .check()?;
        let open = result
            .take::<Vec<WorkEntry>>(0)?
            .into_iter()
            .next()
            .ok_or(AppError::Conflict("not checked in"))?;

        let closed = WorkEntry {
            id: WorkEntryId::generate(),
            user: open.user,
            check_in: open.check_in,
            check_out: Some(Timestamp::now()),
        };
        let saved: Option<WorkEntry> = db.create(closed.id.record()).content(closed).await?;
        saved.ok_or_else(|| AppError::Internal("failed to close work entry".into()))
    }

    pub async fn read(id: &WorkEntryId, db: &Database) -> Result<Option<WorkEntry>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// Every stint of `user`, newest first — the open one (if any) included.
    /// Ordered by `check_in`: the open entry's `open_` key doesn't sort with
    /// the ULIDs, so id order would misplace it.
    pub async fn list_for_user(user: &UserId, db: &Database) -> Result<Vec<WorkEntry>, AppError> {
        let mut result = db
            .query("SELECT * FROM work_entry WHERE user = $usr ORDER BY check_in DESC")
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<WorkEntry>>(0)?)
    }

    /// Persist corrected instants (manager fix-ups on closed entries; the web
    /// layer validates ordering and rejects open entries).
    pub async fn update(
        mut self,
        check_in: Timestamp,
        check_out: Timestamp,
        db: &Database,
    ) -> Result<WorkEntry, AppError> {
        self.check_in = check_in;
        self.check_out = Some(check_out);
        let updated: Option<WorkEntry> = db.update(self.id.record()).content(self).await?;
        updated.ok_or(AppError::NotFound)
    }

    pub async fn remove(id: &WorkEntryId, db: &Database) -> Result<Option<WorkEntry>, AppError> {
        Ok(db.delete(id.record()).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database;

    #[tokio::test]
    async fn check_in_is_exclusive_until_check_out() {
        let db = database::init_mem().await.unwrap();
        // A `record<user>` column checks the table of the id, not row
        // existence — a fabricated id keeps this test free of user ceremony.
        let user = UserId::from_key(&Ulid::new().to_string());

        let open = WorkEntry::check_in(&user, &db).await.unwrap();
        assert!(open.is_open());
        assert_eq!(open.get_id().key(), format!("open_{}", user.key()));

        // Second check-in must not create a second stint or reset the clock.
        let dup = WorkEntry::check_in(&user, &db).await;
        assert!(matches!(dup, Err(AppError::Conflict(_))));
        let entries = WorkEntry::list_for_user(&user, &db).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].get_check_in(), open.get_check_in());

        let closed = WorkEntry::check_out(&user, &db).await.unwrap();
        assert!(!closed.is_open());
        assert!(closed.get_check_out().unwrap() >= closed.get_check_in());

        // The open slot is free again; a stray second check-out conflicts.
        assert!(matches!(
            WorkEntry::check_out(&user, &db).await,
            Err(AppError::Conflict(_))
        ));
        WorkEntry::check_in(&user, &db).await.unwrap();
        let entries = WorkEntry::list_for_user(&user, &db).await.unwrap();
        assert_eq!(entries.len(), 2);
    }
}
