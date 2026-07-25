//! Ids that sort in write order, for tables whose listings lean on `id` to
//! break ties (or to mean "newest first" outright).
//!
//! `Ulid::new()` fills the low 80 bits at random, so two ids minted in the
//! *same millisecond* sort arbitrarily against each other — millisecond-
//! accurate ordering only. That is not enough when a burst of writes lands
//! inside one tick (a recurring appointment publish expanding a series, a
//! teacher saving templates back to back, a test creating rows in a loop):
//! `ORDER BY id DESC` then scrambles them. [`next_ulid`] mints from one
//! process-wide [`Generator`], which keeps ids strictly increasing within a
//! millisecond by incrementing the random part instead of redrawing it.

use std::sync::{LazyLock, Mutex};

use ulid::{Generator, Ulid};

static IDS: LazyLock<Mutex<Generator>> = LazyLock::new(|| Mutex::new(Generator::new()));

/// The next id in write order. Process-wide: two processes writing the same
/// table still only get millisecond accuracy, which is what a distributed id
/// can promise anyway.
pub fn next_ulid() -> Ulid {
    let mut ids = IDS.lock().expect("monotonic id generator poisoned");
    // The only error is exhausting the random bits *within* one millisecond
    // (2^80 ids deep); it clears itself as the clock ticks, so retry rather
    // than fall back to a random id and silently reintroduce the defect.
    loop {
        if let Ok(ulid) = ids.generate() {
            break ulid;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defect this module exists for: ids minted back to back (well inside
    /// one millisecond) must still sort in mint order.
    #[test]
    fn ids_sort_in_mint_order_within_a_millisecond() {
        let ids: Vec<String> = (0..1000).map(|_| next_ulid().to_string()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
    }
}
