//! Corrupted or truncated inputs: always an error or a result, never a panic or a loop.

use teseladb_deflate::{Inflater, Status, deflate, gzip, inflate, zlib};
use teseladb_types::Rng;

#[test]
fn mutated_streams_never_panic() {
    let mut rng = Rng::new(77);
    let text: Vec<u8> = (0..30_000u32).map(|i| b"abcdefgh \n"[(i * 31 % 10) as usize]).collect();
    for level in [0, 1, 6, 9] {
        let good = deflate(&text, level);
        for _ in 0..2_000 {
            let mut bad = good.clone();
            for _ in 0..rng.range(1, 4) {
                let i = rng.below(bad.len());
                bad[i] ^= 1 << rng.range(0, 8);
            }
            if let Ok(out) = inflate(&bad) {
                assert!(out.len() < 10 * text.len() + 100_000);
            }
        }
        for cut in (0..good.len()).step_by(good.len() / 50 + 1) {
            // Truncated: either input is missing or (if the cut falls after the end) it works.
            let mut inf = Inflater::new();
            match inf.feed(&good[..cut], &mut Vec::new()) {
                Ok((used, Status::NeedInput)) => assert_eq!(used, cut),
                Ok((_, Status::Done)) | Err(_) => {}
            }
        }
    }
    for _ in 0..20_000 {
        let n = rng.below(64);
        let mut junk = vec![0u8; n];
        rng.fill(&mut junk);
        let _ = inflate(&junk);
        let _ = zlib::decompress(&[&[0x78, 0x9c][..], &junk].concat());
        let _ = gzip::decompress(&[&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3][..], &junk].concat());
    }
}
