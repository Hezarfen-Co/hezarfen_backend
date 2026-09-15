//! Ids that sort in write order, for tables whose listings lean on `id` to
//! break ties (or to mean "newest first" outright).
//!
//! Every id this crate mints is a v7 uuid from [`next_uuid`] — there is no
//! second convention, and the guard test below scans `src/` for one.
//!
//! A bare `Uuid::new_v7()` redraws its random bits per id, so two ids minted
//! in the *same millisecond* sort arbitrarily against each other —
//! millisecond-accurate ordering only. That is not enough when a burst of
//! writes lands inside one tick (a recurring appointment publish expanding a
//! series, a teacher saving templates back to back, a test creating rows in a
//! loop): `ORDER BY id DESC` then scrambles them. [`next_uuid`] mints through
//! one process-wide [`ContextV7`], which keeps ids strictly increasing within
//! a millisecond by incrementing its counter instead of redrawing it.

use std::sync::{LazyLock, Mutex};

use uuid::{ContextV7, Uuid};

use crate::domain::timestamp::Timestamp;

/// `ContextV7` is `Send` but not `Sync` (interior `Cell`s), so the
/// process-wide context lives behind a mutex — the same serialization the id
/// generator always had, and the price of one process-wide mint order.
static CTX: LazyLock<Mutex<ContextV7>> = LazyLock::new(|| Mutex::new(ContextV7::new()));

/// The next id in write order. The wall clock is the codebase's single read
/// ([`Timestamp::now`]); the context contributes the within-millisecond
/// counter. Process-wide: two processes writing the same table still only get
/// millisecond accuracy, which is what a distributed id can promise anyway.
pub fn next_uuid() -> Uuid {
    let millis = Timestamp::now().as_millis();
    let context = CTX.lock().expect("uuid v7 context poisoned");
    Uuid::new_v7(uuid::Timestamp::from_unix(
        &*context,
        (millis / 1_000) as u64,
        (millis % 1_000) as u32 * 1_000_000,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defect this module exists for: ids minted back to back (well inside
    /// one millisecond) must still sort in mint order. `Uuid`'s `Ord` is
    /// big-endian byte order, exactly what a `uuid` column sorts by.
    #[test]
    fn ids_sort_in_mint_order_within_a_millisecond() {
        let ids: Vec<Uuid> = (0..1000).map(|_| next_uuid()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
    }

    /// No file under `src/` may mint a v4 — the whole crate mints v7 ids, and
    /// a lone v4 at one site is the regression the module doc describes
    /// (millisecond-accurate ordering, silently). The needle is assembled at
    /// run time so this test's own source is not a hit.
    #[test]
    fn nothing_under_src_mints_a_v4_uuid() {
        let needle = format!("new_{}", "v4");
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![root];
        let mut hits = Vec::new();
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read a source directory") {
                let path = entry.expect("a source entry").path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    let text = std::fs::read_to_string(&path).expect("read a source file");
                    if text.contains(&needle) {
                        hits.push(path.display().to_string());
                    }
                }
            }
        }
        hits.sort();
        assert!(hits.is_empty(), "these files mint a v4 uuid: {hits:?}");
    }
}
