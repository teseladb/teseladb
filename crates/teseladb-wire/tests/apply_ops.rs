//! `applyOps` over the in-memory node of `common` (018 FR-011, T012), with the replies of a
//! replica set of RefDb 9.0.4 (measured on 02-11-2026).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

fn op(op: &str, ns: &str, o: Document) -> Value {
    Value::Document(Document::new().with("op", op).with("ns", ns).with("o", o))
}

fn op2(op: &str, ns: &str, o: Document, o2: Document) -> Value {
    Value::Document(Document::new().with("op", op).with("ns", ns).with("o", o).with("o2", o2))
}

fn apply(m: &mut Mem, ops: Vec<Value>) -> Document {
    m.run("admin", doc(&[("applyOps", Value::Array(ops))]))
}

fn id(n: i32) -> Document {
    Document::new().with("_id", n)
}

fn all(m: &mut Mem) -> Value {
    let r = m.run("ap", doc(&[("find", Value::from("t")), ("sort", Value::Document(id(1)))]));
    field(&r, "cursor.firstBatch").clone()
}

#[test]
fn entries_apply_one_by_one() {
    let mut m = Mem::new();
    let create = Document::new().with("create", "t");
    let r = apply(
        &mut m,
        vec![
            op("c", "ap.$cmd", create),
            op("i", "ap.t", id(1).with("a", 1)),
            op("i", "ap.t", id(2).with("a", 2)),
        ],
    );
    assert_eq!(r.get("applied"), Some(&Value::Int32(3)), "{r:?}");
    assert_eq!(r.get("results"), Some(&Value::Array(vec![Value::Boolean(true); 3])));
    let diff = Document::new()
        .with("u", Document::new().with("a", 5))
        .with("i", Document::new().with("b", 1));
    let r = apply(
        &mut m,
        vec![
            op2("u", "ap.t", Document::new().with("$v", 2).with("diff", diff), id(1)),
            op("d", "ap.t", id(2)),
            op("d", "ap.t", id(99)),
            op("i", "ap.t", id(1).with("a", 5).with("b", 1).with("c", 3)),
            op("n", "", Document::new().with("msg", "hi")),
        ],
    );
    assert_eq!(r.get("applied"), Some(&Value::Int32(4)), "{r:?}");
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)));
    let one = id(1).with("a", 5).with("b", 1).with("c", 3);
    assert_eq!(all(&mut m), Value::Array(vec![Value::Document(one)]));
    // A legacy update by operators and a replacement.
    let set = Document::new().with("$set", Document::new().with("q", 1));
    let r = apply(
        &mut m,
        vec![op2("u", "ap.t", set, id(1)), op2("u", "ap.t", id(1).with("z", 1), id(1))],
    );
    assert_eq!(r.get("applied"), Some(&Value::Int32(2)), "{r:?}");
    assert_eq!(all(&mut m), Value::Array(vec![Value::Document(id(1).with("z", 1))]));
}

#[test]
fn failures_as_refdb_gives_them() {
    let mut m = Mem::new();
    let r = m.run("ap", doc(&[("create", Value::from("t"))]));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let diff = Document::new()
        .with("$v", 2)
        .with("diff", Document::new().with("u", Document::new().with("a", 1)));
    let r = apply(
        &mut m,
        vec![op("i", "ap.t", id(60)), op2("u", "ap.t", diff, id(99)), op("i", "ap.t", id(61))],
    );
    assert_eq!(r.get("applied"), Some(&Value::Int32(3)), "{r:?}");
    assert_eq!(
        r.get("results"),
        Some(&Value::Array(vec![
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(true)
        ]))
    );
    assert_eq!(r.get("code"), Some(&Value::Int32(8)));
    assert_eq!(r.get("errmsg"), Some(&Value::from("applyOps had one or more errors applying ops")));
    let r = apply(&mut m, vec![op("x", "ap.t", id(1))]);
    assert_eq!(
        (r.get("code"), r.get("errmsg")),
        (
            Some(&Value::Int32(2)),
            Some(&Value::from(
                "Enumeration value 'x' for field 'OplogEntryBase.op' is not a valid value."
            ))
        )
    );
    let r = apply(&mut m, vec![op("i", "ap.nope", id(1))]);
    assert_eq!(r.get("code"), Some(&Value::Int32(26)), "{r:?}");
    assert_eq!(
        r.get("errmsg"),
        Some(&Value::from(
            "cannot apply insert or update operation on a non-existent namespace ap.nope: { op: \"i\", ns: \"ap.nope\", o: { _id: 1 } }"
        ))
    );
    let r = m.run("admin", doc(&[("applyOps", Value::Int32(1))]));
    assert_eq!(
        r.get("errmsg"),
        Some(&Value::from("Wrong type for \"applyOps\". Expected a array, got a int."))
    );
    let r = m.run(
        "admin",
        doc(&[("applyOps", Value::Array(vec![])), ("preCondition", Value::Array(vec![]))]),
    );
    assert_eq!(r.get("code"), Some(&Value::Int32(6_711_600)), "{r:?}");
    let r = apply(&mut m, vec![]);
    assert_eq!(
        (r.get("applied"), r.get("ok")),
        (Some(&Value::Int32(0)), Some(&Value::Double(1.0)))
    );
}

/// An entry that fails the command (a namespace that does not exist, an unknown `op`) stops it
/// there: the reply gives `applied` with the entries run up to it, the error, and `results` with
/// `false` for it; the entries before it stay applied (measured on a replica set of
/// `ref:9.0.4`, 03-11-2026).
#[test]
fn an_entry_that_fails_the_command_stops_it() {
    let mut m = Mem::new();
    m.run("ap", doc(&[("create", "c".into())]));
    let keys = |d: &Document| d.iter().map(|(k, _)| k.to_owned()).collect::<Vec<_>>();
    let r = apply(
        &mut m,
        vec![op("i", "ap.c", id(1)), op("i", "ap.nope", id(1)), op("i", "ap.c", id(2))],
    );
    assert_eq!(keys(&r), ["applied", "code", "codeName", "errmsg", "results", "ok"]);
    assert_eq!(r.get("applied"), Some(&Value::Int32(2)));
    assert_eq!(r.get("code"), Some(&Value::Int32(26)));
    assert_eq!(
        r.get("results"),
        Some(&Value::Array(vec![Value::Boolean(true), Value::Boolean(false)]))
    );
    let r = apply(&mut m, vec![op("i", "ap.c", id(3)), op("x", "ap.c", id(4))]);
    assert_eq!(keys(&r), ["applied", "code", "codeName", "errmsg", "results", "ok"]);
    assert_eq!(r.get("code"), Some(&Value::Int32(2)));
    let found = m.run("ap", doc(&[("find", "c".into())]));
    let ids: Vec<Value> = match field(&found, "cursor.firstBatch") {
        Value::Array(a) => a
            .iter()
            .map(|d| match d {
                Value::Document(d) => d.get("_id").cloned().expect("_id"),
                o => panic!("{o:?}"),
            })
            .collect(),
        o => panic!("{o:?}"),
    };
    assert_eq!(ids, [Value::Int32(1), Value::Int32(3)]);
}

/// The refusals of RefDb 9.0.4 (measured on 03-11-2026, `ap.js`): an entry that is not a
/// document is 14, an update without `o2` is 4 with the entry, a `c` entry with a command an
/// oplog never carries is 2; the replacement of a document that is not there does not apply.
#[test]
fn entries_that_do_not_apply() {
    let mut m = Mem::new();
    let ns = "ap.c";
    m.run("ap", doc(&[("create", Value::from("c"))]));
    let r = apply(&mut m, vec![op("i", ns, id(1))]);
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let r = apply(&mut m, vec![Value::Int32(1)]);
    assert_eq!(r.get("code"), Some(&Value::Int32(14)), "{r:?}");
    assert_eq!(
        r.get("errmsg"),
        Some(&Value::from("Wrong type for \"0\". Expected a object, got a int."))
    );
    let r = apply(
        &mut m,
        vec![op("u", ns, Document::new().with("$set", Document::new().with("a", 1)))],
    );
    assert_eq!(r.get("code"), Some(&Value::Int32(4)), "{r:?}");
    let Some(Value::String(msg)) = r.get("errmsg") else { panic!("{r:?}") };
    assert!(
        msg.starts_with("Failed to apply update due to missing _id: { oplogEntry: { op: \"u\""),
        "{msg}"
    );
    let r = apply(&mut m, vec![op2("u", ns, id(9).with("r", 1), id(9))]);
    assert_eq!(field(&r, "results.0"), &Value::Boolean(false), "{r:?}");
    let ins = Document::new().with("insert", "c").with("documents", vec![Value::Document(id(1))]);
    let r = apply(&mut m, vec![op("c", "ap.$cmd", ins)]);
    assert_eq!(r.get("code"), Some(&Value::Int32(2)), "{r:?}");
    assert_eq!(
        r.get("errmsg"),
        Some(&Value::from(
            "Enumeration value 'insert' for field 'commandString' is not a valid value."
        ))
    );
}

fn code_and_message(r: &Document) -> (Option<&Value>, Option<&Value>) {
    (r.get("code"), r.get("errmsg"))
}

fn expect(r: &Document, code: i32, msg: &str) {
    assert_eq!(code_and_message(r), (Some(&Value::Int32(code)), Some(&Value::from(msg))), "{r:?}");
}

fn entry(n: i32) -> Document {
    Document::new().with("op", "i").with("ns", "ap.t").with("o", id(n))
}

/// RefDb 9.0.4 checks `op` and `ns` of every entry before applying any; the other fields of an
/// entry are read when its turn comes, and an error then ends the command with what applied so
/// far (measured on 03-11-2026, `ap9.js`, `ap10.js` and `ap11.js` in the lane's notes).
#[test]
fn malformed_entries_as_refdb_finds_them() {
    let mut m = Mem::new();
    m.run("ap", doc(&[("create", Value::from("t"))]));
    let first = |m: &mut Mem, bad: Value| apply(m, vec![Value::Document(entry(1)), bad]);
    let without = |f: &str| {
        let mut d = entry(2);
        d.remove(f);
        Value::Document(d)
    };
    expect(
        &first(&mut m, without("op")),
        4,
        "Wrong type for \"\". Expected a string, got a missing.",
    );
    expect(&first(&mut m, without("ns")), 20, "op does not contain required \"ns\" field: 1");
    expect(
        &first(&mut m, Value::Document(entry(2).with("ns", 5))),
        20,
        "\"ns\" field is not a string: 1",
    );
    expect(
        &first(&mut m, Value::Document(entry(2).with("op", 5))),
        14,
        "Wrong type for \"op\". Expected a string, got a int.",
    );
    expect(
        &first(&mut m, Value::Int32(5)),
        14,
        "Wrong type for \"1\". Expected a object, got a int.",
    );
    assert_eq!(all(&mut m), Value::Array(vec![]));
    let idl = |f: &str, t: &str, e: &str| {
        format!("BSON field 'OplogEntryBase.{f}' is the wrong type '{t}', expected {e}")
    };
    let number = "types '[double, long, int, decimal]'";
    let uuid = |b: Vec<u8>, s: u8| Value::Binary { subtype: s, bytes: b };
    let cases: Vec<(Value, i32, String)> = vec![
        (Value::Document(entry(2).with("op", "x")), 2, "Enumeration value 'x' for field 'OplogEntryBase.op' is not a valid value.".into()),
        (without("o"), 40414, "BSON field 'OplogEntryBase.o' is missing but a required field".into()),
        (Value::Document(entry(2).with("o", 5)), 14, idl("o", "int", "type 'object'")),
        (Value::Document(entry(2).with("o2", 5)), 14, idl("o2", "int", "type 'object'")),
        (Value::Document(entry(2).with("ts", 5)), 14, idl("ts", "int", "type 'timestamp'")),
        (Value::Document(entry(2).with("t", "x")), 14, idl("t", "string", number)),
        (Value::Document(entry(2).with("v", "x")), 14, idl("v", "string", number)),
        (Value::Document(entry(2).with("wall", 5)), 14, idl("wall", "int", "type 'date'")),
        (Value::Document(entry(2).with("needsRetryImage", 5)), 14, idl("needsRetryImage", "int", "type 'string'")),
        (Value::Document(entry(2).with("multiOpType", "x")), 14, idl("multiOpType", "string", "type 'int'")),
        (Value::Document(entry(2).with("ui", 5)), 14, idl("ui", "int", "type 'binData'")),
        (Value::Document(entry(2).with("ui", uuid(vec![0; 16], 0))), 14, "BSON field 'OplogEntryBase.ui' is the wrong binData type 'general', expected type 'UUID'".into()),
        (Value::Document(entry(2).with("ui", uuid(vec![0; 3], 4))), 207, "uuid must be a 16-byte binary field with UUID (4) subtype".into()),
        (Value::Document(entry(2).with("stmtId", "x")), 8_109_800, "Field 'stmtId: \"x\"' is not valid.".into()),
        (Value::Document(entry(2).with("prevOpTime", 5)), 10_065, "invalid parameter: expected an object (prevOpTime)".into()),
        (Value::Document(entry(2).with("prevOpTime", Document::new().with("t", 1_i64))), 40414, "BSON field 'OpTimeBase.ts' is missing but a required field".into()),
        (Value::Document(entry(2).with("lsid", Document::new().with("id", uuid(vec![0; 16], 4)))), 40414, "BSON field 'OplogEntryBase.lsid.uid' is missing but a required field".into()),
        (Value::Document(entry(2).with("needsRetryImage", "x")), 2, "Enumeration value 'x' for field 'OplogEntryBase.needsRetryImage' is not a valid value.".into()),
        (Value::Document(entry(2).with("needsRetryImage", "postImage")), 72, "applyOps command does not support the internal `needsRetryImage` field".into()),
        // The other fields are read before `needsRetryImage` is refused (`ap11.js`).
        (Value::Document(entry(2).with("needsRetryImage", "preImage").with("multiOpType", "x")), 14, idl("multiOpType", "string", "type 'int'")),
    ];
    for (bad, code, msg) in cases {
        let r = first(&mut m, bad);
        expect(&r, code, &msg);
        assert_eq!(r.get("applied"), Some(&Value::Int32(2)), "{r:?}");
        let results = Value::Array(vec![Value::Boolean(true), Value::Boolean(false)]);
        assert_eq!(r.get("results"), Some(&results), "{r:?}");
    }
    // `ts` without `t` and `txnNumber` without `lsid` are taken, and so are a whole
    // `multiOpType` and `stmtId`, a `prevOpTime` with its `ts` and an `lsid` with its `uid`.
    let ts = Value::Timestamp { time: 1, increment: 1 };
    let lsid = Document::new().with("id", uuid(vec![1; 16], 4)).with("uid", uuid(vec![2; 32], 0));
    let r = apply(
        &mut m,
        vec![
            Value::Document(entry(3).with("ts", ts.clone())),
            Value::Document(entry(4).with("txnNumber", 1_i64)),
            Value::Document(entry(6).with("multiOpType", 1)),
            Value::Document(entry(7).with("stmtId", 0)),
            Value::Document(entry(8).with("prevOpTime", Document::new().with("ts", ts))),
            Value::Document(entry(9).with("lsid", lsid)),
        ],
    );
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    assert_eq!(r.get("applied"), Some(&Value::Int32(6)), "{r:?}");
    // An entry whose `ui` names no collection does not apply.
    let r = apply(&mut m, vec![Value::Document(entry(5).with("ui", uuid(vec![7; 16], 4)))]);
    expect(&r, 8, "applyOps had one or more errors applying ops");
    assert_eq!(field(&r, "results.0"), &Value::Boolean(false));
}

/// A write error or a diff RefDb does not write ends the command with its error and what
/// applied so far; an array subdiff on a field that is not an array leaves it null (measured on
/// 03-11-2026, `ap4.js` to `ap8.js`).
#[test]
fn errors_while_applying_end_the_command() {
    let mut m = Mem::new();
    m.run("ap", doc(&[("create", Value::from("t"))]));
    let index = Document::new()
        .with("key", Document::new().with("u", 1))
        .with("name", "u_1")
        .with("unique", true);
    let r = m.run(
        "ap",
        doc(&[
            ("createIndexes", Value::from("t")),
            ("indexes", Value::Array(vec![Value::Document(index)])),
        ]),
    );
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    m.run(
        "ap",
        doc(&[
            ("insert", Value::from("t")),
            ("documents", Value::Array(vec![Value::Document(id(1).with("a", 5).with("u", 1))])),
        ]),
    );
    let r = apply(
        &mut m,
        vec![
            Value::Document(entry(2).with("o", id(2).with("u", 2))),
            Value::Document(entry(3).with("o", id(3).with("u", 1))),
            Value::Document(entry(4)),
        ],
    );
    expect(&r, 11000, "E11000 duplicate key error collection: ap.t index: u_1 dup key: { u: 1 }");
    assert_eq!(r.get("applied"), Some(&Value::Int32(2)), "{r:?}");
    assert_eq!(field(&r, "keyValue"), &Value::Document(Document::new().with("u", 1)));
    let diff = |d: Document| Document::new().with("$v", 2).with("diff", d);
    let sub = |k: &str, v: Value| Document::new().with(k, v);
    let cases = [
        (
            sub("sa", Value::Document(sub("x", Value::Int32(1)))),
            4_770_503,
            "Unexpected section: x in document diff",
        ),
        (
            sub("z", Value::Document(sub("a", Value::Int32(1)))),
            4_770_503,
            "Unexpected section: z in document diff",
        ),
        (sub("d", Value::Int32(5)), 4_770_507, "Expected d section to be type object"),
        (sub("sb", Value::Int32(5)), 4_770_507, "Expected s section to be type object"),
    ];
    for (d, code, msg) in cases {
        let r = apply(&mut m, vec![op2("u", "ap.t", diff(d), id(1))]);
        expect(&r, code, msg);
        assert_eq!(field(&r, "results.0"), &Value::Boolean(false));
    }
    let array = sub("sa", Value::Document(sub("a", Value::Boolean(true)).with("u0", 5)));
    let r = apply(&mut m, vec![op2("u", "ap.t", diff(array), id(1))]);
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let r = m.run("ap", doc(&[("find", Value::from("t")), ("filter", Value::Document(id(1)))]));
    assert_eq!(field(&r, "cursor.firstBatch.0.a"), &Value::Null, "{r:?}");
}
