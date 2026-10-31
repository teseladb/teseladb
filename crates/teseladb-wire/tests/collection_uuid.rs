//! `collectionUUID` of the commands that take it and the words of the errors raised while an
//! update runs, as a `refrouter` 9.0.4 gives them (measured).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

fn s(x: &str) -> Value {
    Value::from(x)
}

fn d(pairs: &[(&str, Value)]) -> Value {
    Value::Document(doc(pairs))
}

fn code(r: &Document) -> i32 {
    match r.get("code") {
        Some(Value::Int32(c)) => *c,
        _ => panic!("no error in {r:?}"),
    }
}

fn uuid(bytes: &[u8]) -> Value {
    Value::Binary { subtype: 4, bytes: bytes.to_vec() }
}

/// The UUID of collection `coll` of database `db`.
fn uuid_of(m: &mut Mem, coll: &str) -> Value {
    let r = m.run("db", doc(&[("listCollections", 1.into())]));
    let Value::Array(batch) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    for c in batch {
        let Value::Document(c) = c else { continue };
        if c.get("name") == Some(&s(coll)) {
            return field(c, "info.uuid").clone();
        }
    }
    panic!("no {coll} in {r:?}")
}

fn setup() -> (Mem, Value, Value) {
    let mut m = Mem::new();
    for c in ["c", "d"] {
        m.run(
            "db",
            doc(&[("insert", s(c)), ("documents", Value::Array(vec![d(&[("_id", 1.into())])]))]),
        );
    }
    let (uc, ud) = (uuid_of(&mut m, "c"), uuid_of(&mut m, "d"));
    (m, uc, ud)
}

#[test]
fn reads_and_catalog_commands_check_the_uuid() {
    let (mut m, uc, ud) = setup();
    let zero = uuid(&[0; 16]);
    // The right UUID reads; another collection's is named; an unknown one is `null`.
    let r = m.run("db", doc(&[("find", s("c")), ("collectionUUID", uc.clone())]));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0));
    let r = m.run("db", doc(&[("find", s("c")), ("collectionUUID", ud.clone())]));
    assert_eq!(code(&r), 361);
    assert_eq!(field(&r, "errmsg"), &s("Collection UUID does not match that specified"));
    assert_eq!(
        (field(&r, "db"), field(&r, "expectedCollection"), field(&r, "actualCollection")),
        (&s("db"), &s("c"), &s("d"))
    );
    assert_eq!(field(&r, "collectionUUID"), &ud);
    let r = m.run("db", doc(&[("find", s("none")), ("collectionUUID", zero.clone())]));
    assert_eq!((code(&r), field(&r, "actualCollection")), (361, &Value::Null));
    for cmd in [
        doc(&[("drop", s("c")), ("collectionUUID", zero.clone())]),
        doc(&[("collMod", s("c")), ("collectionUUID", zero.clone())]),
    ] {
        assert_eq!(code(&m.run("db", cmd)), 361);
    }
    let rename = doc(&[
        ("renameCollection", s("db.c")),
        ("to", s("db.e")),
        ("collectionUUID", zero.clone()),
    ]);
    assert_eq!(code(&m.run("admin", rename)), 361);
    // Nothing changed: `c` is still there, under its UUID.
    assert_eq!(uuid_of(&mut m, "c"), uc);
    let ok = m.run("db", doc(&[("collMod", s("c")), ("collectionUUID", uc.clone())]));
    assert_eq!(field(&ok, "ok"), &Value::Double(1.0));
    let ok = m.run("db", doc(&[("drop", s("c")), ("collectionUUID", uc)]));
    assert_eq!(field(&ok, "ok"), &Value::Double(1.0));
    // A UUID that is not one.
    let r = m.run("db", doc(&[("find", s("d")), ("collectionUUID", s("x"))]));
    assert_eq!(
        field(&r, "errmsg"),
        &s("BSON field 'find.collectionUUID' is the wrong type 'string', expected type 'binData'")
    );
    let old = Value::Binary { subtype: 3, bytes: vec![0; 16] };
    let r = m.run("db", doc(&[("find", s("d")), ("collectionUUID", old)]));
    assert_eq!(
        field(&r, "errmsg"),
        &s(
            "BSON field 'find.collectionUUID' is the wrong binData type 'UUID(deprecated)', expected type 'UUID'"
        )
    );
    for bad in [uuid(b"ab"), Value::Binary { subtype: 0, bytes: vec![0; 16] }] {
        let r = m.run("db", doc(&[("find", s("d")), ("collectionUUID", bad)]));
        assert_eq!(code(&r), 207);
    }
}

#[test]
fn writes_report_the_uuid_per_statement() {
    let (mut m, _, ud) = setup();
    let zero = uuid(&[0; 16]);
    let docs = Value::Array(vec![d(&[("_id", 5.into())]), d(&[("_id", 6.into())])]);
    let r = m.run(
        "db",
        doc(&[("insert", s("c")), ("documents", docs.clone()), ("collectionUUID", zero.clone())]),
    );
    assert_eq!(
        (field(&r, "n"), field(&r, "writeErrors.0.code")),
        (&Value::Int32(0), &Value::Int32(361))
    );
    assert_eq!(field(&r, "writeErrors.0.expectedCollection"), &s("c"));
    let Value::Array(errors) = field(&r, "writeErrors") else { panic!() };
    assert_eq!(errors.len(), 1);
    // Unordered, every statement fails; a collection that does not exist is not created.
    let r = m.run(
        "db",
        doc(&[
            ("insert", s("new")),
            ("documents", docs),
            ("ordered", false.into()),
            ("collectionUUID", zero.clone()),
        ]),
    );
    let Value::Array(errors) = field(&r, "writeErrors") else { panic!() };
    assert_eq!(errors.len(), 2);
    let r =
        m.run("db", doc(&[("listCollections", 1.into()), ("filter", d(&[("name", s("new"))]))]));
    assert_eq!(field(&r, "cursor.firstBatch"), &Value::Array(vec![]));
    let update = d(&[("q", d(&[])), ("u", d(&[("$set", d(&[("a", 1.into())]))]))]);
    let r = m.run(
        "db",
        doc(&[("update", s("c")), ("updates", Value::Array(vec![update])), ("collectionUUID", ud)]),
    );
    assert_eq!(field(&r, "writeErrors.0.actualCollection"), &s("d"));
    let delete = d(&[("q", d(&[])), ("limit", 1.into())]);
    let r = m.run(
        "db",
        doc(&[
            ("delete", s("c")),
            ("deletes", Value::Array(vec![delete])),
            ("collectionUUID", zero),
        ]),
    );
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(361));
    let bad = m.run(
        "db",
        doc(&[
            ("insert", s("c")),
            ("documents", Value::Array(vec![d(&[])])),
            ("collectionUUID", uuid(b"ab")),
        ]),
    );
    assert_eq!(code(&bad), 207);
}

#[test]
fn errors_while_an_update_runs_say_so() {
    let mut m = Mem::new();
    let one = Value::Array(vec![d(&[("_id", 0.into()), ("s", s("x"))])]);
    m.run("db", doc(&[("insert", s("c")), ("documents", one)]));
    let stmt = |u: Value, upsert: bool| {
        Value::Array(vec![d(&[
            ("q", d(&[("_id", 0.into())])),
            ("u", u),
            ("upsert", upsert.into()),
        ])])
    };
    let inc = d(&[("$inc", d(&[("s", 1.into())]))]);
    let r = m.run("db", doc(&[("update", s("c")), ("updates", stmt(inc.clone(), false))]));
    assert_eq!(
        field(&r, "writeErrors.0.errmsg"),
        &s(
            "Plan executor error during update :: caused by :: Cannot apply $inc to a value of non-numeric type. {_id: 0} has the field 's' of non-numeric type string"
        )
    );
    let deep = d(&[("$set", d(&[("s.x", 1.into())]))]);
    let r = m.run("db", doc(&[("update", s("c")), ("updates", stmt(deep, false))]));
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(28));
    // An upsert that changes the filter's `_id`, in `update` and in `findAndModify`.
    let id = d(&[("$set", d(&[("_id", 5.into())]))]);
    let upsert = Value::Array(vec![d(&[
        ("q", d(&[("_id", 9.into())])),
        ("u", id.clone()),
        ("upsert", true.into()),
    ])]);
    let r = m.run("db", doc(&[("update", s("c")), ("updates", upsert)]));
    assert_eq!(
        field(&r, "writeErrors.0.errmsg"),
        &s(
            "Plan executor error during update :: caused by :: Performing an update on the path '_id' would modify the immutable field '_id'"
        )
    );
    let fam =
        doc(&[("findAndModify", s("c")), ("query", d(&[("_id", 0.into())])), ("update", inc)]);
    let r = m.run("db", fam);
    assert!(
        matches!(field(&r, "errmsg"), Value::String(t) if t.starts_with("Plan executor error during findAndModify :: caused by :: Cannot apply $inc"))
    );
    let fam = doc(&[
        ("findAndModify", s("c")),
        ("query", d(&[("_id", 9.into())])),
        ("update", id),
        ("upsert", true.into()),
    ]);
    let r = m.run("db", fam);
    assert!(
        matches!(field(&r, "errmsg"), Value::String(t) if t.starts_with("Plan executor error during findAndModify :: caused by :: Performing"))
    );
    // A statement the parser refuses keeps its words.
    let bogus = d(&[("$bogus", d(&[("s", 1.into())]))]);
    let r = m.run("db", doc(&[("update", s("c")), ("updates", stmt(bogus, false))]));
    assert!(
        matches!(field(&r, "writeErrors.0.errmsg"), Value::String(t) if t.starts_with("Unknown modifier"))
    );
}

#[test]
fn validation_failures_of_an_update_say_so_too() {
    let mut m = Mem::new();
    let validator = d(&[("a", d(&[("$type", s("int"))]))]);
    m.run("db", doc(&[("create", s("v")), ("validator", validator)]));
    let one = Value::Array(vec![d(&[("_id", 0.into()), ("a", 1.into())])]);
    m.run("db", doc(&[("insert", s("v")), ("documents", one)]));
    let set = d(&[("$set", d(&[("a", s("x"))]))]);
    for (q, upsert) in [(0, false), (5, true)] {
        let u = Value::Array(vec![d(&[
            ("q", d(&[("_id", q.into())])),
            ("u", set.clone()),
            ("upsert", upsert.into()),
        ])]);
        let r = m.run("db", doc(&[("update", s("v")), ("updates", u)]));
        assert_eq!(
            field(&r, "writeErrors.0.errmsg"),
            &s("Plan executor error during update :: caused by :: Document failed validation")
        );
        assert_eq!(field(&r, "writeErrors.0.errInfo.failingDocumentId"), &Value::Int32(q));
    }
    // An insert's failure has no executor words.
    let bad = Value::Array(vec![d(&[("_id", 1.into()), ("a", s("x"))])]);
    let r = m.run("db", doc(&[("insert", s("v")), ("documents", bad)]));
    assert_eq!(field(&r, "writeErrors.0.errmsg"), &s("Document failed validation"));
}
