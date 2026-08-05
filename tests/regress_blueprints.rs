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
use common::{Res, app_and_db, create_course, login_as, me_id, send};
use hezarfen_backend::database::Database;
use hezarfen_backend::domain::class_blueprint::ClassBlueprint;
use hezarfen_backend::domain::course::CourseId;
use hezarfen_backend::domain::user::UserId;
use hezarfen_backend::error::AppError;
use serde_json::{Value, json};

/// One counter, re-read out of the store.
async fn counter(sql: &str, db: &Database) -> i64 {
    let mut result = db.query(sql).await.unwrap().check().unwrap();
    result
        .take::<Vec<i64>>(0)
        .unwrap()
        .first()
        .copied()
        .unwrap_or(0)
}

/// How many rows `sql` selects ids for.
async fn rows(sql: &str, db: &Database) -> i64 {
    let mut result = db.query(sql).await.unwrap().check().unwrap();
    result
        .take::<Vec<surrealdb::types::RecordId>>(0)
        .unwrap()
        .len() as i64
}

/// Is that course on that class, in the store?
async fn attached(class: &str, course: &str, db: &Database) -> bool {
    rows(
        &format!(
            "SELECT VALUE id FROM class_course WHERE class = class_group:{class} \
             AND course = course:{course}"
        ),
        db,
    )
    .await
        == 1
}

/// The blueprint key stored on one attachment, or `None` when the row carries
/// no such key at all — which is the whole provenance rule: absent means a
/// human attached it.
async fn source_of(class: &str, course: &str, db: &Database) -> Option<String> {
    let mut result = db
        .query(format!(
            "SELECT VALUE (IF source != NONE THEN record::id(source) ELSE '' END) \
             FROM class_course WHERE class = class_group:{class} AND course = course:{course}"
        ))
        .await
        .unwrap()
        .check()
        .unwrap();
    result
        .take::<Vec<String>>(0)
        .unwrap()
        .into_iter()
        .next()
        .filter(|key| !key.is_empty())
}

/// Does that grade's template still name that course, in the store?
async fn templated(grade: &str, course: &str, db: &Database) -> bool {
    rows(
        &format!(
            "SELECT VALUE id FROM class_blueprint \
             WHERE grade = '{grade}' AND course:{course} IN courses"
        ),
        db,
    )
    .await
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
async fn create_class(app: &axum::Router, cookie: &str, name: &str, grade: &str) -> String {
    let res = send(
        app,
        "POST",
        "/classes",
        Some(cookie),
        Some(json!({ "name": name, "grade": grade })),
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

/// A course with a seat `capacity`, which `create_course` cannot express.
async fn create_capped_course(app: &axum::Router, cookie: &str, title: &str, cap: i64) -> String {
    let res = send(
        app,
        "POST",
        "/courses",
        Some(cookie),
        Some(json!({ "title": title, "capacity": cap })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "create course {title}");
    res.body["id"].as_str().expect("course id").to_string()
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
        Some(json!({ "grade": "9", "course_ids": [algebra.clone()] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);
    assert_eq!(made.body["blueprint"]["grade"], "9");
    assert_eq!(made.body["blueprint"]["courses"][0], algebra.as_str());
    assert!(skips(&made).is_empty(), "no class exists to skip");

    // The grade is the record key, so a second blueprint for it is a 409 the
    // store decides rather than a find-then-insert two managers can race.
    let again = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade": "9", "course_ids": [] })),
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
    assert_eq!(rows("SELECT VALUE id FROM class_blueprint", &db).await, 0);
    let gone = send(&app, "GET", "/classes/blueprints/9", Some(&manager), None).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);

    // An unaddressable grade is a 400, not a record nobody can ever read back.
    for bad in ["", "9/A"] {
        let refused = send(
            &app,
            "POST",
            "/classes/blueprints",
            Some(&manager),
            Some(json!({ "grade": bad, "course_ids": [] })),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::BAD_REQUEST,
            "grade {bad:?}: {:?}",
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
        Some(json!({ "grade": "9", "course_ids": [algebra.clone(), physics.clone()] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED);

    let class = create_class(&app, &manager, "9-A", "9").await;
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
            Some("9".to_string()),
            "the blueprint must own what it attached"
        );
        assert_eq!(
            rows(
                &format!(
                    "SELECT VALUE id FROM enrollment WHERE course = course:{course} \
                     AND user = user:{student}"
                ),
                &db
            )
            .await,
            1,
            "stocking a class enrolls its roster, in real rows"
        );
    }
    assert_eq!(
        counter(
            &format!("SELECT VALUE (class_course_count ?? 0) FROM class_group:{class}"),
            &db
        )
        .await,
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
    assert_eq!(rows("SELECT VALUE id FROM class_course", &db).await, 2);

    // A class whose grade no blueprint covers is a 404, not an empty success.
    let other = create_class(&app, &manager, "10-A", "10").await;
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
        Some(json!({ "grade": "9", "course_ids": [algebra.clone(), physics.clone()] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);

    let res = send(
        &app,
        "POST",
        "/classes",
        Some(&manager),
        Some(json!({ "name": "9-A", "grade": "9" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{:?}", res.body);
    assert_eq!(
        res.body["stocked_from"], "9",
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
            Some("9".to_string()),
            "the blueprint must own what a create attached, exactly as a pump does"
        );
    }
    assert_eq!(
        counter(
            &format!("SELECT VALUE (class_course_count ?? 0) FROM class_group:{class}"),
            &db
        )
        .await,
        2
    );

    // A class is empty at create, so this is the first place the enrollment
    // rows the attachments owe can be seen: a member added now is enrolled into
    // both courses, in real rows.
    let student = student_in(&app, &db, &class, &manager, "ali").await;
    for course in [&algebra, &physics] {
        assert_eq!(
            rows(
                &format!(
                    "SELECT VALUE id FROM enrollment WHERE course = course:{course} \
                     AND user = user:{student}"
                ),
                &db
            )
            .await,
            1,
            "a stocked class enrolls its roster like any other"
        );
    }

    // A grade no template covers, and a class with no grade at all: `null`,
    // which is the one thing an empty `skipped` could never say.
    for body in [
        json!({ "name": "10-A", "grade": "10" }),
        json!({ "name": "satranç", "grade": "" }),
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
            rows(
                &format!("SELECT VALUE id FROM class_course WHERE class = class_group:{bare}"),
                &db
            )
            .await,
            0,
            "nothing may be attached to a section no template reached"
        );
    }
}

/// A create whose stocking cannot place everything still creates the class: the
/// pair that did not fit comes back in `skipped`, best-effort exactly like
/// every other pump here.
///
/// The dead course is forged in the store rather than deleted through the API,
/// because `DELETE /courses/{id}` now takes the id out of every template naming
/// it (see `deleting_a_course_takes_it_out_of_every_blueprint`) and would leave
/// nothing to skip. What is left here is exactly the state that still occurs: a
/// row written before that cascade existed, and the pump's own window — a
/// course deleted after the pump read the list it walks.
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
        Some(json!({ "grade": "9", "course_ids": [algebra.clone(), physics.clone()] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);

    // Take a course out from under the template, leaving its id in the list —
    // the state the delete's own sweep no longer produces, and the one a pump
    // meets when a course goes after it read the list. Nothing carries it yet
    // (no section exists at grade 9), so this is the pair that cannot fit when
    // the first one is created.
    db.query(format!("DELETE course:{physics}"))
        .await
        .unwrap()
        .check()
        .unwrap();
    assert!(
        templated("9", &physics, &db).await,
        "the id is still listed"
    );

    let res = send(
        &app,
        "POST",
        "/classes",
        Some(&manager),
        Some(json!({ "name": "9-A", "grade": "9" })),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "a pair that will not fit may not cost the class: {:?}",
        res.body
    );
    assert_eq!(res.body["stocked_from"], "9");
    assert_eq!(skips(&res).len(), 1, "{:?}", res.body);
    assert_eq!(skips(&res)[0]["reason"], "course_deleted");
    assert_eq!(skips(&res)[0]["course"], physics.as_str());
    assert_eq!(skips(&res)[0]["class_name"], "9-A");

    let class = res.body["class"]["id"]
        .as_str()
        .expect("class id")
        .to_string();
    assert_eq!(
        rows(
            &format!("SELECT VALUE id FROM class_group WHERE id = class_group:{class}"),
            &db
        )
        .await,
        1,
        "the class is really there — the skip is a report, not a rollback"
    );
    assert!(
        attached(&class, &algebra, &db).await,
        "…and the course that did fit is on it"
    );
    assert_eq!(
        rows(
            &format!("SELECT VALUE id FROM class_course WHERE class = class_group:{class}"),
            &db
        )
        .await,
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
    let a = create_class(&app, &manager, "9-A", "9").await;
    let b = create_class(&app, &manager, "9-B", "9").await;
    let ten = create_class(&app, &manager, "10-A", "10").await;
    let ali = student_in(&app, &db, &a, &manager, "ali").await;
    student_in(&app, &db, &ten, &manager, "veli").await;

    // Created empty, so the create's own pump has nothing to place…
    let made = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade": "9", "course_ids": [] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED);
    assert_eq!(rows("SELECT VALUE id FROM class_course", &db).await, 0);

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
            counter(
                &format!("SELECT VALUE (class_course_count ?? 0) FROM class_group:{class}"),
                &db
            )
            .await,
            1
        );
    }
    assert_eq!(
        counter(
            &format!("SELECT VALUE (class_course_count ?? 0) FROM class_group:{ten}"),
            &db
        )
        .await,
        0
    );
    assert_eq!(
        counter(
            &format!("SELECT VALUE (enrollment_count ?? 0) FROM course:{algebra}"),
            &db
        )
        .await,
        1,
        "one student at the grade, one seat"
    );
    assert_eq!(
        rows(
            &format!("SELECT VALUE id FROM enrollment WHERE user = user:{ali}"),
            &db
        )
        .await,
        1
    );
}

/// Best-effort, the ruling that accepts a partial state: a section that cannot
/// take a course is skipped and *reported*, the others are still stocked, and
/// the skipped one has not moved a single counter.
#[tokio::test]
async fn a_class_that_does_not_fit_is_reported_not_aborted() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    // One seat, and a section with two students — the attach cannot hold the
    // whole class, so the pump's own transaction refuses it whole.
    let tight = create_capped_course(&app, &manager, "seminar", 1).await;
    let full = create_class(&app, &manager, "9-A", "9").await;
    let fits = create_class(&app, &manager, "9-B", "9").await;
    student_in(&app, &db, &full, &manager, "ali").await;
    student_in(&app, &db, &full, &manager, "veli").await;

    let made = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade": "9", "course_ids": [tight.clone()] })),
    )
    .await;
    assert_eq!(
        made.status,
        StatusCode::CREATED,
        "one class that does not fit may not fail the whole edit: {:?}",
        made.body
    );
    let skipped = skips(&made);
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert_eq!(skipped[0]["class"], full.as_str());
    assert_eq!(skipped[0]["class_name"], "9-A", "name the section");
    assert_eq!(skipped[0]["course"], tight.as_str());
    assert_eq!(
        skipped[0]["reason"], "course_full",
        "the reason must be the exact machine code a client branches on"
    );

    assert!(
        attached(&fits, &tight, &db).await,
        "the class that fits is still stocked"
    );
    assert!(!attached(&full, &tight, &db).await);
    // Nothing moved on the skipped class: not its attachment counter, and not
    // one of the seats the refused attach touched on its way to the refusal.
    assert_eq!(
        counter(
            &format!("SELECT VALUE (class_course_count ?? 0) FROM class_group:{full}"),
            &db
        )
        .await,
        0
    );
    assert_eq!(
        counter(
            &format!("SELECT VALUE (enrollment_count ?? 0) FROM course:{tight}"),
            &db
        )
        .await,
        0,
        "the empty class took no seat, and the refused one gave every seat back"
    );
    assert_eq!(rows("SELECT VALUE id FROM enrollment", &db).await, 0);
}

/// One cause, one code, whichever door it came through: the `409` a manager's
/// own `POST /classes/{id}/courses` answers carries the *same* machine code the
/// pump reports as a skip for the state that class is in. The two used to be a
/// code and an English sentence, and a bilingual client cannot translate the
/// sentence — so this pins them equal, over HTTP, on one shared state.
#[tokio::test]
async fn a_manual_attach_answers_the_code_the_pump_reports() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    // One seat, two students: neither door can put this course on this class.
    let tight = create_capped_course(&app, &manager, "seminar", 1).await;
    let class = create_class(&app, &manager, "9-A", "9").await;
    student_in(&app, &db, &class, &manager, "ali").await;
    student_in(&app, &db, &class, &manager, "veli").await;

    let by_hand = send(
        &app,
        "POST",
        &format!("/classes/{class}/courses"),
        Some(&manager),
        Some(json!({ "course_id": tight.clone() })),
    )
    .await;
    assert_eq!(by_hand.status, StatusCode::CONFLICT, "{:?}", by_hand.body);
    assert_eq!(
        by_hand.body["code"], "course_full",
        "the manual 409 must carry the machine code, not prose alone: {:?}",
        by_hand.body
    );
    assert!(
        by_hand.body["error"].as_str().unwrap_or_default().len() > 10,
        "…beside the sentence it always sent — the code is additive: {:?}",
        by_hand.body
    );

    // The same refusal through the pump, on the very same class and course.
    let pumped = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade": "9", "course_ids": [tight.clone()] })),
    )
    .await;
    assert_eq!(pumped.status, StatusCode::CREATED, "{:?}", pumped.body);
    let skipped = skips(&pumped);
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert_eq!(
        skipped[0]["reason"], by_hand.body["code"],
        "one cause must read the same in a skip list and on a manual 409"
    );

    // And the other end of the vocabulary: a duplicate, which *is* a refusal by
    // hand and deliberately not a skip for the pump.
    let roomy = create_course(&app, &manager, "algebra").await;
    let attached = send(
        &app,
        "POST",
        &format!("/classes/{class}/courses"),
        Some(&manager),
        Some(json!({ "course_id": roomy.clone() })),
    )
    .await;
    assert_eq!(attached.status, StatusCode::CREATED, "{:?}", attached.body);
    let again = send(
        &app,
        "POST",
        &format!("/classes/{class}/courses"),
        Some(&manager),
        Some(json!({ "course_id": roomy })),
    )
    .await;
    assert_eq!(again.status, StatusCode::CONFLICT, "{:?}", again.body);
    assert_eq!(again.body["code"], "duplicate", "{:?}", again.body);
}

/// The provenance rule, both halves: dropping a course from the template
/// detaches it wherever the *blueprint* attached it, and leaves it standing
/// wherever a human did.
#[tokio::test]
async fn a_removal_spares_a_hand_attached_course() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let algebra = create_course(&app, &manager, "algebra").await;
    let pumped = create_class(&app, &manager, "9-A", "9").await;
    let byhand = create_class(&app, &manager, "9-B", "9").await;
    let ali = student_in(&app, &db, &pumped, &manager, "ali").await;
    student_in(&app, &db, &byhand, &manager, "veli").await;

    // 9-B gets algebra the old way, *before* the blueprint exists.
    let attached_by_hand = send(
        &app,
        "POST",
        &format!("/classes/{byhand}/courses"),
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
        Some(json!({ "grade": "9", "course_ids": [algebra.clone()] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED);
    assert!(skips(&made).is_empty(), "{:?}", made.body);
    assert_eq!(
        source_of(&byhand, &algebra, &db).await,
        None,
        "a pump may not adopt an attachment it did not make"
    );
    assert_eq!(source_of(&pumped, &algebra, &db).await, Some("9".into()));

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
        counter(
            &format!("SELECT VALUE (class_course_count ?? 0) FROM class_group:{pumped}"),
            &db
        )
        .await,
        0,
        "the detach gives the class its count back"
    );
    assert_eq!(
        counter(
            &format!("SELECT VALUE (class_course_count ?? 0) FROM class_group:{byhand}"),
            &db
        )
        .await,
        1
    );
    assert_eq!(
        rows(
            &format!("SELECT VALUE id FROM enrollment WHERE user = user:{ali}"),
            &db
        )
        .await,
        0,
        "the enrollments the blueprint pumped go with it"
    );
    assert_eq!(
        counter(
            &format!("SELECT VALUE (enrollment_count ?? 0) FROM course:{algebra}"),
            &db
        )
        .await,
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
    let class = create_class(&app, &manager, "9-A", "9").await;
    student_in(&app, &db, &class, &manager, "ali").await;
    let attached_by_hand = send(
        &app,
        "POST",
        &format!("/classes/{class}/courses"),
        Some(&manager),
        Some(json!({ "course_id": algebra.clone() })),
    )
    .await;
    assert_eq!(attached_by_hand.status, StatusCode::CREATED);
    // Age the row to before the column existed. `UNSET`, not `= NONE`: an
    // absent key is what the store holds today, and it is what the rule reads.
    db.query("UPDATE class_course UNSET source")
        .await
        .unwrap()
        .check()
        .unwrap();
    assert_eq!(source_of(&class, &algebra, &db).await, None);

    // Every read still decodes it…
    let listed = send(
        &app,
        "GET",
        &format!("/classes/{class}/courses"),
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
        Some(json!({ "grade": "9", "course_ids": [algebra.clone()] })),
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
    assert_eq!(
        counter(
            &format!("SELECT VALUE (enrollment_count ?? 0) FROM course:{algebra}"),
            &db
        )
        .await,
        1
    );
}

/// The window the lock exists to close, made visible.
///
/// `ClassBlueprint::delete` removes the row and then sweeps by the provenance
/// tag, while a pump reads that row inside the transaction that writes the link
/// — a cross-record read-then-write the store does not serialize. A pump that
/// passed its liveness claim and then lost the blueprint commits a
/// `class_course` row tagged with a record nothing can reach, since the grade
/// label *is* the id and no sweep will ever run for it again.
///
/// Racing two requests would be a coin flip the in-memory engine lies about, so
/// the delete is injected by the database itself: a `DEFINE EVENT` on
/// `class_course` fires *inside* the pump's own transaction, the instant the
/// link row lands — which is exactly "after the guard passed, before the
/// commit", every single time. This is a below-the-lock probe by construction
/// (the store deletes the row, not `ClassBlueprint::delete`), so it does not
/// test `BLUEPRINT_LOCK`; it pins the end state that lock prevents in-process,
/// and the recovery contract that covers the one residue it cannot — a process
/// crash between the delete and its sweep.
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
        Some(json!({ "grade": "9", "course_ids": [algebra.clone()] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);
    // The section has to reach grade 9 without being stocked on the way, so
    // that the pump below is the *first* attach: a section created at the grade
    // is stocked by its own create, and one that existed already was stocked by
    // the template's. Moving it there afterwards is neither — a grade change is
    // deliberately a blueprint no-op.
    let class = create_class(&app, &manager, "9-A", "10").await;
    let moved = send(
        &app,
        "PATCH",
        &format!("/classes/{class}"),
        Some(&manager),
        Some(json!({ "grade": "9" })),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK, "{:?}", moved.body);
    assert!(!attached(&class, &algebra, &db).await);

    // Delete the blueprint from inside the very write that attaches on its
    // behalf.
    db.query(
        "DEFINE EVENT lose_blueprint ON TABLE class_course WHEN $event = 'CREATE' \
         THEN { DELETE type::record('class_blueprint', '9'); };",
    )
    .await
    .unwrap()
    .check()
    .unwrap();

    let pumped = send(
        &app,
        "POST",
        &format!("/classes/{class}/blueprint"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(pumped.status, StatusCode::OK, "{:?}", pumped.body);
    assert!(
        skips(&pumped).is_empty(),
        "the pump was told it succeeded — which is what makes the row below \
         invisible to the caller: {:?}",
        pumped.body
    );

    assert_eq!(
        rows("SELECT VALUE id FROM class_blueprint", &db).await,
        0,
        "the injected delete really landed"
    );
    assert!(
        attached(&class, &algebra, &db).await,
        "the link committed after its blueprint was gone: this is the stranded \
         row, tagged with a record no sweep can ever reach"
    );
    assert_eq!(source_of(&class, &algebra, &db).await, Some("9".into()));

    // The documented recovery: a human detaches it one course at a time, and
    // the counters come back exact.
    let detached = send(
        &app,
        "DELETE",
        &format!("/classes/{class}/courses/{algebra}"),
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
        counter(
            &format!("SELECT VALUE (class_course_count ?? 0) FROM class_group:{class}"),
            &db
        )
        .await,
        0,
        "a stranded row still releases its counter when it is detached"
    );
}

/// The silent miss `matched` exists for: a grade label is free text and matched
/// exactly, so a template keyed `"9 "` reaches none of the sections keyed `"9"`
/// — and it says so with an empty `skipped`, which is the same body a template
/// that stocked every section returns.
///
/// Both halves are asserted from the same section, one label apart, because the
/// count only means anything against the case that *does* reach it: a `matched`
/// wired to the course list would claim 1 on the typo, and one wired to the
/// skip count would answer 0 on the label that works.
#[tokio::test]
async fn a_grade_label_nothing_carries_reports_matched_zero() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let algebra = create_course(&app, &manager, "algebra").await;
    // The section exists first, so a template that finds it stocks it on
    // create.
    let class = create_class(&app, &manager, "9-A", "9").await;

    let typo = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade": "9 ", "course_ids": [algebra.clone()] })),
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
        "no section carries \"9 \", and that must not read as success: {:?}",
        typo.body
    );
    assert!(
        !attached(&class, &algebra, &db).await,
        "a trailing space really is a different grade"
    );

    let right = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade": "9", "course_ids": [algebra.clone()] })),
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
/// The store is asserted alongside the body — the in-memory engine forges wins,
/// and a status read agreeing with a `class_course` table that says otherwise
/// would be worse than no read at all.
#[tokio::test]
async fn a_status_read_names_the_course_a_section_is_short() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let algebra = create_course(&app, &manager, "algebra").await;
    // One seat, and a section with two students: the attach cannot hold the
    // whole class, so that (section, course) pair is skipped.
    let tight = create_capped_course(&app, &manager, "seminar", 1).await;
    let short = create_class(&app, &manager, "9-A", "9").await;
    let full = create_class(&app, &manager, "9-B", "9").await;
    let ali = student_in(&app, &db, &short, &manager, "ali").await;
    student_in(&app, &db, &short, &manager, "veli").await;

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
        Some(json!({ "grade": "9", "course_ids": [algebra.clone(), tight.clone()] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);
    assert_eq!(skips(&made).len(), 1, "{:?}", made.body);

    let drifted = send(
        &app,
        "GET",
        "/classes/blueprints/9/status",
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(drifted.status, StatusCode::OK, "{:?}", drifted.body);
    assert_eq!(drifted.body["grade"], "9");
    assert_eq!(drifted.body["matched"], 2, "both sections carry the label");
    assert_eq!(
        missing(&drifted, &short),
        vec![tight.clone()],
        "exactly the pair the pump refused — not the course it did place: {:?}",
        drifted.body
    );
    assert_eq!(section(&drifted, &short)["class_name"], "9-A");
    assert!(
        missing(&drifted, &full).is_empty(),
        "the section that took the whole list is in sync: {:?}",
        drifted.body
    );
    // …and the report agrees with the store on both halves.
    assert!(attached(&short, &algebra, &db).await);
    assert!(!attached(&short, &tight, &db).await);

    // Now a human fixes it: one student out (so the seat fits) and the course
    // attached by hand, carrying no blueprint tag at all.
    let removed = send(
        &app,
        "DELETE",
        &format!("/classes/{short}/members/{ali}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);
    let byhand = send(
        &app,
        "POST",
        &format!("/classes/{short}/courses"),
        Some(&manager),
        Some(json!({ "course_id": tight.clone() })),
    )
    .await;
    assert_eq!(byhand.status, StatusCode::CREATED, "{:?}", byhand.body);
    assert_eq!(
        source_of(&short, &tight, &db).await,
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

/// Deleting a course takes its id out of every template holding it, in the
/// cascade that detaches it from the sections.
///
/// Without that sweep the id stayed in the list forever and every doc surface
/// lied: `PATCH`ing the template back **as it stands** is the documented
/// self-heal, and it answered `400` ("one of these courses does not exist"),
/// because the handler resolves the ids the *request* names — the very ones it
/// had just read back. So the assertion that matters is not only that the store
/// is clean but that the round trip a manager is told to make succeeds.
#[tokio::test]
async fn deleting_a_course_takes_it_out_of_every_blueprint() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let algebra = create_course(&app, &manager, "algebra").await;
    let history = create_course(&app, &manager, "history").await;
    // No students: the section is here so the pump has something to walk, and
    // an empty roster is what lets the course be deleted at all (one with a
    // student on it is a 409).
    let class = create_class(&app, &manager, "9-A", "9").await;

    let made = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade": "9", "course_ids": [algebra.clone(), history.clone()] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);
    assert!(attached(&class, &history, &db).await);

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
        !templated("9", &history, &db).await,
        "the deleted course is out of the template"
    );
    assert!(
        templated("9", &algebra, &db).await,
        "…and nothing else is — the sweep names one course"
    );
    assert!(
        !attached(&class, &history, &db).await,
        "the section's link goes in the same cascade"
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
        Some(json!({ "grade": "11", "course_ids": [algebra.clone()] })),
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
        !templated("11", &algebra, &db).await,
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
    let class = create_class(&app, &manager, "9-A", "9").await;
    let ali = student_in(&app, &db, &class, &manager, "ali").await;

    let made = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade": "9", "course_ids": [algebra.clone(), history.clone()] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);
    assert!(attached(&class, &history, &db).await);

    // The state a sweep that died half-way leaves: the list is stored without
    // history, its attachment is not.
    db.query(
        "UPDATE type::record('class_blueprint', '9') SET courses = [type::record('course', $c)]",
    )
    .bind(("c", algebra.clone()))
    .await
    .unwrap()
    .check()
    .unwrap();

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
        rows(
            &format!("SELECT VALUE id FROM enrollment WHERE user = user:{ali} AND course = course:{history}"),
            &db
        )
        .await,
        0,
        "…enrollments and all"
    );
    assert_eq!(
        counter(
            &format!("SELECT VALUE (enrollment_count ?? 0) FROM course:{history}"),
            &db
        )
        .await,
        0,
        "…with the seat given back"
    );
    assert!(
        attached(&class, &algebra, &db).await,
        "and the course the template still holds is untouched"
    );
    assert_eq!(
        rows(
            &format!("SELECT VALUE id FROM enrollment WHERE user = user:{ali} AND course = course:{algebra}"),
            &db
        )
        .await,
        1
    );
}

/// A course deleted between the handler's own pre-flight read and the pump that
/// walks the list is pruned out of the stored template — and the body that
/// pruned it must say so. It used to be rendered from the pre-prune handle, so
/// the `201`/`200` listed a course the `GET` a moment later did not.
///
/// Both write routes are driven, and the window is opened by the schema rather
/// than by a lucky interleaving: a `DEFINE EVENT` on `class_blueprint` fires
/// inside the very transaction that stores the list, which is exactly "after
/// the courses were resolved, before the pump runs", every single time.
#[tokio::test]
async fn a_course_pruned_mid_pump_is_out_of_the_body_that_pruned_it() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    let algebra = create_course(&app, &manager, "algebra").await;
    let history = create_course(&app, &manager, "history").await;
    let class = create_class(&app, &manager, "9-A", "9").await;

    db.query(format!(
        "DEFINE EVENT kill_on_create ON TABLE class_blueprint WHEN $event = 'CREATE' \
         THEN {{ DELETE type::record('course', '{algebra}'); }};"
    ))
    .await
    .unwrap()
    .check()
    .unwrap();
    let made = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade": "9", "course_ids": [algebra.clone()] })),
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
    db.query(format!(
        "DEFINE EVENT kill_on_update ON TABLE class_blueprint WHEN $event = 'UPDATE' \
         THEN {{ DELETE type::record('course', '{history}'); }};"
    ))
    .await
    .unwrap()
    .check()
    .unwrap();
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
    let ghost = CourseId::from_key("01J8XZ0K3Q8G7X2M4N5P6R7S8T");

    let refused = ClassBlueprint::create(
        &by,
        ClassBlueprint::grade_key("9").unwrap(),
        vec![ghost.clone()],
        &db,
    )
    .await;
    assert!(
        matches!(refused, Err(AppError::Validation(_))),
        "a course that is gone is a 400, not a template naming it: {refused:?}"
    );
    assert_eq!(
        rows("SELECT VALUE id FROM class_blueprint", &db).await,
        0,
        "…and nothing may be stored"
    );

    let blueprint = ClassBlueprint::create(
        &by,
        ClassBlueprint::grade_key("9").unwrap(),
        vec![algebra.clone()],
        &db,
    )
    .await
    .unwrap();
    let refused = blueprint
        .set_courses(vec![algebra.clone(), ghost], &by, &db)
        .await;
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
