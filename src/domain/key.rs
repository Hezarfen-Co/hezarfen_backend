//! Deterministic record keys shared by the per-sitting exam tables and the
//! image-slot tables.

/// The key for a `(scope, user, seq)` triple, where `scope` is whatever the row
/// hangs off — an exam for attempts and results, a question for answers and
/// drawings. The same triple always maps to the same key, so "one row per
/// student per sitting" holds by construction: a concurrent double write races
/// on one id instead of creating two rows, and no unique index is needed.
///
/// The first sitting keeps the historical `{scope}_{user}` shape — rows written
/// before per-attempt history existed stay addressable, unchanged, forever — so
/// only later sittings carry their number. That bare-`seq == 1` case is
/// migration law, not a style choice: change it and every pre-history row
/// becomes unreachable. ULID keys are alphanumeric, so `_` is an unambiguous
/// joiner.
pub fn sitting(scope: &str, user: &str, seq: i64) -> String {
    if seq == 1 {
        format!("{scope}_{user}")
    } else {
        format!("{scope}_{user}_{seq}")
    }
}

/// The key for one image slot on `owner` — `{owner}_q` for the thing's own
/// illustration, `{owner}_{choice id}` for one option's picture. Uniqueness per
/// slot needs no index this way, and keying by the option's *stable id* means
/// reordering the choice list moves no picture. `_` is not in Crockford base32
/// and a ULID is never `"q"`, so the two shapes can never collide — the rule
/// behind every image table's ids, spelled once.
pub fn slot(owner: &str, slot: Option<&str>) -> String {
    format!("{owner}_{}", slot.unwrap_or("q"))
}

#[cfg(test)]
mod tests {
    use super::{sitting, slot};

    #[test]
    fn a_slot_key_never_collides_with_the_illustration() {
        assert_eq!(slot("Q", None), "Q_q");
        assert_eq!(slot("Q", Some("01J8XZ")), "Q_01J8XZ");
        // A choice id is a ULID, so it can never be the literal "q".
        assert_ne!(slot("Q", Some("01J8XZ")), slot("Q", None));
    }

    #[test]
    fn the_first_sitting_stays_bare_and_later_ones_are_numbered() {
        // Byte-for-byte the shape the four exam id types wrote before this
        // helper existed; a pre-history row is keyed by the bare pair.
        assert_eq!(sitting("EXAM", "USER", 1), "EXAM_USER");
        assert_eq!(sitting("EXAM", "USER", 2), "EXAM_USER_2");
        assert_eq!(sitting("Q", "U", 11), "Q_U_11");
    }
}
