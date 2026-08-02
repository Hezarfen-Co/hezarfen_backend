//! Regressions for the class (şube) layer, its enrollment side, and the sweep a
//! role change runs. Every test here pins a defect that shipped: a link left
//! pointing at a deleted course, the refusal that misnamed it, a hand enroll a
//! class could still undo, and the two-transaction role sweep.

mod common;

use axum::http::StatusCode;
use common::{app_and_db, create_course, login_as, me_id, send};
use hezarfen_backend::constant::{MAX_CLASS_COURSES, MAX_CLASS_MEMBERS};
use hezarfen_backend::database::Database;
use hezarfen_backend::domain::class_course::ClassCourse;
use hezarfen_backend::domain::class_group::{ClassGroup, ClassName};
use hezarfen_backend::domain::class_member::ClassMember;
use hezarfen_backend::domain::course::CourseId;
use hezarfen_backend::domain::user::UserId;
use hezarfen_backend::error::AppError;
use serde_json::json;

/// One counter, re-read out of the store — never off a response body.
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

/// Delete a course row the way a *concurrent* `DELETE /courses/{id}` racing an
/// attach leaves the store: the row goes, the `class_course` link it never saw
/// stays. The real cascade sweeps those links, so this is the only way to reach
/// the state from a test — and the state the pump's in-transaction claim exists
/// to make unreachable in the first place.
async fn wipe_course_row(course: &str, db: &Database) {
    db.query("DELETE type::record('course', $key)")
        .bind(("key", course.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
}

/// B1(a). A class with *zero* members makes the pump's pair loop empty, so the
/// attach transaction touched no row a concurrent course delete wrote and
/// SurrealDB's write-skew let both commit — a `class_course` row pointing at a
/// course that does not exist. The pivot claim is what makes that impossible.
#[tokio::test]
async fn an_attach_onto_a_deleted_course_writes_no_link() {
    let (_app, db) = app_and_db().await;
    let manager = UserId::from_key("manager");
    let class = ClassGroup::create(
        &manager,
        ClassName::try_new("9-A").unwrap(),
        None,
        None,
        &db,
    )
    .await
    .unwrap();
    // A course id nothing ever created: the same row a delete that won the race
    // leaves behind.
    let ghost = CourseId::from_key("01J8XZ0K3Q8G7X2M4N5P6R7S8T");

    let refused = ClassCourse::attach(class.get_id(), &ghost, &manager, &db).await;
    assert!(
        refused.is_err(),
        "attaching a course that is gone must be refused: {refused:?}"
    );
    assert_eq!(
        rows("SELECT VALUE id FROM class_course", &db).await,
        0,
        "no link row may point at a course that does not exist"
    );
    assert_eq!(
        counter("SELECT VALUE class_course_count ?? 0 FROM class_group", &db).await,
        0,
        "…and the class must not count one either, or it is undeletable forever"
    );
}

/// B1(b). The other half: a link that *did* land that way must still be
/// removable. `DELETE /classes/{c}/courses/{id}` read the course before the
/// link, so a deleted course made the route answer 404 forever — and the class
/// 409 forever, over an attachment nothing could sweep.
#[tokio::test]
async fn a_stale_link_detaches_and_frees_its_class() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "manager", "manager").await;
    let course = create_course(&app, &manager, "algebra").await;
    let class = {
        let res = send(
            &app,
            "POST",
            "/classes",
            Some(&manager),
            Some(json!({ "name": "9-A" })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED);
        res.body["id"].as_str().unwrap().to_string()
    };
    let attached = send(
        &app,
        "POST",
        &format!("/classes/{class}/courses"),
        Some(&manager),
        Some(json!({ "course_id": course })),
    )
    .await;
    assert_eq!(attached.status, StatusCode::CREATED);
    wipe_course_row(&course, &db).await;

    let detached = send(
        &app,
        "DELETE",
        &format!("/classes/{class}/courses/{course}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(
        detached.status,
        StatusCode::NO_CONTENT,
        "a link whose course is gone must still detach: {:?}",
        detached.body
    );
    let deleted = send(
        &app,
        "DELETE",
        &format!("/classes/{class}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(
        deleted.status,
        StatusCode::NO_CONTENT,
        "…and the class it blocked must be deletable again: {:?}",
        deleted.body
    );

    // A course that was never attached is still a 404 — the guard that turns
    // "sweep the stale link" into "delete anything" is the link row, not the
    // course row.
    let again = send(
        &app,
        "DELETE",
        &format!("/classes/{class}/courses/{course}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
}

/// B2. Every member add into a class holding such a link answered
/// `409 "<course> is full"` — a course that does not exist, so no capacity
/// anyone could raise would ever let a student in. The two causes come out of
/// one seat claim matching nothing, and they must not read as one answer.
#[tokio::test]
async fn a_member_add_names_a_stale_link_rather_than_calling_it_full() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "manager", "manager").await;
    let student = login_as(&app, &db, "student", "student").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &manager, "algebra").await;
    let class = {
        let res = send(
            &app,
            "POST",
            "/classes",
            Some(&manager),
            Some(json!({ "name": "9-A" })),
        )
        .await;
        res.body["id"].as_str().unwrap().to_string()
    };
    send(
        &app,
        "POST",
        &format!("/classes/{class}/courses"),
        Some(&manager),
        Some(json!({ "course_id": course })),
    )
    .await;
    wipe_course_row(&course, &db).await;

    let refused = send(
        &app,
        "POST",
        &format!("/classes/{class}/members"),
        Some(&manager),
        Some(json!({ "user_id": student_id })),
    )
    .await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    let message = refused.body["error"].as_str().unwrap_or_default();
    assert!(
        message.contains("no longer exists") && message.contains(&course),
        "the refusal must name the vanished course, not a capacity: {message}"
    );
    assert!(
        !message.contains("full"),
        "a course that does not exist is not a full one: {message}"
    );
    assert_eq!(
        rows("SELECT VALUE id FROM class_member", &db).await,
        0,
        "a refused add writes nothing"
    );
}

/// B3. A hand enroll over a row a class pumped must take the row off the class,
/// or the next sweep unenrolls a student an operator placed on purpose —
/// the mirror of the manual unenroll that a class may never undo.
#[tokio::test]
async fn a_hand_enroll_takes_the_row_off_the_class() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "manager", "manager").await;
    let student = login_as(&app, &db, "student", "student").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &manager, "algebra").await;
    let class = {
        let res = send(
            &app,
            "POST",
            "/classes",
            Some(&manager),
            Some(json!({ "name": "9-A" })),
        )
        .await;
        res.body["id"].as_str().unwrap().to_string()
    };
    send(
        &app,
        "POST",
        &format!("/classes/{class}/courses"),
        Some(&manager),
        Some(json!({ "course_id": course })),
    )
    .await;
    let added = send(
        &app,
        "POST",
        &format!("/classes/{class}/members"),
        Some(&manager),
        Some(json!({ "user_id": student_id })),
    )
    .await;
    assert_eq!(added.status, StatusCode::CREATED);

    // The pumped row names its class, and says so on the wire — a client that
    // cannot see `source` cannot tell which of its roster rows is sweepable.
    let roster = send(
        &app,
        "GET",
        &format!("/courses/{course}/enrollments"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(roster.body["items"][0]["source"], json!(class));

    let by_hand = send(
        &app,
        "POST",
        &format!("/courses/{course}/enrollments"),
        Some(&manager),
        Some(json!({ "user_id": student_id })),
    )
    .await;
    assert_eq!(by_hand.status, StatusCode::OK);
    assert_eq!(
        by_hand.body["source"],
        json!(null),
        "a hand enroll must take the row off the class"
    );
    assert_eq!(
        rows("SELECT VALUE id FROM enrollment WHERE source != NONE", &db).await,
        0,
        "…in the stored row, not just the response"
    );

    // Which is the whole point: the class letting the student go now leaves the
    // hand-placed row standing, with its seat still counted.
    let removed = send(
        &app,
        "DELETE",
        &format!("/classes/{class}/members/{student_id}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);
    assert_eq!(
        rows("SELECT VALUE id FROM enrollment", &db).await,
        1,
        "the class may not unenroll a student an operator enrolled by hand"
    );
    assert_eq!(
        counter("SELECT VALUE enrollment_count ?? 0 FROM course", &db).await,
        1,
        "…and the seat it never paid for may not be released"
    );
}

/// B5. The role sweep is one transaction over both tables. Pinned end to end:
/// a promotion leaves no membership, no enrollment and no counter behind, so
/// the class is deletable and no row is left tagged with it. (The half-apply
/// itself needs a crash between two statements, which no test can schedule —
/// this pins the state the fold must always land in.)
#[tokio::test]
async fn a_role_change_sweeps_memberships_and_enrollments_together() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "patron", "admin").await;
    let manager = login_as(&app, &db, "manager", "manager").await;
    let student = login_as(&app, &db, "student", "student").await;
    let student_id = me_id(&app, &student).await;
    let course = create_course(&app, &manager, "algebra").await;
    let class = {
        let res = send(
            &app,
            "POST",
            "/classes",
            Some(&manager),
            Some(json!({ "name": "9-A" })),
        )
        .await;
        res.body["id"].as_str().unwrap().to_string()
    };
    send(
        &app,
        "POST",
        &format!("/classes/{class}/courses"),
        Some(&manager),
        Some(json!({ "course_id": course })),
    )
    .await;
    send(
        &app,
        "POST",
        &format!("/classes/{class}/members"),
        Some(&manager),
        Some(json!({ "user_id": student_id })),
    )
    .await;
    assert_eq!(rows("SELECT VALUE id FROM enrollment", &db).await, 1);

    let promoted = send(
        &app,
        "PATCH",
        &format!("/users/{student_id}/role"),
        Some(&admin),
        Some(json!({ "role": "teacher" })),
    )
    .await;
    assert_eq!(promoted.status, StatusCode::OK);

    assert_eq!(rows("SELECT VALUE id FROM class_member", &db).await, 0);
    assert_eq!(
        rows("SELECT VALUE id FROM enrollment", &db).await,
        0,
        "a non-student holds no roster row, whoever wrote it"
    );
    assert_eq!(
        counter("SELECT VALUE enrollment_count ?? 0 FROM course", &db).await,
        0,
        "the seat comes back exactly once"
    );
    assert_eq!(
        counter("SELECT VALUE class_member_count ?? 0 FROM class_group", &db).await,
        0
    );
    // Nothing is left tagged with the class, so letting it go strands nothing.
    let detached = send(
        &app,
        "DELETE",
        &format!("/classes/{class}/courses/{course}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(detached.status, StatusCode::NO_CONTENT);
    let deleted = send(
        &app,
        "DELETE",
        &format!("/classes/{class}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
}

/// B4. The roster cap is claimed off the course row's *own* `capacity` column
/// now, not off an integer read moments earlier — so a capacity PATCH cannot be
/// outrun by an enroll that snapshotted the old number. The over-admit that
/// bug allowed needs two writers interleaved inside one request, which the
/// in-memory engine cannot be made to schedule; what is pinned here is that the
/// converted statement still refuses, still admits, and still tells a full
/// course from a deleted one.
#[tokio::test]
async fn the_roster_cap_is_claimed_off_the_live_capacity_column() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "manager", "manager").await;
    let first = login_as(&app, &db, "sena", "student").await;
    let second = login_as(&app, &db, "suat", "student").await;
    let first_id = me_id(&app, &first).await;
    let second_id = me_id(&app, &second).await;
    let course = {
        let res = send(
            &app,
            "POST",
            "/courses",
            Some(&manager),
            Some(json!({ "title": "algebra", "capacity": 1 })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED);
        res.body["id"].as_str().unwrap().to_string()
    };
    let enroll = |who: String| {
        let app = app.clone();
        let manager = manager.clone();
        let course = course.clone();
        async move {
            send(
                &app,
                "POST",
                &format!("/courses/{course}/enrollments"),
                Some(&manager),
                Some(json!({ "user_id": who })),
            )
            .await
        }
    };

    assert_eq!(enroll(first_id.clone()).await.status, StatusCode::OK);
    assert_eq!(
        enroll(second_id.clone()).await.status,
        StatusCode::CONFLICT,
        "the cap must bite at one"
    );
    // Re-enrolling the student who is already in is answered off their row, not
    // off the (full) counter, and costs no seat.
    assert_eq!(enroll(first_id.clone()).await.status, StatusCode::OK);
    assert_eq!(
        counter("SELECT VALUE enrollment_count ?? 0 FROM course", &db).await,
        1
    );

    // Raise the cap and the very next claim sees the new number, because it
    // reads the column rather than a copy of it.
    let raised = send(
        &app,
        "PATCH",
        &format!("/courses/{course}"),
        Some(&manager),
        Some(json!({ "capacity": 2 })),
    )
    .await;
    assert_eq!(raised.status, StatusCode::OK);
    assert_eq!(enroll(second_id.clone()).await.status, StatusCode::OK);
    assert_eq!(
        counter("SELECT VALUE enrollment_count ?? 0 FROM course", &db).await,
        2
    );

    // And a course deleted out from under the claim is a 404, not a 409: the
    // conditional write matches nothing either way, and only that path pays for
    // the read that tells them apart.
    wipe_course_row(&course, &db).await;
    db.query("DELETE enrollment")
        .await
        .unwrap()
        .check()
        .unwrap();
    assert_eq!(enroll(first_id).await.status, StatusCode::NOT_FOUND);
}

/// D1. Both link tables key on the *pair* (`<class>_<user>`), so the
/// documented "newest first" was implemented as `ORDER BY id DESC` — which
/// sorts the roster by the member's account ULID, an order that has nothing to
/// do with when anyone was added. The three students here are added in an order
/// their ids do not reproduce, so id-ordering and stamp-ordering disagree.
#[tokio::test]
async fn a_roster_is_ordered_by_when_a_student_was_added() {
    let (_app, db) = app_and_db().await;
    let manager = UserId::from_key("manager");
    let class = ClassGroup::create(
        &manager,
        ClassName::try_new("9-A").unwrap(),
        None,
        None,
        &db,
    )
    .await
    .unwrap()
    .get_id()
    .clone();

    // Keys chosen so `id DESC` (c, b, a) is not the reverse of the insertion
    // order (a, c, b): only a real stamp can tell the two apart. The waits are
    // what keep the millisecond stamps distinct — a tie falls back to the id,
    // which is the very order under test.
    let added = ["a", "c", "b"];
    for key in added {
        ClassMember::add(&class, &UserId::from_key(key), &manager, &db)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }

    let (roster, _) = ClassMember::list_for_class(&class, None, 0, &db)
        .await
        .unwrap();
    let order: Vec<&str> = roster
        .iter()
        .map(|member| member.get_user().key())
        .collect();
    assert_eq!(
        order,
        vec!["b", "c", "a"],
        "the roster must come back newest-added first, not sorted by account id"
    );

    // A row written before the stamp column existed carries no `added_at` at
    // all — the state a real volume is in. It is of unknown age, and NONE
    // sorting last under DESC is what makes that the oldest, rather than an
    // invented number putting it anywhere else.
    db.query("UPDATE class_member SET added_at = NONE WHERE user = $usr")
        .bind(("usr", UserId::from_key("c").record()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let (roster, _) = ClassMember::list_for_class(&class, None, 0, &db)
        .await
        .unwrap();
    assert_eq!(
        roster
            .iter()
            .map(|member| member.get_user().key())
            .collect::<Vec<_>>(),
        vec!["b", "a", "c"],
        "a stamp-less row must sort oldest, not first"
    );
}

/// D1, the other axis: the course list had the same defect, ordered by the
/// *course's* ULID.
#[tokio::test]
async fn a_class_course_list_is_ordered_by_when_it_was_attached() {
    let (app, db) = app_and_db().await;
    let manager_cookie = login_as(&app, &db, "manager", "manager").await;
    let manager = UserId::from_key(&me_id(&app, &manager_cookie).await);
    let class = ClassGroup::create(
        &manager,
        ClassName::try_new("9-A").unwrap(),
        None,
        None,
        &db,
    )
    .await
    .unwrap()
    .get_id()
    .clone();

    // Courses are ULID-keyed, so their ids rise with creation; attaching them
    // back-to-front makes id order and attach order disagree.
    let mut courses = Vec::new();
    for title in ["first", "second", "third"] {
        courses.push(CourseId::from_key(
            &create_course(&app, &manager_cookie, title).await,
        ));
    }
    for course in courses.iter().rev() {
        ClassCourse::attach(&class, course, &manager, &db)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }

    let (attached, _) = ClassCourse::list_for_class(&class, None, 0, &db)
        .await
        .unwrap();
    let order: Vec<&str> = attached
        .iter()
        .map(|link| link.get_course().key())
        .collect();
    assert_eq!(
        order,
        courses.iter().map(|c| c.key()).collect::<Vec<_>>(),
        "the list must come back newest-attached first, not sorted by course id"
    );
}

/// D2. Attaching a course writes one enrollment per member and adding a member
/// writes one per attached course, both in a *single* transaction — with no
/// class-size ceiling anywhere, that transaction was unbounded. The counter's
/// claim now carries the bound, so the refusal happens before the loop runs.
///
/// The counter is seeded rather than filled by 200 real adds: it *is* the gate
/// the claim reads, and the boundary is what is under test, not the 199 adds
/// that lead up to it.
#[tokio::test]
async fn a_class_refuses_the_member_past_its_ceiling() {
    let (_app, db) = app_and_db().await;
    let manager = UserId::from_key("manager");
    let class = ClassGroup::create(
        &manager,
        ClassName::try_new("9-A").unwrap(),
        None,
        None,
        &db,
    )
    .await
    .unwrap()
    .get_id()
    .clone();
    db.query("UPDATE $class SET class_member_count = $at")
        .bind(("class", class.record()))
        .bind(("at", MAX_CLASS_MEMBERS - 1))
        .await
        .unwrap()
        .check()
        .unwrap();

    ClassMember::add(&class, &UserId::from_key("last"), &manager, &db)
        .await
        .expect("the place under the ceiling is still free");
    let refused = ClassMember::add(&class, &UserId::from_key("over"), &manager, &db).await;
    assert!(
        matches!(refused, Err(AppError::ConflictOwned(ref message))
            if message.contains(&MAX_CLASS_MEMBERS.to_string())),
        "a full class is a 409 naming its ceiling, never a 404: {refused:?}"
    );
    assert_eq!(
        counter("SELECT VALUE class_member_count ?? 0 FROM class_group", &db).await,
        MAX_CLASS_MEMBERS,
        "a refused add may not tick the counter past the cap"
    );
    assert_eq!(rows("SELECT VALUE id FROM class_member", &db).await, 1);

    // And the refusal a *deleted* class earns must stay a 404: both come out of
    // the same counter claim matching nothing, and only one of them is a
    // ceiling anybody can make room under.
    db.query("DELETE $class")
        .bind(("class", class.record()))
        .await
        .unwrap()
        .check()
        .unwrap();
    let gone = ClassMember::add(&class, &UserId::from_key("ghost"), &manager, &db).await;
    assert!(
        matches!(gone, Err(AppError::NotFound)),
        "a class that is gone is a 404: {gone:?}"
    );
}

/// The ceilings bind the axis being *added*, and the transaction they bound is
/// the length of the *other* one — so a class that already stands above a
/// ceiling walked straight past both. The class layer shipped before either
/// number existed, so a real volume can hold a 300-strong class, and attaching
/// one course to it wrote 300 enrollments in a single transaction: exactly the
/// unbounded write `MAX_CLASS_MEMBERS` was added to make impossible. The attach
/// now refuses on the other axis too, and says which one.
///
/// The counter is seeded rather than filled by 201 real adds: it *is* what the
/// pump reads, and it is what a stale class carries.
#[tokio::test]
async fn a_class_over_the_other_axis_ceiling_attaches_nothing() {
    let (app, db) = app_and_db().await;
    let cookie = login_as(&app, &db, "manager", "manager").await;
    let manager = UserId::from_key(&me_id(&app, &cookie).await);
    let class = ClassGroup::create(
        &manager,
        ClassName::try_new("9-A").unwrap(),
        None,
        None,
        &db,
    )
    .await
    .unwrap()
    .get_id()
    .clone();
    let algebra = CourseId::from_key(&create_course(&app, &cookie, "algebra").await);
    db.query("UPDATE $class SET class_member_count = $over")
        .bind(("class", class.record()))
        .bind(("over", MAX_CLASS_MEMBERS + 1))
        .await
        .unwrap()
        .check()
        .unwrap();

    let refused = ClassCourse::attach(&class, &algebra, &manager, &db).await;
    assert!(
        matches!(refused, Err(AppError::ConflictOwned(ref message))
            if message.contains(&MAX_CLASS_MEMBERS.to_string())),
        "the refusal must name the axis that is over, not the one with room: {refused:?}"
    );
    assert_eq!(
        rows("SELECT VALUE id FROM class_course", &db).await,
        0,
        "a refused attach may not leave the link behind"
    );
    assert_eq!(
        counter("SELECT VALUE class_course_count ?? 0 FROM class_group", &db).await,
        0,
        "…nor tick the axis it was refused on"
    );

    // Back under the ceiling, the very same attach goes through: the refusal is
    // the standing count, not the class.
    db.query("UPDATE $class SET class_member_count = $at")
        .bind(("class", class.record()))
        .bind(("at", MAX_CLASS_MEMBERS))
        .await
        .unwrap()
        .check()
        .unwrap();
    ClassCourse::attach(&class, &algebra, &manager, &db)
        .await
        .expect("a class exactly at the ceiling is still a bounded transaction");
}

/// The mirror: a class carrying more courses than `MAX_CLASS_COURSES` takes no
/// new member, because adding one enrolls them into every attached course.
#[tokio::test]
async fn a_class_over_the_course_ceiling_takes_no_member() {
    let (_app, db) = app_and_db().await;
    let manager = UserId::from_key("manager");
    let class = ClassGroup::create(
        &manager,
        ClassName::try_new("9-B").unwrap(),
        None,
        None,
        &db,
    )
    .await
    .unwrap()
    .get_id()
    .clone();
    db.query("UPDATE $class SET class_course_count = $over")
        .bind(("class", class.record()))
        .bind(("over", MAX_CLASS_COURSES + 1))
        .await
        .unwrap()
        .check()
        .unwrap();

    let refused = ClassMember::add(&class, &UserId::from_key("ali"), &manager, &db).await;
    assert!(
        matches!(refused, Err(AppError::ConflictOwned(ref message))
            if message.contains(&MAX_CLASS_COURSES.to_string())),
        "the refusal must name the courses, not the roster: {refused:?}"
    );
    assert_eq!(rows("SELECT VALUE id FROM class_member", &db).await, 0);
    assert_eq!(
        counter("SELECT VALUE class_member_count ?? 0 FROM class_group", &db).await,
        0
    );
}
