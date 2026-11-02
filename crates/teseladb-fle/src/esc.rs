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
    /// Reads the documents with `_id` in `ids` together, ahead of the probes that will ask for
    /// them with [`EscReader::get`]: a reader whose reads are rounds to another thread or node
    /// keeps them and answers those probes without one (017 SC-002). The probes and what they
    /// find stay the same.
    fn read_ahead(&mut self, ids: &[Token]) -> Result<(), Self::Error> {
        let _ = ids;
        Ok(())
    }
}

/// Positions read ahead in the first round of a search, and the least of each later round: the
/// search from 1 probes 1, 3, 7, 15, 31, so a value written up to 30 times takes one round.
pub const READ_AHEAD: u64 = 31;
/// Anchors read ahead in the first round: compaction leaves few.
const ANCHORS_AHEAD: u64 = 2;

/// The numbers from `from` a search has read ahead, up to `to`; each round past them reads at
/// least as many again as the search has covered, so a search of `n` takes `log2(n)` rounds.
struct Ahead {
    from: u64,
    to: u64,
}

impl Ahead {
    /// Reads ahead the documents of `id` for the numbers up to `n` and beyond, if not yet read.
    fn cover<R: EscReader>(
        &mut self,
        reader: &mut R,
        n: u64,
        id: impl Fn(u64) -> Token,
    ) -> Result<(), R::Error> {
        if n <= self.to {
            return Ok(());
        }
        let covered = self.to.saturating_add(1).saturating_sub(self.from);
        let to = n.max(self.to.saturating_add(READ_AHEAD.max(covered)));
        let ids: Vec<Token> = (self.to + 1..=to).map(id).collect();
        self.to = to;
        reader.read_ahead(&ids)
    }
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
/// The searches read ahead (`EscReader::read_ahead`): the null anchor, the first anchors and the
/// first positions in one round, which is all a value written a few times needs.
pub fn state<R: EscReader>(reader: &mut R, keys: &EscKeys) -> Result<EscState, R::Error> {
    let mut first: Vec<Token> = (0..=ANCHORS_AHEAD).map(|a| keys.anchor_id(a)).collect();
    first.extend((1..=READ_AHEAD).map(|p| keys.position_id(p)));
    reader.read_ahead(&first)?;
    let null = reader.get(&keys.anchor_id(0))?.and_then(|v| keys.read_anchor(&v));
    let first_anchor = null.map_or(1, |(a, _)| a + 1);
    let mut anchors = Ahead {
        from: first_anchor,
        to: if first_anchor == 1 { ANCHORS_AHEAD } else { first_anchor - 1 },
    };
    let apos = emu_binary(first_anchor, |a| {
        anchors.cover(reader, a, |n| keys.anchor_id(n))?;
        Ok(reader.get(&keys.anchor_id(a))?.is_some())
    })?;
    let anchored = if apos >= first_anchor {
        let value = reader.get(&keys.anchor_id(apos))?.unwrap_or_default();
        keys.read_anchor(&value).map_or(0, |(_, c)| c)
    } else {
        null.map_or(0, |(_, c)| c)
    };
    let mut positions =
        Ahead { from: anchored + 1, to: if anchored == 0 { READ_AHEAD } else { anchored } };
    let count = emu_binary(anchored + 1, |p| {
        positions.cover(reader, p, |n| keys.position_id(n))?;
        Ok(reader.get(&keys.position_id(p))?.is_some())
    })?;
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

    fn read_ahead(&mut self, ids: &[Token]) -> Result<(), R::Error> {
        let unwritten: Vec<Token> =
            ids.iter().filter(|id| !self.written.contains(id)).copied().collect();
        self.inner.read_ahead(&unwritten)
    }
}
