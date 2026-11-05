//! Fuzzing of the Extended JSON and BSON bodies and of the REST request path (019 T004, FR-029,
//! SC-006): valid bodies, mutated (bit flips, cuts, growth, bytes spliced from other bodies),
//! must be read or refused with `400`, never panic; a body that reads writes back in canonical
//! Extended JSON and reads again to the same document. Each mutated body also goes through
//! [`handle`] on a random route with a random `Content-Type`, `Accept` and causal header.
//!
//! By default a few seconds run with fixed seeds; `TESELADB_FUZZ_SECS=<n>` runs `n` seconds with
//! seeds that change every round (the 24-hour run of SC-006), and `TESELADB_FUZZ_SEED` replays
//! one seed.

use std::time::{Duration, Instant};

use teseladb_rest::api::{AFTER_CLUSTER_TIME, Executor, handle};
use teseladb_rest::media::{BSON, CANONICAL_JSON, Format, JSON, decode, encode};
use teseladb_rest::routes::ROUTES;
use teseladb_bson::{Document, Value};
use teseladb_http::http1::Request;
use teseladb_http::server::Response;
use teseladb_types::Rng;

const DEFAULT_ROUNDS: u64 = 20_000;

/// Valid bodies the mutations start from.
const SEEDS: [&str; 8] = [
    r#"{"filter":{"a":{"$gt":{"$numberLong":"9007199254740993"}}},"sort":{"b":-1},"limit":10}"#,
    r#"{"document":{"_id":{"$oid":"5f0c8a7b1c9d440000a1b2c3"},"t":{"$date":"2026-11-02T10:00:00Z"}}}"#,
    r#"{"documents":[{"d":{"$numberDecimal":"1.5E+3"}},{"b":{"$binary":{"base64":"AAE=","subType":"04"}}}]}"#,
    r#"{"pipeline":[{"$match":{"r":{"$regularExpression":{"pattern":"^a","options":"i"}}}}]}"#,
    r#"{"update":{"$set":{"x":{"$numberDouble":"-Infinity"},"ts":{"$timestamp":{"t":1,"i":2}}}}}"#,
    r#"{"k":{"$minKey":1},"m":{"$maxKey":1},"c":{"$code":"x"},"s":{"$symbol":"y"},"u":{"$undefined":true}}"#,
    r#"[{"$set":{"v":2}}]"#,
    r#"{"collStats":"items","scale":1024,"$readPreference":{"mode":"secondary"}}"#,
];

/// Bytes that move Extended JSON into interesting states.
const TOKENS: [&[u8]; 14] = [
    b"{",
    b"}",
    b"[",
    b"]",
    b"\"",
    b":",
    b",",
    b"\\u",
    b"null",
    b"-0.5e999",
    b"\xff",
    b"{\"$numberLong\":",
    b"{\"$oid\":\"",
    b"{\"$date\":{\"$numberLong\":\"",
];

const HEADERS: [Option<&str>; 5] =
    [None, Some(JSON), Some(CANONICAL_JSON), Some(BSON), Some("text/x")];

/// Answers every command with an error, so each route runs to its answer.
struct Refuse;

impl Executor for Refuse {
    fn run(&mut self, _: &str, command: Document) -> Document {
        Document::new().with("ok", 0.0).with("code", 2).with("errmsg", format!("{}", command.len()))
    }
    fn node(&self) -> u64 {
        1
    }
    fn cursor_ns(&mut self, _: i64) -> Option<String> {
        Some("a.b".to_owned())
    }
    fn object_id(&mut self) -> [u8; 12] {
        [1; 12]
    }
    fn forward(&mut self, _: u64, _: &Request) -> Option<Response> {
        None
    }

    fn random(&mut self, out: &mut [u8]) {
        out.fill(1);
    }

    fn now_ms(&self) -> u64 {
        0
    }

    fn principal(&self) -> String {
        String::new()
    }

    fn run_internal(&mut self, db: &str, command: Document) -> Document {
        self.run(db, command)
    }
}

fn mutate(rng: &mut Rng, start: &[u8]) -> Vec<u8> {
    let mut m = start.to_vec();
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

/// Whether a name appears twice in `doc` or in a document inside it.
fn duplicated(doc: &Document) -> bool {
    fn value(v: &Value) -> bool {
        match v {
            Value::Document(d) => duplicated(d),
            Value::Array(vs) => vs.iter().any(value),
            Value::JavaScriptWithScope { scope, .. } => duplicated(scope),
            _ => false,
        }
    }
    let mut names: Vec<&str> = doc.iter().map(|(k, _)| k).collect();
    names.sort_unstable();
    names.windows(2).any(|w| w[0] == w[1]) || doc.iter().any(|(_, v)| value(v))
}

/// A body that reads writes back and reads again to itself.
fn round_trips(body: &[u8], format: Format) {
    let Ok(doc) = decode(body, format) else { return };
    for out in [Format::Canonical, Format::Bson] {
        let bytes = encode(&doc, out).expect("a document that read writes back");
        let again = match decode(&bytes, out) {
            Ok(d) => d,
            // BSON allows a name twice, Extended JSON refuses it when reading.
            Err(e) => {
                assert!(out != Format::Bson && duplicated(&doc), "{e:?}");
                continue;
            }
        };
        // Compared in the output format: NaN is not equal to itself as a value, and Extended
        // JSON writes every NaN as `"NaN"`, without the payload bits BSON may carry.
        assert_eq!(encode(&again, out).ok(), Some(bytes), "{out:?}");
    }
}

fn round(seed: u64) {
    let run = std::panic::AssertUnwindSafe(|| {
        let mut rng = Rng::new(seed);
        let start = SEEDS[rng.range(0, SEEDS.len() as u64) as usize];
        // Half the rounds mutate the BSON of the seed instead of its text.
        let bson = rng
            .chance(0.5)
            .then(|| decode(start.as_bytes(), Format::Relaxed).ok())
            .flatten()
            .and_then(|d| encode(&d, Format::Bson).ok());
        let body = match &bson {
            Some(b) => mutate(&mut rng, b),
            None => mutate(&mut rng, start.as_bytes()),
        };
        for f in [Format::Relaxed, Format::Canonical, Format::Bson] {
            round_trips(&body, f);
        }
        let route = ROUTES[rng.range(0, ROUTES.len() as u64) as usize];
        let path = route.path.replace("{db}", "a").replace("{coll}", "b").replace("{name}", "n");
        let id =
            String::from_utf8_lossy(&body[..body.len().min(12)]).replace(['/', '?', '%', '#'], "_");
        let path = path.replace(
            "{id}",
            if route.op == teseladb_rest::routes::Op::GetMore { "1.1" } else { &id },
        );
        let mut req = Request::new(route.method, &format!("/v1{path}")).body(body.clone());
        if let Some(t) = HEADERS[rng.range(0, HEADERS.len() as u64) as usize] {
            req = req.header("Content-Type", t);
        }
        if let Some(t) = HEADERS[rng.range(0, HEADERS.len() as u64) as usize] {
            req = req.header("Accept", t);
        }
        if rng.chance(0.3) {
            let h = String::from_utf8_lossy(&body[..body.len().min(40)]).into_owned();
            req = req.header(AFTER_CLUSTER_TIME, &h);
        }
        let r = handle(&req, &mut Refuse);
        assert!((200..600).contains(&r.status), "{}", r.status);
    });
    if std::panic::catch_unwind(run).is_err() {
        panic!("finding: replay with TESELADB_FUZZ_SEED={seed}");
    }
}

#[test]
fn bodies_and_requests_survive_mutation() {
    for s in SEEDS {
        let format = Format::Relaxed;
        let body = if s.starts_with('[') { format!("{{\"pipeline\":{s}}}") } else { s.to_owned() };
        assert!(decode(body.as_bytes(), format).is_ok(), "{s}");
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
