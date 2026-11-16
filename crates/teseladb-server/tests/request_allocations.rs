//! Allocations of reading a request (006 FR-039, 012 FR-027): an `insert` of a hundred documents
//! is read with one pass per document, and each canonical document keeps a view of the request's
//! one buffer, where the earlier reading checked each one three times and copied it field by
//! field. The count of the earlier reading is measured here too, with the same calls it made, so
//! the two stay comparable on any host.

#![cfg(target_os = "linux")]

use teseladb_bson::{Document, Limits, RawDoc, Value, validate_message};
use teseladb_io::engine::stats::allocations_here;
use teseladb_io::engine::{DirectIo, EngineConfig, NodeEngine};
use teseladb_wire::connection::Connection;
use teseladb_wire::msg::{HEADER_LEN, Header, opcode};

const DOCS: usize = 100;

fn limits() -> Limits {
    Limits {
        max_size: teseladb_bson::MAX_SIZE + 16 * 1024,
        max_depth: teseladb_bson::MAX_COMMAND_DEPTH,
    }
}

fn stored(i: usize) -> Document {
    let tags: Vec<Value> = (0..4).map(|t| Value::from(format!("tag{t}"))).collect();
    Document::new()
        .with("_id", i as i64)
        .with("name", format!("customer {i}"))
        .with("email", format!("c{i}@example.com"))
        .with("age", 30 + (i % 40) as i32)
        .with("balance", 10.5 * i as f64)
        .with("tags", tags)
        .with("address", Document::new().with("city", "Madrid").with("zip", "28001"))
}

/// The bytes of an `OP_MSG` `insert` whose documents go as a document sequence.
fn insert_message(docs: &[Vec<u8>]) -> Vec<u8> {
    let body = Document::new().with("insert", "c").with("$db", "t").to_bytes().unwrap();
    let mut sections = vec![0u8];
    sections.extend_from_slice(&body);
    let mut payload = b"documents\0".to_vec();
    for d in docs {
        payload.extend_from_slice(d);
    }
    sections.push(1);
    sections.extend_from_slice(&i32::try_from(payload.len() + 4).unwrap().to_le_bytes());
    sections.extend_from_slice(&payload);
    let length = i32::try_from(HEADER_LEN + 4 + sections.len()).unwrap();
    let mut out = Vec::new();
    Header { length, request_id: 1, response_to: 0, op_code: opcode::OP_MSG }.write(&mut out);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&sections);
    out
}

/// The earlier reading of each document: `validate_message`, the specification's check twice,
/// and its fields copied into owned values.
fn earlier(docs: &[Vec<u8>]) -> Vec<Document> {
    docs.iter()
        .map(|d| {
            validate_message(d, limits()).unwrap();
            RawDoc::from_bytes_with(d, limits()).unwrap();
            Document::from(RawDoc::from_bytes_with(d, limits()).unwrap())
        })
        .collect()
}

/// What the reading of the message gives: the allocations of the earlier reading and of the
/// connection's, the documents read, whether they are in the order of the message, and the
/// distance between the first and the last.
#[derive(Debug)]
struct Reading {
    earlier: u64,
    now: u64,
    documents: usize,
    in_order: bool,
    span: usize,
}

/// Both readings of `message`, counted by the allocator of the shard they run on.
fn read_on_shard(docs: &[Vec<u8>], message: &[u8]) -> Reading {
    let before = allocations_here();
    let read = earlier(docs);
    let earlier_count = allocations_here() - before;
    drop(read);

    let mut conn = Connection::new(1);
    let before = allocations_here();
    conn.receive(message);
    let req = conn.next_request().unwrap().unwrap();
    let now = allocations_here() - before;
    let Some(Value::Array(got)) = req.command.get("documents") else {
        return Reading { earlier: earlier_count, now, documents: 0, in_order: false, span: 0 };
    };
    // 012 FR-027: the documents keep views of the request's one buffer, in message order.
    let views: Vec<usize> = got
        .iter()
        .filter_map(|v| match v {
            Value::Document(d) => Some(d.canonical_bytes()?.as_ptr() as usize),
            _ => None,
        })
        .collect();
    Reading {
        earlier: earlier_count,
        now,
        documents: views.len(),
        in_order: views.windows(2).all(|w| w[0] < w[1]),
        span: views.last().zip(views.first()).map_or(usize::MAX, |(l, f)| l - f),
    }
}

#[test]
fn an_insert_of_canonical_documents_allocates_a_fraction_of_the_earlier_reading() {
    let docs: Vec<Vec<u8>> = (0..DOCS).map(|i| stored(i).to_bytes().unwrap()).collect();
    let message = insert_message(&docs);
    let len = message.len();
    // On a shard, where the engine's allocator counts every allocation (023 FR-030).
    let engine = NodeEngine::start(&EngineConfig {
        shards: 1,
        memory: Some(256 << 20),
        direct_io: DirectIo::Off,
        ..EngineConfig::default()
    })
    .unwrap();
    let r = engine.run_on(0, move || read_on_shard(&docs, &message)).unwrap();
    engine.stop();

    eprintln!("allocations reading {DOCS} documents: earlier {}, now {}", r.earlier, r.now);
    assert_eq!(r.documents, DOCS, "{r:?}");
    assert!(r.in_order, "in the order of the message");
    assert!(r.span < len, "inside the one buffer of the message: {r:?}");
    // Each canonical document is one copy of its bytes and its box; the earlier reading made a
    // string and a value for every field, nested ones included.
    assert!(r.now > 0 && r.now * 10 < r.earlier, "{r:?}");
}
