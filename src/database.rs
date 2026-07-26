//! SurrealDB connection (WebSocket to a server; in-memory for tests) + schema.

use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::opt::auth::Root;

use crate::config::Config;
use crate::constant::{BACKFILL, MIGRATION, PRE_REPAIR};
use crate::error::AppError;

/// The shared database handle.
///
/// `Arc` is load-bearing, not decoration. `Surreal`'s own `Clone` mints a
/// fresh server-side session per clone (`Uuid::new_v4()` + a fire-and-forget
/// `SessionId::Clone` to the router, SDK `lib.rs:340`), and axum clones the
/// application state on every request — so a bare `Surreal<Any>` here means a
/// new session, and a matching `SessionId::Drop`, for each request served.
/// Under concurrency those lifecycle events race the queries riding on them:
/// sessions disappear ("Session not found"), and a session whose signin has
/// not been replayed answers "Anonymous access not allowed" / "Specify a
/// namespace" — the same strings [`crate::error::is_session_replay_error`]
/// treats as a transient reconnect, so the failures masquerade as one.
///
/// Cloning the `Arc` shares the single session established at boot instead.
pub type Database = std::sync::Arc<Surreal<Any>>;

/// Connect to the SurrealDB server, sign in as root, and apply the schema.
pub async fn init(cfg: &Config) -> Result<Database, AppError> {
    let db = connect_with_retry(cfg).await;
    db.signin(Root {
        username: cfg.db_user.clone(),
        password: cfg.db_pass.clone(),
    })
    .await?;
    db.use_ns(cfg.db_ns.clone())
        .use_db(cfg.db_name.clone())
        .await?;
    migrate(&db).await?;
    Ok(std::sync::Arc::new(db))
}

/// Dial the database, retrying until it answers.
///
/// Never gives up, because giving up is worse than waiting: the database is
/// normally a sibling container booting in parallel, and a process that exits
/// on the first refusal gets restarted by the runtime, fails again in ~100ms,
/// and burns its whole restart budget inside a second — leaving the backend
/// permanently down over a startup skew that would have cleared on its own.
///
/// Only the dial is retried. Bad credentials or a broken migration still fail
/// hard, since no amount of waiting fixes those.
async fn connect_with_retry(cfg: &Config) -> Surreal<Any> {
    let mut backoff = 1;
    loop {
        match surrealdb::engine::any::connect(cfg.db_url.clone()).await {
            Ok(db) => return db,
            Err(err) => {
                tracing::warn!(
                    "database at {} unreachable ({err}) — retrying in {backoff}s",
                    cfg.db_url
                );
                tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(crate::constant::DB_CONNECT_BACKOFF_MAX_SECS);
            }
        }
    }
}

/// A fresh in-memory database with the schema applied. For tests.
pub async fn init_mem() -> Result<Database, AppError> {
    let db = surrealdb::engine::any::connect("memory").await?;
    db.use_ns("hezarfen").use_db("hezarfen").await?;
    migrate(&db).await?;
    Ok(std::sync::Arc::new(db))
}

/// Apply the schema + backfills. Idempotent — `init` runs it on every boot,
/// and tests re-run it on a live handle to simulate a second boot.
pub async fn migrate(db: &Surreal<Any>) -> Result<(), AppError> {
    db.query(PRE_REPAIR).await?.check()?;
    db.query(MIGRATION).await?.check()?;
    db.query(BACKFILL).await?.check()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn boot_fails_chatbot_messages_left_pending() {
        let db = super::init_mem().await.unwrap();
        db.query(
            "CREATE user:u SET username = 'u', password_hash = 'x';
             CREATE chatbot_thread:c SET user_id = user:u, created_at = 1, updated_at = 1;
             CREATE chatbot_message:m SET thread_id = chatbot_thread:c, user_id = user:u,
                 role = 'assistant', content = '', status = 'pending', created_at = 1;
             CREATE chatbot_message:done SET thread_id = chatbot_thread:c, user_id = user:u,
                 role = 'assistant', content = 'hi', status = 'complete', created_at = 1;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

        // A second boot: the task that would have answered `m` died with the
        // previous process, so the row is failed rather than left waiting.
        super::migrate(&db).await.unwrap();

        let mut rows = db
            .query("SELECT id, status, error_code, completed_at FROM chatbot_message ORDER BY id")
            .await
            .unwrap();
        let rows: Vec<serde_json::Value> = rows.take(0).unwrap();
        // An already-answered turn is untouched; the pending one is failed.
        let done = &rows[0];
        let interrupted = &rows[1];
        assert_eq!(done["status"], "complete");
        assert_eq!(done["error_code"], serde_json::Value::Null);
        assert_eq!(interrupted["status"], "failed");
        assert_eq!(interrupted["error_code"], "interrupted");
        assert!(interrupted["completed_at"].as_i64().unwrap() > 0);
    }

    /// A row written before choices had ids survives the retyped columns: it is
    /// converted, it is writable again, and its stored *position* still names
    /// the same option — a wrong remap here silently regrades exams.
    #[tokio::test]
    async fn boot_converts_positional_choices_and_keeps_the_same_option() {
        let db = super::init_mem().await.unwrap();
        // Roll the columns back to their pre-2026-07-24 shapes and write the
        // kind of row an older binary left behind, `source_bank` included.
        db.query(
            "REMOVE FIELD IF EXISTS choices[*].id ON TABLE exam_question;
             REMOVE FIELD IF EXISTS choices[*].text ON TABLE exam_question;
             REMOVE FIELD IF EXISTS choices[*].id ON TABLE bank_question;
             REMOVE FIELD IF EXISTS choices[*].text ON TABLE bank_question;
             DEFINE FIELD OVERWRITE choices ON exam_question TYPE option<array<string>>;
             DEFINE FIELD OVERWRITE correct ON exam_question TYPE option<int>;
             DEFINE FIELD OVERWRITE source_bank ON exam_question TYPE option<string>;
             DEFINE FIELD OVERWRITE slot ON question_image TYPE option<int>;
             DEFINE FIELD OVERWRITE selected ON exam_answer TYPE option<int>;
             DEFINE FIELD OVERWRITE choices ON bank_question TYPE option<array<string>>;
             DEFINE FIELD OVERWRITE correct ON bank_question TYPE option<int>;
             DEFINE FIELD OVERWRITE slot ON bank_question_image TYPE option<int>;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
        db.query(
            "CREATE user:u SET username = 'u', password_hash = 'x';
             CREATE exam_question:q SET exam = exam:e, text = 'q', kind = 'multiple',
                 points = 5, subject = subject:s, choices = ['3', '4', '5'], correct = 1,
                 source_bank = 'bank_question:b';
             CREATE question_image:i SET exam = exam:e, question = exam_question:q, slot = 2,
                 file = 'f', content_type = 'image/png', size = 1;
             CREATE question_image:whole SET exam = exam:e, question = exam_question:q,
                 file = 'w', content_type = 'image/png', size = 1;
             CREATE exam_answer:a SET exam = exam:e, question = exam_question:q, user = user:u,
                 selected = 2, updated_at = 1;
             CREATE bank_question:b SET owner = user:u, text = 'b', kind = 'multiple',
                 points = 3, choices = ['a', 'b'], correct = 0, created_at = 1;
             CREATE bank_question_image:bi SET bank_question = bank_question:b, slot = 1,
                 file = 'bf', content_type = 'image/png', size = 1;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

        super::migrate(&db).await.unwrap();

        let mut res = db
            .query(
                "SELECT choices, correct FROM ONLY exam_question:q;
                 SELECT VALUE slot FROM ONLY question_image:i;
                 SELECT VALUE slot FROM ONLY question_image:whole;
                 SELECT VALUE selected FROM ONLY exam_answer:a;
                 SELECT choices, correct FROM ONLY bank_question:b;
                 SELECT VALUE slot FROM ONLY bank_question_image:bi;",
            )
            .await
            .unwrap();
        let q: Option<serde_json::Value> = res.take(0).unwrap();
        let q = q.unwrap();
        let img_slot: Option<String> = res.take(1).unwrap();
        let whole_slot: Option<String> = res.take(2).unwrap();
        let selected: Option<String> = res.take(3).unwrap();
        let bank: Option<serde_json::Value> = res.take(4).unwrap();
        let bank = bank.unwrap();
        let bank_slot: Option<String> = res.take(5).unwrap();

        let ids: Vec<&str> = q["choices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            q["choices"][0]["text"], "3",
            "option order (and so the meaning of every stored index) is preserved"
        );
        assert_eq!(q["correct"], ids[1], "correct was index 1");
        assert_eq!(
            img_slot.as_deref(),
            Some(ids[2]),
            "option picture was slot 2"
        );
        assert_eq!(selected.as_deref(), Some(ids[2]), "the answer was index 2");
        assert_eq!(whole_slot, None, "a question-level picture has no index");
        let bank_ids: Vec<&str> = bank["choices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect();
        assert_eq!(bank["correct"], bank_ids[0]);
        assert_eq!(bank_slot.as_deref(), Some(bank_ids[1]));

        // Writable again — every one of these failed coercion before the
        // backfill existed, and `source_bank` outlived its own column.
        db.query(
            "UPDATE exam_question:q SET points = 7;
             UPDATE question_image:i SET size = 2;
             UPDATE exam_answer:a SET updated_at = 2;
             UPDATE bank_question:b SET points = 4;
             UPDATE bank_question_image:bi SET size = 2;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

        // A second boot re-mints nothing.
        super::migrate(&db).await.unwrap();
        let mut res = db
            .query("SELECT VALUE choices.map(|$c| $c.id) FROM ONLY exam_question:q")
            .await
            .unwrap();
        let again: Vec<String> = res.take(0).unwrap();
        assert_eq!(again, ids, "conversion is idempotent");
    }
}
