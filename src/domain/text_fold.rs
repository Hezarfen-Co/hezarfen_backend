//! Case- and diacritic-insensitive folding for search, shared by the needle
//! (Rust side) and the column (SurrealQL side) so the two can never disagree.
//!
//! Rust's `to_lowercase` and SurrealQL's `string::lowercase` are both
//! locale-invariant: Turkish `İ` (U+0130) lowercases to `i` + U+0307
//! (combining dot above), so plain `string::lowercase(text) CONTAINS $q` never
//! matched `İSTANBUL` for a teacher who typed `istanbul`. Folding strips that
//! leftover dot and maps the Turkish letters onto their ASCII base, which also
//! makes the search work in both directions (`istanbul` ↔ `İSTANBUL`,
//! `ıgdır` ↔ `Iğdır`).

/// Replacements applied *after* lowercasing, in order. One table, two
/// consumers ([`fold`] and [`fold_sql`]): the needle and the column are folded
/// by the same rules by construction, which is the whole point.
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

/// Fold a search needle (or any Rust-side string) for comparison.
pub fn fold(text: &str) -> String {
    let mut folded = text.to_lowercase();
    for (from, to) in REPLACEMENTS {
        folded = folded.replace(from, to);
    }
    folded
}

/// The same folding as a SurrealQL expression over `column`, for use inside a
/// `WHERE`. `column` is always a literal field name we wrote — never user
/// input.
pub fn fold_sql(column: &str) -> String {
    let mut expr = format!("string::lowercase({column})");
    for (from, to) in REPLACEMENTS {
        expr = format!("string::replace({expr}, '{from}', '{to}')");
    }
    expr
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_turkish_casing_both_ways() {
        assert_eq!(fold("İSTANBUL"), "istanbul");
        assert_eq!(fold("istanbul"), "istanbul");
        assert_eq!(fold("İstanbul"), "istanbul");
        assert_eq!(fold("ISTANBUL"), "istanbul");
        assert_eq!(fold("ıstanbul"), "istanbul");
        assert_eq!(fold("Iğdır"), "igdir");
        assert_eq!(fold("ÇÖZÜM"), "cozum");
        assert_eq!(fold("Şekil"), "sekil");
    }

    /// The SQL side must fold a literal exactly like the Rust side does, or the
    /// needle and the column disagree again.
    #[tokio::test]
    async fn sql_folding_matches_rust_folding() {
        let db = crate::database::init_mem().await.unwrap();
        for sample in ["İSTANBUL", "Iğdır", "ÇÖZÜM", "istanbul"] {
            let mut result = db
                .query(format!("RETURN {};", fold_sql("$text")))
                .bind(("text", sample.to_string()))
                .await
                .unwrap()
                .check()
                .unwrap();
            let got = result.take::<Option<String>>(0).unwrap().unwrap();
            assert_eq!(got, fold(sample), "sql fold disagrees for {sample}");
        }
    }
}
