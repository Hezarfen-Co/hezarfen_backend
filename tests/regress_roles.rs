//! Regressions for the role sweep and the parent-delete cascades found in the
//! 2026-08-02 sweep: an event seat a demotion left claimed forever, and the
//! two-query deletes that could orphan a child row.
//!
//! Every assertion re-reads the *store*: nothing below is judged on a response
//! body or asked who won a race — the cascade tests drive the reachable half
//! and pin what the store holds once the transaction is done.

mod common;

use axum::http::StatusCode;
use common::{app_and_db, id_of, login, login_as, me_id, send};
use hezarfen_backend::database::Database;
use hezarfen_backend::domain::timestamp::Timestamp;
use serde_json::json;

/// One counter, re-read out of the store — never off a response body. `sql`
/// is a whole scalar query.
async fn counter(sql: &'static str, db: &Database) -> i64 {
    sqlx::query_scalar(sql).fetch_one(db).await.unwrap()
}

/// How many rows `sql` counts.
async fn rows(sql: &'static str, db: &Database) -> i64 {
    counter(sql, db).await
}

/// Put `user` on `event`'s signup list as `cookie`.
async fn seat(app: &axum::Router, cookie: &str, event: &str, user: &str) -> common::Res {
    send(
        app,
        "POST",
        &format!("/events/{event}/register"),
        Some(cookie),
        Some(json!({ "user_id": user })),
    )
    .await
}

/// A one-seat signup list.
async fn capped_event(app: &axum::Router, teacher: &str, capacity: Option<i64>) -> String {
    let audience = match capacity {
        Some(capacity) => json!({ "kind": "registration", "capacity": capacity }),
        None => json!({ "kind": "registration" }),
    };
    let res = send(
        app,
        "POST",
        "/events",
        Some(teacher),
        Some(json!({ "title": "Gezi", "audience": audience })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    id_of(&res.body)
}

/// Free `user`'s seat on `event` as `cookie`.
async fn free_seat(app: &axum::Router, cookie: &str, event: &str, user: &str) -> common::Res {
    send(
        app,
        "DELETE",
        &format!("/events/{event}/register/{user}"),
        Some(cookie),
        None,
    )
    .await
}

/// Push an event's start into the past, which is how its signup list freezes.
/// The API refuses to *schedule* one there (60s grace), so the row is aged in
/// the store — the state a real event reaches by the clock simply running on.
async fn age_event(event: &str, db: &Database) {
    sqlx::query("UPDATE event SET starts_at = 1 WHERE id = $1")
        .bind(uuid::Uuid::parse_str(event).expect("a uuid event id"))
        .execute(db)
        .await
        .unwrap();
}

/// The role sweep dropped enrollments, class memberships, parent links and
/// course staffing — but not signups. A `parent`'s seat is the one that is
/// genuinely *unfreeable*: they cannot reach `DELETE /events/{id}/register/…`
/// (teacher+ only) and no one else may free a non-student's seat, so a
/// capacity-1 event answered "full" forever and the stale holder stayed on the
/// roster (a registration audience resolves on the row's mere existence).
#[tokio::test]
async fn a_demotion_frees_the_event_seat_nothing_else_could_free() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ogretmen", "teacher").await;
    let admin = login_as(&app, &db, "yonetici", "admin").await;
    let ali = login(&app, "ali").await;
    let veli = login(&app, "veli").await;
    let ali_id = me_id(&app, &ali).await;
    let veli_id = me_id(&app, &veli).await;

    let event = capped_event(&app, &teacher, Some(1)).await;
    let res = seat(&app, &teacher, &event, &ali_id).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        counter(
            "SELECT COALESCE(sum(registration_count), 0)::bigint FROM event",
            &db
        )
        .await,
        1
    );

    let res = send(
        &app,
        "PATCH",
        &format!("/users/{ali_id}/role"),
        Some(&admin),
        Some(json!({ "role": "parent" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // The row goes and the seat comes back with it — one is worthless without
    // the other.
    assert_eq!(
        rows("SELECT count(*) FROM registration", &db).await,
        0,
        "a non-student may hold no signup row"
    );
    assert_eq!(
        counter(
            "SELECT COALESCE(sum(registration_count), 0)::bigint FROM event",
            &db
        )
        .await,
        0,
        "the seat must be handed back, or the event is full forever"
    );

    // And the freed seat is really usable.
    let res = seat(&app, &teacher, &event, &veli_id).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/events/{event}/roster"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(common::total(&res.body), 1, "{}", res.body);
}

/// The mirror ruling: a *promotion* strands nothing, because staff free their
/// own seats by hand. Sweeping there destroyed a signup a teacher is entitled
/// to keep — and irreversibly, since re-demoting restores nothing. The seat
/// stays, and the promoted teacher can still give it back themselves.
#[tokio::test]
async fn a_promotion_keeps_the_signup_the_promoted_user_can_still_free() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ogretmen", "teacher").await;
    let admin = login_as(&app, &db, "yonetici", "admin").await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;

    let event = capped_event(&app, &teacher, Some(1)).await;
    assert_eq!(
        seat(&app, &teacher, &event, &ali_id).await.status,
        StatusCode::OK
    );

    let res = send(
        &app,
        "PATCH",
        &format!("/users/{ali_id}/role"),
        Some(&admin),
        Some(json!({ "role": "teacher" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    assert_eq!(
        rows("SELECT count(*) FROM registration", &db).await,
        1,
        "a promotion must not destroy a signup its holder can still free"
    );
    assert_eq!(
        counter(
            "SELECT COALESCE(sum(registration_count), 0)::bigint FROM event",
            &db
        )
        .await,
        1,
        "…and the seat it holds stays claimed with it"
    );

    // Which is the whole justification: the seat is still theirs to free.
    let promoted = send(
        &app,
        "POST",
        "/auth/login",
        None,
        Some(json!({ "username": "ali", "password": "secret1" })),
    )
    .await
    .cookie
    .expect("re-login");
    let res = free_seat(&app, &promoted, &event, &ali_id).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
    assert_eq!(rows("SELECT count(*) FROM registration", &db).await, 0);
    assert_eq!(
        counter(
            "SELECT COALESCE(sum(registration_count), 0)::bigint FROM event",
            &db
        )
        .await,
        0
    );
}

/// A signup list freezes when its event starts, and from then on it is
/// historical record: `register` and `unregister` both answer 409. The sweep
/// obeys the same freeze — rewriting a closed roster behind its back is
/// irrecoverable, since re-registering is refused too.
#[tokio::test]
async fn a_demotion_leaves_a_frozen_signup_list_exactly_as_it_stands() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ogretmen", "teacher").await;
    let admin = login_as(&app, &db, "yonetici", "admin").await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;

    let event = capped_event(&app, &teacher, Some(1)).await;
    assert_eq!(
        seat(&app, &teacher, &event, &ali_id).await.status,
        StatusCode::OK
    );
    age_event(&event, &db).await;

    // The freeze bites for the route…
    let res = free_seat(&app, &teacher, &event, &ali_id).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // …so it must bite for the sweep as well.
    let res = send(
        &app,
        "PATCH",
        &format!("/users/{ali_id}/role"),
        Some(&admin),
        Some(json!({ "role": "parent" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    assert_eq!(
        rows("SELECT count(*) FROM registration", &db).await,
        1,
        "a closed roster must not be rewritten by a role change"
    );
    assert_eq!(
        counter(
            "SELECT COALESCE(sum(registration_count), 0)::bigint FROM event",
            &db
        )
        .await,
        1,
        "…and its seat stays claimed with it — the row is the seat"
    );
}

/// An orphan signup — its event record gone — has no seat to give back and no
/// list that can freeze, and `unregister` answers 404 on the missing event. So
/// the sweep must *delete* it rather than skip it: skipped, it is the exact
/// unremovable row the sweep exists to prevent. (Low reach — `Event::delete`
/// cascades in one transaction — but this is the class, not a coincidence.)
#[tokio::test]
async fn the_sweep_deletes_an_orphan_signup_instead_of_stranding_it() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ogretmen", "teacher").await;
    let admin = login_as(&app, &db, "yonetici", "admin").await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;

    let live = capped_event(&app, &teacher, None).await;
    let doomed = capped_event(&app, &teacher, None).await;
    assert_eq!(
        seat(&app, &teacher, &live, &ali_id).await.status,
        StatusCode::OK
    );
    assert_eq!(
        seat(&app, &teacher, &doomed, &ali_id).await.status,
        StatusCode::OK
    );

    // The state a delete that raced a register used to leave: the row outlives
    // its event. Written straight into the store, since the cascade now makes
    // it unreachable through the API.
    // The state a delete that raced a register used to leave: the row outlives
    // its event. Written straight into the store, since the cascade now makes
    // it unreachable through the API. Real FKs refuse that state, so the
    // delete is forced with FK triggers suspended for the one transaction.
    let mut tx = db.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role = replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("DELETE FROM event WHERE id = $1")
        .bind(uuid::Uuid::parse_str(&doomed).expect("a uuid event id"))
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let res = send(
        &app,
        "PATCH",
        &format!("/users/{ali_id}/role"),
        Some(&admin),
        Some(json!({ "role": "parent" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    assert_eq!(
        rows("SELECT count(*) FROM registration", &db).await,
        0,
        "an orphan signup must go with the rest — nothing else can ever remove it"
    );
    assert_eq!(
        counter(
            "SELECT COALESCE(sum(registration_count), 0)::bigint FROM event",
            &db
        )
        .await,
        0,
        "and the surviving event still gets its seat back"
    );
}

/// How many boards list `user` as a participant, read out of the store.
async fn boards_listing(user: &str, db: &Database) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM board_participant WHERE participant = $1")
        .bind(uuid::Uuid::parse_str(user).expect("a uuid user id"))
        .fetch_one(db)
        .await
        .unwrap()
}

/// A `parent` is barred from the whiteboard outright, and invites already
/// refuse the role — but `set_role` never touched a roster, so a demotion left
/// the user listed forever: still fanned every stroke, and a stale id that 400s
/// the creator's next roster PATCH. The sweep drops them from every list and
/// **deletes nothing** — not a board, not a stroke, not even the board they
/// created, whose room carries on for everyone else with a creator who can no
/// longer use it (a role change must not destroy other people's work; there is
/// no way to hand a board over, so closing or deleting it would be the role
/// change silently ending a live session).
#[tokio::test]
async fn a_demotion_to_parent_leaves_every_board_roster_and_deletes_nothing() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "yonetici", "admin").await;
    let ali = login(&app, "ali").await;
    let veli = login(&app, "veli").await;
    let ali_id = me_id(&app, &ali).await;
    let veli_id = me_id(&app, &veli).await;

    // One board veli was invited to, one board veli created.
    let guest = send(
        &app,
        "POST",
        "/boards",
        Some(&ali),
        Some(json!({ "title": "Geometri", "participants": [veli_id] })),
    )
    .await;
    assert_eq!(guest.status, StatusCode::CREATED, "{}", guest.body);
    let own = send(
        &app,
        "POST",
        "/boards",
        Some(&veli),
        Some(json!({ "title": "Cebir", "participants": [ali_id] })),
    )
    .await;
    assert_eq!(own.status, StatusCode::CREATED, "{}", own.body);
    let own_id = id_of(&own.body);
    assert_eq!(boards_listing(&veli_id, &db).await, 1);

    let res = send(
        &app,
        "PATCH",
        &format!("/users/{veli_id}/role"),
        Some(&admin),
        Some(json!({ "role": "parent" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    assert_eq!(
        boards_listing(&veli_id, &db).await,
        0,
        "a parent may appear on no board roster"
    );
    // Nothing was destroyed, and the board they created is untouched beyond
    // that: same creator, and its other participant still on it.
    assert_eq!(rows("SELECT count(*) FROM board", &db).await, 2);
    let res = send(&app, "GET", &format!("/boards/{own_id}"), Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["creator"], veli_id);
    assert_eq!(res.body["participants"], json!([ali_id]));

    // And the demoted creator is barred from their own board, by role alone.
    let res = send(&app, "GET", &format!("/boards/{own_id}"), Some(&veli), None).await;
    assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.body);
}

/// Everything a demotion to `parent` must sweep, set up on one student: an
/// enrollment (with its seat), a parent link, a board roster entry and an event
/// signup. Answers `(admin cookie, teacher cookie, ali's id)`.
async fn a_student_holding_every_grant(
    app: &axum::Router,
    db: &Database,
) -> (String, String, String) {
    let admin = login_as(app, db, "yonetici", "admin").await;
    let teacher = login_as(app, db, "ogretmen", "teacher").await;
    let anne = login_as(app, db, "anne", "parent").await;
    let anne_id = me_id(app, &anne).await;
    let ali = login(app, "ali").await;
    let veli = login(app, "veli").await;
    let ali_id = me_id(app, &ali).await;

    let course = common::create_course(app, &teacher, "Fizik").await;
    common::enroll(app, &teacher, &course, &ali_id).await;
    let res = send(
        app,
        "POST",
        &format!("/users/{anne_id}/students"),
        Some(&admin),
        Some(json!({ "user_id": ali_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        app,
        "POST",
        "/boards",
        Some(&veli),
        Some(json!({ "title": "Geometri", "participants": [ali_id] })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let event = capped_event(app, &teacher, Some(1)).await;
    let res = seat(app, &teacher, &event, &ali_id).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    (admin, teacher, ali_id)
}

/// `user`'s live role, read out of the store.
async fn role_of(user: &str, db: &Database) -> String {
    sqlx::query_scalar("SELECT role FROM app_user WHERE id = $1")
        .bind(uuid::Uuid::parse_str(user).expect("a uuid user id"))
        .fetch_one(db)
        .await
        .expect("the user row must exist")
}

/// Make the next write of `kind` on `table` fail, from inside whatever
/// transaction performs it: a trigger that raises aborts that very write, and
/// with it the whole cascade's transaction — the only way to fail one statement
/// of a cascade deterministically (no sleeps, no racing tasks).
async fn poison(table: &str, kind: &str, db: &Database) {
    let firing = match kind {
        "CREATE" => "INSERT",
        "UPDATE" => "UPDATE",
        "DELETE" => "DELETE",
        other => panic!("unknown event kind {other}"),
    };
    let mut conn = db.acquire().await.expect("acquire for the trigger");
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE FUNCTION heztest_poison() RETURNS trigger AS $$
         BEGIN RAISE EXCEPTION 'poisoned'; END;
         $$ LANGUAGE plpgsql;
         CREATE TRIGGER heztest_poison BEFORE {firing} ON {table}
         FOR EACH ROW EXECUTE FUNCTION heztest_poison();"
    )))
    .execute(&mut *conn)
    .await
    .expect("define the poison trigger");
}

/// Take the poison back, so the re-sent PATCH can land.
async fn unpoison(table: &str, db: &Database) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP TRIGGER IF EXISTS heztest_poison ON {table};
         DROP FUNCTION IF EXISTS heztest_poison();"
    )))
    .execute(db)
    .await
    .expect("remove the poison trigger");
}

/// Every grant of `ali_id` still standing, and the old role with them — what
/// must hold after a cascade that failed anywhere in its middle.
async fn nothing_was_swept(ali_id: &str, db: &Database) {
    assert_eq!(
        role_of(ali_id, db).await,
        "student",
        "the role write must roll back with the sweep that failed"
    );
    assert_eq!(
        rows("SELECT count(*) FROM enrollment", db).await,
        1,
        "the enrollment must survive a failed cascade"
    );
    assert_eq!(
        counter(
            "SELECT COALESCE(sum(enrollment_count), 0)::bigint FROM course",
            db
        )
        .await,
        1,
        "…and so must its seat, or the roster and the count disagree forever"
    );
    assert_eq!(
        rows("SELECT count(*) FROM parent_link", db).await,
        1,
        "the parent link must survive a failed cascade"
    );
    assert_eq!(
        rows("SELECT count(*) FROM registration", db).await,
        1,
        "the signup must survive a failed cascade"
    );
    assert_eq!(
        counter(
            "SELECT COALESCE(sum(registration_count), 0)::bigint FROM event",
            db
        )
        .await,
        1,
        "…with its seat still claimed"
    );
    assert_eq!(
        boards_listing(ali_id, db).await,
        1,
        "the board roster must be untouched"
    );
}

/// The demotion, re-sent with the fault gone: it must land whole, and the seat
/// it frees must be a seat somebody else can actually take — the capacity check
/// reads the counter, so a row deleted without its counter frees nothing.
async fn the_retry_takes_everything(
    app: &axum::Router,
    db: &Database,
    admin: &str,
    teacher: &str,
    ali_id: &str,
) {
    let res = send(
        app,
        "PATCH",
        &format!("/users/{ali_id}/role"),
        Some(admin),
        Some(json!({ "role": "parent" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(role_of(ali_id, db).await, "parent");
    for table in ["enrollment", "parent_link", "registration", "class_member"] {
        assert_eq!(
            sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) FROM {table}"
            )))
            .fetch_one(db)
            .await
            .unwrap(),
            0,
            "{table} must be swept by the re-sent PATCH"
        );
    }
    assert_eq!(boards_listing(ali_id, db).await, 0);
    assert_eq!(
        counter(
            "SELECT COALESCE(sum(enrollment_count), 0)::bigint FROM course",
            db
        )
        .await,
        0
    );
    assert_eq!(
        counter(
            "SELECT COALESCE(sum(registration_count), 0)::bigint FROM event",
            db
        )
        .await,
        0
    );
    // The seat is usable, which is the whole point of freeing it: the one-seat
    // list took the demoted user's row back and answers a new booking.
    let kemal_id = me_id(app, &login(app, "kemal").await).await;
    let event = ids(&send(app, "GET", "/events", Some(teacher), None).await.body)
        .first()
        .cloned()
        .expect("the event");
    let res = seat(app, teacher, &event, &kemal_id).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

/// The role write and every sweep it owes are **one** transaction, and this is
/// what proves it: a statement in the middle of the cascade is made to fail, and
/// the whole thing must be as if the PATCH never ran — the *old* role, and every
/// grant of it still standing. Run as the eight separate queries this used to
/// be, the same failure answered 500 with the role already lowered, the
/// enrollment, the parent link and the event seat already gone, and nothing to
/// ever put them back.
///
/// The failure is injected the way `regress_classes` does it: a trigger on a
/// table the cascade writes raises inside that very write, so the abort is
/// deterministic instead of a race. Both
/// ends of the cascade are poisoned, in two tests, because they fail different
/// things: the board roster is the *last* statement of the parent arm (so every
/// assertion below it is about a write that already succeeded and must be
/// undone), the enrollment delete is one of the *first* (so the assertions are
/// about writes that must never be reached).
#[tokio::test]
async fn a_failure_late_in_the_cascade_leaves_the_old_role_and_every_grant_standing() {
    let (app, db) = app_and_db().await;
    let (admin, teacher, ali_id) = a_student_holding_every_grant(&app, &db).await;

    poison("board_participant", "DELETE", &db).await;
    let res = send(
        &app,
        "PATCH",
        &format!("/users/{ali_id}/role"),
        Some(&admin),
        Some(json!({ "role": "parent" })),
    )
    .await;
    assert!(
        res.status.is_server_error(),
        "the injected failure must not be answered as success: {} {}",
        res.status,
        res.body
    );
    nothing_was_swept(&ali_id, &db).await;

    unpoison("board_participant", &db).await;
    the_retry_takes_everything(&app, &db, &admin, &teacher, &ali_id).await;
}

/// The same fold, failed at the other end: the enrollment delete is an *early*
/// statement of the cascade, so what this pins is that the role write ahead of
/// it and every arm behind it come down with it too — a cascade that only rolls
/// back what ran after the fault is no transaction at all.
#[tokio::test]
async fn a_failure_early_in_the_cascade_rolls_the_role_write_back_with_it() {
    let (app, db) = app_and_db().await;
    let (admin, teacher, ali_id) = a_student_holding_every_grant(&app, &db).await;

    poison("enrollment", "DELETE", &db).await;
    let res = send(
        &app,
        "PATCH",
        &format!("/users/{ali_id}/role"),
        Some(&admin),
        Some(json!({ "role": "parent" })),
    )
    .await;
    assert!(
        res.status.is_server_error(),
        "the injected failure must not be answered as success: {} {}",
        res.status,
        res.body
    );
    nothing_was_swept(&ali_id, &db).await;

    unpoison("enrollment", &db).await;
    the_retry_takes_everything(&app, &db, &admin, &teacher, &ali_id).await;
}

/// The cascade and the parent row now commit together. Run as two queries, a
/// register or a mark landing in between outlived its event — an orphan keyed
/// on a row that no longer exists, so no read path could reach it and no delete
/// could reclaim it. The state is asserted against the store, not the 204.
#[tokio::test]
async fn deleting_an_event_takes_its_children_with_it() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ogretmen", "teacher").await;
    let ali = login(&app, "ali").await;
    let ali_id = me_id(&app, &ali).await;

    let event = capped_event(&app, &teacher, None).await;
    let res = seat(&app, &teacher, &event, &ali_id).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/events/{event}/attendance"),
        Some(&teacher),
        Some(json!({ "user_id": ali_id, "status": "present" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(rows("SELECT count(*) FROM attendance", &db).await, 1);

    let res = send(
        &app,
        "DELETE",
        &format!("/events/{event}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);

    assert_eq!(rows("SELECT count(*) FROM event", &db).await, 0);
    assert_eq!(
        rows("SELECT count(*) FROM attendance", &db).await,
        0,
        "no mark may outlive its event"
    );
    assert_eq!(
        rows("SELECT count(*) FROM registration", &db).await,
        0,
        "no signup may outlive its event"
    );
}

/// The note half of the same defect: the attachment rows and the note commit
/// together, so an upload can no longer leave a `note_file` pointing at a note
/// that is gone (its *blob* goes too: `db::note::delete` returns the rows it
/// removed and the handler unlinks exactly those).
#[tokio::test]
async fn deleting_a_note_takes_its_attachment_rows_with_it() {
    let (app, db) = app_and_db().await;
    let ali = login(&app, "ali").await;

    let res = send(
        &app,
        "POST",
        "/notes",
        Some(&ali),
        Some(json!({ "title": "Plan", "content": "yarın" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let note = id_of(&res.body);

    let up = common::upload_file(&app, &ali, &note, "plan.txt", "text/plain", b"merhaba").await;
    assert_eq!(up.status, StatusCode::CREATED, "{}", up.body);
    assert_eq!(rows("SELECT count(*) FROM note_file", &db).await, 1);

    let res = send(&app, "DELETE", &format!("/notes/{note}"), Some(&ali), None).await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);

    assert_eq!(rows("SELECT count(*) FROM note", &db).await, 0);
    assert_eq!(
        rows("SELECT count(*) FROM note_file", &db).await,
        0,
        "no attachment row may outlive its note"
    );
}

/// Ids in a `Page` envelope (or a bare array).
fn ids(body: &serde_json::Value) -> Vec<String> {
    common::items(body).iter().map(id_of).collect()
}

/// The demoted-creator privilege leak: `creator` is a historical column no
/// demotion sweeps, so course management had to re-read the caller's *live*
/// role. Both halves are asserted — the gate passes before the demotion and
/// refuses after it, so this proves the gate flipped, not a permanent 404.
#[tokio::test]
async fn demoted_creator_loses_course_management_over_http() {
    let (app, db) = app_and_db().await;
    let admin = login_as(&app, &db, "yonetici", "admin").await;
    let teacher = login_as(&app, &db, "ogretmen", "teacher").await;
    let student = login_as(&app, &db, "ogrenci", "student").await;
    let teacher_id = me_id(&app, &teacher).await;
    let student_id = me_id(&app, &student).await;

    let course = common::create_course(&app, &teacher, "Fizik").await;
    let subject = common::create_subject(&app, &teacher, &course, "Kuvvet").await;
    let res = common::create_exam_with(
        &app,
        &teacher,
        &course,
        json!({ "title": "Ara", "kind": "quiz", "draft": true }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let draft = id_of(&res.body);
    // Assigned to the other student only, so seeing it is a *management* right
    // and never the audience right a course member has.
    let due = Timestamp::now().as_millis() + 86_400_000;
    common::enroll(&app, &teacher, &course, &student_id).await;
    let res = common::create_homework_with(
        &app,
        &teacher,
        &course,
        json!({ "title": "Odev", "subject_id": subject, "due_at": due, "assigned": [student_id] }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let homework = id_of(&res.body);

    // Before the demotion: the creator manages the course.
    let res = send(
        &app,
        "GET",
        &format!("/exams/{draft}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(&app, "GET", "/homework", Some(&teacher), None).await;
    assert!(ids(&res.body).contains(&homework), "{}", res.body);
    let res = send(
        &app,
        "GET",
        &format!("/homework/{homework}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Demote, then enroll them as an ordinary member of their own course: the
    // course-view gate still passes, so what answers below is the management
    // gate alone.
    common::set_role(&db, "ogretmen", "student").await;
    common::enroll(&app, &admin, &course, &teacher_id).await;

    let res = send(
        &app,
        "GET",
        &format!("/exams/{draft}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::NOT_FOUND,
        "a demoted creator still reads a draft: {}",
        res.body
    );
    let res = send(&app, "GET", "/homework", Some(&teacher), None).await;
    assert!(
        !ids(&res.body).contains(&homework),
        "a demoted creator still lists another student's homework: {}",
        res.body
    );
    let res = send(
        &app,
        "GET",
        &format!("/homework/{homework}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::NOT_FOUND,
        "a demoted creator still reads another student's homework: {}",
        res.body
    );
    // ...and the writes are gone too.
    let res = send(
        &app,
        "PATCH",
        &format!("/courses/{course}"),
        Some(&teacher),
        Some(json!({ "title": "Kimya" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{course}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
}

/// The no-orphan half: the role floor must never leave a course nobody can
/// manage, and it must not touch a teacher who is still teacher+.
#[tokio::test]
async fn manager_and_assigned_teacher_keep_a_demoted_creators_course() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mudur", "manager").await;
    let teacher = login_as(&app, &db, "ogretmen", "teacher").await;
    let helper = login_as(&app, &db, "yardimci", "teacher").await;
    let helper_id = me_id(&app, &helper).await;

    let course = common::create_course(&app, &teacher, "Fizik").await;
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/teachers"),
        Some(&manager),
        Some(json!({ "user_id": helper_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    common::set_role(&db, "ogretmen", "student").await;

    // The course keeps two managers: the manager, and the still-teacher
    // assignee — a course the floor orphaned would be the worse bug.
    for cookie in [&manager, &helper] {
        let res = send(
            &app,
            "PATCH",
            &format!("/courses/{course}"),
            Some(cookie),
            Some(json!({ "title": "Kimya" })),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    }
    // Deleting still needs ownership, which an assignee never had.
    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{course}"),
        Some(&helper),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{course}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);
}

/// The catalog half of the same leak: `visible_courses` built its teacher half
/// out of the creator/assignee columns, so a demoted creator kept seeing the
/// course — and its published exams and homework — in the three catalogs.
#[tokio::test]
async fn demoted_creator_drops_out_of_the_catalogs() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ogretmen", "teacher").await;

    let course = common::create_course(&app, &teacher, "Fizik").await;
    let subject = common::create_subject(&app, &teacher, &course, "Kuvvet").await;
    let exam = common::create_exam(&app, &teacher, &course, "Ara", "quiz").await;
    let due = Timestamp::now().as_millis() + 86_400_000;
    let homework = common::create_homework(&app, &teacher, &course, &subject, "Odev", due).await;

    for (path, wanted) in [
        ("/courses", &course),
        ("/exams", &exam),
        ("/homework", &homework),
    ] {
        let res = send(&app, "GET", path, Some(&teacher), None).await;
        assert!(ids(&res.body).contains(wanted), "{path}: {}", res.body);
    }

    // Demoted and *not* enrolled: the course is nothing to them now.
    common::set_role(&db, "ogretmen", "student").await;

    for (path, gone) in [
        ("/courses", &course),
        ("/exams", &exam),
        ("/homework", &homework),
    ] {
        let res = send(&app, "GET", path, Some(&teacher), None).await;
        assert!(
            !ids(&res.body).contains(gone),
            "a demoted creator still sees their course in {path}: {}",
            res.body
        );
    }
}

/// A session's `teacher` is the same shape of historical column as a course's
/// `creator`: it must grant nothing once the account falls below `teacher`.
#[tokio::test]
async fn demoted_session_teacher_loses_the_session() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "mudur", "manager").await;
    let teacher = login_as(&app, &db, "ogretmen", "teacher").await;
    let teacher_id = me_id(&app, &teacher).await;

    // Owned by the manager, so losing the session is not just a side effect of
    // losing the course: the demoted account was only ever its *teacher*.
    let course = common::create_course(&app, &manager, "Fizik").await;
    let res = send(
        &app,
        "POST",
        &format!("/courses/{course}/teachers"),
        Some(&manager),
        Some(json!({ "user_id": teacher_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let starts_at = Timestamp::now().as_millis() + 86_400_000;
    let session = common::create_session(&app, &teacher, &course, starts_at).await;
    // Drop the assignment: only the session's own teacher column is left.
    let res = send(
        &app,
        "DELETE",
        &format!("/courses/{course}/teachers/{teacher_id}"),
        Some(&manager),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT, "{}", res.body);

    let res = send(
        &app,
        "GET",
        &format!("/sessions/{session}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    common::set_role(&db, "ogretmen", "student").await;

    let res = send(
        &app,
        "GET",
        &format!("/sessions/{session}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::FORBIDDEN,
        "a demoted session teacher still reads their session: {}",
        res.body
    );
}
