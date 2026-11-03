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
