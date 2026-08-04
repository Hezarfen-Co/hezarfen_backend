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
    res.body["id"].as_str().expect("class id").to_string()
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
