//! Equivalence of the REST API and the wire protocol (019 T015, FR-005, SC-001): random
//! operations run by REST on one collection and by the wire protocol, written by hand here, on a
//! twin collection with the same data; each answer and, every so often, both collections must
//! match. `TESELADB_EQUIV_OPS=<n>` runs `n` operations (SC-001 asks for 100,000) and
//! `TESELADB_EQUIV_SEED` replays one seed.

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_bson::{Document, Value, extjson};
use teseladb_http::http1::{Limits, Request, ResponseReader};
use teseladb_http::server::Settings;
use teseladb_raftlog::MemDisk;
use teseladb_rest::media::CANONICAL_JSON;
use teseladb_server::http_listener::HttpPortConfig;
use teseladb_server::node::{Node, NodeConfig};
use teseladb_types::Rng;

const DEFAULT_OPS: u64 = 3_000;
const KEYS: u64 = 200;

/// A keep-alive HTTP/1.1 connection that asks for canonical Extended JSON.
struct Http {
    s: TcpStream,
}

impl Http {
    fn call(&mut self, method: &str, target: &str, body: &Document) -> (u16, Document) {
        let text = extjson::to_canonical(
            teseladb_bson::RawDoc::from_bytes(&body.to_bytes().unwrap()).unwrap(),
        );
        let req = Request::new(method, target)
            .header("Host", "h")
            .header("Accept", CANONICAL_JSON)
            .header("Content-Type", CANONICAL_JSON)
            .body(text.into_bytes());
        self.s.write_all(&req.encode().unwrap()).unwrap();
        let mut reader = ResponseReader::new(Limits::default());
        let mut buf = [0u8; 65536];
        loop {
            let n = self.s.read(&mut buf).unwrap();
            assert!(n > 0, "closed before the answer");
            if let Some(r) = reader.feed(&buf[..n]).unwrap() {
                let d = extjson::parse(std::str::from_utf8(&r.body).unwrap()).unwrap_or_default();
                return (r.status, d);
            }
        }
    }
}

fn d(text: &str) -> Document {
    extjson::parse(text).unwrap()
}

fn field(doc: &Document, name: &str) -> Option<Value> {
    doc.get(name).cloned()
}

/// A random filter on `a` or `_id`.
fn filter(rng: &mut Rng) -> Document {
    let x = rng.range(0, KEYS) as i32;
    match rng.range(0, 4) {
        0 => Document::new().with("_id", x),
        1 => Document::new().with("a", Document::new().with("$lt", x % 10)),
        2 => Document::new().with("b", Document::new().with("$gte", x)),
        _ => Document::new(),
    }
}

struct Pair<'a> {
    http: Http,
    wire: &'a mut Client,
}

impl Pair<'_> {
    fn rest(&mut self, method: &str, route: &str, body: &Document) -> (u16, Document) {
        self.http.call(method, &format!("/v1/databases/eq/collections/r{route}"), body)
    }

    /// The wire command `name` on collection `w`, with `rest` after it.
    fn wire(&mut self, name: &str, rest: Document) -> Document {
        let mut cmd = Document::new().with(name, "w");
        for (k, v) in rest.iter() {
            cmd.push(k, v.clone());
        }
        self.wire.run("eq", cmd)
    }

    fn one(&mut self, seed: u64, rng: &mut Rng) {
        let k = rng.range(0, KEYS) as i32;
        let doc = Document::new()
            .with("_id", k)
            .with("a", rng.range(0, 10) as i32)
            .with("b", rng.range(0, KEYS) as i32);
        let f = filter(rng);
        let ctx = format!("seed {seed}");
        match rng.range(0, 10) {
            0 | 1 => {
                let (st, r) =
                    self.rest("POST", "/insertOne", &Document::new().with("document", doc.clone()));
                let w = self
                    .wire("insert", Document::new().with("documents", vec![Value::Document(doc)]));
                let wcode = match w.get("writeErrors") {
                    Some(Value::Array(e)) => {
                        e[0].as_document().and_then(|e| e.get("code")).cloned()
                    }
                    _ => None,
                };
                if st == 200 {
                    assert_eq!(
                        (field(&r, "n"), wcode),
                        (field(&w, "n"), None),
                        "{ctx}: {r:?} {w:?}"
                    );
                } else {
                    assert_eq!(field(&r, "code"), wcode, "{ctx}: {r:?} {w:?}");
                }
            }
            2 => {
                let u = Document::new().with("$inc", Document::new().with("b", 1));
                let multi = rng.chance(0.5);
                let route = if multi { "/updateMany" } else { "/updateOne" };
                // An upsert by `_id` only: any other draws a new ObjectId on each side.
                let upsert = rng.chance(0.3) && f.get("_id").is_some();
                let body = Document::new()
                    .with("filter", f.clone())
                    .with("update", u.clone())
                    .with("upsert", upsert);
                let (_, r) = self.rest("POST", route, &body);
                let stmt = Document::new()
                    .with("q", f)
                    .with("u", u)
                    .with("multi", multi)
                    .with("upsert", upsert);
                let w = self
                    .wire("update", Document::new().with("updates", vec![Value::Document(stmt)]));
                for name in ["n", "nModified", "upserted", "writeErrors"] {
                    assert_eq!(field(&r, name), field(&w, name), "{ctx} {name}: {r:?} {w:?}");
                }
            }
            3 => {
                let repl = Document::new().with("a", 99).with("b", k);
                let body =
                    Document::new().with("filter", f.clone()).with("replacement", repl.clone());
                let (_, r) = self.rest("POST", "/replaceOne", &body);
                let stmt = Document::new().with("q", f).with("u", repl).with("multi", false);
                let w = self
                    .wire("update", Document::new().with("updates", vec![Value::Document(stmt)]));
                assert_eq!(
                    (field(&r, "n"), field(&r, "nModified")),
                    (field(&w, "n"), field(&w, "nModified")),
                    "{ctx}"
                );
            }
            4 => {
                let many = rng.chance(0.2);
                let route = if many { "/deleteMany" } else { "/deleteOne" };
                let (_, r) = self.rest("POST", route, &Document::new().with("filter", f.clone()));
                let stmt = Document::new().with("q", f).with("limit", i32::from(!many));
                let w = self
                    .wire("delete", Document::new().with("deletes", vec![Value::Document(stmt)]));
                assert_eq!(field(&r, "n"), field(&w, "n"), "{ctx}: {r:?} {w:?}");
            }
            5 | 6 => {
                let opts = Document::new()
                    .with("filter", f)
                    .with("sort", Document::new().with("b", -1).with("_id", 1))
                    .with("limit", rng.range(1, 50) as i32)
                    .with("projection", Document::new().with("_id", 1).with("b", 1))
                    .with("batchSize", 1000);
                let (_, r) = self.rest("POST", "/find", &opts);
                let w = self.wire("find", opts);
                assert_eq!(rest_batch(&r), batch(&w, "firstBatch"), "{ctx}");
            }
            7 => {
                let (_, r) = self.rest(
                    "POST",
                    "/countDocuments",
                    &Document::new().with("filter", f.clone()),
                );
                let w = self.wire("count", Document::new().with("query", f));
                let n = |v: Option<Value>| match v {
                    Some(Value::Int32(n)) => i64::from(n),
                    Some(Value::Int64(n)) => n,
                    other => panic!("{other:?}"),
                };
                assert_eq!(n(field(&r, "count")), n(field(&w, "n")), "{ctx}");
            }
            8 => {
                let (_, r) = self.rest("POST", "/distinct", &d(r#"{"key":"a"}"#));
                let w = self.wire("distinct", d(r#"{"key":"a"}"#));
                assert_eq!(field(&r, "values"), field(&w, "values"), "{ctx}");
            }
            _ => {
                let u = Document::new().with("$set", Document::new().with("a", k % 10));
                let body = Document::new()
                    .with("filter", f.clone())
                    .with("update", u.clone())
                    .with("sort", Document::new().with("_id", 1))
                    .with("new", true);
                let (_, r) = self.rest("POST", "/findOneAndUpdate", &body);
                let w = self.wire(
                    "findAndModify",
                    Document::new()
                        .with("query", f)
                        .with("update", u)
                        .with("sort", Document::new().with("_id", 1))
                        .with("new", true),
                );
                assert_eq!(field(&r, "value"), field(&w, "value"), "{ctx}");
            }
        }
    }

    /// Both collections hold the same documents.
    fn same_contents(&mut self, ctx: &str) {
        let all = d(r#"{"sort":{"_id":1},"batchSize":100000}"#);
        let (_, r) = self.rest("POST", "/find", &all);
        let w = self.wire("find", all);
        assert_eq!(rest_batch(&r), batch(&w, "firstBatch"), "{ctx}");
    }
}

fn rest_batch(r: &Document) -> Vec<Value> {
    match r.get("cursor").and_then(Value::as_document).and_then(|c| c.get("firstBatch")) {
        Some(Value::Array(b)) => b.clone(),
        other => panic!("{other:?} in {r:?}"),
    }
}

#[test]
fn rest_and_wire_answer_the_same() {
    let http = HttpPortConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        tls: None,
        settings: Settings::default(),
    };
    let cfg = NodeConfig { http: Some(http), ..config(Vec::new()) };
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut wire = Client::new(&node);
    // Both collections start with the same documents, written through each path.
    let first = docs(0..20);
    assert!(ok(
        &wire.run("eq", Document::new().with("insert", "w").with("documents", first.clone()))
    ));
    let s = TcpStream::connect(node.http_addrs()[0]).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut pair = Pair { http: Http { s }, wire: &mut wire };
    let (st, _) = pair.rest("POST", "/insertMany", &Document::new().with("documents", first));
    assert_eq!(st, 200);
    until("both collections", || {
        let (_, r) = pair.rest("POST", "/countDocuments", &Document::new());
        (r.get("count") == Some(&Value::Int32(20))).then_some(())
    });
    let ops = std::env::var("TESELADB_EQUIV_OPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_OPS);
    let seed = std::env::var("TESELADB_EQUIV_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or(19);
    let mut rng = Rng::new(seed);
    for k in 0..ops {
        pair.one(seed, &mut rng);
        if k % 500 == 499 {
            pair.same_contents(&format!("seed {seed}, after {} operations", k + 1));
        }
    }
    pair.same_contents(&format!("seed {seed}, at the end"));
    drop(node.stop());
}
