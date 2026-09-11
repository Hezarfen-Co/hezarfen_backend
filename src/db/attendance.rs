//! The `attendance` table: event roll call. Every write is a single
//! transaction whose gates ride inside it (the event-existence proof), so
//! the whole module is the persistence half; the workflow that decides it
//! lives in [`crate::service::attendance`].

use surrealdb::types::SurrealValue;

use crate::constant::REGISTRATION_COUNT_FIELD;
use crate::database::{Database, transaction_with_retry};
use crate::db::page::PagedList;
use crate::domain::attendance::{Attendance, AttendanceId, AttendanceStatus};
use crate::domain::event::EventId;
use crate::domain::user::UserId;
use crate::error::AppError;

/// Record (or overwrite) `user`'s status for `event`. One row per (event,
/// user), keyed by a deterministic composite id so this is a single atomic
/// UPSERT — concurrent marks for the same pair can no longer both insert and
/// collide on the unique index (a 500); they converge on the one row.
///
/// The event's existence is proved *inside* the write, the twin of the gate
/// in [`crate::db::session_attendance::mark`]: the handler's event read sits
/// several round trips in front of this, so a bare upsert left a mark on an
/// event [`crate::domain::event::Event`]'s cascade
/// (`DELETE attendance WHERE event = $ev`) had already swept — counted
/// forever in `GET /attendance/{user}` on an event no page shows. Reading
/// the event here would not have closed it either: SurrealDB 3.2.3
/// conflict-checks write sets, not read sets, so the proof has to *move* a
/// value on the event row. It is the bump-and-restore of
/// [`crate::domain::exam_answer::ExamAnswer::save`], on the counter the
/// event already carries: matching nothing is the existence gate, writing
/// the key the delete removes is the collision, and the restore is by
/// captured value (`NONE` included) so no seat is spent or freed.
///
/// Admissible for [`transaction_with_retry`]: no statement can answer
/// "already exists" — the `UPSERT`'s composite id is bijective with the
/// `attendance_event_user` unique tuple, so it resolves onto the row it
/// names instead of colliding with it.
pub async fn mark(
    db: &Database,
    event: &EventId,
    user: &UserId,
    status: AttendanceStatus,
    marked_by: &UserId,
) -> Result<Attendance, AppError> {
    let attendance = Attendance {
        id: AttendanceId::composite(event, user),
        event: event.clone(),
        user: user.clone(),
        status,
        marked_by: marked_by.clone(),
    };
    let (mut result, mut errors) = transaction_with_retry(
        db,
        &format!(
            "BEGIN TRANSACTION;
             LET $was = (SELECT VALUE {REGISTRATION_COUNT_FIELD} FROM ONLY $ev);
             LET $alive = (UPDATE $ev SET {REGISTRATION_COUNT_FIELD} =
                 ({REGISTRATION_COUNT_FIELD} ?? 0) + 1 RETURN VALUE id);
             IF array::len($alive) = 0 {{ THROW 'event_missing' }};
             UPDATE $ev SET {REGISTRATION_COUNT_FIELD} = $was;
             LET $after = (UPSERT $id CONTENT $row RETURN AFTER);
             RETURN $after;
             COMMIT TRANSACTION;"
        ),
        &[
            ("ev".into(), event.record().into_value()),
            ("id".into(), attendance.id.record().into_value()),
            ("row".into(), attendance.into_value()),
        ],
        &["event_missing"],
    )
    .await?;
    // An aborted transaction errors every slot and only the THROW's own
    // slot names the marker.
    if errors
        .values()
        .any(|error| error.to_string().contains("event_missing"))
    {
        return Err(AppError::NotFound);
    }
    if let Some(error) = errors.drain().map(|(_, error)| error).next() {
        return Err(error.into());
    }
    // The trailing `RETURN` is always the last statement before `COMMIT`.
    let slot = result.num_statements().saturating_sub(2);
    result
        .take::<Vec<Attendance>>(slot)?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::Internal("failed to mark attendance".into()))
}

pub async fn list_for_event(
    db: &Database,
    event: &EventId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<Attendance>, i64), AppError> {
    PagedList::new("attendance WHERE event = $ev", "ORDER BY id DESC")
        .bind("ev", event.record())
        .run(limit, offset, db)
        .await
}

/// Every event-attendance row recorded for `user` — the events half of the
/// attendance report.
pub async fn list_for_user(db: &Database, user: &UserId) -> Result<Vec<Attendance>, AppError> {
    let mut result = db
        .query("SELECT * FROM attendance WHERE user = $usr ORDER BY id DESC")
        .bind(("usr", user.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<Attendance>>(0)?)
}

pub async fn remove(
    db: &Database,
    event: &EventId,
    user: &UserId,
) -> Result<Option<Attendance>, AppError> {
    let mut result = db
        .query("DELETE attendance WHERE event = $ev AND user = $usr RETURN BEFORE")
        .bind(("ev", event.record()))
        .bind(("usr", user.record()))
        .await?
        .check()?;
    Ok(result.take::<Vec<Attendance>>(0)?.into_iter().next())
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
        use crate::domain::event::{Event, EventAudience, EventDescription, EventTitle};
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
            let event = Event::create(
                &marker,
                EventTitle::try_new("gezi").unwrap(),
                EventDescription::try_new("").unwrap(),
                EventAudience::School,
                None,
                None,
                &db,
            )
            .await
            .unwrap();
            let id = event.get_id().clone();
            let drop_it = {
                let db = db.clone();
                tokio::spawn(async move { event.delete(&db).await })
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
            if Event::read(&id, &db).await.unwrap().is_none() {
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
