//! One stream of values of a BSON column: the reference value, the integer its deltas move and how
//! each type turns that integer back into a value (measured on a reference router 9.0.4, 003 T029).
//!
//! - 64-bit integers: `int32` (wrapping in 32 bits), `int64`, `bool` (any integer but 0 is
//!   true), `double` (its bits, or its value times 1, 10, 100, 10 000 or 10⁸ as the control byte
//!   says), and, with deltas of deltas, `date`, `timestamp` (its eight bytes) and
//!   `ObjectId` (bytes 0, 1, 9, 2, 10, 3 and 11 from the high end; the other five stay).
//! - 128-bit integers: `string`, JavaScript and symbol (their bytes, big-endian; more than 16
//!   bytes or a leading zero byte count as 0), binary data of at most 16 bytes (its bytes,
//!   little-endian, cut to its length) and `decimal128` (its bytes, little-endian). A zero delta
//!   repeats the value before.
//! - Any other type takes only zero deltas, which repeat it.

use super::simple8b::{self, Block};
use super::{Cell, ColumnError, Sink};
use crate::types;

/// Multipliers of the scaled doubles, by scale index (control bytes `90` to `D0`).
const SCALES: [f64; 5] = [1.0, 10.0, 100.0, 10_000.0, 100_000_000.0];
/// Control byte of plain blocks, the only one most types take.
const PLAIN: u8 = 0x80;
/// Largest scaled double the reference server keeps: 2⁵³.
const MAX_SCALED: f64 = 9_007_199_254_740_992.0;
/// Bytes of a value a 128-bit integer can hold.
const WIDE_BYTES: usize = 16;

const BAD_DOUBLE: ColumnError = ColumnError("Invalid double encoding in BSON Column");

/// Width of the integers of a type.
fn wide(kind: u8) -> bool {
    matches!(
        kind,
        types::STRING | types::JAVASCRIPT | types::SYMBOL | types::BINARY | types::DECIMAL128
    )
}

/// Decoding state of one stream.
#[derive(Default)]
pub(super) struct Stream {
    /// The reference value (the last literal); none before the first one.
    reference: Option<Cell>,
    /// The integer the deltas move; none for the types that only repeat.
    int: Option<u128>,
    /// The last delta, for the types that take deltas of deltas.
    delta: u64,
    /// The last slot, for repeat blocks.
    last: Option<u128>,
    /// Scale index of a double; none while it is kept as its bits.
    scale: Option<usize>,
    /// The last value of a 128-bit stream, which a zero delta repeats.
    prev: Option<Cell>,
}

impl Stream {
    /// A stream whose reference is `cell`.
    pub(super) fn new(cell: Cell) -> Stream {
        Stream {
            int: integer(&cell),
            prev: Some(cell.clone()),
            reference: Some(cell),
            last: Some(0),
            ..Stream::default()
        }
    }

    /// Decodes the blocks of a group with control byte `ctl` into `out`.
    pub(super) fn group(
        &mut self,
        ctl: u8,
        words: impl Iterator<Item = u64>,
        out: &mut impl Sink,
    ) -> Result<(), ColumnError> {
        let kind = self.reference.as_ref().map(|c| c.kind);
        let wide = kind.is_some_and(wide);
        self.start(ctl & 0xF0, kind, wide)?;
        let bits = if wide { 128 } else { 64 };
        for word in words {
            match simple8b::block(word, bits)? {
                Block::Repeat(n) => {
                    for _ in 0..n {
                        out.take(self.apply(self.last)?)?;
                    }
                }
                Block::Slots(slots) => {
                    for s in slots {
                        out.take(self.apply(s)?)?;
                        self.last = s;
                    }
                }
            }
        }
        Ok(())
    }

    /// The checks of a control byte against the stream's type, before its blocks.
    fn start(&mut self, ctl: u8, kind: Option<u8>, wide: bool) -> Result<(), ColumnError> {
        if wide {
            return if ctl == PLAIN {
                Ok(())
            } else {
                Err(ColumnError("Invalid control byte in 128-bit BSON Column"))
            };
        }
        let scale = match ctl {
            PLAIN => None,
            0x90..=0xD0 => Some(usize::from((ctl >> 4) - 9)),
            _ => return Err(ColumnError("Invalid control byte in 64-bit BSON Column")),
        };
        if kind != Some(types::DOUBLE) {
            return match scale {
                None => Ok(()),
                Some(_) => Err(ColumnError("Unexpected control for type in BSONColumn")),
            };
        }
        if scale != self.scale {
            self.rescale(scale)?;
        }
        Ok(())
    }

    /// Moves a double to scale `scale`; it has to come back the same.
    #[expect(
        clippy::cast_precision_loss,
        reason = "a scaled double is an integer below 2^53 that the reference server's encoder checked comes back the same"
    )]
    fn rescale(&mut self, scale: Option<usize>) -> Result<(), ColumnError> {
        let bits = self.int.map(super::low64).ok_or(BAD_DOUBLE)?;
        let current = match self.scale {
            None => f64::from_bits(bits),
            Some(s) => bits.cast_signed() as f64 / SCALES[s],
        };
        let next = match scale {
            None => current.to_bits(),
            Some(s) => scaled(current, SCALES[s])?.cast_unsigned(),
        };
        self.int = Some(u128::from(next));
        self.scale = scale;
        Ok(())
    }

    /// The value of one slot.
    fn apply(&mut self, slot: Option<u128>) -> Result<Option<Cell>, ColumnError> {
        let (Some(slot), Some(reference)) = (slot, &self.reference) else {
            return Ok(None);
        };
        let kind = reference.kind;
        let Some(int) = self.int else {
            if slot != 0 && kind == types::BINARY {
                return Err(ColumnError("BinData length should not exceed 16 in a delta encoding"));
            }
            if slot != 0 {
                return Err(ColumnError("Invalid delta in 64-bit BSON Column encoding"));
            }
            return Ok(Some(reference.clone()));
        };
        if wide(kind) {
            if slot != 0 {
                let next = int.wrapping_add(unzigzag128(slot));
                self.int = Some(next);
                self.prev = Some(Cell { kind, bytes: wide_value(reference, next) });
            }
            return Ok(self.prev.clone());
        }
        let mut d = unzigzag64(super::low64(slot));
        if matches!(kind, types::DATE_TIME | types::TIMESTAMP | types::OBJECT_ID) {
            self.delta = self.delta.wrapping_add(d);
            d = self.delta;
        }
        let next = super::low64(int).wrapping_add(d);
        self.int = Some(u128::from(next));
        Ok(Some(Cell { kind, bytes: narrow_value(reference, next, self.scale) }))
    }
}

/// `d` times `m` as the reference server keeps a scaled double, if it comes back as `d`.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::float_cmp,
    reason = "x is finite and within MAX_SCALED, so it rounds to an i64; the exact comparison is the test that the scaling is lossless"
)]
fn scaled(d: f64, m: f64) -> Result<i64, ColumnError> {
    let x = d * m;
    if !x.is_finite() || x.abs() > MAX_SCALED {
        return Err(BAD_DOUBLE);
    }
    let e = x.round() as i64;
    if e as f64 / m == d { Ok(e) } else { Err(BAD_DOUBLE) }
}

fn unzigzag64(u: u64) -> u64 {
    (u >> 1) ^ (u & 1).wrapping_neg()
}

fn unzigzag128(u: u128) -> u128 {
    (u >> 1) ^ (u & 1).wrapping_neg()
}

/// Positions in an `ObjectId` of the bytes of its integer, from the low end.
const OID_BYTES: [usize; 7] = [11, 3, 10, 2, 9, 1, 0];

/// The integer of a reference value, if its type has one.
fn integer(cell: &Cell) -> Option<u128> {
    let b = &cell.bytes;
    let le = |n: usize| {
        let mut v = [0u8; 16];
        v[..n].copy_from_slice(&b[..n]);
        u128::from_le_bytes(v)
    };
    match cell.kind {
        types::INT32 => Some(u128::from(i64::from(i32::from_le_bytes(word(b))).cast_unsigned())),
        types::INT64 | types::DATE_TIME | types::DOUBLE | types::TIMESTAMP => Some(le(8)),
        types::BOOLEAN => Some(u128::from(b[0])),
        types::OBJECT_ID => {
            Some(OID_BYTES.iter().rev().fold(0, |acc, &i| acc << 8 | u128::from(b[i])))
        }
        types::STRING | types::JAVASCRIPT | types::SYMBOL => {
            let text = &b[4..b.len() - 1];
            let fits = text.len() <= WIDE_BYTES && text.first() != Some(&0);
            Some(if fits { text.iter().fold(0, |acc, &c| acc << 8 | u128::from(c)) } else { 0 })
        }
        types::BINARY => {
            let n = b.len() - 5;
            (n <= WIDE_BYTES).then(|| u128::from_le_bytes(pad(&b[5..])))
        }
        types::DECIMAL128 => Some(le(16)),
        _ => None,
    }
}

fn word(b: &[u8]) -> [u8; 4] {
    b[..4].try_into().expect("four bytes")
}

/// `b` (at most 16 bytes) in the low bytes of 16.
fn pad(b: &[u8]) -> [u8; 16] {
    let mut v = [0u8; 16];
    v[..b.len()].copy_from_slice(b);
    v
}

/// The bytes of a 64-bit type's value for integer `v`.
#[expect(clippy::cast_precision_loss, reason = "a scaled double is an integer below 2^53")]
fn narrow_value(reference: &Cell, v: u64, scale: Option<usize>) -> Vec<u8> {
    match reference.kind {
        types::INT32 => v.to_le_bytes()[..4].to_vec(),
        types::DOUBLE => match scale {
            None => v.to_le_bytes().to_vec(),
            Some(s) => (v.cast_signed() as f64 / SCALES[s]).to_le_bytes().to_vec(),
        },
        types::BOOLEAN => vec![u8::from(v != 0)],
        types::OBJECT_ID => {
            let mut out = reference.bytes.clone();
            for (k, &i) in OID_BYTES.iter().enumerate() {
                out[i] = (v >> (8 * k)).to_le_bytes()[0];
            }
            out
        }
        _ => v.to_le_bytes().to_vec(),
    }
}

/// The bytes of a 128-bit type's value for integer `v`.
fn wide_value(reference: &Cell, v: u128) -> Vec<u8> {
    match reference.kind {
        types::BINARY => {
            let mut out = reference.bytes.clone();
            let le = v.to_le_bytes();
            for (k, byte) in out[5..].iter_mut().enumerate() {
                *byte = le.get(k).copied().unwrap_or(0);
            }
            out
        }
        types::DECIMAL128 => v.to_le_bytes().to_vec(),
        _ => {
            let be = v.to_be_bytes();
            let start = be.iter().position(|&c| c != 0).unwrap_or(be.len());
            let text = &be[start..];
            // At most 16 bytes and the terminator.
            let mut out = i32::try_from(text.len() + 1).unwrap_or(i32::MAX).to_le_bytes().to_vec();
            out.extend_from_slice(text);
            out.push(0);
            out
        }
    }
}
