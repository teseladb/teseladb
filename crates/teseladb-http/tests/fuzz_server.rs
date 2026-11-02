//! Fuzzing of the server side of the HTTP port (019 T004, FR-029, SC-006): HTTP/2 conversations,
//! HPACK blocks, HTTP/1.1 requests and Server-Sent Events streams, mutated (bit flips, cuts,
//! insertions, spliced bytes) and fed in random pieces. Each must give requests, events or an
//! error, never panic, and never hand over a body or a field list past the limits.
//!
//! By default a few seconds run with fixed seeds; `TESELADB_FUZZ_SECS=<n>` runs `n` seconds with
//! seeds that change every round (the 24-hour run of SC-006), and `TESELADB_FUZZ_SEED` replays one
//! seed.

use std::time::{Duration, Instant};

use teseladb_http::hpack::{Decoder, Encoder, Field};
use teseladb_http::http1::Limits;
use teseladb_http::http2::{self, PREFACE, Settings, flag, kind};
use teseladb_http::server::{Conn, Incoming, Settings as ConnSettings};
use teseladb_http::sse::Reader;
use teseladb_types::Rng;

const DEFAULT_ROUNDS: u64 = 5_000;
const MAX_BODY: usize = 1024;
const MAX_LIST: usize = 512;

fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = (payload.len() as u32).to_be_bytes()[1..].to_vec();
    out.extend_from_slice(&[kind, flags]);
    out.extend_from_slice(&stream.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn seeds() -> Vec<Vec<u8>> {
    let mut enc = Encoder::new();
    let get = enc.encode(&[
        Field::new(":method", "GET"),
        Field::new(":scheme", "https"),
        Field::new(":path", "/v1/databases"),
        Field::new(":authority", "node1"),
        Field::new("authorization", "Bearer k"),
    ]);
    let post = enc.encode(&[
        Field::new(":method", "POST"),
        Field::new(":scheme", "https"),
        Field::new(":path", "/v1/x"),
        Field::new("content-length", "4"),
    ]);
    let mut h2 = PREFACE.to_vec();
    h2.extend(frame(kind::SETTINGS, 0, 0, &[0, 4, 0, 0, 0, 10]));
    h2.extend(frame(kind::HEADERS, flag::END_HEADERS | flag::END_STREAM, 1, &get));
    h2.extend(frame(kind::HEADERS, flag::END_HEADERS, 3, &post));
    h2.extend(frame(kind::DATA, flag::PADDED, 3, &[1, b'a', b'b', 0]));
    h2.extend(frame(kind::DATA, flag::END_STREAM, 3, b"cd"));
    h2.extend(frame(kind::WINDOW_UPDATE, 0, 0, &[0, 0, 1, 0]));
    h2.extend(frame(kind::PING, 0, 0, &[7; 8]));
    h2.extend(frame(kind::RST_STREAM, 0, 1, &[0, 0, 0, 8]));
    vec![
        h2,
        get,
        b"POST /v1/x HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\nGET / HTTP/1.1\r\nHost: h\r\n\r\n".to_vec(),
        b"event: change\nid: 1\ndata: {\"a\":1}\ndata: x\r\n\r\n: heartbeat\n\nretry: 9\ndata: y\n\n".to_vec(),
    ]
}

fn mutate(rng: &mut Rng, seeds: &[Vec<u8>]) -> Vec<u8> {
    let mut out = seeds[rng.range(0, seeds.len() as u64) as usize].clone();
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
                out.insert(i, rng.range(0, 256) as u8);
            }
            3 => {
                let other = &seeds[rng.range(0, seeds.len() as u64) as usize];
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

/// `input` in random pieces.
fn pieces<'a>(rng: &mut Rng, input: &'a [u8]) -> Vec<&'a [u8]> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < input.len() {
        let step = (rng.range(1, 64) as usize).min(input.len() - at);
        out.push(&input[at..at + step]);
        at += step;
    }
    out
}

fn feed(rng: &mut Rng, seeds: &[Vec<u8>]) {
    let input = mutate(rng, seeds);
    let settings = ConnSettings {
        http1: Limits { max_head: MAX_LIST, max_body: MAX_BODY },
        http2: Settings {
            max_body: MAX_BODY,
            max_header_list: MAX_LIST as u32,
            ..Settings::default()
        },
    };
    let mut conn = Conn::new(None, settings, 0);
    let mut h2 = http2::Connection::new(settings.http2);
    let mut sse = Reader::default();
    for p in pieces(rng, &input) {
        for i in conn.feed(p, 0) {
            if let Incoming::Request { id, request } = i {
                assert!(request.body.len() <= MAX_BODY);
                conn.respond(id, teseladb_http::server::Response::problem(200));
            }
        }
        conn.take_output();
        for e in h2.feed(p) {
            if let http2::Event::Request { request, .. } = e {
                assert!(request.body.len() <= MAX_BODY);
            }
        }
        h2.take_output();
        for e in sse.feed(p) {
            assert!(e.data.len() <= input.len());
        }
    }
    if let Ok(fields) = Decoder::new(4096, MAX_LIST).decode(&input) {
        let size: usize = fields.iter().map(|f| f.name.len() + f.value.len() + 32).sum();
        assert!(size <= MAX_LIST);
    }
}

#[test]
fn http2_hpack_http1_and_sse_survive_mutated_bytes() {
    let secs: Option<u64> = std::env::var("TESELADB_FUZZ_SECS").ok().and_then(|s| s.parse().ok());
    let replay: Option<u64> = std::env::var("TESELADB_FUZZ_SEED").ok().and_then(|s| s.parse().ok());
    let seeds = seeds();
    let start = Instant::now();
    let mut seed = replay.unwrap_or(0x5eed_2019);
    let mut rounds = 0u64;
    loop {
        let mut rng = Rng::new(seed);
        for _ in 0..100 {
            feed(&mut rng, &seeds);
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
