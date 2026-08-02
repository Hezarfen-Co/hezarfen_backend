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
use common::{app_and_db, create_course, create_session, enroll, login, login_as, me_id, send};
use hezarfen_backend::domain::attendance::AttendanceStatus;
use hezarfen_backend::domain::course_session::{CourseSession, CourseSessionId};
use hezarfen_backend::domain::session_attendance::SessionAttendance;
use hezarfen_backend::domain::settings::Settings;
use hezarfen_backend::domain::timestamp::Timestamp;
use hezarfen_backend::domain::user::UserId;
use hezarfen_backend::error::AppError;
use serde_json::json;

fn soon() -> i64 {
    Timestamp::now().as_millis() + 3_600_000
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

    let course = create_course(&app, &boss, "algebra").await;
    enroll(&app, &boss, &course, &ali_id).await;
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/sessions"),
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
    let rows = SessionAttendance::list_for_user(&UserId::from_key(&t1_id), &db)
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
#[tokio::test]
async fn a_mark_against_a_deleted_session_is_refused_and_stores_nothing() {
    let (app, db) = app_and_db().await;
    let hoca = login_as(&app, &db, "hoca", "teacher").await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;
    let hoca_id = me_id(&app, &hoca).await;
    let ali_ref = UserId::from_key(&ali_id);

    let course = create_course(&app, &hoca, "algebra").await;
    enroll(&app, &hoca, &course, &ali_id).await;
    let session = create_session(&app, &hoca, &course, soon()).await;
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
    let snapshot = CourseSession::read(&CourseSessionId::from_key(&session), &db)
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
        SessionAttendance::list_for_user(&ali_ref, &db)
            .await
            .unwrap()
            .is_empty(),
        "the delete cascade removed the roll-call row"
    );

    // Then the mark arrives, still believing in its snapshot.
    let school = Settings::load(&db).await.unwrap();
    let status = AttendanceStatus::try_new("present", school.get_attendance_statuses()).unwrap();
    let marked = SessionAttendance::mark(
        &snapshot,
        &ali_ref,
        status,
        &UserId::from_key(&hoca_id),
        &db,
    )
    .await;
    assert!(
        matches!(marked, Err(AppError::NotFound)),
        "a mark on a deleted session is a 404"
    );
    // The stored state is the assertion — the return value is not trusted.
    assert!(
        SessionAttendance::list_for_user(&ali_ref, &db)
            .await
            .unwrap()
            .is_empty(),
        "no orphan row was written"
    );
}
