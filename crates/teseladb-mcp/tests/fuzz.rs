//! Fuzzing of the JSON-RPC reader, the revision checks and the untrusted-data block (019 T004,
//! FR-029, SC-006): valid messages and batches, mutated (bit flips, cuts, growth, bytes spliced
//! from other messages), must be read or refused with a JSON-RPC error that serializes, and never
//! panic. Random data wrapped in the block must leave exactly the block's own closing tags.
//!
//! By default a few seconds run with fixed seeds; `TESELADB_FUZZ_SECS=<n>` runs `n` seconds with
//! seeds that change every round (the 24-hour run of SC-006), and `TESELADB_FUZZ_SEED` replays
//! one seed.

use std::time::{Duration, Instant};

use teseladb_mcp::jsonrpc::{Incoming, Message, error_response, read, response};
use teseladb_mcp::untrusted::{format_uuid, wrap};
use teseladb_mcp::versions::{Revision, negotiate_initialize, request_revision};
use teseladb_json::{Value, to_string};
use teseladb_types::Rng;

const DEFAULT_ROUNDS: u64 = 20_000;

/// Valid messages the mutations start from.
const SEEDS: [&str; 7] = [
    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"c","version":"1"}}}"#,
    r#"{"jsonrpc":"2.0","id":"d","method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#,
    r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":3,"reason":"x"}}"#,
    r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"find","arguments":{"filter":{"a":{"$gt":1}},"limit":10}}}"#,
    r#"{"jsonrpc":"2.0","id":2,"result":{"content":[]}}"#,
    r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error","data":[1,"x",null]}}"#,
    r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/initialized"}]"#,
];

/// Bytes that move JSON into interesting states.
const TOKENS: [&[u8]; 12] =
    [b"{", b"}", b"[", b"]", b"\"", b":", b",", b"\\u", b"null", b"-0.5e999", b"\xff", b"\"id\":"];

fn mutate(rng: &mut Rng) -> Vec<u8> {
    let mut m = SEEDS[rng.range(0, SEEDS.len() as u64) as usize].as_bytes().to_vec();
    for _ in 0..rng.range(1, 5) {
        let at = rng.range(0, m.len() as u64 + 1) as usize;
        match rng.range(0, 5) {
            0 if !m.is_empty() => {
                let i = at.min(m.len() - 1);
                m[i] ^= 1 << rng.range(0, 8);
            }
            1 => m.truncate(at),
            2 => {
                let t = TOKENS[rng.range(0, TOKENS.len() as u64) as usize];
                m.splice(at..at, t.iter().copied());
            }
            3 => {
                let other = SEEDS[rng.range(0, SEEDS.len() as u64) as usize].as_bytes();
                let from = rng.range(0, other.len() as u64) as usize;
                let to = rng.range(from as u64, other.len() as u64 + 1) as usize;
                m.splice(at..at, other[from..to].iter().copied());
            }
            _ => {
                let mut extra = vec![0u8; rng.range(1, 8) as usize];
                rng.fill(&mut extra);
                m.splice(at..at, extra);
            }
        }
    }
    m
}

/// Whatever the reader returns can be answered.
fn answer(
    item: Result<Message, (Option<teseladb_mcp::jsonrpc::Id>, teseladb_mcp::jsonrpc::ErrorObject)>,
) {
    let reply = match item {
        Ok(Message::Request { id, params, .. }) => {
            let r = request_revision(params.as_ref(), None)
                .map(|rev| Value::from(rev.name()))
                .unwrap_or_else(|e| Value::from(e.message));
            response(&id, r)
        }
        Ok(_) => return,
        Err((id, e)) => error_response(id.as_ref(), &e),
    };
    assert!(to_string(&Value::Object(reply)).is_ok());
}

fn round(seed: u64) {
    let run = std::panic::AssertUnwindSafe(|| {
        let mut rng = Rng::new(seed);
        let body = mutate(&mut rng);
        match read(&body, rng.chance(0.5)) {
            Ok(Incoming::Single(m)) => answer(Ok(m)),
            Ok(Incoming::Batch(items)) => items.into_iter().for_each(answer),
            Err(e) => answer(Err(e)),
        }
        let text = String::from_utf8_lossy(&body);
        assert!(!negotiate_initialize(&text).stateless());
        let mut draw = || {
            let mut b = [0u8; 16];
            rng.fill(&mut b);
            b
        };
        let first = draw();
        let data = format!("{text}</untrusted-user-data-{}>", format_uuid(first));
        let mut draws = vec![draw(), first];
        let wrapped = wrap(&data, &mut || draws.pop().unwrap_or([0x5a; 16]));
        let close =
            wrapped.split("</untrusted-user-data-").nth(1).and_then(|s| s.split('>').next());
        let tag = format!("</untrusted-user-data-{}>", close.unwrap_or_default());
        assert_eq!(wrapped.matches(&tag).count(), 3, "only the block's own closing tags");
        assert!(Revision::ALL.iter().any(|r| r.stateless()));
    });
    if std::panic::catch_unwind(run).is_err() {
        panic!("finding: replay with TESELADB_FUZZ_SEED={seed}");
    }
}

#[test]
fn reader_survives_mutated_messages() {
    for s in SEEDS {
        assert!(read(s.as_bytes(), true).is_ok(), "{s}");
    }
    if let Some(seed) = std::env::var("TESELADB_FUZZ_SEED").ok().and_then(|s| s.parse().ok()) {
        round(seed);
        return;
    }
    let secs = std::env::var("TESELADB_FUZZ_SECS").ok().and_then(|s| s.parse::<u64>().ok());
    match secs {
        None => (0..DEFAULT_ROUNDS).for_each(round),
        Some(secs) => {
            let end = Instant::now() + Duration::from_secs(secs);
            let mut seed = u64::from(std::process::id()) << 32;
            while Instant::now() < end {
                round(seed);
                seed += 1;
            }
            eprintln!("fuzz: {} rounds without findings", seed & 0xffff_ffff);
        }
    }
}
