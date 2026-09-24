//! The OpenAPI document must not lie about its own bounds.
//!
//! utoipa 5.5's `#[schema(max_length = …)]` accepts a **literal only** — a
//! `const` there fails to compile with "no `literal` value found after this
//! point". So every annotation in `src/web/*.rs` is unavoidably a hand-copied
//! duplicate of a number in `src/constant.rs`, and a copy nothing checks is a
//! copy that silently goes stale. A frontend generating validators from
//! `/api-docs/openapi.json` would then enforce yesterday's rule and get 400s it
//! cannot explain.
//!
//! This test closes that loop from the far end: it builds the real router,
//! fetches the emitted spec, and asserts each published bound still equals the
//! constant the server actually validates against. The expectations below are
//! the constants — never literals — because a test written against literals
//! would be a third copy and would pass while both were wrong.

use axum::body::Body;
use axum::http::Request;
use hezarfen_backend::constant::*;
use hezarfen_backend::state::AppState;
use hezarfen_backend::{build_router, database};
use serde_json::Value;
use tower::ServiceExt;

/// The emitted OpenAPI document, as the server serves it.
async fn spec() -> Value {
    let tenants = database::init_test_tenants().await;
    let app = build_router(AppState {
        db: tenants.control().clone(),
        tenants,
        files_path: tempfile::tempdir().expect("files dir").keep(),
        cookie_secure: false,
        rate_limit: hezarfen_backend::rate_limit::RateLimitConfig::unlimited(),
        chatbot_limit: Default::default(),
        rag_limit: Default::default(),
        exam_presence: Default::default(),
        board_hub: Default::default(),
        ai: None,
        metrics: hezarfen_backend::telemetry::Metrics::noop(),
    });
    let res = app
        .oneshot(
            Request::builder()
                .uri("/api-docs/openapi.json")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("spec response");
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("spec body");
    serde_json::from_slice(&bytes).expect("spec is json")
}

/// Every published request-schema bound, paired with the constant it must
/// equal. Columns: schema, field, JSON Schema keyword, expected value.
///
/// Add a row whenever you add a `#[schema(max_length = …)]`-style attribute to
/// a request DTO — the annotation is the promise, this row is the proof.
fn expectations() -> Vec<(&'static str, &'static str, &'static str, i64)> {
    vec![
        // --- builder (the vendor surface) ---
        (
            "CreateSchool",
            "name",
            "maxLength",
            MAX_SCHOOL_NAME_LEN as i64,
        ),
        (
            "UpdateSchool",
            "name",
            "maxLength",
            MAX_SCHOOL_NAME_LEN as i64,
        ),
        (
            "CreateSchool",
            "admin_username",
            "minLength",
            MIN_USERNAME_LEN as i64,
        ),
        (
            "CreateSchool",
            "admin_username",
            "maxLength",
            MAX_USERNAME_LEN as i64,
        ),
        (
            "CreateSchool",
            "admin_password",
            "minLength",
            MIN_PASSWORD_LEN as i64,
        ),
        (
            "CreateSchool",
            "admin_password",
            "maxLength",
            MAX_PASSWORD_LEN as i64,
        ),
        (
            "BuilderCredentials",
            "username",
            "minLength",
            MIN_USERNAME_LEN as i64,
        ),
        (
            "BuilderCredentials",
            "username",
            "maxLength",
            MAX_USERNAME_LEN as i64,
        ),
        (
            "BuilderCredentials",
            "password",
            "minLength",
            MIN_PASSWORD_LEN as i64,
        ),
        (
            "BuilderCredentials",
            "password",
            "maxLength",
            MAX_PASSWORD_LEN as i64,
        ),
        (
            "AdminPassword",
            "username",
            "minLength",
            MIN_USERNAME_LEN as i64,
        ),
        (
            "AdminPassword",
            "username",
            "maxLength",
            MAX_USERNAME_LEN as i64,
        ),
        (
            "EnterSchool",
            "username",
            "minLength",
            MIN_USERNAME_LEN as i64,
        ),
        (
            "EnterSchool",
            "username",
            "maxLength",
            MAX_USERNAME_LEN as i64,
        ),
        (
            "AdminPassword",
            "password",
            "minLength",
            MIN_PASSWORD_LEN as i64,
        ),
        (
            "AdminPassword",
            "password",
            "maxLength",
            MAX_PASSWORD_LEN as i64,
        ),
        // --- auth / users / notes / messages ---
        (
            "RegisterCredentials",
            "username",
            "minLength",
            MIN_USERNAME_LEN as i64,
        ),
        (
            "RegisterCredentials",
            "username",
            "maxLength",
            MAX_USERNAME_LEN as i64,
        ),
        (
            "RegisterCredentials",
            "password",
            "minLength",
            MIN_PASSWORD_LEN as i64,
        ),
        (
            "RegisterCredentials",
            "password",
            "maxLength",
            MAX_PASSWORD_LEN as i64,
        ),
        (
            "LoginCredentials",
            "username",
            "minLength",
            MIN_USERNAME_LEN as i64,
        ),
        (
            "LoginCredentials",
            "username",
            "maxLength",
            MAX_USERNAME_LEN as i64,
        ),
        (
            "LoginCredentials",
            "password",
            "minLength",
            MIN_PASSWORD_LEN as i64,
        ),
        (
            "LoginCredentials",
            "password",
            "maxLength",
            MAX_PASSWORD_LEN as i64,
        ),
        (
            "CreateUser",
            "username",
            "minLength",
            MIN_USERNAME_LEN as i64,
        ),
        (
            "CreateUser",
            "username",
            "maxLength",
            MAX_USERNAME_LEN as i64,
        ),
        (
            "CreateUser",
            "password",
            "minLength",
            MIN_PASSWORD_LEN as i64,
        ),
        (
            "CreateUser",
            "password",
            "maxLength",
            MAX_PASSWORD_LEN as i64,
        ),
        (
            "CreateUser",
            "student_number",
            "maxLength",
            MAX_STUDENT_NUMBER_LEN as i64,
        ),
        ("UpdateProfile", "name", "maxLength", MAX_NAME_LEN as i64),
        ("UpdateProfile", "surname", "maxLength", MAX_NAME_LEN as i64),
        ("UpdateProfile", "email", "maxLength", MAX_EMAIL_LEN as i64),
        (
            "UpdateProfile",
            "display_name",
            "maxLength",
            MAX_DISPLAY_NAME_LEN as i64,
        ),
        ("UpdateProfile", "bio", "maxLength", MAX_BIO_LEN as i64),
        // Free text with the same length cap the bio has.
        ("UpdateProfile", "address", "maxLength", MAX_ADDRESS_LEN as i64),
        // The emergency contact is a person's name, held to the same bound.
        (
            "UpdateProfile",
            "emergency_contact_name",
            "maxLength",
            MAX_NAME_LEN as i64,
        ),
        // A branş is one entry of the school's `branches` list, so it is
        // bounded by the same per-entry length the list itself is.
        (
            "UpdateProfile",
            "branch",
            "maxLength",
            MAX_SETTINGS_ITEM_LEN as i64,
        ),
        (
            "UpdateProfile",
            "student_number",
            "maxLength",
            MAX_STUDENT_NUMBER_LEN as i64,
        ),
        (
            "CreateNote",
            "title",
            "maxLength",
            MAX_NOTE_TITLE_LEN as i64,
        ),
        (
            "CreateNote",
            "content",
            "maxLength",
            MAX_NOTE_CONTENT_LEN as i64,
        ),
        (
            "UpdateNote",
            "title",
            "maxLength",
            MAX_NOTE_TITLE_LEN as i64,
        ),
        (
            "UpdateNote",
            "content",
            "maxLength",
            MAX_NOTE_CONTENT_LEN as i64,
        ),
        (
            "CreateCourseNote",
            "title",
            "maxLength",
            MAX_NOTE_TITLE_LEN as i64,
        ),
        (
            "CreateCourseNote",
            "content",
            "maxLength",
            MAX_NOTE_CONTENT_LEN as i64,
        ),
        (
            "UpdateCourseNote",
            "title",
            "maxLength",
            MAX_NOTE_TITLE_LEN as i64,
        ),
        (
            "UpdateCourseNote",
            "content",
            "maxLength",
            MAX_NOTE_CONTENT_LEN as i64,
        ),
        (
            "SendMessage",
            "subject",
            "maxLength",
            MAX_MESSAGE_SUBJECT_LEN as i64,
        ),
        (
            "SendMessage",
            "body",
            "maxLength",
            MAX_MESSAGE_BODY_LEN as i64,
        ),
        (
            "SendMessage",
            "label",
            "maxLength",
            MAX_MESSAGE_LABEL_LEN as i64,
        ),
        // --- events / appointments / courses / subjects / sessions ---
        (
            "CreateEvent",
            "title",
            "maxLength",
            MAX_EVENT_TITLE_LEN as i64,
        ),
        (
            "CreateEvent",
            "description",
            "maxLength",
            MAX_EVENT_DESCRIPTION_LEN as i64,
        ),
        (
            "UpdateEvent",
            "title",
            "maxLength",
            MAX_EVENT_TITLE_LEN as i64,
        ),
        (
            "UpdateEvent",
            "description",
            "maxLength",
            MAX_EVENT_DESCRIPTION_LEN as i64,
        ),
        (
            "MarkAttendance",
            "status",
            "maxLength",
            MAX_SETTINGS_ITEM_LEN as i64,
        ),
        (
            "PublishSlots",
            "note",
            "maxLength",
            MAX_APPOINTMENT_NOTE_LEN as i64,
        ),
        (
            "BookAppointment",
            "reason",
            "maxLength",
            MAX_APPOINTMENT_REASON_LEN as i64,
        ),
        (
            "CancelRequest",
            "reason",
            "maxLength",
            MAX_APPOINTMENT_REASON_LEN as i64,
        ),
        (
            "RejectRequest",
            "reason",
            "maxLength",
            MAX_APPOINTMENT_REASON_LEN as i64,
        ),
        (
            "CreateCourse",
            "title",
            "maxLength",
            MAX_COURSE_TITLE_LEN as i64,
        ),
        (
            "CreateCourse",
            "description",
            "maxLength",
            MAX_COURSE_DESCRIPTION_LEN as i64,
        ),
        (
            "UpdateCourse",
            "title",
            "maxLength",
            MAX_COURSE_TITLE_LEN as i64,
        ),
        (
            "UpdateCourse",
            "description",
            "maxLength",
            MAX_COURSE_DESCRIPTION_LEN as i64,
        ),
        (
            "CreateClass",
            "name",
            "maxLength",
            MAX_CLASS_NAME_LEN as i64,
        ),
        (
            "CreateClass",
            "grade_level",
            "minimum",
            MIN_GRADE_LEVEL as i64,
        ),
        (
            "CreateClass",
            "grade_level",
            "maximum",
            MAX_GRADE_LEVEL as i64,
        ),
        (
            "CreateBlueprint",
            "grade_level",
            "minimum",
            MIN_GRADE_LEVEL as i64,
        ),
        (
            "CreateBlueprint",
            "grade_level",
            "maximum",
            MAX_GRADE_LEVEL as i64,
        ),
        (
            "UpdateClass",
            "name",
            "maxLength",
            MAX_CLASS_NAME_LEN as i64,
        ),
        (
            "UpdateClass",
            "grade_level",
            "minimum",
            MIN_GRADE_LEVEL as i64,
        ),
        (
            "UpdateClass",
            "grade_level",
            "maximum",
            MAX_GRADE_LEVEL as i64,
        ),
        (
            "CreateExamInCourse",
            "title",
            "maxLength",
            MAX_EXAM_TITLE_LEN as i64,
        ),
        (
            "CreateExamInCourse",
            "description",
            "maxLength",
            MAX_EXAM_DESCRIPTION_LEN as i64,
        ),
        (
            "CreateExamInCourse",
            "kind",
            "maxLength",
            MAX_SETTINGS_ITEM_LEN as i64,
        ),
        (
            "CreateExamInCourse",
            "duration_ms",
            "minimum",
            MIN_EXAM_DURATION_MS,
        ),
        (
            "CreateExamInCourse",
            "duration_ms",
            "maximum",
            MAX_EXAM_DURATION_MS,
        ),
        (
            "CreateExamInCourse",
            "max_attempts",
            "minimum",
            UNLIMITED_EXAM_ATTEMPTS,
        ),
        (
            "CreateExamInCourse",
            "max_attempts",
            "maximum",
            MAX_EXAM_ATTEMPTS,
        ),
        (
            "CreateSubject",
            "name",
            "maxLength",
            MAX_SUBJECT_NAME_LEN as i64,
        ),
        (
            "CreateSubject",
            "description",
            "maxLength",
            MAX_SUBJECT_DESCRIPTION_LEN as i64,
        ),
        (
            "CreateHomework",
            "title",
            "maxLength",
            MAX_HOMEWORK_TITLE_LEN as i64,
        ),
        (
            "CreateHomework",
            "description",
            "maxLength",
            MAX_HOMEWORK_DESCRIPTION_LEN as i64,
        ),
        (
            "CreateHomework",
            "assigned",
            "maxItems",
            MAX_HOMEWORK_ASSIGNED as i64,
        ),
        (
            "CreateSessionInCourse",
            "topic",
            "maxLength",
            MAX_SESSION_TOPIC_LEN as i64,
        ),
        (
            "UpdateSession",
            "topic",
            "maxLength",
            MAX_SESSION_TOPIC_LEN as i64,
        ),
        (
            "MarkRollCall",
            "status",
            "maxLength",
            MAX_SETTINGS_ITEM_LEN as i64,
        ),
        (
            "UpdateSubject",
            "name",
            "maxLength",
            MAX_SUBJECT_NAME_LEN as i64,
        ),
        (
            "UpdateSubject",
            "description",
            "maxLength",
            MAX_SUBJECT_DESCRIPTION_LEN as i64,
        ),
        // --- exams / questions / question bank ---
        (
            "UpdateExam",
            "title",
            "maxLength",
            MAX_EXAM_TITLE_LEN as i64,
        ),
        (
            "UpdateExam",
            "description",
            "maxLength",
            MAX_EXAM_DESCRIPTION_LEN as i64,
        ),
        ("UpdateExam", "duration_ms", "minimum", MIN_EXAM_DURATION_MS),
        ("UpdateExam", "duration_ms", "maximum", MAX_EXAM_DURATION_MS),
        (
            "UpdateExam",
            "max_attempts",
            "minimum",
            UNLIMITED_EXAM_ATTEMPTS,
        ),
        ("UpdateExam", "max_attempts", "maximum", MAX_EXAM_ATTEMPTS),
        ("GradeResult", "mark", "minimum", MIN_MARK),
        ("GradeResult", "mark", "maximum", MAX_MARK),
        (
            "CreateQuestion",
            "text",
            "maxLength",
            MAX_QUESTION_TEXT_LEN as i64,
        ),
        ("CreateQuestion", "points", "minimum", MIN_QUESTION_POINTS),
        ("CreateQuestion", "points", "maximum", MAX_QUESTION_POINTS),
        (
            "CreateQuestion",
            "choices",
            "minItems",
            MIN_QUESTION_CHOICES as i64,
        ),
        (
            "CreateQuestion",
            "choices",
            "maxItems",
            MAX_QUESTION_CHOICES as i64,
        ),
        (
            "UpdateQuestion",
            "text",
            "maxLength",
            MAX_QUESTION_TEXT_LEN as i64,
        ),
        ("UpdateQuestion", "points", "minimum", MIN_QUESTION_POINTS),
        ("UpdateQuestion", "points", "maximum", MAX_QUESTION_POINTS),
        (
            "UpdateQuestion",
            "choices",
            "minItems",
            MIN_QUESTION_CHOICES as i64,
        ),
        (
            "UpdateQuestion",
            "choices",
            "maxItems",
            MAX_QUESTION_CHOICES as i64,
        ),
        (
            "SaveAnswer",
            "text",
            "maxLength",
            MAX_ANSWER_TEXT_LEN as i64,
        ),
        (
            "AskQuestion",
            "title",
            "maxLength",
            MAX_POOL_QUESTION_TITLE_LEN as i64,
        ),
        (
            "AskQuestion",
            "body",
            "maxLength",
            MAX_POOL_QUESTION_BODY_LEN as i64,
        ),
        (
            "OfferSolution",
            "body",
            "maxLength",
            MAX_SOLUTION_BODY_LEN as i64,
        ),
        (
            "CreateBankQuestion",
            "text",
            "maxLength",
            MAX_QUESTION_TEXT_LEN as i64,
        ),
        (
            "CreateBankQuestion",
            "points",
            "minimum",
            MIN_QUESTION_POINTS,
        ),
        (
            "CreateBankQuestion",
            "points",
            "maximum",
            MAX_QUESTION_POINTS,
        ),
        (
            "CreateBankQuestion",
            "choices",
            "minItems",
            MIN_QUESTION_CHOICES as i64,
        ),
        (
            "CreateBankQuestion",
            "choices",
            "maxItems",
            MAX_QUESTION_CHOICES as i64,
        ),
        (
            "UpdateBankQuestion",
            "text",
            "maxLength",
            MAX_QUESTION_TEXT_LEN as i64,
        ),
        (
            "UpdateBankQuestion",
            "points",
            "minimum",
            MIN_QUESTION_POINTS,
        ),
        (
            "UpdateBankQuestion",
            "points",
            "maximum",
            MAX_QUESTION_POINTS,
        ),
        (
            "UpdateBankQuestion",
            "choices",
            "minItems",
            MIN_QUESTION_CHOICES as i64,
        ),
        (
            "UpdateBankQuestion",
            "choices",
            "maxItems",
            MAX_QUESTION_CHOICES as i64,
        ),
        (
            "ChoiceBody",
            "text",
            "maxLength",
            MAX_CHOICE_TEXT_LEN as i64,
        ),
        // --- homework / settings / terms / chatbot ---
        (
            "UpdateHomework",
            "title",
            "maxLength",
            MAX_HOMEWORK_TITLE_LEN as i64,
        ),
        (
            "UpdateHomework",
            "description",
            "maxLength",
            MAX_HOMEWORK_DESCRIPTION_LEN as i64,
        ),
        (
            "UpdateHomework",
            "assigned",
            "maxItems",
            MAX_HOMEWORK_ASSIGNED as i64,
        ),
        (
            "SubmitHomework",
            "text",
            "maxLength",
            MAX_HOMEWORK_TEXT_LEN as i64,
        ),
        ("GradeHomework", "mark", "minimum", MIN_MARK),
        ("GradeHomework", "mark", "maximum", MAX_MARK),
        (
            "ExamKindDto",
            "name",
            "maxLength",
            MAX_SETTINGS_ITEM_LEN as i64,
        ),
        ("ExamKindDto", "weight", "minimum", MIN_EXAM_KIND_WEIGHT),
        ("ExamKindDto", "weight", "maximum", MAX_EXAM_KIND_WEIGHT),
        (
            "PatchExamWeight",
            "weight",
            "minimum",
            MIN_EXAM_KIND_WEIGHT,
        ),
        (
            "PatchExamWeight",
            "weight",
            "maximum",
            MAX_EXAM_KIND_WEIGHT,
        ),
        ("GradeBandDto", "min", "minimum", MIN_MARK),
        ("GradeBandDto", "min", "maximum", MAX_MARK),
        (
            "GradeBandDto",
            "label",
            "maxLength",
            MAX_GRADE_LABEL_LEN as i64,
        ),
        (
            "UpdateSettings",
            "exam_kinds",
            "maxItems",
            MAX_SETTINGS_LIST_LEN as i64,
        ),
        (
            "UpdateSettings",
            "attendance_statuses",
            "maxItems",
            MAX_SETTINGS_LIST_LEN as i64,
        ),
        (
            "UpdateSettings",
            "grade_bands",
            "maxItems",
            MAX_GRADE_BANDS as i64,
        ),
        (
            "UpdateSettings",
            "max_file_bytes",
            "minimum",
            MIN_MAX_FILE_BYTES,
        ),
        (
            "UpdateSettings",
            "max_file_bytes",
            "maximum",
            MAX_MAX_FILE_BYTES,
        ),
        (
            "UpdateSettings",
            "chatbot_history_turns",
            "minimum",
            MIN_CHATBOT_HISTORY_TURNS,
        ),
        (
            "UpdateSettings",
            "chatbot_history_turns",
            "maximum",
            MAX_CHATBOT_HISTORY_TURNS,
        ),
        (
            "UpdateSettings",
            "max_chatbot_threads",
            "minimum",
            MIN_MAX_CHATBOT_THREADS,
        ),
        (
            "UpdateSettings",
            "max_chatbot_threads",
            "maximum",
            MAX_MAX_CHATBOT_THREADS,
        ),
        (
            "UpdateSettings",
            "max_chatbot_message_len",
            "minimum",
            MIN_MAX_CHATBOT_MESSAGE_LEN,
        ),
        (
            "UpdateSettings",
            "max_chatbot_message_len",
            "maximum",
            MAX_MAX_CHATBOT_MESSAGE_LEN,
        ),
        // --- the devamsızlık axes: the branş vocabulary, what an absence may
        // be excused as, and the per-dönem day limits. The two lists are
        // school policy like `exam_kinds`, so they carry the same bounds; the
        // limits are days, bounded so a typo cannot disable the rule.
        (
            "UpdateSettings",
            "branches",
            "maxItems",
            MAX_SETTINGS_LIST_LEN as i64,
        ),
        (
            "UpdateSettings",
            "excuse_kinds",
            "maxItems",
            MAX_SETTINGS_LIST_LEN as i64,
        ),
        ("UpdateSettings", "max_excused_absent_days", "minimum", 0),
        (
            "UpdateSettings",
            "max_excused_absent_days",
            "maximum",
            MAX_ABSENCE_DAYS,
        ),
        ("UpdateSettings", "max_unexcused_absent_days", "minimum", 0),
        (
            "UpdateSettings",
            "max_unexcused_absent_days",
            "maximum",
            MAX_ABSENCE_DAYS,
        ),
        // The read side publishes the same two lists, and the bound has to
        // hold on both or a client validating a `GET` against them is wrong.
        (
            "SettingsResponse",
            "branches",
            "maxItems",
            MAX_SETTINGS_LIST_LEN as i64,
        ),
        (
            "SettingsResponse",
            "excuse_kinds",
            "maxItems",
            MAX_SETTINGS_LIST_LEN as i64,
        ),
        // --- food program (the school-editable lists live in /settings) ---
        (
            "MealSlotDto",
            "name",
            "maxLength",
            MAX_SETTINGS_ITEM_LEN as i64,
        ),
        ("MealSlotDto", "serving_minute", "minimum", 0),
        (
            "MealSlotDto",
            "serving_minute",
            "maximum",
            MAX_MEAL_SERVING_MINUTE,
        ),
        (
            "UpdateSettings",
            "meal_slots",
            "maxItems",
            MAX_SETTINGS_LIST_LEN as i64,
        ),
        (
            "UpdateSettings",
            "dietary_tags",
            "maxItems",
            MAX_SETTINGS_LIST_LEN as i64,
        ),
        ("UpdateSettings", "meal_cancel_cutoff_minutes", "minimum", 0),
        (
            "UpdateSettings",
            "meal_cancel_cutoff_minutes",
            "maximum",
            MAX_MEAL_CANCEL_CUTOFF_MINUTES,
        ),
        // --- menus and dishes ---
        // `date` is a fixed-width `YYYY-MM-DD`, so both length bounds are 10 —
        // the shape itself, not a constant anyone could tune.
        ("CreateMenu", "date", "minLength", 10),
        ("CreateMenu", "date", "maxLength", 10),
        ("CreateMenu", "capacity", "minimum", 0),
        ("CreateMenu", "capacity", "maximum", MAX_MENU_CAPACITY),
        ("UpdateMenu", "capacity", "minimum", 0),
        ("UpdateMenu", "capacity", "maximum", MAX_MENU_CAPACITY),
        ("CreateDish", "price_minor", "minimum", 0),
        ("UpdateDish", "price_minor", "minimum", 0),
        ("CreateDish", "name", "maxLength", MAX_DISH_NAME_LEN as i64),
        ("UpdateDish", "name", "maxLength", MAX_DISH_NAME_LEN as i64),
        (
            "CreateDish",
            "description",
            "maxLength",
            MAX_DISH_DESCRIPTION_LEN as i64,
        ),
        (
            "UpdateDish",
            "description",
            "maxLength",
            MAX_DISH_DESCRIPTION_LEN as i64,
        ),
        ("CreateDish", "price_minor", "maximum", MAX_DISH_PRICE_MINOR),
        ("UpdateDish", "price_minor", "maximum", MAX_DISH_PRICE_MINOR),
        ("CreateDish", "tags", "maxItems", MAX_DISH_TAGS as i64),
        ("UpdateDish", "tags", "maxItems", MAX_DISH_TAGS as i64),
        (
            "UpdateDietaryProfile",
            "tags",
            "maxItems",
            MAX_DIETARY_TAGS as i64,
        ),
        (
            "UpdateDietaryProfile",
            "note",
            "maxLength",
            MAX_DIETARY_NOTE_LEN as i64,
        ),
        ("RecordCredit", "amount_minor", "minimum", 1),
        (
            "RecordCredit",
            "amount_minor",
            "maximum",
            MAX_LEDGER_AMOUNT_MINOR,
        ),
        (
            "RecordCredit",
            "method",
            "maxLength",
            MAX_LEDGER_METHOD_LEN as i64,
        ),
        (
            "RecordCredit",
            "note",
            "maxLength",
            MAX_LEDGER_NOTE_LEN as i64,
        ),
        ("RecordCredit", "request_key", "minLength", 1),
        (
            "RecordCredit",
            "request_key",
            "maxLength",
            MAX_PAYMENT_REQUEST_KEY_LEN as i64,
        ),
        // --- school payments: fee plans and the payment ledger ---
        ("InstallmentBody", "amount_minor", "minimum", 1),
        (
            "InstallmentBody",
            "amount_minor",
            "maximum",
            MAX_LEDGER_AMOUNT_MINOR,
        ),
        ("InstallmentBody", "due_at", "minimum", 0),
        (
            "CreateFeePlan",
            "name",
            "maxLength",
            MAX_FEE_PLAN_NAME_LEN as i64,
        ),
        ("CreateFeePlan", "installments", "minItems", 1),
        (
            "CreateFeePlan",
            "installments",
            "maxItems",
            MAX_FEE_PLAN_INSTALLMENTS as i64,
        ),
        (
            "UpdateFeePlan",
            "name",
            "maxLength",
            MAX_FEE_PLAN_NAME_LEN as i64,
        ),
        ("UpdateFeePlan", "installments", "minItems", 1),
        (
            "UpdateFeePlan",
            "installments",
            "maxItems",
            MAX_FEE_PLAN_INSTALLMENTS as i64,
        ),
        (
            "AssignFeePlan",
            "student_ids",
            "maxItems",
            MAX_FEE_PLAN_ASSIGN_STUDENTS as i64,
        ),
        ("RecordPayment", "amount_minor", "minimum", 1),
        (
            "RecordPayment",
            "amount_minor",
            "maximum",
            MAX_LEDGER_AMOUNT_MINOR,
        ),
        (
            "RecordPayment",
            "method",
            "maxLength",
            MAX_LEDGER_METHOD_LEN as i64,
        ),
        (
            "RecordPayment",
            "note",
            "maxLength",
            MAX_LEDGER_NOTE_LEN as i64,
        ),
        ("RecordPayment", "request_key", "minLength", 1),
        (
            "RecordPayment",
            "request_key",
            "maxLength",
            MAX_PAYMENT_REQUEST_KEY_LEN as i64,
        ),
        ("RecordRefund", "amount_minor", "minimum", 1),
        (
            "RecordRefund",
            "amount_minor",
            "maximum",
            MAX_LEDGER_AMOUNT_MINOR,
        ),
        (
            "RecordRefund",
            "method",
            "maxLength",
            MAX_LEDGER_METHOD_LEN as i64,
        ),
        (
            "RecordRefund",
            "note",
            "maxLength",
            MAX_LEDGER_NOTE_LEN as i64,
        ),
        ("RecordRefund", "request_key", "minLength", 1),
        (
            "RecordRefund",
            "request_key",
            "maxLength",
            MAX_PAYMENT_REQUEST_KEY_LEN as i64,
        ),
        (
            "RecordReversal",
            "note",
            "maxLength",
            MAX_LEDGER_NOTE_LEN as i64,
        ),
        ("CreateTerm", "name", "maxLength", MAX_TERM_NAME_LEN as i64),
        ("UpdateTerm", "name", "maxLength", MAX_TERM_NAME_LEN as i64),
        (
            "SendChatbotMessage",
            "content",
            "maxLength",
            MAX_CHATBOT_MESSAGE_LEN as i64,
        ),
        // --- chatbot thread titles (constant promoted out of
        // domain/chatbot_thread.rs, where it escaped both surfaces) ---
        (
            "CreateChatbotThread",
            "title",
            "maxLength",
            MAX_CHATBOT_THREAD_TITLE_LEN as i64,
        ),
        (
            "RenameChatbotThread",
            "title",
            "maxLength",
            MAX_CHATBOT_THREAD_TITLE_LEN as i64,
        ),
        // --- the RAG nest reuses the chatbot's shapes and its school knobs:
        // the same title cap and the same per-message character cap ---
        (
            "CreateRagThread",
            "title",
            "maxLength",
            MAX_CHATBOT_THREAD_TITLE_LEN as i64,
        ),
        (
            "RenameRagThread",
            "title",
            "maxLength",
            MAX_CHATBOT_THREAD_TITLE_LEN as i64,
        ),
        (
            "SendRagMessage",
            "content",
            "maxLength",
            MAX_CHATBOT_MESSAGE_LEN as i64,
        ),
        // --- whiteboards ---
        (
            "CreateBoard",
            "title",
            "maxLength",
            MAX_BOARD_TITLE_LEN as i64,
        ),
        (
            "CreateBoard",
            "participants",
            "maxItems",
            MAX_BOARD_PARTICIPANTS as i64,
        ),
        (
            "UpdateBoard",
            "title",
            "maxLength",
            MAX_BOARD_TITLE_LEN as i64,
        ),
        (
            "UpdateBoard",
            "participants",
            "maxItems",
            MAX_BOARD_PARTICIPANTS as i64,
        ),
        // --- academic years and the class×course instance (the K12 anchor) ---
        // A year is named like a term ("2026-2027"), so it shares the term
        // name's bound; its two grade fields are şube rungs on the grade
        // ladder, bounded like `CreateClass.grade_level`.
        (
            "CreateYear",
            "name",
            "maxLength",
            MAX_ACADEMIC_YEAR_NAME_LEN as i64,
        ),
        (
            "UpdateYear",
            "name",
            "maxLength",
            MAX_ACADEMIC_YEAR_NAME_LEN as i64,
        ),
        (
            "PromotionBody",
            "from_grade",
            "minimum",
            MIN_GRADE_LEVEL as i64,
        ),
        (
            "PromotionBody",
            "from_grade",
            "maximum",
            MAX_GRADE_LEVEL as i64,
        ),
        (
            "PromotionBody",
            "to_grade",
            "minimum",
            MIN_GRADE_LEVEL as i64,
        ),
        (
            "PromotionBody",
            "to_grade",
            "maximum",
            MAX_GRADE_LEVEL as i64,
        ),
        // The instance's weekly hours: the karne weight, bounded so one
        // instance cannot be made to dominate the year average by a typo.
        ("UpdateInstance", "ders_saati", "minimum", MIN_DERS_SAATI),
        ("UpdateInstance", "ders_saati", "maximum", MAX_DERS_SAATI),
        // The holiday calendar's one free-text field, and the optional topic
        // a weekly-plan slot may stamp onto every lesson it generates.
        (
            "CreateHoliday",
            "name",
            "maxLength",
            MAX_HOLIDAY_NAME_LEN as i64,
        ),
        (
            "UpdateHoliday",
            "name",
            "maxLength",
            MAX_HOLIDAY_NAME_LEN as i64,
        ),
        ("CreateSlot", "topic", "maxLength", MAX_SESSION_TOPIC_LEN as i64),
        // The offering spine (course × grade_level templates) and the
        // per-class instance overrides: same bounds as the catalog course
        // they inherit from.
        ("CreateOffering", "grade_level", "minimum", MIN_GRADE_LEVEL as i64),
        ("CreateOffering", "grade_level", "maximum", MAX_GRADE_LEVEL as i64),
        ("CreateOffering", "title", "maxLength", MAX_COURSE_TITLE_LEN as i64),
        (
            "CreateOffering",
            "description",
            "maxLength",
            MAX_COURSE_DESCRIPTION_LEN as i64,
        ),
        ("CreateOffering", "default_ders_saati", "minimum", MIN_DERS_SAATI),
        ("CreateOffering", "default_ders_saati", "maximum", MAX_DERS_SAATI),
        ("UpdateOffering", "title", "maxLength", MAX_COURSE_TITLE_LEN as i64),
        (
            "UpdateOffering",
            "description",
            "maxLength",
            MAX_COURSE_DESCRIPTION_LEN as i64,
        ),
        ("UpdateOffering", "default_ders_saati", "minimum", MIN_DERS_SAATI),
        ("UpdateOffering", "default_ders_saati", "maximum", MAX_DERS_SAATI),
        ("UpdateInstance", "title", "maxLength", MAX_COURSE_TITLE_LEN as i64),
        (
            "UpdateInstance",
            "description",
            "maxLength",
            MAX_COURSE_DESCRIPTION_LEN as i64,
        ),
    ]
}

#[tokio::test]
async fn published_bounds_match_the_constants() {
    let spec = spec().await;
    let schemas = &spec["components"]["schemas"];

    let mut wrong = Vec::new();
    for (schema, field, keyword, want) in expectations() {
        let got = schemas[schema]["properties"][field][keyword].as_i64();
        if got != Some(want) {
            wrong.push(format!(
                "  {schema}.{field}: spec says {keyword}={got:?}, constant says {want}"
            ));
        }
    }

    assert!(
        wrong.is_empty(),
        "the OpenAPI spec disagrees with src/constant.rs — a client generating \
         validators from it would enforce the wrong rule:\n{}\n\nutoipa takes \
         literals only, so fix the `#[schema(...)]` attribute to match the constant.",
        wrong.join("\n")
    );
}

/// Every JSON-bodied operation must publish its `422`.
///
/// axum's `Json` extractor answers `422 Unprocessable Entity` whenever the body
/// parses as JSON but does not fit the DTO — a number where a string belongs, a
/// missing required field, or (on the three `deny_unknown_fields` DTOs) a key
/// the request does not accept. That is a status the server really returns on
/// *every* route taking a JSON body, so a generated client that has no case for
/// it will mishandle the most common client mistake there is. (`400` is a
/// different failure: a body that is not JSON at all, or one whose values are
/// well-typed but violate a domain rule — those already carry an `{error}`
/// payload and their own declarations.)
///
/// Hand-applying that declaration across ~24 files drifts the moment someone
/// adds a route, so the emitted document is what gets asserted, not the source:
/// a new JSON-bodied operation fails this test until it declares its `422`.
/// Multipart operations are out of scope on purpose — their rejection is
/// `400`, and they are checked by the same walk below.
#[tokio::test]
async fn every_json_body_operation_declares_422() {
    let spec = spec().await;

    let mut missing = Vec::new();
    let mut wrongly_declared = Vec::new();
    for (path, item) in spec["paths"].as_object().expect("paths").iter() {
        for (method, operation) in item.as_object().expect("path item").iter() {
            let Some(content) = operation["requestBody"]["content"].as_object() else {
                continue;
            };
            let declares_422 = operation["responses"]["422"].is_object();
            if content.contains_key("application/json") {
                if !declares_422 {
                    missing.push(format!("  {} {path}", method.to_uppercase()));
                }
            } else if declares_422 {
                wrongly_declared.push(format!(
                    "  {} {path} ({})",
                    method.to_uppercase(),
                    content.keys().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }

    assert!(
        missing.is_empty(),
        "{} operation(s) take a JSON body but do not declare the `422` axum answers \
         when that body does not fit the DTO — a generated client has no case for a \
         status the server really returns:\n{}\n\nAdd \
         `(status = 422, description = \"…\")` to each `responses(...)` block.",
        missing.len(),
        missing.join("\n")
    );
    assert!(
        wrongly_declared.is_empty(),
        "{} non-JSON operation(s) declare a `422` they never return — a malformed \
         multipart body is rejected with `400`:\n{}",
        wrongly_declared.len(),
        wrongly_declared.join("\n")
    );
}

#[tokio::test]
async fn every_page_limit_param_matches_its_constant() {
    // `#[param(maximum = 500)]` is written out beside MAX_PAGE_LIMIT, and
    // constant.rs merely *asks* a human to keep the two in step. This makes
    // that request enforceable.
    //
    // EVERY `limit` parameter, not the first one found: `src/web/page.rs` is
    // the shared one, but `src/web/users.rs` declares its own search paging
    // with the literal repeated. Checking one would have left the other free
    // to drift — which is exactly the bug this test was written to prevent.
    let spec = spec().await;

    let limits: Vec<_> = spec["paths"]
        .as_object()
        .expect("paths")
        .values()
        .filter_map(|path| path.as_object())
        .flat_map(|path| path.values())
        .filter_map(|op| op["parameters"].as_array())
        .flatten()
        .filter(|param| param["name"] == "limit")
        .collect();

    assert!(
        !limits.is_empty(),
        "no endpoint publishes a `limit` query parameter — the paging contract vanished"
    );

    let drifted: Vec<String> = limits
        .iter()
        .filter(|param| param["schema"]["maximum"].as_i64() != Some(MAX_PAGE_LIMIT))
        .map(|param| format!("  {:?}", param["schema"]["maximum"]))
        .collect();

    assert!(
        drifted.is_empty(),
        "{} of {} published `limit` parameters disagree with MAX_PAGE_LIMIT ({}):\n{}\n\n\
         Update the `#[param(maximum = ...)]` that drifted (src/web/page.rs is the shared \
         one; src/web/users.rs declares its own).",
        drifted.len(),
        limits.len(),
        MAX_PAGE_LIMIT,
        drifted.join("\n")
    );
}

/// Every `#[schema(...)]` bound in the web layer must have a row in
/// [`expectations`] — otherwise the table is itself a mirror that silently
/// lags, and a new annotation gets published with nothing checking it.
///
/// This is not hypothetical: the `CreateHomework.assigned` row was added by a
/// text substitution that silently matched nothing after rustfmt rewrapped the
/// anchor line. The suite stayed green with the row missing. This test is what
/// catches that class.
#[test]
fn expectations_cover_every_annotation() {
    let root = env!("CARGO_MANIFEST_DIR");
    let keywords = [
        "max_length",
        "min_length",
        "minimum",
        "maximum",
        "min_items",
        "max_items",
    ];

    let mut annotated = 0;
    // Walk sub-directories too: a resource split into `web/<x>/` (exams) keeps
    // its annotations, and a flat read_dir would silently stop counting them.
    let mut dirs = vec![format!("{root}/src/web")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).expect("read src/web") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                dirs.push(path.to_string_lossy().into_owned());
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("read source");
            // Scan whole attributes, not lines: rustfmt wraps a long
            // `#[schema(example = "…", max_length = 5_000)]` across several lines,
            // and a line-based count silently misses the wrapped keyword.
            //
            // `#[param(...)]` bounds are query parameters, covered by
            // `every_page_limit_param_matches_its_constant` instead.
            for (at, _) in source.match_indices("#[schema(") {
                let rest = &source[at..];
                let end = rest.find(")]").map_or(rest.len(), |end| end + 2);
                let attribute = &rest[..end];
                annotated += keywords
                    .iter()
                    .filter(|kw| attribute.contains(&format!("{kw} = ")))
                    .count();
            }
        }
    }

    assert_eq!(
        annotated,
        expectations().len(),
        "src/web/ carries {annotated} `#[schema(...)]` bounds but the expectations table \
         has {} rows — every annotation needs a row, or it is published with nothing \
         asserting it still matches its constant.",
        expectations().len()
    );
}

/// README.md quotes how many bounds this suite checks — a hand-kept copy of
/// [`expectations`]`.len()`, and one that had already gone stale twice before
/// anything watched it. This is that watcher.
#[test]
fn the_readme_states_the_real_bound_count() {
    // Anchored on the sentence itself, not a line number or a loose digit
    // scan: the README is 73KB and full of other numbers.
    const ANCHOR: &str = "`tests/spec_bounds.rs` builds the OpenAPI document, reads all ";
    let readme = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md"))
        .expect("read README.md");
    let at = readme.find(ANCHOR).unwrap_or_else(|| {
        panic!(
            "README.md no longer contains the sentence this test reads:\n  \"{ANCHOR}…\"\n\n\
             It states how many published bounds this suite checks ({} today). Restore that \
             wording, or move the count and re-anchor this test on its new phrasing.",
            expectations().len()
        )
    });
    let stated: String = readme[at + ANCHOR.len()..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();

    assert_eq!(
        stated.parse::<usize>().ok(),
        Some(expectations().len()),
        "README.md says this suite checks {stated:?} published bounds, but the expectations \
         table has {} rows.\n\nEdit README.md so that sentence reads \"reads all {} published \
         bounds\" — the table is the truth, the README is the copy.",
        expectations().len(),
        expectations().len()
    );
}

/// The three image-meta bodies — exam question images, bank template images,
/// and pool question/solution photos — are hand-kept copies of one shape,
/// `{content_type, size}`. The write half behind them is shared
/// (`src/web/image.rs`), but the DTOs stay three types because their *names*
/// are the published contract, so nothing but this comparison of the emitted
/// JSON stops one of them from quietly growing a field the others lack.
#[tokio::test]
async fn the_image_meta_bodies_stay_the_same_shape() {
    let spec = spec().await;
    let schemas = &spec["components"]["schemas"];

    // Field names with their JSON type, prose (description/example) stripped —
    // the wording is allowed to differ per system, the shape is not.
    let shape = |name: &str| -> Vec<String> {
        let properties = schemas[name]["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{name} is not an object schema — was it renamed?"));
        let mut fields: Vec<String> = properties
            .iter()
            .map(|(field, spec)| format!("{field}: {}/{}", spec["type"], spec["format"]))
            .collect();
        fields.sort();
        let mut required: Vec<String> = schemas[name]["required"]
            .as_array()
            .map(|items| items.iter().map(|item| item.to_string()).collect())
            .unwrap_or_default();
        required.sort();
        fields.push(format!("required: {required:?}"));
        fields
    };

    let want = shape("ImageMetaResponse");
    for other in ["BankImageMeta", "PoolImageMeta", "ProfileAvatar"] {
        assert_eq!(
            want,
            shape(other),
            "{other} has drifted from ImageMetaResponse — the image-meta bodies are \
             copies of one shape and must be changed together"
        );
    }
}

/// The class refusal codes are a **published** vocabulary: a blueprint pump
/// reports them as a skip `reason`, and the two manual attach routes answer
/// them as the `code` on their `409`. A client branches on them *per route*,
/// and the two ceiling codes are not the same on the two attach axes — a full
/// roster and a full course list are different refusals — so this asserts each
/// route's `409` names **exactly** its own set, and that the whole vocabulary
/// is accounted for on the `SkipResponse.reason` field that documents it.
///
/// Anywhere-in-the-document containment was the earlier shape and could not
/// fail: `course_full` survived as a `#[schema(example)]` and `duplicate` in
/// prose, so a code dropped from a `409` description, moved to the wrong route
/// or added undocumented all passed.
///
/// `course_full` itself is gone with the seat claim: an instance no longer
/// holds a capacity, so a full course is not a refusal any more.
#[tokio::test]
async fn the_class_refusal_codes_stay_published() {
    let spec = spec().await;

    // Every backticked snake_case word in a description, minus the terms that
    // are not codes — so a code added to (or moved into) a description shows up
    // here without the test being told about it.
    let quoted = |text: &str| -> Vec<String> {
        text.split('`')
            .skip(1)
            .step_by(2)
            .filter(|word| {
                word.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                    && !matches!(*word, "code" | "max_class_members" | "max_class_courses")
            })
            .map(str::to_string)
            .collect()
    };
    let sorted = |mut codes: Vec<String>| -> Vec<String> {
        codes.sort();
        codes.dedup();
        codes
    };
    let want = |codes: &[&str]| sorted(codes.iter().map(|c| c.to_string()).collect());

    // The pump's own half, on the field that carries it. `Attached::refusal_code`
    // answers these on the course axis, which is the only axis a pump runs.
    let pump = [
        "class_deleted",
        "course_deleted",
        "class_at_course_ceiling",
        "class_roster_too_large",
        "blueprint_deleted",
    ];
    // Per route, in the axis's own words: `class_at_*_ceiling` is the axis being
    // attached, the `*_too_large` pair the other one. The instance route is
    // where a course is attached to a section now; the second path segment of
    // its `DELETE` is the instance id, not a course id.
    let instances = [
        "duplicate",
        "class_at_course_ceiling",
        "class_roster_too_large",
        "academic_year_archived",
    ];
    let members = [
        "duplicate",
        "class_at_roster_ceiling",
        "class_course_list_too_large",
        "linked_course_missing",
        "academic_year_archived",
    ];
    // `academic_year_archived` refuses the whole request up front (the class's
    // academic year is archived), so no pump ever *skips* an item for it — it
    // belongs on the two routes' 409s, never in `SkipResponse.reason`.
    let whole_request = ["academic_year_archived"];

    for (path, expected) in [
        ("/classes/{id}/instances", instances.as_slice()),
        ("/classes/{id}/members", members.as_slice()),
    ] {
        let description = spec["paths"][path]["post"]["responses"]["409"]["description"]
            .as_str()
            .unwrap_or_else(|| panic!("{path} must declare a 409 with a description"));
        assert_eq!(
            sorted(quoted(description)),
            want(expected),
            "the `409` on POST {path} must name exactly the refusal codes that route \
             answers — a client branches on them, and one it never sees documented (or \
             one it is promised and never gets) is a branch written against nothing"
        );
    }

    // …and the closed set itself, on the one field that explains all of it: the
    // union of both routes plus the pump, so a code documented on one surface
    // but missing from the field that collects them all still fails here.
    //
    // What this cannot catch: a code this file has never heard of. The three
    // lists above are hand-kept, so a new `Attached` variant reaches the API
    // documented nowhere and every assertion here still passes. The compiler
    // holds that end instead — `refusal_code`'s match is exhaustive, so the
    // variant cannot be added without a code — and
    // `class_blueprint::tests::each_axis_names_the_ceiling_it_actually_hit`
    // pins what that code is per axis. Adding one means editing both.
    let reason =
        spec["components"]["schemas"]["SkipResponse"]["properties"]["reason"]["description"]
            .as_str()
            .expect("SkipResponse.reason must document the code vocabulary");
    let everything: Vec<&str> = pump
        .iter()
        .chain(instances.iter())
        .chain(members.iter())
        .copied()
        .filter(|code| !whole_request.contains(code))
        .collect();
    assert_eq!(
        sorted(quoted(reason)),
        want(&everything),
        "`SkipResponse.reason` is where the whole closed vocabulary is written down — \
         every code either route or a pump can answer must appear there, and nothing else"
    );
}
