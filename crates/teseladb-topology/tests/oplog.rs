//! The format of `local.oplog.rs` (018 FR-009, contract `oplog-format.md`, T011). The expected
//! entries are those RefDb 9.0.4 (`ref:9.0`) wrote for the same operations, field by field
//! and in their order.

use teseladb_bson::{Document, Value};
use teseladb_topology::oplog::{
    APPLY_OPS_NS, DROP_REFUSED, DROP_REFUSED_CODE, NULL_OP_TIME, OPLOG_NS, OpType, Operation,
    OplogEntry, OplogError, TxnInfo, WRITE_REFUSED, transaction_entries,
};
use teseladb_topology::replset::{OpTime, Timestamp};

const NS: &str = "ventas.pedidos";
const UI: [u8; 16] = [0xe9; 16];

fn at(increment: u32) -> OpTime {
    OpTime { ts: Timestamp { time: 1_793_359_031, increment }, term: 2 }
}

fn uuid() -> Value {
    Value::Binary { subtype: 4, bytes: UI.to_vec() }
}

fn lsid() -> Document {
    Document::new()
        .with("id", Value::Binary { subtype: 4, bytes: vec![0xc0; 16] })
        .with("uid", Value::Binary { subtype: 0, bytes: vec![0xe3; 32] })
}

fn id(n: i32) -> Document {
    Document::new().with("_id", n)
}

fn insert(n: i32) -> Operation {
    Operation::new(OpType::Insert, NS, Some(UI), id(n), Some(id(n)))
}

fn ts(op: OpTime) -> Value {
    Value::Timestamp { time: op.ts.time, increment: op.ts.increment }
}

fn null_op_time() -> Document {
    Document::new().with("ts", Value::Timestamp { time: 0, increment: 0 }).with("t", -1i64)
}

#[test]
fn retryable_update_matches_refdb() {
    let diff = Document::new()
        .with("d", Document::new().with("sub", false))
        .with("u", Document::new().with("estado", "enviado").with("n", 2));
    let o = Document::new().with("$v", 2).with("diff", diff);
    let entry = OplogEntry {
        txn: Some(TxnInfo { lsid: lsid(), txn_number: 2 }),
        operation: Operation::new(OpType::Update, NS, Some(UI), o.clone(), Some(id(42))),
        retry_image: None,
        stmt_id: Some(0),
        extra: Document::new(),
        op_time: Some(at(3)),
        version: 2,
        wall_ms: Some(1_793_359_031_975),
        prev_op_time: Some(NULL_OP_TIME),
        multi_op_type: None,
    };
    let expected = Document::new()
        .with("lsid", lsid())
        .with("txnNumber", 2i64)
        .with("op", "u")
        .with("ns", NS)
        .with("ui", uuid())
        .with("o", o)
        .with("o2", id(42))
        .with("stmtId", 0)
        .with("ts", ts(at(3)))
        .with("t", 2i64)
        .with("v", 2i64)
        .with("wall", Value::DateTime(1_793_359_031_975))
        .with("prevOpTime", null_op_time());
    let doc = entry.to_document();
    assert_eq!(doc, expected);
    assert_eq!(OplogEntry::from_document(&doc), Ok(entry));
}

/// A retryable `findAndModify` carries `needsRetryImage` before `stmtId`, and the `applyOps`
/// entry of a retryable insert of several documents `multiOpType: 1` after `prevOpTime`
/// (measured on RefDb 9.0.4 on 02-11-2026).
#[test]
fn retry_image_and_multi_op_type_round_trip() {
    let doc = Document::new()
        .with("lsid", lsid())
        .with("txnNumber", 8i64)
        .with("op", "u")
        .with("ns", NS)
        .with("ui", uuid())
        .with("o", Document::new().with("a", 1))
        .with("o2", id(1))
        .with("needsRetryImage", "preImage")
        .with("stmtId", 0)
        .with("ts", ts(at(7)))
        .with("t", 2i64)
        .with("v", 2i64)
        .with("prevOpTime", null_op_time())
        .with("multiOpType", 1);
    let entry = OplogEntry::from_document(&doc).unwrap();
    assert_eq!(entry.retry_image.as_deref(), Some("preImage"));
    assert_eq!(entry.multi_op_type, Some(1));
    assert_eq!(entry.to_document(), doc);
    for (field, bad) in [("needsRetryImage", Value::Int32(1)), ("multiOpType", Value::from("1"))] {
        let wrong = doc.clone().with(field, bad);
        assert_eq!(OplogEntry::from_document(&wrong), Err(OplogError::WrongType(field)));
    }
}

#[test]
fn create_with_its_version_context_round_trips() {
    let doc = Document::new()
        .with("op", "c")
        .with("ns", "ventas.$cmd")
        .with("ui", uuid())
        .with("o", Document::new().with("create", "pedidos"))
        .with("o2", Document::new().with("catalogId", 21i64))
        .with("versionContext", Document::new().with("OFCV", "9.0"))
        .with("ts", ts(at(1)))
        .with("t", 2i64)
        .with("v", 2i64)
        .with("wall", Value::DateTime(1_793_359_031_969));
    let entry = OplogEntry::from_document(&doc).expect("valid entry");
    assert_eq!(entry.operation.op, OpType::Command);
    assert!(entry.extra.get("versionContext").is_some());
    assert_eq!(entry.to_document(), doc);
}

#[test]
fn noop_and_plain_delete() {
    let noop = Document::new()
        .with("op", "n")
        .with("ns", "")
        .with("o", Document::new().with("hi", 1))
        .with("ts", ts(at(9)))
        .with("t", 2i64)
        .with("v", 2i64)
        .with("wall", Value::DateTime(1));
    let e = OplogEntry::from_document(&noop).expect("noop");
    assert_eq!((e.operation.op, e.operation.ui, e.txn.is_none()), (OpType::Noop, None, true));
    assert_eq!(e.to_document(), noop);
    let delete = Operation::new(OpType::Delete, "big.c", Some(UI), id(1), None).to_document();
    assert_eq!(delete.iter().map(|(k, _)| k).collect::<Vec<_>>(), ["op", "ns", "ui", "o"]);
}

#[test]
fn small_transaction_is_one_apply_ops() {
    let mut next = 7;
    let entries = transaction_entries(
        TxnInfo { lsid: lsid(), txn_number: 6 },
        vec![insert(1), insert(2)],
        1_793_359_031_985,
        &mut || {
            next += 1;
            at(next - 1)
        },
    );
    assert_eq!(entries.len(), 1);
    let ops =
        vec![Value::Document(insert(1).to_document()), Value::Document(insert(2).to_document())];
    let expected = Document::new()
        .with("lsid", lsid())
        .with("txnNumber", 6i64)
        .with("op", "c")
        .with("ns", APPLY_OPS_NS)
        .with("o", Document::new().with("applyOps", ops))
        .with("ts", ts(at(7)))
        .with("t", 2i64)
        .with("v", 2i64)
        .with("wall", Value::DateTime(1_793_359_031_985))
        .with("prevOpTime", null_op_time());
    assert_eq!(entries[0].to_document(), expected);
}

fn big(n: i32, len: usize) -> Operation {
    let doc = id(n).with("big", "x".repeat(len));
    Operation::new(OpType::Insert, "edge.c", Some(UI), doc, Some(id(n)))
}

fn split(ops: Vec<Operation>) -> Vec<Document> {
    let mut next = 0;
    let txn = TxnInfo { lsid: lsid(), txn_number: 1 };
    transaction_entries(txn, ops, 0, &mut || {
        next += 1;
        at(next)
    })
    .iter()
    .map(OplogEntry::to_document)
    .collect()
}

fn apply_ops_len(d: &Document) -> usize {
    match d.get("o") {
        Some(Value::Document(o)) => match o.get("applyOps") {
            Some(Value::Array(a)) => a.len(),
            _ => 0,
        },
        _ => 0,
    }
}

/// RefDb 9.0.4: four inserts of 4 MiB − 400 bytes fit in one entry (`o` of 16 776 048 bytes);
/// with 4 MiB + 100 they split 3 + 1, and the entries chain with `partialTxn` and `count`.
#[test]
fn large_transaction_splits_like_refdb() {
    let under = split((0..4).map(|i| big(i, 4 * 1024 * 1024 - 400)).collect());
    assert_eq!(under.len(), 1);
    let over = split((0..4).map(|i| big(i, 4 * 1024 * 1024 + 100)).collect());
    assert_eq!(over.iter().map(apply_ops_len).collect::<Vec<_>>(), [3, 1]);
    let Some(Value::Document(first)) = over[0].get("o") else { panic!("o") };
    assert_eq!(first.get("partialTxn"), Some(&Value::Boolean(true)));
    let Some(Value::Document(last)) = over[1].get("o") else { panic!("o") };
    assert_eq!(last.get("count"), Some(&Value::Int64(4)));
    assert_eq!(over[1].get("prevOpTime"), Some(&Value::Document(at(1).to_document())));
    assert_eq!(over[0].get("prevOpTime"), Some(&Value::Document(null_op_time())));
    let one = split((0..2).map(|i| big(i, 16 * 1024 * 1024 - 200)).collect());
    assert_eq!(one.iter().map(apply_ops_len).collect::<Vec<_>>(), [1, 1]);
}

/// A transaction without writes leaves no entry.
#[test]
fn empty_transaction_has_no_entries() {
    assert!(split(Vec::new()).is_empty());
}

#[test]
fn malformed_entries_are_refused() {
    let base = || {
        Document::new()
            .with("op", "i")
            .with("ns", NS)
            .with("o", id(1))
            .with("ts", ts(at(1)))
            .with("t", 2i64)
            .with("wall", Value::DateTime(1))
    };
    let parse = |d: Document| OplogEntry::from_document(&d).err();
    let without = |k: &str| {
        let mut d = base();
        d.remove(k);
        d
    };
    let with = |k: &str, v: Value| {
        let mut d = base();
        d.insert(k, v);
        d
    };
    assert_eq!(parse(without("op")), Some(OplogError::Missing("op")));
    assert_eq!(parse(without("ns")), Some(OplogError::Missing("ns")));
    assert_eq!(parse(without("o")), Some(OplogError::Missing("o")));
    assert_eq!(parse(with("op", Value::from("x"))), Some(OplogError::UnknownOp("x".into())));
    assert_eq!(parse(with("op", Value::Int32(1))), Some(OplogError::WrongType("op")));
    assert_eq!(parse(with("ns", Value::Int32(1))), Some(OplogError::WrongType("ns")));
    assert_eq!(parse(with("o", Value::Int32(1))), Some(OplogError::WrongType("o")));
    assert_eq!(parse(with("o2", Value::Int32(1))), Some(OplogError::WrongType("o2")));
    assert_eq!(parse(with("ui", Value::Int32(1))), Some(OplogError::WrongType("ui")));
    let short = Value::Binary { subtype: 4, bytes: vec![1; 15] };
    assert_eq!(parse(with("ui", short)), Some(OplogError::WrongType("ui")));
    assert_eq!(parse(with("ts", Value::Int32(1))), Some(OplogError::WrongType("ts")));
    assert_eq!(parse(with("t", Value::from("a"))), Some(OplogError::WrongType("t")));
    assert_eq!(parse(with("v", Value::Double(2.5))), Some(OplogError::WrongType("v")));
    assert_eq!(parse(with("wall", Value::Int32(1))), Some(OplogError::WrongType("wall")));
    assert_eq!(parse(with("lsid", Value::Int32(1))), Some(OplogError::WrongType("lsid")));
    assert_eq!(parse(with("txnNumber", Value::Int32(1))), Some(OplogError::Missing("lsid")));
    let mut no_txn = base();
    no_txn.insert("lsid", lsid());
    assert_eq!(parse(no_txn), Some(OplogError::Missing("txnNumber")));
    assert_eq!(parse(with("stmtId", Value::from("a"))), Some(OplogError::WrongType("stmtId")));
    assert_eq!(
        parse(with("prevOpTime", Value::Int32(1))),
        Some(OplogError::WrongType("prevOpTime"))
    );
    let bad_prev = Document::new().with("ts", Value::Int32(1)).with("t", 1i64);
    assert_eq!(parse(with("prevOpTime", bad_prev.into())), Some(OplogError::WrongType("ts")));
    let no_t = Document::new().with("ts", ts(at(1)));
    assert_eq!(parse(with("prevOpTime", no_t.into())), Some(OplogError::Missing("t")));
}

/// What `refrestore --oplogReplay` gives `applyOps` for the operations of a transaction:
/// `ts: Timestamp(0, 0)`, `t: null` and `v: 0` (captured on 02-11-2026 from refrestore
/// 100.20.0). A `refnode` 9.0.4 takes an entry with `ts` and no `t`, with a null `t` or `v`, and
/// a term written as a double (measured on 02-11-2026); the term it does not have is -1.
#[test]
fn entries_of_refrestore() {
    let d = Document::new()
        .with("ts", ts(at(0)))
        .with("t", Value::Null)
        .with("v", 0)
        .with("op", "i")
        .with("ns", NS)
        .with("o", id(1));
    let e = OplogEntry::from_document(&d).expect("a nested operation of refrestore");
    assert_eq!(e.op_time.map(|o| o.term), Some(-1));
    assert_eq!(e.version, 0);
    let mut no_t = d.clone();
    no_t.remove("t");
    let e = OplogEntry::from_document(&no_t).expect("ts without t");
    assert_eq!(e.op_time.map(|o| o.term), Some(-1));
    let null_v = d.clone().with("v", Value::Null);
    assert_eq!(OplogEntry::from_document(&null_v).expect("null v").version, 2);
    let mut double_t = d.clone();
    double_t.insert("t", Value::Double(3.0));
    let e = OplogEntry::from_document(&double_t).expect("a double term");
    assert_eq!(e.op_time.map(|o| o.term), Some(3));
    double_t.insert("t", Value::Double(3.5));
    assert_eq!(OplogEntry::from_document(&double_t).err(), Some(OplogError::WrongType("t")));
}

/// An entry given to `applyOps` may come without `ts`, `t`, `v` or `wall`; integer terms and
/// versions in 32 bits are read as they are written by older tools.
#[test]
fn optional_fields_and_small_integers() {
    let d = Document::new().with("op", "i").with("ns", NS).with("o", id(1));
    let e = OplogEntry::from_document(&d).expect("entry without times");
    assert_eq!((e.op_time, e.wall_ms, e.version), (None, None, 2));
    assert_eq!(e.to_document().get("v"), Some(&Value::Int64(2)));
    let d =
        d.with("ts", ts(at(1))).with("t", 2).with("v", 2).with("txnNumber", 3).with("lsid", lsid());
    let e = OplogEntry::from_document(&d).expect("int32 fields");
    assert_eq!((e.op_time, e.version), (Some(at(1)), 2));
    assert_eq!(e.txn.map(|t| t.txn_number), Some(3));
}

#[test]
fn op_types_by_code() {
    for op in [OpType::Insert, OpType::Update, OpType::Delete, OpType::Command, OpType::Noop] {
        assert_eq!(OpType::from_code(op.code()), Some(op));
    }
    assert_eq!(OpType::from_code("db"), None);
}

/// RefDb 9.0.4 refuses writes to the oplog with `InvalidNamespace` and its drop with
/// `Location5255000`.
#[test]
fn writes_to_the_oplog_are_refused() {
    assert_eq!(WRITE_REFUSED, "cannot write to local.oplog.rs");
    assert_eq!(OPLOG_NS, "local.oplog.rs");
    assert_eq!(DROP_REFUSED_CODE, 5_255_000);
    assert_eq!(DROP_REFUSED, "can't drop live oplog while replicating");
}
