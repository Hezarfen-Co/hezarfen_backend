//! The `work_entry` table: the open-slot check-in/check-out pair, the
//! newest-first log, and manager corrections guarded by the stored order.
//!
//! "At most one open stint per staff member" is the database's own rule here:
//! the partial unique index `work_entry_open` covers only rows whose
//! `check_out` is NULL, so a second check-in is a 23505 (mapped to the
//! check-in conflict below) and a check-out is one conditional `UPDATE` —
//! zero rows means there was no open stint to close. The old
//! take-and-re-file dance existed only to free the deterministic open id;
//! with the index, closing the stint *is* freeing the slot.

use crate::database::Database;
use crate::db::field_update::FieldUpdate;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::domain::work_entry::{WorkEntry, WorkEntryId, out_before_in_error};
use crate::error::AppError;

/// The row as the table spells it (`app_user` column), one step from the
/// domain struct (whose field is `user`).
struct WorkEntryRow {
    id: WorkEntryId,
    app_user: UserId,
    check_in: Timestamp,
    check_out: Option<Timestamp>,
}

impl From<WorkEntryRow> for WorkEntry {
    fn from(row: WorkEntryRow) -> Self {
        Self {
            id: row.id,
            user: row.app_user,
            check_in: row.check_in,
            check_out: row.check_out,
        }
    }
}

/// Open a stint for `user`, stamped with the server clock. The
/// `work_entry_open` partial unique index makes this atomic: if an open
/// entry already exists the insert is refused and the caller gets the
/// conflict — two racing check-ins can never create two open rows or reset
/// a running clock. One statement, one verdict, no retry loop.
pub async fn check_in(db: &Database, user: &UserId) -> Result<WorkEntry, AppError> {
    let id = WorkEntryId::generate();
    let check_in = Timestamp::now();
    let inserted = sqlx::query_as!(
        WorkEntryRow,
        "INSERT INTO work_entry (id, app_user, check_in) VALUES ($1, $2, $3) \
         RETURNING id AS \"id: WorkEntryId\", app_user AS \"app_user: UserId\", check_in AS \"check_in: Timestamp\", check_out AS \"check_out: Timestamp\"",
        id.uuid(),
        user.uuid(),
        check_in.as_millis(),
    )
    .fetch_one(db)
    .await;
    match inserted {
        Ok(row) => Ok(row.into()),
        Err(e) if crate::database::unique_violation(&e) == Some("work_entry_open") => {
            Err(AppError::Conflict("already checked in — check out first"))
        }
        Err(e) => Err(e.into()),
    }
}

/// Close `user`'s open stint: one conditional `UPDATE` takes it and stamps
/// `check_out`, which also frees the open slot for the next check-in (the
/// partial unique index only covers rows with a NULL `check_out`). Zero
/// rows = no open stint, the caller's conflict. Of two racing check-outs
/// exactly one receives the row — the statement takes the row's lock and
/// the loser re-evaluates against the winner's committed state.
pub async fn check_out(db: &Database, user: &UserId) -> Result<WorkEntry, AppError> {
    let out = Timestamp::now();
    let closed = sqlx::query_as!(
        WorkEntryRow,
        "UPDATE work_entry SET check_out = $2 \
         WHERE app_user = $1 AND check_out IS NULL \
         RETURNING id AS \"id: WorkEntryId\", app_user AS \"app_user: UserId\", check_in AS \"check_in: Timestamp\", check_out AS \"check_out: Timestamp\"",
        user.uuid(),
        out.as_millis(),
    )
    .fetch_optional(db)
    .await?;
    closed
        .map(WorkEntry::from)
        .ok_or(AppError::Conflict("not checked in"))
}

pub async fn read(db: &Database, id: &WorkEntryId) -> Result<Option<WorkEntry>, AppError> {
    let row = sqlx::query_as!(
        WorkEntryRow,
        "SELECT id AS \"id: WorkEntryId\", app_user AS \"app_user: UserId\", check_in AS \"check_in: Timestamp\", check_out AS \"check_out: Timestamp\" FROM work_entry WHERE id = $1",
        id.uuid(),
    )
    .fetch_optional(db)
    .await?;
    Ok(row.map(WorkEntry::from))
}

/// Every stint of `user`, newest first — the open one (if any) included.
/// Ordered by `check_in DESC`; the minted `id` breaks ties between two
/// stints sharing a `check_in` (uuid v7 sorts in mint order, which is what
/// keeps offset paging from skipping a row).
pub async fn list_for_user(
    db: &Database,
    user: &UserId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<WorkEntry>, i64), AppError> {
    let rows = match limit {
        Some(limit) => {
            sqlx::query_as!(
                WorkEntryRow,
                "SELECT id AS \"id: WorkEntryId\", app_user AS \"app_user: UserId\", check_in AS \"check_in: Timestamp\", check_out AS \"check_out: Timestamp\" FROM work_entry \
             WHERE app_user = $1 ORDER BY check_in DESC, id DESC \
             LIMIT $2 OFFSET $3",
                user.uuid(),
                limit,
                offset,
            )
            .fetch_all(db)
            .await?
        }
        None if offset > 0 => {
            sqlx::query_as!(
                WorkEntryRow,
                "SELECT id AS \"id: WorkEntryId\", app_user AS \"app_user: UserId\", check_in AS \"check_in: Timestamp\", check_out AS \"check_out: Timestamp\" FROM work_entry \
                 WHERE app_user = $1 ORDER BY check_in DESC, id DESC \
                 OFFSET $2",
                user.uuid(),
                offset,
            )
            .fetch_all(db)
            .await?
        }
        None => {
            sqlx::query_as!(
                WorkEntryRow,
                "SELECT id AS \"id: WorkEntryId\", app_user AS \"app_user: UserId\", check_in AS \"check_in: Timestamp\", check_out AS \"check_out: Timestamp\" FROM work_entry \
                 WHERE app_user = $1 ORDER BY check_in DESC, id DESC",
                user.uuid(),
            )
            .fetch_all(db)
            .await?
        }
    };
    // A window that can hide rows needs the count over the same WHERE;
    // an unpaged read from row zero already holds every row.
    let total = if limit.is_some() || offset > 0 {
        sqlx::query_scalar!("SELECT count(*) FROM work_entry WHERE app_user = $1", user.uuid())
            .fetch_one(db)
            .await?
            .unwrap_or(0)
    } else {
        rows.len() as i64
    };
    Ok((rows.into_iter().map(WorkEntry::from).collect(), total))
}

/// Persist corrected instants (manager fix-ups on closed entries; the web
/// layer validates ordering and rejects open entries).
/// `None` = the correction left that instant out, so it is not written at
/// all — the stint's `user` and `check_in`/`check_out` stamps are never
/// replayed from a struct the handler read before its validation awaits.
/// `check_out` is never cleared here: an open stint is refused upstream, so
/// "absent" and "null" both mean keep.
pub async fn update(
    db: &Database,
    entry: WorkEntry,
    check_in: Option<Timestamp>,
    check_out: Option<Timestamp>,
) -> Result<WorkEntry, AppError> {
    FieldUpdate::new("work_entry", entry.id.uuid())
        .set("check_in", check_in.map(|t| t.as_millis()))
        .set("check_out", check_out.map(|t| t.as_millis()))
        // Same race closer as the schedule ranges, with this table's own
        // field names and message: a correction of one instant is only
        // written while it still orders against the other as *stored*.
        .ordered("check_in", "check_out", out_before_in_error())
        .run::<WorkEntry>(db)
        .await
}

pub async fn remove(db: &Database, id: &WorkEntryId) -> Result<Option<WorkEntry>, AppError> {
    let row = sqlx::query_as!(
        WorkEntryRow,
        "DELETE FROM work_entry WHERE id = $1 \
         RETURNING id AS \"id: WorkEntryId\", app_user AS \"app_user: UserId\", check_in AS \"check_in: Timestamp\", check_out AS \"check_out: Timestamp\"",
        id.uuid(),
    )
    .fetch_optional(db)
    .await?;
    Ok(row.map(WorkEntry::from))
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
        let user = UserId::from_key(&Ulid::generate().to_string());

        let open = check_in(&db, &user).await.unwrap();
        assert!(open.is_open());
        assert_eq!(open.get_id().key(), format!("open_{}", user.key()));

        // Second check-in must not create a second stint or reset the clock.
        let dup = check_in(&db, &user).await;
        assert!(matches!(dup, Err(AppError::Conflict(_))));
        let (entries, _) = list_for_user(&db, &user, None, 0).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].get_check_in(), open.get_check_in());

        let closed = check_out(&db, &user).await.unwrap();
        assert!(!closed.is_open());
        assert!(closed.get_check_out().unwrap() >= closed.get_check_in());

        // The open slot is free again; a stray second check-out conflicts.
        assert!(matches!(
            check_out(&db, &user).await,
            Err(AppError::Conflict(_))
        ));
        check_in(&db, &user).await.unwrap();
        let (entries, _) = list_for_user(&db, &user, None, 0).await.unwrap();
        assert_eq!(entries.len(), 2);
    }
}
