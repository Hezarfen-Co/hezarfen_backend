use surrealdb::types::{RecordId, RecordIdKey, SurrealValue};

use crate::constant::{ATTENDANCE_TABLE, REGISTRATION_COUNT_FIELD};
use crate::database::{Database, transaction_with_retry};
use crate::domain::event::EventId;
use crate::db::page::PagedList;
use crate::domain::user::UserId;
use crate::error::{AppError, ValidationError};

#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct AttendanceId(RecordId);

impl AttendanceId {
    /// A deterministic id for the (event, user) pair. Because the same pair
    /// always maps to the same record id, marking is a single atomic UPSERT with
    /// no find-then-insert race, and one-row-per-pair holds by construction.
    /// ULID keys are alphanumeric, so `_` is an unambiguous joiner.
    pub fn composite(event: &EventId, user: &UserId) -> Self {
        Self(RecordId::new(
            ATTENDANCE_TABLE,
            format!("{}_{}", event.key(), user.key()),
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

/// A validated attendance state — one of the school's configured statuses
/// ([`crate::domain::settings::Settings::get_attendance_statuses`]): the core
/// `present | absent | late | excused` plus any the school added. Stored rows
/// keep their status even if the school later edits the list.
#[derive(Debug, Clone, PartialEq, Eq, SurrealValue)]
pub struct AttendanceStatus(String);

impl AttendanceStatus {
    pub fn try_new(value: &str, allowed: &[String]) -> Result<Self, ValidationError> {
        if !allowed.iter().any(|status| status == value) {
            return Err(ValidationError::Invalid {
                field: "status",
                reason: "is not one of this school's attendance statuses (see GET /settings)",
            });
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, SurrealValue)]
pub struct Attendance {
    id: AttendanceId,
    event: EventId,
    user: UserId,
    status: AttendanceStatus,
    marked_by: UserId,
}

impl Attendance {
    pub fn get_id(&self) -> &AttendanceId {
        &self.id
    }

    pub fn get_event(&self) -> &EventId {
        &self.event
    }

    pub fn get_user(&self) -> &UserId {
        &self.user
    }

    pub fn get_status(&self) -> &AttendanceStatus {
        &self.status
    }

    pub fn get_marked_by(&self) -> &UserId {
        &self.marked_by
    }

    /// Record (or overwrite) `user`'s status for `event`. One row per (event,
    /// user), keyed by a deterministic composite id so this is a single atomic
    /// UPSERT — concurrent marks for the same pair can no longer both insert and
    /// collide on the unique index (a 500); they converge on the one row.
    ///
    /// The event's existence is proved *inside* the write, the twin of the gate
    /// in [`crate::domain::session_attendance::SessionAttendance::mark`]: the
    /// handler's event read sits several round trips in front of this, so a
    /// bare upsert left a mark on an event [`crate::domain::event::Event`]'s
    /// cascade (`DELETE attendance WHERE event = $ev`) had already swept —
    /// counted forever in `GET /attendance/{user}` on an event no page shows.
    /// Reading the event here would not have closed it either: SurrealDB 3.2.3
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
        event: &EventId,
        user: &UserId,
        status: AttendanceStatus,
        marked_by: &UserId,
        db: &Database,
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
        event: &EventId,
        limit: Option<i64>,
        offset: i64,
        db: &Database,
    ) -> Result<(Vec<Attendance>, i64), AppError> {
        PagedList::new("attendance WHERE event = $ev", "ORDER BY id DESC")
            .bind("ev", event.record())
            .run(limit, offset, db)
            .await
    }

    /// Every event-attendance row recorded for `user` — the events half of the
    /// attendance report.
    pub async fn list_for_user(user: &UserId, db: &Database) -> Result<Vec<Attendance>, AppError> {
        let mut result = db
            .query("SELECT * FROM attendance WHERE user = $usr ORDER BY id DESC")
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Attendance>>(0)?)
    }

    pub async fn remove(
        event: &EventId,
        user: &UserId,
        db: &Database,
    ) -> Result<Option<Attendance>, AppError> {
        let mut result = db
            .query("DELETE attendance WHERE event = $ev AND user = $usr RETURN BEFORE")
            .bind(("ev", event.record()))
            .bind(("usr", user.record()))
            .await?
            .check()?;
        Ok(result.take::<Vec<Attendance>>(0)?.into_iter().next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn status_must_be_in_the_allowed_list() {
        let allowed: Vec<String> = crate::domain::settings::Settings::defaults()
            .get_attendance_statuses()
            .to_vec();
        for status in ["present", "absent", "late", "excused"] {
            assert_eq!(
                AttendanceStatus::try_new(status, &allowed)
                    .unwrap()
                    .as_str(),
                status
            );
        }
        assert!(AttendanceStatus::try_new("maybe", &allowed).is_err());
        assert!(AttendanceStatus::try_new("", &allowed).is_err());
        // A school-added status is accepted once it's in the list.
        let extended: Vec<String> = allowed
            .iter()
            .cloned()
            .chain(["online".to_string()])
            .collect();
        assert!(AttendanceStatus::try_new("online", &extended).is_ok());
    }

    /// The session twin's race, one table over
    /// ([`crate::domain::session_attendance::SessionAttendance`]): a
    /// `DEFINE EVENT` on `event` fires inside the delete's own transaction, and
    /// [`Event::delete`] sweeps its attendance rows before removing the row, so
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

        let allowed: Vec<String> = crate::domain::settings::Settings::defaults()
            .get_attendance_statuses()
            .to_vec();
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
                tokio::spawn(
                    async move { Attendance::mark(&id, &student, status, &marker, &db).await },
                )
            };
            let (drop_it, marked) = (drop_it.await.unwrap(), marked.await.unwrap());
            assert!(
                !matches!(marked, Err(AppError::Db(_))),
                "round {round}: a raced mark must be answered, not 500: {marked:?}"
            );

            // Stored state is the whole verdict; a return value is not evidence.
            if Event::read(&id, &db).await.unwrap().is_none() {
                swept += 1;
                orphans += Attendance::list_for_event(&id, None, 0, &db)
                    .await
                    .unwrap()
                    .0
                    .len();
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
