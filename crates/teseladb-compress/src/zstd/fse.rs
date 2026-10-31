//! Finite State Entropy tables (RFC 8878, section 4.1): reading a table description, spreading
//! the symbols over the states and the decoding and encoding steps.

use crate::Error;
use crate::zstd::bits::{BackReader, ForwardReader};

/// Smallest accuracy log a table description may give.
const MIN_LOG: u32 = 5;

/// One state of a decoding table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Cell {
    /// The symbol this state decodes.
    pub(crate) symbol: u8,
    /// Bits read to reach the next state.
    pub(crate) bits: u8,
    /// Next state before adding the bits read.
    pub(crate) base: u16,
}

/// A decoding table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Table {
    pub(crate) log: u32,
    pub(crate) cells: Vec<Cell>,
}

impl Table {
    /// The table of a single symbol (mode `RLE`): no bits are ever read.
    pub(crate) fn rle(symbol: u8) -> Table {
        Table { log: 0, cells: vec![Cell { symbol, bits: 0, base: 0 }] }
    }

    /// The table of normalized counts `norm` (`-1`: "less than 1") with `log` accuracy, which
    /// fill exactly `1 << log` states: the predefined distributions, or a description that
    /// [`Table::read`] checked.
    pub(crate) fn spread(norm: &[i16], log: u32) -> Table {
        let size = 1usize << log;
        let mut cells = vec![Cell::default(); size];
        let mut next = vec![0u32; norm.len()];
        // "Less than 1" symbols take the last cells, one each.
        let mut high = size;
        for (s, &c) in norm.iter().enumerate() {
            if c == -1 {
                high -= 1;
                cells[high].symbol = s as u8;
                next[s] = 1;
            } else {
                next[s] = c as u32;
            }
        }
        let step = (size >> 1) + (size >> 3) + 3;
        let mask = size - 1;
        let mut pos = 0usize;
        for (s, &c) in norm.iter().enumerate() {
            for _ in 0..c.max(0) {
                cells[pos].symbol = s as u8;
                pos = (pos + step) & mask;
                while pos >= high {
                    pos = (pos + step) & mask;
                }
            }
        }
        for cell in &mut cells {
            let s = usize::from(cell.symbol);
            let state = next[s];
            next[s] += 1;
            let bits = log - state.ilog2();
            cell.bits = bits as u8;
            cell.base = ((state << bits) - size as u32) as u16;
        }
        Table { log, cells }
    }

    /// Reads a table description at the start of `src` (symbols up to `max_symbol`, accuracy up
    /// to `max_log`); the table and the bytes the description took.
    pub(crate) fn read(
        src: &[u8],
        max_log: u32,
        max_symbol: usize,
    ) -> Result<(Table, usize), Error> {
        let mut r = ForwardReader::new(src);
        let log = r.peek(4)? + MIN_LOG;
        r.consume(4);
        if log > max_log {
            return Err(Error::Corrupt("FSE accuracy log too large"));
        }
        let mut remaining = (1i32 << log) + 1;
        let mut threshold = 1i32 << log;
        let mut nbits = log + 1;
        let mut norm: Vec<i16> = Vec::new();
        let mut previous_zero = false;
        while remaining > 1 {
            if previous_zero {
                // Two-bit repeat flags: more zero counts; 3 means another flag follows.
                loop {
                    let n = r.peek(2)?;
                    r.consume(2);
                    norm.extend(std::iter::repeat_n(0, n as usize));
                    if n != 3 {
                        break;
                    }
                }
                if norm.len() > max_symbol {
                    return Err(Error::Corrupt("FSE symbol out of range"));
                }
            }
            let max = (2 * threshold - 1) - remaining;
            let low = r.peek(nbits - 1)? as i32;
            let mut value;
            if low < max {
                value = low;
                r.consume(nbits - 1);
            } else {
                value = r.peek(nbits)? as i32;
                if value >= threshold {
                    value -= max;
                }
                r.consume(nbits);
            }
            let count = value - 1;
            remaining -= count.abs();
            norm.push(count as i16);
            if norm.len() > max_symbol + 1 {
                return Err(Error::Corrupt("FSE symbol out of range"));
            }
            previous_zero = count == 0;
            // A count never exceeds what remains, so `remaining` stays at least 1 and the loop
            // ends with exactly 1 left: the counts fill the table.
            while remaining < threshold {
                nbits -= 1;
                threshold >>= 1;
            }
        }
        Ok((Table::spread(&norm, log), r.bytes_used()))
    }

    /// The initial state, read from `r`.
    pub(crate) fn init(&self, r: &mut BackReader<'_>) -> usize {
        r.read(self.log) as usize
    }

    /// The state after `state`, reading its bits from `r`.
    pub(crate) fn next(&self, state: usize, r: &mut BackReader<'_>) -> usize {
        let c = self.cells[state];
        usize::from(c.base) + r.read(u32::from(c.bits)) as usize
    }
}

/// The encoding side of a table: for every symbol, the state that leads to each next state.
#[derive(Debug)]
pub(crate) struct Encoder<'t> {
    table: &'t Table,
    /// `to[symbol][next]`: the state of `symbol` whose interval holds `next`.
    to: Vec<Vec<u16>>,
    /// A state of each symbol, to start with.
    first: Vec<u16>,
}

impl<'t> Encoder<'t> {
    pub(crate) fn new(table: &'t Table) -> Encoder<'t> {
        let size = table.cells.len();
        let symbols = table.cells.iter().map(|c| usize::from(c.symbol)).max().unwrap_or(0) + 1;
        let mut to = vec![Vec::new(); symbols];
        let mut first = vec![0u16; symbols];
        for (state, c) in table.cells.iter().enumerate() {
            let s = usize::from(c.symbol);
            if to[s].is_empty() {
                to[s] = vec![0u16; size];
                first[s] = state as u16;
            }
            let lo = usize::from(c.base);
            for slot in &mut to[s][lo..lo + (1 << c.bits)] {
                *slot = state as u16;
            }
        }
        Encoder { table, to, first }
    }

    /// A state that decodes `symbol` (the last symbol of a stream).
    pub(crate) fn start(&self, symbol: u8) -> usize {
        usize::from(self.first[usize::from(symbol)])
    }

    /// The state that decodes `symbol` and leads to `next`, with the bits to write and their
    /// width.
    pub(crate) fn step(&self, symbol: u8, next: usize) -> (usize, u64, u32) {
        let state = usize::from(self.to[usize::from(symbol)][next]);
        let c = self.table.cells[state];
        (state, (next - usize::from(c.base)) as u64, u32::from(c.bits))
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_descriptions() {
        assert!(Table::read(&[0x00], 4, 10).is_err());
        assert!(Table::read(&[], 9, 10).is_err());
        // Random descriptions: every one is refused or gives a table whose cells fill it; symbols
        // past the maximum, accuracies past the limit and descriptions cut short are refused.
        let mut seen = [false; 3];
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..200_000 {
            let mut bytes = [0u8; 12];
            for b in &mut bytes {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *b = state as u8;
            }
            let len = 1 + (state >> 40) as usize % bytes.len();
            match Table::read(&bytes[..len], 9, 3) {
                Ok((t, used)) => {
                    assert_eq!(t.cells.len(), 1 << t.log);
                    assert!(used <= bytes.len());
                }
                Err(Error::Corrupt("FSE symbol out of range")) => seen[0] = true,
                Err(Error::Corrupt("FSE accuracy log too large")) => seen[1] = true,
                Err(_) => seen[2] = true,
            }
        }
        assert_eq!(seen, [true; 3]);
    }

    #[test]
    fn every_interval_of_a_symbol_is_disjoint_and_complete() {
        let norm = [
            4i16, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1,
            1, 1, 1, 1, -1, -1, -1, -1,
        ];
        let t = Table::spread(&norm, 6);
        let e = Encoder::new(&t);
        for (s, &c) in norm.iter().enumerate() {
            let want = if c == -1 { 1 } else { c as usize };
            let states: Vec<usize> = (0..64).map(|n| e.step(s as u8, n).0).collect();
            let mut distinct = states.clone();
            distinct.sort_unstable();
            distinct.dedup();
            assert_eq!(distinct.len(), want, "symbol {s}");
            assert!(distinct.iter().all(|&st| usize::from(t.cells[st].symbol) == s));
        }
    }
}
