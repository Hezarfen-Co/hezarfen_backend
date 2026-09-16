//! An exam delete must take every child with it — including the one child the
//! store cannot refuse on its own.
//!
//! Every other child of an exam (answers, drawings, questions, pictures) writes
//! the exam row inside its own transaction, so a delete racing one of them
//! contends on a single key and the store aborts a side. An attempt cannot do
//! that: its create rides `cap::claim_and_create` against the *student's* row
//! (`exam_sat_total`), which this delete never touches, so nothing collides and
//! the freeze/existence gate is a plain read. The start path now takes
//! `FOR UPDATE` on the exam row from the exam read through the insert, so a
//! delete waits behind an in-flight start and a start after the delete finds
//! no row. The old hole was that `start_attempt` held a process
//! `EXAM_LOCK.write()` while `delete_exam` held nothing.
//!
//! This suite drives the real handlers: the gates live on the HTTP path as
//! well as the store.

mod common;

use axum::http::StatusCode;
use common::{
    add_member, app_and_db, attach_instance, create_class, create_course, create_exam,
    create_exam_with, create_term, enroll, ensure_year, id_of, items, login_as, me_id, send,
    taught, taught_under, total,
};
use hezarfen_backend::db::exam_attempt::list_for_exam;
use hezarfen_backend::domain::exam::ExamId;
use serde_json::json;

/// The delete is held open for a second *after* the exam row is gone but
/// before its cascade runs — an `AFTER DELETE` trigger on the table sleeps
/// inside the delete's own transaction, so the window is opened by the database
/// rather than by a lucky interleaving. A start firing into that window used to
/// read an exam that was still there (deleted, uncommitted), create its
/// sitting, and commit past a sweep that had already run on a snapshot without
/// it. That row was unreachable afterwards — every route to an attempt goes
/// through its exam — while the student's lifetime sitting counter stayed up
/// for good, and a badge minted off it is never revoked.
///
/// The window needs no second writer to be real: the exam row lock is what the
/// start and the sweep contend on, and it behaves the same whoever wins. The
/// pairing is mutation-tested — dropping the row lock from `delete_exam` turns
/// red.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attempt_started_inside_a_delete_never_outlives_the_exam() {
    let (app, db) = app_and_db().await;
    let mudur = login_as(&app, &db, "mudur_sil", "manager").await;
    let teacher = login_as(&app, &db, "ogretmen_sil", "teacher").await;
    let student = login_as(&app, &db, "ogrenci_sil", "student").await;
    let student_id = me_id(&app, &student).await;

    // Hold the delete open for a full second once the row is gone, while its
    // cascade still has to run: an AFTER DELETE trigger sleeping inside the
    // delete's own transaction is the Postgres shape of the old window event.
    let mut conn = db.acquire().await.expect("acquire for the trigger");
    sqlx::raw_sql(
        "CREATE FUNCTION heztest_hold_delete() RETURNS trigger AS $$
         BEGIN PERFORM pg_sleep(1.0); RETURN NULL; END;
         $$ LANGUAGE plpgsql;
         CREATE TRIGGER heztest_hold_delete AFTER DELETE ON exam
         FOR EACH ROW EXECUTE FUNCTION heztest_hold_delete();",
    )
    .execute(&mut *conn)
    .await
    .expect("define the window trigger");

    let mut sittings = 0;
    for round in 0..3 {
        // The exam's parents are the instance and its dönem now, so the round
        // mints the whole stack; the plain teacher stays the şube's homeroom
        // one, which is what lets their cookie act on the instance.
        let t = taught_under(&app, &mudur, &teacher, &format!("Fizik {round}")).await;
        enroll(&app, &teacher, &t.instance, &student_id).await;
        // `open` mode, so the exam is actually sittable: an unscheduled one
        // answers the start with a 409 before it ever writes, which would make
        // this test pass on a missing lease.
        let res = create_exam_with(
            &app,
            &teacher,
            &t.instance,
            json!({ "title": "Vize", "kind": "yazili", "mode": "open", "term": t.term }),
        )
        .await;
        assert_eq!(
            res.status,
            StatusCode::CREATED,
            "round {round}: create exam"
        );
        let exam = id_of(&res.body);

        let drop_it = {
            let (app, teacher, exam) = (app.clone(), teacher.clone(), exam.clone());
            tokio::spawn(async move {
                send(
                    &app,
                    "DELETE",
                    &format!("/exams/{exam}"),
                    Some(&teacher),
                    None,
                )
                .await
            })
        };
        // The start fires inside the held window: without the lease it reads an
        // exam row the delete has removed but not committed.
        // race-window staging — do not convert to poll
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let sit = {
            let (app, student, exam) = (app.clone(), student.clone(), exam.clone());
            tokio::spawn(async move {
                send(
                    &app,
                    "POST",
                    &format!("/exams/{exam}/attempt"),
                    Some(&student),
                    None,
                )
                .await
            })
        };
        let (dropped, sat) = (drop_it.await.unwrap(), sit.await.unwrap());
        assert_eq!(
            dropped.status,
            StatusCode::NO_CONTENT,
            "round {round}: the delete must succeed"
        );
        // A 404 for the start is the correct answer; the only defect is stored
        // state. Nothing may 500 either way.
        assert!(
            sat.status == StatusCode::NOT_FOUND || sat.status.is_success(),
            "round {round}: a raced start must be answered, not {}: {:?}",
            sat.status,
            sat.body
        );

        // Stored state is the whole verdict; a response code is not evidence.
        let id = ExamId::from_key(&exam);
        sittings += list_for_exam(&db, &id).await.unwrap().len();
    }
    assert_eq!(sittings, 0, "a sitting outlived its exam");
}

/// An archived academic year freezes the exams hanging off its şubeler'
/// instances: every write route under `/exams/{id}` (the lists below) answers
/// `409 academic_year_archived` — authoring, grading, the images on both sides,
/// the ortak-sınav audience pair, and the sitting itself — while every read
/// stays open. Past years are a read-only archive, not a hidden one.
///
/// The sitting is deliberately *live* when the archive lands: a student mid-exam
/// is the case where a freeze can do real damage, and every one of their write
/// routes has to refuse with the coded 409 rather than a 500 or a silent write.
/// The guards sit after each handler's authz, so a caller who may not write
/// still gets its `403` (asserted below) — the archive never leaks exam
/// structure.
///
/// Not covered here: the exam-room WebSocket. Its door (`GET
/// /exams/{id}/attempt/ws`) and its `finish` frame carry their own guards, but
/// this binary drives the router through `oneshot` and cannot upgrade — the
/// socket needs the real-TCP harness in `tests/e2e.rs`. The REST
/// `POST /exams/{id}/attempt/answers` below does cover the shared
/// `save_answer_in` funnel that every WS `answer` frame also goes through.
#[tokio::test]
async fn an_archived_years_exams_take_no_writes_but_still_read() {
    let (app, db) = app_and_db().await;
    let manager = login_as(&app, &db, "arsiv_sinav_mudur", "manager").await;
    let student = login_as(&app, &db, "arsiv_sinav_ogrenci", "student").await;
    let student_id = me_id(&app, &student).await;

    // The freeze is the *year's*: every exam write reads its instance's year
    // (`class_course::require_open` → şube → year), so the fixture's şube has to
    // sit in the year that gets archived.
    let t = taught(&app, &manager, "Tarih").await;
    enroll(&app, &manager, &t.instance, &student_id).await;
    let subject = common::create_subject(&app, &manager, &t.course, "Kronoloji").await;

    // `open` mode so the sitting is real: the attempt below has to be running
    // when the archive lands.
    let res = create_exam_with(
        &app,
        &manager,
        &t.instance,
        json!({ "title": "Vize", "kind": "yazili", "mode": "open", "term": t.term.clone() }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let exam = id_of(&res.body);

    let sibling_class =
        create_class(&app, &manager, "Tarih 2", json!({ "year": t.year.clone() })).await;
    let sibling = attach_instance(&app, &manager, &sibling_class, &t.course).await;
    let announced = send(
        &app,
        "POST",
        &format!("/exams/{exam}/audience"),
        Some(&manager),
        Some(json!({ "instance": sibling })),
    )
    .await;
    assert_eq!(
        announced.status,
        StatusCode::OK,
        "the announcement predating the archive: {}",
        announced.body
    );

    // One choice question — it supplies the qid every question route needs and
    // the choice id the option-picture routes need.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions"),
        Some(&manager),
        Some(json!({ "subject_id": subject, "text": "Ne zaman?", "kind": "choice", "points": 10,
                     "choices": [{"id": "c0", "text": "1453"}, {"id": "c1", "text": "1071"}], "correct": "c0" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let question = id_of(&res.body);
    let choice = res.body["choices"][0]["id"]
        .as_str()
        .expect("choice id")
        .to_string();

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);

    // The year goes past. No route archives one — a year is frozen when the
    // office declares it done — so the stored state is written straight into
    // the store, the way the role bootstrap and the sibling suites do it.
    let year = uuid::Uuid::parse_str(&t.year).unwrap();
    let archived = sqlx::query("UPDATE academic_year SET archived_at = 1 WHERE id = $1")
        .bind(year)
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(
        archived.rows_affected(),
        1,
        "the fixture's year is archived"
    );

    // The JSON write routes: exam, results, the question authoring set, and
    // the ortak-sınav audience pair (its announcement is re-sent — a no-op
    // against an open year, which the archive must still refuse). `from-bank`
    // takes an id that need not exist — the freeze is judged before the bank
    // lookup, and a 404 here would be the bug.
    let staff_writes: Vec<(&str, String, Option<serde_json::Value>)> = vec![
        (
            "PATCH",
            format!("/exams/{exam}"),
            Some(json!({ "title": "Final" })),
        ),
        ("DELETE", format!("/exams/{exam}"), None),
        (
            "POST",
            format!("/exams/{exam}/audience"),
            Some(json!({ "instance": sibling })),
        ),
        ("DELETE", format!("/exams/{exam}/audience/{sibling}"), None),
        (
            "POST",
            format!("/exams/{exam}/results"),
            Some(json!({ "user_id": student_id, "mark": 80 })),
        ),
        (
            "DELETE",
            format!("/exams/{exam}/results/{student_id}"),
            None,
        ),
        (
            "POST",
            format!("/exams/{exam}/questions"),
            Some(json!({ "subject_id": subject, "text": "Kim?", "kind": "text", "points": 5 })),
        ),
        (
            "PATCH",
            format!("/exams/{exam}/questions/{question}"),
            Some(json!({ "text": "Nerede?" })),
        ),
        (
            "DELETE",
            format!("/exams/{exam}/questions/{question}"),
            None,
        ),
        (
            "POST",
            format!("/exams/{exam}/questions/from-bank/yok"),
            Some(json!({ "subject_id": subject })),
        ),
        (
            "POST",
            format!("/exams/{exam}/questions/{question}/refresh-from-bank"),
            None,
        ),
        (
            "POST",
            format!("/exams/{exam}/questions/{question}/to-bank"),
            None,
        ),
        (
            "DELETE",
            format!("/exams/{exam}/questions/{question}/image"),
            None,
        ),
        (
            "DELETE",
            format!("/exams/{exam}/questions/{question}/choices/{choice}/image"),
            None,
        ),
    ];
    for (method, uri, body) in &staff_writes {
        let res = send(&app, method, uri, Some(&manager), body.clone()).await;
        assert_eq!(
            (res.status, res.body["code"].clone()),
            (StatusCode::CONFLICT, json!("academic_year_archived")),
            "{method} {uri} must be frozen by the archive: {}",
            res.body
        );
    }

    // The student's own writes — the live sitting. `save_answer` is the REST
    // face of `save_answer_in`, the funnel the WebSocket's `answer` shares.
    let sit_writes: Vec<(&str, String, Option<serde_json::Value>)> = vec![
        ("POST", format!("/exams/{exam}/attempt"), None),
        (
            "POST",
            format!("/exams/{exam}/attempt/answers"),
            Some(json!({ "question_id": question, "selected": choice })),
        ),
        (
            "DELETE",
            format!("/exams/{exam}/attempt/answers/{question}/image"),
            None,
        ),
        ("POST", format!("/exams/{exam}/attempt/finish"), None),
    ];
    for (method, uri, body) in &sit_writes {
        let res = send(&app, method, uri, Some(&student), body.clone()).await;
        assert_eq!(
            (res.status, res.body["code"].clone()),
            (StatusCode::CONFLICT, json!("academic_year_archived")),
            "{method} {uri} must be frozen by the archive: {}",
            res.body
        );
    }

    // The three multipart uploads. The guard runs *before* the body is read, so
    // these never touch the blob store.
    let uploads = [
        (
            format!("/exams/{exam}/questions/{question}/image"),
            manager.clone(),
        ),
        (
            format!("/exams/{exam}/questions/{question}/choices/{choice}/image"),
            manager.clone(),
        ),
        (
            format!("/exams/{exam}/attempt/answers/{question}/image"),
            student.clone(),
        ),
    ];
    for (uri, cookie) in &uploads {
        let res = common::upload_file_at(&app, cookie, uri, "a.png", "image/png", b"png").await;
        assert_eq!(
            (res.status, res.body["code"].clone()),
            (StatusCode::CONFLICT, json!("academic_year_archived")),
            "POST {uri} must be frozen by the archive: {}",
            res.body
        );
    }

    // Reads stay open on both sides of the desk.
    for (uri, cookie) in [
        (format!("/exams/{exam}"), &manager),
        (format!("/exams/{exam}/questions"), &manager),
        (format!("/exams/{exam}/results"), &manager),
        (
            format!("/exams/{exam}/attempts/{student_id}/answers"),
            &manager,
        ),
        (format!("/exams/{exam}/attempt"), &student),
        (format!("/exams/{exam}/attempt/questions"), &student),
    ] {
        let res = send(&app, "GET", &uri, Some(cookie), None).await;
        assert_eq!(
            res.status,
            StatusCode::OK,
            "GET {uri} must stay readable: {}",
            res.body
        );
    }

    // A caller who may not write is still told *that* first: the 403 precedes
    // the 409, or the archive leaks the exam's structure to outsiders.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&student),
        Some(json!({ "title": "Sızıntı" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);

    // Re-opening the year thaws them again.
    let reopened = sqlx::query("UPDATE academic_year SET archived_at = NULL WHERE id = $1")
        .bind(year)
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(reopened.rows_affected(), 1, "the year is open again");
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&manager),
        Some(json!({ "title": "Final" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
}

/// One instance's line out of a `?term=` karne report.
fn karne_line<'a>(body: &'a serde_json::Value, instance: &str) -> &'a serde_json::Value {
    body["instances"]
        .as_array()
        .unwrap_or_else(|| panic!("a karne report with an instances array: {body}"))
        .iter()
        .find(|line| line["class_course"] == instance)
        .unwrap_or_else(|| panic!("a karne line for {instance}: {body}"))
}

/// The ortak sınav round trip over HTTP: 5-A and 5-B teach the same course in
/// the same year (two instances), an exam written on 5-A is announced to 5-B,
/// and the **same mark** then stands in 5-B's karne — the per-instance read
/// joins `exam_audience`, so the announcement is what attributes it there.
/// 5-B's own exam list carries it while the announcement stands; withdrawal
/// takes both back out. The student is enrolled by hand on 5-A's instance too
/// (an operator may place anyone), which is what makes one mark readable from
/// both sides without a second grade.
#[tokio::test]
async fn an_ortak_sinav_lands_in_the_targets_karne_until_withdrawn() {
    let (app, db) = app_and_db().await;
    let mudur = login_as(&app, &db, "mudur_ortak", "manager").await;
    let teacher = login_as(&app, &db, "ogretmen_ortak", "teacher").await;
    let student = login_as(&app, &db, "ogrenci_ortak_b", "student").await;
    let student_id = me_id(&app, &student).await;
    let teacher_id = me_id(&app, &teacher).await;

    let year = ensure_year(&app, &mudur).await;
    let term = create_term(&app, &mudur, &year, "1. Dönem").await;
    let course = create_course(&app, &teacher, "Matematik").await;
    let a_class = create_class(
        &app,
        &mudur,
        "5-A",
        json!({ "year": year, "teacher_id": teacher_id }),
    )
    .await;
    let b_class = create_class(&app, &mudur, "5-B", json!({ "year": year })).await;
    let a_inst = attach_instance(&app, &mudur, &a_class, &course).await;
    let b_inst = attach_instance(&app, &mudur, &b_class, &course).await;

    // 5-B's roster carries the student; the hand enrollment on 5-A's instance
    // is the operator's row the grader's enrollment gate reads.
    add_member(&app, &mudur, &b_class, &student_id).await;
    enroll(&app, &mudur, &a_inst, &student_id).await;

    let exam = create_exam(&app, &teacher, &a_inst, &term, "Ortak Yazılı", "yazili").await;
    let graded = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&mudur),
        Some(json!({ "user_id": student_id, "mark": 85 })),
    )
    .await;
    assert_eq!(graded.status, StatusCode::OK, "grade: {}", graded.body);

    // A karne line exists per instance the student's şubeler carry — 5-B's is
    // the only one here — and before the announcement the mark is not
    // attributed to it: nothing addresses the exam to 5-B's instance.
    let before = send(
        &app,
        "GET",
        &format!("/marks/karne?term={term}"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(before.status, StatusCode::OK, "karne: {}", before.body);
    assert_eq!(
        karne_line(&before.body, &b_inst)["average"],
        serde_json::Value::Null,
        "5-B's line carries nothing before the announcement: {}",
        before.body
    );

    // Announce. The answer is the audience, owner first.
    let announced = send(
        &app,
        "POST",
        &format!("/exams/{exam}/audience"),
        Some(&mudur),
        Some(json!({ "instance": b_inst })),
    )
    .await;
    assert_eq!(
        announced.status,
        StatusCode::OK,
        "announce: {}",
        announced.body
    );
    assert_eq!(
        announced.body.as_array().map(Vec::len),
        Some(2),
        "{}",
        announced.body
    );
    assert_eq!(announced.body[0]["instance"], json!(a_inst));
    assert_eq!(announced.body[1]["instance"], json!(b_inst));
    assert_eq!(announced.body[1]["class"], json!(b_class));
    assert_eq!(announced.body[1]["course"], json!(course));

    // 5-B's own list carries the exam now, and the mark is attributed to its
    // line — the whole point of the announcement.
    let listed = send(
        &app,
        "GET",
        &format!("/instances/{b_inst}/exams"),
        Some(&mudur),
        None,
    )
    .await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.body);
    assert_eq!(
        total(&listed.body),
        1,
        "5-B carries the exam: {}",
        listed.body
    );
    assert_eq!(id_of(&items(&listed.body)[0]), exam);

    let during = send(
        &app,
        "GET",
        &format!("/marks/karne?term={term}"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(during.status, StatusCode::OK, "karne: {}", during.body);
    assert_eq!(
        karne_line(&during.body, &b_inst)["average"],
        json!(85.0),
        "the announcement puts the mark on 5-B's line: {}",
        during.body
    );

    // Withdrawal: the audience is back to the owner, 5-B drops the exam, and
    // the mark leaves its line — the announcement was the only attribution.
    let withdrawn = send(
        &app,
        "DELETE",
        &format!("/exams/{exam}/audience/{b_inst}"),
        Some(&mudur),
        None,
    )
    .await;
    assert_eq!(
        withdrawn.status,
        StatusCode::OK,
        "withdraw: {}",
        withdrawn.body
    );
    assert_eq!(
        withdrawn.body.as_array().map(Vec::len),
        Some(1),
        "{}",
        withdrawn.body
    );
    assert_eq!(withdrawn.body[0]["instance"], json!(a_inst));

    let relay = send(
        &app,
        "GET",
        &format!("/instances/{b_inst}/exams"),
        Some(&mudur),
        None,
    )
    .await;
    assert_eq!(total(&relay.body), 0, "5-B dropped it: {}", relay.body);
    let after = send(
        &app,
        "GET",
        &format!("/marks/karne?term={term}"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(
        karne_line(&after.body, &b_inst)["average"],
        serde_json::Value::Null,
        "the withdrawal takes the mark off 5-B's line: {}",
        after.body
    );
}

/// Detaching an instance an exam is announced *to* (one it does not own) is
/// not a foreign-key failure: the audience row naming it is swept with the
/// instance, and the exam — owned by a sibling — stands with its owner's row.
#[tokio::test]
async fn detaching_an_audience_instance_sweeps_its_row() {
    let (app, db) = app_and_db().await;
    let mudur = login_as(&app, &db, "mudur_ortak_detach_a", "manager").await;
    let teacher = login_as(&app, &db, "ogretmen_ortak_detach_a", "teacher").await;

    let year = ensure_year(&app, &mudur).await;
    let term = create_term(&app, &mudur, &year, "1. Dönem").await;
    let course = create_course(&app, &teacher, "Fizik").await;
    let a_class = create_class(&app, &mudur, "6-A", json!({ "year": year })).await;
    let b_class = create_class(&app, &mudur, "6-B", json!({ "year": year })).await;
    let a_inst = attach_instance(&app, &mudur, &a_class, &course).await;
    let b_inst = attach_instance(&app, &mudur, &b_class, &course).await;
    let exam = create_exam(&app, &mudur, &a_inst, &term, "Ortak Vize", "yazili").await;
    let announced = send(
        &app,
        "POST",
        &format!("/exams/{exam}/audience"),
        Some(&mudur),
        Some(json!({ "instance": b_inst })),
    )
    .await;
    assert_eq!(announced.status, StatusCode::OK, "{}", announced.body);

    let detached = send(
        &app,
        "DELETE",
        &format!("/classes/{b_class}/instances/{b_inst}"),
        Some(&mudur),
        None,
    )
    .await;
    assert_eq!(
        detached.status,
        StatusCode::NO_CONTENT,
        "detaching an announced-to instance must not 500: {}",
        detached.body
    );

    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM exam_audience WHERE class_course = $1")
            .bind(sqlx::types::Uuid::parse_str(&b_inst).unwrap())
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(rows, 0, "the row naming the detached instance is gone");
    // The exam is a sibling's: it stands, addressed to its owner alone.
    let read = send(&app, "GET", &format!("/exams/{exam}"), Some(&mudur), None).await;
    assert_eq!(
        read.status,
        StatusCode::OK,
        "the exam stands: {}",
        read.body
    );
    let audience = send(
        &app,
        "GET",
        &format!("/exams/{exam}/audience"),
        Some(&mudur),
        None,
    )
    .await;
    assert_eq!(
        audience.body.as_array().map(Vec::len),
        Some(1),
        "{}",
        audience.body
    );
    assert_eq!(audience.body[0]["instance"], json!(a_inst));
}

/// Detaching the instance that **owns** an exam which is announced to a
/// sibling: the audience rows of the exams being swept go with them, wherever
/// they point. Without that sweep the `exam` delete trips
/// `exam_audience_exam_fkey` (`NO ACTION`) and the detach is a `500` — the
/// probe is mutation-tested, dropping `exam = ANY(...)` from the sweep's
/// delete turns this red.
#[tokio::test]
async fn detaching_the_owner_instance_sweeps_its_exams_audiences() {
    let (app, db) = app_and_db().await;
    let mudur = login_as(&app, &db, "mudur_ortak_detach_b", "manager").await;
    let teacher = login_as(&app, &db, "ogretmen_ortak_detach_b", "teacher").await;

    let year = ensure_year(&app, &mudur).await;
    let term = create_term(&app, &mudur, &year, "1. Dönem").await;
    let course = create_course(&app, &teacher, "Kimya").await;
    let a_class = create_class(&app, &mudur, "7-A", json!({ "year": year })).await;
    let b_class = create_class(&app, &mudur, "7-B", json!({ "year": year })).await;
    let a_inst = attach_instance(&app, &mudur, &a_class, &course).await;
    let b_inst = attach_instance(&app, &mudur, &b_class, &course).await;
    let exam = create_exam(&app, &mudur, &a_inst, &term, "Ortak Final", "yazili").await;
    let announced = send(
        &app,
        "POST",
        &format!("/exams/{exam}/audience"),
        Some(&mudur),
        Some(json!({ "instance": b_inst })),
    )
    .await;
    assert_eq!(announced.status, StatusCode::OK, "{}", announced.body);

    let detached = send(
        &app,
        "DELETE",
        &format!("/classes/{a_class}/instances/{a_inst}"),
        Some(&mudur),
        None,
    )
    .await;
    assert_eq!(
        detached.status,
        StatusCode::NO_CONTENT,
        "detaching the owner must not 500: {}",
        detached.body
    );

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM exam_audience WHERE exam = $1")
        .bind(sqlx::types::Uuid::parse_str(&exam).unwrap())
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(rows, 0, "the swept exam left no audience behind");
    // The sibling no longer carries it.
    let listed = send(
        &app,
        "GET",
        &format!("/instances/{b_inst}/exams"),
        Some(&mudur),
        None,
    )
    .await;
    assert_eq!(total(&listed.body), 0, "{}", listed.body);
    // And 7-B itself is untouched.
    let read = send(
        &app,
        "GET",
        &format!("/instances/{b_inst}"),
        Some(&mudur),
        None,
    )
    .await;
    assert_eq!(
        read.status,
        StatusCode::OK,
        "7-B's instance stands: {}",
        read.body
    );
}

/// The cutover: a student enrolled **only** in an instance the exam is
/// announced to can see the exam, sit it, and be graded on it — the
/// announcement is what admits them (`exam_audience`), not a roster row on the
/// owner's instance. Both read and write doors were owner-only before this;
/// an unrelated student stays refused on every one of them.
#[tokio::test]
async fn an_announced_student_can_see_sit_and_be_graded() {
    let (app, db) = app_and_db().await;
    let mudur = login_as(&app, &db, "mudur_ortak_sit", "manager").await;
    let teacher = login_as(&app, &db, "ogretmen_ortak_sit", "teacher").await;
    let b_teacher = login_as(&app, &db, "ogretmen_ortak_sit_b", "teacher").await;
    let x_teacher = login_as(&app, &db, "ogretmen_ortak_sit_x", "teacher").await;
    let b_student = login_as(&app, &db, "ogrenci_ortak_sit_b", "student").await;
    let outsider = login_as(&app, &db, "ogrenci_ortak_sit_x", "student").await;
    let b_student_id = me_id(&app, &b_student).await;
    let outsider_id = me_id(&app, &outsider).await;
    let teacher_id = me_id(&app, &teacher).await;
    let b_teacher_id = me_id(&app, &b_teacher).await;

    let year = ensure_year(&app, &mudur).await;
    let term = create_term(&app, &mudur, &year, "1. Dönem").await;
    let course = create_course(&app, &teacher, "Biyoloji").await;
    let a_class = create_class(
        &app,
        &mudur,
        "10-A",
        json!({ "year": year, "teacher_id": teacher_id }),
    )
    .await;
    let b_class = create_class(&app, &mudur, "10-B", json!({ "year": year })).await;
    let a_inst = attach_instance(&app, &mudur, &a_class, &course).await;
    let b_inst = attach_instance(&app, &mudur, &b_class, &course).await;
    // Only 10-B's roster carries the student; nobody is enrolled on 10-A's
    // instance, where the exam is written.
    add_member(&app, &mudur, &b_class, &b_student_id).await;

    let created = create_exam_with(
        &app,
        &teacher,
        &a_inst,
        json!({ "title": "Ortak Sınav", "kind": "yazili", "mode": "open", "term": term }),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let exam = id_of(&created.body);

    // Before the announcement the 10-B student is nobody to this exam.
    let pre_read = send(
        &app,
        "GET",
        &format!("/exams/{exam}"),
        Some(&b_student),
        None,
    )
    .await;
    assert_eq!(pre_read.status, StatusCode::FORBIDDEN, "{}", pre_read.body);
    let pre_sit = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&b_student),
        None,
    )
    .await;
    assert_eq!(pre_sit.status, StatusCode::FORBIDDEN, "{}", pre_sit.body);

    let announced = send(
        &app,
        "POST",
        &format!("/exams/{exam}/audience"),
        Some(&mudur),
        Some(json!({ "instance": b_inst })),
    )
    .await;
    assert_eq!(announced.status, StatusCode::OK, "{}", announced.body);

    // The announced section's own teacher now runs the exam's teacher side:
    // grading its students, reading the results and statistics, and watching
    // the monitor.
    let assigned = send(
        &app,
        "POST",
        &format!("/instances/{b_inst}/teachers"),
        Some(&mudur),
        Some(json!({ "user_id": b_teacher_id })),
    )
    .await;
    assert_eq!(
        assigned.status,
        StatusCode::OK,
        "assign 10-B's teacher: {}",
        assigned.body
    );

    // (a) The read door opens for a student of an addressed instance.
    let read = send(
        &app,
        "GET",
        &format!("/exams/{exam}"),
        Some(&b_student),
        None,
    )
    .await;
    assert_eq!(
        read.status,
        StatusCode::OK,
        "the announcement admits them: {}",
        read.body
    );

    // (b) The sitting door opens too.
    let sat = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&b_student),
        None,
    )
    .await;
    assert_eq!(sat.status, StatusCode::CREATED, "sitting: {}", sat.body);

    // …and the teacher's monitor sees them: the roster is the exam's whole
    // audience, so an announced-to section's sitter is not a blind spot.
    let live = send(
        &app,
        "GET",
        &format!("/exams/{exam}/live"),
        Some(&mudur),
        None,
    )
    .await;
    assert_eq!(live.status, StatusCode::OK, "monitor: {}", live.body);
    let names: Vec<&str> = live.body["students"]
        .as_array()
        .unwrap_or_else(|| panic!("a live snapshot with students: {}", live.body))
        .iter()
        .filter_map(|row| row["user"]["id"].as_str())
        .collect();
    assert!(
        names.contains(&b_student_id.as_str()),
        "the monitor lists the announced-to section's sitter ({}): {}",
        b_student_id,
        live.body
    );

    // (c) And the mark can be recorded — by the **announced section's own
    // teacher** — standing on their own instance's line.
    let graded = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&b_teacher),
        Some(json!({ "user_id": b_student_id, "mark": 90 })),
    )
    .await;
    assert_eq!(graded.status, StatusCode::OK, "grading: {}", graded.body);
    let karne = send(
        &app,
        "GET",
        &format!("/marks/karne?term={term}"),
        Some(&b_student),
        None,
    )
    .await;
    assert_eq!(
        karne_line(&karne.body, &b_inst)["average"],
        json!(90.0),
        "the mark stands on 10-B's line: {}",
        karne.body
    );

    // The results readers and the monitor, from that teacher's cookie too.
    for (uri, what) in [
        (format!("/exams/{exam}/results"), "results"),
        (format!("/exams/{exam}/statistics"), "statistics"),
        (format!("/exams/{exam}/live"), "the monitor"),
        (format!("/exams/{exam}/audience"), "the audience"),
    ] {
        let res = send(&app, "GET", &uri, Some(&b_teacher), None).await;
        assert_eq!(
            res.status,
            StatusCode::OK,
            "10-B's teacher reads {what}: {}",
            res.body
        );
    }

    // An unrelated teacher manages no addressed instance: refused at every
    // teacher-side door, grading included.
    for (method, uri, body, what) in [
        (
            "POST",
            format!("/exams/{exam}/results"),
            Some(json!({ "user_id": b_student_id, "mark": 60 })),
            "grading",
        ),
        ("GET", format!("/exams/{exam}/results"), None, "results"),
        (
            "GET",
            format!("/exams/{exam}/statistics"),
            None,
            "statistics",
        ),
        ("GET", format!("/exams/{exam}/live"), None, "the monitor"),
    ] {
        let res = send(&app, method, &uri, Some(&x_teacher), body).await;
        assert_eq!(
            res.status,
            StatusCode::FORBIDDEN,
            "an unrelated teacher on {what}: {}",
            res.body
        );
    }

    // An unrelated student is refused at every door.
    let f_read = send(
        &app,
        "GET",
        &format!("/exams/{exam}"),
        Some(&outsider),
        None,
    )
    .await;
    assert_eq!(f_read.status, StatusCode::FORBIDDEN, "{}", f_read.body);
    let f_sit = send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt"),
        Some(&outsider),
        None,
    )
    .await;
    assert_eq!(f_sit.status, StatusCode::FORBIDDEN, "{}", f_sit.body);
    let f_grade = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&mudur),
        Some(json!({ "user_id": outsider_id, "mark": 50 })),
    )
    .await;
    assert_eq!(
        f_grade.status,
        StatusCode::BAD_REQUEST,
        "grading a student outside every addressed instance: {}",
        f_grade.body
    );
}
