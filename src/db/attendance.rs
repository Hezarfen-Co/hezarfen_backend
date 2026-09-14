//! The `attendance` table: event roll call. Every write is one guarded
//! statement whose gates ride inside it, so the whole module is the
//! persistence half; the workflow that decides it lives in
//! [`crate::service::attendance`].

use crate::database::{Database, tx_with_retry};
use crate::db::page::PagedList;
use crate::domain::attendance::{Attendance, AttendanceStatus};
use crate::domain::event::EventId;
use crate::domain::timestamp::Timestamp;
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
    // Owned captures (`Send` rule of `tx_with_retry`'s closure).
    let event = event.clone();
    let user = *user;
    let marked_by = *marked_by;
    tx_with_retry(db, false, async move |tx| {
        // The existence gate and the collision key: the event row is locked
        // inside the mark's own transaction, so a concurrent delete cascade
        // either lands whole before the mark (404, nothing stored) or queues
        // behind the mark's commit — and then its sweep, reading after that
        // commit, takes the mark's row with it. Without the lock the insert
        // never queued on the event at all: a mark racing
        // `DELETE /events/{id}` could commit an orphan its cascade's
        // already-run sweep never saw.
        let live = sqlx::query!(
            r#"SELECT 1 AS "one!: i64" FROM event WHERE id = $1 FOR NO KEY UPDATE"#,
            event.uuid()
        )
        .fetch_optional(&mut *tx)
        .await?;
        if live.is_none() {
            // The event row is gone: exactly today's "no such event" refusal.
            // (The foreign keys name users the routes already authenticated;
            // a violation there stays a database error, as before.)
            return Err(AppError::NotFound);
        }
        let row = query_as!(
            Attendance,
            "INSERT INTO attendance (event, app_user, status, marked_by, marked_at)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (event, app_user) DO UPDATE SET status = $3, marked_by = $4, marked_at = $5
             RETURNING event AS \"event: EventId\", app_user AS \"user: UserId\", \
                      status AS \"status: AttendanceStatus\", marked_by AS \"marked_by: UserId\"",
            event.uuid(),
            user.uuid(),
            status.as_str(),
            marked_by.uuid(),
            Timestamp::now().as_millis(),
        )
        .fetch_one(&mut *tx)
        .await?;
        Ok(row)
    })
    .await
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
    /// ([`crate::db::session_attendance::mark`]): the event's delete sweeps its
    /// attendance rows and removes the row in one transaction, and a mark that
    /// wrote into the gap was a bare upsert nothing would ever sweep again.
    /// The mark now writes the event row too (its seats counter), so the two
    /// transactions touch one row and Postgres refuses one of them; the barrier
    /// releases both sides together so every interleaving gets its chance.
    /// Mutation-tested: turning the claim back into a read turns it red.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_mark_written_inside_a_delete_never_outlives_the_event() {
        use crate::domain::event::{
            EventAudience, EventAudienceKind, EventDescription, EventTitle,
        };

        // A real `app_user` row: markers, students and event creators are
        // foreign keys now.
        async fn a_person(db: &Database, label: &str) -> UserId {
            let user = UserId::generate();
            sqlx::query(
                "INSERT INTO app_user (id, username, created_at, role) \
                 VALUES ($1, $2, 0, 'teacher')",
            )
            .bind(user.uuid())
            .bind(format!("{label}-{}", &user.key()[30..]))
            .execute(db)
            .await
            .unwrap();
            user
        }

        let (db, _leases) = crate::database::init_test_db().await;
        let allowed: Vec<String> = Settings::defaults().get_attendance_statuses().to_vec();
        let marker = a_person(&db, "marker").await;
        let (mut swept, mut orphans) = (0, 0);
        for round in 0..8 {
            let event = crate::db::event::create(
                &db,
                &marker,
                EventTitle::try_new("gezi").unwrap(),
                EventDescription::try_new("").unwrap(),
                EventAudience {
                    kind: EventAudienceKind::School,
                    role: None,
                    course: None,
                    class: None,
                    capacity: None,
                },
                None,
                None,
            )
            .await
            .unwrap();
            let id = event.get_id().clone();

            // Delete and mark released together: both write the event row, so
            // Postgres serializes them and refuses whichever lost.
            let gate = std::sync::Arc::new(tokio::sync::Barrier::new(2));
            let drop_it = {
                let (db, gate, event) = (db.clone(), gate.clone(), event);
                tokio::spawn(async move {
                    gate.wait().await;
                    crate::db::event::delete(&db, event).await
                })
            };
            let marked = {
                let (db, id, marker, gate) = (db.clone(), id.clone(), marker, gate);
                let student = a_person(&db, "student").await;
                let status = AttendanceStatus::try_new("present", &allowed).unwrap();
                tokio::spawn(async move {
                    gate.wait().await;
                    mark(&db, &id, &student, status, &marker).await
                })
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
        eprintln!("Event::delete raced by a mark: {swept}/8 rounds deleted the event");
        assert!(
            swept > 0,
            "no round ever deleted the event, so the window was never reached"
        );
        assert_eq!(orphans, 0, "a mark outlived its event");
    }
}
