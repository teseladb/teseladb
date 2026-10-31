//! Random values of every type for the key tests, weighted toward the cases that meet: the same
//! number in several types, the edges of each type, strings with `00`, nested documents.

#![allow(dead_code)]

use teseladb_bson::{Decimal128, Document, RawDoc, Value, types};
use teseladb_types::Rng;

pub fn pick<T: Clone>(rng: &mut Rng, items: &[T]) -> T {
    items[rng.range(0, items.len() as u64) as usize].clone()
}

/// Numbers that meet: the same value in several types, neighbours, and the edges.
fn number(rng: &mut Rng) -> Value {
    match rng.range(0, 10) {
        0 => Value::Int32(rng.range(0, 101) as i32 - 50),
        1 => Value::Int64(pick(rng, &[i64::MIN, i64::MAX, 0, -1, 9_007_199_254_740_993, 1 << 62])),
        2 => Value::Double(rng.range(0, 101) as f64 / 8.0 - 6.0),
        3 => Value::Double(pick(
            rng,
            &[
                0.0,
                -0.0,
                f64::NAN,
                f64::INFINITY,
                f64::NEG_INFINITY,
                0.1,
                1e308,
                f64::MAX,
                f64::MIN_POSITIVE,
                f64::from_bits(1),
                -f64::from_bits(1),
                9_007_199_254_740_992.0,
            ],
        )),
        4 => loop {
            let x = f64::from_bits(rng.next_u64());
            if x.is_finite() {
                break Value::Double(x);
            }
        },
        5 => Value::Int32(pick(rng, &[i32::MIN, i32::MAX, 5, -5, 0])),
        _ => {
            let text = match rng.range(0, 6) {
                0 => pick(
                    rng,
                    &["NaN", "-NaN", "Infinity", "-Infinity", "0", "-0", "0E+20", "0E-30"],
                )
                .to_owned(),
                1 => pick(
                    rng,
                    &["5", "5.0", "-5.00", "0.1", "1E+308", "-1.5", "0.125", "9223372036854775807"],
                )
                .to_owned(),
                2 => {
                    pick(rng, &["9.999999999999999999999999999999999E+6144", "1E-6176", "-1E-6176"])
                        .to_owned()
                }
                _ => {
                    let digits: String = (0..rng.range(1, 35))
                        .map(|_| char::from(b'0' + rng.range(0, 10) as u8))
                        .collect();
                    let sign = if rng.chance(0.5) { "-" } else { "" };
                    format!("{sign}{digits}E{}", rng.range(0, 41) as i32 - 20)
                }
            };
            Value::Decimal128(text.parse::<Decimal128>().unwrap())
        }
    }
}

fn text(rng: &mut Rng) -> String {
    (0..rng.range(0, 4)).map(|_| pick(rng, &["a", "b", "\0", "é", "Z", "ab"])).collect()
}

fn name(rng: &mut Rng) -> String {
    pick(rng, &["a", "b", "ab", ""]).to_owned()
}

fn value(rng: &mut Rng, depth: u32) -> Value {
    let kinds = if depth == 0 { 19 } else { 21 };
    match rng.range(0, kinds) {
        0..=5 => number(rng),
        6 => Value::String(text(rng)),
        7 => Value::Symbol(text(rng)),
        8 => {
            let subtype = pick(rng, &[0, 1, types::BINARY_OLD, types::BINARY_UUID, 0x80]);
            let len = if subtype == types::BINARY_UUID { 16 } else { rng.range(0, 3) as usize };
            Value::Binary { subtype, bytes: (0..len).map(|_| pick(rng, &[0, 1, 255])).collect() }
        }
        9 => Value::ObjectId([pick(rng, &[0, 1, 255]); 12]),
        10 => Value::Boolean(rng.chance(0.5)),
        11 => Value::DateTime(pick(rng, &[i64::MIN, -1, 0, 1, i64::MAX])),
        12 => Value::Timestamp {
            time: pick(rng, &[0, 1, u32::MAX]),
            increment: pick(rng, &[0, 1, u32::MAX]),
        },
        13 => Value::Regex { pattern: name(rng), options: pick(rng, &["", "i", "im"]).to_owned() },
        14 => Value::DbPointer { namespace: name(rng), id: [pick(rng, &[0, 1]); 12] },
        15 => Value::JavaScript(text(rng)),
        16 => pick(rng, &[Value::MinKey, Value::MaxKey, Value::Null, Value::Undefined]),
        17 => Value::JavaScriptWithScope {
            code: name(rng),
            scope: Document::new().with("n", number(rng)),
        },
        18 => number(rng),
        19 => {
            let mut d = Document::new();
            for _ in 0..rng.range(0, 3) {
                d.push(name(rng), value(rng, depth - 1));
            }
            Value::Document(d)
        }
        _ => Value::Array((0..rng.range(0, 3)).map(|_| value(rng, depth - 1)).collect()),
    }
}

/// The pool as documents `{v: value}`.
pub fn pool(seed: u64, n: usize) -> Vec<Vec<u8>> {
    let mut rng = Rng::new(seed);
    (0..n).map(|_| Document::new().with("v", value(&mut rng, 2)).to_bytes().unwrap()).collect()
}

pub fn raw(doc: &[u8]) -> teseladb_bson::RawValue<'_> {
    RawDoc::from_bytes(doc).unwrap().get("v").unwrap()
}
