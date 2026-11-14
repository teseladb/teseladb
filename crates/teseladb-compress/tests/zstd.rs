//! Zstandard (RFC 8878): frames written by the reference `zstd` command line (levels 1 to 22,
//! with and without checksum and content size), round trips of this crate's encoder and corrupt
//! frames.
//!
//! The `.zst` files of `tests/data` were made with `zstd` 1.5 from the `.bin` inputs next to them
//! (`zstd -3`, `-19`, `-1 --no-check`, `-c < input` for a frame without content size and
//! `--ultra -22 --long=27`); `repeat`, `zeros`, `hello` and `empty` are rebuilt below.
//!
//! Raw content dictionaries (RFC 8878 §5, 004 FR-022): `dict.raw` is forty JSON documents,
//! `docs.dict.zst` and `doc.dict19.zst` were made with `zstd -3 -D dict.raw docs.bin` and
//! `zstd -19 -D dict.raw doc.bin` (1.5.7), and `zstd -d -D dict.raw` decodes this crate's frames
//! with that dictionary (checked when the vectors were made).

use teseladb_compress::{Error, zstd};
use teseladb_types::Rng;

const LIMIT: usize = 64 << 20;

fn data(name: &str) -> Vec<u8> {
    match name {
        "empty" => Vec::new(),
        "hello" => b"hello, world\n".to_vec(),
        "repeat" => b"abcdefghij0123456789".repeat(16_000),
        "zeros" => vec![0; 500_000],
        _ => std::fs::read(format!("{}/tests/data/{name}.bin", env!("CARGO_MANIFEST_DIR")))
            .expect("read succeeds"),
    }
}

fn frame(file: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/data/{file}.zst", env!("CARGO_MANIFEST_DIR")))
        .expect("read succeeds")
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
        assert_eq!(
            zstd::decompress(&frame(file), LIMIT).expect("decompress succeeds"),
            data(input),
            "{file}"
        );
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
    assert_eq!(zstd::decompress(&s, LIMIT).expect("decompress succeeds"), want);
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
        assert_eq!(
            zstd::decompress(&c, LIMIT).expect("decompress succeeds"),
            d,
            "{} bytes",
            d.len()
        );
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
    // A dictionary id: the reference server never sends one.
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
            let at = 4 + rng.below(s.len() - 4);
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
    assert_eq!(zstd::decompress(&s, LIMIT).expect("decompress succeeds"), data("hello"));
}

#[test]
#[expect(
    clippy::cast_possible_truncation,
    reason = "the header fields are built byte by byte from a 200 000-byte size"
)]
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
#[expect(clippy::cast_possible_truncation, reason = "a handmade block is a few bytes long")]
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
        zstd::decompress(&handmade(13, &[(true, rle_block.clone())]), LIMIT)
            .expect("decompress succeeds"),
        vec![b'a'; 13]
    );
    // The next block repeats the three tables (mode 3).
    let repeat_block = vec![10 << 3 | 1, b'a', 1, 0b11_11_11_00, 0b100];
    let both = handmade(26, &[(false, rle_block.clone()), (true, repeat_block.clone())]);
    assert_eq!(zstd::decompress(&both, LIMIT).expect("decompress succeeds"), vec![b'a'; 26]);
    // Repeating with no table before, an RLE symbol past the largest code, reserved mode bits.
    assert!(zstd::decompress(&handmade(13, &[(true, repeat_block)]), LIMIT).is_err());
    let mut bad = rle_block.clone();
    bad[4] = 36;
    assert!(zstd::decompress(&handmade(13, &[(true, bad)]), LIMIT).is_err());
    let mut bad = rle_block;
    bad[3] |= 1;
    assert!(zstd::decompress(&handmade(13, &[(true, bad)]), LIMIT).is_err());
}

fn documents(seed: u64, n: usize) -> Vec<Vec<u8>> {
    let mut rng = Rng::new(seed);
    let cities = ["Madrid", "Sevilla", "Bilbao", "Valencia"];
    (0..n)
        .map(|i| {
            let city = cities[rng.below(4)];
            format!(
                r#"{{"_id": {i}, "name": "customer-{}", "email": "user{}@example.org", "address": {{"street": "{} Main Street", "city": "{city}", "zip": "{:05}"}}, "active": {}}}"#,
                rng.range(0, 10_000),
                rng.range(0, 100_000),
                rng.range(1, 1_000),
                rng.range(0, 100_000),
                rng.range(0, 2) == 1
            )
            .into_bytes()
        })
        .collect()
}

#[test]
fn decodes_reference_frames_with_a_raw_content_dictionary() {
    let file = |f: &str| std::fs::read(format!("{}/tests/data/{f}", env!("CARGO_MANIFEST_DIR")));
    let dict = file("dict.raw").expect("file succeeds");
    for (frame_file, input) in [("docs.dict.zst", "docs.bin"), ("doc.dict19.zst", "doc.bin")] {
        let got =
            zstd::decompress_with_dict(&file(frame_file).expect("file succeeds"), LIMIT, &dict)
                .expect("decompress_with_dict succeeds");
        assert_eq!(got, file(input).expect("file succeeds"), "{frame_file}");
    }
    // Without the dictionary its matches reach before the frame.
    assert!(zstd::decompress(&file("docs.dict.zst").expect("file succeeds"), LIMIT).is_err());
}

#[test]
fn round_trips_with_a_dictionary() {
    let docs = documents(21, 200);
    let samples: Vec<&[u8]> = docs[..150].iter().map(Vec::as_slice).collect();
    let dict = zstd::train_dict(&samples, 4096);
    assert!(!dict.is_empty() && dict.len() <= 4096, "{}", dict.len());
    let mut plain = 0;
    let mut with = 0;
    for d in &docs[150..] {
        let c = zstd::compress_with_dict(d, &dict);
        assert_eq!(
            &zstd::decompress_with_dict(&c, LIMIT, &dict).expect("decompress_with_dict succeeds"),
            d
        );
        plain += zstd::compress(d).len();
        with += c.len();
    }
    // Small documents of one shape: the dictionary pays.
    assert!(with * 10 < plain * 8, "{with} with the dictionary, {plain} without");
    // Any data, also larger than a block, round trips; an empty dictionary is no dictionary.
    for d in [Vec::new(), data("text"), data("repeat"), data("mixed")] {
        let c = zstd::compress_with_dict(&d, &dict);
        assert_eq!(
            zstd::decompress_with_dict(&c, LIMIT, &dict).expect("decompress_with_dict succeeds"),
            d
        );
        assert_eq!(zstd::compress_with_dict(&d, &[]), zstd::compress(&d));
    }
    // Another dictionary does not decode it.
    let c = zstd::compress_with_dict(&docs[160], &dict);
    let other = zstd::train_dict(&[b"something else entirely, nothing alike".as_slice()], 64);
    assert!(zstd::decompress_with_dict(&c, LIMIT, &other).is_err());
    assert_eq!(zstd::decompress_with_dict(&c, 10, &dict), Err(Error::TooLarge));
    // Only one frame goes with a dictionary.
    let mut two = c.clone();
    two.extend_from_slice(&c);
    assert!(matches!(zstd::decompress_with_dict(&two, LIMIT, &dict), Err(Error::Corrupt(_))));
    assert!(matches!(
        zstd::decompress_with_dict(&[1, 2, 3, 4], LIMIT, &dict),
        Err(Error::Corrupt(_))
    ));
}

#[test]
fn training_keeps_what_the_samples_repeat() {
    // Nothing to learn from: no dictionary.
    assert!(zstd::train_dict(&[], 1024).is_empty());
    // A capacity below one d-mer (8 bytes) holds no segment.
    assert!(zstd::train_dict(&[b"abcdefghabcdefgh".as_slice(); 3], 7).is_empty());
    assert!(zstd::train_dict(&[b"tiny".as_slice()], 1024).is_empty());
    let docs = documents(22, 300);
    let samples: Vec<&[u8]> = docs.iter().map(Vec::as_slice).collect();
    let dict = zstd::train_dict(&samples, 2048);
    assert!(dict.len() <= 2048);
    assert_eq!(dict, zstd::train_dict(&samples, 2048), "deterministic");
    let text = String::from_utf8_lossy(&dict);
    for field in ["\"address\"", "Main Street", "@example.org"] {
        assert!(text.contains(field), "{field} in {text}");
    }
    // A capacity smaller than a segment still gives one segment's worth at most.
    assert!(zstd::train_dict(&samples, 16).len() <= 16);
}

#[test]
fn corrupt_frames_with_a_dictionary_never_panic() {
    let mut rng = Rng::new(23);
    let docs = documents(24, 60);
    let samples: Vec<&[u8]> = docs.iter().map(Vec::as_slice).collect();
    let dict = zstd::train_dict(&samples, 2048);
    let frames: Vec<Vec<u8>> = docs.iter().map(|d| zstd::compress_with_dict(d, &dict)).collect();
    for i in 0..5_000 {
        let mut s = frames[i % frames.len()].clone();
        for _ in 0..rng.range(1, 6) {
            let at = 4 + rng.below(s.len() - 4);
            s[at] ^= 1 << rng.range(0, 8);
        }
        let _ = zstd::decompress_with_dict(&s, 1 << 20, &dict);
    }
}
