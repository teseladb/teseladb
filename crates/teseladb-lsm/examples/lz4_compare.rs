//! Our LZ4 (FR-037) on the blocks of a corpus, for a comparison with another implementation of
//! the format: it writes the corpus's blocks and their LZ4 blocks as ours makes them, times our
//! compression and decompression, and checks blocks another implementation made from the same
//! corpus.
//!
//! `cargo run --release -p teseladb-lsm --example lz4_compare -- write <dir>` writes
//! `<dir>/blocks.bin` (the corpus in blocks) and `<dir>/ours.bin` (our LZ4 blocks) and prints our
//! ratio and speeds; `... -- check <dir>` decompresses `<dir>/theirs.bin` with ours and checks
//! each block against the corpus. Each file is a series of records: length (u32 little-endian),
//! then the bytes; `ours.bin` and `theirs.bin` put the decompressed length (u32) before each
//! block.
//!
//! The corpus is two halves: documents of the shape of the sustained load of 002 (`workload.py`:
//! a padding of 50 to 2,000 bytes, one in 500 of 60,000), packed in blocks of 16 KiB as the LSM
//! writes them; and text with words drawn from a vocabulary, which repeats less.

use std::fs;
use std::hint::black_box;
use std::path::Path;
use std::time::Instant;

const BLOCK: usize = 16 << 10;
const BLOCKS: usize = 4096;
const ROUNDS: usize = 5;

/// xorshift64*.
fn draws(mut state: u64) -> impl FnMut() -> u64 {
    move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

fn corpus() -> Vec<Vec<u8>> {
    let mut next = draws(0x5EED);
    let mut blocks = Vec::new();
    let mut block = Vec::new();
    let mut n = 0u64;
    while blocks.len() < BLOCKS / 2 {
        let c = next() % 16;
        let pad = if next().is_multiple_of(500) { 60_000 } else { 50 + (next() % 1951) as usize };
        let doc = format!(
            "{{\"_id\":\"c{c:03}-{n:08}\",\"c\":{c},\"n\":{n},\"g\":{},\"v\":{},\"s\":\"s{}\",\"tags\":[{},{}],\"ver\":{},\"pad\":\"{}\"}}",
            n % 16,
            next() % 100,
            next() % 1000,
            next() % 100,
            next() % 100,
            next() % 40,
            "x".repeat(pad)
        );
        n += 1;
        block.extend_from_slice(doc.as_bytes());
        while block.len() >= BLOCK {
            let rest = block.split_off(BLOCK);
            blocks.push(std::mem::replace(&mut block, rest));
        }
    }
    let words: Vec<String> = (0..4000)
        .map(|_| (0..2 + next() % 9).map(|_| char::from(b'a' + (next() % 26) as u8)).collect())
        .collect();
    while blocks.len() < BLOCKS {
        let mut b = Vec::with_capacity(BLOCK);
        while b.len() < BLOCK {
            b.extend_from_slice(words[(next() % 4000) as usize].as_bytes());
            b.push(if next().is_multiple_of(12) { b'\n' } else { b' ' });
        }
        b.truncate(BLOCK);
        blocks.push(b);
    }
    blocks
}

fn put(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn records(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 4 <= data.len() {
        let n = u32::from_le_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        out.push(&data[at + 4..at + 4 + n]);
        at += 4 + n;
    }
    out
}

/// A length field that was 15 goes on in the bytes after it.
fn more_length(block: &[u8], at: &mut usize) -> usize {
    let mut n = 0;
    loop {
        let b = block[*at];
        *at += 1;
        n += usize::from(b);
        if b != 255 {
            return n;
        }
    }
}

/// The matches of an LZ4 block: where each starts in the output, its offset and its length.
fn matches(block: &[u8]) -> Vec<(usize, usize, usize)> {
    let (mut at, mut op, mut out) = (0, 0, Vec::new());
    loop {
        let token = usize::from(block[at]);
        at += 1;
        let mut lit = token >> 4;
        if lit == 15 {
            lit += more_length(block, &mut at);
        }
        at += lit;
        op += lit;
        if at == block.len() {
            return out;
        }
        let offset = usize::from(u16::from_le_bytes([block[at], block[at + 1]]));
        at += 2;
        let mut len = token & 15;
        if len == 15 {
            len += more_length(block, &mut at);
        }
        len += 4;
        out.push((op, offset, len));
        op += len;
    }
}

/// The best of `ROUNDS` runs of `f`, in MB/s of `bytes`.
fn speed(bytes: usize, mut f: impl FnMut()) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..ROUNDS {
        let t = Instant::now();
        f();
        best = best.min(t.elapsed().as_secs_f64());
    }
    bytes as f64 / best / 1e6
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (mode, dir) = (args[1].as_str(), Path::new(&args[2]));
    let blocks = corpus();
    let total: usize = blocks.iter().map(Vec::len).sum();
    match mode {
        "write" => {
            let ours: Vec<Vec<u8>> =
                blocks.iter().map(|b| teseladb_lsm::lz4_compress_block(b)).collect();
            let compressed: usize = ours.iter().map(Vec::len).sum();
            let c = speed(total, || {
                for b in &blocks {
                    black_box(teseladb_lsm::lz4_compress_block(b));
                }
            });
            let d = speed(total, || {
                for (o, b) in ours.iter().zip(&blocks) {
                    black_box(teseladb_lsm::lz4_decompress_block(o, b.len()).expect("ours"));
                }
            });
            // Into one buffer, as the C program writes into one: the speed of the codec alone.
            let mut buf = Vec::with_capacity(1 << 20);
            let c_into = speed(total, || {
                for b in &blocks {
                    buf.clear();
                    teseladb_lsm::lz4_compress_block_into(b, &mut buf);
                    black_box(&buf);
                }
            });
            let d_into = speed(total, || {
                for (o, b) in ours.iter().zip(&blocks) {
                    buf.clear();
                    assert!(teseladb_lsm::lz4_decompress_block_into(o, b.len(), &mut buf));
                    black_box(&buf);
                }
            });
            let (mut plain, mut packed) = (Vec::new(), Vec::new());
            for (o, b) in ours.iter().zip(&blocks) {
                put(&mut plain, b);
                let mut rec = (b.len() as u32).to_le_bytes().to_vec();
                rec.extend_from_slice(o);
                put(&mut packed, &rec);
            }
            fs::write(dir.join("blocks.bin"), plain).expect("blocks.bin");
            fs::write(dir.join("ours.bin"), packed).expect("ours.bin");
            println!(
                "ours: {} blocks, {total} bytes, ratio {:.3}, compress {c:.0} MB/s, decompress {d:.0} MB/s",
                blocks.len(),
                compressed as f64 / total as f64
            );
            println!(
                "ours into one buffer: compress {c_into:.0} MB/s, decompress {d_into:.0} MB/s"
            );
        }
        // Only decompression, `rounds` times over the corpus into one buffer: a run for a
        // profiler. A fourth argument, `docs` or `text`, keeps one half of the corpus.
        "decode" => {
            let rounds: usize = args.get(3).map_or(50, |r| r.parse().expect("rounds"));
            let half = match args.get(4).map(String::as_str) {
                Some("docs") => 0..BLOCKS / 2,
                Some("text") => BLOCKS / 2..BLOCKS,
                _ => 0..BLOCKS,
            };
            let blocks = &blocks[half];
            let total: usize = blocks.iter().map(Vec::len).sum();
            let ours: Vec<Vec<u8>> =
                blocks.iter().map(|b| teseladb_lsm::lz4_compress_block(b)).collect();
            let mut buf = Vec::with_capacity(1 << 20);
            let t = Instant::now();
            for _ in 0..rounds {
                for (o, b) in ours.iter().zip(blocks) {
                    buf.clear();
                    assert!(teseladb_lsm::lz4_decompress_block_into(o, b.len(), &mut buf));
                    black_box(&buf);
                }
            }
            let mbs = (total * rounds) as f64 / t.elapsed().as_secs_f64() / 1e6;
            println!("ours decoding {rounds} rounds: {mbs:.0} MB/s");
        }
        // The matches of our blocks of one half (`docs` or `text`) by offset, split by the loop of
        // the decoder that copies them: the fast one, or the safe one when the match ends within
        // 64 bytes of the end.
        "stats" => {
            let half = if args.get(3).map(String::as_str) == Some("text") {
                BLOCKS / 2..BLOCKS
            } else {
                0..BLOCKS / 2
            };
            let classes = ["1", "2", "4", "3,5-7", "8-15", "16-31", "32+"];
            let class = |o: usize| match o {
                1 => 0,
                2 => 1,
                4 => 2,
                3 | 5..=7 => 3,
                8..=15 => 4,
                16..=31 => 5,
                _ => 6,
            };
            let mut seen = [[(0u64, 0u64); 7]; 2];
            for b in &blocks[half] {
                for (op, offset, len) in matches(&teseladb_lsm::lz4_compress_block(b)) {
                    let safe = usize::from(op + len + 64 >= b.len());
                    let s = &mut seen[safe][class(offset)];
                    s.0 += 1;
                    s.1 += len as u64;
                }
            }
            for (name, loop_) in ["fast", "safe"].iter().zip(&seen) {
                for (c, (n, bytes)) in classes.iter().zip(loop_) {
                    println!("{name} offset {c}: {n} matches, {bytes} bytes");
                }
            }
        }
        "check" => {
            let theirs = fs::read(dir.join("theirs.bin")).expect("theirs.bin");
            let (mut bad, mut other) = (0, 0);
            for (rec, b) in records(&theirs).into_iter().zip(&blocks) {
                let size = u32::from_le_bytes(rec[..4].try_into().unwrap()) as usize;
                if teseladb_lsm::lz4_decompress_block(&rec[4..], size).as_deref() != Some(&b[..]) {
                    bad += 1;
                }
                if teseladb_lsm::lz4_compress_block(b) != rec[4..] {
                    other += 1;
                }
            }
            println!("ours decompressing theirs: {bad} blocks differ of {}", blocks.len());
            println!("ours compressing as theirs: {other} blocks not byte for byte the same");
        }
        _ => panic!("write or check"),
    }
}
