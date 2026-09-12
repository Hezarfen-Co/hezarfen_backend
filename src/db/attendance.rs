//! The `attendance` table: event roll call. Every write is one guarded
//! statement whose gates ride inside it, so the whole module is the
//! persistence half; the workflow that decides it lives in
//! [`crate::service::attendance`].

use crate::database::{Database, foreign_key_violation};
use crate::db::page::PagedList;
use crate::domain::attendance::{Attendance, AttendanceStatus};
use crate::domain::event::EventId;
use crate::domain::user::UserId;
use crate::error::AppError;
use sqlx::query_as;

/// Record (or overwrite) `user`'s status for `event`. The pair is the
/// table's primary key, so this is a single atomic upsert — concurrent
/// marks for the same pair converge on the one row instead of racing.
///
/// The event's existence is proved by the foreign key itself: a mark naming
/// an event whose row is gone is refused `23503`, mapped to the same 404 the
/// old existence-proof transaction produced. No bump-and-restore trick, no
/// retry loop — one statement decides.
pub async fn mark(
    db: &Database,
    event: &EventId,
    user: &UserId,
    status: AttendanceStatus,
    marked_by: &UserId,
) -> Result<Attendance, AppError> {
    match query_as!(
        Attendance,
        "INSERT INTO attendance (event, app_user, status, marked_by)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (event, app_user) DO UPDATE SET status = $3, marked_by = $4
         RETURNING event AS \"event: EventId\", app_user AS \"user: UserId\", \
                  status AS \"status: AttendanceStatus\", marked_by AS \"marked_by: UserId\"",
        event.uuid(),
        user.uuid(),
        status.as_str(),
        marked_by.uuid()
    )
    .fetch_one(db)
    .await
    {
        Ok(row) => Ok(row),
        // The event row is gone: exactly today's "no such event" refusal.
        // (The other foreign keys name users the routes already authenticated;
        // a violation there stays a database error, as before.)
        Err(err) if foreign_key_violation(&err) => Err(AppError::NotFound),
        Err(err) => Err(err.into()),
    }
}

pub async fn list_for_event(
    db: &Database,
    event: &EventId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Attendance>, i64), AppError> {
    // The composite key's text form ordered the old listing; on the natural
    // key that order is (event, then user).
    PagedList::new(
        "attendance WHERE event = $1",
        "ORDER BY event DESC, app_user DESC",
    )
    .bind(event.uuid())
    .run(limit, offset, db)
    .await
}

/// Every event-attendance row recorded for `user` — the events half of the
/// attendance report.
pub async fn list_for_user(db: &Database, user: &UserId) -> Result<Vec<Attendance>, AppError> {
    let rows = query_as!(
        Attendance,
        "SELECT event AS \"event: EventId\", app_user AS \"user: UserId\", \
                status AS \"status: AttendanceStatus\", \
                marked_by AS \"marked_by: UserId\" \
         FROM attendance
         WHERE app_user = $1
         ORDER BY event DESC, app_user DESC",
        user.uuid()
    )
    .fetch_all(db)
    .await?;
    Ok(rows)
}

pub async fn remove(
    db: &Database,
    event: &EventId,
    user: &UserId,
) -> Result<Option<Attendance>, AppError> {
    let gone = query_as!(
        Attendance,
        "DELETE FROM attendance WHERE event = $1 AND app_user = $2
         RETURNING event AS \"event: EventId\", app_user AS \"user: UserId\", \
                  status AS \"status: AttendanceStatus\", \
                  marked_by AS \"marked_by: UserId\"",
        event.uuid(),
        user.uuid()
    )
    .fetch_optional(db)
    .await?;
    Ok(gone)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::settings::Settings;

    /// The session twin's race, one table over
    /// ([`crate::db::session_attendance::mark`]): a
    /// `DEFINE EVENT` on `event` fires inside the delete's own transaction, and
    /// the event's delete sweeps its attendance rows before removing the row, so
    /// the `SLEEP` lands exactly between the sweep and the commit — the window
    /// where a bare upsert wrote a mark nothing would ever sweep again.
    ///
    /// Real server, and `#[ignore]`d for it: the subject is the store's
    /// conflict detection, which `init_mem`'s embedded engine does not have —
    /// it commits both writes and answers `Ok` to each, so this passes there on
    /// broken code. Mutation-tested: turning the claim back into a read
    /// (`SELECT VALUE id FROM $ev`) turns it red.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs a real SurrealDB server: podman start hezarfen-surrealdb && cargo test -- --ignored"]
    async fn a_mark_written_inside_a_delete_never_outlives_the_event() {
        use crate::domain::event::{EventAudience, EventDescription, EventTitle};
        let (db, _serialized) = crate::database::init_test_server("event_attendance_race").await;
        db.query(
            "DEFINE EVENT hold_the_window ON TABLE event WHEN $event = 'DELETE' \
             THEN { SLEEP 1s; };",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

        let allowed: Vec<String> = Settings::defaults().get_attendance_statuses().to_vec();
        let marker = UserId::from_key("t");
        let (mut swept, mut orphans) = (0, 0);
        for round in 0..4 {
            let event = crate::db::event::create(
                &db,
                &marker,
                EventTitle::try_new("gezi").unwrap(),
                EventDescription::try_new("").unwrap(),
                EventAudience::School,
                None,
                None,
            )
            .await
            .unwrap();
            let id = event.get_id().clone();
            let drop_it = {
                let db = db.clone();
                tokio::spawn(async move { crate::db::event::delete(&db, event).await })
            };
            // The mark starts inside the held window: the sweep has run and the
            // event row is gone but uncommitted.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let marked = {
                let (db, id, marker) = (db.clone(), id.clone(), marker.clone());
                let student = UserId::from_key(&format!("s{round}"));
                let status = AttendanceStatus::try_new("present", &allowed).unwrap();
                tokio::spawn(async move { mark(&db, &id, &student, status, &marker).await })
            };
            let (drop_it, marked) = (drop_it.await.unwrap(), marked.await.unwrap());
            assert!(
                !matches!(marked, Err(AppError::Db(_))),
                "round {round}: a raced mark must be answered, not 500: {marked:?}"
            );

            // Stored state is the whole verdict; a return value is not evidence.
            if crate::db::event::read(&db, &id).await.unwrap().is_none() {
                swept += 1;
                orphans += list_for_event(&db, &id, None, 0).await.unwrap().0.len();
            } else if drop_it.is_ok() {
                panic!("round {round}: the delete reported success but the event is still there");
            }
        }
        eprintln!("Event::delete raced by a mark: {swept}/4 rounds deleted the event");
        assert!(
            swept > 0,
            "no round ever deleted the event, so the window was never reached"
        );
        assert_eq!(orphans, 0, "a mark outlived its event");
    }
}
