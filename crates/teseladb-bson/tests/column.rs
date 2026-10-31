//! BSON columns (binary subtype 7) against a `refrouter` 9.0.4 (003 FR-002, T029).
//!
//! `data/bson-columns.txt` holds columns measured on a `refrouter` 9.0.4 (the columns of its own time
//! series buckets, columns made by hand, random ones and mutations of the others): whether a
//! message carrying the column is accepted or answered with 378 `NonConformantBSON`, and the rows
//! `$_internalUnpackBucket` reads from it, or `!` where unpacking fails. Line:
//! `<1|0> <column hex> <rows>`, rows as `-` (no value) or `<type>:<value hex>` separated by `,`.

mod common;

use common::hex;
use teseladb_bson::column::{self, Cell};
use teseladb_bson::{BsonError, Document, Limits, RawDoc, Value, validate_message};
use teseladb_types::Rng;

/// Rows the measurements decoded at most (the fixture has no column past it).
const MAX_ROWS: usize = 4096;
const NON_CONFORMANT: (i32, &str) = (378, "NonConformantBSON");

struct Measured {
    bytes: Vec<u8>,
    accepted: bool,
    rows: Option<Vec<Option<Cell>>>,
}

fn measured() -> Vec<Measured> {
    let text = include_str!("data/bson-columns.txt");
    text.lines()
        .map(|line| {
            let mut parts = line.split(' ');
            let accepted = parts.next() == Some("1");
            let bytes = match parts.next().expect("a column") {
                "-" => Vec::new(),
                h => hex(h),
            };
            let rows = match parts.next().expect("rows") {
                "!" => None,
                "=" => Some(Vec::new()),
                cells => Some(cells.split(',').map(cell).collect()),
            };
            Measured { bytes, accepted, rows }
        })
        .collect()
}

fn cell(text: &str) -> Option<Cell> {
    let (kind, bytes) = text.split_once(':')?;
    Some(Cell { kind: u8::from_str_radix(kind, 16).expect("a type"), bytes: hex(bytes) })
}

/// `{_id: 1, c: BinData(7, column)}`.
fn holder(column: &[u8]) -> Vec<u8> {
    Document::new()
        .with("_id", 1)
        .with("c", Value::Binary { subtype: 7, bytes: column.to_vec() })
        .to_bytes()
        .expect("writes")
}

#[test]
fn messages_are_refused_where_refdb_refuses_them() {
    let all = measured();
    assert!(all.len() > 600, "{}", all.len());
    let refused = all.iter().filter(|m| !m.accepted).count();
    assert!(refused > 100 && refused < all.len() - 100, "both kinds: {refused}");
    for m in &all {
        let h: String = m.bytes.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(column::validate(&m.bytes).is_ok(), m.accepted, "{h}");
        let doc = holder(&m.bytes);
        let message = validate_message(&doc, Limits::default());
        let stored = RawDoc::from_bytes(&doc).map(|_| ());
        // The BSON specification takes any bytes in a binary; only a message is checked.
        assert_eq!(stored, Ok(()), "{h}");
        if m.accepted {
            assert_eq!(message, Ok(()), "{h}");
        } else {
            let want = BsonError::InvalidColumn { offset: 16, path: "c".into() };
            assert_eq!(message, Err(want), "{h}");
        }
    }
}

#[test]
fn columns_decode_as_refdb_unpacks_them() {
    let (mut rows, mut failures) = (0, 0);
    for m in measured().iter().filter(|m| m.accepted) {
        let h: String = m.bytes.iter().map(|b| format!("{b:02x}")).collect();
        let got = column::decode(&m.bytes, MAX_ROWS);
        match &m.rows {
            Some(want) => {
                assert_eq!(got.as_ref(), Ok(want), "{h}");
                rows += want.len();
            }
            None => {
                assert!(got.is_err(), "{h}: {got:?}");
                failures += 1;
            }
        }
    }
    assert!(rows > 10_000 && failures > 50, "{rows} rows, {failures} failures");
}

#[test]
fn the_error_names_the_element_and_its_path() {
    let bad = Value::Binary { subtype: 7, bytes: vec![7] };
    let doc = Document::new()
        .with("_id", "n1")
        .with("a", Document::new().with("b", Value::Array(vec![Value::Int32(1), bad.clone()])));
    let e = validate_message(&doc.to_bytes().unwrap(), Limits::default()).unwrap_err();
    assert!(matches!(&e, BsonError::InvalidColumn { path, .. } if path == "a.b.1"), "{e:?}");
    assert_eq!(e.code(), NON_CONFORMANT);
    assert_eq!(e.to_string(), "Invalid BSON column in element with field name 'a.b.1'");
    // The first fault in document order decides: a column before a deep document wins, a deep
    // document before a column wins (measured).
    let mut deep = Document::new().with("x", 1);
    for _ in 0..205 {
        deep = Document::new().with("a", deep);
    }
    let limits = Limits { max_size: 1 << 20, max_depth: 400 };
    let first = Document::new().with("c", bad.clone()).with("d", deep.clone());
    let e = validate_message(&first.to_bytes_with(limits).unwrap(), Limits::default());
    assert!(matches!(e, Err(BsonError::InvalidColumn { .. })), "{e:?}");
    let second = Document::new().with("d", deep).with("c", bad);
    let e = validate_message(&second.to_bytes_with(limits).unwrap(), Limits::default());
    assert!(matches!(e, Err(BsonError::TooDeep { .. })), "{e:?}");
}

/// A literal document of `levels` levels: `03 00 {a: {a: … {x: 1}}} 00`.
fn literal_of_depth(levels: usize) -> Vec<u8> {
    let mut d = Document::new().with("x", 1);
    for _ in 1..levels {
        d = Document::new().with("a", d);
    }
    let bytes = d.to_bytes_with(Limits { max_size: 1 << 20, max_depth: 400 }).unwrap();
    let mut out = vec![0x03, 0];
    out.extend_from_slice(&bytes);
    out.push(0);
    out
}

#[test]
fn literals_hold_up_to_201_levels_wherever_the_column_is() {
    assert_eq!(column::validate(&literal_of_depth(201)), Ok(()));
    assert!(column::validate(&literal_of_depth(202)).is_err());
    let mut reference = literal_of_depth(201);
    reference[0] = 0xF1;
    reference.remove(1);
    reference.push(0);
    assert_eq!(column::validate(&reference), Ok(()));
    let mut reference = literal_of_depth(202);
    reference[0] = 0xF1;
    reference.remove(1);
    reference.push(0);
    assert!(column::validate(&reference).is_err());
    // The levels of the document that holds the column do not count.
    let mut held =
        Document::new().with("c", Value::Binary { subtype: 7, bytes: literal_of_depth(201) });
    for _ in 0..170 {
        held = Document::new().with("a", held);
    }
    assert_eq!(validate_message(&held.to_bytes().unwrap(), Limits::default()), Ok(()));
}

#[test]
fn decoding_stops_at_the_rows_it_is_given() {
    // An int32 literal and a block of 120 repeats.
    let col = hex("100005000000800f0000000000000000");
    assert_eq!(column::decode(&col, 121).map(|r| r.len()), Ok(121));
    assert!(column::decode(&col, 120).is_err());
    assert!(column::decode(&hex("07"), 10).is_err());
}

#[test]
fn binaries_past_16_bytes_take_no_deltas() {
    // A `refrouter` 9.0.4 repeats a binary of 17 bytes on a zero delta and refuses any other delta
    // (measured).
    let literal = format!("0500110000000041{}", "00".repeat(16));
    let repeat = hex(&format!("{literal}800e0000000000000000"));
    let rows = column::decode(&repeat, 4).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], rows[1]);
    let delta = hex(&format!("{literal}802e0000000000000000"));
    let e = column::decode(&delta, 4).unwrap_err();
    assert_eq!(e.to_string(), "BinData length should not exceed 16 in a delta encoding");
}

#[test]
fn mutated_columns_never_panic() {
    let all = measured();
    let mut rng = Rng::new(29);
    let (mut decoded, mut valid) = (0, 0);
    for _ in 0..200_000 {
        let base = &all[rng.range(0, all.len() as u64) as usize].bytes;
        let mut b = base.clone();
        for _ in 0..rng.range(1, 4) {
            if b.is_empty() || rng.chance(0.2) {
                b.insert(rng.range(0, b.len() as u64 + 1) as usize, rng.next_u32() as u8);
            } else if rng.chance(0.1) {
                b.truncate(rng.range(0, b.len() as u64) as usize);
            } else {
                let at = rng.range(0, b.len() as u64) as usize;
                b[at] ^= 1 << rng.range(0, 8);
            }
        }
        let ok = column::validate(&b).is_ok();
        valid += usize::from(ok);
        if let Ok(rows) = column::decode(&b, MAX_ROWS) {
            assert!(ok, "decoded but refused");
            assert!(rows.len() <= MAX_ROWS);
            decoded += 1;
        }
    }
    assert!(valid > 10_000 && decoded > 5_000, "{valid} valid, {decoded} decoded");
}
