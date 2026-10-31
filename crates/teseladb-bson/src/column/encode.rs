//! Writing BSON columns (015 FR-006, T012): the values of one field of a time series bucket, row
//! by row, as a column that [`super::decode`] and RefDb's unpacking read back the same.
//!
//! The layout is the one [`super`] describes, kept to its simplest parts: a value of a type with
//! an integer ([`super::stream`]) becomes a zigzag delta of the reference before it (a delta of
//! deltas for dates, timestamps and ObjectIds), packed with the plain Simple-8b selectors; a value
//! of another type, a type change, a delta that needs more than 60 bits, or a value the integer
//! cannot rebuild (a string of more than 16 bytes, a binary of another subtype or length) becomes
//! a new literal. A row without value is a slot with every bit set. The bytes are not RefDb's
//! own (it also scales doubles and interleaves objects), but every value comes back the same.

use super::{BLOCK, Cell, END};
use crate::types;

/// Control byte of a group of plain blocks: its low nibble holds the blocks minus one.
const PLAIN: u8 = 0x80;
/// Most blocks one control byte heads.
const MAX_BLOCKS: usize = 16;
/// Slots of the plain selectors `1` to `14`, by selector (index 0 unused).
const SLOTS: [u32; 15] = [0, 60, 30, 20, 15, 12, 10, 8, 7, 6, 5, 4, 3, 2, 1];
/// Selectors whose slots start at bit 8, with extensions.
const EXTENDED: [usize; 2] = [7, 8];
/// Bits of a plain block's payload.
const PAYLOAD_BITS: u32 = 60;
/// Most bytes of a string or binary a 128-bit integer rebuilds.
const WIDE_BYTES: usize = 16;

/// What the next delta builds on: the reference's type, its integer and, for the types with
/// deltas of deltas, the last delta.
struct Reference {
    cell: Cell,
    int: u128,
    delta: u64,
}

/// The column of `rows`, one per row (`None`: no value).
pub fn encode(rows: &[Option<Cell>]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut slots: Vec<u128> = Vec::new();
    let mut reference: Option<Reference> = None;
    for row in rows {
        let Some(cell) = row else {
            slots.push(u128::MAX);
            continue;
        };
        match reference.as_mut().and_then(|r| r.slot(cell)) {
            Some(slot) => slots.push(slot),
            None => {
                flush(&mut out, &mut slots);
                literal(&mut out, cell);
                reference = Some(Reference::new(cell));
            }
        }
    }
    flush(&mut out, &mut slots);
    out.push(END);
    out
}

impl Reference {
    fn new(cell: &Cell) -> Reference {
        Reference { cell: cell.clone(), int: integer(cell).unwrap_or(0), delta: 0 }
    }

    /// The slot that turns the reference into `cell`, moving the reference along; `None` when
    /// `cell` needs a literal.
    fn slot(&mut self, cell: &Cell) -> Option<u128> {
        if cell.kind != self.cell.kind {
            return None;
        }
        let Some(next) = integer(cell) else {
            // Types without an integer take only zero deltas: the same value again.
            return (*cell == self.cell).then_some(0);
        };
        if !rebuilds(&self.cell, cell) {
            return None;
        }
        if wide(cell.kind) {
            let slot = zigzag128(next.wrapping_sub(self.int));
            return fits(slot).then(|| {
                self.int = next;
                slot
            });
        }
        let d = (next as u64).wrapping_sub(self.int as u64);
        let step = if deltas_of_deltas(cell.kind) { d.wrapping_sub(self.delta) } else { d };
        let slot = u128::from(zigzag64(step));
        fits(slot).then(|| {
            self.int = next;
            if deltas_of_deltas(cell.kind) {
                self.delta = d;
            }
            slot
        })
    }
}

/// Whether the integer of `cell` gives back `cell` from reference `reference` (same type): the
/// decoder keeps the reference's subtype and length for binaries and the unused bytes of an
/// ObjectId, and strings without a leading zero byte of at most 16 bytes.
fn rebuilds(reference: &Cell, cell: &Cell) -> bool {
    match cell.kind {
        types::STRING | types::JAVASCRIPT | types::SYMBOL => {
            let text = &cell.bytes[4..cell.bytes.len() - 1];
            text.len() <= WIDE_BYTES && text.first() != Some(&0)
        }
        types::BINARY => {
            cell.bytes.len() == reference.bytes.len() && cell.bytes[..5] == reference.bytes[..5]
        }
        types::OBJECT_ID => [4, 5, 6, 7, 8].iter().all(|&i| cell.bytes[i] == reference.bytes[i]),
        _ => true,
    }
}

/// Whether a slot fits a 60-bit slot without being the "no value" pattern.
fn fits(slot: u128) -> bool {
    slot < (1u128 << PAYLOAD_BITS) - 1
}

fn deltas_of_deltas(kind: u8) -> bool {
    matches!(kind, types::DATE_TIME | types::TIMESTAMP | types::OBJECT_ID)
}

fn wide(kind: u8) -> bool {
    matches!(
        kind,
        types::STRING | types::JAVASCRIPT | types::SYMBOL | types::BINARY | types::DECIMAL128
    )
}

fn zigzag64(v: u64) -> u64 {
    (v << 1) ^ ((v as i64) >> 63) as u64
}

fn zigzag128(v: u128) -> u128 {
    (v << 1) ^ ((v as i128) >> 127) as u128
}

/// Positions in an ObjectId of the bytes of its integer, from the low end (as the decoder).
const OID_BYTES: [usize; 7] = [11, 3, 10, 2, 9, 1, 0];

/// The integer of `cell` as the decoder builds it, if its type has one it can move.
fn integer(cell: &Cell) -> Option<u128> {
    let b = &cell.bytes;
    let le = |n: usize| {
        let mut v = [0u8; 16];
        v[..n].copy_from_slice(&b[..n]);
        u128::from_le_bytes(v)
    };
    match cell.kind {
        types::INT32 => {
            let w: [u8; 4] = b.get(..4)?.try_into().ok()?;
            Some(u128::from(i32::from_le_bytes(w) as i64 as u64))
        }
        types::INT64 | types::DATE_TIME | types::DOUBLE | types::TIMESTAMP => Some(le(8)),
        types::BOOLEAN => Some(u128::from(b[0])),
        types::OBJECT_ID => {
            Some(OID_BYTES.iter().rev().fold(0, |acc, &i| acc << 8 | u128::from(b[i])))
        }
        types::STRING | types::JAVASCRIPT | types::SYMBOL => {
            // A text the integer cannot hold counts as 0, as the decoder takes it.
            let text = &b[4..b.len() - 1];
            let fits = text.len() <= WIDE_BYTES && text.first() != Some(&0);
            Some(if fits { text.iter().fold(0, |acc, &c| acc << 8 | u128::from(c)) } else { 0 })
        }
        types::BINARY => {
            let data = &b[5..];
            (data.len() <= WIDE_BYTES).then(|| {
                let mut v = [0u8; 16];
                v[..data.len()].copy_from_slice(data);
                u128::from_le_bytes(v)
            })
        }
        types::DECIMAL128 => Some(le(16)),
        _ => None,
    }
}

/// Writes `cell` as a literal: its type, an empty name and its value.
fn literal(out: &mut Vec<u8>, cell: &Cell) {
    out.push(cell.kind);
    out.push(0);
    out.extend_from_slice(&cell.bytes);
}

/// Writes the pending `slots` as groups of plain blocks.
fn flush(out: &mut Vec<u8>, slots: &mut Vec<u128>) {
    let mut blocks = Vec::new();
    let mut rest = slots.as_slice();
    while !rest.is_empty() {
        let (word, used) = block(rest);
        blocks.push(word);
        rest = &rest[used..];
    }
    for group in blocks.chunks(MAX_BLOCKS) {
        out.push(PLAIN | (group.len() - 1) as u8);
        for word in group {
            out.extend_from_slice(&word.to_le_bytes()[..BLOCK]);
        }
    }
    slots.clear();
}

/// The block that packs the most of the first `slots` (each under `2⁶⁰ - 1`, or `u128::MAX` for
/// no value), and how many it took: the selector with the most slots that all fit, down to the
/// single slot of 60 bits, which takes any of them.
fn block(slots: &[u128]) -> (u64, usize) {
    let fit = |selector: usize| {
        let count = SLOTS[selector] as usize;
        let mask = (1u128 << (PAYLOAD_BITS / SLOTS[selector])) - 1;
        count <= slots.len() && slots[..count].iter().all(|&s| s == u128::MAX || s < mask)
    };
    // Selectors 7 and 8 keep their slots elsewhere (they have extensions): not used here.
    let selector = (1..SLOTS.len() - 1)
        .filter(|s| !EXTENDED.contains(s))
        .find(|&s| fit(s))
        .unwrap_or(SLOTS.len() - 1);
    let count = SLOTS[selector] as usize;
    let width = PAYLOAD_BITS / SLOTS[selector];
    let mask = (1u128 << width) - 1;
    let payload = slots[..count].iter().enumerate().fold(0u64, |acc, (j, &s)| {
        let v = if s == u128::MAX { mask } else { s };
        acc | ((v as u64) << (j as u32 * width))
    });
    ((payload << 4) | selector as u64, count)
}
