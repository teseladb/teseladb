//! BSON columns written by this crate: every column `encode` writes passes the reference server's
//! layout check and reads back, row by row, the values it was given.

use teseladb_bson::column::{Cell, decode, encode, validate};
use teseladb_bson::types;

#[expect(
    clippy::unnecessary_wraps,
    reason = "a row of a column is an Option<Cell>, and every helper builds one"
)]
fn cell(kind: u8, bytes: Vec<u8>) -> Option<Cell> {
    Some(Cell { kind, bytes })
}

fn int32(v: i32) -> Option<Cell> {
    cell(types::INT32, v.to_le_bytes().to_vec())
}

fn int64(v: i64) -> Option<Cell> {
    cell(types::INT64, v.to_le_bytes().to_vec())
}

fn double(v: f64) -> Option<Cell> {
    cell(types::DOUBLE, v.to_le_bytes().to_vec())
}

fn date(v: i64) -> Option<Cell> {
    cell(types::DATE_TIME, v.to_le_bytes().to_vec())
}

fn string(s: &str) -> Option<Cell> {
    let mut b = i32::try_from(s.len() + 1).expect("a test length").to_le_bytes().to_vec();
    b.extend_from_slice(s.as_bytes());
    b.push(0);
    cell(types::STRING, b)
}

fn binary(subtype: u8, data: &[u8]) -> Option<Cell> {
    let mut b = i32::try_from(data.len()).expect("a test length").to_le_bytes().to_vec();
    b.push(subtype);
    b.extend_from_slice(data);
    cell(types::BINARY, b)
}

fn oid(bytes: [u8; 12]) -> Option<Cell> {
    cell(types::OBJECT_ID, bytes.to_vec())
}

fn round_trip(rows: &[Option<Cell>]) {
    let col = encode(rows);
    assert_eq!(validate(&col), Ok(()), "{rows:?}");
    assert_eq!(decode(&col, rows.len()).as_deref(), Ok(rows), "{rows:?} as {col:02x?}");
}

#[test]
fn steady_values_become_deltas_and_take_less_than_literals() {
    let rows: Vec<Option<Cell>> = (0..1000).map(|i| int32(i * 3)).collect();
    round_trip(&rows);
    // 1 000 values of 4 bytes: the deltas fit in far less.
    assert!(encode(&rows).len() < 600, "{} bytes", encode(&rows).len());
    let dates: Vec<Option<Cell>> = (0..1000).map(|i| date(1_767_225_600_000 + i * 1000)).collect();
    round_trip(&dates);
    // A steady step is a delta of deltas of zero.
    assert!(encode(&dates).len() < 200, "{} bytes", encode(&dates).len());
}

#[test]
fn every_type_reads_back() {
    let doc = vec![5, 0, 0, 0, 0];
    round_trip(&[
        int32(1),
        int32(-7),
        int32(i32::MIN),
        int32(i32::MAX),
        int64(i64::MIN),
        int64(i64::MAX),
        int64(0),
        double(0.5),
        double(-1e300),
        double(f64::NAN),
        string("x"),
        string("y"),
        string(""),
        string("a string longer than sixteen bytes"),
        string("z"),
        binary(0, &[1, 2, 3]),
        binary(0, &[1, 2, 4]),
        binary(4, &[9; 16]),
        binary(0, &[7; 20]),
        oid([0x6a, 0xe5, 0x3d, 0x5b, 1, 2, 3, 4, 5, 0, 0, 1]),
        oid([0x6a, 0xe5, 0x3d, 0x5b, 1, 2, 3, 4, 5, 0, 0, 2]),
        oid([0x6a, 0xe5, 0x3d, 0x5c, 9, 9, 9, 9, 9, 0, 0, 3]),
        cell(types::BOOLEAN, vec![1]),
        cell(types::BOOLEAN, vec![0]),
        cell(types::NULL, vec![]),
        cell(types::NULL, vec![]),
        cell(types::DOCUMENT, doc.clone()),
        cell(types::DOCUMENT, doc),
        cell(types::DOCUMENT, vec![12, 0, 0, 0, 0x10, b'p', 0, 1, 0, 0, 0, 0]),
        cell(types::TIMESTAMP, 5u64.to_le_bytes().to_vec()),
        cell(types::TIMESTAMP, 9u64.to_le_bytes().to_vec()),
        cell(types::DECIMAL128, [1u8; 16].to_vec()),
        cell(types::DECIMAL128, [2u8; 16].to_vec()),
        cell(types::MIN_KEY, vec![]),
        cell(types::MAX_KEY, vec![]),
    ]);
}

#[test]
fn rows_without_value_keep_their_place() {
    round_trip(&[None, None, int32(1), None, int32(2), None]);
    round_trip(&[None]);
    round_trip(&[]);
    let rows: Vec<Option<Cell>> =
        (0..200).map(|i| if i % 3 == 0 { None } else { date(i * 1000) }).collect();
    round_trip(&rows);
    // A big jump needs a literal.
    round_trip(&[int64(0), int64(i64::MAX), int64(i64::MIN), None, int64(1)]);
}

/// A small deterministic generator (xorshift), so that the test needs no crate.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "the random values are reinterpreted into every type of a column on purpose"
)]
fn random_columns_read_back() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..2000 {
        let len = (rng.next() % 300) as usize;
        let kind = rng.next() % 7;
        let mut base = rng.next() as i64;
        let rows: Vec<Option<Cell>> = (0..len)
            .map(|_| {
                if rng.next().is_multiple_of(9) {
                    return None;
                }
                let step = (rng.next() % 2000) as i64 - 1000;
                let big = rng.next().is_multiple_of(50);
                base = if big { rng.next() as i64 } else { base.wrapping_add(step) };
                match kind {
                    0 => int32(base as i32),
                    1 => int64(base),
                    2 => date(base),
                    3 => double(base as f64 / 7.0),
                    4 => string(&format!("{}", base % 1000)),
                    5 => cell(types::BOOLEAN, vec![(base & 1) as u8]),
                    _ => match rng.next() % 3 {
                        0 => int32(base as i32),
                        1 => string("s"),
                        _ => date(base),
                    },
                }
            })
            .collect();
        round_trip(&rows);
    }
}
