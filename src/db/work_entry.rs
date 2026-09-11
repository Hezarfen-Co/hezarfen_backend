//! The `work_entry` table: the open-slot check-in/check-out pair, the
//! newest-first log, and manager corrections guarded by the stored order.

use surrealdb::types::SurrealValue;

use crate::database::{Database, transaction_with_retry};
use crate::db::field_update::FieldUpdate;
use crate::db::page::PagedList;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::domain::work_entry::{WorkEntry, WorkEntryId, out_before_in_error};
use crate::error::AppError;

/// Open a stint for `user`, stamped with the server clock. `INSERT IGNORE`
/// on the deterministic open id makes this atomic: if an open entry
/// already exists the insert is skipped (empty result) and the caller gets
/// a conflict — two racing check-ins can never create two open rows or
/// reset a running clock.
pub async fn check_in(db: &Database, user: &UserId) -> Result<WorkEntry, AppError> {
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
pub async fn check_out(db: &Database, user: &UserId) -> Result<WorkEntry, AppError> {
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

pub async fn read(db: &Database, id: &WorkEntryId) -> Result<Option<WorkEntry>, AppError> {
    Ok(db.select(id.record()).await?)
}

/// Every stint of `user`, newest first — the open one (if any) included.
/// Ordered by `check_in`, never by id alone: the open entry's `open_` key
/// doesn't sort with the ULIDs, so id order would misplace it. The `id`
/// tie-break behind it only ever separates two *closed* stints sharing a
/// `check_in` — there is one open row per user, so it can never tie with
/// itself — and that is what keeps offset paging from skipping a row.
pub async fn list_for_user(
    db: &Database,
    user: &UserId,
    limit: Option<i64>,
    offset: i64,
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
    db: &Database,
    entry: WorkEntry,
    check_in: Option<Timestamp>,
    check_out: Option<Timestamp>,
) -> Result<WorkEntry, AppError> {
    FieldUpdate::new(entry.get_id().record())
        .set("check_in", check_in)
        .set("check_out", check_out)
        // Same race closer as the schedule ranges, with this table's own
        // field names and message: a correction of one instant is only
        // written while it still orders against the other as *stored*.
        .ordered("check_in", "check_out", out_before_in_error())
        .run::<WorkEntry>(db)
        .await
}

pub async fn remove(db: &Database, id: &WorkEntryId) -> Result<Option<WorkEntry>, AppError> {
    Ok(db.delete(id.record()).await?)
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
