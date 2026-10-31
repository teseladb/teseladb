//! The Unicode properties of the regular expressions against the pinned Unicode 15.1.0 files: the
//! embedded tables (`data/unicode-props.bin`) rebuilt from the character database. With
//! `TESELADB_WRITE_TABLES=1` the rebuilt tables are written.

use std::collections::BTreeMap;
use std::path::PathBuf;

use teseladb_search::analysis::props::{Binary, CATEGORIES};

const MAGIC: &[u8; 8] = b"TSLPRP01";
const CODE_POINTS: usize = 0x11_0000;

/// A file of the directory `scripts/fetch-pinned.sh scripts/unicode-search.sha256` downloads into.
fn ucd(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.tools/unicode/ucd-15.1.0")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("{}: {e}; run scripts/fetch-pinned.sh scripts/unicode-search.sha256", path.display())
    })
}

fn hex(s: &str) -> u32 {
    u32::from_str_radix(s.trim(), 16).unwrap()
}

/// `(first, last, fields after the range)` for each data line of a property file.
fn property_lines(text: &str) -> Vec<(u32, u32, Vec<String>)> {
    text.lines()
        .filter_map(|l| {
            let data = l.split('#').next().unwrap().trim();
            if data.is_empty() {
                return None;
            }
            let mut fields = data.split(';').map(str::trim);
            let range = fields.next().unwrap();
            let (a, b) = range.split_once("..").unwrap_or((range, range));
            Some((hex(a), hex(b), fields.map(str::to_string).collect()))
        })
        .collect()
}

/// Runs of equal non-zero values as `(first, last, value)`.
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

/// Loose matching of names (UAX #44, LM3): case, spaces, `_` and `-` ignored.
fn loose(name: &str) -> String {
    name.chars().filter(|c| !matches!(c, ' ' | '_' | '-')).flat_map(char::to_lowercase).collect()
}

struct UnicodeData {
    category: Vec<u32>,
    mirrored: Vec<bool>,
    uppercase: Vec<[u32; 2]>,
    names: Vec<(u32, String)>,
}

fn unicode_data() -> UnicodeData {
    let mut data = UnicodeData {
        category: vec![0; CODE_POINTS],
        mirrored: vec![false; CODE_POINTS],
        uppercase: Vec::new(),
        names: Vec::new(),
    };
    let mut first = None;
    for line in ucd("UnicodeData.txt").lines() {
        let f: Vec<&str> = line.split(';').collect();
        let cp = hex(f[0]);
        let gc = CATEGORIES.iter().position(|&c| c == f[2]).unwrap() as u32;
        let range = if f[1].ends_with(", First>") {
            first = Some(cp);
            continue;
        } else if f[1].ends_with(", Last>") {
            first.take().unwrap()..=cp
        } else {
            cp..=cp
        };
        for c in range {
            data.category[c as usize] = gc;
            data.mirrored[c as usize] = f[9] == "Y";
        }
        if !f[12].is_empty() {
            data.uppercase.push([cp, hex(f[12])]);
        }
        if !f[1].starts_with('<') {
            data.names.push((cp, f[1].to_string()));
        }
    }
    data
}

fn binary_bits(data: &UnicodeData) -> Vec<u32> {
    let mut bits = vec![0_u32; CODE_POINTS];
    let files =
        [ucd("PropList.txt"), ucd("DerivedCoreProperties.txt"), ucd("emoji/emoji-data.txt")];
    for text in &files {
        for (a, b, fields) in property_lines(text) {
            if let Some(i) = Binary::ALL.iter().position(|p| p.ucd_name() == fields[0]) {
                (a..=b).for_each(|c| bits[c as usize] |= 1 << i);
            }
        }
    }
    let mirrored = Binary::ALL.iter().position(|&p| p == Binary::BidiMirrored).unwrap();
    for (c, &m) in data.mirrored.iter().enumerate() {
        if m {
            bits[c] |= 1 << mirrored;
        }
    }
    bits
}

/// The aliases of the values of `property` in `PropertyValueAliases.txt`, each as `long;short…`.
fn aliases(property: &str) -> Vec<Vec<String>> {
    ucd("PropertyValueAliases.txt")
        .lines()
        .filter_map(|l| {
            let data = l.split('#').next().unwrap().trim();
            let fields: Vec<String> = data.split(';').map(|f| f.trim().to_string()).collect();
            (fields[0] == property).then(|| {
                let mut names = vec![fields[2].clone(), fields[1].clone()];
                names.extend(fields[3..].iter().cloned());
                names.dedup();
                names
            })
        })
        .collect()
}

/// Scripts or blocks: the range values (index + 1) and the alias lines in index order.
fn named_ranges(file: &str, property: &str, sort: bool) -> (Vec<[u32; 3]>, Vec<String>) {
    let lines = property_lines(&ucd(file));
    let mut order: Vec<String> = Vec::new();
    for (_, _, fields) in &lines {
        if !order.contains(&fields[0]) {
            order.push(fields[0].clone());
        }
    }
    if sort {
        order.sort();
    }
    let all = aliases(property);
    let names: Vec<String> = order
        .iter()
        .map(|n| {
            let found = all.iter().find(|a| loose(&a[0]) == loose(n)).unwrap();
            found.join(";")
        })
        .collect();
    let mut values = vec![0_u32; CODE_POINTS];
    for (a, b, fields) in lines {
        let i = order.iter().position(|n| *n == fields[0]).unwrap() as u32 + 1;
        (a..=b).for_each(|c| values[c as usize] = i);
    }
    (ranges(&values), names)
}

fn put_records<const N: usize>(out: &mut Vec<u8>, records: &[[u32; N]]) {
    out.extend((records.len() as u32).to_le_bytes());
    for r in records {
        r.iter().for_each(|w| out.extend(w.to_le_bytes()));
    }
}

fn put_lines(out: &mut Vec<u8>, lines: &[String]) {
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    out.extend((text.len() as u32).to_le_bytes());
    out.extend(text.as_bytes());
}

fn put_varint(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7F) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn build() -> Vec<u8> {
    let data = unicode_data();
    let mut out = MAGIC.to_vec();
    put_records(&mut out, &ranges(&data.category));
    put_records(&mut out, &ranges(&binary_bits(&data)));
    let (scripts, script_names) = named_ranges("Scripts.txt", "sc", true);
    let (blocks, block_names) = named_ranges("Blocks.txt", "blk", false);
    put_records(&mut out, &scripts);
    put_records(&mut out, &blocks);
    put_records(&mut out, &data.uppercase);
    put_lines(&mut out, &script_names);
    put_lines(&mut out, &block_names);
    let mut frequency: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, n) in &data.names {
        n.split(' ').for_each(|w| *frequency.entry(w).or_default() += 1);
    }
    let mut words: Vec<(&str, usize)> = frequency.into_iter().collect();
    words.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let index: BTreeMap<&str, u32> =
        words.iter().enumerate().map(|(i, (w, _))| (*w, i as u32)).collect();
    put_lines(&mut out, &words.iter().map(|(w, _)| w.to_string()).collect::<Vec<_>>());
    let mut stream = Vec::new();
    let mut previous = 0;
    for (cp, name) in &data.names {
        put_varint(&mut stream, cp - previous);
        previous = *cp;
        put_varint(&mut stream, name.split(' ').count() as u32);
        name.split(' ').for_each(|w| put_varint(&mut stream, index[w]));
    }
    out.extend((stream.len() as u32).to_le_bytes());
    out.extend(stream);
    out
}

#[test]
fn embedded_tables_match_the_unicode_files() {
    let built = build();
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data/unicode-props.bin");
    if std::env::var_os("TESELADB_WRITE_TABLES").is_some() {
        std::fs::write(&path, &built).unwrap();
    }
    let data = std::fs::read(&path).unwrap();
    assert!(built == data, "data/unicode-props.bin is stale: rerun with TESELADB_WRITE_TABLES=1");
}
