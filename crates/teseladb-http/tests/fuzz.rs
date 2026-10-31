//! Fuzzing of the HTTP/1.1 response reader and the URL reader (013 T036, FR-018, SC-003): real
//! responses (with a length, in chunks, until the end, interim), mutated (bit flips, cuts,
//! insertions, spliced bytes) and fed in random pieces, which must give a response or an error
//! and never panic, nor take more than the limits.
//!
//! By default a few seconds run with fixed seeds; `TESELADB_FUZZ_SECS=<n>` runs `n` seconds with
//! seeds that change every round (the 24-hour run of SC-003), and `TESELADB_FUZZ_SEED` replays one
//! seed.

use std::time::{Duration, Instant};

use teseladb_http::client::Url;
use teseladb_http::http1::{Limits, ResponseReader};
use teseladb_types::Rng;

const DEFAULT_ROUNDS: u64 = 20_000;
const LIMITS: Limits = Limits { max_head: 512, max_body: 1024 };

const SEEDS: [&[u8]; 5] = [
    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"keys\": []}\n",
    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5;ext=1\r\nhello\r\n6\r\n world\r\n0\r\nX-T: t\r\n\r\n",
    b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 204 No Content\r\n\r\n",
    b"HTTP/1.1 403 Forbidden\r\n\r\n<ErrorResponse/>",
    b"https://sts.us-east-1.amazonaws.com:443/?Action=GetCallerIdentity",
];

fn mutate(rng: &mut Rng) -> Vec<u8> {
    let mut out = SEEDS[rng.range(0, SEEDS.len() as u64) as usize].to_vec();
    for _ in 0..=rng.range(0, 4) {
        let len = out.len() as u64;
        match rng.range(0, 5) {
            0 if len > 0 => {
                let i = rng.range(0, len) as usize;
                out[i] ^= 1 << rng.range(0, 8);
            }
            1 if len > 0 => out.truncate(rng.range(0, len) as usize),
            2 => {
                let i = rng.range(0, len + 1) as usize;
                let bytes = [b'\r', b'\n', b':', b' ', b'0', b'f', b';', 0xff];
                out.insert(i, bytes[rng.range(0, bytes.len() as u64) as usize]);
            }
            3 => {
                let other = SEEDS[rng.range(0, SEEDS.len() as u64) as usize];
                let cut = rng.range(0, other.len() as u64 + 1) as usize;
                let at = rng.range(0, len + 1) as usize;
                out.splice(at..at, other[..cut].iter().copied());
            }
            _ if len > 0 => {
                let i = rng.range(0, len) as usize;
                out[i] = rng.range(0, 256) as u8;
            }
            _ => {}
        }
    }
    out
}

fn feed(rng: &mut Rng) {
    let input = mutate(rng);
    let mut reader = ResponseReader::new(LIMITS);
    let mut at = 0;
    let mut done = false;
    while at < input.len() && !done {
        let step = (rng.range(1, 64) as usize).min(input.len() - at);
        match reader.feed(&input[at..at + step]) {
            Ok(Some(r)) => {
                assert!(r.body.len() <= LIMITS.max_body);
                done = true;
            }
            Ok(None) => {}
            Err(_) => done = true,
        }
        at += step;
    }
    if !done && let Ok(r) = reader.finish() {
        assert!(r.body.len() <= LIMITS.max_body);
    }
    let _ = Url::parse(&String::from_utf8_lossy(&input));
}

#[test]
fn responses_and_urls_survive_mutated_bytes() {
    let secs: Option<u64> = std::env::var("TESELADB_FUZZ_SECS").ok().and_then(|s| s.parse().ok());
    let replay: Option<u64> = std::env::var("TESELADB_FUZZ_SEED").ok().and_then(|s| s.parse().ok());
    let start = Instant::now();
    let mut seed = replay.unwrap_or(0x5eed_4777);
    let mut rounds = 0u64;
    loop {
        let mut rng = Rng::new(seed);
        for _ in 0..100 {
            feed(&mut rng);
            rounds += 1;
        }
        if replay.is_some() {
            break;
        }
        match secs {
            Some(s) if start.elapsed() < Duration::from_secs(s) => {
                seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15)
            }
            Some(_) => break,
            None if rounds < DEFAULT_ROUNDS => seed += 1,
            None => break,
        }
    }
    eprintln!("fuzz: {rounds} inputs in {:?}", start.elapsed());
}
