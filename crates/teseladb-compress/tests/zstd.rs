//! Zstandard (RFC 8878): frames written by the reference `zstd` command line (levels 1 to 22,
//! with and without checksum and content size), round trips of this crate's encoder and corrupt
//! frames (006 FR-002).
//!
//! The `.zst` files of `tests/data` were made with `zstd` 1.5 from the `.bin` inputs next to them
//! (`zstd -3`, `-19`, `-1 --no-check`, `-c < input` for a frame without content size and
//! `--ultra -22 --long=27`); `repeat`, `zeros`, `hello` and `empty` are rebuilt below.

use teseladb_compress::{Error, zstd};
use teseladb_types::Rng;

const LIMIT: usize = 64 << 20;

fn data(name: &str) -> Vec<u8> {
    match name {
        "empty" => Vec::new(),
        "hello" => b"hello, world\n".to_vec(),
        "repeat" => b"abcdefghij0123456789".repeat(16_000),
        "zeros" => vec![0; 500_000],
        _ => {
            std::fs::read(format!("{}/tests/data/{name}.bin", env!("CARGO_MANIFEST_DIR"))).unwrap()
        }
    }
}

fn frame(file: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/data/{file}.zst", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

const VECTORS: [(&str, &str); 13] = [
    ("empty.3", "empty"),
    ("hello.3", "hello"),
    ("text.3", "text"),
    ("text.19", "text"),
    ("text.nocheck", "text"),
    ("text.stream", "text"),
    ("repeat.3", "repeat"),
    ("repeat.22", "repeat"),
    ("random.3", "random"),
    ("zeros.3", "zeros"),
    ("mixed.3", "mixed"),
    ("mixed.19", "mixed"),
    ("mixed.stream", "mixed"),
];

#[test]
fn decodes_reference_frames() {
    for (file, input) in VECTORS {
        assert_eq!(zstd::decompress(&frame(file), LIMIT).unwrap(), data(input), "{file}");
    }
}

#[test]
fn concatenated_and_skippable_frames() {
    let mut s = frame("hello.3");
    // A skippable frame (magic 0x184D2A5?) between two frames is ignored.
    s.extend_from_slice(&[0x53, 0x2a, 0x4d, 0x18, 3, 0, 0, 0, 1, 2, 3]);
    s.extend_from_slice(&frame("text.3"));
    let mut want = data("hello");
    want.extend_from_slice(&data("text"));
    assert_eq!(zstd::decompress(&s, LIMIT).unwrap(), want);
}

#[test]
fn round_trips() {
    let mut rng = Rng::new(9);
    let mut random = vec![0u8; 300_000];
    rng.fill(&mut random);
    let mut cases = vec![
        Vec::new(),
        b"x".to_vec(),
        data("hello"),
        data("text"),
        data("mixed"),
        data("repeat"),
        data("zeros"),
        random,
        (0..200_000u32).flat_map(|i| (i % 1000).to_be_bytes()).collect(),
    ];
    // Literal and match lengths at the edges of their codes.
    for n in [15usize, 16, 17, 63, 64, 65, 131_071, 131_072, 131_073] {
        let mut v = vec![0u8; n];
        rng.fill(&mut v);
        let copy = v.clone();
        v.extend_from_slice(&copy);
        cases.push(v);
    }
    for d in cases {
        let c = zstd::compress(&d);
        assert_eq!(zstd::decompress(&c, LIMIT).unwrap(), d, "{} bytes", d.len());
    }
}

#[test]
fn compresses_redundant_data() {
    let t = data("text");
    assert!(zstd::compress(&t).len() < t.len() * 6 / 10);
    assert!(zstd::compress(&data("zeros")).len() < 1_000);
    let mut random = vec![0u8; 100_000];
    Rng::new(10).fill(&mut random);
    // Incompressible data costs only the frame and block headers.
    assert!(zstd::compress(&random).len() < random.len() + 64);
}

#[test]
fn frame_header_errors() {
    assert_eq!(zstd::decompress(&[], LIMIT), Err(Error::Truncated));
    assert!(matches!(zstd::decompress(&[1, 2, 3, 4, 5], LIMIT), Err(Error::Corrupt(_))));
    let hello = frame("hello.3");
    // Reserved bit of the frame header descriptor.
    let mut s = hello.clone();
    s[4] |= 0x08;
    assert!(matches!(zstd::decompress(&s, LIMIT), Err(Error::Corrupt(_))));
    // A dictionary id: RefDb never sends one.
    let s = [0x28, 0xb5, 0x2f, 0xfd, 0x21, 7, 5, 1, 0, 0];
    assert!(matches!(zstd::decompress(&s, LIMIT), Err(Error::Unsupported(_))));
    // Reserved block type.
    let s = [0x28, 0xb5, 0x2f, 0xfd, 0x20, 0, 0b111, 0, 0];
    assert!(matches!(zstd::decompress(&s, LIMIT), Err(Error::Corrupt(_))));
    // Declared content over the limit.
    assert_eq!(zstd::decompress(&frame("text.3"), 1000), Err(Error::TooLarge));
    // Without content size, the limit stops the output.
    assert_eq!(zstd::decompress(&frame("text.stream"), 1000), Err(Error::TooLarge));
    // Skippable frame cut short.
    assert_eq!(
        zstd::decompress(&[0x50, 0x2a, 0x4d, 0x18, 9, 0, 0, 0, 1], LIMIT),
        Err(Error::Truncated)
    );
}

#[test]
fn checksum_and_size_mismatches() {
    let mut s = frame("text.3");
    let n = s.len();
    s[n - 1] ^= 1;
    assert_eq!(zstd::decompress(&s, LIMIT), Err(Error::Checksum));
    // Content size field that does not match the blocks (`hello.3`: single segment, 1-byte size).
    let mut s = frame("hello.3");
    s[5] += 1;
    assert!(matches!(zstd::decompress(&s, LIMIT), Err(Error::Corrupt(_))));
}

#[test]
fn every_truncation_fails() {
    for file in ["hello.3", "text.19", "mixed.stream"] {
        let f = frame(file);
        for cut in (0..f.len()).step_by(f.len() / 300 + 1) {
            assert!(zstd::decompress(&f[..cut], LIMIT).is_err(), "{file} cut at {cut}");
        }
    }
}

#[test]
fn corrupt_frames_never_panic() {
    let mut rng = Rng::new(11);
    let frames =
        [frame("text.19"), frame("mixed.3"), frame("hello.3"), zstd::compress(&data("mixed"))];
    for i in 0..3_000 {
        let mut s = frames[i % frames.len()].clone();
        let flips = rng.range(1, 6);
        for _ in 0..flips {
            let at = rng.range(4, s.len() as u64) as usize;
            s[at] ^= 1 << rng.range(0, 8);
        }
        let _ = zstd::decompress(&s, 1 << 20);
    }
}

#[test]
fn content_size_without_single_segment() {
    // `hello.3` is single segment with a 1-byte size; the same blocks with a window descriptor
    // and a 4-byte size decode the same.
    let f = frame("hello.3");
    let mut s = f[..4].to_vec();
    s.extend_from_slice(&[0x80 | 0x04, 0x00]);
    s.extend_from_slice(&13u32.to_le_bytes());
    s.extend_from_slice(&f[6..]);
    assert_eq!(zstd::decompress(&s, LIMIT).unwrap(), data("hello"));
}

#[test]
fn raw_literals_larger_than_a_block() {
    // Window of 128 KiB, one compressed block whose raw literals claim 200 000 bytes.
    let mut s = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, 7 << 3];
    let body_len = 3u32;
    let header = 1 | 2 << 1 | body_len << 3;
    s.extend_from_slice(&header.to_le_bytes()[..3]);
    let n = 200_000u32;
    s.extend_from_slice(&[((n & 0xf) as u8) << 4 | 0b1100, (n >> 4) as u8, (n >> 12) as u8]);
    assert!(matches!(zstd::decompress(&s, LIMIT), Err(Error::Corrupt(_))));
}

/// A single-segment frame without checksum holding `blocks` (each: last flag, body of a
/// compressed block) and declaring `size` bytes of content.
fn handmade(size: u8, blocks: &[(bool, Vec<u8>)]) -> Vec<u8> {
    let mut s = vec![0x28, 0xb5, 0x2f, 0xfd, 0x20, size];
    for (last, body) in blocks {
        let h = u32::from(*last) | 2 << 1 | (body.len() as u32) << 3;
        s.extend_from_slice(&h.to_le_bytes()[..3]);
        s.extend_from_slice(body);
    }
    s
}

#[test]
fn rle_literals_and_rle_then_repeated_tables() {
    // Literals: RLE of ten `a`. Sequences: one, with RLE tables for the three codes (literal
    // length code 4, offset code 2 with two zero bits = offset value 4 = offset 1, match length
    // code 0 = 3), the two offset bits in a one-byte stream.
    let rle_block = vec![10 << 3 | 1, b'a', 1, 0b01_01_01_00, 4, 2, 0, 0b100];
    assert_eq!(
        zstd::decompress(&handmade(13, &[(true, rle_block.clone())]), LIMIT).unwrap(),
        vec![b'a'; 13]
    );
    // The next block repeats the three tables (mode 3).
    let repeat_block = vec![10 << 3 | 1, b'a', 1, 0b11_11_11_00, 0b100];
    let both = handmade(26, &[(false, rle_block.clone()), (true, repeat_block.clone())]);
    assert_eq!(zstd::decompress(&both, LIMIT).unwrap(), vec![b'a'; 26]);
    // Repeating with no table before, an RLE symbol past the largest code, reserved mode bits.
    assert!(zstd::decompress(&handmade(13, &[(true, repeat_block)]), LIMIT).is_err());
    let mut bad = rle_block.clone();
    bad[4] = 36;
    assert!(zstd::decompress(&handmade(13, &[(true, bad)]), LIMIT).is_err());
    let mut bad = rle_block;
    bad[3] |= 1;
    assert!(zstd::decompress(&handmade(13, &[(true, bad)]), LIMIT).is_err());
}
