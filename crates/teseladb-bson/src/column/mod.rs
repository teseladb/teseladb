//! BSON columns: binary subtype 7, the compressed columns of RefDb's time series buckets
//!.
//!
//! Everything here comes from measuring a `refrouter` 9.0.4: the columns it writes into its own
//! buckets, the values `$_internalUnpackBucket` reads back from columns made by hand, and which
//! binaries it refuses with 378 `NonConformantBSON` on a message.
//!
//! A column is a run of control bytes ended by `00`:
//! - a literal: a BSON element with an empty name (types `01` to `13`, `7F` and `FF`), the
//!   reference that the deltas after it build on;
//! - a group of Simple-8b blocks: the control byte holds a scale in its high nibble and the
//!   number of 8-byte blocks minus one in its low nibble; each block holds zigzag deltas
//!   (`simple8b`);
//! - interleaved mode (`F0`, `F1`, `F2`): a reference document whose scalars become streams that
//!   take blocks in turn until a `00` (`interleaved`).
//!
//! [`validate`] is the check RefDb makes on every message: only the layout (literals as
//! elements, whole blocks, a reference document, the final `00`), never the values. [`decode`]
//! reads the values as RefDb's unpacking does, and fails where it fails; [`encode`] writes a
//! column that reads back the same values (015).

mod encode;
mod interleaved;
mod simple8b;
mod stream;

use std::fmt;

use crate::read::{self, Mode};
use crate::types;

pub use encode::encode;
use stream::Stream;

/// Nested levels a literal or a reference document of a column may have, itself the first: a
/// `refrouter` 9.0.4 takes 201 and refuses 202 wherever the column is (measured, 003 T029).
pub const MAX_DEPTH: usize = 201;

/// Control byte that ends a column, or interleaved mode.
const END: u8 = 0x00;
/// Interleaved mode of the first layout, where arrays are scalars of the reference.
const INTERLEAVED_LEGACY: u8 = 0xF0;
/// Interleaved mode over a reference document.
const INTERLEAVED_OBJECT: u8 = 0xF1;
/// Interleaved mode over a reference array.
const INTERLEAVED_ARRAY: u8 = 0xF2;
/// First control byte that is neither a literal type nor refused: from here on a byte heads a
/// group of blocks, except the literal types `7F` and `FF` and the interleaved bytes.
const FIRST_GROUP: u8 = 0x20;
/// Bytes in a Simple-8b block.
const BLOCK: usize = 8;

/// One value of a column: its element type and the bytes of its value, as in a BSON element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    /// Element type.
    pub kind: u8,
    /// Bytes of the value.
    pub bytes: Vec<u8>,
}

/// Why a column cannot be read; the texts are a `refrouter` 9.0.4's where it gives one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnError(pub &'static str);

impl fmt::Display for ColumnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for ColumnError {}

/// What a control byte starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Control {
    End,
    Literal,
    Interleaved,
    Group,
    Refused,
}

fn control(c: u8) -> Control {
    match c {
        END => Control::End,
        INTERLEAVED_LEGACY | INTERLEAVED_OBJECT | INTERLEAVED_ARRAY => Control::Interleaved,
        types::MAX_KEY | types::MIN_KEY => Control::Literal,
        c if c <= types::DECIMAL128 => Control::Literal,
        c if c < FIRST_GROUP => Control::Refused,
        _ => Control::Group,
    }
}

/// The mode literals and reference documents are checked with: RefDb's checks of a message,
/// a column inside them refused.
fn inner_mode() -> Mode<'static> {
    Mode::Check { depth: 1, max: MAX_DEPTH, spec: false, column: true, notes: None, array: false }
}

/// The low 64 bits of a slot or an integer: the 64-bit types keep their deltas in them, and the
/// arithmetic on them wraps as RefDb's decoder does.
pub(super) fn low64(v: u128) -> u64 {
    let [b0, b1, b2, b3, b4, b5, b6, b7, ..] = v.to_le_bytes();
    u64::from_le_bytes([b0, b1, b2, b3, b4, b5, b6, b7])
}

const LAYOUT: ColumnError = ColumnError("Invalid BSON column");

/// Checks the layout of column `b` as RefDb checks it on every message.
///
/// # Errors
///
/// A [`ColumnError`] naming the first part of `b` that breaks the column layout.
pub fn validate(b: &[u8]) -> Result<(), ColumnError> {
    let end = walk(b, 0, true)?;
    if end == b.len() { Ok(()) } else { Err(LAYOUT) }
}

/// The end of the run of controls at `p` (after its `00`); `top` outside interleaved mode.
fn walk(b: &[u8], mut p: usize, top: bool) -> Result<usize, ColumnError> {
    loop {
        let c = *b.get(p).ok_or(LAYOUT)?;
        p = match control(c) {
            Control::End => return Ok(p + 1),
            Control::Literal => literal_end(b, p)?,
            Control::Interleaved if top => {
                let after = read::document(b, p + 1, inner_mode()).map_err(|_| LAYOUT)?;
                walk(b, after, false)?
            }
            Control::Interleaved | Control::Refused => return Err(LAYOUT),
            Control::Group => group_end(b, p)?,
        };
    }
}

/// Where the literal at `p` ends: an element with an empty name.
fn literal_end(b: &[u8], p: usize) -> Result<usize, ColumnError> {
    if b.get(p + 1) != Some(&0) {
        return Err(LAYOUT);
    }
    read::element(b, p, inner_mode()).map(|(_, _, end)| end).map_err(|_| LAYOUT)
}

/// Where the group of blocks at `p` ends.
fn group_end(b: &[u8], p: usize) -> Result<usize, ColumnError> {
    let end = p + 1 + BLOCK * blocks_in(b[p]);
    if end > b.len() { Err(LAYOUT) } else { Ok(end) }
}

/// Blocks in the group a control byte starts.
fn blocks_in(c: u8) -> usize {
    usize::from(c & 0x0F) + 1
}

/// The blocks of the group at `p`, as words.
fn words(b: &[u8], p: usize) -> impl Iterator<Item = u64> + '_ {
    b[p + 1..p + 1 + BLOCK * blocks_in(b[p])]
        .as_chunks::<BLOCK>()
        .0
        .iter()
        .map(|w| u64::from_le_bytes(*w))
}

/// The values of column `b`, one per row (`None` for a row without value), as RefDb unpacks
/// them; more than `max_rows` rows is an error.
///
/// # Errors
///
/// A [`ColumnError`] for a layout the validation refuses, a block or control RefDb's decoder refuses,
/// or more rows than `max_rows`.
pub fn decode(b: &[u8], max_rows: usize) -> Result<Vec<Option<Cell>>, ColumnError> {
    validate(b)?;
    let mut rows = Rows { out: Vec::new(), max: max_rows };
    let mut st = Stream::default();
    let mut p = 0;
    loop {
        p = match control(b[p]) {
            Control::End => return Ok(rows.out),
            Control::Literal => {
                let end = literal_end(b, p)?;
                let cell = Cell { kind: b[p], bytes: b[p + 2..end].to_vec() };
                st = Stream::new(cell.clone());
                rows.push(Some(cell))?;
                end
            }
            Control::Interleaved => {
                let (end, last) = interleaved::decode(b, p, &mut rows)?;
                st = last.map(Stream::new).unwrap_or_default();
                end
            }
            Control::Group => {
                st.group(b[p], words(b, p), &mut rows)?;
                group_end(b, p)?
            }
            Control::Refused => return Err(LAYOUT),
        };
    }
}

/// The rows decoded so far, within their limit.
struct Rows {
    out: Vec<Option<Cell>>,
    max: usize,
}

impl Rows {
    fn push(&mut self, row: Option<Cell>) -> Result<(), ColumnError> {
        if self.out.len() == self.max {
            return Err(ColumnError("BSON column with more rows than the bucket holds"));
        }
        self.out.push(row);
        Ok(())
    }
}

/// Something that takes decoded rows: the column, or the buffer of an interleaved stream.
trait Sink {
    fn take(&mut self, row: Option<Cell>) -> Result<(), ColumnError>;
}

impl Sink for Rows {
    fn take(&mut self, row: Option<Cell>) -> Result<(), ColumnError> {
        self.push(row)
    }
}
