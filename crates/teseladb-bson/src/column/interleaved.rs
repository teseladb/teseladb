//! Interleaved mode of a BSON column (measured on a `refrouter` 9.0.4, 003 T029).
//!
//! The control byte (`F0`, `F1` or `F2`) is followed by a reference document; each scalar in it,
//! in order, starts a stream (with `F0` arrays are scalars too). Row after row, a stream whose
//! values have run out takes the next group of blocks. The mode ends when the first stream finds
//! `00` with every other stream run out and at least one row produced.
//!
//! A row is the reference with each scalar replaced by its stream's value: a scalar without value
//! is left out, a document or array whose scalars are all left out goes too (an empty one in the
//! reference always stays), and a row with nothing is a row without value. Under `F2` the row is
//! an array whose elements are numbered again from 0.

use std::collections::VecDeque;

use super::stream::Stream;
use super::{Cell, ColumnError, Control, END, INTERLEAVED_ARRAY, INTERLEAVED_LEGACY, Rows, Sink};
use crate::{read, types};

/// A node of the reference: a scalar (its stream) or a document or array.
enum Node {
    Scalar(usize),
    Nested { kind: u8, children: Vec<(Vec<u8>, Node)> },
}

impl Sink for VecDeque<Option<Cell>> {
    fn take(&mut self, row: Option<Cell>) -> Result<(), ColumnError> {
        self.push_back(row);
        Ok(())
    }
}

/// Decodes the interleaved run at `p` into `rows`: where it ends, and its last row.
pub(super) fn decode(
    b: &[u8],
    p: usize,
    rows: &mut Rows,
) -> Result<(usize, Option<Cell>), ColumnError> {
    let mode = b[p];
    let len = usize::try_from(i32::from_le_bytes(b[p + 1..p + 5].try_into().expect("a length")))
        .map_err(|_| super::LAYOUT)?;
    let reference = &b[p + 1..p + 1 + len];
    let mut streams = Vec::new();
    let children = nodes(reference, mode == INTERLEAVED_LEGACY, &mut streams)?;
    if streams.is_empty() {
        return Err(ColumnError(
            "Invalid BSONColumn encoding during initialization of interleaved mode",
        ));
    }
    let root_kind = if mode == INTERLEAVED_ARRAY { types::ARRAY } else { types::DOCUMENT };
    let mut buffers: Vec<VecDeque<Option<Cell>>> =
        streams.iter().map(|_| VecDeque::new()).collect();
    let mut at = p + 1 + len;
    let mut last = None;
    let mut produced = 0usize;
    loop {
        let mut values = Vec::with_capacity(streams.len());
        for i in 0..streams.len() {
            if buffers[i].is_empty() {
                match super::control(b[at]) {
                    Control::End
                        if i == 0 && produced > 0 && buffers.iter().all(VecDeque::is_empty) =>
                    {
                        return Ok((at + 1, last));
                    }
                    Control::Group => {
                        streams[i].group(b[at], super::words(b, at), &mut buffers[i])?;
                        at = super::group_end(b, at)?;
                    }
                    _ => {
                        return Err(ColumnError(
                            "Not all BSON Column interleaved encoding decoders are exhausted",
                        ));
                    }
                }
            }
            values.push(buffers[i].pop_front().flatten());
        }
        let row = build(&children, &values, mode == INTERLEAVED_ARRAY)
            .map(|bytes| Cell { kind: root_kind, bytes });
        last.clone_from(&row);
        rows.push(row)?;
        produced += 1;
    }
}

/// The nodes of the reference document `doc`, its scalars added to `streams`.
fn nodes(
    doc: &[u8],
    legacy: bool,
    streams: &mut Vec<Stream>,
) -> Result<Vec<(Vec<u8>, Node)>, ColumnError> {
    let mut out = Vec::new();
    let mut p = 4;
    while doc[p] != END {
        let kind = doc[p];
        // Names are bytes here: RefDb takes names that are not UTF-8.
        let (_, _, end) = read::element(doc, p, super::inner_mode()).map_err(|_| super::LAYOUT)?;
        let name_end = p + 1 + crate::arch::find_nul(&doc[p + 1..]).ok_or(super::LAYOUT)?;
        let value = &doc[name_end + 1..end];
        let node = if kind == types::DOCUMENT || (kind == types::ARRAY && !legacy) {
            Node::Nested { kind, children: nodes(value, legacy, streams)? }
        } else {
            streams.push(Stream::new(Cell { kind, bytes: value.to_vec() }));
            Node::Scalar(streams.len() - 1)
        };
        out.push((doc[p + 1..name_end].to_vec(), node));
        p = end;
    }
    Ok(out)
}

/// The bytes of the document a node makes from `values`, if it has anything.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "a reference document holds one value of at most 16 MiB per field of a column, far below 2^31 bytes"
)]
fn build(children: &[(Vec<u8>, Node)], values: &[Option<Cell>], renumber: bool) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    let mut count = 0usize;
    let mut any = false;
    for (name, child) in children {
        let (kind, bytes) = match child {
            Node::Scalar(i) => match &values[*i] {
                Some(cell) => {
                    any = true;
                    (cell.kind, cell.bytes.clone())
                }
                None => continue,
            },
            Node::Nested { kind, children: grand } => match build(grand, values, false) {
                Some(bytes) => {
                    any = true;
                    (*kind, bytes)
                }
                None if grand.is_empty() => (*kind, empty_document()),
                None => continue,
            },
        };
        body.push(kind);
        if renumber {
            body.extend_from_slice(count.to_string().as_bytes());
        } else {
            body.extend_from_slice(name);
        }
        body.push(0);
        body.extend_from_slice(&bytes);
        count += 1;
    }
    if !any {
        return None;
    }
    let mut out = ((body.len() + 5) as i32).to_le_bytes().to_vec();
    out.extend_from_slice(&body);
    out.push(END);
    Some(out)
}

fn empty_document() -> Vec<u8> {
    vec![5, 0, 0, 0, 0]
}
