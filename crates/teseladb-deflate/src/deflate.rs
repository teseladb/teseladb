//! Streaming DEFLATE compression (RFC 1951): LZ77 with a hash-chain table and lazy matching
//! with zlib's per-level parameters, and for each block the smallest of the three formats
//! (dynamic, fixed or stored).

use crate::huffman::{code_lengths, codes};
use crate::inflate::{CL_ORDER, DIST_BASE, DIST_EXTRA, LEN_BASE, LEN_EXTRA, WINDOW};

const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const HASH_BITS: u32 = 15;
/// Input accumulated before closing a block.
const BLOCK_INPUT: usize = 128 * 1024;

/// Parameters of a level (zlib's): `good` shortens the search once a match is that good, `lazy`
/// is the longest match that is still worth improving, `nice` stops the search and `chain` limits
/// the candidates.
#[derive(Clone, Copy, Debug)]
struct Params {
    good: usize,
    lazy: usize,
    nice: usize,
    chain: usize,
    /// Fast levels: matches longer than this are not inserted position by position.
    max_insert: usize,
}

fn params(level: u8) -> Params {
    let p = |good, lazy, nice, chain| Params { good, lazy, nice, chain, max_insert: MAX_MATCH };
    match level {
        1 => Params { max_insert: 4, ..p(4, 0, 8, 4) },
        2 => Params { max_insert: 5, ..p(4, 0, 16, 8) },
        3 => Params { max_insert: 6, ..p(4, 0, 32, 32) },
        4 => p(4, 4, 16, 16),
        5 => p(8, 16, 32, 32),
        6 => p(8, 16, 128, 128),
        7 => p(8, 32, 128, 256),
        8 => p(32, 128, 258, 1024),
        _ => p(32, 258, 258, 4096),
    }
}

#[derive(Clone, Copy, Debug)]
enum Token {
    Lit(u8),
    Match { len: u16, dist: u16 },
}

/// Bit writer (from the low bit, as DEFLATE requires).
#[derive(Debug, Default)]
struct BitWriter {
    buf: u64,
    cnt: u32,
}

impl BitWriter {
    /// Appends `n` ≤ 16 bits; flushes 32 bits at a time.
    #[inline]
    fn put(&mut self, out: &mut Vec<u8>, v: u32, n: u32) {
        self.buf |= u64::from(v) << self.cnt;
        self.cnt += n;
        if self.cnt >= 32 {
            out.extend_from_slice(&(self.buf as u32).to_le_bytes());
            self.buf >>= 32;
            self.cnt -= 32;
        }
    }

    /// Flushes what is left up to the byte boundary.
    fn align(&mut self, out: &mut Vec<u8>) {
        while self.cnt > 0 {
            out.push(self.buf as u8);
            self.buf >>= 8;
            self.cnt = self.cnt.saturating_sub(8);
        }
        self.buf = 0;
    }
}

/// Length code of each length 0..=258 (table computed at compile time).
static LEN_CODE: [u8; MAX_MATCH + 1] = {
    let mut t = [0u8; MAX_MATCH + 1];
    let mut code = 0;
    let mut len = MIN_MATCH;
    while len <= MAX_MATCH {
        while code + 1 < 29 && LEN_BASE[code + 1] as usize <= len {
            code += 1;
        }
        t[len] = code as u8;
        len += 1;
    }
    t
};

/// Distance code: direct for 1..=256 and through `(dist - 1) >> 7` for the rest (like zlib).
static DIST_CODE: [u8; 512] = {
    let mut t = [0u8; 512];
    let mut d = 1;
    let mut code = 0;
    while d <= 32768 {
        while code + 1 < 30 && DIST_BASE[code + 1] as usize <= d {
            code += 1;
        }
        if d <= 256 {
            t[d - 1] = code as u8;
        } else if (d - 1) % 128 == 0 {
            t[256 + ((d - 1) >> 7)] = code as u8;
        }
        d += 1;
    }
    t
};

#[inline]
fn len_code(len: usize) -> usize {
    usize::from(LEN_CODE[len])
}

#[inline]
fn dist_code(dist: usize) -> usize {
    let d = dist - 1;
    usize::from(if d < 256 { DIST_CODE[d] } else { DIST_CODE[256 + (d >> 7)] })
}

/// Equal bytes at the start of `a` and `b` (up to `max`), 8 at a time.
#[inline]
fn common_prefix(a: &[u8], b: &[u8], max: usize) -> usize {
    let mut l = 0;
    while l + 8 <= max {
        let x = u64::from_le_bytes(a[l..l + 8].try_into().expect("8 bytes"));
        let y = u64::from_le_bytes(b[l..l + 8].try_into().expect("8 bytes"));
        if x != y {
            return l + ((x ^ y).trailing_zeros() / 8) as usize;
        }
        l += 8;
    }
    while l < max && a[l] == b[l] {
        l += 1;
    }
    l
}

/// Streaming compressor.
#[derive(Debug)]
pub struct Deflater {
    level: u8,
    p: Params,
    /// History (up to `WINDOW` already compressed bytes) followed by the pending input.
    buf: Vec<u8>,
    /// Position in `buf` of the first uncompressed byte.
    start: usize,
    /// Absolute position of `buf[0]`.
    base: u64,
    head: Vec<u32>,
    prev: Vec<u32>,
    bits: BitWriter,
    finished: bool,
}

impl Deflater {
    /// Compressor of level `level` (0 = stored, 1 = fast … 9 = best compression).
    pub fn new(level: u8) -> Deflater {
        let level = level.min(9);
        Deflater {
            level,
            p: params(level),
            buf: Vec::new(),
            start: 0,
            base: 0,
            head: vec![0; 1 << HASH_BITS],
            prev: vec![0; WINDOW],
            bits: BitWriter::default(),
            finished: false,
        }
    }

    /// Adds input; writes to `out` the blocks that can already be closed.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) {
        assert!(!self.finished, "feed after finish");
        self.buf.extend_from_slice(input);
        while self.buf.len() - self.start >= BLOCK_INPUT {
            let end = self.start + BLOCK_INPUT;
            self.block(end, false, out);
        }
    }

    /// Closes the stream (last block and padding to a byte).
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        if self.finished {
            return;
        }
        let end = self.buf.len();
        self.block(end, true, out);
        self.bits.align(out);
        self.finished = true;
    }

    fn hash(&self, i: usize) -> usize {
        let b = &self.buf[i..i + 3];
        let v = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        (v.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
    }

    /// Inserts position `i` of `buf` into the hash table.
    fn insert(&mut self, i: usize) {
        if i + MIN_MATCH > self.buf.len() {
            return;
        }
        let h = self.hash(i);
        let abs = (self.base + i as u64) as u32;
        self.prev[abs as usize % WINDOW] = self.head[h];
        self.head[h] = abs.wrapping_add(1);
    }

    /// A chain candidate at absolute position `c`, `dist` bytes back, is still in the buffer and
    /// within the window (older links of the chain are not).
    fn reachable(&self, c: u64, dist: usize) -> bool {
        (1..=WINDOW).contains(&dist) & (c >= self.base)
    }

    /// Best match at `i` (up to `end`) better than `min_len`: (length, distance).
    fn longest(&self, i: usize, end: usize, min_len: usize) -> Option<(usize, usize)> {
        if i + MIN_MATCH > end {
            return None;
        }
        let max = (end - i).min(MAX_MATCH);
        let abs_i = self.base + i as u64;
        let mut chain = if min_len >= self.p.good { self.p.chain / 4 } else { self.p.chain };
        let mut best: Option<(usize, usize)> = None;
        let mut best_len = min_len;
        let mut cand = self.head[self.hash(i)];
        while cand != 0 && chain > 0 {
            chain -= 1;
            let c = u64::from(cand - 1);
            let dist = abs_i.wrapping_sub(c) as usize;
            if !self.reachable(c, dist) {
                break;
            }
            let j = (c - self.base) as usize;
            if self.buf[j + best_len.min(max - 1)] == self.buf[i + best_len.min(max - 1)] {
                let l = common_prefix(&self.buf[j..], &self.buf[i..], max);
                if l > best_len {
                    best_len = l;
                    best = Some((l, dist));
                    if l >= self.p.nice || l == max {
                        break;
                    }
                }
            }
            cand = self.prev[c as usize % WINDOW];
        }
        best.filter(|&(l, _)| l >= MIN_MATCH)
    }

    /// Fast levels (no lazy matching), like zlib's `deflate_fast`: a single hash per position,
    /// inserted before searching.
    fn tokens_fast(&mut self, end: usize) -> Vec<Token> {
        let mut toks = Vec::with_capacity((end - self.start) / 3);
        let mut i = self.start;
        while i < end {
            if i + MIN_MATCH > end {
                toks.push(Token::Lit(self.buf[i]));
                self.insert(i);
                i += 1;
                continue;
            }
            let h = self.hash(i);
            let abs = (self.base + i as u64) as u32;
            let first = self.head[h];
            self.prev[abs as usize % WINDOW] = first;
            self.head[h] = abs.wrapping_add(1);
            let max = (end - i).min(MAX_MATCH);
            let (mut best, mut best_dist) = (MIN_MATCH - 1, 0);
            let mut cand = first;
            let mut chain = self.p.chain;
            while cand != 0 && chain > 0 {
                chain -= 1;
                let c = u64::from(cand - 1);
                let dist = (self.base + i as u64).wrapping_sub(c) as usize;
                if !self.reachable(c, dist) {
                    break;
                }
                let j = (c - self.base) as usize;
                if self.buf[j + best.min(max - 1)] == self.buf[i + best.min(max - 1)] {
                    let l = common_prefix(&self.buf[j..], &self.buf[i..], max);
                    if l > best {
                        best = l;
                        best_dist = dist;
                        if l >= self.p.nice {
                            break;
                        }
                    }
                }
                cand = self.prev[c as usize % WINDOW];
            }
            if best < MIN_MATCH {
                toks.push(Token::Lit(self.buf[i]));
                i += 1;
                continue;
            }
            toks.push(Token::Match { len: best as u16, dist: best_dist as u16 });
            if best <= self.p.max_insert {
                for k in 1..best {
                    self.insert(i + k);
                }
            }
            i += best;
        }
        toks
    }

    fn tokens(&mut self, end: usize) -> Vec<Token> {
        if self.p.lazy == 0 {
            return self.tokens_fast(end);
        }
        let mut toks = Vec::with_capacity((end - self.start) / 2);
        let mut i = self.start;
        let lazy = self.p.lazy > 0;
        while i < end {
            let m = self.longest(i, end, MIN_MATCH - 1);
            let Some((mut len, mut dist)) = m else {
                toks.push(Token::Lit(self.buf[i]));
                self.insert(i);
                i += 1;
                continue;
            };
            // Lazy matching: if the next position has a better match, a literal is emitted.
            if lazy && len < self.p.lazy {
                self.insert(i);
                if let Some((l2, d2)) = self.longest(i + 1, end, len) {
                    toks.push(Token::Lit(self.buf[i]));
                    i += 1;
                    len = l2;
                    dist = d2;
                } else {
                    toks.push(Token::Match { len: len as u16, dist: dist as u16 });
                    for k in 1..len {
                        self.insert(i + k);
                    }
                    i += len;
                    continue;
                }
            }
            toks.push(Token::Match { len: len as u16, dist: dist as u16 });
            if len <= self.p.max_insert {
                for k in 0..len {
                    self.insert(i + k);
                }
            } else {
                self.insert(i);
            }
            i += len;
        }
        toks
    }

    fn block(&mut self, end: usize, last: bool, out: &mut Vec<u8>) {
        if self.level == 0 {
            self.stored(self.start, end, last, out);
        } else {
            let toks = self.tokens(end);
            self.emit(&toks, self.start, end, last, out);
        }
        self.start = end;
        // Only the window is kept as history. It is trimmed when the excess is at least as large
        // as what stays (otherwise a large input would be moved entirely on every
        // block).
        let cut = self.start.saturating_sub(WINDOW);
        if self.start > 2 * WINDOW && 2 * cut >= self.buf.len() {
            self.buf.drain(..cut);
            self.start -= cut;
            self.base += cut as u64;
        }
    }

    fn stored(&mut self, start: usize, end: usize, last: bool, out: &mut Vec<u8>) {
        let mut pos = start;
        loop {
            let n = (end - pos).min(0xffff);
            let fin = last && pos + n == end;
            self.bits.put(out, u32::from(fin), 1);
            self.bits.put(out, 0, 2);
            self.bits.align(out);
            out.extend_from_slice(&(n as u16).to_le_bytes());
            out.extend_from_slice(&(!(n as u16)).to_le_bytes());
            out.extend_from_slice(&self.buf[pos..pos + n]);
            pos += n;
            if pos == end {
                break;
            }
        }
    }

    fn emit(&mut self, toks: &[Token], start: usize, end: usize, last: bool, out: &mut Vec<u8>) {
        let mut lf = vec![0u32; 286];
        let mut df = vec![0u32; 30];
        for t in toks {
            match *t {
                Token::Lit(b) => lf[b as usize] += 1,
                Token::Match { len, dist } => {
                    lf[257 + len_code(usize::from(len))] += 1;
                    df[dist_code(usize::from(dist))] += 1;
                }
            }
        }
        lf[256] = 1;
        let ll = code_lengths(&lf, 15);
        let mut dl = code_lengths(&df, 15);
        if dl.iter().all(|&l| l == 0) {
            // At least one distance code (some readers require it).
            dl[0] = 1;
        }
        // Size in bits of each option.
        let extra_bits: u64 = toks
            .iter()
            .map(|t| match *t {
                Token::Lit(_) => 0,
                Token::Match { len, dist } => {
                    u64::from(LEN_EXTRA[len_code(usize::from(len))])
                        + u64::from(DIST_EXTRA[dist_code(usize::from(dist))])
                }
            })
            .sum();
        let cost = |l: &[u8], f: &[u32]| -> u64 {
            l.iter().zip(f).map(|(&a, &b)| u64::from(a) * u64::from(b)).sum()
        };
        let (fl, fd) = fixed_lengths();
        let fixed_bits = 3 + cost(&fl[..286], &lf) + cost(&fd, &df) + extra_bits;
        let header = DynHeader::new(&ll, &dl);
        let dyn_bits = 3 + header.bits() + cost(&ll, &lf) + cost(&dl, &df) + extra_bits;
        let raw_len = (end - start) as u64;
        let stored_bits = (raw_len.div_ceil(0xffff).max(1) * 5 + raw_len) * 8 + 10;
        if stored_bits <= fixed_bits.min(dyn_bits) {
            self.stored(start, end, last, out);
        } else if fixed_bits <= dyn_bits {
            self.bits.put(out, u32::from(last), 1);
            self.bits.put(out, 1, 2);
            self.write_tokens(toks, &fl, &fd, out);
        } else {
            self.bits.put(out, u32::from(last), 1);
            self.bits.put(out, 2, 2);
            header.write(&mut self.bits, out);
            self.write_tokens(toks, &ll, &dl, out);
        }
    }

    fn write_tokens(&mut self, toks: &[Token], ll: &[u8], dl: &[u8], out: &mut Vec<u8>) {
        let lc = codes(ll);
        let dc = codes(dl);
        for t in toks {
            match *t {
                Token::Lit(b) => {
                    self.bits.put(out, u32::from(lc[b as usize]), u32::from(ll[b as usize]))
                }
                Token::Match { len, dist } => {
                    let (len, dist) = (usize::from(len), usize::from(dist));
                    let li = len_code(len);
                    self.bits.put(out, u32::from(lc[257 + li]), u32::from(ll[257 + li]));
                    self.bits.put(
                        out,
                        (len - usize::from(LEN_BASE[li])) as u32,
                        u32::from(LEN_EXTRA[li]),
                    );
                    let di = dist_code(dist);
                    self.bits.put(out, u32::from(dc[di]), u32::from(dl[di]));
                    self.bits.put(
                        out,
                        (dist - usize::from(DIST_BASE[di])) as u32,
                        u32::from(DIST_EXTRA[di]),
                    );
                }
            }
        }
        self.bits.put(out, u32::from(lc[256]), u32::from(ll[256]));
    }
}

fn fixed_lengths() -> (Vec<u8>, Vec<u8>) {
    let mut lit = vec![8u8; 288];
    lit[144..256].fill(9);
    lit[256..280].fill(7);
    (lit, vec![5u8; 30])
}

/// Header of a dynamic block: lengths encoded with repetitions (16, 17, 18) and their own
/// Huffman code.
struct DynHeader {
    nlen: usize,
    ndist: usize,
    ncode: usize,
    cl_len: Vec<u8>,
    ops: Vec<(u8, u8)>,
}

impl DynHeader {
    fn new(ll: &[u8], dl: &[u8]) -> DynHeader {
        let nlen = (257..=286).rev().find(|&n| ll[n - 1] != 0).unwrap_or(257).max(257);
        let ndist = (1..=30).rev().find(|&n| dl[n - 1] != 0).unwrap_or(1).max(1);
        let all: Vec<u8> = ll[..nlen].iter().chain(&dl[..ndist]).copied().collect();
        // Repetitions: 16 = repeat the previous 3–6 times, 17 = 3–10 zeros, 18 = 11–138 zeros.
        let mut ops = Vec::new();
        let mut i = 0;
        while i < all.len() {
            let v = all[i];
            let run = all[i..].iter().take_while(|&&x| x == v).count();
            if v == 0 && run >= 3 {
                let n = run.min(138);
                ops.push(if n >= 11 { (18, (n - 11) as u8) } else { (17, (n - 3) as u8) });
                i += n;
            } else if v != 0 && run >= 4 {
                ops.push((v, 0));
                let n = (run - 1).min(6);
                ops.push((16, (n - 3) as u8));
                i += 1 + n;
            } else {
                ops.push((v, 0));
                i += 1;
            }
        }
        let mut freq = [0u32; 19];
        for &(sym, _) in &ops {
            freq[sym as usize] += 1;
        }
        let cl_len = code_lengths(&freq, 7);
        let ncode = (4..=19).rev().find(|&n| cl_len[CL_ORDER[n - 1]] != 0).unwrap_or(4);
        DynHeader { nlen, ndist, ncode, cl_len, ops }
    }

    fn bits(&self) -> u64 {
        let extra = |s: u8| match s {
            16 => 2,
            17 => 3,
            18 => 7,
            _ => 0,
        };
        14 + 3 * self.ncode as u64
            + self
                .ops
                .iter()
                .map(|&(s, _)| u64::from(self.cl_len[s as usize]) + extra(s))
                .sum::<u64>()
    }

    fn write(&self, bits: &mut BitWriter, out: &mut Vec<u8>) {
        bits.put(out, (self.nlen - 257) as u32, 5);
        bits.put(out, (self.ndist - 1) as u32, 5);
        bits.put(out, (self.ncode - 4) as u32, 4);
        for &i in &CL_ORDER[..self.ncode] {
            bits.put(out, u32::from(self.cl_len[i]), 3);
        }
        let cc = codes(&self.cl_len);
        for &(s, x) in &self.ops {
            bits.put(out, u32::from(cc[s as usize]), u32::from(self.cl_len[s as usize]));
            match s {
                16 => bits.put(out, u32::from(x), 2),
                17 => bits.put(out, u32::from(x), 3),
                18 => bits.put(out, u32::from(x), 7),
                _ => {}
            }
        }
    }
}

/// Compresses all of `data`.
pub fn deflate(data: &[u8], level: u8) -> Vec<u8> {
    let mut d = Deflater::new(level);
    let mut out = Vec::with_capacity(data.len() / 2 + 64);
    d.feed(data, &mut out);
    d.finish(&mut out);
    out
}
