//! SurrealDB connection (WebSocket to a server; in-memory for tests) + schema.

use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::opt::auth::Root;

use crate::config::Config;
use crate::constant::CHATBOT_PENDING_STALE_SECS;
use crate::domain::user::{Password, User, Username};
use crate::error::AppError;
use crate::migration_sql::MIGRATION_BATCHES;

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

/// Connect to the SurrealDB server, sign in as root, apply the schema and seed
/// the admin. One process, one database, stop-the-world deploys — so all of it
/// runs unconditionally on every boot.
pub async fn init(cfg: &Config) -> Result<Database, AppError> {
    // Validated before the connection, not after: half a credential pair is a
    // deployment mistake, and reporting it only after a successful dial buries
    // it under an outage that isn't one.
    let admin = admin_credentials(cfg)?;
    let db = connect_with_retry(cfg).await;
    db.signin(Root {
        username: cfg.db_user.clone(),
        password: cfg.db_pass.clone(),
    })
    .await?;
    db.use_ns(cfg.db_ns.clone())
        .use_db(cfg.db_name.clone())
        .await?;
    let db = std::sync::Arc::new(db);
    migrate(&db).await?;
    if let Some((username, password)) = admin {
        User::ensure_admin(username, password, &db).await?;
    }
    Ok(db)
}

/// The `ADMIN_USERNAME` / `ADMIN_PASSWORD` bootstrap pair, if configured.
/// Both-or-neither: half a pair aborts startup rather than silently running
/// without the seed.
fn admin_credentials(cfg: &Config) -> Result<Option<(Username, Password)>, AppError> {
    match (&cfg.admin_username, &cfg.admin_password) {
        (Some(username), Some(password)) => Ok(Some((
            Username::try_new(username)
                .map_err(|err| AppError::Internal(format!("invalid ADMIN_USERNAME: {err}")))?,
            Password::try_new(password)
                .map_err(|err| AppError::Internal(format!("invalid ADMIN_PASSWORD: {err}")))?,
        ))),
        (None, None) => Ok(None),
        _ => Err(AppError::Internal(
            "ADMIN_USERNAME and ADMIN_PASSWORD must be set together".into(),
        )),
    }
}

/// The two ways a rival's claim shows up as an error on ours: our `CREATE` hit
/// their row, or the key-value layer aborted our transaction because theirs
/// touched the same key first.
///
/// Read from the *typed* error surface, not the message. Every conflict wording
/// the storage engines produce — surrealkv's "Transaction conflict: Write
/// conflict, retry the transaction. This transaction can be retried",
/// surrealmx's read-conflict variant, RocksDB's `Busy`/`TryAgain` — funnels
/// through one `KvsError::TransactionConflict` into
/// `QueryError::TransactionConflict` (surrealdb-core 3.2.3
/// `err/to_types.rs:295`, `kvs/err.rs:64-179`), and a duplicate record id into
/// `AlreadyExistsError::Record` (`to_types.rs:206`). So the two checks below
/// cover all of them, where matching substrings covered only whichever wording
/// the engine of the day happened to use.
///
/// The message fallback stays for one reason: these errors also travel over the
/// WebSocket, where the SDK rebuilds them from the wire (`Error::from_parts`),
/// and a server that omitted the typed details would otherwise turn a lost race
/// into a failed boot. Widest known wordings, cheap, and belt-and-braces.
pub(crate) fn lost_the_race(err: &surrealdb::Error) -> bool {
    use surrealdb::types::{ErrorDetails, QueryError};
    if err.is_already_exists()
        || matches!(
            err.details(),
            ErrorDetails::Query(Some(QueryError::TransactionConflict))
        )
    {
        return true;
    }
    let message = err.to_string();
    message.contains("already exists") || message.contains("can be retried")
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
    for sql in MIGRATION_BATCHES {
        let mut query = db.query(sql);
        for (name, value) in migration_binds() {
            query = query.bind((name, value));
        }
        query.await?.check()?;
    }
    Ok(())
}

/// Parameters the batches are executed with. Bound, not baked into the SQL
/// string: a `const` cannot be interpolated into another `const`, and a
/// hand-copied 300000 would drift silently.
fn migration_binds() -> [(&'static str, i64); 1] {
    [("stale_ms", CHATBOT_PENDING_STALE_SECS * 1_000)]
}

#[cfg(test)]
mod tests {

    /// The admin seed must not be interruptible into a state no later boot can
    /// repair. `create` + `set_role` was: killed between the two writes it
    /// leaves a *student* row holding ADMIN_USERNAME, which `ensure_admin` then
    /// refuses to promote on every subsequent boot (rightly — it cannot tell
    /// that row from a stranger who registered the name first). The row is
    /// therefore minted with its role in one statement, and this pins that.
    #[test]
    fn the_admin_seed_mints_the_role_with_the_row() {
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/domain/user.rs"))
                .expect("read user.rs");
        let start = source
            .find("pub async fn ensure_admin")
            .expect("ensure_admin is declared");
        let body = &source[start..start + source[start..].find("\n    }\n").expect("body ends")];
        assert!(
            !body.contains("set_role"),
            "ensure_admin promotes after creating; an interruption between those two writes \
             leaves a student account on ADMIN_USERNAME that no boot can heal:\n{body}"
        );
        assert!(
            body.contains("create_with_role"),
            "the seed must mint the row with Role::Admin in one statement"
        );
    }

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

    #[tokio::test]
    async fn the_retired_claim_columns_are_dropped_off_a_live_row() {
        // Stale-data path for the 2026-07-30 removal of the chat claim queue.
        // A dev volume carries `chatbot_message` rows *with* claim values and an
        // `ai_worker` table, so the boot has to clear the values while their
        // definitions still stand (PRE_REPAIR) and only then retire the columns
        // — SCHEMAFULL rejects every write to a row storing a column that no
        // longer exists, which would make the row unanswerable forever.
        let db = super::init_mem().await.unwrap();
        db.query(
            "DEFINE FIELD claimed_by ON chatbot_message TYPE option<string>;
             DEFINE FIELD claimed_at ON chatbot_message TYPE option<int>;
             DEFINE TABLE ai_worker SCHEMAFULL;
             DEFINE FIELD seen_at ON ai_worker TYPE int;
             DEFINE TABLE migration_lock SCHEMALESS;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();
        db.query(
            "CREATE user:u SET username = 'u', password_hash = 'x';
             CREATE chatbot_thread:c SET user_id = user:u, created_at = 1, updated_at = 1;
             CREATE chatbot_message:m SET thread_id = chatbot_thread:c, user_id = user:u,
                 role = 'assistant', content = '', status = 'pending',
                 created_at = time::unix(time::now()) * 1000,
                 claimed_by = 'a-dead-replica', claimed_at = 1;
             CREATE ai_worker:w SET seen_at = 1;
             CREATE migration_lock:the_lock SET holder = 'a-dead-replica', expires_at = 1;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

        // Twice: the repair and both removals have to be idempotent, since
        // every boot runs them.
        super::migrate(&db).await.unwrap();
        super::migrate(&db).await.unwrap();

        let mut info = db.query("INFO FOR TABLE chatbot_message").await.unwrap();
        let table: serde_json::Value = info.take::<Option<serde_json::Value>>(0).unwrap().unwrap();
        let fields = table["fields"].as_object().expect("fields");
        assert!(!fields.contains_key("claimed_by"), "{table}");
        assert!(!fields.contains_key("claimed_at"), "{table}");
        let mut tables = db.query("INFO FOR DB").await.unwrap();
        let dbinfo: serde_json::Value = tables
            .take::<Option<serde_json::Value>>(0)
            .unwrap()
            .unwrap();
        let tables = dbinfo["tables"].as_object().expect("tables");
        assert!(!tables.contains_key("ai_worker"), "{dbinfo}");
        // Same story one table over: the elected-boot lease, stale leader row
        // and all.
        assert!(!tables.contains_key("migration_lock"), "{dbinfo}");

        // The load-bearing half: the row that carried a claim is still
        // writable. A leftover value under no definition fails here.
        db.query("UPDATE chatbot_message:m SET status = 'complete', content = 'ok'")
            .await
            .unwrap()
            .check()
            .unwrap();
        let mut row = db
            .query("SELECT * FROM ONLY chatbot_message:m")
            .await
            .unwrap();
        let row: serde_json::Value = row.take::<Option<serde_json::Value>>(0).unwrap().unwrap();
        assert_eq!(row["status"], "complete", "{row}");
        assert!(row.get("claimed_by").is_none(), "{row}");
    }

    /// The other half of the sweep: a restart seconds after a dispatch must not
    /// kill the turn — the answering task may still be alive on the other side
    /// of the bridge, and the row is not yet past the stale horizon.
    #[tokio::test]
    async fn boot_spares_a_chatbot_message_pending_since_just_now() {
        let db = super::init_mem().await.unwrap();
        db.query(
            "CREATE user:u SET username = 'u', password_hash = 'x';
             CREATE chatbot_thread:c SET user_id = user:u, created_at = 1, updated_at = 1;
             CREATE chatbot_message:fresh SET thread_id = chatbot_thread:c, user_id = user:u,
                 role = 'assistant', content = '', status = 'pending',
                 created_at = time::unix(time::now()) * 1000 - 2000;
             CREATE chatbot_message:old SET thread_id = chatbot_thread:c, user_id = user:u,
                 role = 'assistant', content = '', status = 'pending',
                 created_at = time::unix(time::now()) * 1000
                     - ($stale + 1) * 1000;",
        )
        .bind(("stale", crate::constant::CHATBOT_PENDING_STALE_SECS))
        .await
        .unwrap()
        .check()
        .unwrap();

        super::migrate(&db).await.unwrap();

        let mut res = db
            .query(
                "SELECT status, error_code FROM ONLY chatbot_message:fresh;
                 SELECT VALUE status FROM ONLY chatbot_message:old;",
            )
            .await
            .unwrap();
        let fresh: Option<serde_json::Value> = res.take(0).unwrap();
        let fresh = fresh.unwrap();
        let old: Option<String> = res.take(1).unwrap();
        assert_eq!(
            fresh["status"], "pending",
            "a two-second-old turn is still being answered"
        );
        assert_eq!(fresh["error_code"], serde_json::Value::Null);
        assert_eq!(old.as_deref(), Some("failed"), "past the horizon it goes");
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
