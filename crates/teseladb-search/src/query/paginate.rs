//! `searchAfter`, `searchBefore` and the `searchSequenceToken` of each result (011 FR-003), as
//! the reference search server 1.70.6 pages (measured 02-11-2026, `scratchpad/lane3k/m14.js`): a token is a
//! base64 protocol buffer with the result's score (field 2) or sort values (field 3) and what
//! breaks ties; `searchAfter` gives the results that follow the token's, `searchBefore` those
//! that precede it, nearest first. The reference search server breaks ties with the reference search library's document number, which
//! depends on the order it read the collection; here the key of the document breaks them, so a
//! token carries the key (field 5) and its bytes differ from the reference search server's.

use std::cmp::Ordering;

use teseladb_bson::{Document, Value};

use super::search::SearchError;
use super::search::values::Key;
use super::sort::{self, SortField, SortValue};
use crate::definition::Definition;
use crate::indexer::Found;

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Protocol buffer wire types.
const VARINT: u8 = 0;
const FIXED64: u8 = 1;
const BYTES: u8 = 2;
const FIXED32: u8 = 5;

fn base64(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(BASE64[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn from_base64(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::new();
    for chunk in bytes.chunks(4) {
        let pad = chunk.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 {
            return None;
        }
        let mut n = 0u32;
        for &c in &chunk[..4 - pad] {
            n = (n << 6) | BASE64.iter().position(|&b| b == c)? as u32;
        }
        n <<= 6 * pad as u32;
        let all = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&all[..3 - pad]);
    }
    Some(out)
}

fn varint(out: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

fn tag(out: &mut Vec<u8>, field: u8, wire: u8) {
    out.push((field << 3) | wire);
}

fn bytes_field(out: &mut Vec<u8>, field: u8, b: &[u8]) {
    tag(out, field, BYTES);
    varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

/// A sort value as a message: 1 missing, 3 score, 4 number (as the reference search server), 5 date, 6 boolean,
/// 7 objectId, 8 UUID, 9 string.
fn encode_value(v: &SortValue) -> Vec<u8> {
    let mut out = Vec::new();
    match v {
        SortValue::Missing => {
            tag(&mut out, 1, VARINT);
            varint(&mut out, 1);
        }
        SortValue::Score(s) => {
            tag(&mut out, 3, FIXED32);
            out.extend_from_slice(&s.to_le_bytes());
        }
        SortValue::Key(k) => match k {
            Key::Number(n) => {
                tag(&mut out, 4, FIXED64);
                out.extend_from_slice(&n.to_le_bytes());
            }
            Key::Date(ms) => {
                tag(&mut out, 5, VARINT);
                varint(&mut out, *ms as u64);
            }
            Key::Bool(b) => {
                tag(&mut out, 6, VARINT);
                varint(&mut out, u64::from(*b));
            }
            Key::ObjectId(o) => bytes_field(&mut out, 7, o),
            Key::Uuid(u) => bytes_field(&mut out, 8, u),
            Key::Token(s, _) | Key::Text(s) => bytes_field(&mut out, 9, s.as_bytes()),
            Key::Null => {
                tag(&mut out, 1, VARINT);
                varint(&mut out, 1);
            }
        },
    }
    out
}

/// A field read from a protocol buffer.
enum Field<'a> {
    Varint(u64),
    Fixed64([u8; 8]),
    Bytes(&'a [u8]),
    Fixed32([u8; 4]),
}

/// The fields of message `b`, or `None` when it is not one.
fn fields(mut b: &[u8]) -> Option<Vec<(u8, Field<'_>)>> {
    fn read_varint(b: &mut &[u8]) -> Option<u64> {
        let mut n = 0u64;
        for shift in (0..64).step_by(7) {
            let (&byte, rest) = b.split_first()?;
            *b = rest;
            n |= u64::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                return Some(n);
            }
        }
        None
    }
    let mut out = Vec::new();
    while !b.is_empty() {
        let key = read_varint(&mut b)?;
        let field = u8::try_from(key >> 3).ok().filter(|f| *f > 0)?;
        let value = match (key & 7) as u8 {
            VARINT => Field::Varint(read_varint(&mut b)?),
            FIXED64 => {
                let (head, rest) = b.split_at_checked(8)?;
                b = rest;
                Field::Fixed64(head.try_into().ok()?)
            }
            BYTES => {
                let len = usize::try_from(read_varint(&mut b)?).ok()?;
                let (head, rest) = b.split_at_checked(len)?;
                b = rest;
                Field::Bytes(head)
            }
            FIXED32 => {
                let (head, rest) = b.split_at_checked(4)?;
                b = rest;
                Field::Fixed32(head.try_into().ok()?)
            }
            _ => return None,
        };
        out.push((field, value));
    }
    Some(out)
}

fn decode_value(b: &[u8]) -> Option<SortValue> {
    let mut value = SortValue::Missing;
    for (field, f) in fields(b)? {
        value = match (field, f) {
            (1, Field::Varint(_)) => SortValue::Missing,
            (3, Field::Fixed32(s)) => SortValue::Score(f32::from_le_bytes(s)),
            (4, Field::Fixed64(n)) => SortValue::Key(Key::Number(f64::from_le_bytes(n))),
            (5, Field::Varint(ms)) => SortValue::Key(Key::Date(ms as i64)),
            (6, Field::Varint(b)) => SortValue::Key(Key::Bool(b != 0)),
            (7, Field::Bytes(o)) => SortValue::Key(Key::ObjectId(o.try_into().ok()?)),
            (8, Field::Bytes(u)) => SortValue::Key(Key::Uuid(u.to_vec())),
            (9, Field::Bytes(s)) => SortValue::Key(Key::Text(String::from_utf8(s.to_vec()).ok()?)),
            _ => return None,
        };
    }
    Some(value)
}

/// A token: the place of a result in the order of the results.
#[derive(Clone, Debug, PartialEq)]
struct Place {
    values: Vec<SortValue>,
    key: Vec<u8>,
}

fn encode(place: &Place, by_score: bool) -> String {
    let mut out = Vec::new();
    match (by_score, place.values.first()) {
        (true, Some(SortValue::Score(s))) => {
            tag(&mut out, 2, FIXED32);
            out.extend_from_slice(&s.to_le_bytes());
        }
        _ => place.values.iter().for_each(|v| bytes_field(&mut out, 3, &encode_value(v))),
    }
    bytes_field(&mut out, 5, &place.key);
    base64(&out)
}

/// The place a token names, read for `fields` (`by_score` without `sort`).
fn decode(token: &str, by_score: bool) -> Option<Place> {
    let bytes = from_base64(token)?;
    let mut place = Place { values: Vec::new(), key: Vec::new() };
    let mut score = None;
    for (field, f) in fields(&bytes)? {
        match (field, f) {
            (1, Field::Varint(_)) => {}
            (2, Field::Fixed32(s)) => score = Some(f32::from_le_bytes(s)),
            (3, Field::Bytes(v)) => place.values.push(decode_value(v)?),
            (4, Field::Bytes(_)) => {}
            (5, Field::Bytes(k)) => place.key = k.to_vec(),
            _ => return None,
        }
    }
    if by_score {
        // A token without a score is at 0 (measured: a sort's token finds nothing then).
        place.values = vec![SortValue::Score(score.unwrap_or(0.0))];
    }
    Some(place)
}

/// The token named `name` of `spec`, read for the order of the results.
fn token_at(spec: &Document, name: &str, by_score: bool) -> Result<Option<Place>, SearchError> {
    match spec.get(name) {
        None => Ok(None),
        Some(Value::String(t)) => decode(t, by_score)
            .map(Some)
            .ok_or_else(|| SearchError(format!("\"{name}\" Invalid format for token value"))),
        Some(_) => Err(SearchError(format!("\"{name}\" must be a string"))),
    }
}

/// The documents `found` in the order `spec` asks for, after or before its token, each with
/// its own token.
pub(crate) fn arrange(
    found: Vec<Found>,
    spec: &Document,
    definition: &Definition,
) -> Result<Vec<Found>, SearchError> {
    let sorted = sort::parse(spec)?;
    let by_score = sorted.is_none();
    let fields: Vec<SortField> = sorted.unwrap_or_else(sort::by_score);
    let before = token_at(spec, "searchBefore", by_score)?;
    let after = token_at(spec, "searchAfter", by_score)?;
    let mut placed: Vec<(Place, Found)> = found
        .into_iter()
        .map(|f| {
            let values =
                fields.iter().map(|s| sort::value_of(definition, &f.doc, f.score, s)).collect();
            (Place { values, key: f.key.clone() }, f)
        })
        .collect();
    let (order, from) = match before {
        Some(p) => (sort::reversed(&fields), Some(p)),
        None => (fields, after),
    };
    let cmp =
        |a: &Place, b: &Place| sort::compare_all((&a.values, &a.key), (&b.values, &b.key), &order);
    placed.sort_by(|a, b| cmp(&a.0, &b.0));
    if let Some(from) = from {
        placed.retain(|(p, _)| cmp(p, &from) == Ordering::Greater);
    }
    Ok(placed
        .into_iter()
        .map(|(p, mut f)| {
            f.token = Some(encode(&p, by_score));
            f
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips() {
        for n in 0..8u8 {
            let b: Vec<u8> = (0..n).map(|i| i.wrapping_mul(97)).collect();
            assert_eq!(from_base64(&base64(&b)), Some(b));
        }
        assert_eq!(base64(b"\x08\x05"), "CAU=");
        assert_eq!(from_base64("abc"), None);
        assert_eq!(from_base64("a==="), None);
        assert_eq!(from_base64("ab*="), None);
    }

    #[test]
    fn messages_read_back() {
        let values = [
            SortValue::Missing,
            SortValue::Score(0.5),
            SortValue::Key(Key::Number(-1.5)),
            SortValue::Key(Key::Date(-86_400_000)),
            SortValue::Key(Key::Bool(true)),
            SortValue::Key(Key::ObjectId([7; 12])),
            SortValue::Key(Key::Uuid(vec![1, 2])),
            SortValue::Key(Key::Text("b".to_owned())),
        ];
        for v in values {
            assert_eq!(decode_value(&encode_value(&v)), Some(v));
        }
        assert_eq!(
            decode_value(&encode_value(&SortValue::Key(Key::Null))),
            Some(SortValue::Missing)
        );
        // The reference search server's token of a sort on a number reads as its value and document.
        let p = decode("CAUaCSEAAAAAAAD4vyICEAo=", false).expect("read");
        assert_eq!(p.values, vec![SortValue::Key(Key::Number(-1.5))]);
        assert_eq!(decode("CMlJFWM8A6P+", true), None);
        assert_eq!(
            decode("CAUVklIfPiICEAo=", true).map(|p| p.values),
            Some(vec![SortValue::Score(0.15558842)])
        );
        assert_eq!(fields(&[0x80]).map(|f| f.len()), None);
        assert_eq!(fields(&[0x08]).map(|f| f.len()), None);
        assert_eq!(fields(&[0x00, 0x01]).map(|f| f.len()), None);
        assert_eq!(decode_value(&[0x3a, 0x01, 0x00]), None);
        assert_eq!(decode_value(&[0x4a, 0x01, 0xff]), None);
        assert_eq!(decode_value(&[0x10, 0x01]), None);
    }
}
