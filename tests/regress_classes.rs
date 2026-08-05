//! Regressions for the class section (şube) layer, its enrollment side, and the sweep a
//! role change runs. Every test here pins a defect that shipped: a link left
//! pointing at a deleted course, the refusal that misnamed it, a hand enroll a
//! class could still undo, and the two-transaction role sweep.

mod common;

use axum::http::StatusCode;
use common::{Res, app_and_db, create_course, login_as, me_id, send, total};
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
        res.body["class"]["id"].as_str().unwrap().to_string()
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
        res.body["class"]["id"].as_str().unwrap().to_string()
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
    // …and it says so as a machine code too, the one a pump's skip list spells
    // for this cause. A bilingual client branches on this, never on the words.
    assert_eq!(
        refused.body["code"], "linked_course_missing",
        "the stale-link refusal must carry its machine code: {:?}",
        refused.body
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
        res.body["class"]["id"].as_str().unwrap().to_string()
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
        res.body["class"]["id"].as_str().unwrap().to_string()
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
        matches!(refused, Err(AppError::ConflictCoded { code, ref message })
            if code == "class_at_roster_ceiling"
                && message.contains(&MAX_CLASS_MEMBERS.to_string())),
        "a full class is a 409 naming its ceiling and coded, never a 404, and the code \
         must name the ceiling the prose does — the roster, not the course list: {refused:?}"
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
        matches!(refused, Err(AppError::ConflictCoded { code, ref message })
            if code == "class_roster_too_large"
                && message.contains(&MAX_CLASS_MEMBERS.to_string())),
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
        matches!(refused, Err(AppError::ConflictCoded { code, ref message })
            if code == "class_course_list_too_large"
                && message.contains(&MAX_CLASS_COURSES.to_string())),
        "the refusal must name the courses, not the roster — in its code as well as \
         its prose: {refused:?}"
    );
    assert_eq!(rows("SELECT VALUE id FROM class_member", &db).await, 0);
    assert_eq!(
        counter("SELECT VALUE class_member_count ?? 0 FROM class_group", &db).await,
        0
    );
}

// ---- homeroom teacher (sınıf öğretmeni) ------------------------------------

/// Create a class as `cookie` (asserts 201); returns the response with its
/// `body` narrowed to the created class — the `201` is
/// `{class, skipped, stocked_from}` since a create stocks from its grade's
/// blueprint, and the tests below are about the class itself. The two outer
/// fields have their own tests (`regress_blueprints`).
async fn create_class(app: &axum::Router, cookie: &str, body: serde_json::Value) -> Res {
    let mut res = send(app, "POST", "/classes", Some(cookie), Some(body)).await;
    assert_eq!(res.status, StatusCode::CREATED, "create class");
    res.body = res.body["class"].clone();
    res
}

/// The `teacher` block of `GET /classes/{id}`, straight out of the store's own
/// read path — never off the write's response body.
async fn stored_teacher(app: &axum::Router, cookie: &str, class: &str) -> serde_json::Value {
    let res = send(app, "GET", &format!("/classes/{class}"), Some(cookie), None).await;
    assert_eq!(res.status, StatusCode::OK, "GET /classes/{class}");
    res.body["teacher"].clone()
}

/// The whole write contract for the homeroom teacher over HTTP: set at create,
/// re-set and cleared by PATCH (both `null` and `""`), and — the one a
/// whole-row save would break — untouched by a PATCH of another field. The
/// response always names the person, never a bare id.
#[tokio::test]
async fn the_homeroom_teacher_round_trips_through_create_and_patch() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "manager", "manager").await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let other = login_as(&app, &db, "other", "teacher").await;
    let teacher_id = me_id(&app, &teacher).await;
    let other_id = me_id(&app, &other).await;

    let created = create_class(
        &app,
        &manager,
        json!({ "name": "9-A", "teacher_id": teacher_id }),
    )
    .await;
    let class = created.body["id"].as_str().unwrap().to_string();
    assert_eq!(created.body["teacher"]["id"], json!(teacher_id));
    assert_eq!(
        created.body["teacher"]["username"],
        json!("teacher"),
        "the response must name the teacher, not echo an id"
    );
    assert_eq!(
        stored_teacher(&app, &manager, &class).await["id"],
        json!(teacher_id)
    );

    // A name-only PATCH must leave the teacher exactly where it is.
    let renamed = send(
        &app,
        "PATCH",
        &format!("/classes/{class}"),
        Some(&manager),
        Some(json!({ "name": "9-B" })),
    )
    .await;
    assert_eq!(renamed.status, StatusCode::OK);
    assert_eq!(renamed.body["teacher"]["id"], json!(teacher_id));
    assert_eq!(
        stored_teacher(&app, &manager, &class).await["id"],
        json!(teacher_id)
    );

    // …and a teacher-only PATCH must leave the name alone.
    let moved = send(
        &app,
        "PATCH",
        &format!("/classes/{class}"),
        Some(&manager),
        Some(json!({ "teacher_id": other_id })),
    )
    .await;
    assert_eq!(moved.status, StatusCode::OK);
    assert_eq!(moved.body["name"], json!("9-B"));
    assert_eq!(moved.body["teacher"]["id"], json!(other_id));

    // Both spellings of "no teacher" land on the same stored row.
    for clear in [json!(null), json!("")] {
        send(
            &app,
            "PATCH",
            &format!("/classes/{class}"),
            Some(&manager),
            Some(json!({ "teacher_id": other_id })),
        )
        .await;
        let cleared = send(
            &app,
            "PATCH",
            &format!("/classes/{class}"),
            Some(&manager),
            Some(json!({ "teacher_id": clear })),
        )
        .await;
        assert_eq!(cleared.status, StatusCode::OK, "clearing with {clear}");
        assert_eq!(
            cleared.body["teacher"],
            json!(null),
            "clearing with {clear}"
        );
        assert_eq!(stored_teacher(&app, &manager, &class).await, json!(null));
    }
}

/// A student is not staff: naming one as the homeroom teacher is a `400` on
/// both write paths, and so is an id that names nobody at all.
#[tokio::test]
async fn a_student_or_a_ghost_cannot_be_the_homeroom_teacher() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "manager", "manager").await;
    let student = login_as(&app, &db, "ali", "student").await;
    let student_id = me_id(&app, &student).await;
    let ghost = "01J8XZ0K3Q8G7X2M4N5P6R7S8T";

    for bad in [student_id.as_str(), ghost] {
        let refused = send(
            &app,
            "POST",
            "/classes",
            Some(&manager),
            Some(json!({ "name": "9-A", "teacher_id": bad })),
        )
        .await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "create with {bad}");
    }
    assert_eq!(
        rows("SELECT VALUE id FROM class_group", &db).await,
        0,
        "a refused create may write no class"
    );

    let class = create_class(&app, &manager, json!({ "name": "9-A" }))
        .await
        .body["id"]
        .as_str()
        .unwrap()
        .to_string();
    for bad in [student_id.as_str(), ghost] {
        let refused = send(
            &app,
            "PATCH",
            &format!("/classes/{class}"),
            Some(&manager),
            Some(json!({ "name": "9-B", "teacher_id": bad })),
        )
        .await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "patch with {bad}");
    }
    // The refused PATCH carried a name too — it must not have landed either.
    let stored = send(
        &app,
        "GET",
        &format!("/classes/{class}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(stored.body["name"], json!("9-A"));
    assert_eq!(stored.body["teacher"], json!(null));
}

/// End-to-end demotion: `PATCH /users/{id}/role` down to `student` clears that
/// account off every class it homeroomed, and off nobody else's — a demoted
/// user may hold no section.
#[tokio::test]
async fn a_demotion_clears_the_homeroom_teacher_everywhere() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "mudur", "admin").await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let keeper = login_as(&app, &db, "keeper", "teacher").await;
    let teacher_id = me_id(&app, &teacher).await;
    let keeper_id = me_id(&app, &keeper).await;

    let mut held = Vec::new();
    for name in ["9-A", "9-B"] {
        held.push(
            create_class(
                &app,
                &admin,
                json!({ "name": name, "teacher_id": teacher_id }),
            )
            .await
            .body["id"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }
    let untouched = create_class(
        &app,
        &admin,
        json!({ "name": "10-A", "teacher_id": keeper_id }),
    )
    .await
    .body["id"]
        .as_str()
        .unwrap()
        .to_string();

    let demoted = send(
        &app,
        "PATCH",
        &format!("/users/{teacher_id}/role"),
        Some(&admin),
        Some(json!({ "role": "student" })),
    )
    .await;
    assert_eq!(demoted.status, StatusCode::OK);

    for class in &held {
        assert_eq!(
            stored_teacher(&app, &admin, class).await,
            json!(null),
            "a demoted user may not stay a class's homeroom teacher"
        );
    }
    assert_eq!(
        stored_teacher(&app, &admin, &untouched).await["id"],
        json!(keeper_id),
        "…and nobody else's assignment may move"
    );
}

// ---- a student's own section ------------------------------------------------

/// `GET /classes/me` is the one class read a student can make: it returns
/// exactly their own memberships (with the homeroom teacher joined on), it does
/// not fall through to the teacher-only `GET /classes/{id}`, and it pages.
#[tokio::test]
async fn a_student_reads_exactly_their_own_classes() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "manager", "manager").await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let student = login_as(&app, &db, "ali", "student").await;
    let lonely = login_as(&app, &db, "veli", "student").await;
    let teacher_id = me_id(&app, &teacher).await;
    let student_id = me_id(&app, &student).await;

    // Added oldest-first, with a gap wide enough that `added_at` really
    // orders them — the composite ids break a tie in account-ULID order, which
    // says nothing about who joined first.
    let mut mine = Vec::new();
    for name in ["9-A", "9-B"] {
        let class = create_class(
            &app,
            &manager,
            json!({ "name": name, "teacher_id": teacher_id }),
        )
        .await
        .body["id"]
            .as_str()
            .unwrap()
            .to_string();
        let added = send(
            &app,
            "POST",
            &format!("/classes/{class}/members"),
            Some(&manager),
            Some(json!({ "user_id": student_id })),
        )
        .await;
        assert_eq!(added.status, StatusCode::CREATED);
        mine.push(class);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // A class the student is *not* in must never show up.
    create_class(&app, &manager, json!({ "name": "10-A" })).await;

    let res = send(&app, "GET", "/classes/me", Some(&student), None).await;
    assert_eq!(
        res.status,
        StatusCode::OK,
        "/classes/me must not fall through to the teacher-only /classes/{{id}}"
    );
    assert!(
        res.body.get("items").is_some() && res.body.get("total").is_some(),
        "…which is what a page envelope, rather than one class object, proves: {:?}",
        res.body
    );
    assert_eq!(total(&res.body), 2);
    let ids = |body: &serde_json::Value| -> Vec<String> {
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["id"].as_str().unwrap().to_string())
            .collect()
    };
    // Unsorted: the documented order is newest membership first, so the class
    // joined *second* must lead. Sorting here is what made this assertion dead.
    assert_eq!(
        ids(&res.body),
        vec![mine[1].clone(), mine[0].clone()],
        "exactly the student's own classes, newest membership first"
    );
    assert_eq!(
        res.body["items"][0]["teacher"]["username"],
        json!("teacher"),
        "the homeroom teacher must be resolved, not left a bare id"
    );
    // The office's account names stay out of a student's reach.
    assert_eq!(
        res.body["items"][0]["creator"],
        json!(null),
        "a student may not learn who in the office created their class"
    );
    let staff = send(&app, "GET", "/classes/me", Some(&manager), None).await;
    assert_eq!(total(&staff.body), 0, "staff are never class members");
    let staff_view = send(
        &app,
        "GET",
        &format!("/classes/user/{student_id}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(
        staff_view.body["items"][0]["creator"]["username"],
        json!("manager"),
        "…but teacher+ still sees the creator"
    );

    // The window must actually move: the second page is the *older* class, not
    // the first page again. Asserting only the length let `offset` be ignored.
    let paged = send(
        &app,
        "GET",
        "/classes/me?limit=1&offset=1",
        Some(&student),
        None,
    )
    .await;
    assert_eq!(paged.status, StatusCode::OK);
    assert_eq!(total(&paged.body), 2);
    assert_eq!(
        ids(&paged.body),
        vec![mine[0].clone()],
        "?offset=1 must return the second row of the full order, not the first"
    );
    let first = send(
        &app,
        "GET",
        "/classes/me?limit=1&offset=0",
        Some(&student),
        None,
    )
    .await;
    assert_eq!(ids(&first.body), vec![mine[1].clone()]);

    // A student in no class gets an empty page, not a 404.
    let empty = send(&app, "GET", "/classes/me", Some(&lonely), None).await;
    assert_eq!(empty.status, StatusCode::OK);
    assert_eq!(total(&empty.body), 0);
    assert!(empty.body["items"].as_array().unwrap().is_empty());
}

/// `GET /classes/user/{id}` follows the observer rule the per-student reports
/// use: teacher+ and a *linked* parent may read it, everyone else — an
/// unlinked parent, another student — is a `403` with no existence leak.
#[tokio::test]
async fn only_staff_and_a_linked_parent_read_another_users_classes() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "mudur", "admin").await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let student = login_as(&app, &db, "ali", "student").await;
    let peer = login_as(&app, &db, "veli", "student").await;
    let parent = login_as(&app, &db, "anne", "parent").await;
    let stranger = login_as(&app, &db, "baba", "parent").await;
    let student_id = me_id(&app, &student).await;
    let parent_id = me_id(&app, &parent).await;
    let teacher_id = me_id(&app, &teacher).await;

    let class = create_class(
        &app,
        &admin,
        json!({ "name": "9-A", "teacher_id": teacher_id }),
    )
    .await
    .body["id"]
        .as_str()
        .unwrap()
        .to_string();
    send(
        &app,
        "POST",
        &format!("/classes/{class}/members"),
        Some(&admin),
        Some(json!({ "user_id": student_id })),
    )
    .await;
    let linked = send(
        &app,
        "POST",
        &format!("/users/{parent_id}/students"),
        Some(&admin),
        Some(json!({ "user_id": student_id })),
    )
    .await;
    assert!(
        linked.status.is_success(),
        "link student: {}",
        linked.status
    );

    let path = format!("/classes/user/{student_id}");
    // `creator` is the half of this route that is not about who may read it:
    // a linked parent is below teacher+, so the office account that made the
    // class must be hidden from them exactly as it is from their child — while
    // the homeroom teacher, which is the point of the read, stays named for
    // both.
    for (who, cookie, creator) in [
        ("teacher", &teacher, json!("mudur")),
        ("linked parent", &parent, json!(null)),
    ] {
        let res = send(&app, "GET", &path, Some(cookie), None).await;
        assert_eq!(res.status, StatusCode::OK, "{who} may read");
        assert_eq!(total(&res.body), 1, "{who} sees the class");
        assert_eq!(res.body["items"][0]["id"], json!(class));
        assert_eq!(
            res.body["items"][0]["teacher"]["username"],
            json!("teacher"),
            "{who} must be told who the homeroom teacher is"
        );
        let seen = &res.body["items"][0]["creator"];
        match creator.as_str() {
            Some(name) => assert_eq!(seen["username"], json!(name), "{who} may see the creator"),
            None => assert_eq!(
                *seen,
                json!(null),
                "{who} is below teacher+ and may not learn an office account's name"
            ),
        }
    }
    for (who, cookie) in [("unlinked parent", &stranger), ("another student", &peer)] {
        let res = send(&app, "GET", &path, Some(cookie), None).await;
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{who} may not");
    }

    // The pagination envelope, and the 404 a user who does not exist gets.
    let paged = send(
        &app,
        "GET",
        &format!("{path}?limit=1&offset=0"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(paged.status, StatusCode::OK);
    assert_eq!(total(&paged.body), 1);
    assert_eq!(paged.body["items"].as_array().unwrap().len(), 1);
    let ghost = send(
        &app,
        "GET",
        "/classes/user/01J8XZ0K3Q8G7X2M4N5P6R7S8T",
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(ghost.status, StatusCode::NOT_FOUND);
}

/// The documented decision on a `class_member` row whose class is gone: the
/// page still *counts* it (the window was cut from the membership rows) but
/// skips it from `items`, and answers `200` rather than a `500` or a short
/// page that silently loses the rest. Unreachable through the API — the class
/// delete guard refuses while members exist — so the row is forced here the
/// only way it could ever arise, a class row vanishing under its memberships.
#[tokio::test]
async fn a_membership_whose_class_is_gone_is_counted_but_skipped() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "manager", "manager").await;
    let student = login_as(&app, &db, "ali", "student").await;
    let student_id = me_id(&app, &student).await;

    let mut classes = Vec::new();
    for name in ["9-A", "9-B"] {
        let class = create_class(&app, &manager, json!({ "name": name }))
            .await
            .body["id"]
            .as_str()
            .unwrap()
            .to_string();
        let added = send(
            &app,
            "POST",
            &format!("/classes/{class}/members"),
            Some(&manager),
            Some(json!({ "user_id": student_id })),
        )
        .await;
        assert_eq!(added.status, StatusCode::CREATED);
        classes.push(class);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // The guard would refuse this, so it is spelled as the raw row loss it
    // stands in for; the membership row is deliberately left behind.
    db.query("DELETE type::record('class_group', $key)")
        .bind(("key", classes[0].clone()))
        .await
        .unwrap()
        .check()
        .unwrap();

    let res = send(&app, "GET", "/classes/me", Some(&student), None).await;
    assert_eq!(
        res.status,
        StatusCode::OK,
        "a dangling membership is not a 500"
    );
    assert_eq!(
        total(&res.body),
        2,
        "total counts the membership rows, which is what the window was cut from"
    );
    let items = res.body["items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        1,
        "the class that is gone is skipped, not faked"
    );
    assert_eq!(
        items[0]["id"],
        json!(classes[1]),
        "…and the surviving class is still there, in order"
    );
}

// ---- the demotion-vs-assign race, made deterministic -----------------------
//
// The window is between a handler's "this account is teacher+" check and the
// write that records it: a `PATCH /users/{id}/role` landing in there sweeps
// nothing (the rows it would sweep are not written yet) and nothing ever
// re-sweeps, so the assignment would stand forever naming a non-teacher. Every
// such handler therefore re-reads the live role *after* its write
// (`web::undo_if_demoted`).
//
// Racing it from a second task would be a coin flip the in-memory engine is
// known to lie about, so the demotion is injected by the database itself: a
// `DEFINE EVENT` on the very table the handler writes fires *inside* that
// write, which is exactly "after the role check, before the re-read", every
// single time. Nothing in `src/` knows about it — the seam is the schema, and
// these tests are what stop the three `undo_if_demoted` calls being deleted.

/// Demote `user` from inside the next write to `table`, the instant it lands.
async fn demote_during_writes_to(table: &str, event: &str, user: &str, db: &Database) {
    db.query(format!(
        "DEFINE EVENT demote_mid_write ON TABLE {table} WHEN $event = '{event}' THEN {{ \
         UPDATE type::record('user', '{user}') SET role = 'student'; }};"
    ))
    .await
    .unwrap()
    .check()
    .unwrap();
}

/// The homeroom teacher's live role, straight out of the store.
async fn role_of(user: &str, db: &Database) -> String {
    let mut result = db
        .query("SELECT VALUE role FROM user WHERE id = type::record('user', $k)")
        .bind(("k", user.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
    result.take::<Vec<String>>(0).unwrap().pop().unwrap()
}

/// `POST /classes` naming a teacher who is demoted while the row is being
/// written: `409`, and the class is rolled back **whole** — no teacherless
/// class left standing, and no reference stranded on the term it linked (which
/// would make that term undeletable forever).
#[tokio::test]
async fn a_create_whose_teacher_is_demoted_mid_write_rolls_back_whole() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "manager", "manager").await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let teacher_id = me_id(&app, &teacher).await;
    let term = send(
        &app,
        "POST",
        "/terms",
        Some(&manager),
        Some(json!({ "name": "2026", "starts_at": 100, "ends_at": 200 })),
    )
    .await;
    assert_eq!(term.status, StatusCode::CREATED);
    let term_id = term.body["id"].as_str().unwrap().to_string();

    demote_during_writes_to("class_group", "CREATE", &teacher_id, &db).await;
    let res = send(
        &app,
        "POST",
        "/classes",
        Some(&manager),
        Some(json!({ "name": "9-A", "teacher_id": teacher_id, "term_id": term_id })),
    )
    .await;

    assert_eq!(
        res.status,
        StatusCode::CONFLICT,
        "a teacher demoted mid-write must not be recorded as one: {:?}",
        res.body
    );
    assert!(
        res.body["error"].as_str().unwrap().contains("demoted"),
        "the 409 must say why: {:?}",
        res.body
    );
    assert_eq!(role_of(&teacher_id, &db).await, "student", "the seam fired");
    assert_eq!(
        rows("SELECT VALUE id FROM class_group", &db).await,
        0,
        "the 409 promises nothing was created — so nothing may be there"
    );
    assert_eq!(
        counter("SELECT VALUE class_count ?? 0 FROM term", &db).await,
        0,
        "…and least of all a reference stranded on the term"
    );
    let freed = send(
        &app,
        "DELETE",
        &format!("/terms/{term_id}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(
        freed.status,
        StatusCode::NO_CONTENT,
        "which is the point: the term must still be deletable"
    );
}

/// The same rollback at a grade a **blueprint** covers, which is what pins the
/// order of the two: a create stocks itself from its grade's template, and that
/// stocking runs *after* this demotion check. Were it the other way round the
/// class would hold courses by the time the rollback ran, `ClassGroup::delete`
/// refuses one that does, and the `409` promising nothing was created would
/// leave a stocked section standing behind it.
#[tokio::test]
async fn a_rolled_back_create_at_a_blueprinted_grade_leaves_nothing_behind() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "manager", "manager").await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let teacher_id = me_id(&app, &teacher).await;
    let course = create_course(&app, &manager, "algebra").await;
    let made = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade": "9", "course_ids": [course] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);

    demote_during_writes_to("class_group", "CREATE", &teacher_id, &db).await;
    let res = send(
        &app,
        "POST",
        "/classes",
        Some(&manager),
        Some(json!({ "name": "9-A", "grade": "9", "teacher_id": teacher_id })),
    )
    .await;

    assert_eq!(
        res.status,
        StatusCode::CONFLICT,
        "the demotion still wins over the stocking: {:?}",
        res.body
    );
    assert_eq!(
        rows("SELECT VALUE id FROM class_group", &db).await,
        0,
        "the rollback must still have been able to delete the class"
    );
    assert_eq!(
        rows("SELECT VALUE id FROM class_course", &db).await,
        0,
        "…which it only can because nothing was stocked onto it first"
    );
}

/// `PATCH /classes/{id}` naming a teacher who is demoted while the row is being
/// written: `409`, and the column it just set is taken back.
#[tokio::test]
async fn a_patch_whose_teacher_is_demoted_mid_write_undoes_the_column() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "manager", "manager").await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let teacher_id = me_id(&app, &teacher).await;
    let class = create_class(&app, &manager, json!({ "name": "9-A" }))
        .await
        .body["id"]
        .as_str()
        .unwrap()
        .to_string();

    demote_during_writes_to("class_group", "UPDATE", &teacher_id, &db).await;
    let res = send(
        &app,
        "PATCH",
        &format!("/classes/{class}"),
        Some(&manager),
        Some(json!({ "teacher_id": teacher_id })),
    )
    .await;

    assert_eq!(
        res.status,
        StatusCode::CONFLICT,
        "a teacher demoted mid-write must not be recorded as one: {:?}",
        res.body
    );
    assert_eq!(role_of(&teacher_id, &db).await, "student", "the seam fired");
    assert_eq!(
        counter(
            "SELECT VALUE (IF teacher = NONE { 0 } ELSE { 1 }) FROM class_group",
            &db
        )
        .await,
        0,
        "the assignment this request wrote must be taken back"
    );
    // The class itself is untouched — only the column this request set is.
    let stored = send(
        &app,
        "GET",
        &format!("/classes/{class}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(stored.status, StatusCode::OK);
    assert_eq!(stored.body["name"], json!("9-A"));
    assert_eq!(stored.body["teacher"], json!(null));
}

/// The same guard on the other assignment it protects:
/// `POST /courses/{id}/teachers` for an account demoted while the list is being
/// written is a `409`, with the assignment dropped again.
#[tokio::test]
async fn a_course_assignment_whose_teacher_is_demoted_mid_write_is_undone() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "manager", "manager").await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let teacher_id = me_id(&app, &teacher).await;
    let course = create_course(&app, &manager, "algebra").await;

    demote_during_writes_to("course", "UPDATE", &teacher_id, &db).await;
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/teachers"),
        Some(&manager),
        Some(json!({ "user_id": teacher_id })),
    )
    .await;

    assert_eq!(
        res.status,
        StatusCode::CONFLICT,
        "a teacher demoted mid-write must not be recorded as one: {:?}",
        res.body
    );
    assert_eq!(role_of(&teacher_id, &db).await, "student", "the seam fired");
    assert_eq!(
        counter("SELECT VALUE array::len(teachers ?? []) FROM course", &db).await,
        0,
        "the assignment must be dropped again, not left granting nothing"
    );
}

/// The other end of that rollback: if the delete is ever *refused* — the class
/// took a member or a course between its own create and the rollback — the
/// class is still there, so answering the `409` whose text promises "nothing
/// was created" would be a lie about the stored state. It must surface as an
/// internal error instead.
///
/// Unreachable through the API (the class is one statement old and both of its
/// counters are absent), so the seam does both halves at once: demote the
/// teacher *and* tick `class_member_count` inside the very write that creates
/// the row, which is the only ordering that reaches this branch.
#[tokio::test]
async fn a_rollback_the_guard_refuses_is_a_500_not_a_lying_409() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "manager", "manager").await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let teacher_id = me_id(&app, &teacher).await;

    db.query(format!(
        "DEFINE EVENT demote_and_occupy ON TABLE class_group WHEN $event = 'CREATE' THEN {{ \
         UPDATE type::record('user', '{teacher_id}') SET role = 'student'; \
         UPDATE $after.id SET class_member_count = 1; }};"
    ))
    .await
    .unwrap()
    .check()
    .unwrap();

    let res = send(
        &app,
        "POST",
        "/classes",
        Some(&manager),
        Some(json!({ "name": "9-A", "teacher_id": teacher_id })),
    )
    .await;

    assert_eq!(
        res.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "a rollback that could not happen may not be reported as one: {:?}",
        res.body
    );
    assert_eq!(
        rows("SELECT VALUE id FROM class_group", &db).await,
        1,
        "the class really is still there — which is why the 409 would have lied"
    );
    assert_eq!(
        counter(
            "SELECT VALUE (IF teacher = NONE { 0 } ELSE { 1 }) FROM class_group",
            &db
        )
        .await,
        0,
        "…though the demoted teacher is off it, the undo having run first"
    );
}

// ---- the grade filter on the index -----------------------------------------

/// Names of a `/classes` page, in the order it came back.
fn names(res: &Res) -> Vec<String> {
    common::items(&res.body)
        .iter()
        .map(|class| class["name"].as_str().unwrap().to_string())
        .collect()
}

/// `?grade=` is the read a manager holding a blueprint's skip list needs: which
/// sections carry the label the pump keyed on. It is the *query's* `WHERE`, so
/// `total` counts the filtered set and a window walks that set alone — drop the
/// predicate and every assertion below sees the unfiltered four instead.
#[tokio::test]
async fn the_class_index_filters_by_grade() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    for (name, grade) in [
        ("9-A", "9"),
        ("9-B", "9"),
        ("10-A", "10"),
        // A club-shaped section: created with no grade at all, so its row
        // carries no `grade` key.
        ("satranc", ""),
    ] {
        create_class(&app, &manager, json!({ "name": name, "grade": grade })).await;
    }
    let list = async |query: &str| -> Res {
        let res = send(
            &app,
            "GET",
            &format!("/classes{query}"),
            Some(&manager),
            None,
        )
        .await;
        assert_eq!(
            res.status,
            StatusCode::OK,
            "GET /classes{query}: {}",
            res.body
        );
        res
    };

    // Omitted: today's behaviour, every class.
    let res = list("").await;
    assert_eq!(total(&res.body), 4);
    assert_eq!(names(&res).len(), 4);

    // One label, matched exactly: both sections at it, and a `total` that
    // counts only them.
    let res = list("?grade=9").await;
    assert_eq!(total(&res.body), 2, "total counts the filtered set");
    assert_eq!(names(&res), ["9-B", "9-A"], "newest first, 9 only");
    // "9" is not a prefix match, a fold, or a trim.
    assert_eq!(names(&list("?grade=10").await), ["10-A"]);
    for miss in ["?grade=11", "?grade=%209", "?grade=9-A"] {
        let res = list(miss).await;
        assert_eq!(total(&res.body), 0, "GET /classes{miss} total");
        assert!(
            names(&res).is_empty(),
            "GET /classes{miss} is an empty page"
        );
    }

    // Empty `?grade=` is *no* grade, the same reading `grade_or_none` gives a
    // write — the sections a blueprint can never cover.
    let res = list("?grade=").await;
    assert_eq!((total(&res.body), names(&res)), (1, vec!["satranc".into()]));

    // Filter and window compose: the window is cut from the filtered set, and
    // `total` stays that set's size on every page of it.
    let first = list("?grade=9&limit=1&offset=0").await;
    assert_eq!(total(&first.body), 2);
    assert_eq!(names(&first), ["9-B"]);
    assert_eq!(first.body["limit"], 1);
    let second = list("?grade=9&limit=1&offset=1").await;
    assert_eq!(total(&second.body), 2);
    assert_eq!(names(&second), ["9-A"], "consecutive pages are disjoint");
    assert_eq!(second.body["offset"], 1);
    let past = list("?grade=9&limit=1&offset=9").await;
    assert!(names(&past).is_empty(), "past the end is an empty page");
    assert_eq!(
        total(&past.body),
        2,
        "…with the filtered total still honest"
    );

    // A label the write paths would refuse is refused here too, rather than
    // reading as "no such grade".
    let res = send(
        &app,
        "GET",
        &format!("/classes?grade={}", "9".repeat(21)),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
}

/// The heir a detach hands a shared row to is **claimed**, not merely read —
/// and a claim that matches nothing takes the release arm.
///
/// Two pure reads picked that heir: the rival classes attached to the course,
/// then the ones the student is also in. Nothing in the sweep's write set
/// touched the class it settled on, and SurrealDB conflict-checks no read, so
/// `DELETE /classes/{heir}/members/{user}` and
/// `DELETE /classes/{heir}/courses/{course}` could both commit *inside* this
/// long sweep and leave the row tagged with a class holding neither link — a
/// class whose own 0/0 delete guard then passes, stranding an enrollment no
/// route can reach (both ends 404 on the link rows that are gone). The claim
/// moves a counter on the heir's own row, which is the record every one of
/// those writers moves too, so the store settles the pair.
///
/// The real interleaving needs the store's conflict detection, which the
/// in-memory engine does not have (`class_pump::tests::a_detached_row_is_never_
/// handed_to_a_class_that_let_it_go`, `#[ignore]`d, drives it on a real
/// server). What is deterministic here is the other half of the same claim: an
/// heir whose class row is *gone* while its link rows survive — a state this
/// layer really carries (`a_membership_whose_class_is_gone_is_counted_but_
/// skipped`) — must not be handed the row either.
#[tokio::test]
async fn a_detach_never_hands_a_row_to_a_class_that_is_gone() {
    let (app, db) = app_and_db().await;
    let staff = login_as(&app, &db, "manager", "manager").await;
    let manager = UserId::from_key(&me_id(&app, &staff).await);
    let student = UserId::from_key("student");
    let algebra = CourseId::from_key(&create_course(&app, &staff, "algebra").await);
    let mut made = Vec::new();
    for name in ["9-B", "9-A"] {
        let class = ClassGroup::create(
            &manager,
            ClassName::try_new(name).unwrap(),
            None,
            None,
            None,
            &db,
        )
        .await
        .unwrap();
        let class = class.get_id().clone();
        ClassMember::add(&class, &student, &manager, &db)
            .await
            .unwrap();
        ClassCourse::attach(&class, &algebra, &manager, &db)
            .await
            .unwrap();
        made.push(class);
    }
    let (owner, heir) = (made[0].clone(), made[1].clone());
    assert_eq!(
        rows(
            &format!(
                "SELECT VALUE id FROM enrollment WHERE source = class_group:{}",
                owner.key()
            ),
            &db
        )
        .await,
        1,
        "the second attach skips the row the first wrote, so the first owns it"
    );

    // The heir's class row goes while both of its link rows stay: the state a
    // class deleted out from under its own links leaves.
    db.query("DELETE $c")
        .bind(("c", heir.record()))
        .await
        .unwrap()
        .check()
        .unwrap();

    ClassCourse::detach(&owner, &algebra, &db).await.unwrap();
    assert_eq!(
        rows("SELECT VALUE id FROM enrollment", &db).await,
        0,
        "a row handed to a class that is not there is one nothing can ever sweep"
    );
    assert_eq!(
        counter("SELECT VALUE enrollment_count ?? 0 FROM course", &db).await,
        0,
        "…and its seat must come back with it"
    );
}
