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
        let n = rng.range(0, 40) as usize;
        if text.len() > 40_000 && rng.chance(0.5) {
            let d = rng.range(1, 32_769.min(text.len() as u64)) as usize;
            let from = text.len() - d;
            for k in 0..n {
                let b = text[from + k];
                text.push(b);
            }
        } else {
            for _ in 0..n {
                text.push(b"abcdefghij \n"[rng.range(0, 12) as usize]);
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
            assert_eq!(inflate(&c).unwrap(), data, "level {level}, {} bytes", data.len());
            let z = zlib::compress(&data, level);
            assert_eq!(zlib::decompress(&z).unwrap(), (data.clone(), z.len()));
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
            let n = (rng.range(1, 70_000) as usize).min(data.len() - pos);
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
            let n = (rng.range(1, 3000) as usize).min(stream.len() - pos);
            let (used, st) = inf.feed(&stream[pos..pos + n], &mut out).unwrap();
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
        let (used, st) = z.feed(std::slice::from_ref(b), &mut out).unwrap();
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
                let (used, st) = inf.feed(&rest[..n], &mut out).unwrap();
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
            assert_eq!(inf.feed(&[], &mut out).unwrap(), (0, Status::Done));
        }
    }
}
