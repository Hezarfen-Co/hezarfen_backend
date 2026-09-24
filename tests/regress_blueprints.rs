//! Grade blueprints: the course template a whole grade's class sections are
//! stocked from.
//!
//! Every assertion reads STORED state out of the database rather than a
//! response body — the in-memory engine forges wins, and a blueprint's whole
//! job is what ends up in `class_course` and `enrollment`.
//!
//! The three rules under test are the ones that make this layer more than a
//! loop: an edit retro-pumps the classes that already exist, a class that does
//! not fit is *skipped and reported* rather than aborting the others, and a
//! removal reaches only the attachments the blueprint itself made.

mod common;

use axum::http::StatusCode;
use common::{
    FIXTURE_YEAR_ENDS_AT, GHOST_ID, Res, app_and_db, blob_dir, create_course, create_exam,
    create_homework, create_subject, create_term, create_year, id_of, login_as, me_id, send,
    upload_file_at,
};
use hezarfen_backend::database::Database;
use hezarfen_backend::domain::grade::GradeLevel;
use hezarfen_backend::domain::course::CourseId;
use hezarfen_backend::domain::monotonic_id::next_uuid;
use hezarfen_backend::domain::timestamp::Timestamp;
use hezarfen_backend::domain::user::UserId;
use hezarfen_backend::error::AppError;
use hezarfen_backend::service::class_blueprint;
use serde_json::{Value, json};

/// One counter, re-read out of the store. `sql` is a whole scalar query.
async fn counter(sql: &'static str, db: &Database) -> i64 {
    sqlx::query_scalar(sql).fetch_one(db).await.unwrap()
}

/// How many rows `sql` counts.
async fn rows(sql: &'static str, db: &Database) -> i64 {
    counter(sql, db).await
}

/// One row's counter column, re-read out of the store.
async fn count_on(column: &'static str, table: &'static str, id: &str, db: &Database) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT {column} FROM {table} WHERE id = $1"
    )))
    .bind(uuid::Uuid::parse_str(id).expect("a uuid row id"))
    .fetch_one(db)
    .await
    .unwrap()
}

/// Is that course on that class, in the store?
async fn attached(class: &str, course: &str, db: &Database) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM class_course WHERE class = $1 AND course = $2",
    )
    .bind(uuid::Uuid::parse_str(class).expect("a uuid class id"))
    .bind(uuid::Uuid::parse_str(course).expect("a uuid course id"))
    .fetch_one(db)
    .await
    .unwrap()
        == 1
}

/// The blueprint id stored on one attachment, or `None` when the row carries
/// no such key at all — which is the whole provenance rule: absent means a
/// human attached it.
async fn source_of(class: &str, course: &str, db: &Database) -> Option<uuid::Uuid> {
    sqlx::query_scalar::<_, Option<uuid::Uuid>>(
        "SELECT source FROM class_course WHERE class = $1 AND course = $2",
    )
    .bind(uuid::Uuid::parse_str(class).expect("a uuid class id"))
    .bind(uuid::Uuid::parse_str(course).expect("a uuid course id"))
    .fetch_one(db)
    .await
    .unwrap()
}

async fn blueprint_id(grade_level: i16, db: &Database) -> uuid::Uuid {
    sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT id FROM class_blueprint WHERE grade_level = $1",
    )
    .bind(grade_level)
        .fetch_one(db)
        .await
        .unwrap()
}

/// Does that grade's template still name that course, in the store?
async fn templated(grade_level: i16, course: &str, db: &Database) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM blueprint_course bc \
         JOIN class_blueprint b ON b.id = bc.blueprint \
         WHERE b.grade_level = $1 AND bc.course = $2",
    )
    .bind(grade_level)
    .bind(CourseId::from_key(course))
    .fetch_one(db)
    .await
    .unwrap()
        == 1
}

/// The course list one grade's template hands back over HTTP, ready to be sent
/// straight back at it — the self-heal `PATCH` a manager is told to make.
async fn held(app: &axum::Router, cookie: &str, grade: &str) -> Vec<Value> {
    let res = send(
        app,
        "GET",
        &format!("/classes/blueprints/{grade}"),
        Some(cookie),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{:?}", res.body);
    res.body["courses"].as_array().unwrap().clone()
}

/// Create a class section as `cookie`; returns its id.
async fn create_class(
    app: &axum::Router,
    cookie: &str,
    name: &str,
    grade_level: i16,
) -> String {
    let res = send(
        app,
        "POST",
        "/classes",
        Some(cookie),
        Some(json!({ "name": name, "grade_level": grade_level })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "create class {name}");
    res.body["class"]["id"]
        .as_str()
        .expect("class id")
        .to_string()
}

/// Register a student, put them in a class, and hand back their id.
async fn student_in(
    app: &axum::Router,
    db: &Database,
    class: &str,
    manager: &str,
    name: &str,
) -> String {
    let cookie = login_as(app, db, name, "student").await;
    let id = me_id(app, &cookie).await;
    let res = send(
        app,
        "POST",
        &format!("/classes/{class}/members"),
        Some(manager),
        Some(json!({ "user_id": id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "add {name} to {class}");
    id
}

/// The instance id of that (class, course) pair, as text — the anchor every
/// `enrollment` row and every per-instance counter keys on. A pair the store
/// does not carry is a bug in the test's own setup, so it panics.
async fn instance_of(class: &str, course: &str, db: &Database) -> String {
    sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT id FROM class_course WHERE class = $1 AND course = $2",
    )
    .bind(uuid_of(class))
    .bind(CourseId::from_key(course))
    .fetch_one(db)
    .await
    .unwrap()
    .to_string()
}

/// A row id as the uuid every query here binds.
fn uuid_of(id: &str) -> uuid::Uuid {
    uuid::Uuid::parse_str(id).expect("a uuid row id")
}

fn skips(res: &Res) -> &Vec<Value> {
    res.body["skipped"].as_array().expect("a skip list")
}

/// One section's entry in a `GET /classes/blueprints/{grade}/status` body.
fn section<'a>(res: &'a Res, class: &str) -> &'a Value {
    res.body["sections"]
        .as_array()
        .expect("a section list")
        .iter()
        .find(|section| section["class"] == class)
        .unwrap_or_else(|| panic!("no section {class} in {:?}", res.body))
}

/// The courses a section is short, as plain strings.
fn missing(res: &Res, class: &str) -> Vec<String> {
    section(res, class)["missing"]
        .as_array()
        .expect("a missing list")
        .iter()
        .map(|course| course.as_str().expect("a course id").to_string())
        .collect()
}

/// The whole CRUD surface, and the 409s that keep one blueprint per grade.
#[tokio::test]
async fn a_blueprint_is_created_read_updated_and_deleted() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let algebra = create_course(&app, &manager, "algebra").await;
    let physics = create_course(&app, &manager, "physics").await;

    let made = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade_level": 9, "course_ids": [algebra.clone()] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);
    assert_eq!(made.body["blueprint"]["grade_level"], 9);
    assert_eq!(made.body["blueprint"]["courses"][0], algebra.as_str());
    assert!(skips(&made).is_empty(), "no class exists to skip");

    // The grade is the record key, so a second blueprint for it is a 409 the
    // store decides rather than a find-then-insert two managers can race.
    let again = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade_level": 9, "course_ids": [] })),
    )
    .await;
    assert_eq!(again.status, StatusCode::CONFLICT, "{:?}", again.body);

    let read = send(&app, "GET", "/classes/blueprints/9", Some(&manager), None).await;
    assert_eq!(read.status, StatusCode::OK);
    assert_eq!(read.body["courses"][0], algebra.as_str());

    let listed = send(&app, "GET", "/classes/blueprints", Some(&manager), None).await;
    assert_eq!(listed.status, StatusCode::OK);
    assert_eq!(listed.body["items"].as_array().unwrap().len(), 1);

    let patched = send(
        &app,
        "PATCH",
        "/classes/blueprints/9",
        Some(&manager),
        Some(json!({ "course_ids": [physics.clone()] })),
    )
    .await;
    assert_eq!(patched.status, StatusCode::OK, "{:?}", patched.body);
    assert_eq!(patched.body["blueprint"]["courses"][0], physics.as_str());
    assert_eq!(
        patched.body["blueprint"]["courses"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "the list is a set, not a delta — algebra is gone"
    );

    let dropped = send(
        &app,
        "DELETE",
        "/classes/blueprints/9",
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(dropped.status, StatusCode::NO_CONTENT);
    assert_eq!(rows("SELECT count(*) FROM class_blueprint", &db).await, 0);
    let gone = send(&app, "GET", "/classes/blueprints/9", Some(&manager), None).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);

    // An off-ladder rung is a 400, not a record nobody can ever read back.
    for bad in [-1, 13, 99] {
        let refused = send(
            &app,
            "POST",
            "/classes/blueprints",
            Some(&manager),
            Some(json!({ "grade_level": bad, "course_ids": [] })),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "grade_level {bad}: {:?}",
            refused.body
        );
    }
}

/// A section made after the template exists is stocked in one call, and the
/// stocking is the ordinary pump: real enrollment rows for its roster.
#[tokio::test]
async fn a_fresh_class_takes_its_grades_blueprint() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let algebra = create_course(&app, &manager, "algebra").await;
    let physics = create_course(&app, &manager, "physics").await;
    let made = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade_level": 9, "course_ids": [algebra.clone(), physics.clone()] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED);

    let class = create_class(&app, &manager, "9-A", 9).await;
    let student = student_in(&app, &db, &class, &manager, "ali").await;

    let applied = send(
        &app,
        "POST",
        &format!("/classes/{class}/blueprint"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(applied.status, StatusCode::OK, "{:?}", applied.body);
    assert!(skips(&applied).is_empty(), "{:?}", applied.body);

    for course in [&algebra, &physics] {
        assert!(attached(&class, course, &db).await);
        assert_eq!(
            source_of(&class, course, &db).await,
            Some(blueprint_id(9, &db).await),
            "the blueprint must own what it attached"
        );
        let instance = instance_of(&class, course, &db).await;
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM enrollment WHERE class_course = $1 AND app_user = $2",
            )
            .bind(uuid_of(&instance))
            .bind(UserId::from_key(&student))
            .fetch_one(&db)
            .await
            .unwrap(),
            1,
            "stocking a class enrolls its roster, in real rows"
        );
    }
    assert_eq!(
        count_on("class_course_count", "class_group", &class, &db).await,
        2
    );

    // Idempotent: a second apply attaches nothing new and skips nothing.
    let twice = send(
        &app,
        "POST",
        &format!("/classes/{class}/blueprint"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(twice.status, StatusCode::OK);
    assert!(skips(&twice).is_empty());
    assert_eq!(rows("SELECT count(*) FROM class_course", &db).await, 2);

    // A class whose grade no blueprint covers is a 404, not an empty success.
    let other = create_class(&app, &manager, "10-A", 10).await;
    let none = send(
        &app,
        "POST",
        &format!("/classes/{other}/blueprint"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(none.status, StatusCode::NOT_FOUND);
}

/// The other direction of the retro-pump: a section created *after* the
/// template exists is stocked by `POST /classes` itself, and a grade no
/// template covers says so with `stocked_from: null` rather than being
/// indistinguishable from a template that applied cleanly.
#[tokio::test]
async fn creating_a_class_stocks_it_from_its_grades_blueprint() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let algebra = create_course(&app, &manager, "algebra").await;
    let physics = create_course(&app, &manager, "physics").await;
    let made = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade_level": 9, "course_ids": [algebra.clone(), physics.clone()] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);

    let res = send(
        &app,
        "POST",
        "/classes",
        Some(&manager),
        Some(json!({ "name": "9-A", "grade_level": 9 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{:?}", res.body);
    assert_eq!(
        res.body["stocked_from"], 9,
        "the create must name the template that stocked it: {:?}",
        res.body
    );
    assert!(skips(&res).is_empty(), "{:?}", res.body);
    let class = res.body["class"]["id"]
        .as_str()
        .expect("class id")
        .to_string();

    // Stored state, not the answer: the links are really there, and they carry
    // the blueprint's tag, so a later edit or delete can take them back.
    for course in [&algebra, &physics] {
        assert!(attached(&class, course, &db).await, "{course} is attached");
        assert_eq!(
            source_of(&class, course, &db).await,
            Some(blueprint_id(9, &db).await),
            "the blueprint must own what a create attached, exactly as a pump does"
        );
    }
    assert_eq!(
        count_on("class_course_count", "class_group", &class, &db).await,
        2
    );

    // A class is empty at create, so this is the first place the enrollment
    // rows the attachments owe can be seen: a member added now is enrolled into
    // both courses, in real rows.
    let student = student_in(&app, &db, &class, &manager, "ali").await;
    for course in [&algebra, &physics] {
        let instance = instance_of(&class, course, &db).await;
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM enrollment WHERE class_course = $1 AND app_user = $2",
            )
            .bind(uuid_of(&instance))
            .bind(UserId::from_key(&student))
            .fetch_one(&db)
            .await
            .unwrap(),
            1,
            "a stocked class enrolls its roster like any other"
        );
    }

    // A grade no template covers, and a class with no grade at all: `null`,
    // which is the one thing an empty `skipped` could never say.
    for body in [
        json!({ "name": "10-A", "grade_level": 10 }),
        // A club-shaped section sits at the ladder's floor, like any class.
        json!({ "name": "satranç", "grade_level": 0 }),
    ] {
        let res = send(&app, "POST", "/classes", Some(&manager), Some(body)).await;
        assert_eq!(res.status, StatusCode::CREATED, "{:?}", res.body);
        assert!(
            res.body["stocked_from"].is_null(),
            "no template covers it: {:?}",
            res.body
        );
        assert!(skips(&res).is_empty(), "{:?}", res.body);
        let bare = res.body["class"]["id"].as_str().unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM class_course WHERE class = $1",)
                .bind(uuid::Uuid::parse_str(bare).expect("a uuid class id"))
                .fetch_one(&db)
                .await
                .unwrap(),
            0,
            "nothing may be attached to a section no template reached"
        );
    }
}

/// A create whose stocking cannot place everything still creates the class: the
/// pair that did not fit comes back in `skipped`, best-effort exactly like
/// every other pump here.
/// The dead course is forged in the store rather than deleted through the API,
/// because `DELETE /courses/{id}` takes the id out of every template naming it
/// (see `deleting_a_course_takes_it_out_of_every_blueprint`) and would leave
/// nothing to skip. Under the `blueprint_course` junction the live store cannot
/// hold this state at all — the link is a foreign key — so the course row is
/// deleted with FK triggers suspended: the legacy-row state the prune still
/// exists for, and the same shape the pump's own window meets (a course gone
/// after the pump read the list it walks).
#[tokio::test]
async fn a_create_reports_what_its_blueprint_could_not_stock() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let algebra = create_course(&app, &manager, "algebra").await;
    let physics = create_course(&app, &manager, "physics").await;
    let made = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade_level": 9, "course_ids": [algebra.clone(), physics.clone()] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);

    // Take a course out from under the template, leaving its id in the list —
    // the state the delete's own sweep no longer produces, and the one a pump
    // meets when a course goes after it read the list. Nothing carries it yet
    // (no section exists at grade 9), so this is the pair that cannot fit when
    // the first one is created. The junction makes the live state impossible —
    // `blueprint_course.course` is a foreign key — so the course row goes with
    // FK triggers suspended: exactly what a row written before that cascade
    // existed looks like, which is the state the prune still exists for.
    let mut tx = db.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM course WHERE id = $1")
        .bind(CourseId::from_key(&physics))
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        templated(9, &physics, &db).await,
        "the id is still listed"
    );

    let res = send(
        &app,
        "POST",
        "/classes",
        Some(&manager),
        Some(json!({ "name": "9-A", "grade_level": 9 })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "a pair that will not fit may not cost the class: {:?}",
        res.body
    );
    assert_eq!(res.body["stocked_from"], 9);
    assert_eq!(skips(&res).len(), 1, "{:?}", res.body);
    assert_eq!(skips(&res)[0]["reason"], "course_deleted");
    assert_eq!(skips(&res)[0]["course"], physics.as_str());
    assert_eq!(skips(&res)[0]["class_name"], "9-A");

    let class = res.body["class"]["id"]
        .as_str()
        .expect("class id")
        .to_string();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM class_group WHERE id = $1")
            .bind(uuid::Uuid::parse_str(&class).expect("a uuid class id"))
            .fetch_one(&db)
            .await
            .unwrap(),
        1,
        "the class is really there — the skip is a report, not a rollback"
    );
    assert!(
        attached(&class, &algebra, &db).await,
        "…and the course that did fit is on it"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM class_course WHERE class = $1")
            .bind(uuid::Uuid::parse_str(&class).expect("a uuid class id"))
            .fetch_one(&db)
            .await
            .unwrap(),
        1
    );
    // The dangling id is pruned by the run that found it, here as anywhere.
    let read = send(&app, "GET", "/classes/blueprints/9", Some(&manager), None).await;
    assert_eq!(read.body["courses"].as_array().unwrap().len(), 1);
}

/// The ruling that costs the most: editing the template reaches every section
/// already at the grade, not just the ones made afterwards. And the sections at
/// *other* grades are left alone.
#[tokio::test]
async fn an_edit_retro_pumps_every_class_at_the_grade() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let algebra = create_course(&app, &manager, "algebra").await;
    let a = create_class(&app, &manager, "9-A", 9).await;
    let b = create_class(&app, &manager, "9-B", 9).await;
    let ten = create_class(&app, &manager, "10-A", 10).await;
    let ali = student_in(&app, &db, &a, &manager, "ali").await;
    student_in(&app, &db, &ten, &manager, "veli").await;

    // Created empty, so the create's own pump has nothing to place…
    let made = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade_level": 9, "course_ids": [] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED);
    assert_eq!(rows("SELECT count(*) FROM class_course", &db).await, 0);

    // …and the edit is what has to reach the two classes standing there.
    let patched = send(
        &app,
        "PATCH",
        "/classes/blueprints/9",
        Some(&manager),
        Some(json!({ "course_ids": [algebra.clone()] })),
    )
    .await;
    assert_eq!(patched.status, StatusCode::OK, "{:?}", patched.body);
    assert!(skips(&patched).is_empty(), "{:?}", patched.body);

    assert!(attached(&a, &algebra, &db).await);
    assert!(attached(&b, &algebra, &db).await);
    assert!(
        !attached(&ten, &algebra, &db).await,
        "a blueprint pumps its own grade and no other"
    );
    // Counters exact after the retro-pump: one attachment each on the two
    // classes, none on the third, one enrolled seat for the one student at the
    // grade.
    for class in [&a, &b] {
        assert_eq!(
            count_on("class_course_count", "class_group", class, &db).await,
            1
        );
    }
    assert_eq!(
        count_on("class_course_count", "class_group", &ten, &db).await,
        0
    );
    let instance = instance_of(&a, &algebra, &db).await;
    assert_eq!(
        count_on("enrollment_count", "class_course", &instance, &db).await,
        1,
        "one student in the şube that carries the course, one seat on its instance"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM enrollment WHERE app_user = $1")
            .bind(UserId::from_key(&ali))
            .fetch_one(&db)
            .await
            .unwrap(),
        1
    );
}

/// A duplicate is a refusal by hand and deliberately not a skip for the pump:
/// the second `POST /classes/{id}/instances` for a pair the class already
/// carries answers the machine code a client branches on, beside the sentence
/// it always sent.
#[tokio::test]
async fn a_duplicate_attach_answers_its_machine_code() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let roomy = create_course(&app, &manager, "algebra").await;
    let class = create_class(&app, &manager, "9-A", 9).await;
    student_in(&app, &db, &class, &manager, "ali").await;

    let attached = send(
        &app,
        "POST",
        &format!("/classes/{class}/instances"),
        Some(&manager),
        Some(json!({ "course_id": roomy.clone() })),
    )
    .await;
    assert_eq!(attached.status, StatusCode::CREATED, "{:?}", attached.body);

    let again = send(
        &app,
        "POST",
        &format!("/classes/{class}/instances"),
        Some(&manager),
        Some(json!({ "course_id": roomy })),
    )
    .await;
    assert_eq!(again.status, StatusCode::CONFLICT, "{:?}", again.body);
    assert_eq!(again.body["code"], "duplicate", "{:?}", again.body);
    assert!(
        again.body["error"].as_str().unwrap_or_default().len() > 10,
        "…beside the sentence it always sent — the code is additive: {:?}",
        again.body
    );
    // The refused door spent nothing: one instance, one attachment.
    assert_eq!(
        count_on("class_course_count", "class_group", &class, &db).await,
        1
    );
}

/// The provenance rule, both halves: dropping a course from the template
/// detaches it wherever the *blueprint* attached it, and leaves it standing
/// wherever a human did.
#[tokio::test]
async fn a_removal_spares_a_hand_attached_course() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let algebra = create_course(&app, &manager, "algebra").await;
    let pumped = create_class(&app, &manager, "9-A", 9).await;
    let byhand = create_class(&app, &manager, "9-B", 9).await;
    let ali = student_in(&app, &db, &pumped, &manager, "ali").await;
    student_in(&app, &db, &byhand, &manager, "veli").await;

    // 9-B gets algebra the old way, *before* the blueprint exists.
    let attached_by_hand = send(
        &app,
        "POST",
        &format!("/classes/{byhand}/instances"),
        Some(&manager),
        Some(json!({ "course_id": algebra.clone() })),
    )
    .await;
    assert_eq!(attached_by_hand.status, StatusCode::CREATED);
    assert_eq!(
        source_of(&byhand, &algebra, &db).await,
        None,
        "a hand attach carries no blueprint key at all"
    );

    let made = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade_level": 9, "course_ids": [algebra.clone()] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED);
    assert!(skips(&made).is_empty(), "{:?}", made.body);
    assert_eq!(
        source_of(&byhand, &algebra, &db).await,
        None,
        "a pump may not adopt an attachment it did not make"
    );
    assert_eq!(
        source_of(&pumped, &algebra, &db).await,
        Some(blueprint_id(9, &db).await),
    );

    // Drop it from the template.
    let patched = send(
        &app,
        "PATCH",
        "/classes/blueprints/9",
        Some(&manager),
        Some(json!({ "course_ids": [] })),
    )
    .await;
    assert_eq!(patched.status, StatusCode::OK, "{:?}", patched.body);

    assert!(
        !attached(&pumped, &algebra, &db).await,
        "the blueprint takes back what it placed"
    );
    assert!(
        attached(&byhand, &algebra, &db).await,
        "…and nothing else — a hand attach survives"
    );
    assert_eq!(
        count_on("class_course_count", "class_group", &pumped, &db).await,
        0,
        "the detach gives the class its count back"
    );
    assert_eq!(
        count_on("class_course_count", "class_group", &byhand, &db).await,
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM enrollment WHERE app_user = $1")
            .bind(UserId::from_key(&ali))
            .fetch_one(&db)
            .await
            .unwrap(),
        0,
        "the enrollments the blueprint pumped go with it"
    );
    let instance = instance_of(&byhand, &algebra, &db).await;
    assert_eq!(
        count_on("enrollment_count", "class_course", &instance, &db).await,
        1,
        "the hand-attached class keeps its student's seat"
    );
}

/// The migration's whole stale-data claim, made from the state a pre-migration
/// row is actually in: `class_course` carries no `source` key.
///
/// There is no backfill, because absence *is* the meaning — so this is what
/// pins that `source = $blueprint` is false for an absent key, exactly as it is
/// on `enrollment.source`. If it were not, a blueprint's first removal would
/// detach every course the school had ever attached by hand.
#[tokio::test]
async fn a_row_written_before_the_column_reads_as_hand_attached() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let algebra = create_course(&app, &manager, "algebra").await;
    let class = create_class(&app, &manager, "9-A", 9).await;
    student_in(&app, &db, &class, &manager, "ali").await;
    let attached_by_hand = send(
        &app,
        "POST",
        &format!("/classes/{class}/instances"),
        Some(&manager),
        Some(json!({ "course_id": algebra.clone() })),
    )
    .await;
    assert_eq!(attached_by_hand.status, StatusCode::CREATED);
    // Age the row to before the column existed: a NULL `source` is what the
    // store holds for a hand attach, and it is what the rule reads.
    sqlx::query("UPDATE class_course SET source = NULL")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(source_of(&class, &algebra, &db).await, None);

    // Every read still decodes it…
    let listed = send(
        &app,
        "GET",
        &format!("/classes/{class}/instances"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{:?}", listed.body);
    assert_eq!(listed.body["items"].as_array().unwrap().len(), 1);

    // …a blueprint that names the same course does not adopt it…
    let made = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade_level": 9, "course_ids": [algebra.clone()] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);
    assert!(
        skips(&made).is_empty(),
        "a course the class already carries is not a skip: {:?}",
        made.body
    );
    assert_eq!(source_of(&class, &algebra, &db).await, None);

    // …and deleting that blueprint — which drops every course it holds — leaves
    // the aged row exactly where it is.
    let dropped = send(
        &app,
        "DELETE",
        "/classes/blueprints/9",
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(dropped.status, StatusCode::NO_CONTENT);
    assert!(
        attached(&class, &algebra, &db).await,
        "a pre-migration row is hand-attached and unreachable by any blueprint sweep"
    );
    let instance = instance_of(&class, &algebra, &db).await;
    assert_eq!(
        count_on("enrollment_count", "class_course", &instance, &db).await,
        1
    );
}

/// The window the lock exists to close, made visible.
///
/// `service::class_blueprint::delete` removes the row and then sweeps by the provenance
/// tag, while a pump reads that row inside the transaction that writes the link
/// — a cross-record read-then-write the store does not serialize. A pump that
/// passed its liveness claim and then lost the blueprint commits a
/// `class_course` row tagged with a record nothing can reach, since the grade
/// label *is* the id and no sweep will ever run for it again.
///
/// Under Postgres the window itself is shut — `class_course.source` is a real
/// foreign key, so a blueprint delete inside the attach's transaction takes the
/// attach down with it, and a crash between the delete and its sweep is one
/// transaction. What the store can no longer refuse is the residue an old
/// volume can still carry, so the stranded row is forged the one way Postgres
/// allows (FK triggers suspended for the one transaction) and what is pinned is
/// the recovery contract for it: a human detaches it one course at a time, and
/// the counters come back exact.
#[tokio::test]
async fn a_blueprint_lost_mid_attach_strands_a_row_that_stays_detachable() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let algebra = create_course(&app, &manager, "algebra").await;
    let made = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade_level": 9, "course_ids": [algebra.clone()] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);
    // The section has to reach grade 9 without being stocked on the way, so
    // that the pump below is the *first* attach: a section created at the grade
    // is stocked by its own create, and one that existed already was stocked by
    // the template's. Moving it there afterwards is neither — a grade change is
    // deliberately a blueprint no-op.
    let class = create_class(&app, &manager, "9-A", 10).await;
    let moved = send(
        &app,
        "PATCH",
        &format!("/classes/{class}"),
        Some(&manager),
        Some(json!({ "grade_level": 9 })),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "{:?}", moved.body);
    assert!(!attached(&class, &algebra, &db).await);

    // The stranded state: the link row committed, the blueprint it names did
    // not survive. Real FKs refuse that state, so it is written with FK
    // triggers suspended — the manager id is real, the `source` FK is the one
    // the strand consists of. Capture the uuid before the delete: the grade
    // label is UNIQUE, not the FK.
    let manager_id = UserId::from_key(&me_id(&app, &manager).await);
    let stranded = blueprint_id(9, &db).await;
    let mut tx = db.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM class_blueprint WHERE grade_level = 9")
        .execute(&mut *tx)
        .await
        .unwrap();
    // The strand names an offering too (the column is NOT NULL): the pump run
    // above already minted one for (algebra, 9) — reuse it, minting a twin
    // only if none survived, exactly like `ensure_tx` would.
    let offering: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO course_offering (id, course, grade_level, created_by, created_at, updated_at) \
         VALUES ($1, $2, 9, $3, $4, $4) \
         ON CONFLICT (course, grade_level) DO UPDATE SET updated_at = EXCLUDED.updated_at \
         RETURNING id",
    )
    .bind(next_uuid())
    .bind(CourseId::from_key(&algebra))
    .bind(manager_id)
    .bind(Timestamp::now().as_millis())
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO class_course (id, class, course, attached_by, attached_at, source, offering) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(next_uuid())
    .bind(uuid::Uuid::parse_str(&class).expect("a uuid class id"))
    .bind(CourseId::from_key(&algebra))
    .bind(manager_id)
    .bind(Timestamp::now().as_millis())
    .bind(stranded)
    .bind(offering.0)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("UPDATE class_group SET class_course_count = 1 WHERE id = $1")
        .bind(uuid::Uuid::parse_str(&class).expect("a uuid class id"))
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(
        rows("SELECT count(*) FROM class_blueprint", &db).await,
        0,
        "the injected delete really landed"
    );
    assert!(
        attached(&class, &algebra, &db).await,
        "the link stands after its blueprint was gone: this is the stranded \
         row, tagged with a record no sweep can ever reach"
    );
    assert_eq!(source_of(&class, &algebra, &db).await, Some(stranded));

    // The documented recovery: a human detaches it one course at a time, and
    // the counters come back exact.
    let instance = instance_of(&class, &algebra, &db).await;
    let detached = send(
        &app,
        "DELETE",
        &format!("/classes/{class}/instances/{instance}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(
        detached.status,
        StatusCode::NO_CONTENT,
        "{:?}",
        detached.body
    );
    assert!(!attached(&class, &algebra, &db).await);
    assert_eq!(
        count_on("class_course_count", "class_group", &class, &db).await,
        0,
        "a stranded row still releases its counter when it is detached"
    );
}

/// The silent miss `matched` exists for: sections are matched by their ladder
/// rung, so a template keyed at a rung no section carries reaches none of them
/// — and it says so with an empty `skipped`, which is the same body a template
/// that stocked every section returns.
///
/// Both halves are asserted from the same section, one rung apart, because the
/// count only means anything against the case that *does* reach it: a `matched`
/// wired to the course list would claim 1 on the empty rung, and one wired to
/// the skip count would answer 0 on the rung that works.
#[tokio::test]
async fn a_rung_nothing_carries_reports_matched_zero() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let algebra = create_course(&app, &manager, "algebra").await;
    // The section exists first, so a template that finds it stocks it on
    // create.
    let class = create_class(&app, &manager, "9-A", 9).await;

    let typo = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade_level": 11, "course_ids": [algebra.clone()] })),
    )
    .await;
    assert_eq!(typo.status, StatusCode::CREATED, "{:?}", typo.body);
    assert!(
        skips(&typo).is_empty(),
        "nothing was refused — there was nothing to refuse: {:?}",
        typo.body
    );
    assert_eq!(
        typo.body["matched"], 0,
        "no section sits at 11, and that must not read as success: {:?}",
        typo.body
    );
    assert!(
        !attached(&class, &algebra, &db).await,
        "a rung above the section's really is a different grade"
    );

    let right = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade_level": 9, "course_ids": [algebra.clone()] })),
    )
    .await;
    assert_eq!(right.status, StatusCode::CREATED, "{:?}", right.body);
    assert!(skips(&right).is_empty(), "{:?}", right.body);
    assert_eq!(
        right.body["matched"], 1,
        "the one section at the grade was reached: {:?}",
        right.body
    );
    assert!(attached(&class, &algebra, &db).await);
}

/// The skip list a pump returns lives only in that one response body, so
/// `GET /classes/blueprints/{grade}/status` is what a manager asks afterwards:
/// which sections are out of sync, and with which courses.
///
/// Three rules in one run, because they are the same run: a section short a
/// course names *exactly* that pair; satisfying it **by hand** empties the list
/// (the template asks for the course, not for the pump's tag); and a grade with
/// every section in sync reports them all with nothing missing, `matched`
/// counting the sections rather than the courses.
///
/// The section that is short gets there the one way no pump repairs: a grade
/// change is deliberately a blueprint no-op, so a section renamed onto the
/// grade after the template exists carries none of what it holds.
///
/// The store is asserted alongside the body — the in-memory engine forges wins,
/// and a status read agreeing with a `class_course` table that says otherwise
/// would be worse than no read at all.
#[tokio::test]
async fn a_status_read_names_the_course_a_section_is_short() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let algebra = create_course(&app, &manager, "algebra").await;
    // The section starts at grade 10, where nothing stocks it, so the template
    // below is never what attached anything to it.
    let short = create_class(&app, &manager, "9-A", 10).await;

    // No template covers the grade yet, and that is a 404 rather than an empty
    // report — there is nothing to be out of sync with.
    let nothing = send(
        &app,
        "GET",
        "/classes/blueprints/9/status",
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(nothing.status, StatusCode::NOT_FOUND, "{:?}", nothing.body);

    let made = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade_level": 9, "course_ids": [algebra.clone()] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);
    assert!(skips(&made).is_empty(), "{:?}", made.body);
    assert_eq!(made.body["matched"], 0, "no section carries the label yet");

    // A section created at the grade is stocked by its own create, in sync.
    let full = create_class(&app, &manager, "9-B", 9).await;
    assert!(attached(&full, &algebra, &db).await);

    // The other section reaches the grade by a rename, which is deliberately
    // not a pump: it stands at the grade carrying none of the template.
    let moved = send(
        &app,
        "PATCH",
        &format!("/classes/{short}"),
        Some(&manager),
        Some(json!({ "grade_level": 9 })),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "{:?}", moved.body);
    assert!(!attached(&short, &algebra, &db).await);

    let drifted = send(
        &app,
        "GET",
        "/classes/blueprints/9/status",
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(drifted.status, StatusCode::OK, "{:?}", drifted.body);
    assert_eq!(drifted.body["grade_level"], 9);
    assert_eq!(drifted.body["matched"], 2, "both sections carry the label");
    assert_eq!(
        missing(&drifted, &short),
        vec![algebra.clone()],
        "exactly the course that section is short: {:?}",
        drifted.body
    );
    assert_eq!(section(&drifted, &short)["class_name"], "9-A");
    assert!(
        missing(&drifted, &full).is_empty(),
        "the section the pump stocked is in sync: {:?}",
        drifted.body
    );
    // …and the report agrees with the store on both halves.
    assert!(attached(&full, &algebra, &db).await);
    assert!(!attached(&short, &algebra, &db).await);

    // Now a human fixes it: the course attached by hand, carrying no blueprint
    // tag at all.
    let byhand = send(
        &app,
        "POST",
        &format!("/classes/{short}/instances"),
        Some(&manager),
        Some(json!({ "course_id": algebra.clone() })),
    )
    .await;
    assert_eq!(byhand.status, StatusCode::CREATED, "{:?}", byhand.body);
    assert_eq!(
        source_of(&short, &algebra, &db).await,
        None,
        "a hand attach carries no blueprint key — which is the point of the next \
         assertion"
    );

    let synced = send(
        &app,
        "GET",
        "/classes/blueprints/9/status",
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(synced.status, StatusCode::OK, "{:?}", synced.body);
    assert_eq!(
        synced.body["sections"].as_array().unwrap().len(),
        2,
        "every section at the grade is listed, in sync or not: {:?}",
        synced.body
    );
    assert_eq!(
        synced.body["matched"], 2,
        "matched counts the sections, not the courses: {:?}",
        synced.body
    );
    for class in [&short, &full] {
        assert!(
            missing(&synced, class).is_empty(),
            "a link of any source satisfies the template: {:?}",
            synced.body
        );
    }
}

/// Deleting a course takes its id out of every template holding it.
///
/// A template outlives the links it stocked — a section may let an instance go
/// long before the catalog row itself is deletable (the delete's guard reads
/// the course's instance counter) — so the id a template names can point at a
/// row that is gone. Without that sweep the id stayed in the list forever and
/// every doc surface lied: `PATCH`ing the template back **as it stands** is
/// the documented self-heal, and it answered `400` ("one of these courses does
/// not exist"), because the handler resolves the ids the *request* names — the
/// very ones it had just read back. So the assertion that matters is not only
/// that the store is clean but that the round trip a manager is told to make
/// succeeds.
#[tokio::test]
async fn deleting_a_course_takes_it_out_of_every_blueprint() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let algebra = create_course(&app, &manager, "algebra").await;
    let history = create_course(&app, &manager, "history").await;
    // No students: the section is here so the pump has something to walk, and
    // a section that lets its link go is what lets the course be deleted at
    // all (the guard reads the course's own instance counter, so one still
    // teaching it is a 409).
    let class = create_class(&app, &manager, "9-A", 9).await;

    let made = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade_level": 9, "course_ids": [algebra.clone(), history.clone()] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);
    assert!(attached(&class, &history, &db).await);

    // The link goes the way an operator's does — through the instance's own
    // detach — while the *template* goes on naming the course. That is the
    // state the sweep below is about: the id outliving every live link.
    let instances = send(
        &app,
        "GET",
        &format!("/classes/{class}/instances"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(instances.status, StatusCode::OK, "{:?}", instances.body);
    let link = common::items(&instances.body)
        .iter()
        .find(|row| row["course"] == json!(history.as_str()))
        .expect("the pump attached history to the section");
    let detached = send(
        &app,
        "DELETE",
        &format!("/classes/{class}/instances/{}", common::id_of(link)),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(
        detached.status,
        StatusCode::NO_CONTENT,
        "{:?}",
        detached.body
    );

    let deleted = send(
        &app,
        "DELETE",
        &format!("/courses/{history}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{:?}", deleted.body);

    assert!(
        !templated(9, &history, &db).await,
        "the deleted course is out of the template"
    );
    assert!(
        templated(9, &algebra, &db).await,
        "…and nothing else is — the sweep names one course"
    );
    assert!(
        !attached(&class, &history, &db).await,
        "the section's link was already gone — the delete's guard demands it"
    );
    assert!(
        attached(&class, &algebra, &db).await,
        "…and the detach named one instance: the other link stands"
    );

    // The self-heal, made exactly as documented: read the list, send it back.
    let courses = held(&app, &manager, "9").await;
    assert_eq!(
        courses.len(),
        1,
        "the read agrees with the store: {courses:?}"
    );
    let healed = send(
        &app,
        "PATCH",
        "/classes/blueprints/9",
        Some(&manager),
        Some(json!({ "course_ids": courses })),
    )
    .await;
    assert_eq!(healed.status, StatusCode::OK, "{:?}", healed.body);

    // …and the status read has no phantom to report.
    let status = send(
        &app,
        "GET",
        "/classes/blueprints/9/status",
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(status.status, StatusCode::OK, "{:?}", status.body);
    assert!(
        missing(&status, &class).is_empty(),
        "a course that no longer exists is not something a section is short: {:?}",
        status.body
    );
}

/// The case no pump could ever repair: a grade with **zero** sections.
///
/// The prune that used to be the only thing removing a dead id fires while
/// walking a section, and `POST /classes/{id}/blueprint` needs a section to run
/// against — so at a grade nothing carries, no call in the whole API could
/// clear the id. This sweep runs off the course's own delete, which does not
/// care whether the grade has sections.
#[tokio::test]
async fn a_grade_with_no_sections_still_loses_its_deleted_course() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let algebra = create_course(&app, &manager, "algebra").await;

    let made = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade_level": 11, "course_ids": [algebra.clone()] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);
    assert_eq!(made.body["matched"], 0, "no section carries the label");

    let deleted = send(
        &app,
        "DELETE",
        &format!("/courses/{algebra}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{:?}", deleted.body);

    assert!(
        !templated(11, &algebra, &db).await,
        "no section to walk, and the id is gone anyway"
    );
    assert!(
        held(&app, &manager, "11").await.is_empty(),
        "the read agrees with the store"
    );
    let healed = send(
        &app,
        "PATCH",
        "/classes/blueprints/11",
        Some(&manager),
        Some(json!({ "course_ids": [] })),
    )
    .await;
    assert_eq!(
        healed.status,
        StatusCode::OK,
        "the template is editable again: {:?}",
        healed.body
    );
}

/// A sweep that dies half-way must be *repairable*, and the repair is the one
/// every doc surface names: `PATCH` the template with the list it already holds.
///
/// The removal used to be a diff against the handle this caller read — list
/// saved first, then `dropped = read \ wanted` detached one transaction at a
/// time. Any failure inside that loop (a lost round, a reconnect, a liveness
/// reject) returned 500 with the list already stored and only some links swept,
/// and the documented repair then recomputed an *empty* diff and swept nothing:
/// sections kept `class_course` rows for courses the template no longer held,
/// tagged with a live blueprint, and `status` cannot even report an extra.
///
/// The half-swept state is forged in the store rather than provoked with a
/// failure injection, because it is exactly what that loop leaves behind: the
/// stored list without the course, the link rows still there.
#[tokio::test]
async fn a_half_swept_removal_is_finished_by_the_documented_re_patch() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let algebra = create_course(&app, &manager, "algebra").await;
    let history = create_course(&app, &manager, "history").await;
    let class = create_class(&app, &manager, "9-A", 9).await;
    let ali = student_in(&app, &db, &class, &manager, "ali").await;

    let made = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade_level": 9, "course_ids": [algebra.clone(), history.clone()] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);
    assert!(attached(&class, &history, &db).await);

    // The state a sweep that died half-way leaves: the list is stored without
    // history, its attachment is not.
    sqlx::query(
        "DELETE FROM blueprint_course \
         WHERE course = $1 \
           AND blueprint = (SELECT id FROM class_blueprint WHERE grade_level = 9)",
    )
    .bind(CourseId::from_key(&history))
    .execute(&db)
    .await
    .unwrap();

    // The instance rows the heal will sweep — captured while they stand,
    // because the detach takes the history one with it.
    let history_instance = instance_of(&class, &history, &db).await;
    let algebra_instance = instance_of(&class, &algebra, &db).await;

    let courses = held(&app, &manager, "9").await;
    assert_eq!(
        courses.len(),
        1,
        "the read agrees with the store: {courses:?}"
    );
    let healed = send(
        &app,
        "PATCH",
        "/classes/blueprints/9",
        Some(&manager),
        Some(json!({ "course_ids": courses })),
    )
    .await;
    assert_eq!(healed.status, StatusCode::OK, "{:?}", healed.body);

    assert!(
        !attached(&class, &history, &db).await,
        "re-sending the stored list must finish the removal it stored"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM enrollment WHERE app_user = $1 AND class_course = $2",
        )
        .bind(UserId::from_key(&ali))
        .bind(uuid_of(&history_instance))
        .fetch_one(&db)
        .await
        .unwrap(),
        0,
        "…enrollments and all"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM class_course WHERE id = $1")
            .bind(uuid_of(&history_instance))
            .fetch_one(&db)
            .await
            .unwrap(),
        0,
        "…the detached instance gone with it, seat counter and all"
    );
    assert!(
        attached(&class, &algebra, &db).await,
        "and the course the template still holds is untouched"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM enrollment WHERE app_user = $1 AND class_course = $2",
        )
        .bind(UserId::from_key(&ali))
        .bind(uuid_of(&algebra_instance))
        .fetch_one(&db)
        .await
        .unwrap(),
        1
    );
    assert_eq!(
        count_on("enrollment_count", "class_course", &algebra_instance, &db).await,
        1,
        "the instance the template still holds keeps its seat"
    );
}

/// A course deleted between the handler's own pre-flight read and the pump that
/// walks the list is pruned out of the stored template — and the body that
/// pruned it must say so. It used to be rendered from the pre-prune handle, so
/// the `201`/`200` listed a course the `GET` a moment later did not.
///
/// Both write routes are driven, and the window is opened by the schema rather
/// than by a lucky interleaving: a trigger on `blueprint_course` fires inside
/// the very transaction that stores the list — after the link row landed, so
/// the foreign key allows the course's own death — which is exactly "after the
/// courses were resolved, before the pump runs", every single time.
#[tokio::test]
async fn a_course_pruned_mid_pump_is_out_of_the_body_that_pruned_it() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let algebra = create_course(&app, &manager, "algebra").await;
    let history = create_course(&app, &manager, "history").await;
    let class = create_class(&app, &manager, "9-A", 9).await;

    let mut conn = db.acquire().await.expect("acquire for the trigger");
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE FUNCTION heztest_kill_on_create() RETURNS trigger AS $$
         BEGIN DELETE FROM blueprint_course WHERE blueprint = NEW.blueprint \
               AND course = NEW.course;
               DELETE FROM course WHERE id = NEW.course; RETURN NULL; END;
         $$ LANGUAGE plpgsql;
         CREATE TRIGGER heztest_kill_on_create AFTER INSERT ON blueprint_course
         FOR EACH ROW WHEN (NEW.course = '{algebra}')
         EXECUTE FUNCTION heztest_kill_on_create();"
    )))
    .execute(&mut *conn)
    .await
    .expect("define the kill-on-create trigger");
    let made = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade_level": 9, "course_ids": [algebra.clone()] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);
    assert_eq!(skips(&made).len(), 1, "the seam fired: {:?}", made.body);
    assert_eq!(skips(&made)[0]["reason"], "course_deleted");
    assert!(
        made.body["blueprint"]["courses"]
            .as_array()
            .unwrap()
            .is_empty(),
        "the response may not list a course this very call pruned: {:?}",
        made.body
    );
    assert!(
        held(&app, &manager, "9").await.is_empty(),
        "…which is the read it has to agree with"
    );

    // The same window on the edit route.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE FUNCTION heztest_kill_on_update() RETURNS trigger AS $$
         BEGIN DELETE FROM blueprint_course WHERE blueprint = NEW.blueprint \
               AND course = NEW.course;
               DELETE FROM course WHERE id = NEW.course; RETURN NULL; END;
         $$ LANGUAGE plpgsql;
         CREATE TRIGGER heztest_kill_on_update AFTER INSERT ON blueprint_course
         FOR EACH ROW WHEN (NEW.course = '{history}')
         EXECUTE FUNCTION heztest_kill_on_update();"
    )))
    .execute(&mut *conn)
    .await
    .expect("define the kill-on-update trigger");
    let patched = send(
        &app,
        "PATCH",
        "/classes/blueprints/9",
        Some(&manager),
        Some(json!({ "course_ids": [history.clone()] })),
    )
    .await;
    assert_eq!(patched.status, StatusCode::OK, "{:?}", patched.body);
    assert_eq!(skips(&patched).len(), 1, "{:?}", patched.body);
    assert!(
        patched.body["blueprint"]["courses"]
            .as_array()
            .unwrap()
            .is_empty(),
        "the edit's body may not list it either: {:?}",
        patched.body
    );
    assert!(held(&app, &manager, "9").await.is_empty());
    assert!(
        !attached(&class, &algebra, &db).await && !attached(&class, &history, &db).await,
        "and nothing was attached for either dead course"
    );
}

/// A template may never *name* a course that is gone, whichever write stores
/// the list.
///
/// The handler's `resolve_courses` is a pure read, and a `DELETE /courses/{id}`
/// landing between it and the write sweeps a `class_blueprint` row that does not
/// exist yet — SurrealDB conflict-checks no read, so both commit. `prune` fires
/// only while walking a section, so at a grade carrying none the dangling id is
/// permanent, which is the state `a_grade_with_no_sections_still_loses_its_
/// deleted_course` promises is unreachable.
///
/// Driven at the domain, with a course that is already gone: that is the exact
/// state the losing side of the race is in when its transaction runs, and it is
/// deterministic where two live requests are not. Over HTTP the pre-flight read
/// answers this same `400` first, so the surface does not move.
#[tokio::test]
async fn a_template_write_refuses_a_course_that_is_gone() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let by = UserId::from_key(&me_id(&app, &manager).await);
    let algebra = CourseId::from_key(&create_course(&app, &manager, "algebra").await);
    let ghost = CourseId::from_key(GHOST_ID);

    let refused = class_blueprint::create(
        &db,
        &by,
        GradeLevel::new(9).unwrap(),
        vec![ghost.clone()],
    )
    .await;
    assert!(
        matches!(refused, Err(AppError::Validation(_))),
        "a course that is gone is a 400, not a template naming it: {refused:?}"
    );
    assert_eq!(
        rows("SELECT count(*) FROM class_blueprint", &db).await,
        0,
        "…and nothing may be stored"
    );

    let blueprint = class_blueprint::create(
        &db,
        &by,
        GradeLevel::new(9).unwrap(),
        vec![algebra.clone()],
    )
    .await
    .unwrap();
    let refused =
        class_blueprint::set_courses(&db, blueprint, vec![algebra.clone(), ghost], &by).await;
    assert!(
        matches!(refused, Err(AppError::Validation(_))),
        "the edit carries the same claim: {refused:?}"
    );
    assert_eq!(
        held(&app, &manager, "9").await.len(),
        1,
        "…and the stored list did not move"
    );
}

/// The bytes a swept instance held live only on disk: the exam questions'
/// illustrations and the class's homework submission files. The sweep removes
/// the rows and hands their blob names back, and the route is what unlinks
/// them — a route that drops that list strands the bytes with nothing left
/// pointing at them, which is the leak this pins closed.
///
/// This is the blueprint half of the sweep: dropping a course from the grade's
/// template detaches the instances the blueprint itself attached, so the
/// `PATCH` a manager makes is what owes the unlink, not only the row removal.
#[tokio::test]
async fn a_blueprint_removal_unlinks_the_swept_instances_blob_bytes() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let year = create_year(&app, &manager, "2026-2027").await;
    let term = create_term(&app, &manager, &year, "1. Dönem").await;
    let algebra = create_course(&app, &manager, "algebra").await;

    // The template first, then the section: a create stocks from its grade's
    // blueprint, so this instance carries the blueprint as its source — which
    // is what lets the removal below sweep it at all.
    let made = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade_level": 9, "course_ids": [algebra.clone()] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);
    let class =
        common::create_class(&app, &manager, "9-A", json!({ "grade_level": 9, "year": year })).await;
    assert_eq!(
        source_of(&class, &algebra, &db).await,
        Some(blueprint_id(9, &db).await),
        "the create must stock the section from the template"
    );
    let instance = instance_of(&class, &algebra, &db).await;

    // One student, so a submission file can exist: adding them to the şube
    // pumped an enrollment into the instance the template attached.
    let ali = login_as(&app, &db, "ali", "student").await;
    let ali_id = me_id(&app, &ali).await;
    let added = send(
        &app,
        "POST",
        &format!("/classes/{class}/members"),
        Some(&manager),
        Some(json!({ "user_id": ali_id })),
    )
    .await;
    assert_eq!(added.status, StatusCode::CREATED, "{:?}", added.body);

    let subject = create_subject(&app, &manager, &algebra, "Cebir").await;
    let exam = create_exam(&app, &manager, &instance, &term, "Vize", "yazili").await;
    let question = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions"),
        Some(&manager),
        Some(json!({ "subject_id": subject, "text": "x kare", "kind": "text", "points": 5 })),
    )
    .await;
    assert_eq!(question.status, StatusCode::CREATED, "{}", question.body);
    let question = id_of(&question.body);
    let image = upload_file_at(
        &app,
        &manager,
        &format!("/exams/{exam}/questions/{question}/image"),
        "map.png",
        "image/png",
        b"png-bytes",
    )
    .await;
    assert_eq!(image.status, StatusCode::CREATED, "{}", image.body);

    let homework = create_homework(
        &app,
        &manager,
        &instance,
        &subject,
        "Ödev",
        FIXTURE_YEAR_ENDS_AT,
    )
    .await;
    let submission = upload_file_at(
        &app,
        &ali,
        &format!("/homework/{homework}/submission/files"),
        "odev.pdf",
        "application/pdf",
        b"pdf-bytes",
    )
    .await;
    assert_eq!(
        submission.status,
        StatusCode::CREATED,
        "{}",
        submission.body
    );

    // Both blob names, straight out of the store: the bytes on disk are keyed
    // by the row's own file name.
    let files: Vec<String> = sqlx::query_scalar(
        "SELECT file FROM question_image WHERE exam = $1
         UNION ALL
         SELECT f.file FROM homework_file f
          WHERE f.submission IN (SELECT id FROM homework_submission WHERE homework = $2)",
    )
    .bind(uuid_of(&exam))
    .bind(uuid_of(&homework))
    .fetch_all(&db)
    .await
    .expect("the blob rows");
    assert_eq!(
        files.len(),
        2,
        "the fixture must have stored both blobs: {files:?}"
    );
    for file in &files {
        assert!(
            blob_dir().join(file).exists(),
            "the fixture's blob {file} never landed on disk"
        );
    }

    let patched = send(
        &app,
        "PATCH",
        "/classes/blueprints/9",
        Some(&manager),
        Some(json!({ "course_ids": [] })),
    )
    .await;
    assert_eq!(patched.status, StatusCode::OK, "{:?}", patched.body);
    assert!(skips(&patched).is_empty(), "{:?}", patched.body);
    assert!(
        !attached(&class, &algebra, &db).await,
        "the template takes back the instance it placed"
    );
    for file in &files {
        assert!(
            !blob_dir().join(file).exists(),
            "the swept instance's blob {file} lingers on disk with nothing \
             pointing at it"
        );
    }
}
