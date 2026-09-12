//! Deterministic wire keys shared by the per-sitting exam tables and the
//! image-slot tables.
//
// With natural composite primary keys these are no longer storage ids — a
// row is keyed by its component columns. They are the **HTTP edge** form only:
// when a sitting key or a slot key crosses a path or body, it travels as one
// underscore-joined string, and these helpers are its single spelling.

/// The wire key for a `(scope, user, seq)` triple, where `scope` is whatever
/// the row hangs off — an exam for attempts and results, a question for
/// answers and drawings. The first sitting keeps the historical
/// `{scope}_{user}` shape — rows written before per-attempt history existed
/// stay addressable, unchanged, forever — so only later sittings carry their
/// number. That bare-`seq == 1` case is migration law, not a style choice:
/// change it and every pre-history row becomes unreachable. UUID strings
/// contain only `-` and hex, so `_` is an unambiguous joiner.
pub fn sitting(scope: &str, user: &str, seq: i64) -> String {
    if seq == 1 {
        format!("{scope}_{user}")
    } else {
        format!("{scope}_{user}_{seq}")
    }
}

/// The wire key for one image slot on `owner` — `{owner}_q` for the thing's
/// own illustration, `{owner}_{choice id}` for one option's picture. A UUID is
/// never `"q"` and never contains `_`, so the two shapes can never collide —
/// the rule behind every image table's slot keys, spelled once.
pub fn slot(owner: &str, slot: Option<&str>) -> String {
    format!("{owner}_{}", slot.unwrap_or("q"))
}

#[cfg(test)]
mod tests {
    use super::{sitting, slot};

    const QUESTION: &str = "0198f1a2-3b4c-7d5e-8f90-1a2b3c4d5e6f";
    const STUDENT: &str = "0198f1a2-3b4c-7d5e-8f90-aa2b3c4d5e6f";

    #[test]
    fn a_slot_key_never_collides_with_the_illustration() {
        assert_eq!(slot(QUESTION, None), format!("{QUESTION}_q"));
        assert_eq!(slot(QUESTION, Some(STUDENT)), format!("{QUESTION}_{STUDENT}"));
        // A choice id is a UUID, so it can never be the literal "q".
        assert_ne!(slot(QUESTION, Some(STUDENT)), slot(QUESTION, None));
    }

    #[test]
    fn the_first_sitting_stays_bare_and_later_ones_are_numbered() {
        // Byte-for-byte the shape the four sitting-keyed tables wrote before
        // this helper existed; a pre-history row is keyed by the bare pair.
        assert_eq!(sitting(QUESTION, STUDENT, 1), format!("{QUESTION}_{STUDENT}"));
        assert_eq!(
            sitting(QUESTION, STUDENT, 2),
            format!("{QUESTION}_{STUDENT}_2")
        );
        assert_eq!(sitting("Q", "U", 11), "Q_U_11");
    }
}
