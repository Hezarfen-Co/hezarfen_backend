//! Two different folds, for two different jobs — never swap them:
//!
//! * [`search_fold`] / [`search_fold_sql`] are the **search** fold:
//!   deliberately *destructive*, collapsing `ı ş ğ ç ö ü â î û` onto their
//!   ASCII bases so a teacher on an ASCII keyboard still finds accented data.
//! * [`case_fold_tr`] is the **identity** fold: Turkish-correct case folding
//!   only. It answers "are these two entries the same word?" and must never
//!   conflate distinct letters (`tur` ≠ `tür`, `kir` ≠ `kır`).
//!
//! The search fold is shared by the needle (Rust side) and the column
//! (SurrealQL side) so the two can never disagree.
//!
//! Rust's `to_lowercase` and SurrealQL's `string::lowercase` are both
//! locale-invariant: Turkish `İ` (U+0130) lowercases to `i` + U+0307
//! (combining dot above), so plain `string::lowercase(text) CONTAINS $q` never
//! matched `İSTANBUL` for a teacher who typed `istanbul`. Folding strips that
//! leftover dot and maps the Turkish letters onto their ASCII base, which also
//! makes the search work in both directions (`istanbul` ↔ `İSTANBUL`,
//! `ıgdır` ↔ `Iğdır`).

/// Replacements applied *after* lowercasing, in order. One table, two
/// consumers ([`search_fold`] and [`search_fold_sql`]): the needle and the
/// column are folded by the same rules by construction, which is the whole
/// point.
const REPLACEMENTS: &[(&str, &str)] = &[
    ("\u{307}", ""), // combining dot above, left behind by İ → i̇
    ("ı", "i"),
    ("ş", "s"),
    ("ğ", "g"),
    ("ç", "c"),
    ("ö", "o"),
    ("ü", "u"),
    ("â", "a"),
    ("î", "i"),
    ("û", "u"),
];

/// Fold a search needle (or any Rust-side string) for comparison. **Search
/// only** — this is lossy, `tür` and `tur` both fold to `tur`. For "is this
/// the same word?" use [`case_fold_tr`].
pub fn search_fold(text: &str) -> String {
    let mut folded = text.to_lowercase();
    for (from, to) in REPLACEMENTS {
        folded = folded.replace(from, to);
    }
    folded
}

/// The same folding as a SurrealQL expression over `column`, for use inside a
/// `WHERE`. `column` is always a literal field name we wrote — never user
/// input.
pub fn search_fold_sql(column: &str) -> String {
    let mut expr = format!("string::lowercase({column})");
    for (from, to) in REPLACEMENTS {
        expr = format!("string::replace({expr}, '{from}', '{to}')");
    }
    expr
}

/// Case-fold under the **Turkish** casing rule, for identity comparisons
/// (list dedup). Only the two dotted/dotless pairs are folded together —
/// `i` ↔ `İ` and `ı` ↔ `I` — so `TÜR` and `tür` are one word while `tur` and
/// `tür` stay two. Never use this for search: it will not match an ASCII
/// spelling of an accented word (that is [`search_fold`]'s job).
pub fn case_fold_tr(text: &str) -> String {
    let mut folded = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            // Rust would lowercase these locale-invariantly: `İ` → `i` plus a
            // combining dot, and `I` → `i` (the *dotted* i, wrong letter).
            'İ' => folded.push('i'),
            'I' => folded.push('ı'),
            _ => folded.extend(ch.to_lowercase()),
        }
    }
    // A decomposed `İ` (`i` + combining dot above) is the same letter as `i`.
    folded.replace("i\u{307}", "i")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_turkish_casing_both_ways() {
        assert_eq!(search_fold("İSTANBUL"), "istanbul");
        assert_eq!(search_fold("istanbul"), "istanbul");
        assert_eq!(search_fold("İstanbul"), "istanbul");
        assert_eq!(search_fold("ISTANBUL"), "istanbul");
        assert_eq!(search_fold("ıstanbul"), "istanbul");
        assert_eq!(search_fold("Iğdır"), "igdir");
        assert_eq!(search_fold("ÇÖZÜM"), "cozum");
        assert_eq!(search_fold("Şekil"), "sekil");
    }

    /// The identity fold folds case (Turkish rules) and *nothing else* — the
    /// truth table the settings dedup depends on.
    #[test]
    fn case_fold_tr_folds_case_but_not_letters() {
        // Same word, different case.
        for (upper, lower) in [
            ("TÜR", "tür"),
            ("KIR", "kır"),
            ("İSTANBUL", "istanbul"),
            ("ISPARTA", "ısparta"),
            ("İZİN", "izin"),
            ("SINAV", "sınav"),
            ("LAB", "lab"),
        ] {
            assert_eq!(
                case_fold_tr(upper),
                case_fold_tr(lower),
                "{upper} and {lower} are the same word"
            );
        }
        // Distinct words the destructive search fold wrongly collapsed.
        for (a, b) in [("tur", "tür"), ("kir", "kır"), ("sıra", "sira")] {
            assert_ne!(
                case_fold_tr(a),
                case_fold_tr(b),
                "{a} and {b} are different words"
            );
            assert_eq!(
                search_fold(a),
                search_fold(b),
                "search fold still collapses {a}/{b}"
            );
        }
        // Decomposed `İ` (i + combining dot) is still plain `i`.
        assert_eq!(case_fold_tr("i\u{307}zin"), "izin");
    }

    /// The SQL side must fold a literal exactly like the Rust side does, or the
    /// needle and the column disagree again.
    #[tokio::test]
    async fn sql_folding_matches_rust_folding() {
        let db = crate::database::init_mem().await.unwrap();
        for sample in ["İSTANBUL", "Iğdır", "ÇÖZÜM", "istanbul"] {
            let mut result = db
                .query(format!("RETURN {};", search_fold_sql("$text")))
                .bind(("text", sample.to_string()))
                .await
                .unwrap()
                .check()
                .unwrap();
            let got = result.take::<Option<String>>(0).unwrap().unwrap();
            assert_eq!(got, search_fold(sample), "sql fold disagrees for {sample}");
        }
    }
}
