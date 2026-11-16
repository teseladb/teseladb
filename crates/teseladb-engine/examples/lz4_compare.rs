//! Compares the crate's LZ4 codec with a reference LZ4 implementation, on the blocks of a
//! corpus and on the same corpus as a stream: it writes the corpus and what ours makes of
//! it, times our compression and decompression, and checks what the reference program made
//! from the same corpus.
//!
//! `cargo run --release --example lz4_compare -- write <dir>` writes
//! `<dir>/blocks.bin` (the corpus in blocks), `<dir>/ours.bin` (our LZ4 blocks),
//! `<dir>/stream_ours.bin` (the corpus as one stream of 64 KiB fragments, each compressed from
//! where it is in the corpus) and `<dir>/stream_ext_ours.bin` (the same fragments, each
//! compressed from a copy in one of two buffers, so that the data before is an external
//! dictionary), and prints our ratios and speeds; `... -- check <dir>` decompresses
//! `<dir>/theirs.bin`, `<dir>/stream_theirs.bin` and `<dir>/stream_ext_theirs.bin` with ours and
//! checks them against the corpus and against ours byte for byte. Each file is a series of
//! records: length (u32 little-endian), then the bytes; the compressed files put the
//! decompressed length (u32) before each block.
//!
//! The stream is compressed as rpc's `lz4_fragmented_compressor` compresses a frame: one
//! `Lz4Stream`, reset, and `compress_fast_continue` with acceleration 0 on each fragment, into
//! room for its bound. It is decompressed with `decompress_safe_continue` into one buffer (each
//! fragment right after the one before, the prefix mode) and into buffers of 64 KiB as that
//! compressor's decompression does (each fragment in a buffer of its own, the external
//! dictionary mode).
//!
//! The corpus is two halves: documents of the shape of TeselaDB's sustained load (a padding of
//! 50 to 2,000 bytes, one in 500 of 60,000), packed in blocks of 16 KiB as its LSM writes them;
//! and text with words drawn from a vocabulary, which repeats less.

// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::fs;
use std::hint::black_box;
use std::path::Path;
use std::time::Instant;

use teseladb::engine::lz4::{self, Lz4Stream, Lz4StreamDecode};

const BLOCK: usize = 16 << 10;
const BLOCKS: usize = 4096;
const ROUNDS: usize = 5;
/// The fragments of the stream.
const FRAGMENT: usize = 64 << 10;

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

/// Compressed blocks with their decompressed lengths, as `ours.bin` keeps them.
fn packed<'a>(blocks: impl Iterator<Item = (&'a Vec<u8>, usize)>) -> Vec<u8> {
    let mut out = Vec::new();
    for (block, size) in blocks {
        let mut rec = (size as u32).to_le_bytes().to_vec();
        rec.extend_from_slice(block);
        put(&mut out, &rec);
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

/// The corpus as fragments of one stream, each compressed from where it is in `data` or, with
/// `ext`, from a copy in one of two buffers of [`FRAGMENT`] bytes taken in turn. `out` keeps a
/// buffer of the bound for each fragment, and the lengths written come back.
fn compress_stream(
    stream: &mut Lz4Stream,
    data: &[u8],
    ext: bool,
    out: &mut [Vec<u8>],
) -> Vec<usize> {
    let mut copies = [vec![0u8; FRAGMENT], vec![0u8; FRAGMENT]];
    stream.reset_stream();
    data.chunks(FRAGMENT)
        .zip(out.iter_mut())
        .enumerate()
        .map(|(k, (fragment, dst))| {
            let src = if ext {
                let copy = &mut copies[k % 2];
                copy[..fragment.len()].copy_from_slice(fragment);
                copy.as_ptr()
            } else {
                fragment.as_ptr()
            };
            // SAFETY: the fragment, `dst` and the fragment before (in `data` or in the other
            // copy) are alive and unchanged.
            unsafe {
                stream.compress_fast_continue(src, dst.as_mut_ptr(), fragment.len(), dst.len(), 0)
            }
        })
        .collect()
}

/// The fragments of a stream decompressed one after another into `out`, which they fill.
fn decompress_stream(blocks: &[&[u8]], out: &mut [u8]) -> bool {
    let mut decoder = Lz4StreamDecode::new();
    let base = out.as_mut_ptr();
    let mut at = 0;
    for block in blocks {
        let size = FRAGMENT.min(out.len() - at);
        // SAFETY: `out` has room for `size` bytes at `at`, and the fragments before are in it.
        let got = unsafe {
            decoder.decompress_safe_continue(block.as_ptr(), base.add(at), block.len(), size)
        };
        if got != Ok(size) {
            return false;
        }
        at += size;
    }
    at == out.len()
}

/// The fragments of a stream decompressed each into a buffer of its own of `buffers`, as rpc's
/// fragmented compressor gives them buffers of 64 KiB.
fn decompress_buffers(blocks: &[&[u8]], buffers: &mut [Vec<u8>], total: usize) -> bool {
    let mut decoder = Lz4StreamDecode::new();
    let mut left = total;
    for (block, buffer) in blocks.iter().zip(buffers.iter_mut()) {
        let size = FRAGMENT.min(left);
        // SAFETY: the buffer has room for `size` bytes; the one before is alive and unchanged.
        let got = unsafe {
            decoder.decompress_safe_continue(block.as_ptr(), buffer.as_mut_ptr(), block.len(), size)
        };
        if got != Ok(size) {
            return false;
        }
        left -= size;
    }
    left == 0
}

/// Ratio and speeds of the stream of `data`, with `ext` as [`compress_stream`]'s; returns the
/// fragments.
fn stream_run(data: &[u8], ext: bool, label: &str) -> Vec<Vec<u8>> {
    let total = data.len();
    let mut stream = Lz4Stream::new();
    let mut room: Vec<Vec<u8>> =
        data.chunks(FRAGMENT).map(|f| vec![0u8; lz4::compress_bound(f.len())]).collect();
    let lengths = compress_stream(&mut stream, data, ext, &mut room);
    let fragments: Vec<Vec<u8>> =
        room.iter().zip(&lengths).map(|(r, &n)| r[..n].to_vec()).collect();
    let compressed: usize = lengths.iter().sum();
    let c = speed(total, || {
        black_box(compress_stream(&mut stream, data, ext, &mut room));
    });
    let views: Vec<&[u8]> = fragments.iter().map(Vec::as_slice).collect();
    let mut out = vec![0u8; total];
    assert!(decompress_stream(&views, &mut out) && out == data, "ours {label}");
    let d = speed(total, || {
        assert!(decompress_stream(&views, &mut out));
        black_box(&out);
    });
    let mut buffers: Vec<Vec<u8>> = views.iter().map(|_| vec![0u8; FRAGMENT]).collect();
    assert!(decompress_buffers(&views, &mut buffers, total), "ours {label} in buffers");
    assert!(buffers.concat()[..total] == *data, "ours {label} in buffers");
    let d_buffers = speed(total, || {
        assert!(decompress_buffers(&views, &mut buffers, total));
        black_box(&buffers);
    });
    println!(
        "ours stream ({label}): {} fragments, ratio {:.3}, compress {c:.0} MB/s, decompress {d:.0} MB/s, decompress into 64 KiB buffers {d_buffers:.0} MB/s",
        fragments.len(),
        compressed as f64 / total as f64
    );
    fragments
}

/// Checks a stream the reference program made against ours and the corpus.
fn check_stream(dir: &Path, file: &str, ours: &str, data: &[u8]) {
    let theirs = fs::read(dir.join(file)).expect(file);
    let ours = fs::read(dir.join(ours)).expect(ours);
    let theirs: Vec<&[u8]> = records(&theirs).into_iter().map(|r| &r[4..]).collect();
    let ours: Vec<&[u8]> = records(&ours).into_iter().map(|r| &r[4..]).collect();
    let mut out = vec![0u8; data.len()];
    let decoded = decompress_stream(&theirs, &mut out) && out == data;
    let mut buffers: Vec<Vec<u8>> = theirs.iter().map(|_| vec![0u8; FRAGMENT]).collect();
    let in_buffers = decompress_buffers(&theirs, &mut buffers, data.len())
        && buffers.concat()[..data.len()] == *data;
    let other = theirs.iter().zip(&ours).filter(|(t, o)| t != o).count()
        + theirs.len().abs_diff(ours.len());
    println!(
        "ours decompressing {file}: {}, into 64 KiB buffers: {}",
        if decoded { "the corpus" } else { "DIFFERENT" },
        if in_buffers { "the corpus" } else { "DIFFERENT" }
    );
    println!(
        "{file} against ours: {other} fragments not byte for byte the same of {}",
        theirs.len()
    );
}

pub(crate) fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (mode, dir) = (args[1].as_str(), Path::new(&args[2]));
    let blocks = corpus();
    let total: usize = blocks.iter().map(Vec::len).sum();
    match mode {
        "write" => {
            let ours: Vec<Vec<u8>> = blocks.iter().map(|b| lz4::compress_block(b)).collect();
            let compressed: usize = ours.iter().map(Vec::len).sum();
            let c = speed(total, || {
                for b in &blocks {
                    black_box(lz4::compress_block(b));
                }
            });
            let d = speed(total, || {
                for (o, b) in ours.iter().zip(&blocks) {
                    black_box(lz4::decompress_block(o, b.len()).expect("ours"));
                }
            });
            // Into one buffer, as the reference program writes into one: the speed of the
            // codec alone.
            let mut buf = Vec::with_capacity(1 << 20);
            let c_into = speed(total, || {
                for b in &blocks {
                    buf.clear();
                    lz4::compress_block_into(b, &mut buf);
                    black_box(&buf);
                }
            });
            let d_into = speed(total, || {
                for (o, b) in ours.iter().zip(&blocks) {
                    buf.clear();
                    lz4::decompress_block_into(o, b.len(), &mut buf).expect("ours");
                    black_box(&buf);
                }
            });
            let mut plain = Vec::new();
            for b in &blocks {
                put(&mut plain, b);
            }
            fs::write(dir.join("blocks.bin"), plain).expect("blocks.bin");
            fs::write(dir.join("ours.bin"), packed(ours.iter().zip(blocks.iter().map(Vec::len))))
                .expect("ours.bin");
            println!(
                "ours: {} blocks, {total} bytes, ratio {:.3}, compress {c:.0} MB/s, decompress {d:.0} MB/s",
                blocks.len(),
                compressed as f64 / total as f64
            );
            println!(
                "ours into one buffer: compress {c_into:.0} MB/s, decompress {d_into:.0} MB/s"
            );
            let data = blocks.concat();
            let sizes = data.chunks(FRAGMENT).map(<[u8]>::len);
            let prefix = stream_run(&data, false, "prefix");
            fs::write(dir.join("stream_ours.bin"), packed(prefix.iter().zip(sizes.clone())))
                .expect("stream_ours.bin");
            let ext = stream_run(&data, true, "external dictionary");
            fs::write(dir.join("stream_ext_ours.bin"), packed(ext.iter().zip(sizes)))
                .expect("stream_ext_ours.bin");
        }
        // Only decompression, `rounds` times over the corpus into one buffer: a run for a
        // profiler. A fourth argument, `docs` or `text`, keeps one half of the corpus; `stream`
        // decodes the stream of the whole corpus instead.
        "decode" => {
            let rounds: usize = args.get(3).map_or(50, |r| r.parse().expect("rounds"));
            if args.get(4).map(String::as_str) == Some("stream") {
                let data = blocks.concat();
                let fragments = stream_run(&data, false, "prefix");
                let views: Vec<&[u8]> = fragments.iter().map(Vec::as_slice).collect();
                let mut out = vec![0u8; data.len()];
                let t = Instant::now();
                for _ in 0..rounds {
                    assert!(decompress_stream(&views, &mut out));
                    black_box(&out);
                }
                let mbs = (data.len() * rounds) as f64 / t.elapsed().as_secs_f64() / 1e6;
                println!("ours stream decoding {rounds} rounds: {mbs:.0} MB/s");
                return;
            }
            let half = match args.get(4).map(String::as_str) {
                Some("docs") => 0..BLOCKS / 2,
                Some("text") => BLOCKS / 2..BLOCKS,
                _ => 0..BLOCKS,
            };
            let blocks = &blocks[half];
            let total: usize = blocks.iter().map(Vec::len).sum();
            let ours: Vec<Vec<u8>> = blocks.iter().map(|b| lz4::compress_block(b)).collect();
            let mut buf = Vec::with_capacity(1 << 20);
            let t = Instant::now();
            for _ in 0..rounds {
                for (o, b) in ours.iter().zip(blocks) {
                    buf.clear();
                    lz4::decompress_block_into(o, b.len(), &mut buf).expect("ours");
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
                for (op, offset, len) in matches(&lz4::compress_block(b)) {
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
                if lz4::decompress_block(&rec[4..], size).as_deref() != Ok(&b[..]) {
                    bad += 1;
                }
                if lz4::compress_block(b) != rec[4..] {
                    other += 1;
                }
            }
            println!("ours decompressing theirs: {bad} blocks differ of {}", blocks.len());
            println!("ours compressing as theirs: {other} blocks not byte for byte the same");
            let data = blocks.concat();
            check_stream(dir, "stream_theirs.bin", "stream_ours.bin", &data);
            check_stream(dir, "stream_ext_theirs.bin", "stream_ext_ours.bin", &data);
        }
        _ => panic!("write, decode, stats or check"),
    }
}
