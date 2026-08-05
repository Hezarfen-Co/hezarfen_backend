//! Regression for the self-review answer-key leak found in the 2026-08-02
//! sweep: the review gate proved "the caller has a mark" by reading their
//! *latest* mark, so a mark left at seq 1 satisfied it forever. A student who
//! started a retake could read the answer key — and their own `is_correct` /
//! `auto_score` flags — on the sheet they were still writing.
//!
//! The gate now refuses (409) while the caller can still *write* a sitting —
//! one in progress, or one they may still start under `max_attempts` while the
//! exam is open. Refusing only the in-progress case still leaked: a finished
//! seq 1 handed out the key one `POST /attempt` before it was used.
//! Deterministic: every sitting is started over HTTP, no clock or race involved.
//!
//! And its cross-exam twin: the question bank copies `correct` into every exam
//! a template is instantiated into, so a graded exam's review used to hand out
//! the key to the copy the caller was still answering in *another* exam. Those
//! questions now come back keyless — keyed on the template, so the rest of the
//! reviewed exam is untouched.

mod common;

use axum::http::StatusCode;
use common::{
    app_and_db, create_course, create_exam_with, create_subject, enroll, id_of, login, login_as,
    me_id, send,
};
use serde_json::json;

/// A bank template instantiated into two exams copies its `correct` into both.
/// Reviewing the graded one must not hand out the key to the copy the caller is
/// still answering in the other — but only that question blanks out.
#[tokio::test]
async fn review_hides_the_key_of_a_bank_question_live_under_another_sitting() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "rev_bank_t", "teacher").await;
    let student = login(&app, "rev_bank_s").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "maths").await;
    let subject = create_subject(&app, &teacher, &course, "arithmetic").await;
    enroll(&app, &teacher, &course, &student_id).await;

    // One template, instantiated into both exams below.
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&teacher),
        Some(json!({ "subject_id": subject, "text": "2 + 2?", "kind": "choice",
                     "points": 10, "choices": [{"id": "c0", "text": "3"}, {"id": "c1", "text": "4"}],
                     "correct": "c1" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let template = id_of(&res.body);

    let exam = |title: &str, review: bool| {
        create_exam_with(
            &app,
            &teacher,
            &course,
            json!({ "title": title, "kind": "quiz", "mode": "open",
                    "max_attempts": 1, "allow_review": review }),
        )
    };
    let res = exam("graded quiz", true).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let graded = id_of(&res.body);
    let res = exam("live quiz", false).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let live = id_of(&res.body);

    let from_bank = async |exam: &str| {
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/questions/from-bank/{template}"),
            Some(&teacher),
            Some(json!({ "subject_id": subject })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
        res
    };
    let res = from_bank(&graded).await;
    let shared = id_of(&res.body);
    let shared_right = res.body["correct"].as_str().expect("key").to_string();
    from_bank(&live).await;

    // A hand-written question in the same exam: no template, never hidden.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{graded}/questions"),
        Some(&teacher),
        Some(json!({ "text": "3 + 3?", "kind": "choice", "points": 10,
                     "subject_id": subject,
                     "choices": [{"id": "c0", "text": "5"}, {"id": "c1", "text": "6"}],
                     "correct": "c1" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let own = id_of(&res.body);

    // Sit the graded exam, answer the shared question right, get marked, finish.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{graded}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{graded}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": shared, "selected": shared_right })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{graded}/results"),
        Some(&teacher),
        Some(json!({ "mark": 90, "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{graded}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let key_uri = format!("/exams/{graded}/review/questions");
    let sheet_uri = format!("/exams/{graded}/review/attempts/1/answers");
    let key_of = |body: &serde_json::Value, id: &str| {
        common::items(body)
            .iter()
            .find(|q| q["id"] == id)
            .expect("question in the review list")["correct"]
            .clone()
    };

    // Nothing live yet: the whole key is readable, both questions score.
    let res = send(&app, "GET", &key_uri, Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(key_of(&res.body, &shared), json!(shared_right));
    let res = send(&app, "GET", &sheet_uri, Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["answers"][0]["is_correct"], json!(true));
    assert_eq!(
        res.body["auto_score"],
        json!({"earned": 10, "possible": 20})
    );

    // Now the caller sits the other exam, which holds a copy of the template.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{live}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["status"], "in_progress", "{}", res.body);

    // The leak: the shared question's key is gone, the hand-written one's is
    // not, and review itself stays open.
    let res = send(&app, "GET", &key_uri, Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(key_of(&res.body, &shared), json!(null), "{}", res.body);
    assert_ne!(key_of(&res.body, &own), json!(null), "{}", res.body);

    // Same on the sheet: no correctness flag, and no score to subtract it out of.
    let res = send(&app, "GET", &sheet_uri, Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["answers"][0]["question"], shared, "{}", res.body);
    assert_eq!(res.body["answers"][0]["is_correct"], json!(null));
    assert_eq!(res.body["auto_score"], json!({"earned": 0, "possible": 10}));

    // Submitting the other sitting gives the key back.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{live}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(&app, "GET", &key_uri, Some(&student), None).await;
    assert_eq!(
        key_of(&res.body, &shared),
        json!(shared_right),
        "{}",
        res.body
    );
}

/// The template link runs both ways, and the join has to see both columns. A
/// question authored by hand inside exam A and *saved* to the bank carries
/// `banked_as` and no `from_bank`; its copy in exam B carries `from_bank` and
/// no `banked_as`. Matching either side on `from_bank` alone saw neither, and
/// reviewing A handed out the key to a question live in B. Covered here:
///   - A holds `banked_as`, B holds `from_bank` (the reported probe),
///   - A holds `from_bank`, B holds `banked_as` (the mirror),
///   - A holds both (instantiated from one template, re-saved as another) and
///     B holds only the second — the chain a coalesce would miss.
#[tokio::test]
async fn review_hides_a_question_banked_out_of_it_and_live_elsewhere() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "rev_bank2_t", "teacher").await;
    let student = login(&app, "rev_bank2_s").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "physics").await;
    let subject = create_subject(&app, &teacher, &course, "optics").await;
    enroll(&app, &teacher, &course, &student_id).await;

    let exam = async |title: &str, review: bool| {
        let res = create_exam_with(
            &app,
            &teacher,
            &course,
            json!({ "title": title, "kind": "quiz", "mode": "open",
                    "max_attempts": 1, "allow_review": review }),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
        id_of(&res.body)
    };
    let graded = exam("reviewed quiz", true).await;
    let live = exam("live quiz", false).await;

    // A question written by hand into `exam` — no provenance either way.
    let hand = async |exam: &str, text: &str| {
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/questions"),
            Some(&teacher),
            Some(json!({ "text": text, "kind": "choice", "points": 10,
                         "subject_id": subject,
                         "choices": [{"id": "c0", "text": "no"}, {"id": "c1", "text": "yes"}],
                         "correct": "c1" })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
        id_of(&res.body)
    };
    // Save it into the bank: writes `banked_as`, leaves `from_bank` alone.
    let to_bank = async |exam: &str, question: &str| {
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/questions/{question}/to-bank"),
            Some(&teacher),
            None,
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
        id_of(&res.body)
    };
    // Instantiate a template into `exam`: writes `from_bank`.
    let from_bank = async |exam: &str, template: &str| {
        let res = send(
            &app,
            "POST",
            &format!("/exams/{exam}/questions/from-bank/{template}"),
            Some(&teacher),
            Some(json!({ "subject_id": subject })),
        )
        .await;
        assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
        id_of(&res.body)
    };

    // 1. banked out of the reviewed exam, instantiated into the live one.
    let banked = hand(&graded, "banked out of A").await;
    from_bank(&live, &to_bank(&graded, &banked).await).await;

    // 2. the mirror: banked out of the live exam, instantiated into the
    //    reviewed one.
    let mirrored = from_bank(
        &graded,
        &to_bank(&live, &hand(&live, "banked out of B").await).await,
    )
    .await;

    // 3. the chain: instantiated into the reviewed exam from one template, then
    //    re-saved as a second — and only the second reached the live exam.
    let res = send(
        &app,
        "POST",
        "/bank-questions",
        Some(&teacher),
        Some(json!({ "subject_id": subject, "text": "chained", "kind": "choice",
                     "points": 10, "choices": [{"id": "c0", "text": "no"}, {"id": "c1", "text": "yes"}],
                     "correct": "c1" })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let chained = from_bank(&graded, &id_of(&res.body)).await;
    from_bank(&live, &to_bank(&graded, &chained).await).await;

    // The control: hand-written, never banked, never instantiated.
    let own = hand(&graded, "purely local").await;

    // Sit the reviewed exam, get marked, finish.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{graded}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{graded}/results"),
        Some(&teacher),
        Some(json!({ "mark": 90, "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(
        &app,
        "POST",
        &format!("/exams/{graded}/attempt/finish"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    let key_uri = format!("/exams/{graded}/review/questions");
    let key_of = |body: &serde_json::Value, id: &str| {
        common::items(body)
            .iter()
            .find(|q| q["id"] == id)
            .expect("question in the review list")["correct"]
            .clone()
    };

    // Nothing live: the whole key reads.
    let res = send(&app, "GET", &key_uri, Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    for id in [&banked, &mirrored, &chained, &own] {
        assert_ne!(key_of(&res.body, id), json!(null), "{}", res.body);
    }

    // The caller opens a sitting on the exam holding the copies.
    let res = send(
        &app,
        "POST",
        &format!("/exams/{live}/attempt"),
        Some(&student),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["status"], "in_progress", "{}", res.body);

    // The leak: all three shared questions blank out, the local one does not,
    // and review itself stays open.
    let res = send(&app, "GET", &key_uri, Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(key_of(&res.body, &banked), json!(null), "{}", res.body);
    assert_eq!(key_of(&res.body, &mirrored), json!(null), "{}", res.body);
    assert_eq!(key_of(&res.body, &chained), json!(null), "{}", res.body);
    assert_ne!(key_of(&res.body, &own), json!(null), "{}", res.body);
}

/// A student mid-retake must not be able to read the answer key or the
/// correctness flags on any of their own sittings — and the reads must open
/// again once the last sitting allowed is submitted.
#[tokio::test]
async fn a_retake_in_progress_closes_the_students_own_review_reads() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "rev_race_t", "teacher").await;
    let student = login(&app, "rev_race_s").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "biology").await;
    let subject = create_subject(&app, &teacher, &course, "cells").await;
    enroll(&app, &teacher, &course, &student_id).await;
    let res = create_exam_with(
        &app,
        &teacher,
        &course,
        json!({
            "title": "cell quiz", "kind": "quiz", "mode": "open",
            "max_attempts": 2, "allow_review": true,
        }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let exam = id_of(&res.body);

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        Some(json!({
            "text": "Name an organelle.", "kind": "text", "points": 10,
            "subject_id": subject,
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let question = id_of(&res.body);

    let attempt_uri = format!("/exams/{exam}/attempt");
    let finish_uri = format!("/exams/{exam}/attempt/finish");
    let key_uri = format!("/exams/{exam}/review/questions");
    let sheet_uri = format!("/exams/{exam}/review/attempts/1/answers");

    // Seq 1: sit, answer, grade, finish.
    let res = send(&app, "POST", &attempt_uri, Some(&student), None).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": question, "text": "mitochondria" })),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        Some(json!({ "mark": 40, "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(&app, "POST", &finish_uri, Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // Post-finish review: still shut, because seq 2 is still on the table.
    // Reading the key here and *then* posting the retake was the leak.
    let res = send(&app, "GET", &key_uri, Some(&student), None).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // Seq 2: the retake starts and is still writable.
    let res = send(&app, "POST", &attempt_uri, Some(&student), None).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["attempt"], 2, "{}", res.body);
    assert_eq!(res.body["status"], "in_progress", "{}", res.body);

    // The leak: neither the answer key nor a judged sheet may be readable now.
    for uri in [&key_uri, &sheet_uri] {
        let res = send(&app, "GET", uri, Some(&student), None).await;
        assert_eq!(res.status, StatusCode::CONFLICT, "{uri}: {}", res.body);
    }

    // Submitting the retake reopens review.
    let res = send(&app, "POST", &finish_uri, Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    for uri in [&key_uri, &sheet_uri] {
        let res = send(&app, "GET", uri, Some(&student), None).await;
        assert_eq!(res.status, StatusCode::OK, "{uri}: {}", res.body);
    }
}

/// The leak the in-progress gate alone could not see: a *finished* sitting on an
/// exam that still allows another one. The student reads the key at leisure and
/// then posts the retake with it in hand — no sitting is open at the moment of
/// the read, so nothing refused them. Review must stay shut until no further
/// sitting is available, and open the moment that is true.
#[tokio::test]
async fn review_stays_shut_while_another_sitting_is_still_available() {
    let (app, db) = app_and_db().await;
    let teacher = login_as(&app, &db, "rev_left_t", "teacher").await;
    let student = login(&app, "rev_left_s").await;
    let student_id = me_id(&app, &student).await;

    let course = create_course(&app, &teacher, "chemistry").await;
    let subject = create_subject(&app, &teacher, &course, "bonds").await;
    enroll(&app, &teacher, &course, &student_id).await;
    // Unlimited sittings: no amount of finishing exhausts them.
    let res = create_exam_with(
        &app,
        &teacher,
        &course,
        json!({
            "title": "bond quiz", "kind": "quiz", "mode": "open",
            "max_attempts": 0, "allow_review": true,
        }),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let exam = id_of(&res.body);

    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/questions"),
        Some(&teacher),
        Some(json!({
            "text": "Which bond shares electrons?", "kind": "choice", "points": 10,
            "subject_id": subject,
            "choices": [{"id": "c0", "text": "ionic"}, {"id": "c1", "text": "covalent"}],
            "correct": "c1",
        })),
    )
    .await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    let question = id_of(&res.body);
    // Choice ids are re-minted server-side; the key is whatever came back.
    let right = res.body["correct"].as_str().expect("key").to_string();
    let wrong = res.body["choices"][0]["id"].as_str().unwrap().to_string();

    let attempt_uri = format!("/exams/{exam}/attempt");
    let finish_uri = format!("/exams/{exam}/attempt/finish");
    let key_uri = format!("/exams/{exam}/review/questions");
    let sheet_uri = format!("/exams/{exam}/review/attempts/1/answers");

    // Seq 1: sit, answer, grade, finish. Nothing is open now.
    let res = send(&app, "POST", &attempt_uri, Some(&student), None).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    send(
        &app,
        "POST",
        &format!("/exams/{exam}/attempt/answers"),
        Some(&student),
        Some(json!({ "question_id": question, "selected": wrong })),
    )
    .await;
    let res = send(
        &app,
        "POST",
        &format!("/exams/{exam}/results"),
        Some(&teacher),
        Some(json!({ "mark": 20, "user_id": student_id })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(&app, "POST", &finish_uri, Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);

    // The leak, exactly: no sitting open, a mark in hand, seq 2 still startable.
    for uri in [&key_uri, &sheet_uri] {
        let res = send(&app, "GET", uri, Some(&student), None).await;
        assert_eq!(res.status, StatusCode::CONFLICT, "{uri}: {}", res.body);
    }
    // And the retake really is available — that is what makes the read a leak.
    let res = send(&app, "POST", &attempt_uri, Some(&student), None).await;
    assert_eq!(res.status, StatusCode::CREATED, "{}", res.body);
    assert_eq!(res.body["attempt"], 2, "{}", res.body);
    let res = send(&app, "POST", &finish_uri, Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(&app, "GET", &key_uri, Some(&student), None).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);

    // Closing the door — lowering `max_attempts` to what the student already
    // used blocks future starts — opens review, key included.
    let res = send(
        &app,
        "PATCH",
        &format!("/exams/{exam}"),
        Some(&teacher),
        Some(json!({ "max_attempts": 2 })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = send(&app, "POST", &attempt_uri, Some(&student), None).await;
    assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.body);
    for uri in [&key_uri, &sheet_uri] {
        let res = send(&app, "GET", uri, Some(&student), None).await;
        assert_eq!(res.status, StatusCode::OK, "{uri}: {}", res.body);
    }
    let res = send(&app, "GET", &key_uri, Some(&student), None).await;
    assert_eq!(
        common::items(&res.body)[0]["correct"],
        json!(right),
        "{}",
        res.body
    );
}
