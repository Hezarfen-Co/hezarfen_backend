//! Which AI services are connected right now, and which one gets the next
//! request.
//!
//! Membership is edge-driven: a worker appears when its handshake succeeds and
//! disappears when its QUIC connection closes. Nothing here polls or expires —
//! [`crate::constant::AI_IDLE_TIMEOUT_SECS`] on the transport is what turns a
//! silently dead service into a closed connection, and the connection task
//! turns that into a [`Registry::remove`].

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::ai::error::AiError;
use crate::constant::{AI_DEFAULT_CONCURRENT_PER_WORKER, AI_MAX_CONCURRENT_PER_WORKER};

/// One connected AI service.
///
/// `C` is the connection handle. In production it is a `quinn::Connection`
/// (see [`AiRegistry`]); the tests instantiate it with a cheap stand-in so the
/// routing rules can be exercised without a live socket.
pub struct Worker<C> {
    pub id: String,
    pub service: String,
    pub capabilities: Vec<String>,
    /// Capabilities this worker claimed in its `Hello` and then answered
    /// `unknown_capability` for. The advertised list is left untouched; this
    /// is the subtraction applied on top of it, so a self-contradicting answer
    /// stops routing without rewriting what the service declared. See
    /// [`Registry::withdraw`].
    withdrawn: Mutex<HashSet<String>>,
    /// Ceiling from the worker's `Hello`, already clamped by
    /// [`clamp_concurrency`].
    pub max_concurrent: usize,
    /// Requests currently in flight to this worker. Owned by [`Lease`]: it is
    /// incremented under the registry lock at pick time and decremented on
    /// drop, so an early return or a panic in the caller cannot leak capacity.
    inflight: AtomicUsize,
    conn: C,
}

impl<C> Worker<C> {
    pub fn inflight(&self) -> usize {
        self.inflight.load(Ordering::Relaxed)
    }

    pub fn conn(&self) -> &C {
        &self.conn
    }

    /// Does this worker offer `capability` right now? False for a name it
    /// never declared, and false for one it declared and then withdrew by
    /// refusing it as unknown ([`Registry::withdraw`]).
    pub fn offers(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
            && !self
                .withdrawn
                .lock()
                .expect("ai registry lock")
                .contains(capability)
    }
}

/// A reserved slot on a worker. Holding one is what makes the worker's
/// `inflight` count non-zero; dropping it releases the slot.
pub struct Lease<C> {
    worker: Arc<Worker<C>>,
}

impl<C> Lease<C> {
    pub fn worker(&self) -> &Worker<C> {
        &self.worker
    }

    pub fn conn(&self) -> &C {
        self.worker.conn()
    }
}

/// Debug without requiring `C: Debug` — the connection handle has nothing
/// useful to print, and a bound here would infect every `Result<Lease<_>, _>`
/// assertion.
impl<C> std::fmt::Debug for Lease<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lease")
            .field("worker", &self.worker.id)
            .field("inflight", &self.worker.inflight())
            .finish()
    }
}

impl<C> Drop for Lease<C> {
    fn drop(&mut self) {
        self.worker.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A worker's state at one instant, for logging and health output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerSnapshot {
    pub id: String,
    pub service: String,
    pub capabilities: Vec<String>,
    pub inflight: usize,
    pub max_concurrent: usize,
}

/// The set of connected workers, keyed by worker id.
pub struct Registry<C> {
    workers: Arc<Mutex<HashMap<String, Arc<Worker<C>>>>>,
}

/// The production registry: workers are live QUIC connections.
pub type AiRegistry = Registry<quinn::Connection>;

impl<C> Clone for Registry<C> {
    fn clone(&self) -> Self {
        Self {
            workers: Arc::clone(&self.workers),
        }
    }
}

impl<C> Default for Registry<C> {
    fn default() -> Self {
        Self {
            workers: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl<C> Registry<C> {
    /// Add a freshly handshaken worker. `id` is caller-assigned (a ULID) and
    /// must be unique; reusing one replaces the old entry.
    ///
    /// A fresh `Hello` is also how a withdrawn capability comes back: the
    /// entry is built from the handshake alone (see [`Registry::withdraw`] for
    /// why a claim is ever subtracted), so a service that reconnects —
    /// necessarily landing here under a new worker id — offers everything it
    /// declares again.
    pub fn insert(
        &self,
        id: String,
        service: String,
        capabilities: Vec<String>,
        max_concurrent: usize,
        conn: C,
    ) {
        let worker = Arc::new(Worker {
            id: id.clone(),
            service,
            capabilities,
            withdrawn: Mutex::new(HashSet::new()),
            max_concurrent,
            inflight: AtomicUsize::new(0),
            conn,
        });
        self.workers
            .lock()
            .expect("ai registry lock")
            .insert(id, worker);
    }

    /// Drop a worker whose connection closed. In-flight leases keep their
    /// `Arc<Worker>` alive until they finish (and fail on their own, since the
    /// connection is gone) — removal only stops *new* picks.
    pub fn remove(&self, id: &str) {
        self.workers.lock().expect("ai registry lock").remove(id);
    }

    /// Reserve a slot on the least-loaded worker offering `capability`.
    ///
    /// Least-inflight rather than round-robin: AI requests are wildly uneven
    /// in cost, so "whose turn is it" routes a 20-second inference onto a
    /// worker already grinding one while an idle worker sits next to it. Ties
    /// break on worker id, which is a ULID, so the ordering is stable and the
    /// choice is reproducible in a failure report.
    ///
    /// Selection and the `inflight` increment happen under one lock, so two
    /// concurrent picks cannot both see the same free slot and overshoot
    /// `max_concurrent`.
    pub fn pick(&self, capability: &str) -> Result<Lease<C>, AiError> {
        let workers = self.workers.lock().expect("ai registry lock");
        let mut offering = workers
            .values()
            .filter(|w| w.offers(capability))
            .peekable();
        if offering.peek().is_none() {
            return Err(AiError::NoWorker(capability.to_string()));
        }
        let chosen = offering
            .filter(|w| w.inflight() < w.max_concurrent)
            .min_by(|a, b| {
                a.inflight().cmp(&b.inflight()).then_with(|| {
                    // A reconnect keeps the dying connection in the registry
                    // until its idle timeout. Equal load must prefer the
                    // newer id, or the replay that fires on register is sent
                    // to the connection that is about to time out.
                    b.id.cmp(&a.id)
                })
            })
            .ok_or_else(|| AiError::Busy(capability.to_string()))?;
        chosen.inflight.fetch_add(1, Ordering::Relaxed);
        Ok(Lease {
            worker: Arc::clone(chosen),
        })
    }

    /// Every connected worker, ordered by id for stable output.
    pub fn snapshot(&self) -> Vec<WorkerSnapshot> {
        let workers = self.workers.lock().expect("ai registry lock");
        let mut out: Vec<_> = workers
            .values()
            .map(|w| WorkerSnapshot {
                id: w.id.clone(),
                service: w.service.clone(),
                // What the worker is offering, not what it declared: a
                // withdrawn capability must vanish from discovery exactly as
                // it vanishes from routing.
                capabilities: w
                    .capabilities
                    .iter()
                    .filter(|c| w.offers(c))
                    .cloned()
                    .collect(),
                inflight: w.inflight(),
                max_concurrent: w.max_concurrent,
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Is anything at all connected for `capability`?
    ///
    /// What counts is what a worker is *offering*, so a claim one of them has
    /// withdrawn ([`Registry::withdraw`]) reads as absent — which is what lets
    /// a door branch on this and stop queuing work nobody will do.
    pub fn has_capability(&self, capability: &str) -> bool {
        self.workers
            .lock()
            .expect("ai registry lock")
            .values()
            .any(|w| w.offers(capability))
    }

    /// Withdraw one capability from one worker: it advertised the name in its
    /// `Hello` and then answered a dispatch with a permanent, self-contradictory
    /// refusal (`unknown_capability`). The claim cannot be trusted again for
    /// this connection, so `pick` and `has_capability` stop seeing it and the
    /// worker drops out of discovery for that name.
    ///
    /// This is a subtraction, never a mutation of the declared list: a fresh
    /// [`Registry::insert`] — every reconnect does one, under a new worker id —
    /// restores the capability, so a service that fixes itself recovers with no
    /// operator action. Scoped to the one worker that answered: a sibling
    /// process declaring the same capability is untouched.
    ///
    /// A no-op for a worker already removed (its connection closed) or for a
    /// capability it never declared, so a late answer from a dead or confused
    /// worker cannot invent a state.
    pub fn withdraw(&self, id: &str, capability: &str) {
        let workers = self.workers.lock().expect("ai registry lock");
        let Some(worker) = workers.get(id) else {
            return;
        };
        if !worker.capabilities.iter().any(|c| c == capability) {
            return;
        }
        worker
            .withdrawn
            .lock()
            .expect("ai registry lock")
            .insert(capability.to_string());
    }
}

/// Fold a worker's self-declared concurrency into the allowed range. An absent
/// or zero value means "you decide" and gets the default; an inflated one is
/// capped rather than refused, so a misconfigured service still works, just
/// not greedily.
pub fn clamp_concurrency(declared: Option<u32>) -> usize {
    match declared {
        None | Some(0) => AI_DEFAULT_CONCURRENT_PER_WORKER,
        Some(n) => (n as usize).min(AI_MAX_CONCURRENT_PER_WORKER),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Routing is decided entirely by ids, capabilities and counts, so the
    /// connection handle can be a unit here.
    type TestRegistry = Registry<()>;

    fn registry_with(workers: &[(&str, &[&str], usize)]) -> TestRegistry {
        let reg = TestRegistry::default();
        for (id, caps, max) in workers {
            reg.insert(
                (*id).to_string(),
                "svc".into(),
                caps.iter().map(|c| (*c).to_string()).collect(),
                *max,
                (),
            );
        }
        reg
    }

    #[tokio::test]
    async fn an_unknown_capability_has_no_worker() {
        let reg = registry_with(&[("w1", &["ocr.extract"], 2)]);
        let err = reg.pick("grade.essay").unwrap_err();
        assert!(matches!(&err, AiError::NoWorker(c) if c == "grade.essay"));
        assert!(err.is_retryable());
        assert!(!reg.has_capability("grade.essay"));
        assert!(reg.has_capability("ocr.extract"));
    }

    #[tokio::test]
    async fn a_full_worker_is_busy_not_missing() {
        // The distinction is the whole point: `Busy` means the capacity exists
        // and the caller should come back, `NoWorker` means nothing is there.
        let reg = registry_with(&[("w1", &["ocr.extract"], 1)]);
        let _held = reg.pick("ocr.extract").unwrap();
        let err = reg.pick("ocr.extract").unwrap_err();
        assert!(matches!(err, AiError::Busy(c) if c == "ocr.extract"));
    }

    #[tokio::test]
    async fn a_lease_releases_its_slot_when_dropped() {
        let reg = registry_with(&[("w1", &["ocr.extract"], 1)]);
        {
            let lease = reg.pick("ocr.extract").unwrap();
            assert_eq!(lease.worker().inflight(), 1);
            assert!(reg.pick("ocr.extract").is_err());
        }
        assert_eq!(reg.snapshot()[0].inflight, 0);
        reg.pick("ocr.extract").expect("slot came back");
    }

    #[tokio::test]
    async fn equal_load_prefers_the_newer_worker() {
        // A reconnect leaves the dying connection registered until its idle
        // timeout. The replay that fires on the new Hello must not be handed
        // to that older id.
        let reg = registry_with(&[
            ("01a0cafe-old", &["rag.index"], 4),
            ("01a0cb0f-new", &["rag.index"], 4),
        ]);
        let lease = reg.pick("rag.index").unwrap();
        assert_eq!(lease.worker().id, "01a0cb0f-new");
    }


    #[tokio::test]
    async fn picks_spread_across_workers_by_least_inflight() {
        let reg = registry_with(&[("w1", &["ocr.extract"], 4), ("w2", &["ocr.extract"], 4)]);
        let leases: Vec<_> = (0..4).map(|_| reg.pick("ocr.extract").unwrap()).collect();
        let mut per_worker = std::collections::HashMap::new();
        for l in &leases {
            *per_worker.entry(l.worker().id.clone()).or_insert(0) += 1;
        }
        assert_eq!(per_worker.get("w1"), Some(&2));
        assert_eq!(per_worker.get("w2"), Some(&2));
    }

    #[tokio::test]
    async fn a_busy_worker_is_skipped_for_an_idle_one() {
        // Round-robin would hand w1 the next request anyway. Least-inflight is
        // what keeps a slow worker from accumulating a queue.
        let reg = registry_with(&[("w1", &["ocr.extract"], 8), ("w2", &["ocr.extract"], 8)]);
        let _busy: Vec<_> = (0..3)
            .map(|_| {
                let l = reg.pick("ocr.extract").unwrap();
                assert!(l.worker().id == "w1" || l.worker().id == "w2");
                l
            })
            .collect();
        // Whichever holds fewer gets the next one.
        let counts: HashMap<_, _> = reg
            .snapshot()
            .into_iter()
            .map(|s| (s.id, s.inflight))
            .collect();
        let lighter = if counts["w1"] <= counts["w2"] {
            "w1"
        } else {
            "w2"
        };
        let next = reg.pick("ocr.extract").unwrap();
        assert_eq!(next.worker().id, lighter);
    }

    #[tokio::test]
    async fn capacity_is_not_overshot_by_concurrent_picks() {
        // pick() selects and increments under one lock; if it did not, two
        // threads could both read `inflight == 0` on a max_concurrent-of-1
        // worker and both proceed.
        let reg = Arc::new(registry_with(&[("w1", &["ocr.extract"], 4)]));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let reg = Arc::clone(&reg);
            handles.push(std::thread::spawn(move || reg.pick("ocr.extract").ok()));
        }
        let leases: Vec<_> = handles
            .into_iter()
            .filter_map(|h| h.join().unwrap())
            .collect();
        assert_eq!(
            leases.len(),
            4,
            "exactly the declared capacity was handed out"
        );
        assert_eq!(reg.snapshot()[0].inflight, 4);
    }

    #[tokio::test]
    async fn removing_a_worker_stops_new_picks_but_not_the_one_in_flight() {
        let reg = registry_with(&[("w1", &["ocr.extract"], 4)]);
        let held = reg.pick("ocr.extract").unwrap();
        reg.remove("w1");
        assert!(matches!(
            reg.pick("ocr.extract").unwrap_err(),
            AiError::NoWorker(_)
        ));
        // The lease outlives removal — its request is still on the wire.
        assert_eq!(held.worker().id, "w1");
        assert!(reg.snapshot().is_empty());
    }

    #[tokio::test]
    async fn a_withdrawn_capability_stops_routing_and_a_fresh_hello_restores_it() {
        let reg = registry_with(&[("w1", &["ocr.extract", "ocr.classify"], 4)]);
        reg.withdraw("w1", "ocr.extract");
        assert!(!reg.has_capability("ocr.extract"));
        assert!(
            reg.has_capability("ocr.classify"),
            "the sibling claim is untouched"
        );
        assert!(matches!(
            reg.pick("ocr.extract").unwrap_err(),
            AiError::NoWorker(_)
        ));
        assert_eq!(
            reg.snapshot()[0].capabilities,
            vec!["ocr.classify".to_string()],
            "discovery reports what is offered, not what was declared"
        );

        // A reconnect re-inserts the worker from its handshake alone.
        reg.insert(
            "w1".into(),
            "svc".into(),
            vec!["ocr.extract".into(), "ocr.classify".into()],
            4,
            (),
        );
        assert!(reg.has_capability("ocr.extract"));
        reg.pick("ocr.extract").expect("restored");
    }

    #[tokio::test]
    async fn withdrawing_one_worker_leaves_a_sibling_offering_the_same() {
        let reg = registry_with(&[("w1", &["ocr.extract"], 2), ("w2", &["ocr.extract"], 2)]);
        reg.withdraw("w1", "ocr.extract");
        assert!(reg.has_capability("ocr.extract"));
        assert_eq!(reg.pick("ocr.extract").unwrap().worker().id, "w2");

        // Neither a name the worker never declared nor a worker already gone
        // is a state withdraw may invent.
        reg.withdraw("w2", "grade.essay");
        assert!(!reg.has_capability("grade.essay"));
        reg.remove("w2");
        reg.withdraw("w2", "ocr.extract");
        assert!(!reg.has_capability("ocr.extract"));
    }

    #[tokio::test]
    async fn declared_concurrency_is_clamped_into_range() {
        assert_eq!(clamp_concurrency(None), AI_DEFAULT_CONCURRENT_PER_WORKER);
        assert_eq!(clamp_concurrency(Some(0)), AI_DEFAULT_CONCURRENT_PER_WORKER);
        assert_eq!(clamp_concurrency(Some(3)), 3);
        assert_eq!(
            clamp_concurrency(Some(u32::MAX)),
            AI_MAX_CONCURRENT_PER_WORKER
        );
    }
}
