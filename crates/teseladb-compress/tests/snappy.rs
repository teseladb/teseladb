//! Snappy (raw format, as RefDb's `OP_COMPRESSED` with compressor id 1 carries it): vectors
//! written by hand from the format description, round trips and corrupt streams.

use teseladb_compress::{Error, snappy};
use teseladb_types::Rng;

const LIMIT: usize = 64 << 20;

fn text(rng: &mut Rng, n: usize) -> Vec<u8> {
    const WORDS: [&str; 12] =
        ["the", "of", "range", "leader", "raft", "core", "and", "document", "a", "is", "to", "log"];
    let mut out = Vec::with_capacity(n + 16);
    while out.len() < n {
        out.extend_from_slice(WORDS[rng.below(WORDS.len())].as_bytes());
        out.push(b' ');
    }
    out.truncate(n);
    out
}

#[test]
fn literal_only_stream() {
    // Length 5, then a literal of 5 bytes (tag (5 - 1) << 2).
    let s = [5, 4 << 2, b'h', b'e', b'l', b'l', b'o'];
    assert_eq!(snappy::decompress(&s, LIMIT).expect("decompress succeeds"), b"hello");
    assert_eq!(snappy::decompressed_len(&s).expect("decompressed_len succeeds"), 5);
}

#[test]
fn copies_of_each_width() {
    // "abcd" then a 1-byte-offset copy of 8 bytes at offset 4 (overlapping), a 2-byte-offset copy
    // of 4 at offset 12 and a 4-byte-offset copy of 2 at offset 3.
    let mut s = vec![18, 3 << 2, b'a', b'b', b'c', b'd'];
    s.extend_from_slice(&[0b001 | ((8 - 4) << 2), 4]);
    s.extend_from_slice(&[0b10 | ((4 - 1) << 2), 12, 0]);
    s.extend_from_slice(&[0b11 | ((2 - 1) << 2), 3, 0, 0, 0]);
    assert_eq!(snappy::decompress(&s, LIMIT).expect("decompress succeeds"), b"abcdabcdabcdabcdbc");
}

#[test]
#[expect(
    clippy::cast_possible_truncation,
    reason = "the stream is built byte by byte from lengths its match arms bound"
)]
fn long_literal_lengths() {
    for n in [59usize, 60, 61, 255, 256, 65_536, 70_000] {
        let data: Vec<u8> = (0..n).map(|i| (i * 7 % 251) as u8).collect();
        let mut s = Vec::new();
        let mut len = n;
        while len >= 0x80 {
            s.push((len as u8) | 0x80);
            len >>= 7;
        }
        s.push(len as u8);
        let m = n - 1;
        match m {
            0..=59 => s.push((m as u8) << 2),
            60..=0xff => s.extend_from_slice(&[60 << 2, m as u8]),
            0x100..=0xffff => s.extend_from_slice(&[61 << 2, m as u8, (m >> 8) as u8]),
            _ => s.extend_from_slice(&[62 << 2, m as u8, (m >> 8) as u8, (m >> 16) as u8]),
        }
        s.extend_from_slice(&data);
        assert_eq!(snappy::decompress(&s, LIMIT).expect("decompress succeeds"), data, "{n}");
    }
    // A 4-byte length (tag 63).
    let s = [3, 63 << 2, 2, 0, 0, 0, b'x', b'y', b'z'];
    assert_eq!(snappy::decompress(&s, LIMIT).expect("decompress succeeds"), b"xyz");
}

#[test]
fn round_trips() {
    let mut rng = Rng::new(6);
    let mut random = vec![0u8; 200_000];
    rng.fill(&mut random);
    let cases: Vec<Vec<u8>> = vec![
        Vec::new(),
        b"a".to_vec(),
        b"abcabcabcabcabcabcabcabcabc".to_vec(),
        vec![0; 300_000],
        text(&mut rng, 250_000),
        random,
        (0..100_000u32).flat_map(|i| (i / 3).to_le_bytes()).collect(),
    ];
    for data in cases {
        let c = snappy::compress(&data).expect("compress succeeds");
        assert_eq!(snappy::decompressed_len(&c).expect("decompressed_len succeeds"), data.len());
        assert_eq!(snappy::decompress(&c, LIMIT).expect("decompress succeeds"), data);
    }
}

#[test]
fn compresses_redundant_data() {
    let mut rng = Rng::new(7);
    let t = text(&mut rng, 100_000);
    assert!(snappy::compress(&t).expect("compress succeeds").len() < t.len() / 2);
    let zeros = vec![0u8; 100_000];
    assert!(snappy::compress(&zeros).expect("compress succeeds").len() < 5_000);
}

#[test]
fn corrupt_streams_fail() {
    // Empty input, an unterminated length, a length over 32 bits.
    assert_eq!(snappy::decompress(&[], LIMIT), Err(Error::Truncated));
    assert_eq!(snappy::decompress(&[0x80], LIMIT), Err(Error::Truncated));
    assert!(matches!(
        snappy::decompress(&[0xff, 0xff, 0xff, 0xff, 0x7f], LIMIT),
        Err(Error::Corrupt(_))
    ));
    // Declared length over the limit.
    assert_eq!(snappy::decompress(&[100, 0], 10), Err(Error::TooLarge));
    // Literal running past the end; literal length bytes missing.
    assert_eq!(snappy::decompress(&[5, 4 << 2, b'a'], LIMIT), Err(Error::Truncated));
    assert_eq!(snappy::decompress(&[5, 61 << 2, 1], LIMIT), Err(Error::Truncated));
    // Copy with offset 0, offset before the start, missing offset bytes.
    assert!(matches!(
        snappy::decompress(&[5, 0, b'a', 0b10 | (3 << 2), 0, 0], LIMIT),
        Err(Error::Corrupt(_))
    ));
    assert!(matches!(
        snappy::decompress(&[5, 0, b'a', 0b10 | (3 << 2), 2, 0], LIMIT),
        Err(Error::Corrupt(_))
    ));
    assert_eq!(snappy::decompress(&[5, 0, b'a', 0b11, 1], LIMIT), Err(Error::Truncated));
    assert_eq!(snappy::decompress(&[5, 0, b'a', 0b01], LIMIT), Err(Error::Truncated));
    // More or less output than declared.
    assert!(matches!(snappy::decompress(&[1, 4, b'a', b'b'], LIMIT), Err(Error::Corrupt(_))));
    assert!(matches!(snappy::decompress(&[3, 0, b'a'], LIMIT), Err(Error::Corrupt(_))));
    assert!(matches!(
        snappy::decompress(&[2, 0, b'a', 0b01 | (4 << 2), 1], LIMIT),
        Err(Error::Corrupt(_))
    ));
}

#[test]
fn random_input_never_panics() {
    let mut rng = Rng::new(8);
    let good = snappy::compress(&text(&mut rng, 5_000)).expect("compress succeeds");
    for i in 0..20_000 {
        let mut s = if i % 2 == 0 {
            let mut v = vec![0u8; rng.below(64)];
            rng.fill(&mut v);
            v
        } else {
            good.clone()
        };
        if !s.is_empty() {
            let flips = rng.range(1, 4);
            for _ in 0..flips {
                let at = rng.below(s.len());
                s[at] ^= 1 << rng.range(0, 8);
            }
            s.truncate(rng.below(s.len() + 1));
        }
        let _ = snappy::decompress(&s, 1 << 20);
    }
}

#[test]
fn copies_just_over_sixty_four() {
    // Matches of 65 to 67 bytes split as 60 plus the rest, so that no piece is under 4.
    let mut rng = Rng::new(12);
    for n in [64usize, 65, 66, 67, 68, 129, 131] {
        let mut x = vec![0u8; 200];
        rng.fill(&mut x);
        let mut data = x.clone();
        data.extend_from_slice(&x[..n]);
        data.push(0xee);
        assert_eq!(
            snappy::decompress(&snappy::compress(&data).expect("compress succeeds"), LIMIT)
                .expect("decompress succeeds"),
            data,
            "{n}"
        );
    }
    assert_eq!(Error::Truncated.to_string(), "truncated compressed data");
    for e in [Error::Corrupt("x"), Error::TooLarge, Error::Checksum, Error::Unsupported("y")] {
        assert!(!e.to_string().is_empty());
    }
}
