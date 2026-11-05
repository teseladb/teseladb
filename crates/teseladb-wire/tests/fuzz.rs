//! Fuzzing of the wire protocol's reader (006 T010, FR-030, SC-005): valid messages of every
//! kind, mutated (bit flips, cut, grown, lengths rewritten, bytes spliced from other messages)
//! and fed in random pieces to a connection, which must answer, close, or wait for more, and
//! never panic or read out of bounds.
//!
//! By default a few seconds run with fixed seeds; `TESELADB_FUZZ_SECS=<n>` runs `n` seconds with
//! seeds that change every round (the 24-hour run of SC-005), and `TESELADB_FUZZ_SEED` replays
//! one seed.

use std::time::{Duration, Instant};

use teseladb_wire::compressed::{self, Compressor};
use teseladb_wire::connection::Connection;
use teseladb_wire::dispatch::{ClusterTime, NodeInfo, Topology, dispatch};
use teseladb_wire::msg::{HEADER_LEN, Header, flags, opcode};
use teseladb_bson::{Document, Value};
use teseladb_hash::crc32c;
use teseladb_types::Rng;

const DEFAULT_ROUNDS: u64 = 3_000;

fn node() -> NodeInfo {
    NodeInfo {
        topology: Topology { process_id: [7; 12], counter: 0 },
        cluster_time: ClusterTime { time: 1, increment: 1 },
        now_ms: 0,
        keys: None,
        replica_set: None,
    }
}

fn op_msg(flag_bits: u32, sections: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let len = (HEADER_LEN + 4 + sections.len()) as i32;
    Header { length: len, request_id: 1, response_to: 0, op_code: opcode::OP_MSG }.write(&mut out);
    out.extend_from_slice(&flag_bits.to_le_bytes());
    out.extend_from_slice(sections);
    out
}

/// Valid messages the mutations start from.
fn seeds() -> Vec<Vec<u8>> {
    let tv = Document::new().with("processId", Value::ObjectId([7; 12])).with("counter", 0i64);
    let docs = [
        Document::new().with("ping", 1).with("$db", "admin"),
        Document::new()
            .with("hello", 1)
            .with("compression", vec![Value::from("zstd"), Value::from("snappy")])
            .with(
                "client",
                Document::new()
                    .with("driver", Document::new().with("name", "x").with("version", "1"))
                    .with("os", Document::new().with("type", "L")),
            )
            .with("$db", "admin"),
        Document::new()
            .with("hello", 1)
            .with("topologyVersion", tv)
            .with("maxAwaitTimeMS", 10)
            .with("$db", "admin"),
        Document::new()
            .with("insert", "c")
            .with("$db", "t")
            .with("nested", Document::new().with("a", vec![Value::Int32(1), Value::Null])),
    ];
    let mut out = Vec::new();
    for d in &docs {
        let mut s = vec![0u8];
        s.extend_from_slice(&d.to_bytes().unwrap());
        out.push(op_msg(0, &s));
        // With a document sequence.
        let mut seq = b"documents\0".to_vec();
        seq.extend_from_slice(&Document::new().with("a", 1).to_bytes().unwrap());
        let mut with_seq = s.clone();
        with_seq.push(1);
        with_seq.extend_from_slice(&((seq.len() + 4) as i32).to_le_bytes());
        with_seq.extend_from_slice(&seq);
        out.push(op_msg(flags::EXHAUST_ALLOWED, &with_seq));
        // With a checksum.
        let mut c = op_msg(flags::CHECKSUM_PRESENT, &s);
        let len = (c.len() + 4) as i32;
        c[..4].copy_from_slice(&len.to_le_bytes());
        let crc = crc32c(&c);
        c.extend_from_slice(&crc.to_le_bytes());
        out.push(c);
        // Compressed each way.
        for k in Compressor::ALL {
            out.push(compressed::compress(&op_msg(0, &s), k));
        }
        // As an OP_QUERY.
        let mut q = 0i32.to_le_bytes().to_vec();
        q.extend_from_slice(b"admin.$cmd\0");
        q.extend_from_slice(&[0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]);
        q.extend_from_slice(&d.to_bytes().unwrap());
        let mut m = Vec::new();
        Header {
            length: (HEADER_LEN + q.len()) as i32,
            request_id: 2,
            response_to: 0,
            op_code: opcode::OP_QUERY,
        }
        .write(&mut m);
        m.extend_from_slice(&q);
        out.push(m);
    }
    out
}

fn mutate(rng: &mut Rng, pool: &[Vec<u8>]) -> Vec<u8> {
    let mut m = pool[rng.range(0, pool.len() as u64) as usize].clone();
    for _ in 0..rng.range(1, 5) {
        match rng.range(0, 6) {
            0 if !m.is_empty() => {
                let at = rng.range(0, m.len() as u64) as usize;
                m[at] ^= 1 << rng.range(0, 8);
            }
            1 => m.truncate(rng.range(0, m.len() as u64 + 1) as usize),
            2 => {
                let mut extra = vec![0u8; rng.range(1, 32) as usize];
                rng.fill(&mut extra);
                let at = rng.range(0, m.len() as u64 + 1) as usize;
                m.splice(at..at, extra);
            }
            3 if m.len() >= 4 => {
                // Rewrite the length to the real one or a nearby value: the parser goes deeper.
                let delta = rng.range(0, 9) as i64 - 4;
                let len = (m.len() as i64 + delta) as i32;
                m[..4].copy_from_slice(&len.to_le_bytes());
            }
            4 if m.len() > HEADER_LEN + 8 => {
                // An inner length (a section, document or string length) set to an edge value.
                let at = rng.range(HEADER_LEN as u64, (m.len() - 4) as u64) as usize;
                let v = [0i32, -1, 1, 4, 5, i32::MAX, i32::MIN][rng.range(0, 7) as usize];
                m[at..at + 4].copy_from_slice(&v.to_le_bytes());
            }
            _ => {
                let other = &pool[rng.range(0, pool.len() as u64) as usize];
                let from = rng.range(0, other.len() as u64) as usize;
                let at = rng.range(0, m.len() as u64 + 1) as usize;
                m.splice(at..at, other[from..].iter().copied());
            }
        }
    }
    m
}

/// Feeds `stream` in random pieces; every request is dispatched and answered.
fn feed(rng: &mut Rng, stream: &[u8]) {
    let mut c = Connection::new(1);
    let mut at = 0;
    while at < stream.len() {
        let n = rng.range(1, 64.min(stream.len() - at) as u64 + 1) as usize;
        c.receive(&stream[at..at + n]);
        at += n;
        loop {
            match c.next_request() {
                Ok(Some(req)) => {
                    if let teseladb_wire::dispatch::Action::Reply(d) =
                        dispatch(&mut c, &req, &node())
                    {
                        let bytes = c.reply(&req, &d, false);
                        assert!(bytes.len() >= HEADER_LEN);
                    }
                }
                Ok(None) => break,
                Err(_) => return,
            }
        }
    }
}

fn round(seed: u64, pool: &[Vec<u8>]) {
    let run = std::panic::AssertUnwindSafe(|| {
        let mut rng = Rng::new(seed);
        let mut stream = Vec::new();
        for _ in 0..rng.range(1, 4) {
            stream.extend_from_slice(&mutate(&mut rng, pool));
        }
        feed(&mut rng, &stream);
    });
    if std::panic::catch_unwind(run).is_err() {
        panic!("finding: replay with TESELADB_FUZZ_SEED={seed}");
    }
}

#[test]
fn reader_survives_mutated_messages() {
    let pool = seeds();
    // Every seed message is valid as it is.
    for m in &pool {
        let mut c = Connection::new(1);
        c.receive(m);
        assert!(c.next_request().unwrap().is_some());
    }
    if let Some(seed) = std::env::var("TESELADB_FUZZ_SEED").ok().and_then(|s| s.parse().ok()) {
        round(seed, &pool);
        return;
    }
    let secs = std::env::var("TESELADB_FUZZ_SECS").ok().and_then(|s| s.parse::<u64>().ok());
    match secs {
        None => (0..DEFAULT_ROUNDS).for_each(|seed| round(seed, &pool)),
        Some(secs) => {
            let end = Instant::now() + Duration::from_secs(secs);
            let mut seed = u64::from(std::process::id()) << 32;
            while Instant::now() < end {
                round(seed, &pool);
                seed += 1;
            }
            eprintln!("fuzz: {} rounds without findings", seed & 0xffff_ffff);
        }
    }
}
