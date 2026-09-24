//! Regressions for the class section (şube) layer, its enrollment side, and the sweep a
//! role change runs. Every test here pins a defect that shipped: a link left
//! pointing at a deleted course, the refusal that misnamed it, a hand enroll a
//! class could still undo, and the two-transaction role sweep.

mod common;

use axum::http::StatusCode;
use common::{
    ABSENT_ID, GHOST_ID, Res, add_member, app_and_db, attach_instance, create_course, create_exam,
    create_term, create_year, items, login_as, me_id, send, taught, total,
};
use hezarfen_backend::constant::{MAX_CLASS_COURSES, MAX_CLASS_MEMBERS};
use hezarfen_backend::database::Database;
use hezarfen_backend::domain::class_group::ClassName;
use hezarfen_backend::domain::grade::GradeLevel;
use hezarfen_backend::domain::course::CourseId;
use hezarfen_backend::domain::user::UserId;
use hezarfen_backend::error::AppError;
use hezarfen_backend::service::{class_course, class_group, class_member};
use serde_json::json;
use sqlx::Row as _;

/// The instance id out of a `POST /classes/{id}/instances` response — the
/// anchor every roster, exam and detach under that attachment keys on.
fn instance_of(res: &Res) -> String {
    res.body["id"].as_str().expect("instance id").to_string()
}

/// One counter, re-read out of the store — never off a response body. `sql`
/// is a whole scalar query; each call spells the aggregate it asserts on.
async fn counter(sql: &'static str, db: &Database) -> i64 {
    sqlx::query_scalar(sql).fetch_one(db).await.unwrap()
}

/// How many rows `sql` counts.
async fn rows(sql: &'static str, db: &Database) -> i64 {
    counter(sql, db).await
}

/// A real `app_user` row for the fixture actor or member a domain call must
/// name: the actor and member columns are foreign keys now, so they are rows,
/// not fabricated ids. The username is unique, so every call for the same name
/// shares one row, whichever call minted it. These rows never log in, so the
/// hash is a stub.
async fn fixture_user(db: &Database, username: &str) -> UserId {
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

/// Delete a row the way a racing write that lost would have left it: the row
/// gone, its referring rows untouched. Real FKs refuse that state, so the
/// delete is forced the one way Postgres allows: FK triggers suspended for the
/// one transaction.
async fn wipe_row(table: &str, id: uuid::Uuid, db: &Database) {
    let mut tx = db.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM {table} WHERE id = $1"
    )))
    .bind(id)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

/// Delete a course row the way a *concurrent* `DELETE /courses/{id}` racing an
/// attach leaves the store: the row goes, the `class_course` link it never saw
/// stays. The real cascade sweeps those links, so this is the only way to reach
/// the state from a test — and the state the pump's in-transaction claim exists
/// to make unreachable in the first place.
async fn wipe_course_row(course: &str, db: &Database) {
    wipe_row(
        "course",
        uuid::Uuid::parse_str(course).expect("a uuid course id"),
        db,
    )
    .await;
}

/// B1(a). A class with *zero* members makes the pump's pair loop empty, so the
/// attach transaction touched no row a concurrent course delete wrote and
/// SurrealDB's write-skew let both commit — a `class_course` row pointing at a
/// course that does not exist. The pivot claim is what makes that impossible.
#[tokio::test]
async fn an_attach_onto_a_deleted_course_writes_no_link() {
    let (_app, db) = app_and_db().await;
    let manager = fixture_user(&db, "manager").await;
    let class = class_group::create(
        &db,
        &manager,
        ClassName::try_new("9-A").unwrap(),
        GradeLevel::new(9).unwrap(),
        None,
        None,
    )
    .await
    .unwrap();
    // A course id nothing ever created: the same row a delete that won the race
    // leaves behind.
    let ghost = CourseId::from_key(GHOST_ID);

    let refused = class_course::attach(&db, class.get_id(), &ghost, &manager).await;
    assert!(
        refused.is_err(),
        "attaching a course that is gone must be refused: {refused:?}"
    );
    assert_eq!(
        rows("SELECT count(*) FROM class_course", &db).await,
        0,
        "no link row may point at a course that does not exist"
    );
    assert_eq!(
        counter(
            "SELECT COALESCE(sum(class_course_count), 0)::bigint FROM class_group",
            &db
        )
        .await,
        0,
        "…and the class must not count one either, or it is undeletable forever"
    );
}

/// B1(b). The other half: a link that *did* land that way must still be
/// removable. The detach keys on the link row itself (`DELETE
/// /classes/{c}/instances/{instance}`), so a course that vanished under it is
/// still swept — a route that read the course first would answer 404 forever
/// and leave the class 409 forever, over an attachment nothing could sweep.
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
            Some(json!({ "name": "9-A", "grade_level": 9 })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED);
        res.body["class"]["id"].as_str().unwrap().to_string()
    };
    let attached = send(
        &app,
        "POST",
        &format!("/classes/{class}/instances"),
        Some(&manager),
        Some(json!({ "course_id": course })),
    )
    .await;
    assert_eq!(attached.status, StatusCode::CREATED);
    let instance = instance_of(&attached);
    // The course row goes the way a concurrent `DELETE /courses/{id}` racing
    // the attach would leave it: gone, the link it never saw still standing.
    wipe_course_row(&course, &db).await;

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

    // An instance that is no longer attached is still a 404 — the guard that
    // turns "sweep the stale link" into "delete anything" is the link row, not
    // the course row.
    let again = send(
        &app,
        "DELETE",
        &format!("/classes/{class}/instances/{instance}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
}

/// B2. A class whose link names a catalog row that is gone — the state a
/// `DELETE /courses/{id}` racing the attach leaves behind — must not lock the
/// operator out. The pump's pairs are `(instance, user)` and never read the
/// catalog row, so the add lands and the student is enrolled; the stale link
/// then sweeps through its *own* route (the link row is the guard, never the
/// course row), and the class is deletable again once its roster has left.
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
            Some(json!({ "name": "9-A", "grade_level": 9 })),
        )
        .await;
        res.body["class"]["id"].as_str().unwrap().to_string()
    };
    let attached = send(
        &app,
        "POST",
        &format!("/classes/{class}/instances"),
        Some(&manager),
        Some(json!({ "course_id": course })),
    )
    .await;
    assert_eq!(attached.status, StatusCode::CREATED);
    let instance = instance_of(&attached);
    wipe_course_row(&course, &db).await;

    let added = send(
        &app,
        "POST",
        &format!("/classes/{class}/members"),
        Some(&manager),
        Some(json!({ "user_id": student_id })),
    )
    .await;
    assert_eq!(
        added.status,
        StatusCode::CREATED,
        "a link whose course is gone must not lock the roster: {:?}",
        added.body
    );
    // The student is on the instance's roster, tagged with the şube that
    // pumped them: what the pump names is the instance, and the catalog row
    // the link points at is not part of that pair.
    let roster = send(
        &app,
        "GET",
        &format!("/instances/{instance}/enrollments"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(roster.status, StatusCode::OK, "{:?}", roster.body);
    assert_eq!(
        total(&roster.body),
        1,
        "the pump enrolled the student: {:?}",
        roster.body
    );
    assert_eq!(items(&roster.body)[0]["user"]["id"], json!(student_id));
    assert_eq!(items(&roster.body)[0]["source"], json!(class));

    // The stale link detaches through its own route, and the class is free
    // once the roster has left it.
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
    let left = send(
        &app,
        "DELETE",
        &format!("/classes/{class}/members/{student_id}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(left.status, StatusCode::NO_CONTENT, "{:?}", left.body);
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
        "…and the class the stale link blocked is deletable again: {:?}",
        deleted.body
    );
    assert_eq!(
        rows("SELECT count(*) FROM class_member", &db).await,
        0,
        "the şube delete takes its roster history with it"
    );
    assert_eq!(
        rows("SELECT count(*) FROM enrollment", &db).await,
        0,
        "…and the instance's roster went with the detach"
    );
    assert_eq!(
        counter(
            "SELECT COALESCE(sum(enrollment_count), 0)::bigint FROM class_course",
            &db
        )
        .await,
        0,
        "…with each instance's seat counter"
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
            Some(json!({ "name": "9-A", "grade_level": 9 })),
        )
        .await;
        res.body["class"]["id"].as_str().unwrap().to_string()
    };
    let attached = send(
        &app,
        "POST",
        &format!("/classes/{class}/instances"),
        Some(&manager),
        Some(json!({ "course_id": course })),
    )
    .await;
    assert_eq!(attached.status, StatusCode::CREATED);
    let instance = instance_of(&attached);
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
        &format!("/instances/{instance}/enrollments"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(roster.body["items"][0]["source"], json!(class));

    let by_hand = send(
        &app,
        "POST",
        &format!("/instances/{instance}/enrollments"),
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
        rows(
            "SELECT count(*) FROM enrollment WHERE source IS NOT NULL",
            &db
        )
        .await,
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
        rows("SELECT count(*) FROM enrollment", &db).await,
        1,
        "the class may not unenroll a student an operator enrolled by hand"
    );
    assert_eq!(
        counter(
            "SELECT COALESCE(sum(enrollment_count), 0)::bigint FROM class_course",
            &db
        )
        .await,
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
            Some(json!({ "name": "9-A", "grade_level": 9 })),
        )
        .await;
        res.body["class"]["id"].as_str().unwrap().to_string()
    };
    let attached = send(
        &app,
        "POST",
        &format!("/classes/{class}/instances"),
        Some(&manager),
        Some(json!({ "course_id": course })),
    )
    .await;
    assert_eq!(attached.status, StatusCode::CREATED);
    let instance = instance_of(&attached);
    send(
        &app,
        "POST",
        &format!("/classes/{class}/members"),
        Some(&manager),
        Some(json!({ "user_id": student_id })),
    )
    .await;
    assert_eq!(rows("SELECT count(*) FROM enrollment", &db).await, 1);

    let promoted = send(
        &app,
        "PATCH",
        &format!("/users/{student_id}/role"),
        Some(&admin),
        Some(json!({ "role": "teacher" })),
    )
    .await;
    assert_eq!(promoted.status, StatusCode::OK);

    assert_eq!(
        rows(
            "SELECT count(*) FROM class_member WHERE left_at IS NULL",
            &db
        )
        .await,
        0,
        "a non-student holds no live stint; the history row is what a soft \
         leave keeps"
    );
    assert_eq!(
        rows("SELECT count(*) FROM enrollment", &db).await,
        0,
        "a non-student holds no roster row, whoever wrote it"
    );
    assert_eq!(
        counter(
            "SELECT COALESCE(sum(enrollment_count), 0)::bigint FROM class_course",
            &db
        )
        .await,
        0,
        "the seat comes back exactly once"
    );
    assert_eq!(
        counter(
            "SELECT COALESCE(sum(class_member_count), 0)::bigint FROM class_group",
            &db
        )
        .await,
        0
    );
    // Nothing is left tagged with the class, so letting it go strands nothing.
    let detached = send(
        &app,
        "DELETE",
        &format!("/classes/{class}/instances/{instance}"),
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

/// D1. Both link tables key on the *pair* (`<class>_<user>`), so the
/// documented "newest first" was implemented as `ORDER BY id DESC` — which
/// sorts the roster by the member's account ULID, an order that has nothing to
/// do with when anyone was added. The three students here are added in an order
/// their ids do not reproduce, so id-ordering and stamp-ordering disagree.
#[tokio::test]
async fn a_roster_is_ordered_by_when_a_student_was_added() {
    let (_app, db) = app_and_db().await;
    let manager = fixture_user(&db, "manager").await;
    let class = class_group::create(
        &db,
        &manager,
        ClassName::try_new("9-A").unwrap(),
        GradeLevel::new(9).unwrap(),
        None,
        None,
    )
    .await
    .unwrap()
    .get_id()
    .clone();

    // Members added in an order no id ordering reproduces: only a real stamp
    // can tell the two apart. The waits are what keep the stamps distinct —
    // a tie falls back to the id, which is the very order under test. The
    // minted rows carry uuid ids, so the assertions read back through the
    // name -> id map.
    let mut names: Vec<(&str, UserId)> = Vec::new();
    for key in ["a", "c", "b"] {
        let member = fixture_user(&db, key).await;
        class_member::add(&db, &class, &member, &manager)
            .await
            .unwrap();
        names.push((key, member));
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    let name_of = |id: &UserId| {
        names
            .iter()
            .find(|(_, mid)| mid == id)
            .map(|(k, _)| *k)
            .unwrap()
    };

    let (roster, _) = class_member::list_for_class(&db, &class, None, 0)
        .await
        .unwrap();
    let order: Vec<&str> = roster
        .iter()
        .map(|member| name_of(member.get_user()))
        .collect();
    assert_eq!(
        order,
        vec!["b", "c", "a"],
        "the roster must come back newest-added first, not sorted by account id"
    );

    // A row whose age nobody knows: the shipped `joined_at` is `NOT NULL`
    // since the K12 remodel (a stint is minted with a stamp, always), so
    // "no stamp at all" is unrepresentable — the oldest stamp there is *is*
    // the unknown age. It sorts last under DESC, which is what makes such a
    // row read as the oldest rather than an invented number putting it
    // anywhere else.
    sqlx::query(
        "UPDATE class_member SET joined_at = 0
         WHERE app_user = (SELECT id FROM app_user WHERE username = 'c')",
    )
    .execute(&db)
    .await
    .unwrap();
    let (roster, _) = class_member::list_for_class(&db, &class, None, 0)
        .await
        .unwrap();
    assert_eq!(
        roster
            .iter()
            .map(|member| name_of(member.get_user()))
            .collect::<Vec<_>>(),
        vec!["b", "a", "c"],
        "a row of unknown age must sort oldest, not first"
    );
}

/// D1, the other axis: the course list had the same defect, ordered by the
/// *course's* ULID.
#[tokio::test]
async fn a_class_course_list_is_ordered_by_when_it_was_attached() {
    let (app, db) = app_and_db().await;
    let manager_cookie = login_as(&app, &db, "manager", "manager").await;
    let manager = UserId::from_key(&me_id(&app, &manager_cookie).await);
    let class = class_group::create(
        &db,
        &manager,
        ClassName::try_new("9-A").unwrap(),
        GradeLevel::new(9).unwrap(),
        None,
        None,
    )
    .await
    .unwrap()
    .get_id()
    .clone();

    // Course ids rise with creation (monotonic mint); attaching them
    // back-to-front makes id order and attach order disagree.
    let mut courses = Vec::new();
    for title in ["first", "second", "third"] {
        courses.push(CourseId::from_key(
            &create_course(&app, &manager_cookie, title).await,
        ));
    }
    for course in courses.iter().rev() {
        class_course::attach(&db, &class, course, &manager)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }

    let (attached, _) = class_course::list_for_class(&db, &class, None, 0)
        .await
        .unwrap();
    let order: Vec<String> = attached
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
    let manager = fixture_user(&db, "manager").await;
    let class = class_group::create(
        &db,
        &manager,
        ClassName::try_new("9-A").unwrap(),
        GradeLevel::new(9).unwrap(),
        None,
        None,
    )
    .await
    .unwrap()
    .get_id()
    .clone();
    sqlx::query("UPDATE class_group SET class_member_count = $1 WHERE id = $2")
        .bind(MAX_CLASS_MEMBERS - 1)
        .bind(class.clone())
        .execute(&db)
        .await
        .unwrap();

    let last = fixture_user(&db, "last").await;
    class_member::add(&db, &class, &last, &manager)
        .await
        .expect("the place under the ceiling is still free");
    let over = fixture_user(&db, "over").await;
    let refused = class_member::add(&db, &class, &over, &manager).await;
    assert!(
        matches!(refused, Err(AppError::ConflictCoded { code, ref message })
            if code == "class_at_roster_ceiling"
                && message.contains(&MAX_CLASS_MEMBERS.to_string())),
        "a full class is a 409 naming its ceiling and coded, never a 404, and the code \
         must name the ceiling the prose does — the roster, not the course list: {refused:?}"
    );
    assert_eq!(
        counter(
            "SELECT COALESCE(sum(class_member_count), 0)::bigint FROM class_group",
            &db
        )
        .await,
        MAX_CLASS_MEMBERS,
        "a refused add may not tick the counter past the cap"
    );
    assert_eq!(rows("SELECT count(*) FROM class_member", &db).await, 1);

    // And the refusal a *deleted* class earns must stay a 404: both come out of
    // the same counter claim matching nothing, and only one of them is a
    // ceiling anybody can make room under.
    wipe_row("class_group", class.uuid(), &db).await;
    let ghost = fixture_user(&db, "ghost").await;
    let gone = class_member::add(&db, &class, &ghost, &manager).await;
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
    let class = class_group::create(
        &db,
        &manager,
        ClassName::try_new("9-A").unwrap(),
        GradeLevel::new(9).unwrap(),
        None,
        None,
    )
    .await
    .unwrap()
    .get_id()
    .clone();
    let algebra = CourseId::from_key(&create_course(&app, &cookie, "algebra").await);
    sqlx::query("UPDATE class_group SET class_member_count = $1 WHERE id = $2")
        .bind(MAX_CLASS_MEMBERS + 1)
        .bind(class.clone())
        .execute(&db)
        .await
        .unwrap();

    let refused = class_course::attach(&db, &class, &algebra, &manager).await;
    assert!(
        matches!(refused, Err(AppError::ConflictCoded { code, ref message })
            if code == "class_roster_too_large"
                && message.contains(&MAX_CLASS_MEMBERS.to_string())),
        "the refusal must name the axis that is over, not the one with room: {refused:?}"
    );
    assert_eq!(
        rows("SELECT count(*) FROM class_course", &db).await,
        0,
        "a refused attach may not leave the link behind"
    );
    assert_eq!(
        counter(
            "SELECT COALESCE(sum(class_course_count), 0)::bigint FROM class_group",
            &db
        )
        .await,
        0,
        "…nor tick the axis it was refused on"
    );

    // Back under the ceiling, the very same attach goes through: the refusal is
    // the standing count, not the class.
    sqlx::query("UPDATE class_group SET class_member_count = $1 WHERE id = $2")
        .bind(MAX_CLASS_MEMBERS)
        .bind(class.clone())
        .execute(&db)
        .await
        .unwrap();
    class_course::attach(&db, &class, &algebra, &manager)
        .await
        .expect("a class exactly at the ceiling is still a bounded transaction");
}

/// The mirror: a class carrying more courses than `MAX_CLASS_COURSES` takes no
/// new member, because adding one enrolls them into every attached course.
#[tokio::test]
async fn a_class_over_the_course_ceiling_takes_no_member() {
    let (_app, db) = app_and_db().await;
    let manager = fixture_user(&db, "manager").await;
    let class = class_group::create(
        &db,
        &manager,
        ClassName::try_new("9-B").unwrap(),
        GradeLevel::new(9).unwrap(),
        None,
        None,
    )
    .await
    .unwrap()
    .get_id()
    .clone();
    sqlx::query("UPDATE class_group SET class_course_count = $1 WHERE id = $2")
        .bind(MAX_CLASS_COURSES + 1)
        .bind(class.clone())
        .execute(&db)
        .await
        .unwrap();

    let ali = fixture_user(&db, "ali").await;
    let refused = class_member::add(&db, &class, &ali, &manager).await;
    assert!(
        matches!(refused, Err(AppError::ConflictCoded { code, ref message })
            if code == "class_course_list_too_large"
                && message.contains(&MAX_CLASS_COURSES.to_string())),
        "the refusal must name the courses, not the roster — in its code as well as \
         its prose: {refused:?}"
    );
    assert_eq!(rows("SELECT count(*) FROM class_member", &db).await, 0);
    assert_eq!(
        counter(
            "SELECT COALESCE(sum(class_member_count), 0)::bigint FROM class_group",
            &db
        )
        .await,
        0
    );
}

// ---- homeroom teacher (sınıf öğretmeni) ------------------------------------

/// Create a class as `cookie` (asserts 201); returns the response with its
/// `body` narrowed to the created class — the `201` is
/// `{class, skipped, stocked_from}` since a create stocks from its grade's
/// blueprint, and the tests below are about the class itself. The two outer
/// fields have their own tests (`regress_blueprints`).
async fn create_class(app: &axum::Router, cookie: &str, mut body: serde_json::Value) -> Res {
    if body.get("grade_level").is_none() {
        body["grade_level"] = serde_json::json!(9);
    }
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
        json!({ "name": "9-A", "grade_level": 9, "teacher_id": teacher_id }),
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
        Some(json!({ "name": "9-B", "grade_level": 9 })),
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
    let ghost = GHOST_ID;

    for bad in [student_id.as_str(), ghost] {
        let refused = send(
            &app,
            "POST",
            "/classes",
            Some(&manager),
            Some(json!({ "name": "9-A", "grade_level": 9, "teacher_id": bad })),
        )
        .await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "create with {bad}");
    }
    assert_eq!(
        rows("SELECT count(*) FROM class_group", &db).await,
        0,
        "a refused create may write no class"
    );

    let class = create_class(&app, &manager, json!({ "name": "9-A", "grade_level": 9 }))
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
            Some(json!({ "name": "9-B", "grade_level": 9, "teacher_id": bad })),
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

    // Added oldest-first, with a gap wide enough that `joined_at` really
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
        json!({ "name": "9-A", "grade_level": 9, "teacher_id": teacher_id }),
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
        &format!("/classes/user/{ABSENT_ID}"),
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
    wipe_row(
        "class_group",
        uuid::Uuid::parse_str(&classes[0]).expect("a uuid class id"),
        &db,
    )
    .await;

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

/// Demote `user` from inside the next write to `table`, the instant it lands:
/// an AFTER trigger fires inside the write's own transaction, between the
/// handler's role check and its post-write re-read.
async fn demote_during_writes_to(table: &str, event: &str, user: &str, db: &Database) {
    let firing = match event {
        "CREATE" => "INSERT",
        "UPDATE" => "UPDATE",
        other => panic!("unknown event kind {other}"),
    };
    let mut conn = db.acquire().await.expect("acquire for the trigger");
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE FUNCTION heztest_demote_mid_write() RETURNS trigger AS $$
         BEGIN
           UPDATE app_user SET role = 'student' WHERE id = '{user}';
           RETURN NULL;
         END;
         $$ LANGUAGE plpgsql;
         CREATE TRIGGER heztest_demote_mid_write AFTER {firing} ON {table}
         FOR EACH ROW EXECUTE FUNCTION heztest_demote_mid_write();"
    )))
    .execute(&mut *conn)
    .await
    .expect("define the demote trigger");
}

/// The homeroom teacher's live role, straight out of the store.
async fn role_of(user: &str, db: &Database) -> String {
    sqlx::query_scalar("SELECT role FROM app_user WHERE id = $1")
        .bind(UserId::from_key(user))
        .fetch_one(db)
        .await
        .unwrap()
}

/// `POST /classes` naming a teacher who is demoted while the row is being
/// written: `409`, and the class is rolled back **whole** — no teacherless
/// class left standing, and no reference stranded on the academic year it
/// linked (which would make that year undeletable forever).
#[tokio::test]
async fn a_create_whose_teacher_is_demoted_mid_write_rolls_back_whole() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "manager", "manager").await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let teacher_id = me_id(&app, &teacher).await;
    let year = create_year(&app, &manager, "2026-2027").await;
    let term_id = create_term(&app, &manager, &year, "1. Dönem").await;

    demote_during_writes_to("class_group", "CREATE", &teacher_id, &db).await;
    let res = send(
        &app,
        "POST",
        "/classes",
        Some(&manager),
        Some(json!({ "name": "9-A", "grade_level": 9, "teacher_id": teacher_id, "year": year })),
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
        rows("SELECT count(*) FROM class_group", &db).await,
        0,
        "the 409 promises nothing was created — so nothing may be there"
    );
    assert_eq!(
        counter(
            "SELECT COALESCE(sum(class_count), 0)::bigint FROM academic_year",
            &db
        )
        .await,
        0,
        "…and least of all a reference stranded on the year it linked"
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
        Some(json!({ "grade_level": 9, "course_ids": [course] })),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);

    demote_during_writes_to("class_group", "CREATE", &teacher_id, &db).await;
    let res = send(
        &app,
        "POST",
        "/classes",
        Some(&manager),
        Some(json!({ "name": "9-A", "grade_level": 9, "teacher_id": teacher_id })),
    )
    .await;

    assert_eq!(
        res.status,
        StatusCode::CONFLICT,
        "the demotion still wins over the stocking: {:?}",
        res.body
    );
    assert_eq!(
        rows("SELECT count(*) FROM class_group", &db).await,
        0,
        "the rollback must still have been able to delete the class"
    );
    assert_eq!(
        rows("SELECT count(*) FROM class_course", &db).await,
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
    let class = create_class(&app, &manager, json!({ "name": "9-A", "grade_level": 9 }))
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
            "SELECT count(*) FROM class_group WHERE teacher IS NOT NULL",
            &db,
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
/// `POST /instances/{id}/teachers` for an account demoted while the list is
/// being written is a `409`, with the assignment dropped again. Staffing is an
/// *instance's* own list now — the catalog course carries no teachers.
#[tokio::test]
async fn an_instance_assignment_whose_teacher_is_demoted_mid_write_is_undone() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "manager", "manager").await;
    let teacher = login_as(&app, &db, "teacher", "teacher").await;
    let teacher_id = me_id(&app, &teacher).await;
    let instance = taught(&app, &manager, "algebra").await.instance;

    demote_during_writes_to("class_course_teacher", "CREATE", &teacher_id, &db).await;
    let res = send(
        &app,
        "POST",
        &format!("/instances/{instance}/teachers"),
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
        counter("SELECT count(*) FROM class_course_teacher", &db).await,
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

    let mut conn = db.acquire().await.expect("acquire for the trigger");
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE FUNCTION heztest_demote_and_occupy() RETURNS trigger AS $$
         BEGIN
           UPDATE app_user SET role = 'student' WHERE id = '{teacher_id}';
           UPDATE class_group SET class_member_count = 1 WHERE id = NEW.id;
           RETURN NULL;
         END;
         $$ LANGUAGE plpgsql;
         CREATE TRIGGER heztest_demote_and_occupy AFTER INSERT ON class_group
         FOR EACH ROW EXECUTE FUNCTION heztest_demote_and_occupy();"
    )))
    .execute(&mut *conn)
    .await
    .expect("define the demote-and-occupy trigger");

    let res = send(
        &app,
        "POST",
        "/classes",
        Some(&manager),
        Some(json!({ "name": "9-A", "grade_level": 9, "teacher_id": teacher_id })),
    )
    .await;

    assert_eq!(
        res.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "a rollback that could not happen may not be reported as one: {:?}",
        res.body
    );
    assert_eq!(
        rows("SELECT count(*) FROM class_group", &db).await,
        1,
        "the class really is still there — which is why the 409 would have lied"
    );
    assert_eq!(
        counter(
            "SELECT count(*) FROM class_group WHERE teacher IS NOT NULL",
            &db,
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

/// `?grade_level=` is the read a manager holding a blueprint's skip list
/// needs: which sections carry the rung the pump keyed on. It is the *query's*
/// `WHERE`, so `total` counts the filtered set and a window walks that set
/// alone — drop the predicate and every assertion below sees the unfiltered
/// four instead.
#[tokio::test]
async fn the_class_index_filters_by_grade() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mgr", "manager").await;
    for (name, grade_level) in [
        ("9-A", 9),
        ("9-B", 9),
        ("10-A", 10),
        // A club-shaped section: created at the ladder's floor, like every
        // class — it just never shares a rung with an ordinary section here.
        ("satranc", 0),
    ] {
        create_class(&app, &manager, json!({ "name": name, "grade_level": grade_level }))
            .await;
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

    // One rung, matched exactly: both sections at it, and a `total` that
    // counts only them.
    let res = list("?grade_level=9").await;
    assert_eq!(total(&res.body), 2, "total counts the filtered set");
    assert_eq!(names(&res), ["9-B", "9-A"], "newest first, 9 only");
    assert_eq!(names(&list("?grade_level=10").await), ["10-A"]);
    // Rungs nobody carries are empty pages, not errors — the integers leave
    // no spelling games to play (an unparseable value like `anaokulu` never
    // reaches the handler: the query decoder refuses it with a `400`).
    for miss in ["?grade_level=11", "?grade_level=12"] {
        let res = list(miss).await;
        assert_eq!(total(&res.body), 0, "GET /classes{miss} total");
        assert!(
            names(&res).is_empty(),
            "GET /classes{miss} is an empty page"
        );
    }

    // The floor is a rung like any other: it filters, it does not mean
    // "no grade" — no such class exists any more.
    let res = list("?grade_level=0").await;
    assert_eq!((total(&res.body), names(&res)), (1, vec!["satranc".into()]));

    // Filter and window compose: the window is cut from the filtered set, and
    // `total` stays that set's size on every page of it.
    let first = list("?grade_level=9&limit=1&offset=0").await;
    assert_eq!(total(&first.body), 2);
    assert_eq!(names(&first), ["9-B"]);
    assert_eq!(first.body["limit"], 1);
    let second = list("?grade_level=9&limit=1&offset=1").await;
    assert_eq!(total(&second.body), 2);
    assert_eq!(names(&second), ["9-A"], "consecutive pages are disjoint");
    assert_eq!(second.body["offset"], 1);
    let past = list("?grade_level=9&limit=1&offset=9").await;
    assert!(names(&past).is_empty(), "past the end is an empty page");
    assert_eq!(
        total(&past.body),
        2,
        "…with the filtered total still honest"
    );

    // A rung the write paths would refuse is refused here too, rather than
    // reading as "no such grade".
    let res = send(
        &app,
        "GET",
        "/classes?grade_level=13",
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.body);
}

/// The member exit releases the roster row its own section wrote — and the
/// repair its sweep must *not* make is handing that row to a section whose row
/// is gone.
///
/// There is no shared row to hand anywhere since the K12 remodel: a şube
/// teaches the course as its **own instance**, so the section the student
/// leaves gives back exactly the seat that section pumped (the fixture asserts
/// that first — each attach writes its own row, tagged with its own class).
/// What survives from the old hand-off is its guard: when the sweep looks for a
/// rival still claiming the row, the rival it may settle on can be a section
/// deleted out from under its own links — a state this layer really carries
/// (`a_membership_whose_class_is_gone_is_counted_but_skipped`) — and such a
/// section must take the release arm. A row tagged with a class that is not
/// there is an enrollment nothing can ever sweep: both ends 404 on link rows
/// that are gone.
///
/// The real interleaving needs the store's conflict detection; what is
/// deterministic here is that half of it: the gone section is never handed the
/// row, the row is released, and the counter lands on zero with it.
#[tokio::test]
async fn a_member_exit_never_hands_a_row_to_a_class_that_is_gone() {
    let (app, db) = app_and_db().await;
    let staff = login_as(&app, &db, "manager", "manager").await;
    let manager = UserId::from_key(&me_id(&app, &staff).await);
    let student = fixture_user(&db, "heir_ogrenci").await;
    let algebra = CourseId::from_key(&create_course(&app, &staff, "algebra").await);
    let mut made = Vec::new();
    for name in ["9-B", "9-A"] {
        let class = class_group::create(
            &db,
            &manager,
            ClassName::try_new(name).unwrap(),
            GradeLevel::new(9).unwrap(),
            None,
            None,
        )
        .await
        .unwrap();
        let class = class.get_id().clone();
        class_member::add(&db, &class, &student, &manager)
            .await
            .unwrap();
        let instance = class_course::attach(&db, &class, &algebra, &manager)
            .await
            .unwrap();
        made.push((class, instance.get_id().clone()));
    }
    let owner = made[0].0.clone();
    let heir = made[1].0.clone();
    let owner_instance = made[0].1.clone();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM enrollment WHERE source = $1",)
            .bind(owner.clone())
            .fetch_one(&db)
            .await
            .unwrap(),
        1,
        "each attach pumps its own instance: the first class owns the row it wrote"
    );

    // The heir's class row goes while both of its link rows stay: the state a
    // class deleted out from under its own links leaves.
    wipe_row("class_group", heir.uuid(), &db).await;

    class_member::leave(&db, &owner, &student).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM enrollment WHERE class_course = $1")
            .bind(owner_instance.uuid())
            .fetch_one(&db)
            .await
            .unwrap(),
        0,
        "a row handed to a class that is not there is one nothing can ever sweep"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(sum(enrollment_count), 0)::bigint FROM class_course WHERE id = $1"
        )
        .bind(owner_instance.uuid())
        .fetch_one(&db)
        .await
        .unwrap(),
        0,
        "…and its seat must come back with it"
    );
}

/// #22. A class in an archived academic year is read-only: every write axis it
/// has — the class itself, its roster, its instances, their rosters and the
/// blueprint pump — answers the coded `academic_year_archived` 409, while every
/// read stays open. Re-opening the year thaws all of it.
///
/// The year is what a şube hangs off since the K12 remodel (a dönem is a
/// grading slice *inside* it), so this is the same freeze the term used to
/// carry, one level up. There is no archive route for a year yet — the `409`
/// that would gate one has no door — so the past is minted the way an operator
/// would: the column directly.
#[tokio::test]
async fn an_archived_year_freezes_every_class_write_and_no_read() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "arch_manager", "manager").await;
    let student = login_as(&app, &db, "arch_student", "student").await;
    let other = login_as(&app, &db, "arch_other", "student").await;
    let student_id = me_id(&app, &student).await;
    let other_id = me_id(&app, &other).await;

    let year = create_year(&app, &manager, "2024-2025").await;
    let term_id = create_term(&app, &manager, &year, "2024").await;

    // The course to attach, and a second one that stays unattached — what
    // proves a refusal came from the *class's* side.
    let dated = create_course(&app, &manager, "algebra").await;
    let open_course = create_course(&app, &manager, "geometry").await;

    let class = create_class(
        &app,
        &manager,
        json!({ "name": "9-A", "grade_level": 9, "year": year }),
    )
    .await;
    let class = common::id_of(&class.body);

    // Everything the frozen state must already hold: an instance, a member,
    // and a blueprint at this grade for the pump to try.
    let attached = send(
        &app,
        "POST",
        &format!("/classes/{class}/instances"),
        Some(&manager),
        Some(json!({ "course_id": dated })),
    )
    .await;
    assert_eq!(attached.status, StatusCode::CREATED, "attach while open");
    let instance = instance_of(&attached);
    let joined = send(
        &app,
        "POST",
        &format!("/classes/{class}/members"),
        Some(&manager),
        Some(json!({ "user_id": student_id })),
    )
    .await;
    assert_eq!(joined.status, StatusCode::CREATED, "member while open");
    let blueprint = send(
        &app,
        "POST",
        "/classes/blueprints",
        Some(&manager),
        Some(json!({ "grade_level": 9, "course_ids": [dated] })),
    )
    .await;
    assert_eq!(blueprint.status, StatusCode::CREATED, "blueprint");

    // The year goes past: nothing in the API archives one yet, so the column
    // is written the way the archive half of a term used to write it.
    sqlx::query("UPDATE academic_year SET archived_at = $1 WHERE id = $2")
        .bind(1_700_000_000_000_i64)
        .bind(uuid::Uuid::parse_str(&year).unwrap())
        .execute(&db)
        .await
        .unwrap();

    // One refusal per write route, each 409 with the machine code — never a
    // bare 409, which a client cannot tell from any other refusal.
    let writes: Vec<(&str, String, Option<serde_json::Value>)> = vec![
        (
            "PATCH",
            format!("/classes/{class}"),
            Some(json!({ "name": "9-B", "grade_level": 9 })),
        ),
        (
            "POST",
            format!("/classes/{class}/members"),
            Some(json!({ "user_id": other_id })),
        ),
        (
            "DELETE",
            format!("/classes/{class}/members/{student_id}"),
            None,
        ),
        (
            "POST",
            format!("/classes/{class}/instances"),
            // An unattached *catalog* course: the class's own year is the
            // refusal, which is what this probe is for.
            Some(json!({ "course_id": open_course })),
        ),
        (
            "DELETE",
            format!("/classes/{class}/instances/{instance}"),
            None,
        ),
        ("POST", format!("/classes/{class}/blueprint"), None),
        ("DELETE", format!("/classes/{class}"), None),
        (
            "PATCH",
            format!("/instances/{instance}"),
            Some(json!({ "ders_saati": 4 })),
        ),
        (
            "POST",
            format!("/instances/{instance}/enrollments"),
            Some(json!({ "user_id": other_id })),
        ),
        (
            "POST",
            format!("/instances/{instance}/sessions"),
            Some(json!({ "starts_at": 1_900_000_000_000_i64 })),
        ),
    ];
    for (method, uri, body) in writes {
        let res = send(&app, method, &uri, Some(&manager), body).await;
        assert_eq!(res.status, StatusCode::CONFLICT, "{method} {uri}");
        assert_eq!(
            res.body["code"], "academic_year_archived",
            "{method} {uri} code"
        );
    }

    // Reads are untouched — a past year is read-only, not hidden.
    for uri in [
        format!("/classes/{class}"),
        format!("/classes/{class}/members"),
        format!("/classes/{class}/instances"),
        format!("/instances/{instance}"),
        format!("/academic-years/{year}"),
        format!("/terms/{term_id}"),
    ] {
        let res = send(&app, "GET", &uri, Some(&manager), None).await;
        assert_eq!(res.status, StatusCode::OK, "GET {uri}");
    }
    let members = send(
        &app,
        "GET",
        &format!("/classes/{class}/members"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(total(&members.body), 1, "no write landed");

    // Re-opening the year thaws the whole set; one write is enough to show it.
    sqlx::query("UPDATE academic_year SET archived_at = NULL WHERE id = $1")
        .bind(uuid::Uuid::parse_str(&year).unwrap())
        .execute(&db)
        .await
        .unwrap();
    let patched = send(
        &app,
        "PATCH",
        &format!("/classes/{class}"),
        Some(&manager),
        Some(json!({ "name": "9-B", "grade_level": 9 })),
    )
    .await;
    assert_eq!(patched.status, StatusCode::OK, "thawed: {:?}", patched.body);
    assert_eq!(patched.body["name"], "9-B");
}

// ---- the instance anchor ----------------------------------------------------

/// The remodel's core promise: a catalog course is a *title* shared by every
/// section that teaches it, and each şube teaching it is its own instance. Two
/// şubeler attaching one course mint two instances; a student of 5-A is on
/// 5-A's roster alone; and an exam written on 5-A's instance is invisible from
/// 5-B's — which is exactly what the old school-wide singleton could not do.
#[tokio::test]
async fn two_subeler_teaching_one_course_are_two_instances() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "manager", "manager").await;
    let student = login_as(&app, &db, "student", "student").await;
    let student_id = me_id(&app, &student).await;

    let year = create_year(&app, &manager, "2026-2027").await;
    let term = create_term(&app, &manager, &year, "1. Dönem").await;
    let course = create_course(&app, &manager, "Matematik").await;

    let a = create_class(
        &app,
        &manager,
        json!({ "name": "5-A", "grade_level": 5, "year": year }),
    )
    .await;
    let a = common::id_of(&a.body);
    let b = create_class(
        &app,
        &manager,
        json!({ "name": "5-B", "grade_level": 5, "year": year }),
    )
    .await;
    let b = common::id_of(&b.body);

    let ia = send(
        &app,
        "POST",
        &format!("/classes/{a}/instances"),
        Some(&manager),
        Some(json!({ "course_id": course })),
    )
    .await;
    assert_eq!(ia.status, StatusCode::CREATED, "{:?}", ia.body);
    let ia = instance_of(&ia);
    let ib = send(
        &app,
        "POST",
        &format!("/classes/{b}/instances"),
        Some(&manager),
        Some(json!({ "course_id": course })),
    )
    .await;
    assert_eq!(ib.status, StatusCode::CREATED, "{:?}", ib.body);
    let ib = instance_of(&ib);
    assert_ne!(
        ia, ib,
        "one catalog course taught by two şubeler is two instances"
    );

    // Each instance names the same catalog course, and its own şube.
    for (instance, class) in [(&ia, &a), (&ib, &b)] {
        let read = send(
            &app,
            "GET",
            &format!("/instances/{instance}"),
            Some(&manager),
            None,
        )
        .await;
        assert_eq!(read.status, StatusCode::OK, "{:?}", read.body);
        assert_eq!(read.body["course"], json!(course), "both teach one course");
        assert_eq!(read.body["class"], json!(class));
    }

    // A student put in 5-A lands on 5-A's roster — 5-B's stays empty.
    add_member(&app, &manager, &a, &student_id).await;
    let in_a = send(
        &app,
        "GET",
        &format!("/instances/{ia}/enrollments"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(total(&in_a.body), 1, "{:?}", in_a.body);
    assert_eq!(items(&in_a.body)[0]["user"]["id"], json!(student_id));
    let in_b = send(
        &app,
        "GET",
        &format!("/instances/{ib}/enrollments"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(
        total(&in_b.body),
        0,
        "5-B's roster is its own: {:?}",
        in_b.body
    );

    // The exam belongs to the instance it was written on, not to the course.
    let exam = create_exam(&app, &manager, &ia, &term, "1. Yazılı", "yazili").await;
    let seen = send(
        &app,
        "GET",
        &format!("/instances/{ia}/exams"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(total(&seen.body), 1, "{:?}", seen.body);
    assert_eq!(items(&seen.body)[0]["id"], json!(exam));
    let blind = send(
        &app,
        "GET",
        &format!("/instances/{ib}/exams"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(blind.status, StatusCode::OK);
    assert_eq!(
        total(&blind.body),
        0,
        "5-B must not see the exam 5-A sat: {:?}",
        blind.body
    );
}

/// A leave is a *stint*, not a deletion: the history row stays, the counter
/// counts live rows only, and rejoining opens a second stint. That is what
/// keeps the roster cap honest — a şube whose students cycle through it must
/// not fill up on rows nobody holds.
#[tokio::test]
async fn a_leaver_rejoins_into_a_second_stint() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "manager", "manager").await;
    let student = login_as(&app, &db, "student", "student").await;
    let other = login_as(&app, &db, "other", "student").await;
    let student_id = me_id(&app, &student).await;
    let other_id = me_id(&app, &other).await;

    let class = create_class(&app, &manager, json!({ "name": "5-A" })).await;
    let class = common::id_of(&class.body);

    add_member(&app, &manager, &class, &student_id).await;
    assert_eq!(
        counter(
            "SELECT COALESCE(sum(class_member_count), 0)::bigint FROM class_group",
            &db
        )
        .await,
        1
    );

    let left = send(
        &app,
        "DELETE",
        &format!("/classes/{class}/members/{student_id}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(left.status, StatusCode::NO_CONTENT, "{:?}", left.body);
    assert_eq!(
        counter("SELECT count(*) FROM class_member WHERE app_user = (SELECT id FROM app_user WHERE username = 'student')", &db).await,
        1,
        "the stint is stamped, not swept: the section keeps the record"
    );
    assert_eq!(
        counter(
            "SELECT count(*) FROM class_member WHERE left_at IS NULL AND app_user = (SELECT id FROM app_user WHERE username = 'student')",
            &db
        )
        .await,
        0,
        "…and nothing of it is live"
    );
    assert_eq!(
        counter(
            "SELECT COALESCE(sum(class_member_count), 0)::bigint FROM class_group",
            &db
        )
        .await,
        0,
        "the counter follows the live stints, so the seat came back"
    );

    // The rejoined student is a fresh row beside the history — the partial
    // index allows exactly that (one live stint per pair).
    add_member(&app, &manager, &class, &student_id).await;
    assert_eq!(
        counter("SELECT count(*) FROM class_member WHERE app_user = (SELECT id FROM app_user WHERE username = 'student')", &db).await,
        2
    );
    assert_eq!(
        counter(
            "SELECT count(*) FROM class_member WHERE left_at IS NULL AND app_user = (SELECT id FROM app_user WHERE username = 'student')",
            &db
        )
        .await,
        1
    );
    assert_eq!(
        counter(
            "SELECT COALESCE(sum(class_member_count), 0)::bigint FROM class_group",
            &db
        )
        .await,
        1,
        "the counter is the live count, never the row count"
    );

    // The cap reads that counter, so a live leave frees the seat it was
    // counting. Seeded rather than filled by `MAX_CLASS_MEMBERS` real adds:
    // the counter *is* what the claim reads.
    sqlx::query("UPDATE class_group SET class_member_count = $1 WHERE id = $2")
        .bind(MAX_CLASS_MEMBERS)
        .bind(uuid::Uuid::parse_str(&class).unwrap())
        .execute(&db)
        .await
        .unwrap();
    let refused = send(
        &app,
        "POST",
        &format!("/classes/{class}/members"),
        Some(&manager),
        Some(json!({ "user_id": other_id })),
    )
    .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{:?}", refused.body);
    assert_eq!(refused.body["code"], "class_at_roster_ceiling");

    let freed = send(
        &app,
        "DELETE",
        &format!("/classes/{class}/members/{student_id}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(freed.status, StatusCode::NO_CONTENT, "{:?}", freed.body);
    let admitted = send(
        &app,
        "POST",
        &format!("/classes/{class}/members"),
        Some(&manager),
        Some(json!({ "user_id": other_id })),
    )
    .await;
    assert_eq!(
        admitted.status,
        StatusCode::CREATED,
        "a leave hands the seat back to the cap: {:?}",
        admitted.body
    );
    assert_eq!(
        counter(
            "SELECT COALESCE(sum(class_member_count), 0)::bigint FROM class_group",
            &db
        )
        .await,
        MAX_CLASS_MEMBERS,
        "…and the counter lands back on the ceiling, not past it"
    );
}

/// The dönem's own number is one average over the student's instances, each
/// weighted by its `ders_saati` — the karne weight the şube set — and each line
/// is labelled from the school's grade bands. Two instances at different hours
/// are what makes the weighting visible: a plain mean of 60 and 80 is 70, the
/// weighted one 75.
#[tokio::test]
async fn the_karne_weights_each_instance_by_its_ders_saati() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "manager", "manager").await;
    let student = login_as(&app, &db, "student", "student").await;
    let student_id = me_id(&app, &student).await;

    // One şube, two courses it teaches: two instances of one section.
    let t = taught(&app, &manager, "Matematik").await;
    let fizik = create_course(&app, &manager, "Fizik").await;
    let fizik = attach_instance(&app, &manager, &t.class, &fizik).await;
    let hours = send(
        &app,
        "PATCH",
        &format!("/instances/{fizik}"),
        Some(&manager),
        Some(json!({ "ders_saati": 3 })),
    )
    .await;
    assert_eq!(hours.status, StatusCode::OK, "{:?}", hours.body);
    assert_eq!(hours.body["ders_saati"], json!(3));

    add_member(&app, &manager, &t.class, &student_id).await;

    // A mark in each instance, graded by the office.
    for (instance, mark) in [(&t.instance, 60), (&fizik, 80)] {
        let exam = create_exam(&app, &manager, instance, &t.term, "1. Yazılı", "yazili").await;
        let graded = send(
            &app,
            "POST",
            &format!("/exams/{exam}/results"),
            Some(&manager),
            Some(json!({ "mark": mark, "user_id": student_id })),
        )
        .await;
        assert_eq!(
            graded.status,
            StatusCode::OK,
            "grade {mark}: {:?}",
            graded.body
        );
    }

    let karne = send(
        &app,
        "GET",
        &format!("/marks/karne?term={}", t.term),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(karne.status, StatusCode::OK, "{:?}", karne.body);
    let lines = karne.body["instances"].as_array().expect("instances[]");
    assert_eq!(lines.len(), 2, "{:?}", karne.body);
    let line = |instance: &str| {
        lines
            .iter()
            .find(|line| line["class_course"] == json!(instance))
            .unwrap_or_else(|| panic!("no line for {instance}: {}", karne.body))
            .clone()
    };
    let math = line(&t.instance);
    assert_eq!(math["course"], json!("Matematik"), "the catalog title");
    assert_eq!(math["ders_saati"], json!(1));
    assert_eq!(math["average"], json!(60.0));
    assert_eq!(math["band"], json!("3"), "60 is a 3 on the default bands");
    let physics = line(&fizik);
    assert_eq!(physics["ders_saati"], json!(3));
    assert_eq!(physics["average"], json!(80.0));
    assert_eq!(physics["band"], json!("4"));
    assert_eq!(
        karne.body["year_average"],
        json!(75.0),
        "the hours weigh, not the line count: {:?}",
        karne.body
    );
    assert_eq!(karne.body["verdict"], json!("gecti"));
}
