//! The Unicode data of the analyzers against the pinned files: the embedded tables
//! (`data/unicode.bin`) rebuilt from the character database (the word-break classes of the
//! tokenizer from Unicode 12.1.0, the rest from 15.1.0), and the conformance tests of word
//! boundaries (12.1.0) and normalization (15.1.0). With `TESELADB_WRITE_TABLES=1` the rebuilt
//! tables are written.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use teseladb_search::analysis::normalize::{Form, normalize};
use teseladb_search::analysis::uax29::word_boundaries;
use teseladb_search::analysis::unicode::WordBreak;

/// Word-break record bit of Extended_Pictographic, as `src/analysis/unicode.rs` reads it.
const PICTOGRAPHIC: u32 = 0x80;
/// Word-break record bit of letters and numbers.
const ALPHANUMERIC: u32 = 0x40;
/// Word-break record bits of Script=Han, Script=Hiragana, Script=Hangul and Line_Break=SA.
const HAN: u32 = 0x100;
const HIRAGANA: u32 = 0x200;
const HANGUL: u32 = 0x400;
const COMPLEX_CONTEXT: u32 = 0x800;
/// Magic of the tables.
const MAGIC: &[u8; 8] = b"TSLUNI01";

/// `name` in the directory `scripts/fetch-pinned.sh <list>` downloads into.
fn pinned(dir: &str, list: &str, name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.tools/unicode").join(dir);
    let path = path.join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("{}: {e}; run scripts/fetch-pinned.sh scripts/{list}", path.display())
    })
}

/// A file of the Unicode 15.1.0 database.
fn ucd(name: &str) -> String {
    pinned("ucd-15.1.0", "unicode-search.sha256", name)
}

/// A file of the Unicode 12.1.0 database, that of the standard tokenizer.
fn ucd12(name: &str) -> String {
    pinned("ucd-12.1.0", "unicode-tokenizer.sha256", name)
}

fn hex(s: &str) -> u32 {
    u32::from_str_radix(s.trim(), 16).unwrap()
}

/// `(first, last, value)` for each data line of a property file.
fn property_lines(text: &str) -> Vec<(u32, u32, String)> {
    text.lines()
        .filter_map(|l| {
            let data = l.split('#').next().unwrap().trim();
            if data.is_empty() {
                return None;
            }
            let mut fields = data.split(';').map(str::trim);
            let range = fields.next().unwrap();
            let value = fields.collect::<Vec<_>>().join(";");
            let (a, b) = range.split_once("..").unwrap_or((range, range));
            Some((hex(a), hex(b), value))
        })
        .collect()
}

fn ranges(values: &[u32]) -> Vec<[u32; 3]> {
    let mut out: Vec<[u32; 3]> = Vec::new();
    for (c, &v) in values.iter().enumerate() {
        let c = c as u32;
        match out.last_mut() {
            Some(last) if last[2] == v && last[1] + 1 == c => last[1] = c,
            _ if v != 0 => out.push([c, c, v]),
            _ => {}
        }
    }
    out
}

/// The tables built from the pinned files.
fn build() -> Vec<u8> {
    let size = 0x11_0000;
    let mut wb = vec![0_u32; size];
    let mut mark = |a: u32, b: u32, bits: u32| {
        wb[a as usize..=b as usize].iter_mut().for_each(|x| *x |= bits);
    };
    for (a, b, v) in property_lines(&ucd12("auxiliary/WordBreakProperty.txt")) {
        mark(a, b, WordBreak::ALL.iter().position(|w| w.ucd_name() == v).unwrap() as u32);
    }
    let emoji = pinned("emoji-12.1", "unicode-emoji-tokenizer.sha256", "emoji-data.txt");
    for (a, b, v) in property_lines(&emoji) {
        if v == "Extended_Pictographic" {
            mark(a, b, PICTOGRAPHIC);
        }
    }
    for (a, b, v) in property_lines(&ucd12("Scripts.txt")) {
        let bits = match v.as_str() {
            "Han" => HAN,
            "Hiragana" => HIRAGANA,
            "Hangul" => HANGUL,
            _ => 0,
        };
        mark(a, b, bits);
    }
    for (a, b, v) in property_lines(&ucd12("LineBreak.txt")) {
        if v == "SA" {
            mark(a, b, COMPLEX_CONTEXT);
        }
    }
    let mut ccc = vec![0_u32; size];
    let mut decompositions = Vec::new();
    let mut lowercase = Vec::new();
    let mut pool = Vec::new();
    let mut canonical_pairs = Vec::new();
    let mut range_start = None;
    for line in ucd("UnicodeData.txt").lines() {
        let f: Vec<&str> = line.split(';').collect();
        let cp = hex(f[0]);
        let alnum = f[2].starts_with('L') || f[2].starts_with('N');
        let span = if f[1].ends_with(", First>") {
            range_start = Some(cp);
            continue;
        } else if f[1].ends_with(", Last>") {
            range_start.take().unwrap()..=cp
        } else {
            cp..=cp
        };
        for c in span {
            if alnum {
                wb[c as usize] |= ALPHANUMERIC;
            }
            ccc[c as usize] = f[3].parse().unwrap();
        }
        if !f[5].is_empty() {
            let compat = f[5].starts_with('<');
            let parts: Vec<u32> =
                f[5].split(' ').filter(|p| !p.starts_with('<')).map(hex).collect();
            if !compat && parts.len() == 2 {
                canonical_pairs.push((parts[0], parts[1], cp));
            }
            decompositions.push([cp, u32::from(compat), parts.len() as u32, pool.len() as u32]);
            pool.extend(parts);
        }
        if !f[13].is_empty() {
            lowercase.push([cp, hex(f[13])]);
        }
    }
    let norm = property_lines(&ucd("DerivedNormalizationProps.txt"));
    let excluded: BTreeSet<u32> = norm
        .iter()
        .filter(|(_, _, v)| v == "Full_Composition_Exclusion")
        .flat_map(|&(a, b, _)| a..=b)
        .collect();
    let compositions: BTreeMap<(u32, u32), u32> = canonical_pairs
        .into_iter()
        .filter(|(_, _, c)| !excluded.contains(c))
        .map(|(a, b, c)| ((a, b), c))
        .collect();
    let mut casefold = Vec::new();
    for (a, b, v) in &norm {
        if let Some(mapping) = v.strip_prefix("NFKC_CF;") {
            let parts: Vec<u32> = mapping.split_whitespace().map(hex).collect();
            for c in *a..=*b {
                casefold.push([c, parts.len() as u32, pool.len() as u32]);
                pool.extend(&parts);
            }
        }
    }
    let mut out = MAGIC.to_vec();
    let mut put = |records: Vec<Vec<u32>>| {
        out.extend((records.len() as u32).to_le_bytes());
        for v in records.into_iter().flatten() {
            out.extend(v.to_le_bytes());
        }
    };
    put(ranges(&wb).into_iter().map(|r| r.to_vec()).collect());
    put(ranges(&ccc).into_iter().map(|r| r.to_vec()).collect());
    put(decompositions.into_iter().map(|r| r.to_vec()).collect());
    put(compositions.into_iter().map(|((a, b), c)| vec![a, b, c]).collect());
    put(casefold.into_iter().map(|r| r.to_vec()).collect());
    put(lowercase.into_iter().map(|r| r.to_vec()).collect());
    put(pool.into_iter().map(|c| vec![c]).collect());
    out
}

#[test]
fn embedded_tables_match_the_unicode_files() {
    let built = build();
    if std::env::var_os("TESELADB_WRITE_TABLES").is_some() {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data/unicode.bin");
        std::fs::write(path, &built).unwrap();
    }
    let data =
        std::fs::read(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data/unicode.bin")).unwrap();
    assert!(built == data, "data/unicode.bin is stale: rerun with TESELADB_WRITE_TABLES=1");
}

fn field(f: &str) -> String {
    f.split_whitespace()
        .map(|h| char::from_u32(u32::from_str_radix(h, 16).unwrap()).unwrap())
        .collect()
}

#[test]
fn conformance_with_word_break_test() {
    let mut cases = 0;
    for line in ucd12("auxiliary/WordBreakTest.txt").lines() {
        let data = line.split('#').next().unwrap().trim();
        if data.is_empty() {
            continue;
        }
        let mut text = String::new();
        let mut expected = Vec::new();
        for token in data.split_whitespace() {
            match token {
                "÷" => expected.push(text.len()),
                "×" => {}
                hex => text.push(char::from_u32(u32::from_str_radix(hex, 16).unwrap()).unwrap()),
            }
        }
        assert_eq!(word_boundaries(&text), expected, "{line}");
        cases += 1;
    }
    assert!(cases > 1800, "{cases} cases");
}

#[test]
fn conformance_with_normalization_test() {
    let mut cases = 0;
    for line in ucd("NormalizationTest.txt").lines() {
        let data = line.split('#').next().unwrap().trim();
        if data.is_empty() || data.starts_with('@') {
            continue;
        }
        let c: Vec<String> = data.split(';').take(5).map(field).collect();
        for (i, src) in c.iter().enumerate() {
            let (nfc, nfd) = if i < 3 { (&c[1], &c[2]) } else { (&c[3], &c[4]) };
            assert_eq!(&normalize(src, Form::Nfc), nfc, "NFC {line}");
            assert_eq!(&normalize(src, Form::Nfd), nfd, "NFD {line}");
            assert_eq!(normalize(src, Form::Nfkc), c[3], "NFKC {line}");
            assert_eq!(normalize(src, Form::Nfkd), c[4], "NFKD {line}");
        }
        cases += 1;
    }
    assert!(cases > 19_000, "{cases} cases");
}
