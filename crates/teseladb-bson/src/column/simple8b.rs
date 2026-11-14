//! Simple-8b blocks of a BSON column, as a reference router 9.0.4 reads them (measured, 003 T029).
//!
//! The low four bits of a block are its selector:
//! - `1` to `14`: as many slots as fit the other 60 bits (60 of 1 bit, 30 of 2, …, 1 of 60);
//!   selectors `7` and `8` with a zero extension (bits 4 to 7) keep their slots in bits 8 to 63
//!   (8 of 7 bits, 7 of 8 bits);
//! - `7` and `8` with an extension `e`: the 56 bits from bit 8 hold `k` slots of `56 / k` bits,
//!   each one a value over a shift in its low bits: four bits counting single bits for `7`
//!   (`e` 1 to 9), four bits counting nibbles for `8` with `e` 1 to 7, five bits counting
//!   nibbles for `8` with `e` 8 to 13;
//! - `15`: the last slot of the block before, again `120 × (bits 4 to 7 + 1)` times;
//! - `0` and any other extension: refused.
//!
//! A slot with all its bits set has no value. The others are zigzag deltas, shifted back first;
//! a shift past the width wraps around it, as the reference server's shifts do.

use super::ColumnError;

/// Slots of selectors `1` to `14`; `0` is refused and `15` repeats.
const SLOTS: [u32; 15] = [0, 60, 30, 20, 15, 12, 10, 8, 7, 6, 5, 4, 3, 2, 1];
/// Slots of the extensions of selector `7`, by extension (`0` is the plain selector).
const SLOTS_7: [u32; 10] = [8, 9, 8, 7, 6, 5, 4, 3, 2, 1];
/// Slots of the extensions of selector `8`, by extension (`0` is the plain selector).
const SLOTS_8: [u32; 14] = [7, 7, 6, 5, 4, 3, 2, 1, 6, 5, 4, 3, 2, 1];
/// Selector that repeats the last slot.
const REPEAT: u64 = 15;
/// Times a repeat block repeats, per unit of its count.
const REPEAT_UNIT: usize = 120;
/// First extension of selector `8` with a five-bit shift.
const WIDE_SHIFT: u64 = 8;
/// Bits of the extended payload, from bit 8.
const EXTENDED_BITS: u32 = 56;
/// Bits of the plain payload, from bit 4.
const PLAIN_BITS: u32 = 60;

/// What one block holds.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Block {
    /// Its slots, `None` where a slot has no value; each one a zigzag delta of `bits` bits.
    Slots(Vec<Option<u128>>),
    /// The last slot of the block before, this many times.
    Repeat(usize),
}

/// The slots of `word` for values of `bits` bits (64 or 128).
pub(super) fn block(word: u64, bits: u32) -> Result<Block, ColumnError> {
    let selector = word & 0x0F;
    let extension = (word >> 4) & 0x0F;
    match selector {
        0 => Err(ColumnError("invalid selector 0 while loading simple8b block")),
        REPEAT => Ok(Block::Repeat(REPEAT_UNIT * (extension as usize + 1))),
        7 | 8 if extension != 0 => extended(word, selector, extension, bits),
        7 | 8 => {
            let count = if selector == 7 { SLOTS_7[0] } else { SLOTS_8[0] };
            Ok(Block::Slots(plain(word >> 8, count, EXTENDED_BITS / count)))
        }
        _ => {
            let count = SLOTS[selector as usize];
            Ok(Block::Slots(plain(word >> 4, count, PLAIN_BITS / count)))
        }
    }
}

/// `count` slots of `width` bits from the low bits of `payload`.
fn plain(payload: u64, count: u32, width: u32) -> Vec<Option<u128>> {
    (0..count).map(|j| slot(payload, j, width).map(u128::from)).collect()
}

/// Slot `j` of `width` bits, `None` when all its bits are set.
fn slot(payload: u64, j: u32, width: u32) -> Option<u64> {
    let mask = u64::MAX >> (64 - width);
    let s = (payload >> (j * width)) & mask;
    (s != mask).then_some(s)
}

#[expect(clippy::cast_possible_truncation, reason = "the shift is masked to 4 or 5 bits")]
fn extended(word: u64, selector: u64, extension: u64, bits: u32) -> Result<Block, ColumnError> {
    let table: &[u32] = if selector == 7 { &SLOTS_7 } else { &SLOTS_8 };
    let count = *usize::try_from(extension)
        .ok()
        .and_then(|i| table.get(i))
        .ok_or(ColumnError("invalid extended selector while loading simple8b block"))?;
    let width = EXTENDED_BITS / count;
    let shift_bits = if selector == 8 && extension >= WIDE_SHIFT { 5 } else { 4 };
    let unit = if selector == 7 { 1 } else { 4 };
    let slots = (0..count)
        .map(|j| {
            slot(word >> 8, j, width).map(|s| {
                let shift = (s & ((1 << shift_bits) - 1)) as u32 * unit % bits;
                let value = u128::from(s >> shift_bits) << shift;
                if bits == 128 { value } else { value & u128::from(u64::MAX) }
            })
        })
        .collect();
    Ok(Block::Slots(slots))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_selectors_have_the_slots_the_reference_reads() {
        for (selector, count) in SLOTS.iter().enumerate().skip(1) {
            let zeros = vec![Some(0); *count as usize];
            assert_eq!(block(selector as u64, 64), Ok(Block::Slots(zeros)), "{selector}");
        }
        assert_eq!(
            block(0, 64),
            Err(ColumnError("invalid selector 0 while loading simple8b block"))
        );
        assert_eq!(block(0x3F, 64), Ok(Block::Repeat(480)));
    }

    #[test]
    fn extended_selectors_shift_their_values() {
        // Selector 7, extension 7: three slots of 18 bits, 14 of value over 4 of shift (measured
        // on a column of a reference router 9.0.4 holding 1 000 000 steps).
        assert_eq!(block(0x3D09_7F42_5FD0_9777, 64), Ok(Block::Slots(vec![Some(2_000_000); 3])));
        // A shift of 16 nibbles wraps around 64 bits, not around 128.
        let word = 8 | 10 << 4 | ((1 << 5) | 16) << 8;
        assert_eq!(block(word, 64), Ok(Block::Slots(vec![Some(1), Some(0), Some(0), Some(0)])));
        let wide = vec![Some(1 << 64), Some(0), Some(0), Some(0)];
        assert_eq!(block(word, 128), Ok(Block::Slots(wide)));
        assert!(block(7 | 10 << 4, 64).is_err());
        assert!(block(8 | 14 << 4, 64).is_err());
        // A slot with every bit set has no value.
        assert_eq!(block(u64::MAX, 64), Ok(Block::Repeat(1920)));
        assert_eq!(block(0xFFFF_FFFF_FFFF_FF00 | 8 | 7 << 4, 64), Ok(Block::Slots(vec![None])));
    }
}
