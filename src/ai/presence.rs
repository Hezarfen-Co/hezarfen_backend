//! Who can serve which AI capability *anywhere in the deployment* — the GATE.
//!
//! There are two answers to "is an AI service available", and confusing them
//! is the one way this module can do damage, so they are named:
//!
//! * **The gate — this module, the `ai_worker` table.** It answers "could
//!   anything, anywhere, serve `chat.reply`?". Every replica writes a row for
//!   each worker whose QUIC connection *it* holds and restamps it every
//!   [`AI_WORKER_HEARTBEAT_SECS`]; a row unstamped for
//!   [`AI_WORKER_LEASE_SECS`] is ignored and swept, so a replica that dies
//!   leaves no permanent phantom. It is a cache of other processes' sockets:
//!   it lags a registration by up to one heartbeat and a crash by up to one
//!   lease, in *both* directions. Its only job is to spare a user a 503 on a
//!   replica that holds no worker itself, because a peer's claim loop will
//!   answer the turn. It must never guard a write and must never decide a
//!   dispatch.
//! * **The dispatcher — [`crate::ai::registry::Registry`], in process.** It
//!   owns the sockets. Only it knows whether *this* replica can answer, and it
//!   is the only thing the chat claim loop consults before taking a turn. A
//!   claim made on the gate's word would park a turn on a replica with nothing
//!   to send it to.
//!
//! The split is why the table is safe to be wrong: nothing downstream of a
//! wrong answer is irreversible. A false yes costs one accepted turn that
//! settles `failed` (or waits out its staleness window); a false no costs one
//! 503 the user can retry.

use surrealdb::types::RecordId;

use crate::ai::registry::WorkerSnapshot;
use crate::constant::{
    AI_WORKER_ANNOUNCE, AI_WORKER_LEASE_SECS, AI_WORKER_SERVES, AI_WORKER_SWEEP, AI_WORKER_TABLE,
    AI_WORKER_WITHDRAW,
};
use crate::database::Database;
use crate::error::AppError;

fn row(id: &str) -> RecordId {
    RecordId::new(AI_WORKER_TABLE, id)
}

/// Publish (or restamp) the workers this replica holds right now.
///
/// `UPSERT` per worker, and the caller passes the whole live set every
/// heartbeat: a row lost to a database blip, or a worker that registered
/// before the presence table was attached, heals on the next beat instead of
/// staying invisible to every peer forever.
pub async fn announce(db: &Database, workers: &[WorkerSnapshot]) -> Result<(), AppError> {
    for worker in workers {
        db.query(AI_WORKER_ANNOUNCE)
            .bind(("id", row(&worker.id)))
            .bind(("service", worker.service.clone()))
            .bind(("capabilities", worker.capabilities.clone()))
            .await?
            .check()?;
    }
    Ok(())
}

/// Drop a worker that said goodbye. Immediate, so a clean disconnect closes
/// the gate now rather than a lease later.
pub async fn withdraw(db: &Database, id: &str) -> Result<(), AppError> {
    db.query(AI_WORKER_WITHDRAW)
        .bind(("id", row(id)))
        .await?
        .check()?;
    Ok(())
}

/// Forget workers nobody restamped within the lease — their replica died
/// without deregistering them. Any replica may run it: the rows are keyed by
/// worker, not by owner, and a live worker's row is never stale.
pub async fn sweep(db: &Database) -> Result<(), AppError> {
    db.query(AI_WORKER_SWEEP)
        .bind(("lease_ms", AI_WORKER_LEASE_SECS * 1_000))
        .await?
        .check()?;
    Ok(())
}

/// The gate read: does *any* replica hold a worker for `capability`?
///
/// Errors answer "no" rather than propagating. This is consulted on the 503
/// path of a request that has not written anything yet, so the worst a
/// database hiccup can do is tell one user to retry — turning it into a 500
/// would be strictly worse for the same fault.
pub async fn serves(db: &Database, capability: &str) -> bool {
    let found: Result<Vec<RecordId>, AppError> = async {
        Ok(db
            .query(AI_WORKER_SERVES)
            .bind(("capability", capability.to_string()))
            .bind(("lease_ms", AI_WORKER_LEASE_SECS * 1_000))
            .await?
            .check()?
            .take(0)?)
    }
    .await;
    match found {
        Ok(rows) => !rows.is_empty(),
        Err(err) => {
            tracing::warn!("could not read the AI worker gate: {err}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constant::AI_CHAT_CAPABILITY;

    fn worker(id: &str, capabilities: &[&str]) -> WorkerSnapshot {
        WorkerSnapshot {
            id: id.to_string(),
            service: "tutor".to_string(),
            capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
            inflight: 0,
            max_concurrent: 4,
        }
    }

    #[tokio::test]
    async fn a_stale_row_stops_answering_the_gate() {
        // The phantom this exists to prevent: a replica that dies leaves its
        // row behind, and without the age filter that row would keep every
        // peer's chat endpoint reporting a service that is gone.
        let db = crate::database::init_mem().await.unwrap();
        announce(&db, &[worker("w1", &[AI_CHAT_CAPABILITY])])
            .await
            .unwrap();
        assert!(serves(&db, AI_CHAT_CAPABILITY).await);
        assert!(!serves(&db, "ocr.extract").await, "a capability nobody has");

        // Age it past the lease, as a dead replica's row ages on its own.
        db.query("UPDATE ai_worker SET seen_at = seen_at - $ms")
            .bind(("ms", (AI_WORKER_LEASE_SECS + 1) * 1_000))
            .await
            .unwrap()
            .check()
            .unwrap();
        assert!(
            !serves(&db, AI_CHAT_CAPABILITY).await,
            "a stale row must not hold the gate open"
        );
        sweep(&db).await.unwrap();
        let rows: Vec<RecordId> = db
            .query("SELECT VALUE id FROM ai_worker")
            .await
            .unwrap()
            .check()
            .unwrap()
            .take(0)
            .unwrap();
        assert!(rows.is_empty(), "the sweep deletes what the read ignores");
    }

    #[tokio::test]
    async fn announcing_twice_restamps_rather_than_duplicating() {
        let db = crate::database::init_mem().await.unwrap();
        let live = [worker("w1", &[AI_CHAT_CAPABILITY])];
        announce(&db, &live).await.unwrap();
        db.query("UPDATE ai_worker SET seen_at = seen_at - $ms")
            .bind(("ms", (AI_WORKER_LEASE_SECS + 1) * 1_000))
            .await
            .unwrap()
            .check()
            .unwrap();
        assert!(!serves(&db, AI_CHAT_CAPABILITY).await);

        announce(&db, &live).await.unwrap();
        assert!(serves(&db, AI_CHAT_CAPABILITY).await, "the beat healed it");
        let rows: Vec<RecordId> = db
            .query("SELECT VALUE id FROM ai_worker")
            .await
            .unwrap()
            .check()
            .unwrap()
            .take(0)
            .unwrap();
        assert_eq!(rows.len(), 1, "one worker is one row");

        withdraw(&db, "w1").await.unwrap();
        assert!(
            !serves(&db, AI_CHAT_CAPABILITY).await,
            "goodbye is immediate"
        );
    }
}
