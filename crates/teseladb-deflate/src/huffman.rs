//! DEFLATE canonical Huffman codes: two-level decoding tables and length-limited construction for
//! the compressor.

/// Bits of the decoding table's first level.
pub(crate) const FAST_BITS: u32 = 10;
/// Maximum length of a DEFLATE code.
pub(crate) const MAX_BITS: usize = 15;

/// First-level entry: symbol in the low 16 bits and length in the high ones; 0 = the code is
/// longer than `FAST_BITS` (second level) or does not exist.
#[derive(Clone, Debug)]
pub(crate) struct Decoder {
    fast: Vec<u32>,
    /// Second level, as in `puff`: how many codes there are of each length and the symbols in
    /// canonical order.
    count: [u16; MAX_BITS + 1],
    symbols: Vec<u16>,
    /// There is no code at all (e.g. a block without distances).
    empty: bool,
}

/// Why a set of lengths is not valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TableError {
    /// More codes than fit (oversubscribed).
    Oversubscribed,
    /// Missing codes (only a single code of length 1 is allowed).
    Incomplete,
}

/// Reverses the low `len` bits of `code` (DEFLATE packs codes starting from the high bit).
pub(crate) fn reverse(code: u32, len: u32) -> u32 {
    code.reverse_bits() >> (32 - len)
}

impl Decoder {
    /// Builds the table from each symbol's length (0 = no code).
    pub(crate) fn new(lengths: &[u8]) -> Result<Decoder, TableError> {
        let mut count = [0u16; MAX_BITS + 1];
        for &l in lengths {
            count[l as usize] += 1;
        }
        count[0] = 0;
        let total: u16 = count.iter().sum();
        // Kraft check: oversubscribed or incomplete.
        let mut left: i32 = 1;
        for &c in &count[1..] {
            left <<= 1;
            left -= i32::from(c);
            if left < 0 {
                return Err(TableError::Oversubscribed);
            }
        }
        let max = (1..=MAX_BITS).rev().find(|&l| count[l] > 0).unwrap_or(0);
        if left > 0 && total > 0 && !(total == 1 && max == 1) {
            return Err(TableError::Incomplete);
        }
        // First code of each length and symbols in canonical order.
        let mut next = [0u32; MAX_BITS + 2];
        let mut offs = [0u16; MAX_BITS + 2];
        let mut code = 0u32;
        for len in 1..=MAX_BITS {
            code = (code + u32::from(count[len - 1])) << 1;
            next[len] = code;
            offs[len + 1] = offs[len] + count[len];
        }
        let mut symbols = vec![0u16; total as usize];
        let mut fast = vec![0u32; 1 << FAST_BITS];
        for (sym, &l) in lengths.iter().enumerate() {
            if l == 0 {
                continue;
            }
            let l = u32::from(l);
            symbols[offs[l as usize] as usize] = sym as u16;
            offs[l as usize] += 1;
            let c = next[l as usize];
            next[l as usize] += 1;
            if l <= FAST_BITS {
                let r = reverse(c, l);
                let entry = sym as u32 | (l << 16);
                let mut i = r;
                while i < (1 << FAST_BITS) {
                    fast[i as usize] = entry;
                    i += 1 << l;
                }
            }
        }
        Ok(Decoder { fast, count, symbols, empty: total == 0 })
    }

    /// Decodes a symbol with the available bits (`bits` with `avail` valid bits, the first one in
    /// bit 0). Returns (symbol, length) or `None` if more bits are needed; `Err` if the code does
    /// not exist.
    #[inline]
    pub(crate) fn decode(&self, bits: u64, avail: u32) -> Result<Option<(u16, u32)>, ()> {
        let e = self.fast[(bits & ((1 << FAST_BITS) - 1)) as usize];
        if e != 0 {
            let len = e >> 16;
            return Ok((len <= avail).then_some((e as u16, len)));
        }
        if self.empty {
            return Err(());
        }
        // Second level: canonical bit-by-bit decoding (codes longer than `FAST_BITS`).
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..=MAX_BITS as u32 {
            if len > avail {
                return Ok(None);
            }
            code |= ((bits >> (len - 1)) & 1) as i32;
            let count = i32::from(self.count[len as usize]);
            if code - count < first {
                return Ok(Some((self.symbols[(index + (code - first)) as usize], len)));
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err(())
    }
}

/// Huffman code lengths for `freqs` with at most `limit` bits (symbols with frequency 0 get no
/// code). If the optimal tree is deeper, the frequencies are flattened and the tree is rebuilt:
/// the result is still a complete, near-optimal code.
pub(crate) fn code_lengths(freqs: &[u32], limit: u32) -> Vec<u8> {
    let mut f: Vec<u32> = freqs.to_vec();
    loop {
        let lens = unlimited_lengths(&f);
        if lens.iter().all(|&l| u32::from(l) <= limit) {
            return lens;
        }
        for x in f.iter_mut().filter(|x| **x > 0) {
            *x = (*x >> 1) | 1;
        }
    }
}

fn unlimited_lengths(freqs: &[u32]) -> Vec<u8> {
    let mut lens = vec![0u8; freqs.len()];
    let used: Vec<usize> = (0..freqs.len()).filter(|&i| freqs[i] > 0).collect();
    match used.len() {
        0 => return lens,
        // A single symbol: 1-bit code (DEFLATE does not allow 0-bit codes).
        1 => {
            lens[used[0]] = 1;
            return lens;
        }
        _ => {}
    }
    // Huffman tree with two queues (the sorted leaves and the internal nodes, which come out in order).
    let mut leaves: Vec<(u64, usize)> = used.iter().map(|&i| (u64::from(freqs[i]), i)).collect();
    leaves.sort_unstable();
    let n = leaves.len();
    let mut parent = vec![usize::MAX; 2 * n];
    let mut internal: Vec<u64> = Vec::with_capacity(n);
    let (mut li, mut ii) = (0usize, 0usize);
    let pick = |internal: &Vec<u64>, li: &mut usize, ii: &mut usize| -> (u64, usize) {
        let take_leaf = *ii >= internal.len() || (*li < n && leaves[*li].0 <= internal[*ii]);
        if take_leaf {
            *li += 1;
            (leaves[*li - 1].0, *li - 1)
        } else {
            *ii += 1;
            (internal[*ii - 1], n + *ii - 1)
        }
    };
    for k in 0..n - 1 {
        let (wa, a) = pick(&internal, &mut li, &mut ii);
        let (wb, b) = pick(&internal, &mut li, &mut ii);
        parent[a] = n + k;
        parent[b] = n + k;
        internal.push(wa + wb);
    }
    // Depths: the root is the last internal node.
    let mut depth = vec![0u32; 2 * n - 1];
    for node in (0..2 * n - 2).rev() {
        depth[node] = depth[parent[node]] + 1;
    }
    for (k, &(_, sym)) in leaves.iter().enumerate() {
        lens[sym] = depth[k].min(255) as u8;
    }
    lens
}

/// Canonical codes (already reversed to be written from the low bit) of some lengths.
pub(crate) fn codes(lengths: &[u8]) -> Vec<u16> {
    let mut count = [0u32; MAX_BITS + 1];
    for &l in lengths {
        count[l as usize] += 1;
    }
    count[0] = 0;
    let mut next = [0u32; MAX_BITS + 2];
    let mut code = 0u32;
    for len in 1..=MAX_BITS {
        code = (code + count[len - 1]) << 1;
        next[len] = code;
    }
    lengths
        .iter()
        .map(|&l| {
            if l == 0 {
                return 0;
            }
            let c = next[l as usize];
            next[l as usize] += 1;
            reverse(c, u32::from(l)) as u16
        })
        .collect()
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn tables_validate_kraft() {
        assert_eq!(Decoder::new(&[1, 1, 1]).unwrap_err(), TableError::Oversubscribed);
        assert_eq!(Decoder::new(&[2, 2, 2]).unwrap_err(), TableError::Incomplete);
        assert!(Decoder::new(&[1]).is_ok(), "a single 1-bit code");
        assert!(Decoder::new(&[0, 0]).is_ok(), "no codes");
        let d = Decoder::new(&[0, 0]).unwrap();
        assert_eq!(d.decode(0, 64), Err(()));
    }

    #[test]
    fn long_codes_use_the_second_level() {
        // Lengths 1..=15 and two of 15: a complete code with long codes.
        let mut lens: Vec<u8> = (1..=15).collect();
        lens.push(15);
        let d = Decoder::new(&lens).unwrap();
        let c = codes(&lens);
        for (sym, &l) in lens.iter().enumerate() {
            let got = d.decode(u64::from(c[sym]), u32::from(l)).unwrap().unwrap();
            assert_eq!(got, (sym as u16, u32::from(l)));
            if l > 1 {
                assert_eq!(d.decode(u64::from(c[sym]), u32::from(l) - 1), Ok(None), "missing bits");
            }
        }
    }

    #[test]
    fn limited_lengths_are_complete() {
        // Fibonacci frequencies: the optimal tree is very deep.
        let mut f = vec![1u32, 1];
        while f.len() < 30 {
            let n = f[f.len() - 1] + f[f.len() - 2];
            f.push(n);
        }
        for limit in [7, 15] {
            let lens = code_lengths(&f, limit);
            assert!(lens.iter().all(|&l| l >= 1 && u32::from(l) <= limit));
            let kraft: f64 = lens.iter().map(|&l| 2f64.powi(-i32::from(l))).sum();
            assert!((kraft - 1.0).abs() < 1e-12, "{kraft}");
            assert!(Decoder::new(&lens).is_ok());
        }
        assert_eq!(code_lengths(&[0, 5, 0], 15), vec![0, 1, 0]);
        assert_eq!(code_lengths(&[0, 0], 15), vec![0, 0]);
    }
}
