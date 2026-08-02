//! Regression for the self-review answer-key leak found in the 2026-08-02
//! sweep: the review gate proved "the caller has a mark" by reading their
//! *latest* mark, so a mark left at seq 1 satisfied it forever. A student who
//! started a retake could read the answer key — and their own `is_correct` /
//! `auto_score` flags — on the sheet they were still writing.
//!
//! The gate now refuses (409) while the caller's latest sitting is in progress.
//! Deterministic: the retake is started over HTTP, no clock or race involved.

mod common;

use axum::http::StatusCode;
use common::{
    app_and_db, create_course, create_exam_with, create_subject, enroll, id_of, login, login_as,
    me_id, send,
};
use serde_json::json;

/// A student mid-retake must not be able to read the answer key or the
/// correctness flags on any of their own sittings — but the same reads must
/// still work before the retake starts and again once it is submitted.
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

    // Post-finish review: unchanged, this is the behaviour being preserved.
    let res = send(&app, "GET", &key_uri, Some(&student), None).await;
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(res.body["items"][0]["id"], question);

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
