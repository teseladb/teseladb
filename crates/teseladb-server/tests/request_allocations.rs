//! Allocations of reading a request (006 FR-039): an `insert` of a hundred documents is read
//! with one pass per document that keeps each canonical document as the bytes it came in, where
//! the earlier reading checked each one three times and copied it field by field. The count of
//! the earlier reading is measured here too, with the same calls it made, so the two stay
//! comparable on any host.

use teseladb_sys::alloc_count::{CountingAlloc, allocations};
use teseladb_wire::connection::Connection;
use teseladb_wire::msg::{HEADER_LEN, Header, opcode};
use teseladb_bson::{Document, Limits, RawDoc, Value, validate_message};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

const DOCS: usize = 100;

fn limits() -> Limits {
    Limits { max_size: teseladb_bson::MAX_SIZE + 16 * 1024, max_depth: teseladb_bson::MAX_COMMAND_DEPTH }
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

#[test]
fn an_insert_of_canonical_documents_allocates_a_fraction_of_the_earlier_reading() {
    let docs: Vec<Vec<u8>> = (0..DOCS).map(|i| stored(i).to_bytes().unwrap()).collect();
    let message = insert_message(&docs);

    let before = allocations();
    let read = earlier(&docs);
    let earlier_count = allocations() - before;
    drop(read);

    let mut conn = Connection::new(1);
    let before = allocations();
    conn.receive(&message);
    let req = conn.next_request().unwrap().unwrap();
    let now = allocations() - before;
    let Some(Value::Array(got)) = req.command.get("documents") else { panic!("documents") };
    assert_eq!(got.len(), DOCS);

    eprintln!("allocations reading {DOCS} documents: earlier {earlier_count}, now {now}");
    // Each canonical document is one copy of its bytes and its box; the earlier reading made a
    // string and a value for every field, nested ones included.
    assert!(now * 4 < earlier_count, "now {now}, earlier {earlier_count}");
}
