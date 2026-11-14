//! `asciiFolding` and `icuFolding` from the character tables observed on the reference search server 1.70.6
//! (`data/ascii-folding.txt` and `data/icu-folding.txt`: one line per character the filter
//! changes, its code point and the code points it becomes, in hexadecimal; none when the filter
//! removes it). `asciiFolding` maps character by character. `icuFolding` maps character by
//! character and then composes the result (NFC), which is how a run of conjoining jamo becomes
//! one Hangul syllable as on the reference search server.

use std::sync::OnceLock;

use crate::analysis::normalize::{Form, normalize_chars_into};
use crate::analysis::scratch::Chars;

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

/// The characters of `text`, each replaced by its entry in `table`, given to `out`.
fn map(text: &str, table: &Table, mut out: impl FnMut(char)) {
    for c in text.chars() {
        match table.binary_search_by_key(&c, |&(k, _)| k) {
            Ok(i) => table[i].1.chars().for_each(&mut out),
            Err(_) => out(c),
        }
    }
}

/// `text` folded by `asciiFolding`.
pub(super) fn ascii(text: &str) -> String {
    crate::analysis::scratch::written(|out| ascii_into(text, out))
}

/// Appends `text` folded by `asciiFolding` to `out`.
pub(super) fn ascii_into(text: &str, out: &mut String) {
    map(text, ascii_table(), |c| out.push(c));
}

/// `text` folded by `icuFolding`.
#[cfg(test)]
fn icu(text: &str) -> String {
    crate::analysis::scratch::written(|out| icu_into(text, out))
}

/// Appends `text` folded by `icuFolding` to `out`.
pub(super) fn icu_into(text: &str, out: &mut String) {
    let mut mapped = Chars::new();
    map(text, icu_table(), |c| mapped.push(c));
    normalize_chars_into(mapped.iter().copied(), Form::Nfc, out);
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
    fn examples_measured_on_the_reference() {
        assert_eq!(ascii("ÆON Straße Ǆemal ﬁne"), "AEON Strasse DZemal fine");
        assert_eq!(icu("ÆON Straße Ǆemal ＡＢＣ e\u{301}té"), "aeon strasse dzemal abc ete");
        assert_eq!(icu("\u{1100}\u{1161}"), "가");
    }
}
