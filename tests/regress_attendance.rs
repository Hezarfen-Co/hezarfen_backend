//! Regressions for the two roll-call defects found in the 2026-08-02 sweep:
//! the staff-row gate keyed on the session's *current* teacher, and the
//! roll-call write that could land on an already-deleted session.
//!
//! The orphan half is a true race, so it is driven at the domain layer instead
//! of over HTTP: the handler's session snapshot is taken first, the session is
//! then deleted, and the mark is sent with that stale snapshot — exactly the
//! state the racing request holds. The assertion is on what the store holds
//! afterwards, never on the call's own answer.

mod common;

use axum::http::StatusCode;
use common::{
    app_and_db, create_session, enroll, login, login_as, me_id, send, taught, taught_under,
};
use hezarfen_backend::database::Database;
use hezarfen_backend::db::attendance;
use hezarfen_backend::db::course_session;
use hezarfen_backend::db::session_attendance;
use hezarfen_backend::db::settings;
use hezarfen_backend::domain::attendance::AttendanceStatus;
use hezarfen_backend::domain::course_session::CourseSessionId;
use hezarfen_backend::domain::event::EventId;
use hezarfen_backend::domain::timestamp::Timestamp;
use hezarfen_backend::domain::user::UserId;
use hezarfen_backend::error::AppError;
use serde_json::json;

fn soon() -> i64 {
    Timestamp::now().as_millis() + 3_600_000
}

/// Proof a mark queues on its parent row's write lock: the parent row is
/// locked from a second transaction while the mark runs, and the mark may
/// only answer once the guard releases. That queueing is the Postgres shape
/// of the collision the old `DEFINE EVENT` write-probe watched — the lock,
/// taken inside the mark's own transaction, is the shared key a concurrent
/// delete collides with.
async fn queues_on_parent_lock(
    db: &Database,
    table: &'static str,
    row: uuid::Uuid,
    mark: impl Future<Output = StatusCode> + Send + 'static,
) -> StatusCode {
    let mut guard = db.begin().await.expect("guard transaction");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT 1 FROM {table} WHERE id = $1 FOR NO KEY UPDATE"
    )))
    .bind(row)
    .execute(&mut *guard)
    .await
        .expect("lock the parent row");
    let landed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = landed.clone();
    let task = tokio::spawn(async move {
        let status = mark.await;
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
        status
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(
        !landed.load(std::sync::atomic::Ordering::SeqCst),
        "the mark answered without queueing on the parent row's lock"
    );
    guard.rollback().await.expect("release the lock");
    tokio::time::timeout(std::time::Duration::from_secs(10), task)
        .await
        .expect("the mark landed once the lock lifted")
        .expect("mark task")
}

/// A staff roll-call row is management's to write *and* to remove. The removal
/// gate used to ask "is the target the session's teacher?", which a teacher
/// reassignment falsifies: once the session moved to T2, the row T1 holds is no
/// longer the *current* teacher's, so T2 — an ordinary teacher — could delete
/// the staff row the manager+ requirement exists to protect. The gate now asks
/// the target's live role instead, which a reassignment cannot change.
#[tokio::test]
async fn reassigning_the_teacher_does_not_unlock_the_old_teachers_staff_row() {
    let (app, db) = app_and_db().await;
    let boss = login_as(&app, &db, "mudur", "manager").await;
    let t1 = login_as(&app, &db, "hoca", "teacher").await;
    let t2 = login_as(&app, &db, "yeni", "teacher").await;
    let ali = login(&app, "ali").await;
    let t1_id = me_id(&app, &t1).await;
    let t2_id = me_id(&app, &t2).await;
    let ali_id = me_id(&app, &ali).await;

    let t = taught(&app, &boss, "algebra").await;
    enroll(&app, &boss, &t.instance, &ali_id).await;
    let res = send(
        &app,
        "POST",
        &format!("/instances/{}/sessions", t.instance),
        Some(&boss),
        Some(json!({ "starts_at": soon(), "teacher_id": t1_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let session = common::id_of(&res.body);
    let mark_uri = format!("/sessions/{session}/attendance");

    // Management records the staff presence of the then-teacher.
    let res = send(
        &app,
        "POST",
        &mark_uri,
        Some(&boss),
        Some(json!({ "status": "present", "user_id": t1_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // The lesson is handed to another ordinary teacher.
    let res = send(
        &app,
        "PATCH",
        &format!("/sessions/{session}"),
        Some(&boss),
        Some(json!({ "teacher_id": t2_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Who may not then wipe the outgoing teacher's staff row.
    let res = send(
        &app,
        "DELETE",
        &format!("{mark_uri}/{t1_id}"),
        Some(&t2),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);

    // The row is still there, and a manager still clears it.
    let rows = session_attendance::list_for_user(&db, &UserId::from_key(&t1_id))
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "the staff row survived the refused delete");
    let res = send(
        &app,
        "DELETE",
        &format!("{mark_uri}/{t1_id}"),
        Some(&boss),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);

    // And the ordinary case is untouched: the session's teacher clears a
    // student's row without management.
    let res = send(
        &app,
        "POST",
        &mark_uri,
        Some(&t2),
        Some(json!({ "status": "present", "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("{mark_uri}/{ali_id}"),
        Some(&t2),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
}

/// No attendance row may outlive its session. Marking held a session snapshot
/// read four round trips before the write, so a delete landing in that gap left
/// a roll-call row pointing at a session that no longer existed — and that row
/// was unremovable, its only delete route 404ing on the vanished session while
/// `GET /attendance/{user}` counted it forever. The existence gate now rides
/// inside the write itself.
///
/// This drives the *sequential* order only — the delete commits in full before
/// the mark is sent — which a read-only gate already survives. What the
/// concurrent order needs is next door: the mark must write the session row, or
/// the two transactions share no key and both commit.
#[tokio::test]
async fn a_mark_against_a_deleted_session_is_refused_and_stores_nothing() {
    let (app, db) = app_and_db().await;
    let mudur = login_as(&app, &db, "mudur", "manager").await;
    let hoca = login_as(&app, &db, "hoca", "teacher").await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;
    let hoca_id = me_id(&app, &hoca).await;
    let ali_ref = UserId::from_key(&ali_id);

    let t = taught_under(&app, &mudur, &hoca, "algebra").await;
    enroll(&app, &hoca, &t.instance, &ali_id).await;
    let session = create_session(&app, &hoca, &t.instance, soon()).await;
    let res = send(
        &app,
        "POST",
        &format!("/sessions/{session}/attendance"),
        Some(&hoca),
        Some(json!({ "status": "present", "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // The snapshot the racing request is holding, taken while the session lives.
    let snapshot = course_session::read(&db, &CourseSessionId::from_key(&session))
        .await
        .unwrap()
        .expect("session exists");

    // The delete lands first — and its cascade still takes the existing row.
    let res = send(
        &app,
        "DELETE",
        &format!("/sessions/{session}"),
        Some(&hoca),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    assert!(
        session_attendance::list_for_user(&db, &ali_ref)
            .await
            .unwrap()
            .is_empty(),
        "the delete cascade removed the roll-call row"
    );

    // Then the mark arrives, still believing in its snapshot.
    let school = settings::load(&db).await.unwrap();
    let status = AttendanceStatus::try_new("present", school.get_attendance_statuses()).unwrap();
    let marked = session_attendance::mark(
        &db,
        &snapshot,
        &ali_ref,
        status,
        &UserId::from_key(&hoca_id),
    )
    .await;
    assert!(
        matches!(marked, Err(AppError::NotFound)),
        "a mark on a deleted session is a 404"
    );
    // The stored state is the assertion — the return value is not trusted.
    assert!(
        session_attendance::list_for_user(&db, &ali_ref)
            .await
            .unwrap()
            .is_empty(),
        "no orphan row was written"
    );
}

/// A roll call must write its session row *every* time, not only when it also
/// credits the lesson. The credit branch — first mark of a lesson that has
/// begun — was the only writer, so the two cases it skips left the session row
/// untouched and a concurrent `DELETE /sessions/{id}` had nothing to collide
/// with: a sheet opened before the bell, and a student with no row yet on a
/// lesson already counted. (A *re-mark* of a row that exists was never in
/// danger — the delete's cascade and the upsert write that child's own key.)
///
/// Both are driven through the real route, and the assertion is the probe's
/// count, which goes to zero the moment the unconditional touch is dropped.
#[tokio::test]
async fn a_mark_writes_its_session_row_even_when_it_credits_nothing() {
    let (app, db) = app_and_db().await;
    let mudur = login_as(&app, &db, "mudur", "manager").await;
    let hoca = login_as(&app, &db, "hoca", "teacher").await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;
    let veli = login(&app, "veli").await;
    let veli_id = me_id(&app, &veli).await;

    let t = taught_under(&app, &mudur, &hoca, "algebra").await;
    enroll(&app, &hoca, &t.instance, &ali_id).await;
    enroll(&app, &hoca, &t.instance, &veli_id).await;
    let session = create_session(&app, &hoca, &t.instance, soon()).await;
    let mark_uri = format!("/sessions/{session}/attendance");

    // (The write-probe below is `queues_on_parent_lock` now.)

    // The lesson has not begun, so nothing is credited — and before the fix
    // nothing at all was written to the session.
    let status = queues_on_parent_lock(
        &db,
        "course_session",
        uuid::Uuid::parse_str(&session).unwrap(),
        {
            let app = app.clone();
            let hoca = hoca.clone();
            let mark_uri = mark_uri.clone();
            let ali = ali_id.clone();
            async move {
                send(
                    &app,
                    "POST",
                    &mark_uri,
                    Some(&hoca),
                    Some(json!({ "status": "present", "user_id": ali })),
                )
                .await
                .status
            }
        },
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a mark on a future-dated lesson still lands"
    );

    // The bell rings and the first roll call counts the lesson, stamping it.
    sqlx::query("UPDATE course_session SET starts_at = 1 WHERE id = $1")
        .bind(CourseSessionId::from_key(&session))
        .execute(&db)
        .await
        .unwrap();
    let res = send(
        &app,
        "POST",
        &mark_uri,
        Some(&hoca),
        Some(json!({ "status": "present", "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // The rest of the roster arrives on a lesson already counted: the credit
    // branch is closed for good, and it was the only writer.
    let status = queues_on_parent_lock(
        &db,
        "course_session",
        uuid::Uuid::parse_str(&session).unwrap(),
        {
            let app = app.clone();
            let hoca = hoca.clone();
            let mark_uri = mark_uri.clone();
            let veli = veli_id.clone();
            async move {
                send(
                    &app,
                    "POST",
                    &mark_uri,
                    Some(&hoca),
                    Some(json!({ "status": "present", "user_id": veli })),
                )
                .await
                .status
            }
        },
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a mark on an already-counted lesson still lands"
    );

    // That the touch is a bump-and-restore rather than a second credit is
    // `the_first_roll_call_counts_the_lesson_once`'s job, next to the SQL.
}

/// The twin, on event attendance, which had neither half: a bare upsert with no
/// proof its event exists at all. A mark racing `DELETE /events/{id}`'s cascade
/// (`DELETE attendance WHERE event = $ev`) was counted forever in the `events`
/// tally of `GET /attendance/{user}`, on an event no page shows.
///
/// Both halves are asserted here: the write on the event row (the collision,
/// probed as above) and the existence gate (the sequential order, driven for
/// real — a mark sent with an id whose event is gone).
#[tokio::test]
async fn an_event_mark_writes_its_event_and_is_refused_once_it_is_gone() {
    let (app, db) = app_and_db().await;
    let hoca = login_as(&app, &db, "hoca", "teacher").await;
    let hoca_ref = UserId::from_key(&me_id(&app, &hoca).await);
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;
    let ali_ref = UserId::from_key(&ali_id);

    let res = send(
        &app,
        "POST",
        "/events",
        Some(&hoca),
        Some(json!({ "title": "Gezi", "audience": { "kind": "role", "role": "student" } })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let event = common::id_of(&res.body);

    let status = queues_on_parent_lock(
        &db,
        "event",
        uuid::Uuid::parse_str(&event).unwrap(),
        {
            let app = app.clone();
            let hoca = hoca.clone();
            let mark_uri = format!("/events/{event}/attendance");
            let ali2 = ali_id.clone();
            async move {
                send(
                    &app,
                    "POST",
                    &mark_uri,
                    Some(&hoca),
                    Some(json!({ "status": "present", "user_id": ali2 })),
                )
                .await
                .status
            }
        },
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the mark lands, queueing on its event row"
    );

    // The event goes, cascade and all.
    let res = send(
        &app,
        "DELETE",
        &format!("/events/{event}"),
        Some(&hoca),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    assert!(
        attendance::list_for_user(&db, &ali_ref)
            .await
            .unwrap()
            .is_empty(),
        "the delete cascade removed the mark"
    );

    // And the mark that arrives after it is refused rather than stored.
    let school = settings::load(&db).await.unwrap();
    let status = AttendanceStatus::try_new("present", school.get_attendance_statuses()).unwrap();
    let marked =
        attendance::mark(&db, &EventId::from_key(&event), &ali_ref, status, &hoca_ref).await;
    assert!(
        matches!(marked, Err(AppError::NotFound)),
        "a mark on a deleted event is a 404, got {marked:?}"
    );
    // The stored state is the assertion — the return value is not trusted.
    assert!(
        attendance::list_for_user(&db, &ali_ref)
            .await
            .unwrap()
            .is_empty(),
        "no orphan row was written"
    );
}

/// #22, session half: a şube sitting in an archived academic year is a past
/// year — its sessions and their roll call are read-only. The guard runs
/// *after* the authorization check (403 before 409, so a stranger never learns
/// a year's state) and off the instance's year, not inside the shared session
/// loader — reads go through that loader too, and reads stay open.
#[tokio::test]
async fn an_archived_year_freezes_its_sessions_and_roll_call() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "arch_sess_manager", "manager").await;
    let ali = login(&app, "arch_sess_ali").await;
    let ali_id = me_id(&app, &ali).await;

    let t = taught(&app, &manager, "Tarih").await;
    enroll(&app, &manager, &t.instance, &ali_id).await;
    let session = create_session(&app, &manager, &t.instance, soon()).await;
    // A roll-call row exists before the freeze, so the delete route reaches the
    // guard rather than a 404 for a missing row.
    let marked = send(
        &app,
        "POST",
        &format!("/sessions/{session}/attendance"),
        Some(&manager),
        Some(json!({ "status": "present", "user_id": ali_id })),
    )
    .await;
    assert_eq!(marked.status, StatusCode::OK, "{}", marked.body);

    // The şube's year goes to the archive. No route does this — a year freezes
    // when the office declares it done — so the stored state is written
    // straight into the store, the same trick the `starts_at` rewrites above
    // use.
    let year = uuid::Uuid::parse_str(&t.year).unwrap();
    sqlx::query("UPDATE academic_year SET archived_at = 1 WHERE id = $1")
        .bind(year)
        .execute(&db)
        .await
        .unwrap();

    // Every write on the session and its roll call refuses with the coded 409.
    let writes: [(&str, String, Option<serde_json::Value>); 4] = [
        (
            "PATCH",
            format!("/sessions/{session}"),
            Some(json!({ "topic": "yeni" })),
        ),
        (
            "POST",
            format!("/sessions/{session}/attendance"),
            Some(json!({ "status": "absent", "user_id": ali_id })),
        ),
        (
            "DELETE",
            format!("/sessions/{session}/attendance/{ali_id}"),
            None,
        ),
        ("DELETE", format!("/sessions/{session}"), None),
    ];
    for (method, uri, body) in writes {
        let res = send(&app, method, &uri, Some(&manager), body).await;
        assert_eq!(
            res.status,
            StatusCode::CONFLICT,
            "{method} {uri}: {}",
            res.body
        );
        assert_eq!(res.body["code"], "academic_year_archived", "{method} {uri}");
    }

    // Reads stay open — a past year is read-only, not hidden.
    for uri in [
        format!("/sessions/{session}"),
        format!("/sessions/{session}/attendance"),
    ] {
        let res = send(&app, "GET", &uri, Some(&manager), None).await;
        assert_eq!(res.status, StatusCode::OK, "GET {uri}: {}", res.body);
    }

    // Re-opening the year thaws the writes again.
    sqlx::query("UPDATE academic_year SET archived_at = NULL WHERE id = $1")
        .bind(year)
        .execute(&db)
        .await
        .unwrap();
    let res = send(
        &app,
        "PATCH",
        &format!("/sessions/{session}"),
        Some(&manager),
        Some(json!({ "topic": "yeni" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}
