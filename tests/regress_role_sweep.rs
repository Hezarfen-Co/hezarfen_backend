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
//! open by an AFTER DELETE trigger sleeping inside the sweep, so the demotion's
//! transaction is still running when the grant is written.

mod common;

use axum::http::StatusCode;
use common::{app_and_db, login_as, me_id, send, set_role};
use hezarfen_backend::database::Database;
use hezarfen_backend::db::enrollment;
use hezarfen_backend::domain::class_course::ClassCourseId;
use hezarfen_backend::domain::class_group::{ClassGroupId, ClassName};
use hezarfen_backend::domain::grade::GradeLevel;
use hezarfen_backend::domain::event::EventId;
use hezarfen_backend::domain::role::Role;
use hezarfen_backend::domain::timestamp::Timestamp;
use hezarfen_backend::domain::user::UserId;
use hezarfen_backend::service::registration;
use hezarfen_backend::service::{class_group, class_member};
use serde_json::json;
use sqlx::Row as _;

/// How many rows `table` holds.
async fn rows(table: &str, db: &Database) -> usize {
    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
        .fetch_one(db)
        .await
        .unwrap() as usize
}

/// The *live* class memberships — unstamped stints only. The demotion's sweep
/// is soft since the K12 remodel (the row is stamped `left_at`, not deleted),
/// so a total count no longer answers "is anyone still on a roster".
async fn live_memberships(db: &Database) -> usize {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM class_member WHERE left_at IS NULL")
        .fetch_one(db)
        .await
        .unwrap() as usize
}

/// Every row's counter added up, re-read out of the store — never off a return
/// value, and summed because the race tests leave a second parent standing (the
/// bait) whose seat has to come back too.
async fn counter(column: &str, table: &str, db: &Database) -> i64 {
    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
        "SELECT COALESCE(sum({column}), 0)::bigint FROM {table}"
    )))
    .fetch_one(db)
    .await
    .unwrap()
}

/// A real `app_user` row for the fixture actor a domain call must name:
/// every actor column is a foreign key now, so actors are rows, not fabricated
/// ids. The username is unique, so every call for the same name shares one row,
/// whichever call minted it. These actors never log in, so the hash is a stub.
async fn fixture_actor(db: &Database, username: &str) -> UserId {
    sqlx::query(
        "INSERT INTO app_user (id, username, created_at, role) \
         VALUES ($1, $2, 0, 'student') ON CONFLICT DO NOTHING",
    )
    .bind(UserId::generate().uuid())
    .bind(username)
    .execute(db)
    .await
    .unwrap();
    let id: uuid::Uuid = sqlx::query("SELECT id FROM app_user WHERE username = $1")
        .bind(username)
        .fetch_one(db)
        .await
        .unwrap()
        .try_get(0)
        .unwrap();
    UserId::from_key(&id.to_string())
}

/// Demote through the real path — the sweeps ride the role write.
async fn demote(user: &UserId, to: Role, db: &Database) {
    hezarfen_backend::db::user::read(db, user)
        .await
        .unwrap()
        .expect("the account is there");
    hezarfen_backend::service::user::set_role(db, user, to)
        .await
        .unwrap();
}

/// Hold the demotion's transaction open *past the point its sweep has looked*:
/// an AFTER DELETE trigger on the child table sleeps inside the sweep's own
/// `DELETE`, so a grant written during the sleep lands after the snapshot that
/// sweep is working from and before the transaction commits. That is the
/// write-skew itself, opened by the schema rather than by a lucky interleaving.
///
/// It bites only where the sweep has something to delete, so every caller
/// leaves one row of that kind behind first — the bait. Holding the *role*
/// write instead proves nothing: the sweeps run after it, so they see the new
/// row and free it even on unfixed code.
async fn hold_the_sweep(table: &str, db: &Database) {
    let mut conn = db.acquire().await.expect("acquire for the trigger");
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE FUNCTION heztest_hold_the_sweep() RETURNS trigger AS $$
         BEGIN PERFORM pg_sleep(2.0); RETURN NULL; END;
         $$ LANGUAGE plpgsql;
         CREATE TRIGGER heztest_hold_the_sweep AFTER DELETE ON {table}
         FOR EACH ROW EXECUTE FUNCTION heztest_hold_the_sweep();"
    )))
    .execute(&mut *conn)
    .await
    .expect("define the window trigger");
}

/// One held window at a time. Each race test below holds a database
/// transaction open for seconds against the shared Postgres server, so run in
/// parallel they push each other clean out of their windows: measured, all
/// three then pass on code with the claim removed, while serialized all three
/// fail it. Same trap as two race suites sharing one process-wide lock across
/// unrelated databases: the neighbour holds the lease through its window, this
/// start queues, and the timeline stops holding. The guard is handed back into
/// a binding that lives for the whole test the way `init_test_server`'s does.
static ONE_WINDOW_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Start the demotion and wait until it is *inside* the held window, then hand
/// back the handle to join on.
///
/// The wait is a fixed delay against a two-second window, so a loaded machine
/// still lands in it — and the caller proves it did rather than trusting it:
/// [`still_queued`] is asserted while the demotion runs. A window
/// missed the other way (the demotion committing first) would make every one of
/// these tests pass on unfixed code, which is the failure mode a race test has
/// to be loudest about.
async fn demote_in_the_window(
    student: &UserId,
    to: Role,
    db: &Database,
) -> tokio::task::JoinHandle<()> {
    let handle = {
        let (student, db) = (*student, db.clone());
        tokio::spawn(async move { demote(&student, to, &db).await })
    };
    // race-window staging — do not convert to poll
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    handle
}

/// The raced write is *queued*, not answered: the claim locks the user row the
/// demotion is holding, so a raced write that answered inside the window —
/// which is what unfixed code, sharing no key with the demotion, did — fails
/// here instead of in the stored-state assertions.
async fn still_queued<T>(raced: &tokio::task::JoinHandle<T>) {
    // race-window staging — do not convert to poll
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(
        !raced.is_finished(),
        "the raced write answered inside the demotion's window: nothing queued"
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

    let refused =
        registration::register(&db, &event, &student, &fixture_actor(&db, "staff").await).await;
    assert!(
        refused.is_err(),
        "a parent may not be given a seat: {refused:?}"
    );
    assert_eq!(rows("registration", &db).await, 0);
    assert_eq!(
        counter("registration_count", "event", &db).await,
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
    registration::register(&db, &earlier, &student, &fixture_actor(&db, "staff").await)
        .await
        .unwrap();
    hold_the_sweep("registration", &db).await;

    // The grant is written while the demotion holds its window: the claim
    // queues on the demoted user's row, past the sweep's snapshot.
    let demoting = demote_in_the_window(&student, Role::Parent, &db).await;
    let seat = {
        let (db, event) = (db.clone(), event);
        let staff = fixture_actor(&db, "staff").await;
        tokio::spawn(async move { registration::register(&db, &event, &student, &staff).await })
    };
    still_queued(&seat).await;
    demoting.await.unwrap();
    let seat = seat.await.expect("raced registration task");

    assert!(
        !matches!(seat, Err(hezarfen_backend::error::AppError::Db(_))),
        "a raced registration must be answered, not 500: {seat:?}"
    );
    assert_eq!(
        rows("registration", &db).await,
        0,
        "the seat a parent cannot free must not exist ({seat:?})"
    );
    assert_eq!(
        counter("registration_count", "event", &db).await,
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
    registration::register(&db, &event, &student, &fixture_actor(&db, "staff").await)
        .await
        .unwrap();
    // The state the old race left: the seat stands, the account is a parent,
    // and the sweep that should have taken it never saw it. `set_role` is the
    // harness's bare role write — no cascade, which is exactly the staleness.
    set_role(&db, "ogrenci", "parent").await;

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
        counter("registration_count", "event", &db).await,
        0,
        "the place comes back with the row"
    );
}

// ---- defect 2: the course enrollment ---------------------------------------
//
// The K12 remodel keys the roster on the *instance* — one şube teaching one
// catalog course — so the grant these probes race is a `class_course` row's
// roster seat, and its counter lives on that row.

/// The instance the enrollment races key on: a manager mints the şube (school
/// structure), attaches the catalog course, and hands back the pair's id.
async fn an_instance(app: &axum::Router, db: &Database, title: &str) -> ClassCourseId {
    let mudur = login_as(app, db, "mudur", "manager").await;
    ClassCourseId::from_key(&common::taught(app, &mudur, title).await.instance)
}

#[tokio::test]
async fn an_enrollment_is_refused_once_the_student_is_demoted() {
    let (app, db) = app_and_db().await;
    let (_teacher, student) = a_school(&app, &db).await;
    let instance = an_instance(&app, &db, "cebir").await;
    demote(&student, Role::Teacher, &db).await;

    let staff = fixture_actor(&db, "staff").await;
    let refused = enrollment::enroll(&db, &instance, &student, &staff, None).await;
    assert!(
        refused.is_err(),
        "only students hold enrollments: {refused:?}"
    );
    assert_eq!(rows("enrollment", &db).await, 0);
    assert_eq!(
        counter("enrollment_count", "class_course", &db).await,
        0,
        "…and a counted seat would also freeze the instance's detach guard"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_enrollment_never_survives_the_demotion_it_raced() {
    let _serialized = ONE_WINDOW_AT_A_TIME.lock().await;
    let (app, db) = app_and_db().await;
    let (_teacher, student) = a_school(&app, &db).await;
    let instance = an_instance(&app, &db, "cebir").await;
    // The bait: a row the sweep will delete, which is what holds it open.
    let earlier = an_instance(&app, &db, "fizik").await;
    let staff = fixture_actor(&db, "staff").await;
    enrollment::enroll(&db, &earlier, &student, &staff, None)
        .await
        .unwrap();
    hold_the_sweep("enrollment", &db).await;

    let demoting = demote_in_the_window(&student, Role::Teacher, &db).await;
    let enrolled = {
        let (db, instance) = (db.clone(), instance);
        tokio::spawn(
            async move { enrollment::enroll(&db, &instance, &student, &staff, None).await },
        )
    };
    still_queued(&enrolled).await;
    demoting.await.unwrap();
    let enrolled = enrolled.await.expect("raced enrollment task");

    assert_eq!(
        rows("enrollment", &db).await,
        0,
        "a teacher may not be left on an instance roster ({enrolled:?})"
    );
    assert_eq!(counter("enrollment_count", "class_course", &db).await, 0);
}

// ---- defect 3: the class membership ----------------------------------------

async fn a_class(db: &Database) -> ClassGroupId {
    a_class_named("9-A", db).await
}

async fn a_class_named(name: &str, db: &Database) -> ClassGroupId {
    let manager = fixture_actor(db, "manager").await;
    class_group::create(
        db,
        &manager,
        ClassName::try_new(name).unwrap(),
        GradeLevel::new(9).unwrap(),
        None,
        None,
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

    let manager = fixture_actor(&db, "manager").await;
    let refused = class_member::add(&db, &class, &student, &manager).await;
    assert!(
        refused.is_err(),
        "only students belong to a class: {refused:?}"
    );
    assert_eq!(rows("class_member", &db).await, 0);
    assert_eq!(
        counter("class_member_count", "class_group", &db).await,
        0,
        "…and a counted member would freeze the class's delete guard"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_membership_never_survives_the_demotion_it_raced() {
    let _serialized = ONE_WINDOW_AT_A_TIME.lock().await;
    let (app, db) = app_and_db().await;
    let (teacher, student) = a_school(&app, &db).await;
    let class = a_class(&db).await;
    // The bait: a row the sweep *deletes*, which is what holds the window
    // open. A class membership alone would not do since the K12 remodel — the
    // demotion stamps those (`left_at`) rather than deleting them — so the
    // bait is the enrollment row the şube's pump wrote when the student
    // joined a class that carries a course.
    let earlier = a_class_named("9-B", &db).await;
    let manager = fixture_actor(&db, "manager").await;
    let algebra = common::create_course(&app, &teacher, "algebra").await;
    common::attach_instance(&app, &teacher, &earlier.key(), &algebra).await;
    class_member::add(&db, &earlier, &student, &manager)
        .await
        .unwrap();
    hold_the_sweep("enrollment", &db).await;

    let demoting = demote_in_the_window(&student, Role::Parent, &db).await;
    let joined = {
        let (db, class) = (db.clone(), class);
        tokio::spawn(async move { class_member::add(&db, &class, &student, &manager).await })
    };
    still_queued(&joined).await;
    demoting.await.unwrap();
    let joined = joined.await.expect("raced membership task");

    assert_eq!(
        live_memberships(&db).await,
        0,
        "a parent may not be left on a class roster ({joined:?})"
    );
    assert_eq!(
        rows("class_member", &db).await,
        1,
        "…while the stint itself is stamped, not deleted: history survives"
    );
    assert_eq!(counter("class_member_count", "class_group", &db).await, 0);
}

// ---- who may grade where ----------------------------------------------------

/// Record a mark for `student_id` on `exam` as `cookie`; the status alone is
/// what this probe reads.
async fn grade(app: &axum::Router, cookie: &str, exam: &str, student_id: &str) -> StatusCode {
    send(
        app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(cookie),
        Some(json!({ "mark": 85, "user_id": student_id })),
    )
    .await
    .status
}

/// Grading is an *instance* right since the K12 remodel, not a course one: a
/// teacher assigned to one instance grades there and nowhere else, while the
/// şube's own homeroom teacher grades every instance their section carries.
/// Both answers come out of `ensure_instance_teacher`, which is what this pins
/// — the grant must be read per instance, never per catalog course, or a
/// teacher given 5-A's Matematik could grade 5-B's.
#[tokio::test]
async fn grading_rights_are_per_instance_and_per_subes_homeroom() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "manager", "manager").await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let homeroom = login_as(&app, &db, "homeroom", "teacher").await;
    let student = login_as(&app, &db, "student", "student").await;
    let teacher_id = me_id(&app, &teacher).await;
    let homeroom_id = me_id(&app, &homeroom).await;
    let student_id = me_id(&app, &student).await;

    let year = common::create_year(&app, &manager, "2026-2027").await;
    let term = common::create_term(&app, &manager, &year, "1. Dönem").await;
    let maths = common::create_course(&app, &manager, "Matematik").await;
    let physics = common::create_course(&app, &manager, "Fizik").await;

    // 5-A carries a homeroom teacher and two courses; 5-B carries none and
    // teaches one of the same two — so the same catalog course is taught by
    // both sections, as its own instance each.
    let a = common::create_class(
        &app,
        &manager,
        "5-A",
        json!({ "year": year, "teacher_id": homeroom_id }),
    )
    .await;
    let b = common::create_class(&app, &manager, "5-B", json!({ "year": year })).await;
    let x = common::attach_instance(&app, &manager, &a, &maths).await;
    let z = common::attach_instance(&app, &manager, &a, &physics).await;
    let y = common::attach_instance(&app, &manager, &b, &maths).await;
    common::add_member(&app, &manager, &a, &student_id).await;
    common::add_member(&app, &manager, &b, &student_id).await;

    // One exam per instance, written by the office, so every later answer is
    // about the *grader* and not about who wrote the exam.
    let exam_x = common::create_exam(&app, &manager, &x, &term, "1. Yazılı", "yazili").await;
    let exam_z = common::create_exam(&app, &manager, &z, &term, "1. Yazılı", "yazili").await;
    let exam_y = common::create_exam(&app, &manager, &y, &term, "1. Yazılı", "yazili").await;

    // The teacher runs 5-A's Matematik and nothing else.
    let assigned = send(
        &app,
        "POST",
        &format!("/instances/{x}/teachers"),
        Some(&manager),
        Some(json!({ "user_id": teacher_id })),
    )
    .await;
    assert_eq!(assigned.status, StatusCode::OK, "{:?}", assigned.body);

    assert_eq!(
        grade(&app, &teacher, &exam_x, &student_id).await,
        StatusCode::OK,
        "an assigned teacher grades in their own instance"
    );
    let own = send(
        &app,
        "POST",
        &format!("/instances/{x}/exams"),
        Some(&teacher),
        Some(json!({ "title": "2. Yazılı", "kind": "yazili", "term": term })),
    )
    .await;
    assert_eq!(own.status, StatusCode::CREATED, "{:?}", own.body);

    assert_eq!(
        grade(&app, &teacher, &exam_z, &student_id).await,
        StatusCode::FORBIDDEN,
        "the sibling course of the same şube is not theirs"
    );
    assert_eq!(
        grade(&app, &teacher, &exam_y, &student_id).await,
        StatusCode::FORBIDDEN,
        "the same catalog course in another şube is not theirs"
    );

    // The homeroom teacher needs no assignment: every instance their section
    // carries is theirs — and only their section's.
    assert_eq!(
        grade(&app, &homeroom, &exam_x, &student_id).await,
        StatusCode::OK
    );
    assert_eq!(
        grade(&app, &homeroom, &exam_z, &student_id).await,
        StatusCode::OK,
        "any instance of their own şube"
    );
    assert_eq!(
        grade(&app, &homeroom, &exam_y, &student_id).await,
        StatusCode::FORBIDDEN,
        "…but another section's instance is not"
    );
}
