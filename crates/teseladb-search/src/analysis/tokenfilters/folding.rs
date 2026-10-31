//! `asciiFolding` and `icuFolding` from the character tables observed on `refsearch` 1.70.6
//! (`data/ascii-folding.txt` and `data/icu-folding.txt`: one line per character the filter
//! changes, its code point and the code points it becomes, in hexadecimal; none when the filter
//! removes it). `asciiFolding` maps character by character. `icuFolding` maps character by
//! character and then composes the result (NFC), which is how a run of conjoining jamo becomes
//! one Hangul syllable as on `refsearch`.

use std::sync::OnceLock;

use crate::analysis::normalize::{Form, normalize};

/// A folding table: code points in order, each with what it becomes.
type Table = Vec<(char, String)>;

/// Parses a table. The data is part of the crate and checked by its tests: a malformed line
/// is skipped rather than allowed to stop the analyzer.
fn parse(text: &str) -> Table {
    let hex = |h: &str| u32::from_str_radix(h, 16).ok().and_then(char::from_u32);
    let mut table: Table = text
        .lines()
        .filter_map(|line| {
            let (from, to) = line.split_once('\t')?;
            let to: Option<String> = to.split_whitespace().map(hex).collect();
            Some((hex(from)?, to?))
        })
        .collect();
    table.sort_by_key(|&(c, _)| c);
    table
}

fn ascii_table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| parse(include_str!("../../../data/ascii-folding.txt")))
}

fn icu_table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| parse(include_str!("../../../data/icu-folding.txt")))
}

/// `text` with each character replaced by its entry in `table`.
fn map(text: &str, table: &Table) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match table.binary_search_by_key(&c, |&(k, _)| k) {
            Ok(i) => out.push_str(&table[i].1),
            Err(_) => out.push(c),
        }
    }
    out
}

/// `text` folded by `asciiFolding`.
pub(super) fn ascii(text: &str) -> String {
    map(text, ascii_table())
}

/// `text` folded by `icuFolding`.
pub(super) fn icu(text: &str) -> String {
    normalize(&map(text, icu_table()), Form::Nfc)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn tables_parse_whole() {
        let lines = |t: &str| t.lines().count();
        assert_eq!(ascii_table().len(), lines(include_str!("../../../data/ascii-folding.txt")));
        assert_eq!(icu_table().len(), lines(include_str!("../../../data/icu-folding.txt")));
        assert_eq!(parse("zz\t41\n41\tzz\n42\n"), Vec::new());
    }

    #[test]
    fn examples_measured_on_refsearch() {
        assert_eq!(ascii("ÆON Straße Ǆemal ﬁne"), "AEON Strasse DZemal fine");
        assert_eq!(icu("ÆON Straße Ǆemal ＡＢＣ e\u{301}té"), "aeon strasse dzemal abc ete");
        assert_eq!(icu("\u{1100}\u{1161}"), "가");
    }
}
