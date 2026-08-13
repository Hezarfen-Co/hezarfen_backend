//! One defect in three places: a grant written for a user whose role change is
//! sweeping at that very moment.
//!
//! `User::set_role` lowers the role *and* sheds every grant it implied, in one
//! transaction — but the sweeps are snapshots (`DELETE <child> WHERE user =
//! $usr`), and SurrealDB conflict-checks write sets, not read sets. An event
//! seat, a course enrollment and a class membership each write their *other*
//! parent (the event, the course, the class), so before the fix they shared no
//! key with the demotion and both sides committed: a grant standing under a
//! role that may not hold it, with nothing left to re-sweep. The event seat was
//! the one nothing could free afterwards — `unregister` refuses a non-student
//! target and a parent cannot reach the route at all.
//!
//! Each defect is pinned twice: the ordering the claim enforces (the demotion
//! already committed — the sequential half), and the interleaving itself, held
//! open by a `DEFINE EVENT` on the `user` table so the demotion's transaction is
//! still running when the grant is written.

mod common;

use axum::http::StatusCode;
use common::{app_and_db, login_as, me_id, send};
use hezarfen_backend::database::Database;
use hezarfen_backend::domain::class_group::{ClassGroup, ClassGroupId, ClassName};
use hezarfen_backend::domain::class_member::ClassMember;
use hezarfen_backend::domain::course::CourseId;
use hezarfen_backend::domain::enrollment::Enrollment;
use hezarfen_backend::domain::event::EventId;
use hezarfen_backend::domain::registration::Registration;
use hezarfen_backend::domain::role::Role;
use hezarfen_backend::domain::timestamp::Timestamp;
use hezarfen_backend::domain::user::{User, UserId};
use serde_json::json;

/// How many rows `sql` selects ids for.
async fn rows(sql: &str, db: &Database) -> usize {
    let mut result = db.query(sql).await.unwrap().check().unwrap();
    result
        .take::<Vec<surrealdb::types::RecordId>>(0)
        .unwrap()
        .len()
}

/// Every row's counter added up, re-read out of the store — never off a return
/// value, and summed because the race tests leave a second parent standing (the
/// bait) whose seat has to come back too.
async fn counter(sql: &str, db: &Database) -> i64 {
    let mut result = db.query(sql).await.unwrap().check().unwrap();
    result.take::<Vec<i64>>(0).unwrap().into_iter().sum()
}

/// Demote through the real path — the sweeps ride the role write.
async fn demote(user: &UserId, to: Role, db: &Database) {
    User::read(user, db)
        .await
        .unwrap()
        .expect("the account is there")
        .set_role(to, db)
        .await
        .unwrap();
}

/// Hold the demotion's transaction open *past the point its sweep has looked*:
/// a `DEFINE EVENT` on the child table fires inside the sweep's own `DELETE`,
/// so a grant written during the sleep lands after the snapshot that sweep is
/// working from and before the transaction commits. That is the write-skew
/// itself, opened by the schema rather than by a lucky interleaving.
///
/// It bites only where the sweep has something to delete, so every caller
/// leaves one row of that kind behind first — the bait. Holding the *role*
/// write instead proves nothing: the sweeps run after it, so they see the new
/// row and free it even on unfixed code.
async fn hold_the_sweep(table: &str, db: &Database) {
    db.query(format!(
        "DEFINE EVENT hold_the_sweep ON TABLE {table} WHEN $event = 'DELETE' \
         THEN {{ SLEEP 2s }};"
    ))
    .await
    .unwrap()
    .check()
    .unwrap();
}

/// One held window at a time. Each race test below keeps a database
/// transaction open for seconds while `cap`'s counter lock is process-wide, so
/// run in parallel they push each other clean out of their windows: measured,
/// all three then pass on code with the claim removed, while serialized all
/// three fail it. Same trap as the process-wide `EXAM_LOCK` note in
/// `regress_course_delete`, and the guard is handed back into a binding that
/// lives for the whole test the way `init_test_server`'s does.
static ONE_WINDOW_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Start the demotion and wait until it is *inside* the held window, then hand
/// back the handle to join on.
///
/// The wait is a fixed delay against a two-second window, so a loaded machine
/// still lands in it — and the caller proves it did rather than trusting it:
/// [`still_running`] is asserted the moment the raced write returns. A window
/// missed the other way (the demotion committing first) would make every one of
/// these tests pass on unfixed code, which is the failure mode a race test has
/// to be loudest about.
async fn demote_in_the_window(
    student: &UserId,
    to: Role,
    db: &Database,
) -> tokio::task::JoinHandle<()> {
    let handle = {
        let (student, db) = (student.clone(), db.clone());
        tokio::spawn(async move { demote(&student, to, &db).await })
    };
    // race-window staging — do not convert to poll
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    handle
}

/// The raced write returned while the demotion's transaction was still open —
/// which, since every statement in it but the held one is sub-millisecond, is
/// the window.
fn still_running(demoting: &tokio::task::JoinHandle<()>) {
    assert!(
        !demoting.is_finished(),
        "the demotion committed before the raced write: this run proved nothing"
    );
}

/// A logged-in teacher, a student account, and the ids for both.
async fn a_school(app: &axum::Router, db: &Database) -> (String, UserId) {
    let teacher = login_as(app, db, "ogretmen", "teacher").await;
    let student_cookie = login_as(app, db, "ogrenci", "student").await;
    let student = UserId::from_key(&me_id(app, &student_cookie).await);
    (teacher, student)
}

// ---- defect 1: the event seat ---------------------------------------------

async fn a_registration_event(app: &axum::Router, teacher: &str, capacity: i64) -> EventId {
    let res = send(
        app,
        "POST",
        "/events",
        Some(teacher),
        Some(json!({
            "title": "gezi",
            "starts_at": a_week_out(),
            "audience": { "kind": "registration", "capacity": capacity },
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{:?}", res.body);
    EventId::from_key(res.body["id"].as_str().unwrap())
}

/// Far enough ahead that the signup list is open, in ms.
fn a_week_out() -> i64 {
    Timestamp::now().as_millis() + 7 * 24 * 60 * 60 * 1000
}

/// The sequential half: once the demotion has committed, the seat must be
/// refused. Before the fix the claim only ever asked the *event* whether it had
/// room, so a request that had already read a student took the seat regardless.
#[tokio::test]
async fn a_seat_is_refused_once_the_holder_is_a_parent() {
    let (app, db) = app_and_db().await;
    let (teacher, student) = a_school(&app, &db).await;
    let event = a_registration_event(&app, &teacher, 1).await;
    demote(&student, Role::Parent, &db).await;

    let refused = Registration::register(&event, &student, &UserId::from_key("staff"), &db).await;
    assert!(
        refused.is_err(),
        "a parent may not be given a seat: {refused:?}"
    );
    assert_eq!(rows("SELECT VALUE id FROM registration", &db).await, 0);
    assert_eq!(
        counter("SELECT VALUE registration_count ?? 0 FROM event", &db).await,
        0,
        "…and the event must not count one, or the last place is denied forever"
    );
}

/// The race itself, and the assertion is the seat's *fate*: after a demotion
/// that ran alongside a registration, no seat may be left on a list that
/// nothing can take it off. Either the claim refused it, or the demotion lost
/// the write conflict and its re-sent sweep freed it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seat_never_survives_the_demotion_it_raced() {
    let _serialized = ONE_WINDOW_AT_A_TIME.lock().await;
    let (app, db) = app_and_db().await;
    let (teacher, student) = a_school(&app, &db).await;
    let event = a_registration_event(&app, &teacher, 1).await;
    // The bait: a seat the sweep will delete, which is what holds it open.
    let earlier = a_registration_event(&app, &teacher, 1).await;
    Registration::register(&earlier, &student, &UserId::from_key("staff"), &db)
        .await
        .unwrap();
    hold_the_sweep("registration", &db).await;

    // The seat is written inside the held window: past the sweep's snapshot,
    // before the commit.
    let demoting = demote_in_the_window(&student, Role::Parent, &db).await;
    let seat = Registration::register(&event, &student, &UserId::from_key("staff"), &db).await;
    still_running(&demoting);
    demoting.await.unwrap();

    assert!(
        !matches!(seat, Err(hezarfen_backend::error::AppError::Db(_))),
        "a raced registration must be answered, not 500: {seat:?}"
    );
    assert_eq!(
        rows("SELECT VALUE id FROM registration", &db).await,
        0,
        "the seat a parent cannot free must not exist ({seat:?})"
    );
    assert_eq!(
        counter("SELECT VALUE registration_count ?? 0 FROM event", &db).await,
        0,
        "…and its place must be back on the list"
    );
}

/// The stale half, and the reason it is not merely a preventable race: a seat
/// an *earlier* build stranded is already in the volume. Freeing it is a route
/// a teacher can reach — `unregister` bars staff seats, not a parent's.
#[tokio::test]
async fn a_stranded_parent_seat_is_freeable_by_a_teacher() {
    let (app, db) = app_and_db().await;
    let (teacher, student) = a_school(&app, &db).await;
    let event = a_registration_event(&app, &teacher, 1).await;
    Registration::register(&event, &student, &UserId::from_key("staff"), &db)
        .await
        .unwrap();
    // The state the old race left: the seat stands, the account is a parent,
    // and the sweep that should have taken it never saw it.
    db.query("UPDATE $usr SET role = 'parent'")
        .bind(("usr", student.record()))
        .await
        .unwrap()
        .check()
        .unwrap();

    let freed = send(
        &app,
        "DELETE",
        &format!("/events/{}/register/{}", event.key(), student.key()),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(
        freed.status,
        StatusCode::NO_CONTENT,
        "a stranded parent's seat must be freeable: {:?}",
        freed.body
    );
    assert_eq!(
        counter("SELECT VALUE registration_count ?? 0 FROM event", &db).await,
        0,
        "the place comes back with the row"
    );
}

// ---- defect 2: the course enrollment ---------------------------------------

async fn a_course(app: &axum::Router, teacher: &str) -> CourseId {
    CourseId::from_key(&common::create_course(app, teacher, "cebir").await)
}

#[tokio::test]
async fn an_enrollment_is_refused_once_the_student_is_demoted() {
    let (app, db) = app_and_db().await;
    let (teacher, student) = a_school(&app, &db).await;
    let course = a_course(&app, &teacher).await;
    demote(&student, Role::Teacher, &db).await;

    let refused = Enrollment::enroll(&course, &student, &UserId::from_key("staff"), &db).await;
    assert!(
        refused.is_err(),
        "only students hold enrollments: {refused:?}"
    );
    assert_eq!(rows("SELECT VALUE id FROM enrollment", &db).await, 0);
    assert_eq!(
        counter("SELECT VALUE enrollment_count ?? 0 FROM course", &db).await,
        0,
        "…and a counted seat would also freeze the course's delete guard"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_enrollment_never_survives_the_demotion_it_raced() {
    let _serialized = ONE_WINDOW_AT_A_TIME.lock().await;
    let (app, db) = app_and_db().await;
    let (teacher, student) = a_school(&app, &db).await;
    let course = a_course(&app, &teacher).await;
    // The bait: a row the sweep will delete, which is what holds it open.
    let earlier = CourseId::from_key(&common::create_course(&app, &teacher, "fizik").await);
    Enrollment::enroll(&earlier, &student, &UserId::from_key("staff"), &db)
        .await
        .unwrap();
    hold_the_sweep("enrollment", &db).await;

    let demoting = demote_in_the_window(&student, Role::Teacher, &db).await;
    let enrolled = Enrollment::enroll(&course, &student, &UserId::from_key("staff"), &db).await;
    still_running(&demoting);
    demoting.await.unwrap();

    assert_eq!(
        rows("SELECT VALUE id FROM enrollment", &db).await,
        0,
        "a teacher may not be left on a course roster ({enrolled:?})"
    );
    assert_eq!(
        counter("SELECT VALUE enrollment_count ?? 0 FROM course", &db).await,
        0
    );
}

// ---- defect 3: the class membership ----------------------------------------

async fn a_class(db: &Database) -> ClassGroupId {
    a_class_named("9-A", db).await
}

async fn a_class_named(name: &str, db: &Database) -> ClassGroupId {
    ClassGroup::create(
        &UserId::from_key("manager"),
        ClassName::try_new(name).unwrap(),
        None,
        None,
        None,
        db,
    )
    .await
    .unwrap()
    .get_id()
    .clone()
}

#[tokio::test]
async fn a_membership_is_refused_once_the_student_is_demoted() {
    let (app, db) = app_and_db().await;
    let (_teacher, student) = a_school(&app, &db).await;
    let class = a_class(&db).await;
    demote(&student, Role::Parent, &db).await;

    let refused = ClassMember::add(&class, &student, &UserId::from_key("manager"), &db).await;
    assert!(
        refused.is_err(),
        "only students belong to a class: {refused:?}"
    );
    assert_eq!(rows("SELECT VALUE id FROM class_member", &db).await, 0);
    assert_eq!(
        counter("SELECT VALUE class_member_count ?? 0 FROM class_group", &db).await,
        0,
        "…and a counted member would freeze the class's delete guard"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_membership_never_survives_the_demotion_it_raced() {
    let _serialized = ONE_WINDOW_AT_A_TIME.lock().await;
    let (app, db) = app_and_db().await;
    let (_teacher, student) = a_school(&app, &db).await;
    let class = a_class(&db).await;
    // The bait: a membership the sweep will delete, which holds it open.
    let earlier = a_class_named("9-B", &db).await;
    ClassMember::add(&earlier, &student, &UserId::from_key("manager"), &db)
        .await
        .unwrap();
    hold_the_sweep("class_member", &db).await;

    let demoting = demote_in_the_window(&student, Role::Parent, &db).await;
    let joined = ClassMember::add(&class, &student, &UserId::from_key("manager"), &db).await;
    still_running(&demoting);
    demoting.await.unwrap();

    assert_eq!(
        rows("SELECT VALUE id FROM class_member", &db).await,
        0,
        "a parent may not be left on a class roster ({joined:?})"
    );
    assert_eq!(
        counter("SELECT VALUE class_member_count ?? 0 FROM class_group", &db).await,
        0
    );
}
