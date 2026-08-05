//! Regressions for the four holes the 2026-08-05 hunt left on the homework
//! surface: a file upload that outlived its homework, a course delete that
//! swept homework without holding the homework lease, a subset assignment that
//! handed every named student the whole roster, and a file delete that answered
//! a lost round with a 500.
//!
//! Two of them share one root cause and one shape of fix: a child keyed by a
//! deterministic id does not fail against a parent a cascade removed, it
//! *re-creates* it — so both the submission and the grade now move a value on
//! the homework row inside their own transaction, and the two domain-level
//! tests here (`a_submission_under_a_vanished_homework_is_refused`,
//! `a_grade_under_a_vanished_homework_is_refused`) are what pin those gates.
//! The lock-level races pin the handlers.
//!
//! Every race here is judged on the *store*, never on what a request answered:
//! the in-memory engine forges wins under concurrency (src/domain/cap.rs), so a
//! 404 or a 204 is evidence of nothing. The windows are opened by the database
//! itself — a `DEFINE EVENT` on the table under write fires *inside* the
//! writing transaction — rather than by a lucky interleaving.

mod common;

use axum::http::StatusCode;
use common::{
    app_and_db, create_course, create_homework, create_homework_with, create_subject, enroll,
    id_of, items, login_as, me_id, multipart_file, send, send_raw, unenroll,
};
use hezarfen_backend::database::Database;
use serde_json::{Value, json};
use surrealdb::types::RecordId;

/// `HOMEWORK_LOCK` is process-wide, and every race below holds a lease of it
/// for seconds. Two of them running at once starve each other's window — the
/// second one's gates then run *after* the staging meant to happen inside them,
/// and the test judges an interleaving that never happened. One at a time; the
/// deterministic tests need no ticket.
static ONE_RACE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A due date comfortably clear of the 60s past-scheduling grace.
fn far_future() -> i64 {
    hezarfen_backend::domain::timestamp::Timestamp::now().as_millis() + 7_200_000
}

/// How many rows `sql` selects ids for — the stored state, out of the store.
async fn rows(db: &Database, sql: &str) -> usize {
    let mut result = db.query(sql).await.unwrap().check().unwrap();
    result.take::<Vec<RecordId>>(0).unwrap().len()
}

/// One integer field off one user row, absent counting as zero.
async fn counter(db: &Database, user: &str, field: &str) -> i64 {
    let mut result = db
        .query(format!(
            "SELECT VALUE ({field} ?? 0) FROM type::record('user', $key)"
        ))
        .bind(("key", user.to_string()))
        .await
        .unwrap()
        .check()
        .unwrap();
    result
        .take::<Vec<i64>>(0)
        .unwrap()
        .first()
        .copied()
        .unwrap_or(0)
}

/// Upload `bytes` onto the caller's submission to `hw` — no assertion.
async fn upload_hw_file(app: &axum::Router, cookie: &str, hw: &str) -> StatusCode {
    let (status, _, _) = send_raw(
        app,
        "POST",
        &format!("/homework/{hw}/submission/files"),
        Some(cookie),
        Some("multipart/form-data; boundary=hezarfen-test-boundary"),
        multipart_file("odev.pdf", "application/pdf", b"homework bytes"),
    )
    .await;
    status
}

/// A course with one subject and one enrolled student. Returns
/// `(course, subject, student cookie, student id)`.
async fn course_with_student(
    app: &axum::Router,
    db: &Database,
    teacher: &str,
    name: &str,
) -> (String, String, String, String) {
    let student = login_as(app, db, &format!("ogrenci_{name}"), "student").await;
    let student_id = me_id(app, &student).await;
    let course = create_course(app, teacher, name).await;
    let subject = create_subject(app, teacher, &course, "konu").await;
    enroll(app, teacher, &course, &student_id).await;
    (course, subject, student, student_id)
}

/// The upload streams its body *before* it takes `HOMEWORK_LOCK`, and the
/// homework snapshot it acts on was read before that stream — so a delete
/// (which takes and releases the write lease inside that window) leaves the
/// handler holding a homework that is gone. It then auto-created a submission
/// under it, credited both badge counters, wrote a blob and a file row, and
/// answered `201`. Every route to any of that goes through the vanished
/// homework, so nothing could ever read or delete it again, and the counters
/// stayed up for good.
///
/// The window is the delete's own: a `DEFINE EVENT` on `homework` holds its
/// transaction open after the cascade has swept the children, which is exactly
/// the interleaving — the upload's gate read lands inside it (the row is
/// deleted but uncommitted, so it still reads), and its write lands after. The
/// upload's own duration is asserted too: one that arrived after the delete had
/// already committed would be answered by a plain lookup and leave this test
/// green over the hole.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_upload_inside_a_homework_delete_never_orphans() {
    let _serial = ONE_RACE_AT_A_TIME.lock().await;
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ogretmen_odev", "teacher").await;
    let (course, subject, student, student_id) =
        course_with_student(&app, &db, &teacher, "fizik").await;
    let hw = create_homework(&app, &teacher, &course, &subject, "deneme", far_future()).await;

    // Hold the delete open once the cascade has run but before it commits.
    db.query(
        "DEFINE EVENT hold_the_window ON TABLE homework WHEN $event = 'DELETE' \
         THEN { SLEEP 3s; };",
    )
    .await
    .expect("define the window event")
    .check()
    .expect("check the window event");

    let drop_it = {
        let (app, teacher, hw) = (app.clone(), teacher.clone(), hw.clone());
        tokio::spawn(async move {
            send(
                &app,
                "DELETE",
                &format!("/homework/{hw}"),
                Some(&teacher),
                None,
            )
            .await
        })
    };
    // The upload fires inside the held window: its gate reads a homework the
    // delete has removed but not committed.
    tokio::time::sleep(std::time::Duration::from_millis(1_000)).await;
    let uploaded = {
        let (app, student, hw) = (app.clone(), student.clone(), hw.clone());
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            (upload_hw_file(&app, &student, &hw).await, started.elapsed())
        })
    };
    let (dropped, (upload_status, took)) = (drop_it.await.unwrap(), uploaded.await.unwrap());

    assert!(
        took >= std::time::Duration::from_secs(1),
        "the upload never contended with the delete ({took:?}): nothing was raced"
    );
    assert_eq!(
        dropped.status,
        StatusCode::NO_CONTENT,
        "the delete must succeed: {:?}",
        dropped.body
    );
    // A 404 is the correct answer for the upload; the only defect is stored
    // state. Nothing may 500 either way.
    assert!(
        upload_status == StatusCode::NOT_FOUND || upload_status.is_success(),
        "a raced upload must be answered, not {upload_status}"
    );

    // Stored state is the whole verdict.
    assert_eq!(
        rows(&db, "SELECT VALUE id FROM homework_submission").await,
        0,
        "a submission outlived its homework"
    );
    assert_eq!(
        rows(&db, "SELECT VALUE id FROM homework_file").await,
        0,
        "a file outlived its homework"
    );
    assert_eq!(
        counter(&db, &student_id, "homework_submitted_total").await,
        0,
        "a hand-in nothing can reach still counted"
    );
    assert_eq!(
        counter(&db, &student_id, "homework_on_time_total").await,
        0,
        "... and counted as punctual"
    );
}

/// The other half of the same misplaced gate, and the half no domain guard can
/// cover: the homework is still there, only the *audience* moved. A PATCH
/// re-scoping `assigned` holds the write lease across its orphan check, which
/// finds no submission and allows the narrowing; the upload, whose audience
/// check ran on a snapshot read before its body streamed, then lands a
/// submission and a file for a student the homework no longer names — the exact
/// orphan `ensure_no_orphans` exists to refuse.
///
/// The lease is the interleaving: a `DEFINE EVENT` on `homework` holds the
/// PATCH's write open, the upload's pre-flight gate reads the old audience
/// inside that window, and its own lease is granted only once the new audience
/// is committed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_upload_inside_an_audience_patch_is_refused() {
    let _serial = ONE_RACE_AT_A_TIME.lock().await;
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ogretmen_kapsam", "teacher").await;
    let course = create_course(&app, &teacher, "Coğrafya").await;
    let subject = create_subject(&app, &teacher, &course, "iklim").await;
    let dropped_student = login_as(&app, &db, "ogrenci_cikan", "student").await;
    let dropped_id = me_id(&app, &dropped_student).await;
    let kept = login_as(&app, &db, "ogrenci_kalan", "student").await;
    let kept_id = me_id(&app, &kept).await;
    enroll(&app, &teacher, &course, &dropped_id).await;
    enroll(&app, &teacher, &course, &kept_id).await;
    let res = create_homework_with(
        &app,
        &teacher,
        &course,
        json!({
            "title": "harita",
            "subject_id": subject,
            "due_at": far_future(),
            "assigned": [dropped_id.clone(), kept_id.clone()],
        }),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "create the subset homework"
    );
    let hw = id_of(&res.body);

    db.query(
        "DEFINE EVENT hold_the_patch ON TABLE homework WHEN $event = 'UPDATE' \
         THEN { SLEEP 3s; };",
    )
    .await
    .expect("define the window event")
    .check()
    .expect("check the window event");

    let narrowing = {
        let (app, teacher, hw, kept_id) =
            (app.clone(), teacher.clone(), hw.clone(), kept_id.clone());
        tokio::spawn(async move {
            send(
                &app,
                "PATCH",
                &format!("/homework/{hw}"),
                Some(&teacher),
                Some(json!({ "assigned": [kept_id] })),
            )
            .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(1_000)).await;
    let uploaded = {
        let (app, cookie, hw) = (app.clone(), dropped_student.clone(), hw.clone());
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            (upload_hw_file(&app, &cookie, &hw).await, started.elapsed())
        })
    };
    let (patched, (upload_status, took)) = (narrowing.await.unwrap(), uploaded.await.unwrap());

    assert!(
        took >= std::time::Duration::from_secs(1),
        "the upload never contended with the PATCH ({took:?}): nothing was raced"
    );
    assert_eq!(
        patched.status,
        StatusCode::OK,
        "the narrowing must land: {:?}",
        patched.body
    );
    assert_eq!(
        upload_status,
        StatusCode::NOT_FOUND,
        "a student the homework no longer names must be answered 404"
    );
    assert_eq!(
        rows(&db, "SELECT VALUE id FROM homework_submission").await,
        0,
        "a submission landed for a student outside the audience"
    );
    assert_eq!(
        rows(&db, "SELECT VALUE id FROM homework_file").await,
        0,
        "... with a file on it"
    );
}

/// The same hole one layer down, where the lock cannot reach it: a submission
/// write must collide with its homework's delete on the *store*, not merely
/// read the row first. `HomeworkSubmission::upsert` writes a deterministic id,
/// so with the parent gate removed it does not fail against a vanished
/// homework — it re-creates the row it was meant to be refused, badge counters
/// and all.
///
/// Driven at the domain layer on purpose: it is the guarantee that survives the
/// web layer's lock discipline being loosened again, so no handler may be in
/// the way of it. The delete lands first here — the interleaving pinned rather
/// than hoped for — which is precisely the state a lease that stops too early
/// leaves behind.
#[tokio::test]
async fn a_submission_under_a_vanished_homework_is_refused() {
    use hezarfen_backend::domain::homework::{Homework, HomeworkId};
    use hezarfen_backend::domain::homework_submission::HomeworkSubmission;
    use hezarfen_backend::domain::user::UserId;

    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ogretmen_alan", "teacher").await;
    let (course, subject, student, student_id) =
        course_with_student(&app, &db, &teacher, "kimya").await;
    let hw = create_homework(&app, &teacher, &course, &subject, "deneme", far_future()).await;
    // The stale snapshot a handler would still be holding.
    let stale = Homework::read(&HomeworkId::from_key(&hw), &db)
        .await
        .unwrap()
        .expect("the homework exists");
    let user = UserId::from_key(&student_id);

    let dropped = send(
        &app,
        "DELETE",
        &format!("/homework/{hw}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(
        dropped.status,
        StatusCode::NO_CONTENT,
        "delete the homework"
    );

    let refused = HomeworkSubmission::upsert(&stale, &user, None, &db).await;
    assert!(
        refused.is_err(),
        "a submission to a homework that is gone must be refused, got {refused:?}"
    );
    assert_eq!(
        rows(&db, "SELECT VALUE id FROM homework_submission").await,
        0,
        "the refused write must leave no row"
    );
    assert_eq!(
        counter(&db, &student_id, "homework_submitted_total").await,
        0,
        "... and no counter"
    );
    // The student's own route agrees, so the refusal is a 404 and not a 500.
    let over_http = send(
        &app,
        "POST",
        &format!("/homework/{hw}/submission"),
        Some(&student),
        Some(json!({ "text": "geç kaldım" })),
    )
    .await;
    assert_eq!(over_http.status, StatusCode::NOT_FOUND);
}

/// The grade's own twin of the gate above, and the reason `delete_course`'s
/// homework lease is no longer the only thing standing between a grade and an
/// orphan: `HomeworkResult::grade` writes a deterministic id too, so against a
/// homework a cascade already removed it used to succeed — a `homework_result`
/// row nothing can reach (`GET /homework/{id}/result` has no existence check of
/// its own) plus a `marks_given_total` on the grader that no ungrade can give
/// back, since the ungrade route 404s on the vanished homework first.
///
/// Domain-level and sequential on purpose: the guarantee under test is the one
/// that holds when the lease does not, so no handler may be in the way of it.
#[tokio::test]
async fn a_grade_under_a_vanished_homework_is_refused() {
    use hezarfen_backend::domain::homework::HomeworkId;
    use hezarfen_backend::domain::homework_result::{HomeworkResult, HomeworkStatus};
    use hezarfen_backend::domain::user::UserId;

    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ogretmen_not", "teacher").await;
    let teacher_id = me_id(&app, &teacher).await;
    let (course, subject, _student, student_id) =
        course_with_student(&app, &db, &teacher, "muzik").await;
    let hw = create_homework(&app, &teacher, &course, &subject, "solfej", far_future()).await;

    let dropped = send(
        &app,
        "DELETE",
        &format!("/homework/{hw}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(
        dropped.status,
        StatusCode::NO_CONTENT,
        "delete the homework"
    );

    // The ids a handler mid-request would still be carrying.
    let refused = HomeworkResult::grade(
        &HomeworkId::from_key(&hw),
        &UserId::from_key(&student_id),
        HomeworkStatus::try_new("done").unwrap(),
        None,
        &UserId::from_key(&teacher_id),
        &db,
    )
    .await;
    assert!(
        refused.is_err(),
        "a grade on a homework that is gone must be refused, got {refused:?}"
    );
    assert_eq!(
        rows(&db, "SELECT VALUE id FROM homework_result").await,
        0,
        "the refused write must leave no row"
    );
    assert_eq!(
        counter(&db, &teacher_id, "marks_given_total").await,
        0,
        "... and no credit on the grader"
    );
    // The teacher's own route agrees: a 404, never a 500.
    let over_http = send(
        &app,
        "POST",
        &format!("/homework/{hw}/results"),
        Some(&teacher),
        Some(json!({ "user": student_id, "status": "done" })),
    )
    .await;
    assert_eq!(over_http.status, StatusCode::NOT_FOUND);
}

/// `delete_course` took `EXAM_LOCK` and nothing else, while its cascade sweeps
/// the course's homework, submissions, files and results. Grading holds
/// `HOMEWORK_LOCK.write()` across a transaction that only *reads* the homework,
/// and the store conflict-checks write sets rather than read sets — so the
/// sweep ran on a snapshot without the grade and both committed: an orphan
/// `homework_result` under a vanished homework, readable ever after at
/// `GET /homework/{id}/result`, which has no existence check of its own.
///
/// The window is the grade's, not the delete's: a course delete is refused
/// while anyone is enrolled, so the roster has to empty *while* the grade is
/// mid-write — a `DEFINE EVENT` on `homework_result` holds it, gates already
/// passed, across the unenroll and the delete that follow. The grade's own
/// duration is asserted, because a grade that never reached its window would
/// leave this suite green over the very hole it exists for.
///
/// `marks_given_total` is deliberately *not* asserted at zero: no cascade ever
/// gives that counter back (the 08-05 sweep left that standing on purpose), so
/// a grade this test serializes *ahead* of the delete keeps its credit exactly
/// as an uncontested grade followed by a course delete does.
///
/// What this pins, honestly: **the outcome, not the lease**. With the lease it
/// passes because the grade is serialized ahead of the cascade and answered
/// `200`; with the lease removed it passes because
/// [`HomeworkResult::grade`]'s parent gate refuses the late write and answers
/// `404` (measured: 404 after a full 3s window). So removing *either* guard
/// alone leaves this green and only removing both turns it red — the same
/// belt-and-braces shape the upload race above has. The lease's own mutation
/// test died with the gate that replaced it; nothing else pins it, which is
/// why it is kept and documented rather than trusted to a red test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grade_inside_a_course_delete_never_orphans() {
    let _serial = ONE_RACE_AT_A_TIME.lock().await;
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ogretmen_ders", "teacher").await;
    let (course, subject, _student, student_id) =
        course_with_student(&app, &db, &teacher, "tarih").await;
    let hw = create_homework(&app, &teacher, &course, &subject, "ödev", far_future()).await;

    // Hold the grade's own write open, every gate already cleared. Three
    // seconds against a one-second wait below: under a loaded test binary the
    // spawned request can take a while to be polled at all, and the window has
    // to still be open when it gets there.
    db.query(
        "DEFINE EVENT hold_the_grade ON TABLE homework_result WHEN $event = 'CREATE' \
         THEN { SLEEP 3s; };",
    )
    .await
    .expect("define the window event")
    .check()
    .expect("check the window event");

    let grading = {
        let (app, teacher, hw, target) =
            (app.clone(), teacher.clone(), hw.clone(), student_id.clone());
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            let res = send(
                &app,
                "POST",
                &format!("/homework/{hw}/results"),
                Some(&teacher),
                Some(json!({ "user": target, "status": "done", "mark": 90 })),
            )
            .await;
            (res, started.elapsed())
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(1_000)).await;
    // The roster empties mid-grade — the only staging in which the course
    // delete is admitted at all.
    unenroll(&app, &teacher, &course, &student_id).await;
    let dropped = send(
        &app,
        "DELETE",
        &format!("/courses/{course}"),
        Some(&teacher),
        None,
    )
    .await;
    let (graded, took) = grading.await.unwrap();

    assert!(
        took >= std::time::Duration::from_secs(3),
        "the grade never reached its window ({took:?}, answered {}): nothing was raced",
        graded.status
    );
    assert_eq!(
        dropped.status,
        StatusCode::NO_CONTENT,
        "the course delete must succeed: {:?}",
        dropped.body
    );
    assert!(
        graded.status.is_success() || graded.status == StatusCode::NOT_FOUND,
        "a raced grade must be answered, not {}: {:?}",
        graded.status,
        graded.body
    );

    // Stored state is the whole verdict.
    assert_eq!(
        rows(&db, "SELECT VALUE id FROM homework").await,
        0,
        "the homework survived its course"
    );
    assert_eq!(
        rows(&db, "SELECT VALUE id FROM homework_result").await,
        0,
        "a grade outlived the homework it was written against"
    );
}

/// A subset assignment is the one place the homework routes hand a student a
/// list of other students. The course roster itself is teacher+-only, and
/// `get_homework` already 404s a student the subset does not name "so a subset
/// assignment never reveals itself" — but to the students it *does* name it
/// used to serialize the whole list. Deterministic, no race: the response body
/// is the defect.
#[tokio::test]
async fn a_student_sees_only_themselves_in_an_assigned_subset() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ogretmen_liste", "teacher").await;
    let course = create_course(&app, &teacher, "Biyoloji").await;
    let subject = create_subject(&app, &teacher, &course, "hücre").await;
    let ali = login_as(&app, &db, "ali", "student").await;
    let ali_id = me_id(&app, &ali).await;
    let veli = login_as(&app, &db, "veli", "student").await;
    let veli_id = me_id(&app, &veli).await;
    enroll(&app, &teacher, &course, &ali_id).await;
    enroll(&app, &teacher, &course, &veli_id).await;

    let res = create_homework_with(
        &app,
        &teacher,
        &course,
        json!({
            "title": "sunum",
            "subject_id": subject,
            "due_at": far_future(),
            "assigned": [ali_id.clone(), veli_id.clone()],
        }),
    )
    .await;
    assert_eq!(
        res.status,
        StatusCode::CREATED,
        "create the subset homework"
    );
    let hw = id_of(&res.body);

    let assigned = |body: &Value| -> Vec<String> {
        body["assigned"]
            .as_array()
            .expect("assigned is a list on a subset homework")
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    };

    // The single read.
    let mine = send(&app, "GET", &format!("/homework/{hw}"), Some(&ali), None).await;
    assert_eq!(mine.status, StatusCode::OK);
    assert_eq!(
        assigned(&mine.body),
        vec![ali_id.clone()],
        "a student may learn they are assigned, never who else is"
    );

    // ... the cross-course list ...
    let listed = send(&app, "GET", "/homework", Some(&ali), None).await;
    assert_eq!(listed.status, StatusCode::OK);
    assert_eq!(assigned(&items(&listed.body)[0]), vec![ali_id.clone()]);

    // ... and the per-course one.
    let in_course = send(
        &app,
        "GET",
        &format!("/courses/{course}/homework"),
        Some(&ali),
        None,
    )
    .await;
    assert_eq!(in_course.status, StatusCode::OK);
    assert_eq!(assigned(&items(&in_course.body)[0]), vec![ali_id.clone()]);

    // The teacher who runs the course still sees the roster whole — narrowing
    // it for them would break the audience they just set.
    let theirs = send(
        &app,
        "GET",
        &format!("/homework/{hw}"),
        Some(&teacher),
        None,
    )
    .await;
    assert_eq!(theirs.status, StatusCode::OK);
    let mut named = assigned(&theirs.body);
    named.sort();
    let mut both = vec![ali_id, veli_id];
    both.sort();
    assert_eq!(named, both, "the assigning teacher keeps the whole subset");
}

/// A file add and a file delete on one submission write the same row — the
/// upload claims its seat on it, the delete re-stamps it — and both handlers
/// hold only `HOMEWORK_LOCK.read()`, so they contend by design. The delete sent
/// its cascade through a plain `db.query`, unlike every sibling in the module,
/// so a lost round came back as a 500 on a request that had written nothing.
///
/// The window is the *delete's* own — the retry-less side has to be the one
/// holding a stale write when the other commits, or nothing it does can be
/// answered "conflict, retry": a `DEFINE EVENT` on `homework_file` holds the
/// delete's transaction open after it has re-stamped the submission row, and an
/// upload lands on that same row inside it. Held short (150ms): the retry
/// budget is 8 tries over ~250ms of backoff, so a window wider than the budget
/// would fail a *correct* implementation too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_add_racing_a_file_delete_never_500s() {
    let _serial = ONE_RACE_AT_A_TIME.lock().await;
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "ogretmen_dosya", "teacher").await;
    let (course, subject, student, _student_id) =
        course_with_student(&app, &db, &teacher, "resim").await;
    let hw = create_homework(&app, &teacher, &course, &subject, "çizim", far_future()).await;
    assert_eq!(
        upload_hw_file(&app, &student, &hw).await,
        StatusCode::CREATED,
        "the first file"
    );
    let listed = send(
        &app,
        "GET",
        &format!("/homework/{hw}/submission"),
        Some(&student),
        None,
    )
    .await;
    let first = listed.body["files"][0]["id"].as_str().unwrap().to_string();

    db.query(
        "DEFINE EVENT hold_the_delete ON TABLE homework_file WHEN $event = 'DELETE' \
         THEN { SLEEP 150ms; };",
    )
    .await
    .expect("define the window event")
    .check()
    .expect("check the window event");

    let removing = {
        let (app, student, hw, first) = (app.clone(), student.clone(), hw.clone(), first.clone());
        tokio::spawn(async move {
            send(
                &app,
                "DELETE",
                &format!("/homework/{hw}/submission/files/{first}"),
                Some(&student),
                None,
            )
            .await
        })
    };
    // The upload commits on the submission row while the delete is still
    // holding its own write of it.
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    let added = upload_hw_file(&app, &student, &hw).await;
    let removed = removing.await.unwrap();

    assert_ne!(
        removed.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "a lost round is a re-send, not a 500: {:?}",
        removed.body
    );
    assert_ne!(
        added,
        StatusCode::INTERNAL_SERVER_ERROR,
        "... on either side"
    );
    // Whatever the order, the stored count matches the stored rows: the
    // submission's seat counter is what the cap reads next time.
    let files = rows(&db, "SELECT VALUE id FROM homework_file").await;
    let mut result = db
        .query("SELECT VALUE (file_count ?? 0) FROM homework_submission")
        .await
        .unwrap()
        .check()
        .unwrap();
    let counted: Vec<i64> = result.take(0).unwrap();
    assert_eq!(
        counted.first().copied().unwrap_or(0),
        files as i64,
        "the seat counter drifted from the rows"
    );
}
