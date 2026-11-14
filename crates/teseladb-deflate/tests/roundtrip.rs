//! Round trips with random and structured data at every level, input and output in pieces of any
//! size, the full 32 KiB window and interoperability with Python's zlib.

use teseladb_deflate::{Deflater, Inflater, Status, deflate, inflate, zlib};
use teseladb_types::Rng;

fn samples(rng: &mut Rng) -> Vec<Vec<u8>> {
    let mut v = vec![Vec::new(), b"a".to_vec(), b" world".to_vec(), b"abcabcabcabc".to_vec()];
    let mut random = vec![0u8; 70_000];
    rng.fill(&mut random);
    v.push(random);
    // Text with repetitions at every distance (up to beyond the window).
    let mut text = Vec::new();
    while text.len() < 300_000 {
        let n = rng.below(40);
        if text.len() > 40_000 && rng.chance(0.5) {
            let d = 1 + rng.below(32_768.min(text.len() - 1));
            let from = text.len() - d;
            for k in 0..n {
                let b = text[from + k];
                text.push(b);
            }
        } else {
            for _ in 0..n {
                text.push(b"abcdefghij \n"[rng.below(12)]);
            }
        }
    }
    v.push(text);
    v.push(vec![0u8; 200_000]);
    v
}

#[test]
fn all_levels_round_trip() {
    let mut rng = Rng::new(11);
    for data in samples(&mut rng) {
        for level in 0..=9 {
            let c = deflate(&data, level);
            assert_eq!(
                inflate(&c).expect("inflate succeeds"),
                data,
                "level {level}, {} bytes",
                data.len()
            );
            let z = zlib::compress(&data, level);
            assert_eq!(zlib::decompress(&z).expect("decompress succeeds"), (data.clone(), z.len()));
        }
    }
}

#[test]
fn chunked_input_and_output() {
    let mut rng = Rng::new(5);
    for data in samples(&mut rng) {
        // Compression in random pieces.
        let mut d = Deflater::new(6);
        let mut c = Vec::new();
        let mut pos = 0;
        while pos < data.len() {
            let n = (1 + rng.below(70_000 - 1)).min(data.len() - pos);
            d.feed(&data[pos..pos + n], &mut c);
            pos += n;
        }
        d.finish(&mut c);
        d.finish(&mut c); // idempotent
        // Decompression in random pieces (also of one byte), with garbage behind.
        let tail = b"DESPUES";
        let stream = [&c[..], tail].concat();
        let mut inf = Inflater::new();
        let mut out = Vec::new();
        let mut pos = 0;
        loop {
            let n = (1 + rng.below(3000 - 1)).min(stream.len() - pos);
            let (used, st) = inf.feed(&stream[pos..pos + n], &mut out).expect("feed succeeds");
            pos += used;
            if st == Status::Done {
                break;
            }
            assert_eq!(used, n, "unfinished, everything is consumed");
        }
        let trailing = inf.take_trailing();
        let rest = [&trailing[..], &stream[pos..]].concat();
        assert_eq!(rest, tail, "what follows the stream is not counted as its own");
        assert_eq!(out, data);
    }
}

#[test]
fn one_byte_at_a_time() {
    let data = b"The DEFLATE window is 32 KiB; references may overlap: aaaaaaaaaaaaaaaaaaaaaaaa"
        .repeat(50);
    let c = zlib::compress(&data, 9);
    let mut z = zlib::ZlibDecoder::new();
    let mut out = Vec::new();
    for (i, b) in c.iter().enumerate() {
        let (used, st) = z.feed(std::slice::from_ref(b), &mut out).expect("feed succeeds");
        assert_eq!(used, 1);
        assert_eq!(st == Status::Done, i == c.len() - 1);
    }
    assert!(z.is_done());
    assert_eq!(out, data);
}

#[test]
fn levels_trade_speed_for_size() {
    let mut rng = Rng::new(9);
    let text = &samples(&mut rng)[5];
    let sizes: Vec<usize> = [1, 6, 9].iter().map(|&l| deflate(text, l).len()).collect();
    assert!(sizes[2] <= sizes[1] && sizes[1] <= sizes[0], "{sizes:?}");
    assert!(sizes[0] < text.len() / 2, "{sizes:?}");
    let random = &samples(&mut rng)[4];
    // Random data grows no more than the stored-block overhead.
    assert!(deflate(random, 6).len() <= random.len() + 64);
}

#[test]
fn inflater_fed_in_small_pieces_with_trailing_bytes() {
    let data: Vec<u8> = (0..20_000u32).map(|i| (i * 7 % 251) as u8).collect();
    for level in [0u8, 1, 6] {
        let mut z = deflate(&data, level);
        z.extend_from_slice(b"TAIL");
        for piece in [1usize, 3, 7, 64] {
            let mut inf = Inflater::new();
            let mut out = Vec::new();
            let mut rest: &[u8] = &z;
            let mut done = false;
            while !rest.is_empty() {
                let n = piece.min(rest.len());
                let (used, st) = inf.feed(&rest[..n], &mut out).expect("feed succeeds");
                assert!(used <= n);
                if st == Status::Done {
                    done = true;
                    let mut tail = inf.take_trailing();
                    tail.extend_from_slice(&rest[used..]);
                    assert_eq!(tail, b"TAIL", "level {level}, pieces of {piece}");
                    break;
                }
                assert_eq!(used, n, "NeedInput consumes everything");
                rest = &rest[n..];
            }
            assert!(done && inf.is_done());
            assert_eq!(out, data);
            // Nothing more to read: it stays finished.
            assert_eq!(inf.feed(&[], &mut out).expect("feed succeeds"), (0, Status::Done));
        }
    }
}

#[test]
fn trailing_bytes_come_back_wherever_the_input_is_split() {
    let data: Vec<u8> = b"two feeds, then the bytes that follow the stream. ".repeat(40);
    for level in [0u8, 6] {
        let mut z = deflate(&data, level);
        let stream_len = z.len();
        z.extend_from_slice(b"FOLLOWER");
        for split in 1..z.len() {
            let mut inf = Inflater::new();
            let mut out = Vec::new();
            let (used, st) = inf.feed(&z[..split], &mut out).expect("the first part");
            let mut tail = if st == Status::Done {
                let mut t = inf.take_trailing();
                t.extend_from_slice(&z[used..]);
                t
            } else {
                assert_eq!(used, split);
                let (used, st) = inf.feed(&z[split..], &mut out).expect("the second part");
                assert_eq!(st, Status::Done, "level {level}, split {split}");
                let mut t = inf.take_trailing();
                t.extend_from_slice(&z[split + used..]);
                t
            };
            assert_eq!(out, data, "level {level}, split {split}");
            assert_eq!(tail.len(), z.len() - stream_len);
            assert_eq!(std::mem::take(&mut tail), b"FOLLOWER");
        }
    }
}

#[test]
fn decoders_by_default_are_new_ones() {
    assert!(!Inflater::default().is_done());
    assert_eq!(Inflater::default().total_out(), 0);
    let mut out = Vec::new();
    let z = zlib::compress(b"x", 6);
    let mut zd = zlib::ZlibDecoder::default();
    zd.feed(&z, &mut out).expect("a zlib stream");
    assert_eq!(out, b"x");
    let g = teseladb_deflate::gzip::compress(b"y", 6);
    let mut gd = teseladb_deflate::gzip::GzipDecoder::default();
    out.clear();
    gd.feed(&g, &mut out).expect("a gzip member");
    assert_eq!(out, b"y");
}
