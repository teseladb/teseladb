//! Cursors over the sources of a view, shared by scans and time scans: a memtable cursor
//! walks by key; a file cursor seeks through the index and loads its blocks through the
//! [`Reader`], waiting when one is not cached.

use std::sync::Arc;

use teseladb_types::Bytes;

use crate::entry::{self, Kind};
use crate::error::LsmError;
use crate::memtable::Memtable;
use crate::read::{Blocks, Fetch, ReadToken, corrupt};
use crate::sst::block::BlockCursor;
use crate::sst::reader::Table;

/// Where a file cursor is.
pub(crate) enum Pos {
    /// Must seek to an internal key.
    Seek(Vec<u8>),
    /// Must load a block and stand at its first entry.
    Block(usize),
    /// At an entry of block `.0`.
    Ready(usize, BlockCursor),
    /// Past the end.
    Done,
}

/// A cursor over a memtable: it walks by key, so the active memtable may take writes or be
/// frozen while it is open.
pub(crate) struct MemCursor {
    mem: Arc<Memtable>,
    pos: usize,
    key: Vec<u8>,
    kind: Kind,
    value: Bytes,
    valid: bool,
}

type Found = Option<(usize, Vec<u8>, Kind, Bytes)>;

impl MemCursor {
    /// A cursor over `mem`, at no entry until it seeks.
    pub(crate) fn new(mem: Arc<Memtable>) -> MemCursor {
        MemCursor {
            mem,
            pos: 0,
            key: Vec::new(),
            kind: Kind::Put,
            value: Bytes::new(),
            valid: false,
        }
    }

    fn set(&mut self, found: Found) {
        match found {
            Some((p, k, kd, v)) => {
                (self.pos, self.key, self.kind, self.value, self.valid) = (p, k, kd, v, true)
            }
            None => self.valid = false,
        }
    }
}

/// A cursor over one source.
pub(crate) enum Cursor {
    Mem(MemCursor),
    Table { table: Arc<Table>, pos: Pos },
}

impl Cursor {
    pub(crate) fn seek(&mut self, target: Vec<u8>) {
        match self {
            Cursor::Mem(m) => {
                let found =
                    m.mem.seek(&target).map(|(p, k, s)| (p, k.to_vec(), s.kind, s.value.clone()));
                m.set(found);
            }
            Cursor::Table { pos, .. } => *pos = Pos::Seek(target),
        }
    }

    /// Moves past the current entry (only a cursor at an entry advances).
    pub(crate) fn advance(&mut self) -> Result<(), LsmError> {
        match self {
            Cursor::Mem(m) => {
                let found = m
                    .mem
                    .next_after(m.pos, &m.key)
                    .map(|(p, k, s)| (p, k.to_vec(), s.kind, s.value.clone()));
                m.set(found);
            }
            Cursor::Table { table, pos: Pos::Ready(i, c) } => {
                c.advance().map_err(|_| corrupt(table, *i))?;
                if !c.valid() {
                    let next = *i + 1;
                    *self = Cursor::Table { table: table.clone(), pos: Pos::Block(next) };
                }
            }
            Cursor::Table { .. } => unreachable!("only a cursor at an entry advances"),
        }
        Ok(())
    }

    /// Loads what the cursor needs; `Some(token)` if it waits for a read.
    pub(crate) fn ready<B: Blocks>(&mut self, r: &mut B) -> Result<Option<ReadToken>, LsmError> {
        let Cursor::Table { table, pos } = self else {
            return Ok(None);
        };
        loop {
            match pos {
                Pos::Ready(..) | Pos::Done => return Ok(None),
                Pos::Seek(target) => {
                    let Some(i) = table.find_block(target) else {
                        *pos = Pos::Done;
                        continue;
                    };
                    let block = match r.block(table, i)? {
                        Fetch::Ready(b) => b,
                        Fetch::Wait(t) => return Ok(Some(t)),
                    };
                    let c = BlockCursor::seek(block, target, entry::compare)
                        .map_err(|_| corrupt(table, i))?;
                    *pos = if c.valid() { Pos::Ready(i, c) } else { Pos::Block(i + 1) };
                }
                Pos::Block(i) => {
                    let i = *i;
                    if i >= table.blocks() {
                        *pos = Pos::Done;
                        continue;
                    }
                    let block = match r.block(table, i)? {
                        Fetch::Ready(b) => b,
                        Fetch::Wait(t) => return Ok(Some(t)),
                    };
                    let c = BlockCursor::first(block).map_err(|_| corrupt(table, i))?;
                    *pos = if c.valid() { Pos::Ready(i, c) } else { Pos::Block(i + 1) };
                }
            }
        }
    }

    /// The current internal key, if any (after [`Cursor::ready`]).
    pub(crate) fn key(&self) -> Option<&[u8]> {
        match self {
            Cursor::Mem(m) => m.valid.then_some(m.key.as_slice()),
            Cursor::Table { pos: Pos::Ready(_, c), .. } => Some(c.key()),
            Cursor::Table { .. } => None,
        }
    }

    pub(crate) fn kind(&self) -> Kind {
        match self {
            Cursor::Mem(m) => m.kind,
            Cursor::Table { pos: Pos::Ready(_, c), .. } => c.kind(),
            Cursor::Table { .. } => unreachable!("a cursor without an entry has no kind"),
        }
    }

    pub(crate) fn value(&self) -> Bytes {
        match self {
            Cursor::Mem(m) => m.value.clone(),
            Cursor::Table { pos: Pos::Ready(_, c), .. } => c.value(),
            Cursor::Table { .. } => unreachable!("a cursor without an entry has no value"),
        }
    }
}
