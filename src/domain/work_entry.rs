use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::WORK_ENTRY_TABLE;
use crate::database::{Database, transaction_with_retry};
use crate::domain::field_update::FieldUpdate;
use crate::domain::monotonic_id::next_ulid;
use crate::domain::page::PagedList;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

/// The one spelling of "this stint is inverted", shared by the handler's
/// pre-flight check and the write-time `WHERE` guard that re-makes it against
/// the stored row.
pub(crate) fn out_before_in_error() -> AppError {
    AppError::Validation(ValidationError::Invalid {
        field: "check_out",
        reason: "must be at or after check_in",
    })
}

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct WorkEntryId(RecordId);

impl WorkEntryId {
    /// Minted from the process-wide monotonic generator, not `Ulid::new()`:
    /// the log sorts `check_in DESC, id DESC` and the id breaks the tie between
    /// two stints checked in at the same instant. The `open_` key below never
    /// ties with itself (one open stint per user), so it needs no ordering.
    pub fn generate() -> Self {
        Self(RecordId::new(WORK_ENTRY_TABLE, next_ulid().to_string()))
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

    /// Close `user`'s open stint: atomically take the open row and re-file it
    /// under a ULID id, freeing the open slot for the next check-in. Take and
    /// re-file share one transaction — a failed re-file rolls the take back,
    /// so a stint can never vanish half-closed. Of two racing check-outs
    /// exactly one receives the row (the other gets the conflict).
    ///
    /// A lost round is re-sent rather than reported (`transaction_with_retry`):
    /// the abort wrote nothing, so the whole cascade is safe to repeat, and the
    /// `CREATE` cannot answer "already exists" on the way back — the table
    /// carries no `UNIQUE` index and `$closed` is one freshly minted ULID.
    /// Only the guard's own `THROW` is a decision, and it stays a 409.
    pub async fn check_out(user: &UserId, db: &Database) -> Result<WorkEntry, AppError> {
        let (mut result, mut errors) = transaction_with_retry(
            db,
            "BEGIN TRANSACTION;
                 LET $before = (DELETE $open RETURN BEFORE);
                 IF array::len($before) = 0 { THROW 'not_checked_in' };
                 CREATE $closed CONTENT {
                     user: $before[0].user,
                     check_in: $before[0].check_in,
                     check_out: $out,
                 };
                 COMMIT TRANSACTION;",
            &[
                (
                    "open".into(),
                    WorkEntryId::open_for(user).record().into_value(),
                ),
                (
                    "closed".into(),
                    WorkEntryId::generate().record().into_value(),
                ),
                ("out".into(), Timestamp::now().into_value()),
            ],
            &["not_checked_in"],
        )
        .await?;
        // An aborted transaction errors *every* slot, most with a generic
        // "not executed" — only the THROW's own slot names the reason, so scan
        // them all for the marker instead of trusting the first.
        if errors
            .values()
            .any(|error| error.to_string().contains("not_checked_in"))
        {
            return Err(AppError::Conflict("not checked in"));
        }
        if let Some(error) = errors.drain().map(|(_, error)| error).next() {
            return Err(error.into());
        }
        // Statement slots count BEGIN, the LET, and the IF: the CREATE is slot 3.
        let saved: Option<WorkEntry> = result.take::<Vec<WorkEntry>>(3)?.into_iter().next();
        saved.ok_or_else(|| AppError::Internal("failed to close work entry".into()))
    }

    pub async fn read(id: &WorkEntryId, db: &Database) -> Result<Option<WorkEntry>, AppError> {
        Ok(db.select(id.record()).await?)
    }

    /// Every stint of `user`, newest first — the open one (if any) included.
    /// Ordered by `check_in`, never by id alone: the open entry's `open_` key
    /// doesn't sort with the ULIDs, so id order would misplace it. The `id`
    /// tie-break behind it only ever separates two *closed* stints sharing a
    /// `check_in` — there is one open row per user, so it can never tie with
    /// itself — and that is what keeps offset paging from skipping a row.
    pub async fn list_for_user(
        user: &UserId,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<WorkEntry>, i64), AppError> {
        PagedList::new(
            "work_entry WHERE user = $usr",
            "ORDER BY check_in DESC, id DESC",
        )
        .bind("usr", user.record())
        .run(limit, offset, db)
        .await
    }

    /// Persist corrected instants (manager fix-ups on closed entries; the web
    /// layer validates ordering and rejects open entries).
    /// `None` = the correction left that instant out, so it is not written at
    /// all — the stint's `user` and `check_in`/`check_out` stamps are never
    /// replayed from a struct the handler read before its validation awaits.
    /// `check_out` is never cleared here: an open stint is refused upstream, so
    /// "absent" and "null" both mean keep.
    pub async fn update(
        self,
        check_in: Option<Timestamp>,
        check_out: Option<Timestamp>,
        db: &Database,
    ) -> Result<WorkEntry, AppError> {
        FieldUpdate::new(self.id.record())
            .set("check_in", check_in)
            .set("check_out", check_out)
            // Same race closer as the schedule ranges, with this table's own
            // field names and message: a correction of one instant is only
            // written while it still orders against the other as *stored*.
            .ordered("check_in", "check_out", out_before_in_error())
            .run::<WorkEntry>(db)
            .await
    }

    pub async fn remove(id: &WorkEntryId, db: &Database) -> Result<Option<WorkEntry>, AppError> {
        Ok(db.delete(id.record()).await?)
    }
}

#[cfg(test)]
mod tests {
    use ulid::Ulid;

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
        let (entries, _) = WorkEntry::list_for_user(&user, None, 0, &db).await.unwrap();
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
        let (entries, _) = WorkEntry::list_for_user(&user, None, 0, &db).await.unwrap();
        assert_eq!(entries.len(), 2);
    }
}
