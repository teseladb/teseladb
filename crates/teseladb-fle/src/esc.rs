//! The encrypted state collection (ESC): how many times each value of a field was written, kept
//! without saying which value (measured on a `refnode` 9.0.4 with known keys).
//!
//! For a value and contention factor, with `tag = ESCTwiceDerivedTagToken` and `value =
//! ESCTwiceDerivedValueToken`:
//!
//! - each write adds `{_id: HMAC(tag, pos)}` at the next position, from 1;
//! - compaction replaces the positions it read with an anchor `{_id: HMAC(tag, 0 || apos), value:
//!   Encrypt(value, 0 || cpos)}`, anchors numbered from 1 and `cpos` the last position;
//! - cleanup replaces everything with the null anchor `{_id: HMAC(tag, 0 || 0), value:
//!   Encrypt(value, apos || cpos)}`, keeping the last anchor's number and the last position.
//!
//! Positions and anchors are contiguous, so the last one is found with an exponential search
//! followed by a binary one (the "EmuBinary" of the protocol).

use crate::cipher;
use crate::tokens::{self, Token};

/// The `_id` and value keys of one value's ESC documents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EscKeys {
    /// `ESCTwiceDerivedTagToken`.
    pub tag: Token,
    /// `ESCTwiceDerivedValueToken`.
    pub value: Token,
}

impl EscKeys {
    /// The keys of an `ESCDerivedFromDataTokenAndContentionFactor`.
    pub fn of(esc_cf: &Token) -> EscKeys {
        EscKeys {
            tag: tokens::esc_twice_derived_tag(esc_cf),
            value: tokens::esc_twice_derived_value(esc_cf),
        }
    }

    /// The `_id` of the write at position `pos` (from 1).
    pub fn position_id(&self, pos: u64) -> Token {
        self.tag.derive_u64(pos)
    }

    /// The `_id` of anchor `apos` (from 1; 0 is the null anchor).
    pub fn anchor_id(&self, apos: u64) -> Token {
        self.tag.derive_pair(0, apos)
    }

    /// The value of an anchor whose last position is `cpos`, or of the null anchor with the
    /// number of the last anchor in `apos`.
    pub fn anchor_value(&self, apos: u64, cpos: u64, iv: [u8; cipher::IV_LEN]) -> Vec<u8> {
        let mut plain = [0u8; 16];
        plain[..8].copy_from_slice(&apos.to_le_bytes());
        plain[8..].copy_from_slice(&cpos.to_le_bytes());
        cipher::encrypt(&self.value, iv, &plain)
    }

    /// The two numbers of an anchor's value; `None` if it is not one.
    pub fn read_anchor(&self, sealed: &[u8]) -> Option<(u64, u64)> {
        let plain = cipher::decrypt(&self.value, sealed)?;
        let (a, b) = plain.split_first_chunk::<8>()?;
        let b: [u8; 8] = b.try_into().ok()?;
        Some((u64::from_le_bytes(*a), u64::from_le_bytes(b)))
    }
}

/// Reads ESC documents by `_id`.
pub trait EscReader {
    /// The error of a read.
    type Error;
    /// The document with `_id` `id`: `None` if there is none, else its `value` (empty for a
    /// position).
    fn get(&mut self, id: &Token) -> Result<Option<Vec<u8>>, Self::Error>;
}

/// What the ESC holds for one value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EscState {
    /// The null anchor's numbers, if a cleanup left one.
    pub null: Option<(u64, u64)>,
    /// The last anchor, 0 for none (after the null anchor's).
    pub apos: u64,
    /// The last position compaction recorded (in the last anchor or the null one).
    pub anchored: u64,
    /// The last position written.
    pub count: u64,
}

/// The last `k >= from - 1` such that `exists(from ..= k)`, for an `exists` true on a prefix of
/// the numbers from `from`.
pub fn emu_binary<E>(from: u64, mut exists: impl FnMut(u64) -> Result<bool, E>) -> Result<u64, E> {
    let mut found = from - 1;
    let mut step = 1u64;
    // Doubles until a gap.
    loop {
        let probe = found + step;
        if !exists(probe)? {
            break;
        }
        found = probe;
        step = step.saturating_mul(2);
    }
    // The last one is in (found, found + step).
    let (mut lo, mut hi) = (found, found + step);
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if exists(mid)? {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

/// What the ESC holds for the value of `keys`.
pub fn state<R: EscReader>(reader: &mut R, keys: &EscKeys) -> Result<EscState, R::Error> {
    let null = reader.get(&keys.anchor_id(0))?.and_then(|v| keys.read_anchor(&v));
    let first_anchor = null.map_or(1, |(a, _)| a + 1);
    let apos = emu_binary(first_anchor, |a| Ok(reader.get(&keys.anchor_id(a))?.is_some()))?;
    let anchored = if apos >= first_anchor {
        let value = reader.get(&keys.anchor_id(apos))?.unwrap_or_default();
        keys.read_anchor(&value).map_or(0, |(_, c)| c)
    } else {
        null.map_or(0, |(_, c)| c)
    };
    let count = emu_binary(anchored + 1, |p| Ok(reader.get(&keys.position_id(p))?.is_some()))?;
    Ok(EscState { null, apos, anchored, count })
}

/// An [`EscReader`] that also sees the positions an operation wrote before it commits.
pub struct Overlay<'r, R> {
    inner: &'r mut R,
    written: Vec<Token>,
}

impl<'r, R: EscReader> Overlay<'r, R> {
    /// Over `inner`, with nothing written yet.
    pub fn new(inner: &'r mut R) -> Overlay<'r, R> {
        Overlay { inner, written: Vec::new() }
    }

    /// Records a position the operation inserts.
    pub fn write(&mut self, id: Token) {
        self.written.push(id);
    }

    /// The positions written, in order.
    pub fn written(&self) -> &[Token] {
        &self.written
    }

    /// The positions written, given up.
    pub fn into_written(self) -> Vec<Token> {
        self.written
    }
}

impl<R: EscReader> EscReader for Overlay<'_, R> {
    type Error = R::Error;

    fn get(&mut self, id: &Token) -> Result<Option<Vec<u8>>, R::Error> {
        if self.written.contains(id) {
            return Ok(Some(Vec::new()));
        }
        self.inner.get(id)
    }
}
