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
    let db = database::init_mem().await.expect("in-memory db");
    let app = build_router(AppState {
        db,
        files_path: tempfile::tempdir().expect("files dir").keep(),
        cookie_secure: false,
        rate_limit: hezarfen_backend::rate_limit::RateLimitConfig::unlimited(),
        chatbot_limit: Default::default(),
        exam_presence: Default::default(),
        db_up: Default::default(),
        ai: None,
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
        // --- auth / users / notes / messages ---
        (
            "Credentials",
            "username",
            "minLength",
            MIN_USERNAME_LEN as i64,
        ),
        (
            "Credentials",
            "username",
            "maxLength",
            MAX_USERNAME_LEN as i64,
        ),
        (
            "Credentials",
            "password",
            "minLength",
            MIN_PASSWORD_LEN as i64,
        ),
        (
            "Credentials",
            "password",
            "maxLength",
            MAX_PASSWORD_LEN as i64,
        ),
        ("UpdateProfile", "name", "maxLength", MAX_NAME_LEN as i64),
        ("UpdateProfile", "surname", "maxLength", MAX_NAME_LEN as i64),
        ("UpdateProfile", "email", "maxLength", MAX_EMAIL_LEN as i64),
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
    for other in ["BankImageMeta", "PoolImageMeta"] {
        assert_eq!(
            want,
            shape(other),
            "{other} has drifted from ImageMetaResponse — the image-meta bodies are \
             copies of one shape and must be changed together"
        );
    }
}
