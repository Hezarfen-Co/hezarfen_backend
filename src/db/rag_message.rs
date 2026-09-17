//! The `rag_message` table: the turns of a thread, both sides persisted.
//! The user's question lands `complete`, and the assistant's row is written
//! `pending` *before* the AI call so a reload never loses an answer in
//! flight. The task that owns the bridge round trip then flips it to
//! `complete` (with the text and the service's reply) or `failed` (with a
//! code), and the boot sweep in `database.rs` fails whatever a process death
//! left behind past the staleness window. Every turn is written *through* its
//! thread's own row — its guarded statement bumps `updated_at`, so a turn can
//! never outlive the thread it belongs to. The row shape lives in
//! [`crate::domain::rag_message`].

use crate::constant::MAX_ERROR_CODE_LEN;
use crate::database::Database;
use crate::db::page::PagedList;
use crate::domain::rag_message::{
    RagContent, RagMessage, RagMessageId, RagMessageRole, RagMessageStatus, RagReply,
};
use crate::domain::rag_thread::RagThreadId;
use crate::domain::timestamp::Timestamp;
use crate::domain::user::UserId;
use crate::error::AppError;
use sqlx::query_as;
use sqlx::types::Json;

/// Write one turn *through its thread's own row*: the thread's activity
/// bump and the turn's insert are a single guarded statement, which is what
/// makes the thread's existence something this write writes rather than
/// something the caller read and then trusted — a
/// [`delete`](crate::db::rag_thread::delete) racing it touches the very
/// row this statement's gate updates, so under Postgres's row locking the
/// two serialize and no turn is left under a thread that is gone. It also
/// carries the thread's activity stamp, so a turn and its `updated_at` land
/// together. The stamp is written strictly upwards —
/// `GREATEST($now, updated_at + 1)` — because a turn's two rows routinely
/// land inside one millisecond and `updated_at` is the thread list's
/// ordering key.
///
/// [`AppError::NotFound`] = the thread is gone, and nothing was written.
async fn insert(db: &Database, message: RagMessage) -> Result<RagMessage, AppError> {
    let inserted = query_as!(
        RagMessage,
        "WITH touch AS (
             UPDATE rag_thread SET updated_at = GREATEST($2, updated_at + 1)
             WHERE id = $1
             RETURNING 1)
         INSERT INTO rag_message (id, thread, user_id, role, content, status, \
                                  reply, error_code, created_at, completed_at)
         SELECT $3, $1, $4, $5, $6, $7, $8, $9, $10, $11
         WHERE EXISTS (SELECT 1 FROM touch)
         RETURNING id AS \"id: RagMessageId\", thread AS \"thread: RagThreadId\", \
                   user_id AS \"user_id: UserId\", role AS \"role: RagMessageRole\", content AS \"content: RagContent\", \
                   status AS \"status: RagMessageStatus\", reply AS \"reply: Json<RagReply>\", \
                   error_code AS \"error_code: String\", \
                   created_at AS \"created_at: Timestamp\", completed_at AS \"completed_at: Timestamp\"",
        message.get_thread_id().uuid(),
        Timestamp::now().as_millis(),
        message.get_id().uuid(),
        message.user_id.uuid(),
        message.role.as_str(),
        message.content.as_str(),
        message.status.as_str(),
        message.reply as _,
        message.error_code,
        message.created_at.as_millis(),
        message.completed_at.map(|t| t.as_millis()),
    )
    .fetch_optional(db)
    .await?;
    // No row out of the conditional insert means the thread's touch matched
    // nothing: the thread is gone, and nothing was written.
    inserted.ok_or(AppError::NotFound)
}

/// Append the user's question. Nothing is awaited for it, so it is born
/// complete — and it carries no reply: there is nothing but the text.
pub async fn append_user(
    db: &Database,
    thread: &RagThreadId,
    user: &UserId,
    content: RagContent,
) -> Result<RagMessage, AppError> {
    let now = Timestamp::now();
    insert(
        db,
        RagMessage {
            id: RagMessageId::generate(),
            thread: thread.clone(),
            user_id: *user,
            role: RagMessageRole::User,
            content,
            status: RagMessageStatus::Complete,
            reply: None,
            error_code: None,
            created_at: now,
            completed_at: Some(now),
        },
    )
    .await
}

/// Reserve the assistant's answer *before* the AI call: the row exists,
/// empty and `pending`, so a reload finds the turn and can wait on it. The
/// reply column stays NULL until the answer lands — the service's JSON is not
/// known yet.
pub async fn append_pending_assistant(
    db: &Database,
    thread: &RagThreadId,
    user: &UserId,
) -> Result<RagMessage, AppError> {
    insert(
        db,
        RagMessage {
            id: RagMessageId::generate(),
            thread: thread.clone(),
            user_id: *user,
            role: RagMessageRole::Assistant,
            content: RagContent::empty(),
            status: RagMessageStatus::Pending,
            reply: None,
            error_code: None,
            created_at: Timestamp::now(),
            completed_at: None,
        },
    )
    .await
}

/// The whole thread, oldest first — the order both the UI and the AI
/// history payload read in.
pub async fn list_for_thread(
    db: &Database,
    thread: &RagThreadId,
    limit: Option<i64>,
    offset: i64,
) -> Result<(Vec<RagMessage>, i64), AppError> {
    let (rows, total) = PagedList::new(
        "rag_message WHERE thread = $1",
        "ORDER BY created_at ASC, id ASC",
    )
    .bind(thread.uuid())
    .run::<RagMessage>(limit, offset, db)
    .await?;
    Ok((rows.into_iter().map(RagMessage::projected).collect(), total))
}

/// The last `limit` turns, still oldest-first — the tail replayed to the
/// AI service as context. Taken newest-first in the database (so the
/// `LIMIT` keeps the *recent* end) and flipped back here.
pub async fn list_tail(
    db: &Database,
    thread: &RagThreadId,
    limit: usize,
) -> Result<Vec<RagMessage>, AppError> {
    let mut messages: Vec<RagMessage> = query_as!(
        RagMessage,
        "SELECT id AS \"id: RagMessageId\", thread AS \"thread: RagThreadId\", \
                user_id AS \"user_id: UserId\", role AS \"role: RagMessageRole\", content AS \"content: RagContent\", \
                status AS \"status: RagMessageStatus\", reply AS \"reply: Json<RagReply>\", \
                error_code AS \"error_code: String\", \
                created_at AS \"created_at: Timestamp\", completed_at AS \"completed_at: Timestamp\" \
         FROM rag_message WHERE thread = $1 \
         ORDER BY created_at DESC, id DESC LIMIT $2",
        thread.uuid(),
        limit as i64
    )
    .fetch_all(db)
    .await?;
    messages.reverse();
    Ok(messages)
}

/// The last `limit` *settled* turns that carry text, still oldest-first —
/// the tail replayed to the AI service as context. The filter runs in the
/// query, not after it: a `LIMIT` over the raw tail hands back fewer usable
/// rows the moment a run of answers fails, silently shrinking the context
/// instead of reaching further back. No projection is applied — it only
/// ever rewrites a `pending` row, and none is selected here.
pub async fn list_settled_tail(
    db: &Database,
    thread: &RagThreadId,
    limit: usize,
) -> Result<Vec<RagMessage>, AppError> {
    let mut messages: Vec<RagMessage> = query_as!(
        RagMessage,
        "SELECT id AS \"id: RagMessageId\", thread AS \"thread: RagThreadId\", \
                user_id AS \"user_id: UserId\", role AS \"role: RagMessageRole\", content AS \"content: RagContent\", \
                status AS \"status: RagMessageStatus\", reply AS \"reply: Json<RagReply>\", \
                error_code AS \"error_code: String\", \
                created_at AS \"created_at: Timestamp\", completed_at AS \"completed_at: Timestamp\" \
         FROM rag_message WHERE thread = $1 \
           AND status = 'complete' AND content <> '' \
         ORDER BY created_at DESC, id DESC LIMIT $2",
        thread.uuid(),
        limit as i64
    )
    .fetch_all(db)
    .await?;
    messages.reverse();
    Ok(messages)
}

/// The user turn a reserved answer belongs to, by *identity*: the question
/// row of the very POST that reserved it, whose id the answering task
/// carries.
///
/// Never derived from write order. Two POSTs on one thread interleave
/// across the two creates — the rows land `userA, userB, asstA, asstB` —
/// so "the newest user row written before this answer" resolves *both*
/// answers to question B, and question A is never answered.
pub async fn prompt_of(
    db: &Database,
    id: &RagMessageId,
) -> Result<Option<RagMessage>, AppError> {
    let message = query_as!(
        RagMessage,
        "SELECT id AS \"id: RagMessageId\", thread AS \"thread: RagThreadId\", \
                user_id AS \"user_id: UserId\", role AS \"role: RagMessageRole\", content AS \"content: RagContent\", \
                status AS \"status: RagMessageStatus\", reply AS \"reply: Json<RagReply>\", \
                error_code AS \"error_code: String\", \
                created_at AS \"created_at: Timestamp\", completed_at AS \"completed_at: Timestamp\" \
         FROM rag_message WHERE id = $1",
        id.uuid()
    )
    .fetch_optional(db)
    .await?;
    Ok(message)
}

/// Read one turn only if `user` owns it — the poll loop's read.
pub async fn read_for(
    db: &Database,
    id: &RagMessageId,
    user: &UserId,
) -> Result<Option<RagMessage>, AppError> {
    let message = query_as!(
        RagMessage,
        "SELECT id AS \"id: RagMessageId\", thread AS \"thread: RagThreadId\", \
                user_id AS \"user_id: UserId\", role AS \"role: RagMessageRole\", content AS \"content: RagContent\", \
                status AS \"status: RagMessageStatus\", reply AS \"reply: Json<RagReply>\", \
                error_code AS \"error_code: String\", \
                created_at AS \"created_at: Timestamp\", completed_at AS \"completed_at: Timestamp\" \
         FROM rag_message WHERE id = $1",
        id.uuid()
    )
    .fetch_optional(db)
    .await?;
    Ok(message
        .filter(|message| &message.user_id == user)
        .map(RagMessage::projected))
}

/// Land the answer: the text and the service's reply (abstention, reason,
/// citations), the pair every settled answer is judged by. Gated on
/// `status = 'pending'` in the `WHERE`, so a late answer can't overwrite a row
/// the boot sweep (or a timeout) already failed, and two answers can't both
/// apply.
pub async fn complete(
    db: &Database,
    id: &RagMessageId,
    text: RagContent,
    reply: Json<RagReply>,
) -> Result<RagMessage, AppError> {
    settle(db, id, Some(text), Some(reply), None).await
}

/// Mark the answer failed with a short code (`unavailable`, the service's
/// own error code, …). Same pending gate as [`complete`]; a failed turn
/// stores no reply — the service never produced one worth keeping.
pub async fn fail(
    db: &Database,
    id: &RagMessageId,
    error_code: &str,
) -> Result<RagMessage, AppError> {
    settle(db, id, None, None, Some(error_code)).await
}

async fn settle(
    db: &Database,
    id: &RagMessageId,
    text: Option<RagContent>,
    reply: Option<Json<RagReply>>,
    error_code: Option<&str>,
) -> Result<RagMessage, AppError> {
    let status = if text.is_some() {
        RagMessageStatus::Complete
    } else {
        RagMessageStatus::Failed
    };
    let settled = query_as!(
        RagMessage,
        "UPDATE rag_message SET content = $2, status = $3, reply = $4, \
             error_code = $5, completed_at = $6 \
         WHERE id = $1 AND status = 'pending' \
         RETURNING id AS \"id: RagMessageId\", thread AS \"thread: RagThreadId\", \
                   user_id AS \"user_id: UserId\", role AS \"role: RagMessageRole\", content AS \"content: RagContent\", \
                   status AS \"status: RagMessageStatus\", reply AS \"reply: Json<RagReply>\", \
                   error_code AS \"error_code: String\", \
                   created_at AS \"created_at: Timestamp\", completed_at AS \"completed_at: Timestamp\"",
        id.uuid(),
        text.map(|t| t.as_str().to_string()).unwrap_or_default(),
        status.as_str(),
        reply as _,
        error_code.map(|code| code.chars().take(MAX_ERROR_CODE_LEN).collect::<String>()),
        Timestamp::now().as_millis(),
    )
    .fetch_optional(db)
    .await?;
    settled.ok_or(AppError::NotFound)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::rag_message::RagCitedFile;

    /// A real owner and a real thread row: every turn is written through its
    /// thread, so a turn with no thread is refused (see [`insert`]), and the
    /// owner is a foreign key. Fixed keys, one per test.
    async fn a_thread(
        user: &str,
        thread: &str,
    ) -> (Database, crate::database::TestDatabases, UserId, RagThreadId) {
        let (db, leases) = crate::database::init_test_db().await;
        let user = UserId::from_key(user);
        let thread = RagThreadId::from_key(thread);
        sqlx::query("INSERT INTO app_user (id, username, created_at) VALUES ($1, 'rag-fixture', 0)")
            .bind(user.uuid())
            .execute(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO rag_thread (id, owner, created_at, updated_at) \
             VALUES ($1, $2, 0, 0)",
        )
        .bind(thread.uuid())
        .bind(user.uuid())
        .execute(&db)
        .await
        .unwrap();
        (db, leases, user, thread)
    }

    fn a_reply() -> Json<RagReply> {
        Json(RagReply {
            abstained: false,
            reason: String::new(),
            citations: vec![RagCitedFile {
                n: 1,
                file: None,
                pages: vec![12],
                span_ids: vec!["s1".to_string()],
                ders: Some("Fizik".to_string()),
            }],
        })
    }

    #[tokio::test]
    async fn a_turns_two_rows_never_sort_inverted() {
        // Both rows of a turn land in the same millisecond routinely, so the
        // `id` tie-break decides the thread's order. With a random ULID this
        // inverted a fifth of the pairs; here every pair must read back
        // question-then-answer, from both read paths.
        let (db, _leases, user, thread) = a_thread(
            "019732e3-7b00-7000-8000-00000000cafe",
            "019732e3-7b00-7000-8000-00000000beef",
        )
        .await;
        const TURNS: usize = 200;

        for turn in 0..TURNS {
            let content = RagContent::try_new(&format!("soru {turn}")).unwrap();
            append_user(&db, &thread, &user, content).await.unwrap();
            append_pending_assistant(&db, &thread, &user)
                .await
                .unwrap();
        }

        let (whole, _) = list_for_thread(&db, &thread, None, 0).await.unwrap();
        let tail = list_tail(&db, &thread, TURNS * 2).await.unwrap();
        for messages in [&whole, &tail] {
            assert_eq!(messages.len(), TURNS * 2);
            for (turn, pair) in messages.chunks(2).enumerate() {
                assert_eq!(pair[0].get_role(), RagMessageRole::User, "turn {turn}");
                assert_eq!(pair[0].get_content().as_str(), format!("soru {turn}"));
                assert_eq!(
                    pair[1].get_role(),
                    RagMessageRole::Assistant,
                    "turn {turn}"
                );
            }
        }
    }

    /// A settled answer carries the service's reply, and the reserved row it
    /// replaced carried none — the column is written once, by the flip that
    /// settles the turn.
    #[tokio::test]
    async fn a_completed_answer_carries_its_reply() {
        let (db, _leases, user, thread) = a_thread(
            "019732e3-7b00-7000-8000-00000000cafe",
            "019732e3-7b00-7000-8000-00000000beef",
        )
        .await;
        let pending = append_pending_assistant(&db, &thread, &user).await.unwrap();
        assert!(pending.get_reply().is_none(), "nothing is known yet");
        assert_eq!(pending.get_status(), RagMessageStatus::Pending);

        let reply = a_reply();
        let settled = complete(&db, pending.get_id(), RagContent::try_new("F = m·a").unwrap(), reply)
            .await
            .expect("land the answer");
        assert_eq!(settled.get_status(), RagMessageStatus::Complete);
        assert!(settled.get_completed_at().is_some());
        assert_eq!(settled.get_reply(), Some(&a_reply().0));

        // Re-read out of the store: the JSONB round trip is the contract a
        // reader receives, and a return value is not evidence.
        let stored = read_for(&db, pending.get_id(), &user)
            .await
            .unwrap()
            .expect("settled row");
        assert_eq!(stored.get_reply(), Some(&a_reply().0));
    }

    /// The boot sweep (or a timeout) fails a `pending` row; a reply that
    /// arrives afterwards must be refused, not written over it — the
    /// `status = 'pending'` gate in [`settle`] is the whole of that rule, and
    /// the stored row is the verdict.
    #[tokio::test]
    async fn a_late_answer_cannot_overwrite_a_swept_row() {
        let (db, _leases, user, thread) = a_thread(
            "019732e3-7b00-7000-8000-00000000cafe",
            "019732e3-7b00-7000-8000-00000000beef",
        )
        .await;
        let pending = append_pending_assistant(&db, &thread, &user).await.unwrap();
        sqlx::query(
            "UPDATE rag_message SET status = 'failed', error_code = 'interrupted', \
             completed_at = $1 WHERE id = $2",
        )
        .bind(Timestamp::now().as_millis())
        .bind(pending.get_id().uuid())
        .execute(&db)
        .await
        .unwrap();

        let late = complete(
            &db,
            pending.get_id(),
            RagContent::try_new("F = m·a").unwrap(),
            a_reply(),
        )
        .await;
        assert!(
            matches!(late, Err(AppError::NotFound)),
            "a settled row takes no second answer: {late:?}"
        );
        let stored = read_for(&db, pending.get_id(), &user)
            .await
            .unwrap()
            .expect("swept row");
        assert_eq!(stored.get_status(), RagMessageStatus::Failed);
        assert_eq!(stored.get_error_code(), Some("interrupted"));
        assert!(stored.get_reply().is_none(), "the late reply was not stored");
    }
}
