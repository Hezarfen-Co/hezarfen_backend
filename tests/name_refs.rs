//! The settings' two removal guards, now that they are reference counters
//! rather than a count-then-write under a process-wide lock: an exam kind may
//! leave `exam_kinds` only while no mark is written under it, and a meal slot
//! may leave `meal_slots` only while no menu is published for it.
//!
//! Each test below bites on one half of the pair — the retirement's `WHERE`
//! (nothing may leave while it is referenced) or the claim's (nothing may be
//! referenced once it has left). Drop either predicate and a test here fails;
//! that is the point of the suite, because the guard itself is invisible in
//! the happy path.

mod common;

use axum::http::StatusCode;
use common::{create_course, create_exam, enroll, login_as, me_id, send};
use serde_json::{Value, json};

/// The two kinds these tests move in and out of the school's list.
fn kinds(names: &[&str]) -> Value {
    json!(
        names
            .iter()
            .map(|name| json!({ "name": name, "weight": 1 }))
            .collect::<Vec<_>>()
    )
}

fn slots(names: &[&str]) -> Value {
    json!(
        names
            .iter()
            .map(|name| json!({ "name": name, "serving_minute": 720 }))
            .collect::<Vec<_>>()
    )
}

fn listed_kinds(body: &Value) -> Vec<String> {
    body["exam_kinds"]
        .as_array()
        .expect("exam_kinds")
        .iter()
        .map(|kind| kind["name"].as_str().expect("name").to_string())
        .collect()
}

/// A graded student, an exam of `kind`, and the manager's cookie.
struct School {
    app: axum::Router,
    boss: String,
    course: String,
    exam: String,
    student: String,
}

async fn school_with_kinds(names: &[&str]) -> School {
    let (app, db) = common::app_and_db().await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let student_cookie = login_as(&app, &db, "kid", "student").await;
    let student = me_id(&app, &student_cookie).await;
    let res = send(
        &app,
        "PATCH",
        "/settings",
        Some(&boss),
        Some(json!({ "exam_kinds": kinds(names) })),
    )
    .await;
    assert_eq!(res.status, StatusCode::OK, "seed exam kinds");
    let course = create_course(&app, &boss, "Maths").await;
    enroll(&app, &boss, &course, &student).await;
    let exam = create_exam(&app, &boss, &course, "Midterm", names[0]).await;
    School {
        app,
        boss,
        course,
        exam,
        student,
    }
}

impl School {
    async fn grade(&self, mark: i64) -> common::Res {
        self.grade_exam(&self.exam.clone(), mark).await
    }

    async fn grade_exam(&self, exam: &str, mark: i64) -> common::Res {
        send(
            &self.app,
            "POST",
            &format!("/exams/{exam}/results"),
            Some(&self.boss),
            Some(json!({ "user_id": self.student, "mark": mark })),
        )
        .await
    }

    async fn set_kinds(&self, names: &[&str]) -> common::Res {
        send(
            &self.app,
            "PATCH",
            "/settings",
            Some(&self.boss),
            Some(json!({ "exam_kinds": kinds(names) })),
        )
        .await
    }

    async fn settings(&self) -> Value {
        send(&self.app, "GET", "/settings", Some(&self.boss), None)
            .await
            .body
    }
}

/// The retirement's `WHERE`: a kind a mark is written under does not leave the
/// list, and the refused edit changes *nothing* — not the stored list, and not
/// the kind's own usability.
#[tokio::test]
async fn a_graded_kind_cannot_be_removed_and_nothing_moves() {
    let school = school_with_kinds(&["lab", "quiz"]).await;
    assert_eq!(school.grade(80).await.status, StatusCode::OK);

    let res = school.set_kinds(&["quiz"]).await;
    assert_eq!(res.status, StatusCode::CONFLICT);
    assert_eq!(
        res.body["error"],
        "exams of kind 'lab' are already graded — the kind cannot be removed"
    );

    // The list the school reads back still carries the kind...
    assert_eq!(listed_kinds(&school.settings().await), ["lab", "quiz"]);
    // ...and so does the counter: a refused removal must not leave the kind
    // retired, or every later grade under it would be refused instead.
    assert_eq!(school.grade(90).await.status, StatusCode::OK);
}

/// A regrade is not a second reference: one mark, one reference, however many
/// times it is overwritten — otherwise the counter would drift upward and the
/// kind could never be removed again.
#[tokio::test]
async fn marks_release_their_kind_when_they_are_deleted() {
    let school = school_with_kinds(&["lab", "quiz"]).await;
    for mark in [10, 20, 30] {
        assert_eq!(school.grade(mark).await.status, StatusCode::OK);
    }

    let res = send(
        &school.app,
        "DELETE",
        &format!("/exams/{}/results/{}", school.exam, school.student),
        Some(&school.boss),
        None,
    )
    .await;
    assert_eq!(res.status, StatusCode::NO_CONTENT);

    // Nothing is graded under it any more, so it leaves freely.
    assert_eq!(school.set_kinds(&["quiz"]).await.status, StatusCode::OK);
    assert_eq!(listed_kinds(&school.settings().await), ["quiz"]);
}

/// The claim's `WHERE`, the other half of the same race: once the kind is out
/// of the list, a mark can no longer land under it. Whichever of the two
/// writes reaches the counter first, the other is refused — which is what makes
/// the pair safe without a lock spanning both writes.
#[tokio::test]
async fn a_removed_kind_refuses_a_grade() {
    let school = school_with_kinds(&["lab", "quiz"]).await;
    // Nothing graded yet, so the kind leaves — the exam keeps carrying it.
    assert_eq!(school.set_kinds(&["quiz"]).await.status, StatusCode::OK);

    let res = school.grade(80).await;
    assert_eq!(res.status, StatusCode::CONFLICT);
    assert_eq!(
        res.body["error"],
        "the 'lab' exam kind has been removed from the school's settings — \
         add it back before grading this exam"
    );

    // Adding it back puts it in service again.
    assert_eq!(
        school.set_kinds(&["quiz", "lab"]).await.status,
        StatusCode::OK
    );
    assert_eq!(school.grade(80).await.status, StatusCode::OK);
}

/// An exam's kind is frozen once it carries marks: re-pointing it would
/// re-weight them silently (and strand their references on the kind they were
/// counted under). Ungraded, it moves freely — and its next mark is counted
/// under the kind it moved to.
#[tokio::test]
async fn a_graded_exams_kind_is_frozen() {
    let school = school_with_kinds(&["lab", "quiz"]).await;
    let repoint = async || {
        send(
            &school.app,
            "PATCH",
            &format!("/exams/{}", school.exam),
            Some(&school.boss),
            Some(json!({ "kind": "quiz" })),
        )
        .await
    };
    // Ungraded: the move lands.
    assert_eq!(repoint().await.status, StatusCode::OK);
    assert_eq!(school.grade(80).await.status, StatusCode::OK);
    // The mark went to the kind the exam now carries, not the one it left.
    assert_eq!(school.set_kinds(&["quiz"]).await.status, StatusCode::OK);
    assert_eq!(
        school.set_kinds(&["lab", "quiz"]).await.status,
        StatusCode::OK
    );

    // Graded: the move is refused, and the exam keeps the kind it had.
    let back = send(
        &school.app,
        "PATCH",
        &format!("/exams/{}", school.exam),
        Some(&school.boss),
        Some(json!({ "kind": "lab" })),
    )
    .await;
    assert_eq!(back.status, StatusCode::CONFLICT);
    assert_eq!(
        back.body["error"],
        "cannot change the kind of an exam that already has marks"
    );
    // Everything else on the exam still edits, marks or no marks.
    let title = send(
        &school.app,
        "PATCH",
        &format!("/exams/{}", school.exam),
        Some(&school.boss),
        Some(json!({ "title": "Midterm II" })),
    )
    .await;
    assert_eq!(title.status, StatusCode::OK);
}

/// Deleting an exam frees exactly the marks it cascaded — no more. Two exams
/// share the kind here: releasing one mark too many (the shape a count taken
/// outside the cascade's transaction produces) would read as no marks left and
/// let the kind walk out of the settings while the second exam is still graded.
#[tokio::test]
async fn deleting_one_exam_frees_only_its_own_marks() {
    let school = school_with_kinds(&["lab", "quiz"]).await;
    let second = create_exam(&school.app, &school.boss, &school.course, "Final", "lab").await;
    assert_eq!(school.grade(80).await.status, StatusCode::OK);
    assert_eq!(school.grade_exam(&second, 70).await.status, StatusCode::OK);

    let gone = send(
        &school.app,
        "DELETE",
        &format!("/exams/{}", school.exam),
        Some(&school.boss),
        None,
    )
    .await;
    assert_eq!(gone.status, StatusCode::NO_CONTENT);

    // The other exam's mark still holds the kind down.
    let refused = school.set_kinds(&["quiz"]).await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(
        refused.body["error"],
        "exams of kind 'lab' are already graded — the kind cannot be removed"
    );

    // Only when the last graded exam goes does the kind go free.
    let last = send(
        &school.app,
        "DELETE",
        &format!("/exams/{second}"),
        Some(&school.boss),
        None,
    )
    .await;
    assert_eq!(last.status, StatusCode::NO_CONTENT);
    assert_eq!(school.set_kinds(&["quiz"]).await.status, StatusCode::OK);
}

/// A course delete cascades its exams' marks, so it owes their kinds the same
/// references back — otherwise the kind is held down by marks that no longer
/// exist, forever.
#[tokio::test]
async fn deleting_a_course_frees_the_kinds_its_marks_held() {
    let school = school_with_kinds(&["lab", "quiz"]).await;
    assert_eq!(school.grade(80).await.status, StatusCode::OK);
    // A course with a roster refuses the delete; the mark stays either way.
    common::unenroll(&school.app, &school.boss, &school.course, &school.student).await;

    let gone = send(
        &school.app,
        "DELETE",
        &format!("/courses/{}", school.course),
        Some(&school.boss),
        None,
    )
    .await;
    assert_eq!(gone.status, StatusCode::NO_CONTENT, "{}", gone.body);

    assert_eq!(school.set_kinds(&["quiz"]).await.status, StatusCode::OK);
    assert_eq!(listed_kinds(&school.settings().await), ["quiz"]);
}

/// The same pair for meal slots, whose reference is a published menu.
#[tokio::test]
async fn a_published_menu_pins_its_meal_slot() {
    let (app, db) = common::app_and_db().await;
    let boss = login_as(&app, &db, "boss", "manager").await;
    let set_slots = async |names: &[&str]| {
        send(
            &app,
            "PATCH",
            "/settings",
            Some(&boss),
            Some(json!({ "meal_slots": slots(names) })),
        )
        .await
    };
    assert_eq!(set_slots(&["lunch", "snack"]).await.status, StatusCode::OK);

    let menu = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&boss),
        Some(json!({ "date": "2030-01-09", "slot": "lunch" })),
    )
    .await;
    assert_eq!(menu.status, StatusCode::CREATED);
    let menu_id = common::id_of(&menu.body);

    let refused = set_slots(&["snack"]).await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(
        refused.body["error"],
        "menus are already published for the 'lunch' slot — it cannot be removed"
    );

    // Retire the menu and the slot is free to leave — then no new menu may be
    // published under it.
    let deleted = send(
        &app,
        "DELETE",
        &format!("/meals/menus/{menu_id}"),
        Some(&boss),
        None,
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert_eq!(set_slots(&["snack"]).await.status, StatusCode::OK);

    let late = send(
        &app,
        "POST",
        "/meals/menus",
        Some(&boss),
        Some(json!({ "date": "2030-01-10", "slot": "lunch" })),
    )
    .await;
    assert_eq!(
        late.status,
        StatusCode::BAD_REQUEST,
        "slot is no longer served"
    );
}

